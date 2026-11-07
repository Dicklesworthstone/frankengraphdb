//! Catalog CALLs use the native relational pipeline and its live authority.
//! Declared names are intentional: these procedures do not infer a durable
//! catalog or discover names by scanning graph records (fgdb-hne).

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    PreparedGraphSetText, ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc1; 32]);

#[derive(Clone, Copy)]
struct Catalog {
    hidden: bool,
}

impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        let catalog = self.reverse_catalog()?;
        match kind {
            GraphSymbolKind::Label => catalog
                .labels
                .into_iter()
                .find_map(|(id, value)| (value == name).then_some(GraphSymbol::Label(id))),
            GraphSymbolKind::Relation => catalog
                .relations
                .into_iter()
                .find_map(|(id, value)| (value == name).then_some(GraphSymbol::Relation(id))),
            GraphSymbolKind::Property => catalog
                .properties
                .into_iter()
                .find_map(|(id, value)| (value == name).then_some(GraphSymbol::Property(id))),
        }
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        // Identity order deliberately differs from name order; full-width
        // schema identities must survive preparation and scope checks.
        catalog.insert_label(LabelId(1), "Project");
        catalog.insert_label(LabelId(u64::MAX), "Person");
        catalog.insert_label(LabelId(3), "Company");
        catalog.insert_relation(RelationId(1), "WORKS_AT");
        catalog.insert_relation(RelationId(u64::MAX), "KNOWS");
        catalog.insert_property(PropertyKeyId(1), "title");
        catalog.insert_property(PropertyKeyId(u64::MAX), "name");
        if self.hidden {
            catalog.insert_label(LabelId(99), format!("AHidden{}", "x".repeat(4096)));
            catalog.insert_relation(RelationId(99), "HIDDEN_RELATION");
            catalog.insert_property(PropertyKeyId(99), "secret");
        }
        Some(catalog)
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc2; 32], NS, [0xc3; 32])
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, 100, 100_000, 100_000)
}

fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}

fn rows(result: QueryResult) -> (Vec<String>, Vec<Vec<GraphValue>>) {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("catalog CALL must return rows")
    };
    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    QueryValue::Count(value) => {
                        GraphValue::Scalar(CanonicalScalar::Int(i64::try_from(value).unwrap()))
                    }
                    other => panic!("unexpected catalog cell {other:?}"),
                })
                .collect()
        })
        .collect();
    (columns, rows)
}

const PROCEDURES: [(&str, &str, &[&str]); 3] = [
    ("labels", "label", &["Company", "Person", "Project"]),
    (
        "relationshipTypes",
        "relationshipType",
        &["KNOWS", "WORKS_AT"],
    ),
    ("propertyKeys", "propertyKey", &["name", "title"]),
];

#[test]
fn catalog_calls_compose_in_database_read_view_and_transaction_without_graph_scans() {
    let ((), report) = run_async_under_lab(0xca7a_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let view = db.read_session().unwrap();
        let txn = db.begin(&contexts.txn()).unwrap();
        let params = GqlParameters::new();
        for (name, column, expected) in PROCEDURES {
            let expected = (
                vec![column.to_owned()],
                expected.iter().map(|name| vec![text(name)]).collect(),
            );
            for statement in [
                format!("CALL db.{name}()"),
                format!("CALL db.{name}() YIELD {column}"),
                format!("CALL db.{name}() YIELD {column} RETURN {column}"),
            ] {
                let catalog = Catalog { hidden: false };
                assert_eq!(
                    rows(
                        db.query(&cx, &statement, &params, catalog, policy())
                            .unwrap()
                    ),
                    expected,
                    "{statement}",
                );
                assert_eq!(
                    rows(
                        view.query(&cx, &statement, &params, catalog, policy())
                            .unwrap()
                    ),
                    expected,
                );
                assert_eq!(
                    rows(
                        txn.query(&db, &cx, &statement, &params, catalog, policy())
                            .unwrap()
                    ),
                    expected,
                );
            }
        }
        let statement = "CALL db.labels() YIELD label WITH label \
                         WHERE label STARTS WITH 'P' RETURN collect(label) AS names";
        let expected = (
            vec!["names".to_owned()],
            vec![vec![GraphValue::List(
                vec![text("Person"), text("Project")].into(),
            )]],
        );
        // Three private catalog rows feed one delivered collection row.
        // Source admission must not consume the caller's final row allowance.
        let aggregate_policy = GqlQueryPolicy::new(0, 1, 100_000, 100_000);
        for actual in [
            db.query(
                &cx,
                statement,
                &params,
                Catalog { hidden: false },
                aggregate_policy,
            ),
            view.query(
                &cx,
                statement,
                &params,
                Catalog { hidden: false },
                aggregate_policy,
            ),
        ] {
            assert_eq!(rows(actual.unwrap()), expected);
        }
        assert_eq!(
            rows(
                txn.query(
                    &db,
                    &cx,
                    statement,
                    &params,
                    Catalog { hidden: false },
                    aggregate_policy
                )
                .unwrap()
            ),
            expected,
        );
        txn.abort();
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn catalog_work_and_payload_share_the_relational_budget_and_remain_cancellable() {
    let ((), report) = run_async_under_lab(0xca7a_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let params = GqlParameters::new();
        let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            "CALL db.labels() YIELD label RETURN label UNION ALL \
             CALL db.labels() YIELD label RETURN label",
            &[],
            &mut Catalog { hidden: false },
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        let result = db
            .execute_graph_set_governed(&cx, &query, policy())
            .unwrap();
        assert_eq!(result.rows.snapshot_records, 0);
        assert_eq!(result.rows.result_rows, 6);
        let exact = GqlQueryPolicy::new(
            0,
            6,
            result.evaluator.work_units,
            result.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_set_governed(&cx, &query, exact).unwrap(),
            result
        );
        for work in [false, true] {
            let mut short = exact;
            if work {
                short.evaluator.max_work_units -= 1;
            } else {
                short.evaluator.max_scratch_entries -= 1;
            }
            assert!(db.execute_graph_set_governed(&cx, &query, short).is_err());
        }
        use fgdb_types::context::SimulationCheckpointProbe;
        use std::sync::Arc;
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        db.execute_graph_set_governed(&cx.with_checkpoint_probe(probe.clone()), &query, exact)
            .unwrap();
        assert!(probe.calls() > 6);
        for stop in 1..=probe.calls() {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            assert!(matches!(
                db.execute_graph_set_governed(&cx.with_checkpoint_probe(probe), &query, exact),
                Err(fgdb_gql::GqlQueryError::Interrupted(_)),
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_catalog_names_and_budget_refusals_equal_a_catalog_without_hidden_entries() {
    let ((), report) = run_async_under_lab(0xca7a_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(0xc171), NS, "graph", SchemaEpoch(0), 1).unwrap();
        let mut grant = Grant::read_only(
            "main",
            1_000,
            QueryLimits {
                max_nodes: 0,
                max_work: 100_000,
                max_rows: 100,
            },
        );
        grant.labels = Scope::only([LabelId(1), LabelId(3), LabelId(u64::MAX)]);
        grant.relations = Scope::only([RelationId(1), RelationId(u64::MAX)]);
        grant.properties = Scope::only([PropertyKeyId(1), PropertyKeyId(u64::MAX)]);
        let params = GqlParameters::new();
        for (name, column, expected) in PROCEDURES {
            let statement = format!("CALL db.{name}() YIELD {column} RETURN {column}");
            let token = authority.issue_at(&grant, 100).unwrap();
            assert_eq!(
                rows(
                    db.query_authorized(
                        &cx,
                        &authority,
                        &token,
                        "main",
                        &statement,
                        &params,
                        Catalog { hidden: true },
                        policy(),
                        || 100
                    )
                    .unwrap()
                ),
                (
                    vec![column.to_owned()],
                    expected.iter().map(|name| vec![text(name)]).collect()
                ),
            );
            for budget in 0..80 {
                let mut limited = grant.clone();
                limited.limits.max_work = budget;
                let token = authority.issue_at(&limited, 100).unwrap();
                let execute = |hidden| {
                    db.query_authorized(
                        &cx,
                        &authority,
                        &token,
                        "main",
                        &statement,
                        &params,
                        Catalog { hidden },
                        policy(),
                        || 100,
                    )
                    .map(rows)
                    .map_err(|error| error.to_string())
                };
                assert_eq!(
                    execute(true),
                    execute(false),
                    "{statement}, signed work {budget}"
                );
            }
            let mut one_row = grant.clone();
            one_row.limits.max_rows = 1;
            let token = authority.issue_at(&one_row, 100).unwrap();
            let aggregate =
                format!("CALL db.{name}() YIELD {column} RETURN collect({column}) AS names");
            assert_eq!(
                rows(
                    db.query_authorized(
                        &cx,
                        &authority,
                        &token,
                        "main",
                        &aggregate,
                        &params,
                        Catalog { hidden: true },
                        GqlQueryPolicy::new(0, 1, 100_000, 100_000),
                        || 100,
                    )
                    .unwrap()
                ),
                (
                    vec!["names".to_owned()],
                    vec![vec![GraphValue::List(
                        expected
                            .iter()
                            .map(|name| text(name))
                            .collect::<Vec<_>>()
                            .into(),
                    )]]
                ),
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
