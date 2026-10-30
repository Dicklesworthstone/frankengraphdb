//! Sealing an authenticated buffered generation preserves the existing image.
//! Expected history/values are hand-derived; the old sealer is a byte oracle.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::edge_props::{EdgePropertyRow, encode_property_patch};
use fgdb_strata::root::{BlockRef, PartitionRoot, PatchRef};
use fgdb_strata::store::{BlockStore, BufferedReadLimits};
use fgdb_strata::tiered::buffer::BufferLimits;
use fgdb_strata::tiered::memory::MemoryPool;
use fgdb_strata::tiered::sealed::{SealedError, SealedLimits, SealedPartition};
use fgdb_strata::vertex::{VertexRow, encode_patch};
use fgdb_strata::{AdjacencyEntry, PartitionRootVersion, encode_block_with_properties};
use fgdb_types::{
    BranchId, CanonicalList, CanonicalPropertyValue, CanonicalScalar, CommitSeq,
    DatabaseSecurityNamespaceId, EId, GraphId, PurposeContexts, QueryCx, VId,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(5);
const OTHER: RelationId = RelationId(9);
const P: PropertyKeyId = PropertyKeyId(11);
const RAW: usize = 44;
const MEMORY: usize = 8 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

fn run<F: Future<Output = ()> + Send>(
    test: impl FnOnce(PurposeContexts, PathBuf) -> F + Send + 'static,
) {
    let path = std::env::temp_dir().join(format!(
        "fgdb-buffered-sealing-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    let ((), report) = run_async_under_lab(0x7c6_208, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root), path).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 4096,
        max_source_bytes: 256 * 1024,
        max_blocks: 16,
        max_vertex_patches: 16,
        max_work: 100_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

fn edge(id: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(1),
        dst: VId(if id == u128::MAX { 1 } else { 2 }),
        relation: R,
        eid: EId(id),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn property(id: u128, created: u64) -> CanonicalPropertyValue {
    if id == 3 {
        CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![
                CanonicalScalar::Int(3).into(),
                CanonicalScalar::Null.into(),
            ])
            .unwrap(),
        )
    } else {
        CanonicalScalar::Int(if id == u128::MAX {
            91
        } else if id == 1 && created == 2 {
            99
        } else {
            id as i64
        })
        .into()
    }
}

async fn fixture(contexts: &PurposeContexts, path: &Path) -> (BlockStore, PartitionRootVersion) {
    fixture_with_unrelated_blocks(contexts, path, 1).await
}

async fn fixture_with_unrelated_blocks(
    contexts: &PurposeContexts,
    path: &Path,
    unrelated_blocks: usize,
) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(
        &cx,
        path,
        [0x43; 32],
        DatabaseSecurityNamespaceId([0x34; 32]),
    )
    .await
    .unwrap();
    // CSR-sized descriptor, full-width identity, parallel edges and a loop.
    let mut first = vec![edge(u128::MAX, 1, None)];
    first.extend((1..=40).map(|id| edge(id, 1, None)));
    let mut definitions = vec![(first, 1, false)];
    definitions.extend((0..unrelated_blocks).map(|index| {
        (
            vec![AdjacencyEntry {
                src: VId(2),
                dst: VId(3),
                relation: match index {
                    0 => OTHER,
                    1 => RelationId(0),
                    _ => RelationId(1_000 + index as u64),
                },
                eid: EId(500 + index as u128),
                created_at: CommitSeq(1),
                retired_at: None,
            }],
            1,
            true,
        )
    }));
    definitions.extend([
        (vec![edge(1, 1, Some(2)), edge(1, 2, None)], 2, false),
        (vec![edge(2, 1, Some(3))], 3, false),
    ]);
    let mut predecessor = None;
    let mut blocks = Vec::new();
    for (entries, last, unrelated) in definitions {
        let props: Vec<_> = entries
            .iter()
            .map(|entry| vec![(P, property(entry.eid.0, entry.created_at.0))])
            .collect();
        let patch = store
            .put_edge_property_patch(&cx, &encode_property_patch(&props).unwrap())
            .await
            .unwrap();
        let locators: Vec<_> = (1..=entries.len()).map(|index| index as u8).collect();
        let bytes = encode_block_with_properties(
            0,
            if unrelated { None } else { predecessor },
            &entries,
            patch.0,
            &locators,
            &props,
        )
        .unwrap();
        let id = store.put(&cx, &bytes).await.unwrap();
        if !unrelated {
            predecessor = Some(id);
        }
        blocks.push(BlockRef {
            block_id: id.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(last),
        });
    }
    let vertices: Vec<_> = (1..=4)
        .map(|id| VertexRow {
            vid: VId(id),
            birth_ordinal: id as u64,
            created_at: CommitSeq(1),
            retired_at: None,
            labels: vec![LabelId(7)],
            props: Vec::new(),
        })
        .collect();
    let patch = store
        .put_patch(&cx, &encode_patch(&vertices).unwrap())
        .await
        .unwrap();
    let root = store
        .put_root(
            &cx,
            &PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(3),
                blocks,
                vertex_patches: vec![PatchRef {
                    patch_id: patch.0,
                    first_seq: CommitSeq(1),
                    last_seq: CommitSeq(1),
                }],
            },
        )
        .await
        .unwrap();
    (store, root)
}

fn collect(partition: &SealedPartition, cx: &QueryCx, cut: u64) -> Vec<(EId, EdgePropertyRow)> {
    let mut cursor = partition.row(cx, VId(1), R, CommitSeq(cut)).unwrap();
    let mut result = Vec::new();
    while let Some(row) = cursor.next(cx).unwrap() {
        result.push((row.entry.eid, row.properties.to_vec()));
    }
    result
}

#[test]
fn buffered_sealing_matches_native_bytes_and_independent_history_expectations() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for floor in [1, 2, 3] {
            let old = store
                .seal_relation(&cx, root, CommitSeq(floor), R, SealedLimits::default())
                .await
                .unwrap();
            let image = view
                .seal_relation(&cx, CommitSeq(floor), R, SealedLimits::default(), RAW)
                .await
                .unwrap();
            assert_eq!(image.anchor(), old.anchor());
            assert_eq!(
                image.encode(&cx, SealedLimits::default()).unwrap(),
                old.encode(&cx, SealedLimits::default()).unwrap()
            );
            for cut in floor..=3 {
                let mut expected = vec![(EId(u128::MAX), vec![(P, property(u128::MAX, 1))])];
                expected.extend((1..=40).filter(|&id| id != 2 || cut < 3).map(|id| {
                    (
                        EId(id),
                        vec![(P, property(id, if id == 1 && cut >= 2 { 2 } else { 1 }))],
                    )
                }));
                assert_eq!(collect(&image, &cx, cut), expected);
            }
            assert!(
                pool.used() <= baseline,
                "raw source charges survive image completion"
            );
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn sealing_interleaved_relation_ranges_preserves_retire_precedence_and_selected_faults() {
    run(|contexts, path| async move {
        let cx = contexts.query();
        let mut selected_faults = None;
        for unrelated in [1, 128] {
            let path = path.join(unrelated.to_string());
            std::fs::create_dir(&path).unwrap();
            let (store, root) = fixture_with_unrelated_blocks(&contexts, &path, unrelated).await;
            let pool = MemoryPool::new(MEMORY, 0).unwrap();
            let mut view = store
                .open_buffered_root(
                    &cx,
                    root,
                    pool.clone(),
                    BufferedReadLimits {
                        max_root_bytes: 64 * 1024,
                        max_source_bytes: 4 * 1024 * 1024,
                        max_blocks: 256,
                        max_work: 1_000_000,
                        ..limits()
                    },
                )
                .await
                .unwrap();
            let baseline = pool.used();
            let before = view.stats();
            assert!(matches!(
                view.seal_relation(&cx, CommitSeq(1), R, SealedLimits::default(), RAW - 1)
                    .await,
                Err(SealedError::Limit {
                    resource: "sealed source incidences",
                    ..
                })
            ));
            assert_eq!(
                view.stats(),
                before,
                "raw admission precedes payload faults"
            );
            assert_eq!(pool.used(), baseline);
            let image = view
                .seal_relation(&cx, CommitSeq(1), R, SealedLimits::default(), RAW)
                .await
                .unwrap();
            let faults = view.stats().misses - before.misses;
            assert_eq!(
                faults, 6,
                "three selected topology blocks and their three patches are each faulted once"
            );
            if let Some(first) = selected_faults {
                assert_eq!(faults, first, "foreign relation payloads stay unfaulted");
            } else {
                selected_faults = Some(faults);
            }
            let resident = store
                .seal_relation(&cx, root, CommitSeq(1), R, SealedLimits::default())
                .await
                .unwrap();
            assert_eq!(
                image.encode(&cx, SealedLimits::default()).unwrap(),
                resident.encode(&cx, SealedLimits::default()).unwrap()
            );
            for cut in 1..=3 {
                let mut expected = vec![(EId(u128::MAX), vec![(P, property(u128::MAX, 1))])];
                expected.extend((1..=40).filter(|&id| id != 2 || cut < 3).map(|id| {
                    (
                        EId(id),
                        vec![(P, property(id, if id == 1 && cut >= 2 { 2 } else { 1 }))],
                    )
                }));
                // EId(2)'s later block has the SAME creation sequence as its
                // original row. Publication order must retain its retirement.
                // EId(3)'s nested NULL property also survives source transfer.
                assert_eq!(collect(&image, &cx, cut), expected);
            }
            let before = view.stats();
            let absent = view
                .seal_relation(
                    &cx,
                    CommitSeq(1),
                    RelationId(u64::MAX),
                    SealedLimits::default(),
                    0,
                )
                .await
                .unwrap();
            assert_eq!(absent.stats().incidences, 0);
            assert_eq!(view.stats(), before);
            assert!(pool.used() <= baseline);
            drop(view);
            assert_eq!(pool.used(), 0);
        }
    });
}

#[test]
fn raw_source_and_output_limits_are_separate_and_failures_leave_a_reusable_view() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let before = view.stats();
        assert!(matches!(
            view.seal_relation(&cx, CommitSeq(3), R, SealedLimits::default(), RAW - 1,)
                .await,
            Err(SealedError::Limit {
                resource: "sealed source incidences",
                ..
            })
        ));
        assert_eq!(
            view.stats(),
            before,
            "raw admission must precede payload faults"
        );
        let exact = SealedLimits {
            max_incidences: 40,
            ..SealedLimits::default()
        };
        let valid = view
            .seal_relation(&cx, CommitSeq(3), R, exact, RAW)
            .await
            .unwrap();
        assert_eq!(valid.stats().incidences, 40);
        for bad in [
            SealedLimits {
                max_rows: 0,
                ..exact
            },
            SealedLimits {
                max_incidences: 39,
                ..exact
            },
            SealedLimits {
                max_image_bytes: 0,
                ..exact
            },
            SealedLimits {
                max_property_bytes: 0,
                ..exact
            },
        ] {
            assert!(
                view.seal_relation(&cx, CommitSeq(3), R, bad, RAW)
                    .await
                    .is_err()
            );
            assert!(pool.used() <= baseline);
            let retry = view
                .seal_relation(&cx, CommitSeq(3), R, exact, RAW)
                .await
                .unwrap();
            assert_eq!(retry.anchor(), valid.anchor());
        }
        drop(view);
        assert_eq!(
            pool.used(),
            0,
            "derived image does not retain raw source charges"
        );
        assert_eq!(collect(&valid, &cx, 3).len(), 40);
    });
}

#[test]
fn empty_relation_and_future_floor_never_fault_payload_or_widen_the_receipt() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let before = view.stats();
        assert!(matches!(
            view.seal_relation(&cx, CommitSeq(4), R, SealedLimits::default(), RAW,)
                .await,
            Err(SealedError::InvalidFloor)
        ));
        assert_eq!(view.stats(), before);
        let missing = RelationId(99);
        let image = view
            .seal_relation(&cx, CommitSeq(1), missing, SealedLimits::default(), 0)
            .await
            .unwrap();
        assert_eq!(view.stats(), before);
        assert_eq!(image.stats().incidences, 0);
        assert_eq!(image.anchor().relation(), Some(missing));
        assert_eq!(image.anchor().scope().source_root, root);
        assert!(
            image
                .row(&cx, VId(1), missing, CommitSeq(1))
                .unwrap()
                .next(&cx)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            image.row(&cx, VId(1), R, CommitSeq(1)),
            Err(SealedError::RelationOutsideAnchor { .. })
        ));
        let old = store
            .seal_relation(&cx, root, CommitSeq(1), missing, SealedLimits::default())
            .await
            .unwrap();
        assert_eq!(image.anchor(), old.anchor());
    });
}

#[test]
fn source_pool_exhaustion_returns_no_image_and_refunds_every_transient_owner() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        // Leave less than one decoder reservation; source admission must refuse.
        let held = pool.reserve(&cx, MEMORY - baseline - 4096).unwrap();
        assert!(matches!(
            view.seal_relation(&cx, CommitSeq(1), R, SealedLimits::default(), RAW,)
                .await,
            Err(SealedError::BufferedSource(_))
        ));
        assert!(pool.used() <= baseline + held.bytes());
        drop(held);
        let image = view
            .seal_relation(&cx, CommitSeq(1), R, SealedLimits::default(), RAW)
            .await
            .unwrap();
        assert_eq!(collect(&image, &cx, 1).len(), 41);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

fn family_vid(source: usize) -> VId {
    VId((1u128 << 127) | ((source as u128) << 64) | source as u128)
}

fn family_property(source: usize, payload_bytes: usize) -> CanonicalPropertyValue {
    CanonicalScalar::bytes(vec![source as u8; payload_bytes])
        .unwrap()
        .into()
}

async fn family_fixture(
    contexts: &PurposeContexts,
    path: &Path,
    families: usize,
    payload_bytes: usize,
) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(
        &cx,
        path,
        [0x53; 32],
        DatabaseSecurityNamespaceId([0x35; 32]),
    )
    .await
    .unwrap();
    // Reverse source order at birth, then interleave retirements in a different
    // order. Every family keeps its complete immutable-EId publication chain.
    let mut publications: Vec<_> = (1..=families).rev().map(|source| (source, None)).collect();
    for retirement in [3, 4] {
        publications.extend(
            (1..=families)
                .filter(|&source| (if source % 2 == 0 { 3 } else { 4 }) == retirement)
                .map(|source| (source, Some(retirement))),
        );
    }
    let mut previous = vec![None; families];
    let mut blocks = Vec::new();
    for (source, retired) in publications {
        let entries = [AdjacencyEntry {
            src: family_vid(source),
            dst: VId(0),
            relation: R,
            eid: EId((1u128 << 126) | source as u128),
            created_at: CommitSeq(1),
            retired_at: retired.map(CommitSeq),
        }];
        let props = vec![vec![(P, family_property(source, payload_bytes))]];
        let patch = store
            .put_edge_property_patch(&cx, &encode_property_patch(&props).unwrap())
            .await
            .unwrap();
        let bytes =
            encode_block_with_properties(0, previous[source - 1], &entries, patch.0, &[1], &props)
                .unwrap();
        let id = store.put(&cx, &bytes).await.unwrap();
        previous[source - 1] = Some(id);
        blocks.push(BlockRef {
            block_id: id.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(retired.unwrap_or(1)),
        });
    }
    let vertices: Vec<_> = (0..=families)
        .map(|source| VertexRow {
            vid: if source == 0 {
                VId(0)
            } else {
                family_vid(source)
            },
            birth_ordinal: source as u64,
            created_at: CommitSeq(1),
            retired_at: None,
            labels: vec![LabelId(7)],
            props: Vec::new(),
        })
        .collect();
    let mut patches = Vec::new();
    for rows in vertices.chunks(64) {
        let patch = store
            .put_patch(&cx, &encode_patch(rows).unwrap())
            .await
            .unwrap();
        patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(1),
        });
    }
    let root = store
        .put_root(
            &cx,
            &PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(4),
                blocks,
                vertex_patches: patches,
            },
        )
        .await
        .unwrap();
    (store, root)
}

fn family_limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 8 * 1024 * 1024,
        max_blocks: 512,
        max_work: 1_000_000,
        ..limits()
    }
}

#[test]
fn source_family_sealing_fits_below_total_raw_history_and_preserves_exact_images() {
    run(|contexts, path| async move {
        const FAMILIES: usize = 128;
        const PAYLOAD: usize = 8192;
        const HEADROOM: usize = 1536 * 1024;
        let (store, root) = family_fixture(&contexts, &path, FAMILIES, PAYLOAD).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), family_limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let held = pool.reserve(&cx, MEMORY - baseline - HEADROOM).unwrap();
        // This is a lower bound on the old all-family source's OWNED payloads,
        // excluding every vector, routing entry, row and decoder workspace.
        // Retaining all statements cannot fit in this admitted source pool.
        assert!(2 * FAMILIES * PAYLOAD > HEADROOM);
        let before = view.stats();
        assert!(matches!(
            view.seal_relation(
                &cx,
                CommitSeq(1),
                R,
                SealedLimits::default(),
                2 * FAMILIES - 1,
            )
            .await,
            Err(SealedError::Limit {
                resource: "sealed source incidences",
                ..
            })
        ));
        assert_eq!(view.stats(), before);
        assert_eq!(pool.used(), baseline + held.bytes());
        for floor in [1, 3, 4] {
            let expected = store
                .seal_relation(&cx, root, CommitSeq(floor), R, SealedLimits::default())
                .await
                .unwrap();
            let before = view.stats();
            let image = view
                .seal_relation(
                    &cx,
                    CommitSeq(floor),
                    R,
                    SealedLimits::default(),
                    2 * FAMILIES,
                )
                .await
                .unwrap();
            assert_eq!(
                view.stats().misses - before.misses,
                (4 * FAMILIES) as u64,
                "two topology statements and two hosted patches per source, without a reread"
            );
            assert_eq!(image.anchor(), expected.anchor());
            assert_eq!(
                image.encode(&cx, SealedLimits::default()).unwrap(),
                expected.encode(&cx, SealedLimits::default()).unwrap()
            );
            for cut in floor..=4 {
                for source in 1..=FAMILIES {
                    let retirement = if source % 2 == 0 { 3 } else { 4 };
                    let mut cursor = image
                        .row(&cx, family_vid(source), R, CommitSeq(cut))
                        .unwrap();
                    match cursor.next(&cx).unwrap() {
                        Some(row) => {
                            assert!(cut < retirement);
                            assert_eq!(row.entry.eid, EId((1u128 << 126) | source as u128));
                            assert_eq!(row.properties, &[(P, family_property(source, PAYLOAD))]);
                            assert!(cursor.next(&cx).unwrap().is_none());
                        }
                        None => assert!(cut >= retirement),
                    }
                }
            }
            assert!(pool.used() <= baseline + held.bytes());
        }
        // An aggregate output refusal after many completed families releases
        // every private output and source owner; the view can build again.
        assert!(matches!(
            view.seal_relation(
                &cx,
                CommitSeq(1),
                R,
                SealedLimits {
                    max_rows: FAMILIES - 1,
                    ..SealedLimits::default()
                },
                2 * FAMILIES,
            )
            .await,
            Err(SealedError::Limit {
                resource: "descriptors",
                ..
            })
        ));
        assert!(pool.used() <= baseline + held.bytes());
        assert_eq!(
            view.seal_relation(&cx, CommitSeq(1), R, SealedLimits::default(), 2 * FAMILIES,)
                .await
                .unwrap()
                .stats()
                .rows,
            FAMILIES
        );
        drop(held);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn each_source_family_sealing_checkpoint_refunds_and_keeps_the_reader_usable() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    run(|contexts, path| async move {
        let (store, root) = family_fixture(&contexts, &path, 3, 8).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), family_limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let observed = Arc::new(SimulationCheckpointProbe::new(None));
        let measuring = cx.with_checkpoint_probe(Arc::clone(&observed));
        let expected = view
            .seal_relation(&measuring, CommitSeq(1), R, SealedLimits::default(), 6)
            .await
            .unwrap();
        let checkpoints = observed.calls();
        assert!(checkpoints > 3);
        for stop in 1..=checkpoints {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let cancelled = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(
                view.seal_relation(&cancelled, CommitSeq(1), R, SealedLimits::default(), 6)
                    .await
                    .is_err(),
                "checkpoint {stop}"
            );
            assert_eq!(probe.calls(), stop);
            assert!(pool.used() <= baseline, "checkpoint {stop}");
            let retry = view
                .seal_relation(&cx, CommitSeq(1), R, SealedLimits::default(), 6)
                .await
                .unwrap();
            assert_eq!(retry.anchor(), expected.anchor(), "checkpoint {stop}");
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
