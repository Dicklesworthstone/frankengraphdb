//! Actual future/poll boundaries over a private admitted storage-unit fixture.
//! This is not production filesystem, transport or capability-verifier evidence.

use super::super::{SealedLimits, SealedScope, image};
use super::*;
use crate::compact::Compaction;
use crate::{AdjacencyEntry, PartitionRootVersion};
use asupersync::runtime::yield_now::yield_now;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb_types::{BranchId, EId, GraphId, ObjectId, PurposeContexts};
use std::cell::Cell;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

fn source(count: usize, one_group: bool) -> SealedPartition {
    let mut entries: Vec<_> = (0..count)
        .map(|i| AdjacencyEntry {
            src: VId(if one_group { 1 } else { (i % 13) as u128 }),
            dst: VId(if one_group {
                7
            } else {
                u128::MAX - (i % 7) as u128
            }),
            relation: RelationId(if one_group { 1 } else { 1 + (i % 3) as u64 }),
            eid: EId(u128::MAX - i as u128),
            created_at: CommitSeq(1),
            retired_at: (i % 4 == 0).then_some(CommitSeq(3)),
        })
        .collect();
    entries.sort_by_key(|e| (e.src, e.relation, e.dst, e.eid, e.created_at));
    let blocks: Vec<_> = entries
        .chunks(120)
        .map(<[AdjacencyEntry]>::to_vec)
        .collect();
    let block_props = (0..blocks.len()).map(|_| None).collect();
    let limits = SealedLimits::default();
    let image = image::build(
        Compaction {
            blocks,
            block_props,
            dropped: 0,
            superseded: 0,
        },
        limits,
        &mut || Ok(()),
    )
    .unwrap();
    SealedPartition::finish(
        SealedScope {
            source_root: PartitionRootVersion(ObjectId([0x47; 32])),
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 1,
            floor: CommitSeq(1),
            publication: CommitSeq(10),
        },
        image,
        limits,
        &mut || Ok(()),
    )
    .unwrap()
}

#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
fn drive<F: Future>(future: F, guards: &Cell<usize>, quantum: usize) -> (F::Output, usize) {
    let wakes = Arc::new(Wakes::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut task = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    let mut pending = 0;
    loop {
        let before = guards.get();
        let result = future.as_mut().poll(&mut task);
        assert!(
            guards.get() - before <= quantum + 2,
            "unbounded checkpoint work in a poll"
        );
        match result {
            Poll::Ready(result) => {
                assert_eq!(wakes.0.load(Ordering::Relaxed), pending);
                return (result, pending);
            }
            Poll::Pending => {
                pending += 1;
                assert_eq!(wakes.0.load(Ordering::Relaxed), pending);
                assert!(pending < 1_000_000, "resumption must make progress");
            }
        }
    }
}
fn quantum(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

// Independent canonical permutation oracle, not the synchronous builder.
fn expected(source: &SealedPartition) -> Vec<(VId, RelationId, u64)> {
    let mut refs = Vec::new();
    for row in &source.image.rows {
        for at in 0..row.len() {
            let (edge, _) = row.incidence(&source.image, at).unwrap();
            refs.push((edge.dst, edge.relation, refs.len() as u64));
        }
    }
    refs.sort_unstable();
    refs
}
fn actual(index: &SealedIncomingIndex) -> Vec<(VId, RelationId, u64)> {
    let mut refs = Vec::new();
    for row in &index.index.rows {
        let start = refs.len();
        for chunk in &index.index.chunks[row.first_chunk..row.end_chunk] {
            let mut at = 0;
            while let Some(position) = chunk.select(at) {
                refs.push((row.destination, row.relation, position));
                at += 1;
            }
        }
        assert_eq!(refs.len() - start, row.incidences);
    }
    refs
}

#[test]
fn cooperative_build_preserves_canonical_locators_exact_stats_and_source_ownership() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    for count in [0, 1, 24, 255, 256, 257, 700] {
        for one_group in [false, true] {
            let source = source(count, one_group);
            let expected = expected(&source);
            let sync = source
                .incoming_index(&cx, IncomingIndexLimits::default())
                .unwrap();
            assert_eq!(actual(&sync), expected);
            let mut steps = None;
            for q in [1, 3, 7, 257] {
                let guards = Cell::new(0);
                let (result, pending) = drive(
                    source.incoming_index_cooperative_with_checkpoint(
                        &cx,
                        IncomingIndexLimits::default(),
                        quantum(q),
                        yield_now,
                        || {
                            guards.set(guards.get() + 1);
                            Ok::<(), SealedError>(())
                        },
                    ),
                    &guards,
                    q,
                );
                let built = result.unwrap();
                assert_eq!(actual(&built), expected);
                assert_eq!(built.stats(), sync.stats());
                assert_eq!(built.source_anchor(), source.anchor());
                assert!(built.source().shares_image_with(&source));
                assert!(!built.shares_index_with(&sync));
                // Exactly one extra live check per resume. Thus quantum changes
                // do not hide restart/rescan work in construction checkpoints.
                let work = guards.get() - pending;
                assert_eq!(*steps.get_or_insert(work), work);
                if count > 1 && q == 1 {
                    assert!(pending > count);
                }
                for row in &built.index.rows {
                    let mut a = built
                        .row(&cx, row.destination, row.relation, CommitSeq(5))
                        .unwrap();
                    let mut b = sync
                        .row(&cx, row.destination, row.relation, CommitSeq(5))
                        .unwrap();
                    loop {
                        let left = a.next(&cx).unwrap();
                        let right = b.next(&cx).unwrap();
                        match (left, right) {
                            (None, None) => break,
                            (Some(a), Some(b)) => {
                                assert_eq!(a.entry, b.entry);
                                assert!(std::ptr::eq(a.properties.as_ptr(), b.properties.as_ptr()));
                            }
                            _ => panic!("different visibility after cooperative preparation"),
                        }
                    }
                }
            }
        }
    }
    assert_eq!(contexts.outstanding_obligations(), 0);
}

#[derive(Debug)]
enum Refusal {
    Source(SealedError),
    Guard(usize),
}
impl From<SealedError> for Refusal {
    fn from(error: SealedError) -> Self {
        Self::Source(error)
    }
}

#[test]
fn all_live_refusal_cuts_and_suspended_revocations_abandon_without_an_index() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let source = source(24, false);
    let anchor = source.anchor();
    let guards = Cell::new(0);
    let (expected, pauses) = drive(
        source.incoming_index_cooperative_with_checkpoint(
            &cx,
            IncomingIndexLimits::default(),
            quantum(3),
            yield_now,
            || {
                guards.set(guards.get() + 1);
                Ok::<(), Refusal>(())
            },
        ),
        &guards,
        3,
    );
    let expected = expected.unwrap();
    for stop in 1..=guards.get() {
        let seen = Cell::new(0);
        let (result, _) = drive(
            source.incoming_index_cooperative_with_checkpoint(
                &cx,
                IncomingIndexLimits::default(),
                quantum(3),
                yield_now,
                || {
                    seen.set(seen.get() + 1);
                    if seen.get() == stop {
                        Err(Refusal::Guard(stop))
                    } else {
                        Ok(())
                    }
                },
            ),
            &seen,
            3,
        );
        assert!(matches!(result, Err(Refusal::Guard(at)) if at == stop));
        assert_eq!(seen.get(), stop);
        assert_eq!(source.anchor(), anchor);
    }
    for stop in 0..=pauses {
        let revoked = Cell::new(false);
        let seen = Cell::new(0);
        let waker = Waker::from(Arc::new(Wakes::default()));
        let mut task = Context::from_waker(&waker);
        let mut future = Box::pin(source.incoming_index_cooperative_with_checkpoint(
            &cx,
            IncomingIndexLimits::default(),
            quantum(3),
            yield_now,
            || {
                seen.set(seen.get() + 1);
                if revoked.get() {
                    Err(Refusal::Guard(stop))
                } else {
                    Ok(())
                }
            },
        ));
        for _ in 0..stop {
            assert!(future.as_mut().poll(&mut task).is_pending());
        }
        let before = seen.get();
        revoked.set(true);
        let Poll::Ready(result) = future.as_mut().poll(&mut task) else {
            panic!("revocation cannot schedule or populate another slice");
        };
        assert!(matches!(result, Err(Refusal::Guard(at)) if at == stop));
        assert_eq!(seen.get(), before + 1);
        drop(future);
        // Separately test plain drop at the same stage, without driving to EOF.
        let mut abandoned = Box::pin(source.incoming_index_cooperative(
            &cx,
            IncomingIndexLimits::default(),
            quantum(3),
            yield_now,
        ));
        for _ in 0..stop {
            assert!(abandoned.as_mut().poll(&mut task).is_pending());
        }
        drop(abandoned);
        assert_eq!(source.anchor(), anchor);
        assert_eq!(contexts.outstanding_obligations(), 0);
    }
    let retry = runtime
        .block_on(source.incoming_index_cooperative(
            &cx,
            IncomingIndexLimits::default(),
            quantum(7),
            yield_now,
        ))
        .unwrap();
    assert_eq!(actual(&retry), actual(&expected));
    assert_eq!(retry.stats(), expected.stats());
}

#[test]
fn native_allocation_admission_and_typed_source_errors_do_not_reset_on_resume() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let source = source(40, false);
    let sync = source
        .incoming_index(&cx, IncomingIndexLimits::default())
        .unwrap();
    let exact = IncomingIndexLimits {
        max_rows: sync.stats().rows,
        max_incidences: sync.stats().incidences,
        max_workspace_bytes: sync.stats().charged_workspace_bytes,
    };
    for q in [1, 17] {
        let built = runtime
            .block_on(source.incoming_index_cooperative(&cx, exact, quantum(q), yield_now))
            .unwrap();
        assert_eq!(built.stats(), sync.stats());
        for (resource, limits) in [
            (
                "incoming incidences",
                IncomingIndexLimits {
                    max_incidences: exact.max_incidences - 1,
                    ..exact
                },
            ),
            (
                "incoming rows",
                IncomingIndexLimits {
                    max_rows: exact.max_rows - 1,
                    ..exact
                },
            ),
            (
                "incoming workspace bytes",
                IncomingIndexLimits {
                    max_workspace_bytes: exact.max_workspace_bytes - 1,
                    ..exact
                },
            ),
            (
                "incoming workspace bytes",
                IncomingIndexLimits {
                    max_workspace_bytes: 0,
                    ..exact
                },
            ),
        ] {
            let result = runtime.block_on(source.incoming_index_cooperative_with_checkpoint(
                &cx,
                limits,
                quantum(q),
                yield_now,
                || Ok::<(), Refusal>(()),
            ));
            assert!(
                matches!(result, Err(Refusal::Source(SealedError::Limit { resource: actual, .. })) if actual == resource)
            );
            assert!(source.incoming_index(&cx, limits).is_err());
        }
    }
    let mut checks = 0;
    assert!(matches!(
        runtime.block_on(source.incoming_index_cooperative_with_checkpoint(
            &cx,
            IncomingIndexLimits {
                max_incidences: 0,
                ..exact
            },
            quantum(1),
            || -> std::future::Ready<()> {
                panic!("source preflight must precede first scheduling yield")
            },
            || {
                checks += 1;
                Ok::<(), Refusal>(())
            },
        )),
        Err(Refusal::Source(SealedError::Limit {
            resource: "incoming incidences",
            ..
        }))
    ));
    assert_eq!(checks, 1);
}
