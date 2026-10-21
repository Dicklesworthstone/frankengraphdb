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
    let unrelated = vec![AdjacencyEntry {
        src: VId(2),
        dst: VId(3),
        relation: OTHER,
        eid: EId(500),
        created_at: CommitSeq(1),
        retired_at: None,
    }];
    let definitions = [
        (first, 1, false),
        (unrelated, 1, true),
        (vec![edge(1, 1, Some(2)), edge(1, 2, None)], 2, false),
        (vec![edge(2, 1, Some(3))], 3, false),
    ];
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
