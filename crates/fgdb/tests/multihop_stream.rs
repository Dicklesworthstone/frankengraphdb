//! Public multi-hop reads over real MVCC storage and existing native dispatch.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
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
        [0x44; 32],
        DatabaseSecurityNamespaceId([0x55; 32]),
        [0x66; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 5_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn text(direction: GlaDirection, shape: usize, cut: Option<CommitSeq>, suffix: &str) -> String {
    let (left, right) = match direction {
        GlaDirection::Forward => ("-", "->"),
        GlaDirection::Reverse => ("<-", "-"),
        GlaDirection::Undirected => ("-", "-"),
    };
    let second = match shape {
        1 => format!(", (a){left}[s:R]{right}(c)"),
        2 => format!("{left}[s:R]{right}(a)"),
        _ => format!("{left}[s:R]{right}(c)"),
    };
    let temporal = cut
        .map(|s| format!(" FOR SYSTEM_TIME AS OF SEQ {}", s.0))
        .unwrap_or_default();
    let end = if shape == 2 { "a AS c" } else { "c" };
    format!(
        "MATCH (a){left}[r:R]{right}(b){second}{temporal} RETURN r, a, s, b, {end}, r.p AS rp, s.p AS sp, b.p AS bp{suffix}"
    )
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in IDS {
        batch.create_vertex(id, vec![], vec![(P, CanonicalScalar::Int(3))]);
    }
    for (id, a, b, value) in [
        (0, 0, 1, Some(2)),
        (1, 1, 2, Some(-4)),
        (2, 1, 2, Some(7)),
        (3, 2, 0, None),
        (u128::MAX, 1, 1, Some(9)),
    ] {
        batch.add_edge(
            EId(id),
            IDS[a],
            IDS[b],
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
fn property(props: &[(PropertyKeyId, CanonicalScalar)]) -> GraphValue {
    GraphValue::Scalar(
        props
            .iter()
            .find(|(key, _)| *key == P)
            .map(|(_, v)| v.clone())
            .unwrap_or(CanonicalScalar::Null),
    )
}
// Independent Cartesian relation join over owned historical API rows. No
// production cursor positions, incidence seeks, filters or collector is used.
fn oracle(
    db: &Database<MemVfs>,
    cut: CommitSeq,
    direction: GlaDirection,
    shape: usize,
) -> Vec<Vec<GraphValue>> {
    let edges = db.edges_at(cut).unwrap();
    let orient = |a, b| match direction {
        GlaDirection::Forward => vec![(a, b)],
        GlaDirection::Reverse => vec![(b, a)],
        GlaDirection::Undirected if a == b => vec![(a, b)],
        _ => vec![(a, b), (b, a)],
    };
    let mut rows = Vec::new();
    for r in &edges {
        for (a, b) in orient(r.entry.src, r.entry.dst) {
            if r.entry.relation != R {
                continue;
            }
            for s in &edges {
                for (from, c) in orient(s.entry.src, s.entry.dst) {
                    if s.entry.relation != R
                        || from != (if shape == 1 { a } else { b })
                        || (shape == 2 && c != a)
                    {
                        continue;
                    }
                    rows.push(vec![
                        GraphValue::Edge(r.entry.eid),
                        GraphValue::Vertex(a),
                        GraphValue::Edge(s.entry.eid),
                        GraphValue::Vertex(b),
                        GraphValue::Vertex(c),
                        property(&r.props),
                        property(&s.props),
                        property(&db.vertex_at(b, cut).unwrap().unwrap().props),
                    ]);
                }
            }
        }
    }
    rows.sort();
    rows
}

#[test]
fn native_and_direct_join_streams_preserve_history_properties_and_pins_through_reopen() {
    let ((), report) = run_async_under_lab(0x6a6f_6901, |root| async move {
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
        let input = text(GlaDirection::Undirected, 0, None, "");
        let q = PreparedNativeRead::prepare(&input, &GqlParameters::new(), symbols).unwrap();
        let expected = oracle(&db, basis, GlaDirection::Undirected, 0);
        let (_, mut opened) = q.stream(&db, &cx, &GqlParameters::new(), policy()).unwrap();
        assert_eq!(opened.row_stats().snapshot_records, 0);
        assert_eq!(opened.evaluator_stats().work_units, 0);
        let first = opened.next().unwrap().unwrap();
        drop(q);
        let mut update = WriteBatch::new(R);
        update.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(i64::MIN)));
        update.set_vertex_property(IDS[1], P, Some(CanonicalScalar::Int(i64::MAX)));
        update.delete_edge(EId(2));
        let changed = db.write(&commit, update).await.unwrap();
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(IDS[1]);
        let retired = db.write(&commit, cascade).await.unwrap();
        let mut cases = Vec::new();
        for cut in [CommitSeq(0), basis, changed, retired] {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for shape in 0..3 {
                    let wanted = oracle(&db, cut, direction, shape);
                    let q = prepare(&text(direction, shape, None, ""));
                    let eager = db
                        .execute_graph_pattern_governed_at(&cx, &q, cut, policy())
                        .unwrap()
                        .value;
                    assert_eq!(plain(&eager), wanted);
                    let mut stream = db
                        .stream_graph_edges_governed_at(&cx, &q, cut, policy())
                        .unwrap();
                    assert_eq!(
                        plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                        wanted
                    );
                    let native = PreparedNativeRead::prepare(
                        &text(direction, shape, Some(cut), ""),
                        &GqlParameters::new(),
                        symbols,
                    )
                    .unwrap();
                    let (_, mut stream) = native
                        .stream(&db, &cx, &GqlParameters::new(), policy())
                        .unwrap();
                    assert_eq!(stream.snapshot_seq(), cut);
                    assert_eq!(
                        plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                        wanted
                    );
                    cases.push((cut, direction, shape, wanted));
                }
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
        assert_eq!(opened.snapshot_seq(), basis);
        for (cut, direction, shape, wanted) in cases {
            let native = PreparedNativeRead::prepare(
                &text(direction, shape, Some(cut), ""),
                &GqlParameters::new(),
                symbols,
            )
            .unwrap();
            let (_, mut stream) = native
                .stream(&db, &cx, &GqlParameters::new(), policy())
                .unwrap();
            assert_eq!(
                plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                wanted
            );
        }
        let q = prepare(&input);
        let mut old = pinned
            .stream_graph_edges_governed(&cx, &q, policy())
            .unwrap();
        assert_eq!(
            plain(&old.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
            expected
        );
        let native = PreparedNativeRead::prepare(&input, &GqlParameters::new(), symbols).unwrap();
        let (_, mut old) = native
            .stream_in_view(&pinned, &cx, &GqlParameters::new(), policy())
            .unwrap();
        assert_eq!(
            plain(&old.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn total_quotas_parameter_binding_and_frontier_fences_still_precede_partial_delivery() {
    let ((), report) = run_async_under_lab(0x6a6f_6902, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let q = prepare(&text(GlaDirection::Undirected, 1, None, " SKIP 1 LIMIT 5"));
        let mut full = db.stream_graph_edges_governed(&cx, &q, policy()).unwrap();
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let r = full.row_stats();
        let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            r.snapshot_records,
            r.result_rows,
            e.work_units,
            e.scratch_entries,
        );
        let mut retry = db.stream_graph_edges_governed(&cx, &q, exact).unwrap();
        assert_eq!(
            retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected
        );
        assert_eq!((retry.row_stats(), retry.evaluator_stats()), (r, e));
        for p in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut c = db.stream_graph_edges_governed(&cx, &q, p).unwrap();
            let mut prefix = Vec::new();
            loop {
                match c.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("quota refusal changed into: {other:?}"),
                }
            }
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(c.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(c.state(), EdgeScanState::Failed);
            c.close();
            assert!(c.next().is_none());
        }
        let input = "MATCH (a)-[r:R]->(b)-[s:R]->(c) WHERE r.p > $floor OR s.p IS NULL RETURN r, a, s, b, c";
        let args = GqlParameters::new().with_int64("floor", 3).unwrap();
        let mut calls = 0;
        let native = PreparedNativeRead::prepare(input, &args, |kind, name: &str| {
            calls += 1;
            symbols(kind, name)
        })
        .unwrap();
        let resolved = calls;
        for floor in [-5, 3, 10] {
            let args = GqlParameters::new().with_int64("floor", floor).unwrap();
            let q = PreparedGraphText::prepare(input, symbols)
                .unwrap()
                .bind_parameters(&args)
                .unwrap();
            let want = db
                .execute_graph_pattern_governed(&cx, &q, policy())
                .unwrap()
                .value;
            let (_, mut c) = native
                .stream_in_view(&pinned, &cx, &args, policy())
                .unwrap();
            assert_eq!(c.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), want);
        }
        assert_eq!(calls, resolved);
        assert!(
            native
                .stream(&db, &cx, &GqlParameters::new(), policy())
                .is_err()
        );
        let zero = prepare(&text(GlaDirection::Forward, 0, None, " LIMIT 0"));
        for future in [
            db.stream_graph_edges_governed_at(
                &cx,
                &zero,
                CommitSeq(basis.0 + 1),
                GqlQueryPolicy::new(0, 0, 0, 0),
            )
            .map(|_| ()),
            pinned
                .stream_graph_edges_governed_at(
                    &cx,
                    &zero,
                    CommitSeq(basis.0 + 1),
                    GqlQueryPolicy::new(0, 0, 0, 0),
                )
                .map(|_| ()),
        ] {
            assert!(matches!(
                future,
                Err(GqlQueryError::Source(EdgeScanError::Source(_)))
            ));
        }
        let mut zero = db
            .stream_graph_edges_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(zero.next().is_none());
        assert_eq!(zero.row_stats().snapshot_records, 0);
        let invalid = prepare("MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, c");
        assert!(matches!(
            db.stream_graph_edges_governed(&cx, &invalid, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(EdgeScanError::Plan(_)))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn limit_one_joins_only_two_candidate_histories_despite_unrelated_graph_size() {
    let ((), report) = run_async_under_lab(0x6a6f_6903, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut b = WriteBatch::new(R);
        for id in 0..2051 {
            b.create_vertex(VId(id), vec![], vec![]);
        }
        b.add_edge(EId(0), VId(0), VId(1), vec![]);
        b.add_edge(EId(1), VId(1), VId(2), vec![]);
        for id in 3..2051 {
            b.add_edge(EId(id), VId(id), VId(id), vec![]);
        }
        db.write(&commit, b).await.unwrap();
        let q = prepare("MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, s, b, c LIMIT 1");
        let mut c = db
            .stream_graph_edges_governed(&cx, &q, GqlQueryPolicy::new(2, 1, 2000, 256))
            .unwrap();
        assert_eq!(c.row_stats().snapshot_records, 0);
        let row = c.next().unwrap().unwrap();
        assert_eq!(
            row.values(),
            &[
                GraphValue::Edge(EId(0)),
                GraphValue::Vertex(VId(0)),
                GraphValue::Edge(EId(1)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2))
            ]
        );
        assert_eq!(c.row_stats().snapshot_records, 2);
        assert_eq!(c.state(), EdgeScanState::Exhausted);
        let e = c.evaluator_stats();
        assert!(c.next().is_none());
        assert_eq!(c.evaluator_stats(), e);
        let q = prepare("MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, s, b, c");
        let mut c = db
            .stream_graph_edges_governed(&cx, &q, GqlQueryPolicy::new(2, 1, 2000, 256))
            .unwrap();
        assert_eq!(c.next().unwrap().unwrap(), row);
        c.close();
        assert!(c.next().is_none());
        assert_eq!(c.evaluator_stats(), e);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
