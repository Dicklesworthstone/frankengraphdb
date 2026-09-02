use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::{BudgetedGqlError, GqlBudgetDimension, GqlExecutionBudget};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(2);
const STATEMENT: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

fn test_directory() -> std::io::Result<std::path::PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    loop {
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = base.join(format!("fgdb-owned-prepared-{ordinal}"));
        match std::fs::create_dir(&parent) {
            Ok(()) => return Ok(parent.join("database")),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn owned_preparation_is_stable_across_database_view_and_transaction_surfaces() {
    let ((), report) = run_async_under_lab(0x35_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = test_directory().expect("a unique retained test directory is available");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("database creates");

        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![PERSON], vec![]);
        initial.create_vertex(VId(2), vec![PERSON], vec![]);
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

        let bounded = db
            .execute_prepared_query_budgeted_at(&query, basis, GqlExecutionBudget::new(1, 1))
            .expect("exact deterministic bounds succeed");
        assert_eq!(bounded.value, rows);
        assert_eq!(bounded.stats.snapshot_records, 1);
        assert_eq!(bounded.stats.result_rows, 1);

        let snapshot_error = db
            .execute_prepared_query_budgeted_at(
                &query,
                basis,
                GqlExecutionBudget::snapshot_records(0),
            )
            .expect_err("one admitted edge over the bound refuses");
        assert!(matches!(
            snapshot_error,
            BudgetedGqlError::Budget(exceeded)
                if exceeded.dimension == GqlBudgetDimension::SnapshotRecords
                    && exceeded.limit == 0
                    && exceeded.observed == 1
        ));

        let result_error = db
            .execute_prepared_query_budgeted_at(&query, basis, GqlExecutionBudget::result_rows(0))
            .expect_err("one final row over the bound refuses");
        assert!(matches!(
            result_error,
            BudgetedGqlError::Budget(exceeded)
                if exceeded.dimension == GqlBudgetDimension::ResultRows
                    && exceeded.limit == 0
                    && exceeded.observed == 1
        ));

        let node_query = db
            .prepare_gql_query(
                "MATCH (n:Person) RETURN n",
                &RelationBind::new().with_label("Person", PERSON),
            )
            .expect("labeled node scan prepares");
        let node_bounded = db
            .execute_prepared_query_budgeted_at(&node_query, basis, GqlExecutionBudget::new(2, 2))
            .expect("node scan counts the admitted vertex table");
        assert_eq!(node_bounded.value, vec![VId(1), VId(2)]);
        assert_eq!(node_bounded.stats.snapshot_records, 2);
        assert_eq!(node_bounded.stats.result_rows, 2);

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
        let view_bounded = view
            .execute_prepared_query_budgeted(&query, GqlExecutionBudget::new(1, 1))
            .expect("read view reports identical deterministic counters");
        assert_eq!(view_bounded.value, rows);
        assert_eq!(view_bounded.stats, bounded.stats);
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
        let txn_bounded = txn
            .execute_prepared_query_budgeted(&db, &query, GqlExecutionBudget::new(2, 2))
            .expect("transaction exact bounds succeed");
        assert_eq!(txn_bounded.value, txn_rows);
        assert_eq!(txn_bounded.stats.snapshot_records, 2);
        assert_eq!(txn_bounded.stats.result_rows, 2);

        let txn_budget_error = txn
            .execute_prepared_query_budgeted(&db, &query, GqlExecutionBudget::new(1, 2))
            .expect_err("transaction snapshot admission is fail-closed");
        assert!(matches!(
            txn_budget_error,
            BudgetedGqlError::Budget(exceeded)
                if exceeded.dimension == GqlBudgetDimension::SnapshotRecords
                    && exceeded.limit == 1
                    && exceeded.observed == 2
        ));
        let (txn_certified_rows, txn_plan) = txn
            .execute_prepared_query_certified(&db, &query)
            .expect("transaction plan-certifies");
        assert_eq!(txn_rows, vec![VId(2), VId(3)]);
        assert_eq!(txn_certified_rows, txn_rows);
        assert!(txn_plan.verifies_at(query.plan(), txn.basis()));
        let plan_only = txn
            .prepared_query_plan_certificate(&query)
            .expect("plan-only certificate succeeds");
        assert!(plan_only.verifies_at(query.plan(), txn.basis()));

        let debug = format!("{query:?}");
        assert!(!debug.contains(STATEMENT));
        assert!(!debug.contains("RelationId(1)"));
        txn.abort();
    });

    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
