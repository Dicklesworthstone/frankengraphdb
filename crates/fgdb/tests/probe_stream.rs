//! Native correlated existence checks share the original snapshot and meter.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, ReadError, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::edge_stream::{EdgeScanError, EdgeScanState};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(7);
const Q: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 5] = [VId(0), VId(1_u128 << 100), VId(u128::MAX), VId(17), VId(99)];
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x14; 32], DatabaseSecurityNamespaceId([0x25; 32]), [0x36; 32]) }
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 10_000, 5_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Q") => Some(GraphSymbol::Relation(Q)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)), _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn atom(name: &str, relation: &str, target: &str, direction: GlaDirection) -> String {
    let (left, right) = match direction { GlaDirection::Forward => ("-", "->"),
        GlaDirection::Reverse => ("<-", "-"), GlaDirection::Undirected => ("-", "-") };
    format!("{left}[{name}:{relation}]{right}({target})")
}
fn statement(outer: GlaDirection, inner: GlaDirection, anti: bool, cut: Option<CommitSeq>) -> String {
    let temporal = cut.map(|s| format!(" FOR SYSTEM_TIME AS OF SEQ {}", s.0)).unwrap_or_default();
    format!("MATCH (a){}{temporal} WHERE {}EXISTS {{ MATCH (b){} WHERE x.p > 5 }} RETURN r, a, b, b.p AS bp",
        atom("r", "R", "b", outer), if anti { "NOT " } else { "" }, atom("", "Q", "x", inner))
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut roots = WriteBatch::new(R);
    for (at, id) in IDS.into_iter().enumerate() {
        let props = if at == 3 { vec![] } else { vec![(P, if at == 4 { CanonicalScalar::Null }
            else { CanonicalScalar::Int(if at == 2 { 7 } else { 1 }) })] };
        roots.create_vertex(id, vec![], props);
    }
    for (id, from, to) in [(0, 0, 1), (1, 0, 1), (2, 1, 3)] {
        roots.add_edge(EId(id), IDS[from], IDS[to], vec![]);
    }
    let mut probes = WriteBatch::new(Q);
    for (id, from, to) in [(10, 1, 2), (11, 1, 2), (12, 2, 0), (13, 3, 4)] {
        probes.add_edge(EId(id), IDS[from], IDS[to], vec![]);
    }
    db.write_atomic(cx, vec![roots, probes]).await.unwrap()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> { rows.iter().map(|r| r.values().to_vec()).collect() }
fn field(db: &Database<MemVfs>, cut: CommitSeq, id: VId) -> CanonicalScalar {
    db.vertex_at(id, cut).unwrap().unwrap().props.iter().find(|(p, _)| *p == P)
        .map(|(_, v)| v.clone()).unwrap_or(CanonicalScalar::Null)
}
fn orient(from: VId, to: VId, direction: GlaDirection) -> Vec<(VId, VId)> {
    match direction { GlaDirection::Forward => vec![(from, to)], GlaDirection::Reverse => vec![(to, from)],
        GlaDirection::Undirected if from == to => vec![(from, to)], _ => vec![(from, to), (to, from)] }
}
// Independent owned-row Cartesian oracle. Count complete qualifying inner
// occurrences, then reduce to existence; the production index/DFS is not used.
fn oracle(db: &Database<MemVfs>, cut: CommitSeq, outer: GlaDirection, inner: GlaDirection, anti: bool) -> Vec<Vec<GraphValue>> {
    let edges = db.edges_at(cut).unwrap(); let mut rows = Vec::new();
    for edge in edges.iter().filter(|e| e.entry.relation == R) {
        for (a, b) in orient(edge.entry.src, edge.entry.dst, outer) {
            let mut witnesses = 0;
            for child in edges.iter().filter(|e| e.entry.relation == Q) {
                for (from, x) in orient(child.entry.src, child.entry.dst, inner) {
                    if from == b && matches!(field(db, cut, x), CanonicalScalar::Int(v) if v > 5) { witnesses += 1; }
                }
            }
            if (witnesses > 0) != anti {
                rows.push(vec![GraphValue::Edge(edge.entry.eid), GraphValue::Vertex(a), GraphValue::Vertex(b),
                    GraphValue::Scalar(field(db, cut, b))]);
            }
        }
    }
    rows.sort(); rows
}

#[test]
fn probe_truth_and_absence_are_exact_at_six_history_cuts_and_remain_pinned_through_reopen() {
    let ((), report) = run_async_under_lab(0x7072_6f01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let view = db.read_session().unwrap();
        let input = statement(GlaDirection::Forward, GlaDirection::Forward, false, None);
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(&input, &params, symbols).unwrap();
        let expected = oracle(&db, basis, GlaDirection::Forward, GlaDirection::Forward, false);
        assert_eq!(expected.len(), 2); // Parallel outer edges, not four inner witnesses.
        let (_, mut paused) = prepared.stream(&db, &cx, &params, policy()).unwrap();
        assert_eq!(paused.row_stats().snapshot_records, 0);
        let first = paused.next().unwrap().unwrap(); drop(prepared);
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(IDS[2], P, Some(CanonicalScalar::Int(8)));
        change.set_vertex_property(IDS[4], P, Some(CanonicalScalar::Int(7)));
        let changed = db.write(&commit, change).await.unwrap();
        let mut remove = WriteBatch::new(Q); remove.delete_edge(EId(10));
        let one_left = db.write(&commit, remove).await.unwrap();
        let mut remove = WriteBatch::new(Q); remove.delete_edge(EId(11));
        let gone = db.write(&commit, remove).await.unwrap();
        let mut cascade = WriteBatch::new(R); cascade.delete_vertex(IDS[4]);
        let retired = db.write(&commit, cascade).await.unwrap();
        let mut cases = Vec::new();
        let directions = [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected];
        for cut in [CommitSeq(0), basis, changed, one_left, gone, retired] {
            for outer in directions { for inner in directions { for anti in [false, true] {
                let want = oracle(&db, cut, outer, inner, anti);
                let q = prepare(&statement(outer, inner, anti, None));
                let eager = db.execute_graph_pattern_governed_at(&cx, &q, cut, policy()).unwrap();
                assert_eq!(plain(&eager.value), want);
                let mut stream = db.stream_graph_edges_governed_at(&cx, &q, cut, policy()).unwrap();
                assert_eq!(plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()), want);
                let native = PreparedNativeRead::prepare(&statement(outer, inner, anti, Some(cut)), &params, symbols).unwrap();
                let (_, mut stream) = native.stream(&db, &cx, &params, policy()).unwrap();
                assert_eq!(stream.snapshot_seq(), cut);
                assert_eq!(plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()), want);
                cases.push((cut, outer, inner, anti, want));
            } } }
        }
        db.compact(&commit).await.unwrap(); drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        let mut old = vec![first]; old.extend(paused.by_ref().map(Result::unwrap));
        assert_eq!(plain(&old), expected); assert_eq!(paused.snapshot_seq(), basis);
        let native = PreparedNativeRead::prepare(&input, &params, symbols).unwrap();
        let (_, mut pinned) = native.stream_in_view(&view, &cx, &params, policy()).unwrap();
        assert_eq!(plain(&pinned.by_ref().collect::<Result<Vec<_>, _>>().unwrap()), expected);
        for (cut, outer, inner, anti, want) in cases {
            let q = prepare(&statement(outer, inner, anti, None));
            let mut stream = db.stream_graph_edges_governed_at(&cx, &q, cut, policy()).unwrap();
            assert_eq!(plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()), want);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn correlated_vertex_only_and_sequential_probes_do_not_export_locals_or_multiply_rows() {
    let ((), report) = run_async_under_lab(0x7072_6f02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        seed(&mut db, &contexts.commit()).await;
        for input in [
            "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b) } RETURN r, a, b",
            "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b) WHERE b.p IS NULL } RETURN r, a, b",
            "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:Q]->(x) WHERE x.p > 5 } AND NOT EXISTS { MATCH (b)-[:Q]->(x) WHERE x.p IS NULL } RETURN r, a, b",
            "MATCH (a)-[r:R]->(b)-[s:Q]->(c) WHERE EXISTS { MATCH (c)-[:Q]->(a) } RETURN r, a, s, b, c",
        ] {
            let q = prepare(input);
            let expected = db.execute_graph_pattern_governed(&cx, &q, policy()).unwrap().value;
            assert!(!expected.is_empty(), "{input}");
            let native = PreparedNativeRead::prepare(input, &GqlParameters::new(), symbols).unwrap();
            let (_, mut stream) = native.stream(&db, &cx, &GqlParameters::new(), policy()).unwrap();
            let rows = stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected, "{input}");
            assert_eq!(stream.row_stats().result_rows, rows.len() as u64);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_witness_uses_two_candidate_records_and_anti_join_quota_refusal_is_not_absence() {
    let ((), report) = run_async_under_lab(0x7072_6f03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut roots = WriteBatch::new(R);
        for i in 0..5 { roots.create_vertex(VId(i), vec![], vec![]); }
        roots.add_edge(EId(0), VId(0), VId(1), vec![]);
        let mut probes = WriteBatch::new(Q);
        probes.add_edge(EId(1), VId(1), VId(2), vec![]);
        for id in 2..2050 { probes.add_edge(EId(id), VId(3), VId(4), vec![]); }
        db.write_atomic(&commit, vec![roots, probes]).await.unwrap();
        let positive = prepare("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:Q]->(x) } RETURN r, a, b LIMIT 1");
        let small = GqlQueryPolicy::new(2, 1, 10_000, 10_000);
        let mut stream = db.stream_graph_edges_governed(&cx, &positive, small).unwrap();
        assert_eq!(stream.next().unwrap().unwrap().values()[0], GraphValue::Edge(EId(0)));
        assert_eq!(stream.row_stats().snapshot_records, 2);
        assert!(stream.next().is_none());
        let negative = prepare("MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:Q]->(a) } RETURN r, a, b LIMIT 1");
        let mut stream = db.stream_graph_edges_governed(&cx, &negative, GqlQueryPolicy::new(1, 1, 10_000, 10_000)).unwrap();
        assert!(matches!(stream.next(), Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(stream.row_stats().result_rows, 0);
        assert_eq!(stream.state(), EdgeScanState::Failed);
        assert!(stream.next().is_none());
        let mut retry = db.stream_graph_edges_governed(&cx, &negative, small).unwrap();
        assert!(retry.next().unwrap().is_ok());
        assert_eq!(retry.row_stats().snapshot_records, 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn probe_limits_are_cumulative_and_binding_frontier_and_unsupported_errors_precede_delivery() {
    let ((), report) = run_async_under_lab(0x7072_6f04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        let basis = seed(&mut db, &contexts.commit()).await;
        let q = prepare(&statement(GlaDirection::Undirected, GlaDirection::Undirected, false, None));
        let mut full = db.stream_graph_edges_governed(&cx, &q, policy()).unwrap();
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap(); let r = full.row_stats(); let e = full.evaluator_stats();
        assert!(!expected.is_empty());
        let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
        let mut retry = db.stream_graph_edges_governed(&cx, &q, exact).unwrap();
        assert_eq!(retry.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
        for limited in [GqlQueryPolicy::new(r.snapshot_records - 1, 10000, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(100000, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(100000, 10000, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(100000, 10000, u64::MAX, e.scratch_entries - 1)] {
            let mut stream = db.stream_graph_edges_governed(&cx, &q, limited).unwrap(); let mut delivered = Vec::new();
            loop { match stream.next() {
                Some(Ok(row)) => delivered.push(row),
                Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                other => panic!("refusal became exhaustion: {other:?}"),
            } }
            assert_eq!(delivered, expected[..delivered.len()]);
            assert_eq!(stream.row_stats().result_rows, delivered.len() as u64);
            assert_eq!(stream.state(), EdgeScanState::Failed); stream.close(); assert!(stream.next().is_none());
        }
        let params = GqlParameters::new().with_int64("floor", 5).unwrap();
        let input = statement(GlaDirection::Forward, GlaDirection::Forward, false, None).replace("> 5", "> $floor");
        let native = PreparedNativeRead::prepare(&input, &params, symbols).unwrap();
        assert!(matches!(native.stream(&db, &cx, &GqlParameters::new(), policy()), Err(QueryError::PatternText(_))));
        let (_, mut low) = native.stream(&db, &cx, &params, policy()).unwrap();
        let high = GqlParameters::new().with_int64("floor", 100).unwrap();
        let (_, mut high) = native.stream(&db, &cx, &high, policy()).unwrap();
        assert_eq!(low.by_ref().collect::<Result<Vec<_>, _>>().unwrap().len(), 2);
        assert!(high.next().is_none());
        let zero = prepare(&(statement(GlaDirection::Forward, GlaDirection::Forward, false, None) + " LIMIT 0"));
        let mut zero_stream = db.stream_graph_edges_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0)).unwrap();
        assert!(zero_stream.next().is_none()); assert_eq!(zero_stream.row_stats().snapshot_records, 0);
        assert!(matches!(db.stream_graph_edges_governed_at(&cx, &zero, CommitSeq(basis.0 + 1), GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(EdgeScanError::Source(ReadError::BeyondFrontier { .. })))));
        let unsupported = prepare("MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (x)-[:Q]->(y) } RETURN r, a, b LIMIT 0");
        assert!(matches!(db.stream_graph_edges_governed(&cx, &unsupported, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(EdgeScanError::Plan(_)))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
