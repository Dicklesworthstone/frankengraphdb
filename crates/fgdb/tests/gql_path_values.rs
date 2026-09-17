//! Captured path identities survive the real snapshot and transaction adapters.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText, PreparedTemporalGraphText};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const KEY: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000) }
fn routes(rows: &[GraphValueRow]) -> Vec<Vec<EId>> {
    rows.iter().map(|row| row.values()[0].as_path().expect("typed path column").edges().collect()).collect()
}

#[test]
fn temporal_edge_delete_preserves_real_path_ids_and_governed_growth() {
    let ((), report) = run_async_under_lab(0x4_7474, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let keys = DatabaseKeys::new([0x71;32], DatabaseSecurityNamespaceId([0x72;32]), [0x73;32]);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=4 {
            batch.create_vertex(VId(id), vec![], vec![(KEY, CanonicalScalar::Int(id as i64))]);
        }
        for (eid, src, dst) in [(91,1,2), (92,2,4), (93,1,3), (94,3,4), (95,1,4)] {
            batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        let before = db.write(&commit, batch).await.unwrap();
        let template = PreparedTemporalGraphText::prepare(
            "MATCH p = ALL SHORTEST WALK (a)-[:R*1..4]->(b) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.key=1 AND b.key=4 RETURN p", symbols).unwrap();
        let at = |seq| template.bind_parameters(&GqlParameters::new().with_uint64("at", seq).unwrap()).unwrap();
        let old = db.execute_temporal_graph_text_governed(&query, &at(before.0), policy()).unwrap();
        assert_eq!(routes(&old.value), vec![vec![EId(95)]]);
        let mut deletion = WriteBatch::new(R);
        deletion.delete_edge(EId(95));
        let after = db.write(&commit, deletion).await.unwrap();
        let current = db.execute_temporal_graph_text_governed(&query, &at(after.0), policy()).unwrap();
        assert_eq!(routes(&current.value), vec![vec![EId(91), EId(92)], vec![EId(93), EId(94)]]);
        assert_eq!(routes(&db.execute_temporal_graph_text_governed(&query, &at(before.0), policy()).unwrap().value), vec![vec![EId(95)]]);
        let caps = [current.rows.snapshot_records, current.rows.result_rows, current.evaluator.work_units, current.evaluator.scratch_entries];
        for dimension in 0..4 {
            let mut limited = caps;
            limited[dimension] -= 1;
            assert!(db.execute_temporal_graph_text_governed(&query, &at(after.0), GqlQueryPolicy::new(limited[0],limited[1],limited[2],limited[3])).is_err(), "dimension {dimension}");
        }
        let pattern = PreparedGraphText::prepare("MATCH p = ANY SHORTEST WALK (a)-[:R*1..4]->(b) WHERE a.key=1 AND b.key=4 RETURN p", symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut shortcut = WriteBatch::new(R);
        shortcut.add_edge(EId(99), VId(1), VId(4), vec![]);
        txn.write(&mut db, shortcut).unwrap();
        assert_eq!(routes(&txn.execute_graph_pattern_governed(&mut db, &query, &pattern, policy()).unwrap().value), vec![vec![EId(99)]]);
        assert_eq!(routes(&db.execute_graph_pattern_governed(&query, &pattern, policy()).unwrap().value), vec![vec![EId(91), EId(92)]]);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
