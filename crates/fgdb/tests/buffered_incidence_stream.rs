//! Public cold-incidence APIs over real Chronicle/Strata and injected MemVfs.
//! Result vectors below belong only to the independent test oracle.
// Commit-pack reads nest the store's futures past the default recursion
// limit when this crate proves one Send (see fgdb-strata's lib.rs).
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadError, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys,
    MemVfs, MemoryPool, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::store::BufferedScanError;
use fgdb_strata::tiered::edge_scan::{BufferedEdgeDirection, BufferedEdgeEndpoints};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(7);
const S: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(9);
const L: LabelId = LabelId(10);
const IDS: [VId; 4] = [VId(0), VId(1_u128 << 100), VId(u128::MAX), VId(99)];
const EDGES: [(u128, usize, usize, i64); 5] = [
    (0, 0, 1, 7),
    (1, 0, 1, 8),
    (2, 1, 2, 9),
    (3, 2, 0, 10),
    (u128::MAX, 1, 1, 11),
];

type Properties = Vec<(PropertyKeyId, CanonicalPropertyValue)>;
type Row = (
    EId,
    VId,
    VId,
    RelationId,
    Properties,
    Properties,
    Properties,
);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}
fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 512,
        max_vertex_patches: 512,
        max_work: 10_000_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}
fn pool() -> MemoryPool {
    MemoryPool::new(64 * 1024 * 1024, 0).unwrap()
}
async fn cold(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    pool: MemoryPool,
) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        pool,
        limits(),
    )
    .await
    .unwrap()
}
async fn seed(contexts: &PurposeContexts, vfs: &MemVfs) -> Database<MemVfs> {
    let mut db =
        Database::create_with_vfs(&contexts.commit(), vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
    let mut first = WriteBatch::new(R);
    for (at, vid) in IDS.iter().enumerate() {
        first.create_vertex(
            *vid,
            vec![L],
            vec![(P, CanonicalScalar::Int(at as i64 + 1))],
        );
    }
    for (id, src, dst, value) in EDGES {
        first.add_edge(
            EId(id),
            IDS[src],
            IDS[dst],
            vec![(P, CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(
        db.write(&contexts.commit(), first).await.unwrap(),
        CommitSeq(1)
    );
    db
}
fn matches(
    vertex: VId,
    relation: Option<RelationId>,
    direction: BufferedEdgeDirection,
    row: &Row,
) -> bool {
    relation.is_none_or(|relation| row.3 == relation)
        && match direction {
            BufferedEdgeDirection::Outgoing => row.1 == vertex,
            BufferedEdgeDirection::Incoming => row.2 == vertex,
            BufferedEdgeDirection::Undirected => row.1 == vertex || row.2 == vertex,
        }
}
fn expected(cut: u64) -> Vec<Row> {
    if cut == 0 {
        return Vec::new();
    }
    let mut edges: Vec<_> = EDGES
        .into_iter()
        .filter(|(id, _, _, _)| cut < 2 || *id != 2)
        .map(|(id, src, dst, p)| (id, src, dst, R, p))
        .collect();
    if cut >= 3 {
        edges.push((4, 1, 0, S, 20));
    }
    let vertex = |index| {
        vec![(
            P,
            CanonicalScalar::Int(if cut >= 2 && index == 0 {
                21
            } else {
                index as i64 + 1
            })
            .into(),
        )]
    };
    let mut rows: Vec<_> = edges
        .into_iter()
        .map(|(id, src, dst, relation, value)| {
            (
                EId(id),
                IDS[src],
                IDS[dst],
                relation,
                vec![(
                    P,
                    CanonicalScalar::Int(if cut >= 2 && id == 0 { 17 } else { value }).into(),
                )],
                vertex(src),
                vertex(dst),
            )
        })
        .collect();
    rows.sort_by_key(|row| row.0);
    rows
}
fn row(value: &BufferedEdgeEndpoints, cut: CommitSeq) -> Row {
    let edge = value.edge();
    assert!(edge.entry.visible_at(cut));
    assert_eq!(value.source_vertex().labels, [L]);
    assert_eq!(value.target_vertex().labels, [L]);
    assert_eq!(value.source_vertex().vid, edge.entry.src);
    assert_eq!(value.target_vertex().vid, edge.entry.dst);
    assert_eq!(
        core::ptr::eq(value.source_vertex(), value.target_vertex()),
        edge.entry.src == edge.entry.dst
    );
    (
        edge.entry.eid,
        edge.entry.src,
        edge.entry.dst,
        edge.entry.relation,
        edge.props.clone(),
        value.source_vertex().props.clone(),
        value.target_vertex().props.clone(),
    )
}
async fn check_cuts(view: &mut BufferedReadView<MemVfs>, contexts: &PurposeContexts, last: u64) {
    let cx = contexts.query();
    for cut in 0..=last {
        for endpoint in IDS {
            for relation in [None, Some(R), Some(S)] {
                for direction in [
                    BufferedEdgeDirection::Outgoing,
                    BufferedEdgeDirection::Incoming,
                    BufferedEdgeDirection::Undirected,
                ] {
                    let wanted: Vec<_> = expected(cut)
                        .into_iter()
                        .filter(|row| matches(endpoint, relation, direction, row))
                        .collect();
                    let mut scan = view
                        .stream_adjacency_at(
                            &cx,
                            endpoint,
                            relation,
                            direction,
                            CommitSeq(cut),
                            None,
                        )
                        .unwrap();
                    assert_eq!(scan.snapshot_seq(), CommitSeq(cut));
                    let mut actual = Vec::new();
                    while let Some(value) = scan.next(&cx).await.unwrap() {
                        actual.push(row(&value, CommitSeq(cut)));
                    }
                    assert!(scan.is_finished());
                    assert_eq!(actual, wanted);
                    drop(scan);
                    assert_eq!(
                        view.incident_edge_count_at(
                            &cx,
                            endpoint,
                            relation,
                            direction,
                            CommitSeq(cut)
                        )
                        .await
                        .unwrap(),
                        wanted.len() as u64
                    );
                }
            }
        }
    }
}

#[test]
fn buffered_incidence_preserves_history_pins_properties_and_compacted_reopen() {
    let ((), report) = run_async_under_lab(0xe6_33, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let retained_pool = pool();
        let mut pinned = cold(&contexts, &vfs, retained_pool.clone()).await;
        let mut second = WriteBatch::new(R);
        second.set_vertex_property(IDS[0], P, Some(CanonicalScalar::Int(21)));
        second.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(17)));
        second.delete_edge(EId(2));
        assert_eq!(
            db.write(&contexts.commit(), second).await.unwrap(),
            CommitSeq(2)
        );
        let mut third = WriteBatch::new(S);
        third.add_edge(EId(4), IDS[1], IDS[0], vec![(P, CanonicalScalar::Int(20))]);
        assert_eq!(
            db.write(&contexts.commit(), third).await.unwrap(),
            CommitSeq(3)
        );
        assert_eq!(pinned.frontier(), CommitSeq(1));
        check_cuts(&mut pinned, &contexts, 1).await;
        assert!(matches!(
            pinned.stream_adjacency_at(
                &cx,
                IDS[1],
                None,
                BufferedEdgeDirection::Undirected,
                CommitSeq(2),
                None
            ),
            Err(BufferedReadError::BeyondPublication { .. })
        ));
        let mut latest = cold(&contexts, &vfs, pool()).await;
        check_cuts(&mut latest, &contexts, 3).await;
        // Independent public resident storage agrees on exact visible topology.
        let mut resident: Vec<_> = db
            .edges()
            .unwrap()
            .into_iter()
            .map(|edge| {
                (
                    edge.entry.eid,
                    edge.entry.src,
                    edge.entry.dst,
                    edge.entry.relation,
                    edge.props,
                )
            })
            .collect();
        resident.sort_by_key(|row| row.0);
        assert_eq!(
            resident,
            expected(3)
                .into_iter()
                .map(|row| (row.0, row.1, row.2, row.3, row.4))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            latest
                .incident_edge_count(&cx, IDS[1], None, BufferedEdgeDirection::Undirected)
                .await
                .unwrap(),
            4
        );
        let mut suffix = latest
            .stream_adjacency_at(
                &cx,
                IDS[1],
                None,
                BufferedEdgeDirection::Undirected,
                CommitSeq(3),
                Some(EId(4)),
            )
            .unwrap();
        assert_eq!(
            suffix.next(&cx).await.unwrap().unwrap().edge().entry.eid,
            EId(u128::MAX)
        );
        assert!(suffix.next(&cx).await.unwrap().is_none());
        drop(suffix);
        db.compact(&contexts.commit()).await.unwrap();
        drop(db);
        let reopened =
            Database::open_with_vfs(&contexts.commit(), vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let mut compacted = cold(&contexts, &vfs, pool()).await;
        check_cuts(&mut compacted, &contexts, 3).await;
        // Old selected roots remain unaffected by either publication or compaction.
        check_cuts(&mut pinned, &contexts, 1).await;
        check_cuts(&mut latest, &contexts, 3).await;
        assert_eq!(reopened.frontier().unwrap(), CommitSeq(3));
        drop(pinned);
        assert_eq!(retained_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn high_degree_reads_and_counts_do_not_require_an_adjacency_result_vector() {
    let ((), report) = run_async_under_lab(0xe6_34, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db =
            Database::create_with_vfs(&contexts.commit(), vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let mut batch = WriteBatch::new(R);
        for (at, vid) in IDS[..2].iter().enumerate() {
            batch.create_vertex(
                *vid,
                vec![L],
                vec![(P, CanonicalScalar::Int(at as i64 + 1))],
            );
        }
        for id in 0..256 {
            batch.add_edge(
                EId(id),
                IDS[0],
                IDS[1],
                vec![(P, CanonicalScalar::Int(id as i64))],
            );
        }
        batch.add_edge(
            EId(u128::MAX),
            IDS[0],
            IDS[0],
            vec![(P, CanonicalScalar::Int(999))],
        );
        db.write(&contexts.commit(), batch).await.unwrap();
        let memory = pool();
        let mut view = cold(&contexts, &vfs, memory.clone()).await;
        assert!(
            view.adjacency(&cx, IDS[0], Some(R), 3).await.is_err(),
            "bounded vector must not truncate 257 edges to three"
        );
        let mut scan = view
            .stream_adjacency(&cx, IDS[0], Some(R), BufferedEdgeDirection::Outgoing)
            .unwrap();
        let mut count = 0;
        while let Some(value) = scan.next(&cx).await.unwrap() {
            let expected_id = if count < 256 { count } else { u128::MAX };
            assert_eq!(value.edge().entry.eid, EId(expected_id));
            assert_eq!(
                value.edge().props,
                [(
                    P,
                    CanonicalScalar::Int(if count < 256 { count as i64 } else { 999 }).into()
                )]
            );
            count += 1;
        }
        assert_eq!(count, 257);
        drop(scan);
        assert_eq!(
            view.incident_edge_count(&cx, IDS[0], Some(R), BufferedEdgeDirection::Undirected)
                .await
                .unwrap(),
            257
        );
        assert_eq!(
            view.incident_edge_count(&cx, IDS[0], Some(R), BufferedEdgeDirection::Incoming)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            view.incident_edge_count(&cx, IDS[1], Some(R), BufferedEdgeDirection::Incoming)
                .await
                .unwrap(),
            256
        );
        assert_eq!(
            view.incident_edge_count(&cx, IDS[3], None, BufferedEdgeDirection::Undirected)
                .await
                .unwrap(),
            0
        );
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_incidence_refusal_is_not_eof_and_outputs_outlive_the_view() {
    let ((), report) = run_async_under_lab(0xe6_35, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = seed(&contexts, &vfs).await;
        let memory = pool();
        let mut view = cold(&contexts, &vfs, memory.clone()).await;
        let mut scan = view
            .stream_adjacency(&cx, IDS[1], None, BufferedEdgeDirection::Undirected)
            .unwrap();
        drop(scan.next(&cx));
        assert_eq!(scan.work_used(), 0);
        let held = scan.next(&cx).await.unwrap().unwrap();
        assert_eq!(held.edge().entry.eid, EId(0));
        let failure = scan
            .next_candidate_with(&cx, &mut |_| Err::<(), _>("cancel"))
            .await;
        assert!(matches!(failure, Err(BufferedScanError::Control("cancel"))));
        assert!(scan.is_finished());
        scan.close();
        assert!(scan.next(&cx).await.unwrap().is_none());
        drop(scan);
        // Admission pressure cannot turn a nonempty graph into a successful zero.
        let hold = memory.reserve(&cx, memory.limit() - memory.used()).unwrap();
        assert!(
            view.incident_edge_count(&cx, IDS[1], None, BufferedEdgeDirection::Undirected)
                .await
                .is_err()
        );
        drop(hold);
        assert_eq!(
            view.incident_edge_count(&cx, IDS[1], None, BufferedEdgeDirection::Undirected)
                .await
                .unwrap(),
            4
        );
        drop(view);
        assert!(memory.used() > 0);
        assert_eq!(row(&held, CommitSeq(1)), expected(1)[0]);
        drop(held);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn first_cold_incidence_seeks_descriptors_without_reading_unrelated_graph_payloads() {
    let ((), report) = run_async_under_lab(0xe6_36, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        // Each independent source adds a separate block. None of these source
        // keys or destination intervals can contribute to endpoint zero.
        let mut unrelated = WriteBatch::new(R);
        for at in 0..256_u128 {
            let source = VId(1000 + at * 2);
            let target = VId(1001 + at * 2);
            for vertex in [source, target] {
                unrelated.create_vertex(vertex, vec![L], vec![(P, CanonicalScalar::Int(123))]);
            }
            unrelated.add_edge(
                EId(1000 + at),
                source,
                target,
                vec![(P, CanonicalScalar::Int(456))],
            );
        }
        assert_eq!(
            db.write(&contexts.commit(), unrelated).await.unwrap(),
            CommitSeq(2)
        );
        let memory = pool();
        let mut view = cold(&contexts, &vfs, memory.clone()).await;
        assert_eq!(
            view.buffer_stats().bypasses,
            0,
            "admission is distinct from payload refaults"
        );
        let baseline = memory.used();
        let mut missing = view
            .stream_adjacency(&cx, VId(500), None, BufferedEdgeDirection::Undirected)
            .unwrap();
        assert!(missing.next(&cx).await.unwrap().is_none());
        assert!(missing.is_finished());
        drop(missing);
        assert_eq!(
            view.buffer_stats().bypasses,
            0,
            "even the first absent seek faults no graph payload"
        );
        assert_eq!(
            memory.used(),
            baseline,
            "no per-query graph routing survives exhaustion"
        );

        for cut in [CommitSeq(1), CommitSeq(2)] {
            for direction in [
                BufferedEdgeDirection::Outgoing,
                BufferedEdgeDirection::Incoming,
                BufferedEdgeDirection::Undirected,
            ] {
                let before = view.buffer_stats().bypasses;
                let mut scan = view
                    .stream_adjacency_at(&cx, IDS[0], Some(R), direction, cut, None)
                    .unwrap();
                let mut actual = Vec::new();
                while let Some(value) = scan.next(&cx).await.unwrap() {
                    actual.push(row(&value, cut));
                }
                drop(scan);
                let expected: Vec<_> = expected(1)
                    .into_iter()
                    .filter(|row| matches(IDS[0], Some(R), direction, row))
                    .collect();
                assert_eq!(actual, expected);
                assert!(
                    view.buffer_stats().bypasses - before <= 32,
                    "a neighborhood of three identities must not read 256 unrelated blocks"
                );
                assert_eq!(memory.used(), baseline);
            }
        }
        // The same descriptor directory does not grant visibility at a future
        // cut, and the retained view keeps its original immutable generation.
        let mut update = WriteBatch::new(R);
        update.delete_edge(EId(0));
        db.write(&contexts.commit(), update).await.unwrap();
        assert_eq!(view.frontier(), CommitSeq(2));
        assert_eq!(
            view.incident_edge_count(&cx, IDS[0], Some(R), BufferedEdgeDirection::Outgoing)
                .await
                .unwrap(),
            2
        );
        assert!(matches!(
            view.stream_adjacency_at(
                &cx,
                IDS[0],
                Some(R),
                BufferedEdgeDirection::Outgoing,
                CommitSeq(3),
                None
            ),
            Err(BufferedReadError::BeyondPublication { .. })
        ));
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// An integration test's children resolve beside it, not under its own directory.
#[path = "buffered_incidence_stream/retained_gql.rs"]
mod retained_gql;
