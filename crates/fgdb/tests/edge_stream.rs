//! Ordered one-edge execution against real MVCC storage, not an alternate graph.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::edge_stream::{EdgeScanError, EdgeScanState};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(7);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 3] = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa4; 32],
        DatabaseSecurityNamespaceId([0xa5; 32]),
        [0xa6; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 5_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn prepared(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn query(
    direction: GlaDirection,
    filter: &str,
    skip: u64,
    limit: u64,
) -> PreparedGraphPattern<GraphValueRow> {
    let (left, right) = match direction {
        GlaDirection::Forward => ("-", "->"),
        GlaDirection::Reverse => ("<-", "-"),
        GlaDirection::Undirected => ("-", "-"),
    };
    prepared(&format!(
        "MATCH (a){left}[r:R]{right}(b) {filter} RETURN r, a, b, r.p AS ep, a.p AS ap, b.p AS bp SKIP {skip} LIMIT {limit}"
    ))
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, n) in IDS.into_iter().zip([4, 2, 1]) {
        batch.create_vertex(id, vec![LabelId(1)], vec![(P, CanonicalScalar::Int(n))]);
    }
    for (eid, src, dst, value) in [
        (0, 0, 1, Some(2)),
        (1, 2, 0, Some(-1)),
        (2, 2, 0, Some(5)),
        (3, 1, 1, None),
        (u128::MAX, 1, 2, Some(7)),
    ] {
        batch.add_edge(
            EId(eid),
            IDS[src],
            IDS[dst],
            value
                .map(|v| vec![(P, CanonicalScalar::Int(v))])
                .unwrap_or_default(),
        );
    }
    db.write(cx, batch).await.unwrap()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

// The ordinary point/collection API is the source oracle. Build whole owned
// rows without interpreting GLA, then sort; do not use the stream's index walk,
// row collector, filter machinery, candidate order or pagination implementation.
fn oracle(db: &Database<MemVfs>, cut: CommitSeq, direction: GlaDirection) -> Vec<Vec<GraphValue>> {
    let vertex = |id| {
        let row = db
            .vertex_at(id, cut)
            .unwrap()
            .expect("admitted edge endpoint");
        GraphValue::Scalar(
            row.props
                .into_iter()
                .find(|(key, _)| *key == P)
                .map(|(_, v)| v)
                .unwrap_or(CanonicalScalar::Null),
        )
    };
    let mut rows = Vec::new();
    for edge in db.edges_at(cut).unwrap() {
        if edge.entry.relation != R {
            continue;
        }
        let entry = edge.entry;
        let property = GraphValue::Scalar(
            edge.props
                .into_iter()
                .find(|(key, _)| *key == P)
                .map(|(_, v)| v)
                .unwrap_or(CanonicalScalar::Null),
        );
        let ends = match direction {
            GlaDirection::Forward => vec![(entry.src, entry.dst)],
            GlaDirection::Reverse => vec![(entry.dst, entry.src)],
            GlaDirection::Undirected if entry.src == entry.dst => vec![(entry.src, entry.dst)],
            _ => vec![(entry.src, entry.dst), (entry.dst, entry.src)],
        };
        for (src, dst) in ends {
            rows.push(vec![
                GraphValue::Edge(entry.eid),
                GraphValue::Vertex(src),
                GraphValue::Vertex(dst),
                property.clone(),
                vertex(src),
                vertex(dst),
            ]);
        }
    }
    rows.sort();
    rows
}

#[test]
fn streams_match_independent_rows_on_every_cut_before_and_after_recovery_and_cascades() {
    let ((), report) = run_async_under_lab(0xed6e_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let q = query(GlaDirection::Undirected, "", 0, 100);
        let expected = oracle(&db, basis, GlaDirection::Undirected);
        let mut opened = db.stream_graph_edges_governed(&cx, &q, policy()).unwrap();
        assert_eq!(opened.row_stats().snapshot_records, 0);
        assert_eq!(opened.evaluator_stats().work_units, 0);
        assert_eq!(opened.snapshot_seq(), basis);
        let first = opened.next().unwrap().unwrap();
        assert_eq!(first.values(), expected[0].as_slice());
        // Edge payload, endpoint payload and edge retirement all get their own
        // history. Old streams must never borrow fields from the newer image.
        let mut update = WriteBatch::new(R);
        update.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(100)));
        update.set_vertex_property(IDS[0], P, Some(CanonicalScalar::Int(999)));
        update.delete_edge(EId(2));
        let changed = db.write(&commit, update).await.unwrap();
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(IDS[1]);
        let retired = db.write(&commit, cascade).await.unwrap();
        let mut future = WriteBatch::new(R);
        future.add_edge(EId(4), IDS[2], IDS[0], vec![(P, CanonicalScalar::Int(8))]);
        let frontier = db.write(&commit, future).await.unwrap();
        let mut cuts = Vec::new();
        for seq in [CommitSeq(0), basis, changed, retired, frontier] {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                let q = query(direction, "", 0, 100);
                let wanted = oracle(&db, seq, direction);
                let mut stream = db
                    .stream_graph_edges_governed_at(&cx, &q, seq, policy())
                    .unwrap();
                assert_eq!(
                    plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                    wanted
                );
                assert_eq!(stream.row_stats().result_rows, wanted.len() as u64);
                assert_eq!(stream.state(), EdgeScanState::Exhausted);
                assert!(stream.next().is_none());
                cuts.push((seq, direction, wanted));
            }
        }
        for filter in [
            "WHERE a <> b",
            "WHERE r.p IS NULL OR r.p > a.p",
            "WHERE a.p >= 1 AND b.p IS NOT NULL",
        ] {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                let q = query(direction, filter, 1, 3);
                let expected = db
                    .execute_graph_pattern_governed_at(&cx, &q, changed, policy())
                    .unwrap()
                    .value;
                let mut stream = db
                    .stream_graph_edges_governed_at(&cx, &q, changed, policy())
                    .unwrap();
                assert_eq!(
                    stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected
                );
            }
        }
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let mut remaining = vec![first];
        remaining.extend(opened.by_ref().map(Result::unwrap));
        assert_eq!(plain(&remaining), expected);
        // The source view, not the mutable database or query, owns the cut.
        for (seq, direction, wanted) in cuts {
            let q = query(direction, "", 0, 100);
            let mut stream = db
                .stream_graph_edges_governed_at(&cx, &q, seq, policy())
                .unwrap();
            assert_eq!(
                plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                wanted
            );
        }
        let mut old = pinned
            .stream_graph_edges_governed(&cx, &q, policy())
            .unwrap();
        assert_eq!(
            plain(&old.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
            expected
        );
        let mut old_at = pinned
            .stream_graph_edges_governed_at(&cx, &q, basis, policy())
            .unwrap();
        assert_eq!(
            plain(&old_at.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_quotas_are_cumulative_and_error_precedence_does_not_turn_into_empty_results() {
    let ((), report) = run_async_under_lab(0xed6e_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let q = query(
            GlaDirection::Undirected,
            "WHERE r.p IS NULL OR r.p > 0",
            0,
            100,
        );
        let mut stream = db.stream_graph_edges_governed(&cx, &q, policy()).unwrap();
        let expected = stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let r = stream.row_stats();
        let e = stream.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            r.snapshot_records,
            r.result_rows,
            e.work_units,
            e.scratch_entries,
        );
        let mut retry = db.stream_graph_edges_governed(&cx, &q, exact).unwrap();
        // Pausing or grouping pulls in pairs cannot buy a new query allowance.
        let mut rows = Vec::new();
        while retry.state() == EdgeScanState::Open {
            rows.extend(retry.by_ref().take(2).map(Result::unwrap));
        }
        assert_eq!(rows, expected);
        assert_eq!(retry.evaluator_stats(), e);
        for p in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut cursor = db.stream_graph_edges_governed(&cx, &q, p).unwrap();
            let mut delivered = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => delivered.push(row),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("quota error hidden or changed: {other:?}"),
                }
            }
            assert_eq!(delivered, expected[..delivered.len()]);
            assert_eq!(cursor.row_stats().result_rows, delivered.len() as u64);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let empty = query(GlaDirection::Forward, "", 0, 0);
        for result in [
            db.stream_graph_edges_governed_at(
                &cx,
                &empty,
                CommitSeq(basis.0 + 1),
                GqlQueryPolicy::new(0, 0, 0, 0),
            )
            .map(|_| ()),
            pinned
                .stream_graph_edges_governed_at(
                    &cx,
                    &empty,
                    CommitSeq(basis.0 + 1),
                    GqlQueryPolicy::new(0, 0, 0, 0),
                )
                .map(|_| ()),
        ] {
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(EdgeScanError::Source(_)))
            ));
        }
        let unsupported = prepared("MATCH (a)-[r:R]->(b) RETURN a, r");
        assert!(matches!(
            db.stream_graph_edges_governed(&cx, &unsupported, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(EdgeScanError::Plan(_)))
        ));
        let mut zero = db
            .stream_graph_edges_governed(&cx, &empty, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(zero.next().is_none());
        assert_eq!(zero.row_stats().snapshot_records, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn limit_one_reads_one_history_not_a_thousand_edge_table_and_close_does_not_drain() {
    let ((), report) = run_async_under_lab(0xed6e_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in IDS {
            batch.create_vertex(id, vec![], vec![]);
        }
        for id in 0..1024 {
            batch.add_edge(
                EId(id),
                IDS[0],
                IDS[2],
                vec![(P, CanonicalScalar::Int(id as i64))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let q = query(GlaDirection::Undirected, "", 0, 1);
        let p = GqlQueryPolicy::new(1, 1, 512, 128);
        let mut one = db.stream_graph_edges_governed(&cx, &q, p).unwrap();
        assert_eq!(one.evaluator_stats().work_units, 0);
        let first = one.next().unwrap().unwrap();
        assert_eq!(first.values()[0], GraphValue::Edge(EId(0)));
        assert_eq!(one.row_stats().snapshot_records, 1);
        assert_eq!(one.state(), EdgeScanState::Exhausted);
        let stats = one.evaluator_stats();
        assert!(one.next().is_none());
        assert_eq!(one.evaluator_stats(), stats);
        let unlimited = query(GlaDirection::Undirected, "", 0, u64::MAX);
        let mut early = db.stream_graph_edges_governed(&cx, &unlimited, p).unwrap();
        assert_eq!(early.next().unwrap().unwrap(), first);
        early.close();
        early.close();
        assert_eq!(early.state(), EdgeScanState::Closed);
        assert!(early.next().is_none());
        assert_eq!(early.evaluator_stats(), stats);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
