//! Finite reachability probes through the actual pinned Database/native streams.
//! Owned complete-walk oracles are independent of production endpoint support.
use asupersync::lab::run_async_under_lab;
use fgdb::PreparedNativeRead;
use fgdb::{Database, DatabaseKeys, MemVfs, ReadError, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::edge_stream::EdgeScanError;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeSet;

const R: RelationId = RelationId(7);
const Q: RelationId = RelationId(8);
const P: PropertyKeyId = PropertyKeyId(9);
const IDS: [VId; 7] = [
    VId(0),
    VId(1),
    VId(2),
    VId(3),
    VId(42),
    VId(1_u128 << 100),
    VId(u128::MAX),
];
const MODES: [&str; 6] = [
    "WALK",
    "ALL SHORTEST WALK",
    "ANY SHORTEST WALK",
    "TRAIL",
    "ACYCLIC",
    "SIMPLE",
];
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x18; 32],
        DatabaseSecurityNamespaceId([0x29; 32]),
        [0x3a; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 10_000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Q") => Some(GraphSymbol::Relation(Q)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn prepare(input: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(input, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|r| r.values().to_vec()).collect()
}
fn atom(d: GlaDirection, lo: u32, hi: u32) -> String {
    let left = if d == GlaDirection::Reverse {
        "<-"
    } else {
        "-"
    };
    let right = if d == GlaDirection::Forward {
        "->"
    } else {
        "-"
    };
    format!("{left}[:R*{lo}..{hi}]{right}(x)")
}
fn statement(
    edge_root: bool,
    mode: &str,
    d: GlaDirection,
    lo: u32,
    hi: u32,
    anti: bool,
    cut: Option<CommitSeq>,
) -> String {
    let root = if edge_root { "(a)-[r:Q]->(b)" } else { "(a)" };
    let anchor = if edge_root { "b" } else { "a" };
    let cols = if edge_root { "r,a,b" } else { "a,a.p AS p" };
    let at = cut
        .map(|s| format!(" FOR SYSTEM_TIME AS OF SEQ {}", s.0))
        .unwrap_or_default();
    format!(
        "MATCH {root}{at} WHERE {}EXISTS {{ MATCH {mode} ({anchor}){} WHERE x.p > 5 }} RETURN {cols}",
        if anti { "NOT " } else { "" },
        atom(d, lo, hi)
    )
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut b = WriteBatch::new(R);
    for &id in &IDS {
        let v = if id == VId(3) {
            CanonicalScalar::Int(9)
        } else if id == VId(1) {
            CanonicalScalar::Null
        } else {
            CanonicalScalar::Int(0)
        };
        b.create_vertex(id, vec![], vec![(P, v)]);
    }
    for (eid, a, z) in [
        (10, 0, 1),
        (11, 0, 1),
        (12, 1, 2),
        (13, 2, 3),
        (14, 2, 1),
        (15, 3, 3),
    ] {
        b.add_edge(EId(eid), VId(a), VId(z), vec![]);
    }
    let mut q = WriteBatch::new(Q);
    q.add_edge(EId(0), VId(1_u128 << 100), VId(0), vec![]);
    db.write_atomic(cx, vec![b, q]).await.unwrap()
}
fn property(db: &Database<MemVfs>, cut: CommitSeq, v: VId) -> CanonicalScalar {
    db.vertex_at(v, cut)
        .unwrap()
        .unwrap()
        .props
        .iter()
        .find(|(p, _)| *p == P)
        .map(|(_, v)| v.clone())
        .unwrap_or(CanonicalScalar::Null)
}
fn support(
    edges: &[(EId, VId, VId)],
    start: VId,
    d: GlaDirection,
    lo: u32,
    hi: u32,
    mode: &str,
) -> BTreeSet<VId> {
    let mut layer = vec![(vec![start], Vec::<EId>::new())];
    let mut result = BTreeSet::new();
    for depth in 0..=hi {
        if depth >= lo {
            for (vs, es) in &layer {
                let valid = match mode {
                    "TRAIL" => es.iter().collect::<BTreeSet<_>>().len() == es.len(),
                    "ACYCLIC" => vs.iter().collect::<BTreeSet<_>>().len() == vs.len(),
                    "SIMPLE" => {
                        let n = vs.len() - usize::from(vs.len() > 1 && vs.last() == Some(&start));
                        vs[..n].iter().collect::<BTreeSet<_>>().len() == n
                    }
                    _ => true,
                };
                if valid {
                    result.insert(*vs.last().unwrap());
                }
            }
        }
        if depth == hi {
            break;
        }
        let mut next = Vec::new();
        for (vs, es) in layer {
            let v = *vs.last().unwrap();
            for &(eid, a, b) in edges {
                let mut dest = Vec::new();
                if d != GlaDirection::Reverse && a == v {
                    dest.push(b);
                }
                if d != GlaDirection::Forward && b == v && (d != GlaDirection::Undirected || a != b)
                {
                    dest.push(a);
                }
                for to in dest {
                    let mut vs = vs.clone();
                    vs.push(to);
                    let mut es = es.clone();
                    es.push(eid);
                    next.push((vs, es));
                }
            }
        }
        layer = next;
    }
    result
}
// One argument per axis of the fixture matrix the oracle is compared across.
#[allow(clippy::too_many_arguments)]
fn oracle(
    db: &Database<MemVfs>,
    cut: CommitSeq,
    edge_root: bool,
    mode: &str,
    d: GlaDirection,
    lo: u32,
    hi: u32,
    anti: bool,
) -> Vec<Vec<GraphValue>> {
    let all = db.edges_at(cut).unwrap();
    let edges: Vec<_> = all
        .iter()
        .filter(|e| e.entry.relation == R)
        .map(|e| (e.entry.eid, e.entry.src, e.entry.dst))
        .collect();
    let found = |v| {
        support(&edges, v, d, lo, hi, mode)
            .iter()
            .any(|&x| matches!(property(db,cut,x),CanonicalScalar::Int(p) if p>5))
    };
    let mut result = Vec::new();
    if edge_root {
        for e in all.iter().filter(|e| e.entry.relation == Q) {
            if found(e.entry.dst) != anti {
                result.push(vec![
                    GraphValue::Edge(e.entry.eid),
                    GraphValue::Vertex(e.entry.src),
                    GraphValue::Vertex(e.entry.dst),
                ]);
            }
        }
    } else {
        for v in IDS {
            if db.vertex_at(v, cut).unwrap().is_some() && found(v) != anti {
                result.push(vec![
                    GraphValue::Vertex(v),
                    GraphValue::Scalar(property(db, cut, v)),
                ]);
            }
        }
    }
    result.sort();
    result
}

#[test]
fn variable_probe_modes_match_eager_and_owned_oracles_at_every_historical_cut_and_pinned_generation()
 {
    let ((), report) = run_async_under_lab(0x766c_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let input = statement(false, "WALK", GlaDirection::Forward, 2, 4, true, None);
        let q = prepare(&input);
        let old = oracle(&db, basis, false, "WALK", GlaDirection::Forward, 2, 4, true);
        let mut paused = db.stream_graph_values_governed(&cx, &q, policy()).unwrap();
        let first = paused.next().unwrap().unwrap();
        drop(q);
        let mut b = WriteBatch::new(R);
        b.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-1)));
        let hidden = db.write(&commit, b).await.unwrap();
        let mut b = WriteBatch::new(R);
        b.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(9)));
        let visible = db.write(&commit, b).await.unwrap();
        let mut b = WriteBatch::new(R);
        b.delete_edge(EId(12));
        let removed = db.write(&commit, b).await.unwrap();
        let mut b = WriteBatch::new(Q);
        b.delete_vertex(VId(1));
        let cascade = db.write(&commit, b).await.unwrap();
        let mut b = WriteBatch::new(R);
        b.add_edge(EId(u128::MAX), VId(0), VId(2), vec![]);
        let connected = db.write(&commit, b).await.unwrap();
        let mut cases = Vec::new();
        for cut in [
            CommitSeq(0),
            basis,
            hidden,
            visible,
            removed,
            cascade,
            connected,
        ] {
            for mode in MODES {
                for d in [
                    GlaDirection::Forward,
                    GlaDirection::Reverse,
                    GlaDirection::Undirected,
                ] {
                    for (lo, hi) in [(0, 0), (1, 1), (2, 4), (0, 3)] {
                        for anti in [false, true] {
                            for edge_root in [false, true] {
                                let want = oracle(&db, cut, edge_root, mode, d, lo, hi, anti);
                                let text = statement(edge_root, mode, d, lo, hi, anti, None);
                                let q = prepare(&text);
                                assert_eq!(
                                    plain(
                                        &db.execute_graph_pattern_governed_at(
                                            &cx,
                                            &q,
                                            cut,
                                            policy()
                                        )
                                        .unwrap()
                                        .value
                                    ),
                                    want,
                                    "eager {text}"
                                );
                                let p = GqlParameters::new();
                                let n = PreparedNativeRead::prepare(
                                    &statement(edge_root, mode, d, lo, hi, anti, Some(cut)),
                                    &p,
                                    symbols,
                                )
                                .unwrap();
                                let (_, mut stream) = n.stream(&db, &cx, &p, policy()).unwrap();
                                assert_eq!(stream.snapshot_seq(), cut);
                                assert_eq!(
                                    plain(&stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                                    want,
                                    "stream {text}"
                                );
                                cases.push((cut, text, want));
                            }
                        }
                    }
                }
            }
        }
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let mut actual = vec![first];
        actual.extend(paused.by_ref().map(Result::unwrap));
        assert_eq!(plain(&actual), old);
        let p = GqlParameters::new();
        let n = PreparedNativeRead::prepare(&input, &p, symbols).unwrap();
        let (_, mut pinned) = n.stream_in_view(&view, &cx, &p, policy()).unwrap();
        assert_eq!(
            plain(&pinned.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
            old
        );
        // All source-generation assertions are native tests, not model claims.
        for (cut, text, want) in cases {
            let q = prepare(&text);
            let actual = if text.starts_with("MATCH (a)-") {
                db.stream_graph_edges_governed_at(&cx, &q, cut, policy())
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            } else {
                db.stream_graph_values_governed_at(&cx, &q, cut, policy())
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            };
            assert_eq!(plain(&actual), want);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn endpoint_filters_do_not_cut_transit_vertices_and_compound_atoms_reset_history() {
    let ((), report) = run_async_under_lab(0x766c_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit()).await;
        for text in [
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*3..3]->(x) WHERE x.p > 5 } RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*0..2]->(x)-[:R*1..2]->(y) WHERE y.p > 5 } RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*0..2]->(x), (a)-[:R*1..3]->(y) WHERE x <> y AND y.p > 5 } RETURN a",
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*0..0]->(x) } AND NOT EXISTS { MATCH (a)-[:R*1..4]->(x) WHERE x.p IS NULL } RETURN a",
            "MATCH (a)-[r:Q]->(b) WHERE EXISTS { MATCH (b)-[:R*2..4]->(x)-[:R]->(y) WHERE x.p IS NULL OR y.p > 5 } RETURN r,a,b",
        ] {
            let q = prepare(text);
            let eager = db
                .execute_graph_pattern_governed(&cx, &q, policy())
                .unwrap()
                .value;
            let p = GqlParameters::new();
            let n = PreparedNativeRead::prepare(text, &p, symbols).unwrap();
            let (_, mut s) = n.stream(&db, &cx, &p, policy()).unwrap();
            assert_eq!(
                s.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                eager,
                "{text}"
            );
        }
        let q =
            prepare("MATCH (a) WHERE EXISTS { MATCH (a)-[:R*3..3]->(x) WHERE x.p > 5 } RETURN a");
        assert!(
            db.stream_graph_values_governed(&cx, &q, policy())
                .unwrap()
                .map(Result::unwrap)
                .any(|r| r.values() == [GraphValue::Vertex(VId(0))])
        ); // NULL-valued vertex 1 remains transit.
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn proving_absence_is_polynomial_for_walk_ties_and_does_not_scan_unrelated_edges() {
    let ((), report) = run_async_under_lab(0x766c_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut r = WriteBatch::new(R);
        for i in 0..=40 {
            r.create_vertex(VId(i), vec![], vec![]);
        }
        for v in [9999, 10000, 10001] {
            r.create_vertex(VId(v), vec![], vec![]);
        }
        for i in 0..40 {
            for j in 0..2 {
                r.add_edge(EId(2 * i + j + 1), VId(i), VId(i + 1), vec![]);
            }
        }
        for id in 1000..3048 {
            r.add_edge(EId(id), VId(10000), VId(10001), vec![]);
        }
        let mut q = WriteBatch::new(Q);
        q.add_edge(EId(0), VId(9999), VId(0), vec![]);
        db.write_atomic(&commit, vec![r, q]).await.unwrap();
        for mode in &MODES[..3] {
            let text = format!(
                "MATCH (a)-[r:Q]->(b) WHERE NOT EXISTS {{ MATCH {mode} (b)-[:R*40..40]->(x) WHERE x = a }} RETURN r,a,b LIMIT 1"
            );
            let p = GqlParameters::new();
            let n = PreparedNativeRead::prepare(&text, &p, symbols).unwrap();
            let (_, mut s) = n
                .stream(&db, &cx, &p, GqlQueryPolicy::new(81, 1, 100_000, 10_000))
                .unwrap();
            assert!(s.next().unwrap().is_ok());
            assert_eq!(s.row_stats().snapshot_records, 81);
            assert!(s.next().is_none());
            let (_, mut refused) = n
                .stream(&db, &cx, &p, GqlQueryPolicy::new(80, 1, 100_000, 10_000))
                .unwrap();
            assert!(refused.next().unwrap().is_err());
            assert_eq!(refused.row_stats().result_rows, 0);
            assert!(refused.next().is_none());
        }
        let q = prepare(
            "MATCH (a)-[r:Q]->(b) WHERE EXISTS { MATCH (b)-[:R*40..40]->(x) } RETURN r,a,b LIMIT 1",
        );
        let mut s = db
            .stream_graph_edges_governed(&cx, &q, GqlQueryPolicy::new(41, 1, 100_000, 10_000))
            .unwrap();
        assert!(s.next().unwrap().is_ok());
        assert_eq!(s.row_stats().snapshot_records, 41);
        assert!(s.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_limits_binding_errors_and_future_snapshot_fences_apply_before_delivery() {
    let ((), report) = run_async_under_lab(0x766c_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        let basis = seed(&mut db, &c.commit()).await;
        for mode in MODES {
            let text = statement(false, mode, GlaDirection::Undirected, 2, 4, false, None);
            let p = GqlParameters::new();
            let n = PreparedNativeRead::prepare(&text, &p, symbols).unwrap();
            let (_, mut s) = n.stream(&db, &cx, &p, policy()).unwrap();
            let want = s.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            let r = s.row_stats();
            let e = s.evaluator_stats();
            assert!(!want.is_empty());
            let exact = GqlQueryPolicy::new(
                r.snapshot_records,
                r.result_rows,
                e.work_units,
                e.scratch_entries,
            );
            let (_, s) = n.stream(&db, &cx, &p, exact).unwrap();
            assert_eq!(s.collect::<Result<Vec<_>, _>>().unwrap(), want);
            for limit in [
                GqlQueryPolicy::new(r.snapshot_records - 1, 10000, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(1_000_000, r.result_rows - 1, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(1_000_000, 10000, e.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(1_000_000, 10000, u64::MAX, e.scratch_entries - 1),
            ] {
                let (_, mut s) = n.stream(&db, &cx, &p, limit).unwrap();
                let mut prefix = Vec::new();
                loop {
                    match s.next() {
                        Some(Ok(row)) => prefix.push(row),
                        Some(Err(_)) => break,
                        None => panic!("quota became absence"),
                    }
                }
                assert_eq!(prefix, want[..prefix.len()]);
                assert_eq!(s.row_stats().result_rows, prefix.len() as u64);
                assert!(s.next().is_none());
            }
        }
        let text =
            "MATCH (a) WHERE EXISTS { MATCH (a)-[:R*1..4]->(x) WHERE x.p > $floor } RETURN a";
        let p = GqlParameters::new().with_int64("floor", 5).unwrap();
        let n = PreparedNativeRead::prepare(text, &p, symbols).unwrap();
        assert!(n.stream(&db, &cx, &GqlParameters::new(), policy()).is_err());
        let high = GqlParameters::new().with_int64("floor", 100).unwrap();
        let (_, mut s) = n.stream(&db, &cx, &high, policy()).unwrap();
        assert!(s.next().is_none());
        let zero = prepare(
            "MATCH (a)-[r:Q]->(b) WHERE EXISTS { MATCH (b)-[:R*1..1024]->(x) } RETURN r,a,b LIMIT 0",
        );
        let mut s = db
            .stream_graph_edges_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(s.next().is_none());
        assert_eq!(s.row_stats().snapshot_records, 0);
        assert!(matches!(
            db.stream_graph_edges_governed_at(
                &cx,
                &zero,
                CommitSeq(basis.0 + 1),
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(EdgeScanError::Source(
                ReadError::BeyondFrontier { .. }
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
