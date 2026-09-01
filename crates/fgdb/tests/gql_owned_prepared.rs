use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const STATEMENT: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn owned_preparation_is_stable_across_database_view_and_transaction_surfaces() {
    let ((), report) = run_async_under_lab(0x35_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("fgdb-owned-prepared");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("database creates");

        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![]);
        initial.create_vertex(VId(2), vec![], vec![]);
        initial.add_edge(EId(10), VId(1), VId(2), vec![]);
        let basis = db.write(&commit, initial).await.expect("fixture commits");

        let mut statement = STATEMENT.to_owned();
        let mut bind = RelationBind::new().with_relation("R", R);
        let query = db
            .prepare_gql_query(&statement, &bind)
            .expect("query prepares once");
        assert!(query.verifies_definition());

        statement.push_str(" LIMIT 0");
        bind.insert("R", RelationId(99));

        let rows = db
            .execute_prepared_query(&query)
            .expect("database executes retained definition");
        assert_eq!(rows, vec![VId(2)]);

        let (evidence_rows, input, plan, result_digest) = db
            .execute_prepared_query_with_result_digest_at(&query, basis)
            .expect("database issues aligned evidence");
        assert_eq!(evidence_rows, rows);
        assert!(input.verifies_at(query.statement(), query.bind(), basis));
        assert!(plan.verifies_at(query.plan(), basis));
        assert!(plan.verifies_result_digest(&rows, result_digest));

        let view = db.read_session().expect("read view pins");
        let view_rows = view
            .execute_prepared_query(&query)
            .expect("read view reuses the definition");
        let (view_evidence_rows, view_input, view_plan, view_result_digest) = view
            .execute_prepared_query_with_result_digest(&query)
            .expect("read view issues aligned evidence");
        assert_eq!(view_rows, rows);
        assert_eq!(view_evidence_rows, rows);
        assert!(view_input.verifies_at(query.statement(), query.bind(), basis));
        assert!(view_plan.verifies_at(query.plan(), basis));
        assert!(view_plan.verifies_result_digest(&view_rows, view_result_digest));
        assert!(plan.verifies_result_digest(&view_rows, view_result_digest));

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let txn_query = txn
            .prepare_gql_query(query.statement(), query.bind())
            .expect("transaction preparation remains coherent");
        assert_eq!(txn_query, query);

        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(3), vec![], vec![]);
        staged.add_edge(EId(11), VId(1), VId(3), vec![]);
        txn.write(&mut db, staged).expect("overlay stages");

        let txn_rows = txn
            .execute_prepared_query(&db, &query)
            .expect("transaction sees staged overlay");
        let (txn_certified_rows, txn_plan) = txn
            .execute_prepared_query_certified(&db, &query)
            .expect("transaction plan-certifies");
        assert_eq!(txn_rows, vec![VId(2), VId(3)]);
        assert_eq!(txn_certified_rows, txn_rows);
        assert!(txn_plan.verifies_at(query.plan(), txn.basis()));
        assert_eq!(
            txn.prepared_query_plan_certificate(&query)
                .expect("plan-only certificate succeeds"),
            txn_plan
        );

        let debug = format!("{query:?}");
        assert!(!debug.contains(STATEMENT));
        assert!(!debug.contains("RelationId(1)"));
        txn.abort();
    });

    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
