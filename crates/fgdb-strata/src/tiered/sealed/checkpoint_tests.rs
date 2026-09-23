//! Typed caller refusals at the real index/cursor checkpoint seams. The private
//! image fixture is storage-unit evidence, not a fabricated public root receipt.

use super::*;
use super::super::{SealedCursor, SealedLimits, SealedScope, image};
use crate::{AdjacencyEntry, PartitionRootVersion};
use crate::compact::Compaction;
use fgdb_types::{BranchId, EId, GraphId, ObjectId};

#[derive(Debug)]
enum Refusal {
    Source(SealedError),
    Guard(usize),
}
impl From<SealedError> for Refusal {
    fn from(error: SealedError) -> Self { Self::Source(error) }
}

fn entry(src: u128, dst: u128, eid: u128, hidden: bool) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src), dst: VId(dst), eid: EId(eid), relation: RelationId(1),
        created_at: CommitSeq(1), retired_at: hidden.then_some(CommitSeq(3)),
    }
}

fn source(entries: Vec<AdjacencyEntry>) -> SealedPartition {
    let blocks: Vec<_> = entries.chunks(120).map(<[AdjacencyEntry]>::to_vec).collect();
    let block_props = (0..blocks.len()).map(|_| None).collect();
    let limits = SealedLimits::default();
    let image = image::build(Compaction {
        blocks, block_props, dropped: 0, superseded: 0,
    }, limits, &mut || Ok(())).unwrap();
    SealedPartition::finish(SealedScope {
        source_root: PartitionRootVersion(ObjectId([0x35; 32])),
        graph: GraphId(1), branch: BranchId(2), partition: 3,
        floor: CommitSeq(1), publication: CommitSeq(10),
    }, image, limits, &mut || Ok(())).unwrap()
}

#[test]
fn typed_guard_abandons_each_sort_directory_and_encoding_checkpoint() {
    // Alternating destinations force permutation sorting rather than its
    // already-ordered fast path. The original image is never modified.
    let source = source((0..24).map(|i| entry(i, 7 + i % 3, i, false)).collect());
    let anchor = source.anchor();
    let limits = IncomingIndexLimits::default();
    let mut total = 0;
    let expected = SealedIncomingIndex::build_controlled(&source, limits, &mut || {
        total += 1;
        Ok::<(), Refusal>(())
    }).unwrap();
    assert!(total > 100);
    for stop in 1..=total {
        let mut calls = 0;
        let result = SealedIncomingIndex::build_controlled(&source, limits, &mut || {
            calls += 1;
            if calls == stop { Err(Refusal::Guard(stop)) } else { Ok(()) }
        });
        assert!(matches!(result, Err(Refusal::Guard(at)) if at == stop));
        assert_eq!(calls, stop);
        assert_eq!(source.anchor(), anchor);
    }
    let retried = SealedIncomingIndex::build(&source, limits, &mut || Ok(())).unwrap();
    assert_eq!(retried.stats(), expected.stats());
    assert!(retried.source().shares_image_with(&source));
}

fn outgoing(source: &SealedPartition) -> SealedCursor<'_> {
    SealedCursor {
        image: &source.image,
        row: source.image.find_row(VId(1), RelationId(1)),
        position: 0, as_of: CommitSeq(5), finished: false,
    }
}

#[test]
fn invisible_versions_and_chunk_boundaries_keep_typed_failures_terminal() {
    // More than one EF chunk, with no visible answer until the final record.
    let mut entries: Vec<_> = (0..260).map(|i| entry(1, 7, i, true)).collect();
    entries.push(entry(1, 7, 999, false));
    let source = source(entries);
    let index = SealedIncomingIndex::build(&source, IncomingIndexLimits::default(), &mut || Ok(())).unwrap();
    assert!(index.stats().chunks > 1);
    for reversed in [false, true] {
        let mut total = 0;
        let mut out = outgoing(&source);
        let mut incoming = index.open(VId(7), RelationId(1), CommitSeq(5), None, &mut || Ok(())).unwrap();
        let mut count = 0;
        loop {
            let mut guard = || { total += 1; Ok::<(), Refusal>(()) };
            let edge = if reversed {
                incoming.next_controlled(&mut guard)
            } else { out.next_controlled(&mut guard) }.unwrap();
            let Some(edge) = edge else { break; };
            assert_eq!(edge.entry.eid, EId(999));
            count += 1;
        }
        assert_eq!(count, 1);
        assert!(total > 4, "hidden history must contain guard boundaries");
        for stop in 1..=total {
            let mut out = outgoing(&source);
            let mut incoming = index.open(VId(7), RelationId(1), CommitSeq(5), None, &mut || Ok(())).unwrap();
            let mut calls = 0;
            loop {
                let mut guard = || {
                    calls += 1;
                    if calls == stop { Err(Refusal::Guard(stop)) } else { Ok(()) }
                };
                let result = if reversed {
                    incoming.next_controlled(&mut guard)
                } else { out.next_controlled(&mut guard) };
                match result {
                    Err(Refusal::Guard(at)) => { assert_eq!(at, stop); break; }
                    Ok(Some(_)) => {}
                    other => panic!("missing guard failure: {other:?}"),
                }
            }
            assert_eq!(calls, stop);
            let mut resumed = || -> Result<(), Refusal> { panic!("failed cursor resumed") };
            let result = if reversed {
                incoming.next_controlled(&mut resumed)
            } else { out.next_controlled(&mut resumed) };
            assert!(result.unwrap().is_none());
        }
    }
}

#[test]
fn native_quota_errors_stay_source_errors_and_empty_indices_still_checkpoint() {
    let populated = source(vec![entry(1, 2, 3, false)]);
    let result = SealedIncomingIndex::build_controlled(&populated, IncomingIndexLimits {
        max_incidences: 0, ..IncomingIndexLimits::default()
    }, &mut || Ok::<(), Refusal>(()));
    assert!(matches!(result, Err(Refusal::Source(SealedError::Limit {
        resource: "incoming incidences", requested: 1, limit: 0,
    }))));
    let empty = source(Vec::new());
    assert!(matches!(SealedIncomingIndex::build_controlled(&empty, IncomingIndexLimits::default(),
        &mut || Err::<(), Refusal>(Refusal::Guard(1))), Err(Refusal::Guard(1))));
    let index = SealedIncomingIndex::build_controlled(&empty, IncomingIndexLimits::default(),
        &mut || Ok::<(), Refusal>(())).unwrap();
    assert_eq!(index.stats().incidences, 0);
}
