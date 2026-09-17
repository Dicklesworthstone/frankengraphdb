//! Shared ALL-shortest layers must reach the real governed query/transaction
//! path, not just a standalone cursor. A tiny durable graph represents 8^1024
//! matching walks: EXISTS needs one witness, not a materialized occurrence bag.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const ROOT: LabelId = LabelId(1);
const R: RelationId = RelationId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}

fn policy() -> GqlQueryPolicy {
    // Source rows are tiny. This allowance fits shared depth/vertex state but
    // cannot admit an occurrence frontier for 8^1024 paths.
    GqlQueryPolicy::new(10_000, 10_000, 100_000, 10_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Root") => Some(GraphSymbol::Label(ROOT)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}

fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn probe(anti: bool) -> PreparedGraphPattern<GraphValueRow> {
    let negate = if anti { "NOT " } else { "" };
    prepare(&format!(
        "MATCH (a:Root) WHERE {negate}EXISTS {{ \
         MATCH ALL SHORTEST WALK (a)-[:R*1024]->(b) }} RETURN a"
    ))
}

fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
        .collect()
}

async fn seed(db: &mut Database<MemVfs>, commit: &CommitCx, edges: u128) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(7), vec![ROOT], vec![]);
    batch.create_vertex(VId(8), vec![ROOT], vec![]);
    for id in 1..=edges {
        batch.add_edge(EId(id), VId(7), VId(7), vec![]);
    }
    db.write(commit, batch).await.unwrap()
}

#[test]
fn exponential_shortest_witnesses_use_pinned_historical_and_staged_topology() {
    let ((), report) = run_async_under_lab(0xa115_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit, 8).await;
        let pinned = db.read_session().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let exists = probe(false);
        let absent = probe(true);
        let frozen = exists.canonical_bytes();
        for (plan, expected) in [(&exists, vec![VId(7)]), (&absent, vec![VId(8)])] {
            for result in [
                db.execute_graph_pattern_governed(&query, plan, policy()).unwrap(),
                db.execute_graph_pattern_governed_at(&query, plan, basis, policy()).unwrap(),
                pinned.execute_graph_pattern_governed(&query, plan, policy()).unwrap(),
                pinned
                    .execute_graph_pattern_governed_at(&query, plan, basis, policy())
                    .unwrap(),
                transaction
                    .execute_graph_pattern_governed(&db, &query, plan, policy())
                    .unwrap(),
            ] {
                assert_eq!(ids(&result.value), expected);
                assert!(result.evaluator.scratch_entries <= 10_000);
                assert!(result.evaluator.work_units <= 100_000);
            }
        }
        let mut deletion = WriteBatch::new(R);
        for id in 1..=8 {
            deletion.delete_edge(EId(id));
        }
        transaction.write(&mut db, deletion).unwrap();
        for (plan, expected) in [(&exists, vec![]), (&absent, vec![VId(7), VId(8)])] {
            assert_eq!(
                ids(&transaction
                    .execute_graph_pattern_governed(&db, &query, plan, policy())
                    .unwrap()
                    .value),
                expected
            );
        }
        assert_eq!(
            ids(&db.execute_graph_pattern_governed(&query, &exists, policy()).unwrap().value),
            vec![VId(7)],
            "staged deletion escaped its overlay"
        );
        assert_eq!(db.frontier().unwrap(), basis);
        transaction.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (plan, old, current) in [
            (&exists, vec![VId(7)], vec![]),
            (&absent, vec![VId(8)], vec![VId(7), VId(8)]),
        ] {
            assert_eq!(
                ids(&reopened
                    .execute_graph_pattern_governed(&query, plan, policy())
                    .unwrap()
                    .value),
                current
            );
            assert_eq!(
                ids(&reopened
                    .execute_graph_pattern_governed_at(&query, plan, basis, policy())
                    .unwrap()
                    .value),
                old
            );
            assert_eq!(
                ids(&pinned.execute_graph_pattern_governed(&query, plan, policy()).unwrap().value),
                old
            );
        }
        assert_eq!(exists.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn shared_layers_preserve_all_occurrences_in_the_public_value_row_pipeline() {
    let ((), report) = run_async_under_lab(0xa115_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, 2).await;
        for (minimum, maximum, expected) in [(0, 6, 1), (1, 6, 2), (6, 6, 64)] {
            let plan = prepare(&format!(
                "MATCH ALL SHORTEST WALK (a)-[:R*{minimum}..{maximum}]->(b) \
                 WHERE a<>b RETURN ALL b"
            ));
            // No result: endpoint filtering must not invent a longer shortest
            // path, and the private traversal never changes the bound source.
            assert!(db
                .execute_graph_pattern_governed(&query, &plan, policy())
                .unwrap()
                .value
                .is_empty());
            let plan = prepare(&format!(
                "MATCH ALL SHORTEST WALK (a)-[:R*{minimum}..{maximum}]->(b) RETURN ALL b"
            ));
            let result = db.execute_graph_pattern_governed(&query, &plan, policy()).unwrap();
            let mut expected_rows = vec![VId(7); expected];
            if minimum == 0 {
                // The isolated vertex has its own zero-hop shortest walk.
                expected_rows.push(VId(8));
            }
            assert_eq!(ids(&result.value), expected_rows);
            assert_eq!(result.rows.result_rows as usize, expected_rows.len());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn shortest_refusals_are_not_optional_nulls_or_negative_existence_proofs() {
    let ((), report) = run_async_under_lab(0xa115_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, 8).await;
        let optional = prepare(
            "MATCH (a:Root) OPTIONAL MATCH ALL SHORTEST WALK (a)-[:R*1024]->(b) RETURN a,b",
        );
        // OPTIONAL needs all witnesses. A successful existential prefix is not
        // permission to coalesce the bag or emit a fallback null on refusal.
        assert!(matches!(
            db.execute_graph_pattern_governed(&query, &optional, policy()),
            Err(GqlQueryError::Evaluator(_))
        ));
        for anti in [false, true] {
            assert!(matches!(
                db.execute_graph_pattern_governed(
                    &query,
                    &probe(anti),
                    GqlQueryPolicy::new(10_000, 10_000, 100_000, 100),
                ),
                Err(GqlQueryError::Evaluator(_))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
