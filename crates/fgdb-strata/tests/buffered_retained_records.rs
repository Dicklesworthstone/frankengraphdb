//! Authenticated Strata histories with several simultaneously retained records.
//! The test holds returned values, not a replacement graph or decoder model.
// Proving `Send` for the lab future descends the whole buffered-scan and
// put_root stack, which overflows the default limit of 128.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::edge_props::encode_property_patch;
use fgdb_strata::root::{BlockRef, PartitionRoot, PatchRef};
use fgdb_strata::store::{BlockStore, BufferedReadLimits, BufferedScanError};
use fgdb_strata::tiered::buffer::BufferLimits;
use fgdb_strata::tiered::edge_scan::BufferedEdgeDirection;
use fgdb_strata::tiered::memory::MemoryPool;
use fgdb_strata::vertex::{VertexRow, encode_patch};
use fgdb_strata::{AdjacencyEntry, PartitionRootVersion, encode_block_with_properties};
use fgdb_types::{
    BranchId, CanonicalPropertyValue, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId,
    EId, GraphId, PurposeContexts, VId,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(5);
const P: PropertyKeyId = PropertyKeyId(11);
const PARALLEL: u128 = 16;
const MEMORY: usize = 8 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

fn run<F: Future<Output = ()> + Send>(
    test: impl FnOnce(PurposeContexts, PathBuf) -> F + Send + 'static,
) {
    let path = std::env::temp_dir().join(format!(
        "fgdb-retained-records-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    let ((), report) = run_async_under_lab(0x7c6_101, |root| async move {
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

fn text(value: &str) -> CanonicalPropertyValue {
    CanonicalScalar::ucs_basic_text(value).unwrap().into()
}

fn edge(eid: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        eid: EId(eid),
        src: VId(1),
        dst: VId(if eid == u128::MAX { 1 } else { 2 }),
        relation: R,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn vertex(id: u128, created: u64, retired: Option<u64>, value: &str) -> VertexRow {
    VertexRow {
        vid: VId(id),
        birth_ordinal: id as u64,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
        labels: vec![LabelId(9)],
        props: vec![(P, text(value))],
    }
}

async fn fixture(contexts: &PurposeContexts, path: &Path) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(
        &cx,
        path,
        [0x73; 32],
        DatabaseSecurityNamespaceId([0x36; 32]),
    )
    .await
    .unwrap();
    // The self-loop is first in physical destination order, last in EId order.
    let mut initial = vec![edge(u128::MAX, 1, None)];
    initial.extend((1..=PARALLEL).map(|id| edge(id, 1, None)));
    let definitions = [
        (initial, false),
        (vec![edge(1, 1, Some(2)), edge(1, 2, None)], true),
    ];
    let mut predecessor = None;
    let mut blocks = Vec::new();
    for (entries, update) in definitions {
        let props: Vec<_> = entries
            .iter()
            .map(|entry| {
                vec![(
                    P,
                    if entry.created_at == CommitSeq(2) {
                        text(&"replacement-edge".repeat(32))
                    } else {
                        text("original-edge")
                    },
                )]
            })
            .collect();
        let patch = store
            .put_edge_property_patch(&cx, &encode_property_patch(&props).unwrap())
            .await
            .unwrap();
        let locators: Vec<_> = (1..=entries.len()).map(|index| index as u8).collect();
        let bytes = encode_block_with_properties(
            0,
            predecessor,
            &entries,
            patch.0,
            &locators,
            &props,
        )
        .unwrap();
        let id = store.put(&cx, &bytes).await.unwrap();
        predecessor = Some(id);
        blocks.push(BlockRef {
            block_id: id.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(if update { 2 } else { 1 }),
        });
    }
    let mut patches = Vec::new();
    for rows in [
        vec![vertex(1, 1, None, "source"), vertex(2, 1, None, "target")],
        vec![
            vertex(1, 1, Some(2), "source"),
            vertex(1, 2, None, &"replacement-source".repeat(48)),
        ],
    ] {
        let patch = store
            .put_patch(&cx, &encode_patch(&rows).unwrap())
            .await
            .unwrap();
        patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: rows.iter().map(|row| row.created_at).min().unwrap(),
            last_seq: rows
                .iter()
                .map(|row| row.retired_at.unwrap_or(row.created_at).max(row.created_at))
                .max()
                .unwrap(),
        });
    }
    let root = store
        .put_root(
            &cx,
            &PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(2),
                blocks,
                vertex_patches: patches,
            },
        )
        .await
        .unwrap();
    (store, root)
}

fn source_value(cut: u64) -> CanonicalPropertyValue {
    text(&if cut == 1 {
        "source".to_owned()
    } else {
        "replacement-source".repeat(48)
    })
}

#[test]
fn retained_parallel_edges_and_endpoints_fit_without_charging_decoder_space_per_row() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        // Hold BOTH cuts, including old values after a larger replacement.
        let mut held = Vec::new();
        for cut in [1, 2] {
            let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let mut count = 0;
            while let Some(candidate) = scan
                .next_with_endpoints(&cx, Some(R), &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap()
            {
                let row = candidate.row.unwrap();
                let expected_id = if count == PARALLEL {
                    u128::MAX
                } else {
                    count + 1
                };
                assert_eq!(candidate.eid, EId(expected_id));
                assert_eq!(row.edge().entry.eid, candidate.eid);
                assert_eq!(row.source_vertex().props, vec![(P, source_value(cut))]);
                let edge_value = if cut == 2 && expected_id == 1 {
                    text(&"replacement-edge".repeat(32))
                } else {
                    text("original-edge")
                };
                assert_eq!(row.edge().props, vec![(P, edge_value)]);
                if expected_id == u128::MAX {
                    assert!(std::ptr::eq(row.source_vertex(), row.target_vertex()));
                } else {
                    assert_eq!(row.target_vertex().props, vec![(P, text("target"))]);
                }
                held.push((cut, row));
                count += 1;
                // Initial/root metadata has its separate baseline. This tests
                // live reservations at a completed pull, not peak RSS or I/O.
                assert!(pool.used() < baseline + 256 * 1024);
            }
            assert_eq!(count, PARALLEL + 1);
            scan.close();
            assert!(scan.is_finished());
        }
        drop(view);
        drop(store);
        assert!(pool.used() > 0);
        assert!(pool.used() < 128 * 1024);
        for (cut, row) in &held {
            assert_eq!(row.source_vertex().props, vec![(P, source_value(*cut))]);
        }
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn incidence_restarts_keep_retained_history_and_late_refusals_fuse() {
    run(|contexts, path| async move {
        let (store, root) = fixture(&contexts, &path).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(MEMORY, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, root, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let mut join = view.edge_scan(&cx, CommitSeq(2)).unwrap().into_join_scan();
        let mut held = Vec::new();
        for retained in [false, true] {
            for after in [None, Some(EId(1)), Some(EId(PARALLEL)), None] {
                let result = if retained {
                    join.next_incident_retained_with_endpoints(
                        &cx,
                        VId(1),
                        Some(R),
                        BufferedEdgeDirection::Outgoing,
                        after,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .await
                } else {
                    join.next_incident_with_endpoints(
                        &cx,
                        VId(1),
                        Some(R),
                        BufferedEdgeDirection::Outgoing,
                        after,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .await
                };
                let candidate = result.unwrap().unwrap();
                let expected = match after {
                    None => 1,
                    Some(EId(1)) => 2,
                    _ => u128::MAX,
                };
                assert_eq!(candidate.eid, EId(expected));
                let row = candidate.row.unwrap();
                assert_eq!(row.source_vertex().props, vec![(P, source_value(2))]);
                held.push(row);
                assert!(pool.used() < baseline + 256 * 1024);
            }
        }
        let failure = join
            .next_incident_with_endpoints(
                &cx,
                VId(2),
                Some(R),
                BufferedEdgeDirection::Incoming,
                None,
                &mut |_| Err::<(), _>("stop"),
            )
            .await;
        assert!(matches!(failure, Err(BufferedScanError::Control("stop"))));
        assert!(join.is_closed());
        assert!(
            join.next_with_endpoints(&cx, Some(R), &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap()
                .is_none()
        );
        drop(join);
        drop(view);
        drop(store);
        assert!(pool.used() > 0);
        for row in &held {
            assert_eq!(row.source_vertex().props, vec![(P, source_value(2))]);
        }
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}
