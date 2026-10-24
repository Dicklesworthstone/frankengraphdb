//! A cold source consumes the same durable objects as reopen, with independently
//! specified point/adjacency answers and a one-frame eviction/refault boundary.
// Commit-pack reads nest the store's futures past the default recursion
// limit when a lab test proves them Send (see fgdb-strata's lib.rs).
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::edge_props::encode_property_patch;
use fgdb_strata::root::{BlockRef, PartitionRoot, PatchRef};
use fgdb_strata::store::{
    BlockStore, BufferedReadError, BufferedReadLimits, BufferedScanError, BufferedScanEvent,
    StoreError,
};
use fgdb_strata::tiered::buffer::{BufferError, BufferLimits, BufferStats};
use fgdb_strata::tiered::memory::{MemoryError, MemoryPool};
use fgdb_strata::vertex::{VertexRow, encode_patch};
use fgdb_strata::{
    AdjacencyEntry, DeltaBlockVersion, PartitionRootVersion, encode_block,
    encode_block_with_properties,
};
use fgdb_types::{
    BranchId, CanonicalPropertyValue, CanonicalScalar, CommitCx, CommitSeq,
    DatabaseSecurityNamespaceId, EId, GraphId, PurposeContexts, VId,
};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const KEY: [u8; 32] = [0x31; 32];
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x52; 32]);
const REL: RelationId = RelationId(5);
static NEXT: AtomicU64 = AtomicU64::new(0);

fn run<Fut>(test: impl FnOnce(PurposeContexts, PathBuf) -> Fut + Send + 'static)
where
    Fut: Future<Output = ()> + Send,
{
    let dir = std::env::temp_dir().join(format!(
        "fgdb-cold-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    let (_, report) = run_async_under_lab(0xb0_0f, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root), dir).await;
    });
    assert!(report.invariant_violations.is_empty(), "{report:?}");
}

fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 4096,
        max_source_bytes: 64 * 1024,
        max_blocks: 32,
        max_vertex_patches: 32,
        max_work: 1024,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

// Directory construction has a different allowance from a later cursor's
// source work. Keep its retained reservation explicit in resident-cap laws.
fn patch_directory_overhead(patches: usize) -> (usize, usize) {
    // Each Range holds three VIds and a patch ordinal, including trailing
    // alignment padding. Admission reserves one full entry per patch.
    let (entry, _) = std::alloc::Layout::new::<[VId; 3]>()
        .extend(std::alloc::Layout::new::<usize>())
        .unwrap();
    let reserved_bytes = patches * entry.pad_to_align().size();
    // Fill and augment each visit at most P entries. Heapify plus extraction
    // invokes at most 3P/2 sifts, each with two controls per tree level;
    // extraction also spends at most P controls on swaps.
    let height = patches.checked_ilog2().unwrap_or(0) as usize;
    let work = patches * (3 * height + 3);
    (reserved_bytes, work)
}

fn edge(eid: u128, src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        eid: EId(eid),
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn vertex(created: u64, retired: Option<u64>, value: i64) -> VertexRow {
    VertexRow {
        vid: VId(1),
        birth_ordinal: 7,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
        labels: vec![LabelId(9)],
        props: vec![(PropertyKeyId(11), CanonicalScalar::Int(value).into())],
    }
}

fn scan_vertex(vid: u128, created: u64, retired: Option<u64>, value: i64) -> VertexRow {
    VertexRow {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        ..vertex(created, retired, value)
    }
}

async fn vertex_scan_fixture(
    contexts: &PurposeContexts,
    dir: &std::path::Path,
) -> (BlockStore, PartitionRootVersion) {
    let commit = contexts.commit();
    let store = BlockStore::open(&commit, dir, KEY, NS).await.unwrap();
    let mut patches = Vec::new();
    for (rows, first, last) in [
        (
            vec![
                scan_vertex(1, 1, None, 10),
                scan_vertex(3, 2, None, 30),
                scan_vertex(5, 4, None, 50),
            ],
            1,
            4,
        ),
        (
            vec![
                scan_vertex(1, 1, Some(3), 10),
                scan_vertex(1, 3, None, 11),
                scan_vertex(2, 2, None, 20),
                scan_vertex(3, 2, Some(4), 30),
            ],
            1,
            4,
        ),
        (
            vec![
                scan_vertex(1, 3, Some(5), 11),
                scan_vertex(1, 5, None, 12),
                scan_vertex(4, 5, None, 40),
            ],
            3,
            5,
        ),
    ] {
        let patch = store
            .put_patch(&commit, &encode_patch(&rows).unwrap())
            .await
            .unwrap();
        patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(first),
            last_seq: CommitSeq(last),
        });
    }
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(5),
        blocks: vec![],
        vertex_patches: patches,
    };
    let id = store.put_root(&commit, &root).await.unwrap();
    (store, id)
}

async fn fixture(
    contexts: &PurposeContexts,
    dir: &std::path::Path,
) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(&cx, dir, KEY, NS).await.unwrap();
    let first = store
        .put(
            &cx,
            &encode_block(0, None, &[edge(1, 1, 2, 1, None)]).unwrap(),
        )
        .await
        .unwrap();
    let other = store
        .put(
            &cx,
            &encode_block(0, None, &[edge(2, 3, 2, 2, None)]).unwrap(),
        )
        .await
        .unwrap();
    let rows = vec![vec![(
        PropertyKeyId(11),
        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(33)),
    )]];
    let properties = store
        .put_edge_property_patch(&cx, &encode_property_patch(&rows).unwrap())
        .await
        .unwrap();
    let last = store
        .put(
            &cx,
            &encode_block_with_properties(
                0,
                Some(first),
                &[edge(1, 1, 2, 1, Some(3)), edge(1, 1, 2, 3, None)],
                properties.0,
                &[0, 1],
                &rows,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let old = store
        .put_patch(&cx, &encode_patch(&[vertex(1, None, 10)]).unwrap())
        .await
        .unwrap();
    let new = store
        .put_patch(
            &cx,
            &encode_patch(&[vertex(1, Some(3), 10), vertex(3, None, 30)]).unwrap(),
        )
        .await
        .unwrap();
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(3),
        blocks: vec![
            BlockRef {
                block_id: first.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            },
            BlockRef {
                block_id: other.0,
                first_seq: CommitSeq(2),
                last_seq: CommitSeq(2),
            },
            BlockRef {
                block_id: last.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(3),
            },
        ],
        vertex_patches: vec![
            PatchRef {
                patch_id: old.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            },
            PatchRef {
                patch_id: new.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(3),
            },
        ],
    };
    let id = store.put_root(&cx, &root).await.unwrap();
    (store, id)
}

async fn propertied_block(
    store: &BlockStore,
    cx: &CommitCx,
    predecessor: Option<DeltaBlockVersion>,
    entries: &[AdjacencyEntry],
    values: &[Option<i64>],
) -> DeltaBlockVersion {
    assert_eq!(entries.len(), values.len());
    let mut rows = Vec::new();
    let mut locators = Vec::new();
    for value in values {
        if let Some(value) = value {
            rows.push(vec![(
                PropertyKeyId(11),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(*value)),
            )]);
            locators.push(u8::try_from(rows.len()).unwrap());
        } else {
            locators.push(0);
        }
    }
    let patch = store
        .put_edge_property_patch(cx, &encode_property_patch(&rows).unwrap())
        .await
        .unwrap();
    store
        .put(
            cx,
            &encode_block_with_properties(0, predecessor, entries, patch.0, &locators, &rows)
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn routed_fixture(
    contexts: &PurposeContexts,
    dir: &std::path::Path,
) -> (BlockStore, PartitionRootVersion) {
    let cx = contexts.commit();
    let store = BlockStore::open(&cx, dir, KEY, NS).await.unwrap();
    // Neither end of adjacency order supplies the EId interval: 20 is
    // interior, while the maximum 90 is the first physical entry.
    let first = propertied_block(
        &store,
        &cx,
        None,
        &[
            edge(90, 1, 2, 1, None),
            edge(20, 1, 3, 1, None),
            edge(80, 1, 5, 1, None),
        ],
        &[Some(900), None, Some(800)],
    )
    .await;
    let initial_vertex = store
        .put_patch(&cx, &encode_patch(&[vertex(1, None, 11)]).unwrap())
        .await
        .unwrap();
    let mut blocks = vec![BlockRef {
        block_id: first.0,
        first_seq: CommitSeq(1),
        last_seq: CommitSeq(1),
    }];
    let mut patches = vec![PatchRef {
        patch_id: initial_vertex.0,
        first_seq: CommitSeq(1),
        last_seq: CommitSeq(1),
    }];
    for index in 0..20 {
        let unrelated = propertied_block(
            &store,
            &cx,
            None,
            &[edge(1000 + index, 100 + index, 1000 + index, 1, None)],
            &[Some(index as i64)],
        )
        .await;
        blocks.push(BlockRef {
            block_id: unrelated.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(1),
        });
        let patch = store
            .put_patch(
                &cx,
                &encode_patch(&[scan_vertex(100 + index, 1, None, index as i64)]).unwrap(),
            )
            .await
            .unwrap();
        patches.push(PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(1),
            last_seq: CommitSeq(1),
        });
    }
    let other_relation = propertied_block(
        &store,
        &cx,
        None,
        &[AdjacencyEntry {
            relation: RelationId(9),
            ..edge(4, 1, 2, 2, None)
        }],
        &[Some(40)],
    )
    .await;
    blocks.push(BlockRef {
        block_id: other_relation.0,
        first_seq: CommitSeq(2),
        last_seq: CommitSeq(2),
    });
    let successor = propertied_block(
        &store,
        &cx,
        Some(first),
        &[
            edge(90, 1, 2, 1, Some(3)),
            edge(90, 1, 2, 3, None),
            edge(91, 1, 2, 3, None),
            edge(20, 1, 3, 1, Some(3)),
        ],
        &[Some(900), Some(901), Some(910), None],
    )
    .await;
    blocks.push(BlockRef {
        block_id: successor.0,
        first_seq: CommitSeq(1),
        last_seq: CommitSeq(3),
    });
    let latest_vertex = store
        .put_patch(
            &cx,
            &encode_patch(&[vertex(1, Some(3), 11), vertex(3, None, 13)]).unwrap(),
        )
        .await
        .unwrap();
    patches.push(PatchRef {
        patch_id: latest_vertex.0,
        first_seq: CommitSeq(1),
        last_seq: CommitSeq(3),
    });
    let id = store
        .put_root(
            &cx,
            &PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(3),
                blocks,
                vertex_patches: patches,
            },
        )
        .await
        .unwrap();
    (store, id)
}

fn extent_visits(stats: BufferStats) -> u64 {
    stats.hits + stats.misses
}

#[test]
fn edge_point_and_adjacency_routes_keep_publication_winners_before_liveness() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let first = propertied_block(
            &store,
            &commit,
            None,
            &[edge(10, 7, 10, 1, None), edge(50, 7, 50, 1, None)],
            &[Some(100), Some(500)],
        )
        .await;
        // Both interval families put this later publication at their root.
        // Retirement restatements tie on created_at, but retain this ordinal.
        let later = propertied_block(
            &store,
            &commit,
            Some(first),
            &[edge(50, 7, 50, 1, Some(3)), edge(90, 7, 90, 3, None)],
            &[Some(500), Some(900)],
        )
        .await;
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(3),
                    blocks: vec![
                        BlockRef {
                            block_id: first.0,
                            first_seq: CommitSeq(1),
                            last_seq: CommitSeq(1),
                        },
                        BlockRef {
                            block_id: later.0,
                            first_seq: CommitSeq(1),
                            last_seq: CommitSeq(3),
                        },
                    ],
                    vertex_patches: vec![],
                },
            )
            .await
            .unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&query, id, pool.clone(), limits())
            .await
            .unwrap();
        let before = extent_visits(view.stats());
        let old = view
            .edge_at(&query, EId(50), CommitSeq(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(old.entry, edge(50, 7, 50, 1, Some(3)));
        assert_eq!(
            old.props,
            vec![(PropertyKeyId(11), CanonicalScalar::Int(500).into())]
        );
        assert_eq!(extent_visits(view.stats()) - before, 3);
        drop(old);
        let before = extent_visits(view.stats());
        assert!(
            view.edge_at(&query, EId(50), CommitSeq(3))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            extent_visits(view.stats()) - before,
            2,
            "retired winner has no sidecar read"
        );
        for relation in [None, Some(REL)] {
            for (cut, expected) in [
                (
                    1,
                    vec![edge(10, 7, 10, 1, None), edge(50, 7, 50, 1, Some(3))],
                ),
                (3, vec![edge(10, 7, 10, 1, None), edge(90, 7, 90, 3, None)]),
            ] {
                let before = extent_visits(view.stats());
                let found = view
                    .adjacency_at(&query, VId(7), relation, false, CommitSeq(cut), 3)
                    .await
                    .unwrap();
                assert_eq!(found.as_ref(), &expected);
                assert_eq!(extent_visits(view.stats()) - before, 2, "topology only");
            }
            for (cut, expected) in [(1, vec![edge(50, 7, 50, 1, Some(3))]), (3, vec![])] {
                let before = extent_visits(view.stats());
                let found = view
                    .adjacency_at(&query, VId(50), relation, true, CommitSeq(cut), 1)
                    .await
                    .unwrap();
                assert_eq!(found.as_ref(), &expected);
                assert_eq!(
                    extent_visits(view.stats()) - before,
                    2,
                    "complete historical candidates"
                );
            }
        }
        assert!(matches!(
            view.adjacency_at(&query, VId(7), None, false, CommitSeq(3), 2)
                .await,
            Err(BufferedReadError::Limit {
                resource: "buffered adjacency identities",
                requested: 3,
                limit: 2,
            })
        ));
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn cold_points_select_all_matching_histories_and_only_the_winning_property_patch() {
    run(|contexts, dir| async move {
        let (store, id) = routed_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        assert_eq!(extent_visits(view.stats()), 0);
        for (cut, expected) in [(1, vertex(1, Some(3), 11)), (3, vertex(3, None, 13))] {
            let before = extent_visits(view.stats());
            let row = view
                .vertex_at(&cx, VId(1), CommitSeq(cut))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.as_ref(), &expected);
            assert_eq!(extent_visits(view.stats()) - before, 2);
        }
        for (eid, cut, expected, props, visits) in [
            (
                90,
                1,
                Some(edge(90, 1, 2, 1, Some(3))),
                vec![(
                    PropertyKeyId(11),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(900)),
                )],
                3,
            ),
            (
                90,
                3,
                Some(edge(90, 1, 2, 3, None)),
                vec![(
                    PropertyKeyId(11),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(901)),
                )],
                3,
            ),
            (
                80,
                3,
                Some(edge(80, 1, 5, 1, None)),
                vec![(
                    PropertyKeyId(11),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(800)),
                )],
                3,
            ),
            (20, 1, Some(edge(20, 1, 3, 1, Some(3))), vec![], 2),
            (20, 3, None, vec![], 2),
            (85, 3, None, vec![], 2),
            (u128::MAX, 3, None, vec![], 0),
            (90, 0, None, vec![], 0),
        ] {
            let before = extent_visits(view.stats());
            let row = view.edge_at(&cx, EId(eid), CommitSeq(cut)).await.unwrap();
            assert_eq!(
                row.as_ref().map(|row| row.entry),
                expected,
                "edge {eid} at {cut}"
            );
            assert_eq!(
                row.as_ref().map_or(&[][..], |row| row.props.as_slice()),
                props
            );
            assert_eq!(
                extent_visits(view.stats()) - before,
                visits,
                "edge {eid} at {cut}"
            );
        }
        let before = extent_visits(view.stats());
        assert!(
            view.vertex_at(&cx, VId(u128::MAX), CommitSeq(3))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(extent_visits(view.stats()), before);
        let held = view
            .edge_at(&cx, EId(90), CommitSeq(3))
            .await
            .unwrap()
            .unwrap();
        drop(view);
        assert!(pool.used() > 0);
        assert_eq!(
            held.props,
            vec![(
                PropertyKeyId(11),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(901))
            )]
        );
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn cold_adjacency_routes_by_direction_and_relation_without_property_payloads() {
    run(|contexts, dir| async move {
        let (store, id) = routed_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        for (endpoint, relation, incoming, cut, expected, visits) in [
            (
                1,
                Some(REL),
                false,
                1,
                vec![
                    edge(90, 1, 2, 1, Some(3)),
                    edge(20, 1, 3, 1, Some(3)),
                    edge(80, 1, 5, 1, None),
                ],
                2,
            ),
            (
                1,
                Some(REL),
                false,
                3,
                vec![
                    edge(90, 1, 2, 3, None),
                    edge(91, 1, 2, 3, None),
                    edge(80, 1, 5, 1, None),
                ],
                2,
            ),
            (
                2,
                None,
                true,
                3,
                vec![
                    edge(90, 1, 2, 3, None),
                    edge(91, 1, 2, 3, None),
                    AdjacencyEntry {
                        relation: RelationId(9),
                        ..edge(4, 1, 2, 2, None)
                    },
                ],
                3,
            ),
            (
                1,
                Some(RelationId(9)),
                false,
                3,
                vec![AdjacencyEntry {
                    relation: RelationId(9),
                    ..edge(4, 1, 2, 2, None)
                }],
                1,
            ),
            // A destination interval can contain holes. Native matching still
            // decides membership after its conservative route has been read.
            (4, Some(REL), true, 3, vec![], 1),
            (u128::MAX, None, true, 3, vec![], 0),
            (1, Some(RelationId(99)), false, 3, vec![], 0),
            (1, None, false, 0, vec![], 0),
        ] {
            let before = extent_visits(view.stats());
            let found = view
                .adjacency_at(&cx, VId(endpoint), relation, incoming, CommitSeq(cut), 32)
                .await
                .unwrap();
            assert_eq!(found.as_ref(), &expected);
            assert_eq!(extent_visits(view.stats()) - before, visits);
        }
        // Retired identities count toward admission even though the final
        // page contains only three live rows at the selected cut.
        assert!(matches!(
            view.adjacency_at(&cx, VId(1), Some(REL), false, CommitSeq(3), 3)
                .await,
            Err(BufferedReadError::Limit {
                resource: "buffered adjacency identities",
                ..
            })
        ));
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn pruned_payloads_remain_mandatory_at_open_and_authenticate_when_requested() {
    run(|contexts, dir| async move {
        let (store, id) = routed_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        for vertex_payload in [false, true] {
            let object = if vertex_payload {
                view.root().vertex_patches[1].patch_id
            } else {
                view.root().blocks[1].block_id
            };
            let path = store.path(object);
            let original = std::fs::read(&path).unwrap();
            let mut corrupt = original.clone();
            let last = corrupt.len() - 1;
            corrupt[last] ^= 1;
            std::fs::write(&path, corrupt).unwrap();
            // An admitted range excludes this unrelated payload. Neither
            // point lookup nor adjacency needs to refault its bytes.
            let before = extent_visits(view.stats());
            let found = view
                .edge_at(&cx, EId(90), CommitSeq(3))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                found.props,
                vec![(
                    PropertyKeyId(11),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(901))
                )]
            );
            assert_eq!(extent_visits(view.stats()) - before, 3);
            drop(found);
            let found = view
                .vertex_at(&cx, VId(1), CommitSeq(3))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.as_ref(), &vertex(3, None, 13));
            drop(found);
            let found = view
                .adjacency_at(&cx, VId(1), Some(REL), false, CommitSeq(3), 4)
                .await
                .unwrap();
            assert_eq!(
                found.as_ref(),
                &vec![
                    edge(90, 1, 2, 3, None),
                    edge(91, 1, 2, 3, None),
                    edge(80, 1, 5, 1, None)
                ]
            );
            drop(found);
            let requested = if vertex_payload {
                view.vertex_at(&cx, VId(100), CommitSeq(3))
                    .await
                    .map(|_| ())
            } else {
                view.edge_at(&cx, EId(1000), CommitSeq(3)).await.map(|_| ())
            };
            assert!(matches!(
                requested,
                Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
            ));
            let before_open = pool.used();
            assert!(
                matches!(
                    store
                        .open_buffered_root(&cx, id, pool.clone(), limits())
                        .await,
                    Err(BufferedReadError::Store(_))
                ),
                "new admission must authenticate even an unrelated object"
            );
            assert_eq!(
                pool.used(),
                before_open,
                "failed admission refunds its entire reservation"
            );
            std::fs::write(&path, original).unwrap();
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn cold_topology_and_propertyless_points_do_not_refault_an_unneeded_property_patch() {
    run(|contexts, dir| async move {
        let (store, id) = routed_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let block = view.root().blocks.last().unwrap().block_id;
        let bytes = std::fs::read(store.path(block)).unwrap();
        let (_, patch) = fgdb_strata::decode_block_with_properties(&bytes).unwrap();
        let patch = patch.unwrap().0;
        let path = store.path(patch);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let before = extent_visits(view.stats());
        let found = view
            .adjacency_at(&cx, VId(1), Some(REL), false, CommitSeq(3), 4)
            .await
            .unwrap();
        assert_eq!(
            found.as_ref(),
            &vec![
                edge(90, 1, 2, 3, None),
                edge(91, 1, 2, 3, None),
                edge(80, 1, 5, 1, None)
            ]
        );
        assert_eq!(extent_visits(view.stats()) - before, 2);
        drop(found);
        let found = view
            .edge_at(&cx, EId(20), CommitSeq(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.entry, edge(20, 1, 3, 1, Some(3)));
        assert!(found.props.is_empty());
        drop(found);
        assert!(
            view.edge_at(&cx, EId(20), CommitSeq(3))
                .await
                .unwrap()
                .is_none()
        );
        // E80's winning value is in the original block's patch, even though
        // the newer block's conservative EId interval also contains E80.
        let found = view
            .edge_at(&cx, EId(80), CommitSeq(3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found.props,
            vec![(
                PropertyKeyId(11),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(800))
            )]
        );
        drop(found);
        for cut in [1, 3] {
            assert!(
                matches!(
                    view.edge_at(&cx, EId(90), CommitSeq(cut)).await,
                    Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
                ),
                "a selected property is always authenticated, including a retired restatement"
            );
        }
        assert!(matches!(
            store
                .open_buffered_root(&cx, id, pool.clone(), limits())
                .await,
            Err(BufferedReadError::Store(_))
        ));
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn cold_selective_reads_refuse_unfunded_decode_and_result_storage_before_io() {
    run(|contexts, dir| async move {
        let (store, id) = routed_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        // One point result fits, but retaining it while decoding its source
        // does not. A larger bounded adjacency answer also refuses up front.
        let held = pool.reserve(&cx, pool.available() - 1536 * 1024).unwrap();
        let baseline = pool.used();
        for kind in 0..3 {
            let refusal = match kind {
                0 => view.vertex_at(&cx, VId(1), CommitSeq(3)).await.map(|_| ()),
                1 => view.edge_at(&cx, EId(90), CommitSeq(3)).await.map(|_| ()),
                _ => view
                    .adjacency_at(&cx, VId(1), Some(REL), false, CommitSeq(3), 4096)
                    .await
                    .map(|_| ()),
            };
            assert!(matches!(
                refusal,
                Err(BufferedReadError::Buffer(BufferError::Memory(
                    MemoryError::ResourceExhausted { .. }
                )))
            ));
            assert_eq!(extent_visits(view.stats()), 0);
            assert_eq!(pool.used(), baseline);
        }
        drop(held);
        let found = view
            .edge_at(&cx, EId(90), CommitSeq(3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.entry, edge(90, 1, 2, 3, None));
        assert_eq!(
            found.props,
            vec![(
                PropertyKeyId(11),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(901))
            )]
        );
        drop(found);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn cold_history_points_and_reverse_adjacency_survive_one_frame_refaults() {
    run(|contexts, dir| async move {
        let (store, id) = fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        assert_eq!(view.root_id(), id);
        assert_eq!(
            view.stats().misses,
            0,
            "admission did not retain payload frames"
        );
        let baseline = pool.used();
        for cut in [1, 3, 2, 3] {
            let row = view
                .vertex_at(&cx, VId(1), CommitSeq(cut))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.as_ref(),
                &vertex(
                    if cut < 3 { 1 } else { 3 },
                    if cut < 3 { Some(3) } else { None },
                    if cut < 3 { 10 } else { 30 }
                )
            );
            drop(row);
            let found = view
                .edge_at(&cx, EId(1), CommitSeq(cut))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                found.entry,
                edge(
                    1,
                    1,
                    2,
                    if cut < 3 { 1 } else { 3 },
                    if cut < 3 { Some(3) } else { None }
                )
            );
            assert_eq!(
                found.props,
                if cut < 3 {
                    vec![]
                } else {
                    vec![(
                        PropertyKeyId(11),
                        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(33)),
                    )]
                }
            );
        }
        let found = view
            .adjacency_at(&cx, VId(2), None, true, CommitSeq(3), 2)
            .await
            .unwrap();
        assert_eq!(
            found.as_ref(),
            &vec![edge(1, 1, 2, 3, None), edge(2, 3, 2, 2, None)]
        );
        drop(found);
        assert!(view.stats().evictions > 0);
        assert!(view.stats().misses > 1);
        assert!(
            pool.used() < baseline + 128 * 1024,
            "only one small resident extent remains"
        );
        let held = view
            .vertex_at(&cx, VId(1), CommitSeq(3))
            .await
            .unwrap()
            .unwrap();
        drop(view);
        assert!(
            pool.used() > 0,
            "answer owns its charge independently of the view"
        );
        drop(held);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_scan_merges_history_in_identity_order_and_reports_invisible_candidates() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let expected = [
            vec![],
            vec![scan_vertex(1, 1, Some(3), 10)],
            vec![
                scan_vertex(1, 1, Some(3), 10),
                scan_vertex(2, 2, None, 20),
                scan_vertex(3, 2, Some(4), 30),
            ],
            vec![
                scan_vertex(1, 3, Some(5), 11),
                scan_vertex(2, 2, None, 20),
                scan_vertex(3, 2, Some(4), 30),
            ],
            vec![
                scan_vertex(1, 3, Some(5), 11),
                scan_vertex(2, 2, None, 20),
                scan_vertex(5, 4, None, 50),
            ],
            vec![
                scan_vertex(1, 5, None, 12),
                scan_vertex(2, 2, None, 20),
                scan_vertex(4, 5, None, 40),
                scan_vertex(5, 4, None, 50),
            ],
        ];
        for (cut, expected) in expected.into_iter().enumerate() {
            let mut scan = view.vertex_scan(&cx, CommitSeq(cut as u64)).unwrap();
            assert_eq!(scan.snapshot_seq(), CommitSeq(cut as u64));
            assert_eq!(scan.work_used(), 0, "construction does not visit history");
            let mut events = 0usize;
            let mut admitted = Vec::new();
            let mut observe = |event| {
                match event {
                    BufferedScanEvent::Work => events += 1,
                    BufferedScanEvent::Identity(vid) => admitted.push(vid),
                }
                Ok::<(), core::convert::Infallible>(())
            };
            let mut identities = Vec::new();
            let mut visible = Vec::new();
            while let Some(candidate) = scan.next_candidate_with(&cx, &mut observe).await.unwrap() {
                identities.push(candidate.vid);
                if let Some(row) = candidate.row {
                    visible.push(row.as_ref().clone());
                }
            }
            assert_eq!(identities, [VId(1), VId(2), VId(3), VId(4), VId(5)]);
            assert_eq!(admitted, identities, "exactly one admission per identity");
            assert_eq!(visible, expected, "historical cut {cut}");
            assert!(events > identities.len());
            assert_eq!(scan.work_used(), events);
            assert!(events <= 3 + 10 * (256 + 2));
            drop(scan);
            assert_eq!(
                pool.used(),
                baseline,
                "scan misses leave no resident frames"
            );
        }
        assert!(view.stats().bypasses > 0);
        assert_eq!(view.stats().evictions, 0);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_scan_is_lazy_and_does_not_rescan_every_patch_for_every_identity() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let cx = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut patches = Vec::new();
        let count = 32usize;
        for index in 1..=count {
            let patch = store
                .put_patch(
                    &commit,
                    &encode_patch(&[scan_vertex(index as u128, 1, None, index as i64)]).unwrap(),
                )
                .await
                .unwrap();
            patches.push(PatchRef {
                patch_id: patch.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![],
                    vertex_patches: patches,
                },
            )
            .await
            .unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut bounded = limits();
        // Authenticate one object and row per patch, then build the directory.
        // Each cursor row needs four source events (head, history, decode
        // object, decode row) and four affine-retention events (owner,
        // property vector, scalar heap, final split). None may be skipped.
        let scan_work = count * (4 + 4);
        let (_, directory_work) = patch_directory_overhead(count);
        bounded.max_work = 2 * count + directory_work;
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), bounded)
            .await
            .unwrap();
        let baseline = pool.used();
        drop(view.vertex_scan(&cx, CommitSeq(1)).unwrap());
        assert_eq!(view.stats().misses, 0);
        assert_eq!(pool.used(), baseline);
        let mut scan = view.vertex_scan(&cx, CommitSeq(1)).unwrap();
        let held = scan.next(&cx).await.unwrap().unwrap();
        assert_eq!(held.vid, VId(1));
        for index in 2..=count {
            let row = scan.next(&cx).await.unwrap().unwrap();
            assert_eq!(
                row.as_ref(),
                &scan_vertex(index as u128, 1, None, index as i64)
            );
        }
        assert!(scan.next(&cx).await.unwrap().is_none());
        assert_eq!(scan.work_used(), scan_work);
        drop(scan);
        assert_eq!(view.stats().misses, count as u64);
        assert_eq!(view.stats().bypasses, count as u64);
        drop(view);
        assert_eq!(pool.used(), held.charged_bytes());
        drop(held);
        assert_eq!(pool.used(), 0);

        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), bounded)
            .await
            .unwrap();
        let mut scan = view.vertex_scan(&cx, CommitSeq(1)).unwrap();
        // The hosted-query control applies a tighter cursor allowance without
        // lowering the already completed constructor's directory allowance.
        let scan_limit = scan_work - 1;
        let mut admitted_work = 0usize;
        let mut observe = |event| {
            if event == BufferedScanEvent::Work {
                let requested = admitted_work + 1;
                if requested > scan_limit {
                    return Err((requested, scan_limit));
                }
                admitted_work = requested;
            }
            Ok(())
        };
        for index in 1..count {
            let candidate = scan
                .next_candidate_with(&cx, &mut observe)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(candidate.vid, VId(index as u128));
            assert!(candidate.row.is_some());
        }
        assert!(matches!(
            scan.next_candidate_with(&cx, &mut observe).await,
            Err(BufferedScanError::Control((requested, limit)))
                if requested == scan_work && limit == scan_limit
        ));
        assert_eq!(admitted_work, scan_limit);
        assert_eq!(scan.work_used(), scan_limit);
        assert!(
            scan.next(&cx).await.unwrap().is_none(),
            "a work error fuses the cursor"
        );
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_scan_control_failure_at_every_source_event_fuses_and_refunds() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let total = {
            let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
            while scan.next(&cx).await.unwrap().is_some() {}
            scan.work_used()
        };
        for fail_at in 0..total {
            let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
            let mut observed = 0usize;
            let mut observe = |event| {
                if event == BufferedScanEvent::Work {
                    if observed == fail_at {
                        return Err(fail_at);
                    }
                    observed += 1;
                }
                Ok(())
            };
            loop {
                match scan.next_candidate_with(&cx, &mut observe).await {
                    Ok(Some(_)) => {}
                    Err(BufferedScanError::Control(at)) => {
                        assert_eq!(at, fail_at);
                        break;
                    }
                    other => panic!("expected control failure at {fail_at}, got {other:?}"),
                }
            }
            assert_eq!(scan.work_used(), fail_at);
            assert!(
                scan.next_candidate_with(&cx, &mut observe)
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(scan);
            assert_eq!(pool.used(), baseline, "failed source event {fail_at}");
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_scan_heap_admission_and_corrupt_refault_never_expose_a_partial_winner() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let held = pool.reserve(&cx, pool.available() - 100).unwrap();
        assert!(matches!(
            view.vertex_scan(&cx, CommitSeq(5)),
            Err(BufferedReadError::Buffer(BufferError::Memory(
                MemoryError::ResourceExhausted { .. }
            )))
        ));
        assert_eq!(view.stats().misses, 0);
        drop(held);
        assert_eq!(pool.used(), baseline);
        let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
        let mut observe = |event| match event {
            BufferedScanEvent::Work => Ok(()),
            BufferedScanEvent::Identity(vid) => {
                assert_eq!(vid, VId(1));
                Err(7u8)
            }
        };
        assert!(matches!(
            scan.next_candidate_with(&cx, &mut observe).await,
            Err(BufferedScanError::Control(7))
        ));
        assert_eq!(scan.work_used(), 3, "only authenticated heads were visited");
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        assert_eq!(
            view.stats().misses,
            0,
            "identity admission precedes all payload reads"
        );
        assert_eq!(pool.used(), baseline);
        assert!(matches!(
            view.vertex_scan(&cx, CommitSeq(6)),
            Err(BufferedReadError::BeyondPublication { .. })
        ));
        let path = store.path(view.root().vertex_patches[1].patch_id);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(path, bytes).unwrap();
        let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
        assert!(matches!(
            scan.next(&cx).await,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        assert_eq!(
            pool.used(),
            baseline,
            "even the first patch's candidate was released"
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn admission_memory_history_and_result_limits_never_return_a_partial_source() {
    run(|contexts, dir| async move {
        let (store, id) = fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let too_small = MemoryPool::new(1, 0).unwrap();
        assert!(matches!(
            store
                .open_buffered_root(&cx, id, too_small.clone(), limits())
                .await,
            Err(BufferedReadError::Memory(_))
        ));
        assert_eq!(too_small.used(), 0);
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut small = limits();
        small.max_blocks = 2;
        assert!(
            matches!(store.open_buffered_root(&cx, id, pool.clone(), small).await,
            Err(BufferedReadError::Store(error)) if matches!(*error, StoreError::RootReferenceLimit { .. }))
        );
        assert_eq!(pool.used(), 0);
        small = limits();
        small.max_source_bytes = 1;
        assert!(matches!(
            store.open_buffered_root(&cx, id, pool.clone(), small).await,
            Err(BufferedReadError::Limit { .. })
        ));
        assert_eq!(pool.used(), 0);
        small = limits();
        small.max_work = 1;
        assert!(matches!(
            store.open_buffered_root(&cx, id, pool.clone(), small).await,
            Err(BufferedReadError::Limit { .. })
        ));
        assert_eq!(pool.used(), 0);
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        assert!(matches!(
            view.adjacency_at(&cx, VId(2), None, true, CommitSeq(3), 1)
                .await,
            Err(BufferedReadError::Limit {
                resource: "buffered adjacency identities",
                ..
            })
        ));
        assert!(matches!(
            view.vertex_at(&cx, VId(1), CommitSeq(4)).await,
            Err(BufferedReadError::BeyondPublication { .. })
        ));
        assert!(
            view.edge_at(&cx, EId(99), CommitSeq(3))
                .await
                .unwrap()
                .is_none()
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn corrupt_cold_refault_is_rejected_and_refunds_the_pending_frame() {
    run(|contexts, dir| async move {
        let (store, id) = fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let block = view.root().blocks[0].block_id;
        // The corruption is fixture injection, never a database write path.
        let path = store.path(block);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let before = pool.used();
        assert!(matches!(
            view.edge_at(&cx, EId(1), CommitSeq(3)).await,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        assert_eq!(pool.used(), before);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn propertied_durable_source_can_exceed_the_shared_resident_ceiling() {
    // Each property patch must fit the store's per-object ceiling (16 KiB):
    // a 15,000-byte payload encodes to a 16,911-byte patch the store refuses.
    // 240 patches of this payload still exceed the 3 MiB shared ceiling.
    const PAYLOAD: usize = 14_000;
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut blocks = Vec::new();
        let mut source_bytes = 0usize;
        // Distinct payload identities prevent content-addressed deduplication
        // from making this merely a small object referenced many times.
        for index in 0..240u128 {
            let mut payload = vec![0x55; PAYLOAD];
            payload[..16].copy_from_slice(&index.to_le_bytes());
            let rows = vec![vec![(
                PropertyKeyId(11),
                CanonicalPropertyValue::Scalar(CanonicalScalar::bytes(payload).unwrap()),
            )]];
            let property_bytes = encode_property_patch(&rows).unwrap();
            let property = store
                .put_edge_property_patch(&commit, &property_bytes)
                .await
                .unwrap();
            let bytes = encode_block_with_properties(
                0,
                None,
                &[edge(index + 1, index + 1, 999, 1, None)],
                property.0,
                &[1],
                &rows,
            )
            .unwrap();
            source_bytes += property_bytes.len() + bytes.len();
            let block = store.put(&commit, &bytes).await.unwrap();
            blocks.push(BlockRef {
                block_id: block.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(2),
            partition: 0,
            published_at: CommitSeq(1),
            blocks,
            vertex_patches: vec![],
        };
        let id = store.put_root(&commit, &root).await.unwrap();
        let cap = 3 * 1024 * 1024;
        assert!(source_bytes > cap);
        let pool = MemoryPool::new(cap, 0).unwrap();
        let mut bounded = limits();
        bounded.max_root_bytes = 16 * 1024;
        bounded.max_source_bytes = source_bytes;
        bounded.max_blocks = 240;
        // Work is not under test: open also builds the charged descriptor
        // directory (68bf6b14), measured at 11,778 units for 240 blocks.
        bounded.max_work = 240 * 64;
        let mut view = store
            .open_buffered_root(&query, id, pool.clone(), bounded)
            .await
            .unwrap();
        for eid in [1, 240, 1] {
            let found = view
                .edge_at(&query, EId(eid), CommitSeq(1))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(found.entry, edge(eid, eid, 999, 1, None));
            let mut expected = vec![0x55; PAYLOAD];
            expected[..16].copy_from_slice(&(eid - 1).to_le_bytes());
            assert_eq!(
                found.props,
                vec![(
                    PropertyKeyId(11),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::bytes(expected).unwrap())
                )]
            );
            assert!(pool.used() <= cap);
        }
        assert!(view.stats().evictions > 0);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn a_keyed_root_cannot_bypass_cross_object_identity_history() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let first = store
            .put(
                &commit,
                &encode_block(0, None, &[edge(9, 1, 2, 1, None)]).unwrap(),
            )
            .await
            .unwrap();
        let alias = store
            .put(
                &commit,
                &encode_block(0, None, &[edge(9, 3, 4, 1, None)]).unwrap(),
            )
            .await
            .unwrap();
        let old = store
            .put_patch(&commit, &encode_patch(&[vertex(1, None, 10)]).unwrap())
            .await
            .unwrap();
        let changed = store
            .put_patch(&commit, &encode_patch(&[vertex(1, None, 99)]).unwrap())
            .await
            .unwrap();
        for edge_conflict in [true, false] {
            let root = PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(1),
                blocks: if edge_conflict {
                    [first, alias]
                        .into_iter()
                        .map(|id| BlockRef {
                            block_id: id.0,
                            first_seq: CommitSeq(1),
                            last_seq: CommitSeq(1),
                        })
                        .collect()
                } else {
                    vec![]
                },
                vertex_patches: if edge_conflict {
                    vec![]
                } else {
                    [old, changed]
                        .into_iter()
                        .map(|id| PatchRef {
                            patch_id: id.0,
                            first_seq: CommitSeq(1),
                            last_seq: CommitSeq(1),
                        })
                        .collect()
                },
            };
            // Bypass publication in the fixture to prove open itself enforces
            // history. Both objects and the root have valid keyed identities.
            let bytes = fgdb_strata::root::encode_root(&root).unwrap();
            let id = fgdb_strata::root::root_id(&KEY, NS, &bytes);
            std::fs::write(store.path(id), bytes).unwrap();
            let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
            let result = store
                .open_buffered_root(&query, PartitionRootVersion(id), pool.clone(), limits())
                .await;
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("conflicting keyed history must refuse"),
            };
            if edge_conflict {
                assert!(matches!(&error, BufferedReadError::Store(error)
                    if matches!(**error, StoreError::MalformedRoot(fgdb_strata::root::RootError::EdgeIdentityMismatch { .. }))));
            } else {
                assert!(matches!(
                    &error,
                    BufferedReadError::VertexHistoryConflict { vid: VId(1) }
                ));
            }
            assert_eq!(pool.used(), 0);
            // The compact error can outlive the reader without holding graph
            // payloads whose reservations have already been refunded.
            drop(error);
        }
    });
}

#[test]
fn interval_point_and_endpoint_reads_preserve_publication_winners_and_future_fences() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let cx = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let definitions = [
            vec![scan_vertex(50, 1, None, 10)],
            vec![
                scan_vertex(20, 1, None, 20),
                scan_vertex(50, 1, Some(3), 10),
                scan_vertex(70, 1, None, 70),
            ],
            vec![
                scan_vertex(0, 1, None, 0),
                scan_vertex(50, 3, None, 11),
                scan_vertex(100, 1, None, 100),
            ],
            vec![scan_vertex(50, 3, Some(5), 11)],
            vec![scan_vertex(50, 5, None, 12)],
            vec![
                scan_vertex(1, 6, None, 1),
                scan_vertex(50, 5, Some(6), 12),
                scan_vertex(u128::MAX, 6, None, 99),
            ],
            vec![scan_vertex(30, 7, None, 30), scan_vertex(60, 7, None, 60)],
        ];
        let mut patches = Vec::new();
        for rows in &definitions {
            let patch = store
                .put_patch(&commit, &encode_patch(rows).unwrap())
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
        let future_patch = patches.last().unwrap().patch_id;
        // A deliberately dangling late edge exercises the source's existing
        // typed endpoint failure, after the newest vertex statement retires.
        let edge_image = edge(99, 50, 50, 1, None);
        let block = store
            .put(&commit, &encode_block(0, None, &[edge_image]).unwrap())
            .await
            .unwrap();
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(7),
                    blocks: vec![BlockRef {
                        block_id: block.0,
                        first_seq: CommitSeq(1),
                        last_seq: CommitSeq(1),
                    }],
                    vertex_patches: patches,
                },
            )
            .await
            .unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        for cut in 0..=7 {
            let expected = match cut {
                1 | 2 => Some(scan_vertex(50, 1, Some(3), 10)),
                3 | 4 => Some(scan_vertex(50, 3, Some(5), 11)),
                5 => Some(scan_vertex(50, 5, Some(6), 12)),
                _ => None,
            };
            let found = view.vertex_at(&cx, VId(50), CommitSeq(cut)).await.unwrap();
            assert_eq!(found.as_deref(), expected.as_ref(), "point cut {cut}");
            drop(found);
            assert!(
                view.vertex_at(&cx, VId(49), CommitSeq(cut))
                    .await
                    .unwrap()
                    .is_none(),
                "an interval hit is not a vertex: cut {cut}"
            );
            let extreme = view
                .vertex_at(&cx, VId(u128::MAX), CommitSeq(cut))
                .await
                .unwrap();
            assert_eq!(extreme.is_some(), cut >= 6);
            drop(extreme);
            if cut == 0 {
                continue;
            }
            let mut scan = view.edge_scan(&cx, CommitSeq(cut)).unwrap();
            let candidate = scan
                .next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
                .await;
            if let Some(expected) = expected {
                let endpoints = candidate.unwrap().unwrap().row.unwrap();
                assert_eq!(endpoints.source_vertex(), &expected, "endpoint cut {cut}");
                assert_eq!(endpoints.target_vertex(), &expected);
                drop(endpoints);
            } else {
                assert!(matches!(
                    candidate,
                    Err(BufferedScanError::Read(BufferedReadError::DanglingEndpoint))
                ));
            }
            assert!(
                scan.next_with_endpoints(&cx, None, &mut |_| Ok::<_, ()>(()))
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(scan);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
        // Start with no resident extents, then damage a future-only patch whose
        // conservative range includes V50 but whose rows do not contain it.
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let path = store.path(future_patch);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let found = view
            .vertex_at(&cx, VId(50), CommitSeq(5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&*found, &scan_vertex(50, 5, Some(6), 12));
        drop(found);
        assert!(matches!(
            view.vertex_at(&cx, VId(50), CommitSeq(7)).await,
            Err(BufferedReadError::Buffer(BufferError::ChecksumMismatch))
        ));
        let retained = pool.used();
        assert!(matches!(
            store
                .open_buffered_root(&cx, id, pool.clone(), limits())
                .await,
            Err(BufferedReadError::Store(_))
        ));
        assert_eq!(
            pool.used(),
            retained,
            "failed full admission refunds all state"
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_property_history_larger_than_the_resident_cap_opens_and_refaults() {
    const COUNT: usize = 240;
    const PAYLOAD: usize = 14_000;
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut patches = Vec::new();
        let mut source_bytes = 0usize;
        for index in 1..=COUNT {
            let mut payload = vec![0x5a; PAYLOAD];
            payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
            let row = VertexRow {
                props: vec![(
                    PropertyKeyId(11),
                    CanonicalScalar::bytes(payload).unwrap().into(),
                )],
                ..scan_vertex(index as u128, 1, None, 0)
            };
            let bytes = encode_patch(&[row]).unwrap();
            source_bytes += bytes.len();
            let patch = store.put_patch(&commit, &bytes).await.unwrap();
            patches.push(PatchRef {
                patch_id: patch.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![],
                    vertex_patches: patches,
                },
            )
            .await
            .unwrap();
        let cap = 3 * 1024 * 1024;
        assert!(
            source_bytes > cap,
            "distinct durable payloads exceed the cap"
        );
        let pool = MemoryPool::new(cap, 0).unwrap();
        let mut bounded = limits();
        bounded.max_root_bytes = 16 * 1024;
        bounded.max_source_bytes = source_bytes;
        bounded.max_vertex_patches = COUNT;
        let (directory_bytes, directory_work) = patch_directory_overhead(COUNT);
        bounded.max_work = 2 * COUNT + directory_work;
        let mut view = store
            .open_buffered_root(&query, id, pool.clone(), bounded)
            .await
            .unwrap();
        assert_eq!(
            pool.used(),
            bounded.metadata_bytes().unwrap() + directory_bytes
        );
        for index in [1, COUNT, 1] {
            let found = view
                .vertex_at(&query, VId(index as u128), CommitSeq(1))
                .await
                .unwrap()
                .unwrap();
            let mut payload = vec![0x5a; PAYLOAD];
            payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
            assert_eq!(
                found.props,
                vec![(
                    PropertyKeyId(11),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::bytes(payload).unwrap())
                )]
            );
            assert!(pool.used() <= cap);
        }
        assert!(view.stats().evictions > 0);
        let mut scan = view.vertex_scan(&query, CommitSeq(1)).unwrap();
        for index in 1..=COUNT {
            let found = scan.next(&query).await.unwrap().unwrap();
            assert_eq!(found.vid, VId(index as u128));
            let mut payload = vec![0x5a; PAYLOAD];
            payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
            assert_eq!(
                found.props,
                vec![(
                    PropertyKeyId(11),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::bytes(payload).unwrap())
                )]
            );
        }
        assert!(scan.next(&query).await.unwrap().is_none());
        drop(scan);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_restatements_bound_residency_and_meter_every_authenticated_birth_refault() {
    const COUNT: usize = 240;
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let mut patches = Vec::new();
        let mut source_bytes = 0usize;
        let mut birth_bytes = 0usize;
        for index in 0..COUNT {
            let birth = VertexRow {
                props: vec![(
                    PropertyKeyId(11),
                    CanonicalScalar::bytes(vec![0x6b; 14_000]).unwrap().into(),
                )],
                ..vertex(1, None, 0)
            };
            // Every patch has a distinct keyed identity, even though its
            // large first statement is an exact restatement of the birth.
            let bytes =
                encode_patch(&[birth, scan_vertex(index as u128 + 2, 1, None, index as i64)])
                    .unwrap();
            source_bytes += bytes.len();
            if index == 0 {
                birth_bytes = bytes.len();
            }
            let patch = store.put_patch(&commit, &bytes).await.unwrap();
            patches.push(PatchRef {
                patch_id: patch.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![],
                    vertex_patches: patches,
                },
            )
            .await
            .unwrap();
        let cap = 3 * 1024 * 1024;
        assert!(source_bytes > cap);
        let pool = MemoryPool::new(cap, 0).unwrap();
        let mut bounded = limits();
        bounded.max_root_bytes = 16 * 1024;
        bounded.max_vertex_patches = COUNT;
        bounded.max_source_bytes = source_bytes + (COUNT - 1) * birth_bytes;
        // Each incoming patch and each refault has one object plus two rows.
        // Retain this exact authenticated-refault boundary independently of
        // the directory construction that follows successful admission.
        let admission_work = 3 * COUNT + 3 * (COUNT - 1);
        let (directory_bytes, directory_work) = patch_directory_overhead(COUNT);
        bounded.max_work = admission_work + directory_work;
        let mut view = store
            .open_buffered_root(&query, id, pool.clone(), bounded)
            .await
            .unwrap();
        assert_eq!(
            pool.used(),
            bounded.metadata_bytes().unwrap() + directory_bytes
        );
        let found = view
            .vertex_at(&query, VId(1), CommitSeq(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found.props,
            vec![(
                PropertyKeyId(11),
                CanonicalPropertyValue::Scalar(CanonicalScalar::bytes(vec![0x6b; 14_000]).unwrap())
            )]
        );
        drop(found);
        drop(view);
        assert_eq!(pool.used(), 0);

        let mut too_little = bounded;
        too_little.max_work = admission_work - 1;
        assert!(matches!(
            store.open_buffered_root(&query, id, pool.clone(), too_little).await,
            Err(BufferedReadError::Limit { resource: "buffered source work", requested, limit })
                if requested == admission_work && limit == admission_work - 1
        ));
        assert_eq!(pool.used(), 0);
        too_little = bounded;
        too_little.max_source_bytes -= 1;
        assert!(matches!(
            store.open_buffered_root(&query, id, pool.clone(), too_little).await,
            Err(BufferedReadError::Limit { resource: "buffered source bytes", requested, limit })
                if requested == bounded.max_source_bytes && limit == bounded.max_source_bytes - 1
        ));
        assert_eq!(pool.used(), 0);
        let starved = MemoryPool::new(2 * 1024 * 1024, 0).unwrap();
        assert!(matches!(
            store
                .open_buffered_root(&query, id, starved.clone(), bounded)
                .await,
            Err(BufferedReadError::Memory(
                MemoryError::ResourceExhausted { .. }
            ))
        ));
        assert_eq!(starved.used(), 0);
    });
}

#[test]
fn compact_vertex_admission_matches_the_canonical_publication_history_validator() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        // None is an accepted history; false is an identity conflict; true is
        // a retirement conflict. Each case has its own explicit expected law.
        let cases = [
            (
                "retire then replace",
                vec![
                    vertex(1, None, 10),
                    vertex(1, Some(2), 10),
                    vertex(2, None, 20),
                ],
                None,
            ),
            (
                "repeat retired version",
                vec![vertex(1, Some(2), 10), vertex(1, Some(2), 10)],
                None,
            ),
            (
                "prepend contiguous history",
                vec![vertex(3, None, 30), vertex(1, Some(3), 10)],
                None,
            ),
            (
                "changed property",
                vec![vertex(1, None, 10), vertex(1, None, 99)],
                Some(false),
            ),
            (
                "changed labels",
                vec![
                    vertex(1, None, 10),
                    VertexRow {
                        labels: vec![LabelId(10)],
                        ..vertex(1, None, 10)
                    },
                ],
                Some(false),
            ),
            (
                "changed birth ordinal",
                vec![
                    vertex(1, None, 10),
                    VertexRow {
                        birth_ordinal: 8,
                        ..vertex(1, None, 10)
                    },
                ],
                Some(false),
            ),
            (
                "retirement removed",
                vec![vertex(1, Some(2), 10), vertex(1, None, 10)],
                Some(true),
            ),
            (
                "retirement moved",
                vec![vertex(1, Some(2), 10), vertex(1, Some(3), 10)],
                Some(true),
            ),
            (
                "successor gap",
                vec![vertex(1, Some(2), 10), vertex(3, None, 30)],
                Some(false),
            ),
            (
                "successor overlap",
                vec![vertex(1, Some(3), 10), vertex(2, None, 20)],
                Some(false),
            ),
            (
                "changed successor birth",
                vec![
                    vertex(1, Some(2), 10),
                    VertexRow {
                        birth_ordinal: 8,
                        ..vertex(2, None, 20)
                    },
                ],
                Some(false),
            ),
            (
                "late retirement is not a valid prefix",
                vec![
                    vertex(1, None, 10),
                    vertex(2, None, 20),
                    vertex(1, Some(2), 10),
                ],
                Some(false),
            ),
            (
                "prepended gap",
                vec![vertex(3, None, 30), vertex(1, Some(2), 10)],
                Some(false),
            ),
        ];
        for (name, history, expected) in cases {
            let mut patches = Vec::new();
            for (index, row) in history.into_iter().enumerate() {
                let first_seq = row.created_at;
                // Keep root publication ranges monotone even when testing a
                // malicious retirement removal or a prepended old version.
                let bytes =
                    encode_patch(&[row, scan_vertex(index as u128 + 100, 10, None, 0)]).unwrap();
                let patch = store.put_patch(&commit, &bytes).await.unwrap();
                patches.push(PatchRef {
                    patch_id: patch.0,
                    first_seq,
                    last_seq: CommitSeq(10),
                });
            }
            let root = PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                published_at: CommitSeq(10),
                blocks: vec![],
                vertex_patches: patches,
            };
            let bytes = fgdb_strata::root::encode_root(&root).unwrap();
            let id = fgdb_strata::root::root_id(&KEY, NS, &bytes);
            std::fs::write(store.path(id), bytes).unwrap();
            let id = PartitionRootVersion(id);
            let ordinary = store.admit_root(&query, id).await;
            assert_eq!(ordinary.is_ok(), expected.is_none(), "ordinary: {name}");
            let pool = MemoryPool::new(3 * 1024 * 1024, 0).unwrap();
            let compact = store
                .open_buffered_root(&query, id, pool.clone(), limits())
                .await;
            match expected {
                None => assert!(compact.is_ok(), "compact: {name}"),
                Some(false) => assert!(
                    matches!(
                        &compact,
                        Err(BufferedReadError::VertexHistoryConflict { vid: VId(1) })
                    ),
                    "compact: {name}"
                ),
                Some(true) => assert!(
                    matches!(&compact, Err(BufferedReadError::Store(error))
                    if matches!(**error, StoreError::MalformedRoot(fgdb_strata::root::RootError::VertexRetirementMismatch { vid: VId(1), .. }))),
                    "compact: {name}"
                ),
            }
            drop(compact);
            assert_eq!(pool.used(), 0, "{name}");
        }
    });
}

#[test]
fn vertex_topology_scan_matches_complete_history_at_every_retained_cut() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        for cut in 0..=5 {
            let expected = {
                let mut scan = view.vertex_scan(&cx, CommitSeq(cut)).unwrap();
                let mut expected = Vec::new();
                while let Some(candidate) = scan
                    .next_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                    .await
                    .unwrap()
                {
                    expected.push((
                        candidate.vid,
                        candidate.row.map(|row| {
                            assert!(!row.props.is_empty());
                            (
                                row.vid,
                                row.birth_ordinal,
                                row.created_at,
                                row.retired_at,
                                row.labels.clone(),
                            )
                        }),
                    ));
                }
                expected
            };
            let mut scan = view.vertex_scan(&cx, CommitSeq(cut)).unwrap();
            let mut actual = Vec::new();
            let mut admitted = Vec::new();
            let mut observe = |event| {
                if let BufferedScanEvent::Identity(vid) = event {
                    admitted.push(vid);
                }
                Ok::<_, ()>(())
            };
            while let Some(candidate) = scan
                .next_topology_candidate_with(&cx, &mut observe)
                .await
                .unwrap()
            {
                actual.push((
                    candidate.vid,
                    candidate.row.map(|row| {
                        (
                            row.vid(),
                            row.birth_ordinal(),
                            row.created_at(),
                            row.retired_at(),
                            row.labels().to_vec(),
                        )
                    }),
                ));
            }
            assert_eq!(actual, expected, "historical cut {cut}");
            assert_eq!(
                admitted,
                actual.iter().map(|(vid, _)| *vid).collect::<Vec<_>>(),
                "invisible identities retain their source admission"
            );
            drop(scan);
            assert_eq!(pool.used(), baseline);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_topology_and_complete_pulls_can_share_a_patch_without_losing_properties() {
    run(|contexts, dir| async move {
        let commit = contexts.commit();
        let cx = contexts.query();
        let store = BlockStore::open(&commit, &dir, KEY, NS).await.unwrap();
        let make_rows = |repetitions| {
            (1..=4)
                .map(|vid| {
                    let mut row = scan_vertex(vid, 1, None, 0);
                    row.labels = vec![LabelId(vid as u64)];
                    row.props = vec![(
                        PropertyKeyId(11),
                        CanonicalPropertyValue::List(
                            fgdb_types::CanonicalList::try_new(vec![
                                CanonicalScalar::ucs_basic_text(&"payload".repeat(repetitions))
                                    .unwrap()
                                    .into(),
                                CanonicalScalar::Int(vid as i64).into(),
                            ])
                            .unwrap(),
                        ),
                    )];
                    row
                })
                .collect::<Vec<_>>()
        };
        // Keep the original over-capacity object as a refusal control. The
        // sharing law below needs four rows in ONE format-admitted patch;
        // splitting them would erase its Skip->Read->Skip reuse boundary.
        let oversized = encode_patch(&make_rows(300)).unwrap();
        assert!(matches!(
            store.put_patch(&commit, &oversized).await,
            Err(StoreError::ObjectTooLarge { limit: 16_384, observed })
                if observed == oversized.len() as u64
        ));
        let rows = make_rows(240);
        let patch = store
            .put_patch(&commit, &encode_patch(&rows).unwrap())
            .await
            .unwrap();
        let id = store
            .put_root(
                &commit,
                &PartitionRoot {
                    graph: GraphId(1),
                    branch: BranchId(2),
                    partition: 0,
                    published_at: CommitSeq(1),
                    blocks: vec![],
                    vertex_patches: vec![PatchRef {
                        patch_id: patch.0,
                        first_seq: CommitSeq(1),
                        last_seq: CommitSeq(1),
                    }],
                },
            )
            .await
            .unwrap();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let before = view.stats().bypasses;
        let mut scan = view.vertex_scan(&cx, CommitSeq(1)).unwrap();
        let first = scan
            .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        let second = scan.next(&cx).await.unwrap().unwrap();
        let third = scan
            .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
            .await
            .unwrap()
            .unwrap()
            .row
            .unwrap();
        let fourth = scan.next(&cx).await.unwrap().unwrap();
        assert!(scan.next(&cx).await.unwrap().is_none());
        assert_eq!(first.vid(), VId(1));
        assert_eq!(first.labels(), &[LabelId(1)]);
        assert_eq!(second.as_ref(), &rows[1]);
        assert_eq!(third.vid(), VId(3));
        assert_eq!(third.labels(), &[LabelId(3)]);
        assert_eq!(fourth.as_ref(), &rows[3]);
        assert!(
            third.charged_bytes() < second.charged_bytes(),
            "reusing a full patch must not copy its unused properties"
        );
        assert_eq!(first.charged_bytes(), third.charged_bytes());
        drop(scan);
        assert_eq!(
            view.stats().bypasses - before,
            2,
            "Skip->Read refaults once; Read->Skip and the later Read reuse the full patch"
        );
        drop(view);
        assert_eq!(
            pool.used(),
            first.charged_bytes()
                + second.charged_bytes()
                + third.charged_bytes()
                + fourth.charged_bytes(),
            "each returned row retains its own exact reservation"
        );
        drop((first, second, third, fourth));
        assert_eq!(pool.used(), 0);
    });
}

#[test]
fn vertex_topology_refusal_at_each_work_boundary_fuses_and_refunds() {
    run(|contexts, dir| async move {
        let (store, id) = vertex_scan_fixture(&contexts, &dir).await;
        let cx = contexts.query();
        let pool = MemoryPool::new(8 * 1024 * 1024, 0).unwrap();
        let mut view = store
            .open_buffered_root(&cx, id, pool.clone(), limits())
            .await
            .unwrap();
        let baseline = pool.used();
        let total = {
            let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
            while scan
                .next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                .await
                .unwrap()
                .is_some()
            {}
            scan.work_used()
        };
        for fail_at in 0..total {
            let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
            let mut observed = 0;
            let mut observe = |event| {
                if event == BufferedScanEvent::Work {
                    if observed == fail_at {
                        return Err(fail_at);
                    }
                    observed += 1;
                }
                Ok(())
            };
            loop {
                match scan.next_topology_candidate_with(&cx, &mut observe).await {
                    Ok(Some(_)) => {}
                    Err(BufferedScanError::Control(at)) => {
                        assert_eq!(at, fail_at);
                        break;
                    }
                    other => panic!("expected refusal at {fail_at}, got {other:?}"),
                }
            }
            assert_eq!(scan.work_used(), fail_at);
            assert!(scan.next(&cx).await.unwrap().is_none());
            assert!(
                scan.next_topology_candidate_with(&cx, &mut observe)
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(scan);
            assert_eq!(pool.used(), baseline, "source event {fail_at}");
        }
        // Property bytes remain authenticated even though their values are
        // skipped after admission. Corrupt a property in a later restatement:
        // no already-visited historical winner may escape this failed pull.
        let path = store.path(view.root().vertex_patches[1].patch_id);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(path, bytes).unwrap();
        let mut scan = view.vertex_scan(&cx, CommitSeq(5)).unwrap();
        assert!(matches!(
            scan.next_topology_candidate_with(&cx, &mut |_| Ok::<_, ()>(()))
                .await,
            Err(BufferedScanError::Read(BufferedReadError::Buffer(
                BufferError::ChecksumMismatch
            )))
        ));
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        assert_eq!(pool.used(), baseline);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
}
