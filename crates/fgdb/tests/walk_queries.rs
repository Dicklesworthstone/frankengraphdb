//! Bounded WALK on real Chronicle/Strata reads and canonical transaction overlays.
//! The independent oracle multiplies raw edge occurrences one hop at a time.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow,
    WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText,
    PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const ROOT: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const N: PropertyKeyId = PropertyKeyId(1);
const HEAD: &str = "MATCH WALK (a:Root)-[:R*0..3]->(b) WHERE b.n >= $floor";
type Plain = (VId, VId, i64);
type Summary = (VId, u64, u64, i128);
type OptionalRow = (VId, VId, Option<VId>, Option<i64>);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1; 32], DatabaseSecurityNamespaceId([0xc2; 32]), [0xc3; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 5_000_000, 2_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Root") => Some(GraphSymbol::Label(ROOT)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(N)),
        _ => None,
    }
}
fn arguments(floor: i64) -> GqlParameters {
    GqlParameters::new().with_int64("floor", floor).unwrap()
}
fn pattern(floor: i64, limit: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
    let tail = limit.map_or(String::new(), |count| format!(" LIMIT {count}"));
    PreparedGraphText::prepare(&format!("{HEAD} RETURN a,b,b.n AS score ORDER BY a,b{tail}"), symbols)
        .unwrap().bind_parameters(&arguments(floor)).unwrap()
}
fn grouped(floor: i64) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(
        &format!("{HEAD} RETURN a,COUNT(*) AS walks,COUNT(DISTINCT b) AS reached,SUM(b.n) AS total \
            GROUP BY a ORDER BY a"), symbols,
    ).unwrap().bind_parameters(&arguments(floor)).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut data = WriteBatch::new(R);
    for (id, value, root) in [(1, 10, true), (2, 20, false), (3, 30, false),
        (4, 40, false), (5, 50, true), (9, 90, true)] {
        data.create_vertex(VId(id), if root { vec![ROOT] } else { vec![] },
            vec![(N, CanonicalScalar::Int(value))]);
    }
    for (id, source, target) in [(101, 1, 2), (102, 1, 2), (103, 2, 3),
        (104, 3, 1), (105, 2, 4)] {
        data.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    let mut links = WriteBatch::new(S);
    links.add_edge(EId(201), VId(5), VId(1), vec![]);
    links.add_edge(EId(202), VId(5), VId(9), vec![]);
    db.write_atomic(cx, vec![data, links]).await.unwrap()
}
fn changes() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.delete_edge(EId(102));
    batch.delete_edge(EId(105));
    batch.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(22)));
    batch.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(35)));
    batch.add_edge(EId(106), VId(3), VId(4), vec![]);
    batch.add_edge(EId(107), VId(9), VId(2), vec![]);
    batch
}
fn integer(row: &VertexRow) -> Option<i64> {
    row.props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(value) if *key == N => Some(*value),
        _ => None,
    })
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], floor: i64) -> Vec<Plain> {
    let mut output = Vec::new();
    for source in vertices.iter().filter(|row| row.labels.contains(&ROOT)) {
        let mut layer = BTreeMap::from([(source.vid, 1_usize)]);
        for depth in 0..=3 {
            for (&target, &count) in &layer {
                let row = vertices.iter().find(|row| row.vid == target).unwrap();
                if let Some(value) = integer(row).filter(|value| *value >= floor) {
                    for _ in 0..count { output.push((source.vid, target, value)); }
                }
            }
            if depth == 3 { break; }
            let mut next = BTreeMap::<VId, usize>::new();
            for (at, count) in layer {
                for edge in edges.iter().filter(|edge| edge.entry.relation == R && edge.entry.src == at) {
                    *next.entry(edge.entry.dst).or_default() += count;
                }
            }
            layer = next;
        }
    }
    output.sort();
    output
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        let CanonicalScalar::Int(value) = row.values()[2].as_scalar().unwrap() else {
            panic!("noninteger fixture endpoint")
        };
        (row.values()[0].as_vertex().unwrap(), row.values()[1].as_vertex().unwrap(), *value)
    }).collect()
}
fn optional(rows: &[GraphValueRow]) -> Vec<OptionalRow> {
    rows.iter().map(|row| {
        let value = match row.values()[3].as_scalar().unwrap() {
            CanonicalScalar::Int(value) => Some(*value),
            CanonicalScalar::Null => None,
            _ => panic!("unexpected fixture value"),
        };
        (row.values()[0].as_vertex().unwrap(), row.values()[1].as_vertex().unwrap(),
            row.values()[2].as_vertex(), value)
    }).collect()
}
fn oracle_summary(rows: &[Plain]) -> Vec<Summary> {
    let mut groups = BTreeMap::<VId, (u64, BTreeSet<VId>, i128)>::new();
    for &(source, target, value) in rows {
        let group = groups.entry(source).or_default();
        group.0 += 1;
        group.1.insert(target);
        group.2 += i128::from(value);
    }
    groups.into_iter().map(|(source, (count, reached, sum))| (source, count, reached.len() as u64, sum)).collect()
}
fn summaries(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),
        row.values()[0].as_count().unwrap(), row.values()[1].as_count().unwrap(),
        row.values()[2].as_integer().unwrap())).collect()
}

#[test]
fn walk_rows_and_aggregates_follow_staged_effects_but_pinned_history_survives_reopen() {
    let ((), report) = run_async_under_lab(0x7a1c_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let query = pattern(0, None); let aggregate = grouped(0);
        let frozen = query.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 0);
        assert_eq!(old.len(), 11);
        assert_eq!(oracle_summary(&old), vec![(VId(1), 9, 4, 210), (VId(5), 1, 1, 50), (VId(9), 1, 1, 90)]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [
            db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap(),
            pinned.execute_graph_pattern_governed(&cx, &query, policy()).unwrap(),
            pinned.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &query, policy()).unwrap(),
        ] { assert_eq!(plain(&result.value), old); }
        for result in [
            db.execute_graph_aggregate_governed(&cx, &aggregate, policy()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx, &aggregate, policy()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy()).unwrap(),
        ] { assert_eq!(summaries(&result.value), oracle_summary(&old)); }
        // Vertex 2 fails this endpoint predicate, yet walks through it still
        // reach 3 and 4. A rejected endpoint is not a pruned transit vertex.
        let filtered = pattern(25, None);
        let expected_filtered = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 25);
        assert_eq!(expected_filtered.len(), 6);
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &filtered, policy()).unwrap().value), expected_filtered);
        txn.write(&mut db, changes()).unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap(), 0);
        assert_ne!(expected, old);
        assert_eq!(oracle_summary(&expected), vec![(VId(1), 5, 4, 117), (VId(5), 1, 1, 50), (VId(9), 5, 5, 197)]);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &query, policy()).unwrap().value), expected);
        assert_eq!(summaries(&txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy()).unwrap().value), oracle_summary(&expected));
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), expected);
        assert_eq!(summaries(&reopened.execute_graph_aggregate_governed(&cx, &aggregate, policy()).unwrap().value), oracle_summary(&expected));
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap().value), old);
        assert_eq!(summaries(&reopened.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy()).unwrap().value), oracle_summary(&old));
        assert_eq!(plain(&pinned.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), old);
        assert_eq!(summaries(&pinned.execute_graph_aggregate_governed(&cx, &aggregate, policy()).unwrap().value), oracle_summary(&old));
        assert_eq!(query.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_fixed_optional_walks_admit_intermediate_properties_and_zero_hop_isolates() {
    let ((), report) = run_async_under_lab(0x7a1c_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let text = "MATCH (a:Root)-[:S]->(anchor) OPTIONAL MATCH WALK (anchor)-[:R*1..2]->(b) \
            WHERE b.n >= $floor RETURN a,anchor,b,b.n AS score ORDER BY anchor,b";
        let query = PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&arguments(25)).unwrap();
        assert!(!query.plan().scans_edges(), "both base tables must be admitted for the WALK child");
        let rows = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
        assert_eq!(rows.rows.snapshot_records, 13);
        assert_eq!(optional(&rows.value), vec![
            (VId(5), VId(1), Some(VId(3)), Some(30)), (VId(5), VId(1), Some(VId(3)), Some(30)),
            (VId(5), VId(1), Some(VId(4)), Some(40)), (VId(5), VId(1), Some(VId(4)), Some(40)),
            (VId(5), VId(9), None, None),
        ]);
        let zero = PreparedGraphText::prepare(
            "MATCH (a:Root)-[:S]->(anchor) OPTIONAL MATCH WALK (anchor)-[:R*0]->(b) \
             WHERE b.n >= $floor RETURN a,anchor,b,b.n AS score ORDER BY anchor,b", symbols,
        ).unwrap().bind_parameters(&arguments(25)).unwrap();
        assert_eq!(optional(&db.execute_graph_pattern_governed(&cx, &zero, policy()).unwrap().value), vec![
            (VId(5), VId(1), None, None), (VId(5), VId(9), Some(VId(9)), Some(90)),
        ]);
        let absent = PreparedGraphText::prepare(
            "MATCH (a:Root) WHERE NOT EXISTS { MATCH WALK (a)-[:R*1..2]->(b) \
             WHERE b.n >= $floor } RETURN a", symbols,
        ).unwrap().bind_parameters(&arguments(25)).unwrap();
        let absent = db.execute_graph_pattern_governed(&cx, &absent, policy()).unwrap();
        let ids: Vec<_> = absent.value.iter().map(|row| row.values()[0].as_vertex().unwrap()).collect();
        assert_eq!(ids, vec![VId(5), VId(9)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn walk_observations_survive_rejected_endpoints_refused_output_and_zero_pages() {
    let ((), report) = run_async_under_lab(0x7a1c_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 {
            for mutation in 0..4 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let query = pattern(25, Some(u64::from(mode != 2)));
                let output = txn.execute_graph_pattern_governed(&db, &cx, &query,
                    GqlQueryPolicy::new(1_000, u64::from(mode == 0), 5_000_000, 2_000_000));
                match mode {
                    0 => assert_eq!(output.unwrap().value.len(), 1),
                    1 => assert!(matches!(output, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(output.unwrap().value.is_empty()),
                }
                if mutation == 3 {
                    // No competing write: a completed/refused zero-output read
                    // must not poison an otherwise valid write transaction.
                    txn.commit(&mut db, &commit).await.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                    continue;
                }
                let mut winner = WriteBatch::new(R);
                match mutation {
                    0 => winner.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(26))),
                    1 => winner.add_edge(EId(300), VId(4), VId(9), vec![]),
                    _ => winner.create_vertex(VId(8), vec![ROOT], vec![(N, CanonicalScalar::Int(80))]),
                };
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // Commit immediately. A later read could otherwise repair a
                // missing transit, absent-edge or zero-hop-population witness.
                let result = txn.commit(&mut db, &commit).await;
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))), "mode={mode}, mutation={mutation}");
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn walk_queries_enforce_exact_limits_and_keep_authority_and_snapshot_error_precedence() {
    let ((), report) = run_async_under_lab(0x7a1c_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let query = pattern(0, Some(2));
        let measured = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
        assert_eq!(measured.rows.snapshot_records, 13);
        assert_eq!(measured.rows.result_rows, 2);
        let exact = GqlQueryPolicy::new(13, 2, measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&cx, &query, exact).unwrap(), measured);
        for refused in [
            GqlQueryPolicy::new(12, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(13, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(13, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(13, 2, u64::MAX, measured.evaluator.scratch_entries - 1),
        ] { assert!(db.execute_graph_pattern_governed(&cx, &query, refused).is_err()); }
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(txn.execute_graph_pattern_governed(&foreign, &cx, &query, zero),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))));
        assert!(matches!(db.execute_graph_pattern_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::BeyondFrontier { .. })))));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
