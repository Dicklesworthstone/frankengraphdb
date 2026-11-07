//! Catalog discovery must use the native query pipeline and the same Warden
//! boundary as graph reads. A catalog is supplied explicitly by the trusted
//! host; unused declarations are discoverable, hidden declarations are not.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, QueryResult, QueryValue,
    WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GlaLimitDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, PreparedGraphSetText, ReverseSymbolCatalog,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Scope};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xca; 32]);
const CALLS: [(&str, &str, &str); 3] = [
    ("CALL db.labels()", "label", "Alpha"),
    ("CALL db.relationshipTypes()", "relationshipType", "A"),
    ("CALL db.propertyKeys()", "propertyKey", "alpha"),
];

#[derive(Clone)]
struct Catalog(ReverseSymbolCatalog);

impl Catalog {
    fn sample() -> Self {
        let mut names = ReverseSymbolCatalog::new();
        // ID order deliberately differs from the required name order.
        names.insert_label(LabelId(1), "Zulu");
        names.insert_label(LabelId(2), "Alpha");
        names.insert_label(LabelId(3), "Unused");
        names.insert_relation(RelationId(1), "Z");
        names.insert_relation(RelationId(2), "A");
        names.insert_relation(RelationId(3), "UNUSED_R");
        names.insert_property(PropertyKeyId(1), "zeta");
        names.insert_property(PropertyKeyId(2), "alpha");
        names.insert_property(PropertyKeyId(3), "unused");
        Self(names)
    }

    fn with_hidden(mut self, count: u64) -> Self {
        for id in 100..100 + count {
            self.0.insert_label(LabelId(id), format!("HiddenLabel{id}"));
            self.0
                .insert_relation(RelationId(id), format!("HIDDEN_RELATION_{id}"));
            self.0
                .insert_property(PropertyKeyId(id), format!("hidden_property_{id}"));
        }
        self
    }
}

impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match kind {
            GraphSymbolKind::Label => self
                .0
                .labels
                .iter()
                .find_map(|(id, value)| (value == name).then_some(GraphSymbol::Label(*id))),
            GraphSymbolKind::Relation => self
                .0
                .relations
                .iter()
                .find_map(|(id, value)| (value == name).then_some(GraphSymbol::Relation(*id))),
            GraphSymbolKind::Property => self
                .0
                .properties
                .iter()
                .find_map(|(id, value)| (value == name).then_some(GraphSymbol::Property(*id))),
        }
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        Some(self.0.clone())
    }
}

struct ObservedCatalog<'a> {
    current: &'a RefCell<Catalog>,
    reverse_calls: &'a Cell<usize>,
}

impl GraphSymbolResolver for ObservedCatalog<'_> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.current.borrow_mut().resolve_symbol(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        self.reverse_calls.set(self.reverse_calls.get() + 1);
        self.current.borrow().reverse_catalog()
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}

fn authority(seed: u64) -> Authority {
    Authority::new(AuthKey::from_seed(seed), NS, "graph", SchemaEpoch(0), 1).unwrap()
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::only([LabelId(2)]);
    grant.relations = Scope::only([RelationId(2)]);
    grant.properties = Scope::only([PropertyKeyId(2)]);
    grant
}

async fn empty(cx: &CommitCx) -> Database<MemVfs> {
    Database::open_memory(cx, DatabaseKeys::new([0xcb; 32], NS, [0xcc; 32]))
        .await
        .unwrap()
}

fn text(value: &str) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(
        CanonicalScalar::ucs_basic_text(value).unwrap(),
    ))
}

fn int(value: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))
}

fn rows(columns: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).to_owned()).collect(),
        rows,
    }
}

fn names(column: &str, names: &[&str]) -> QueryResult {
    rows(
        &[column],
        names.iter().map(|name| vec![text(name)]).collect(),
    )
}

#[test]
fn all_catalog_procedures_discover_unused_names_in_lexical_order_without_graph_records() {
    let ((), report) = run_async_under_lab(0xca01, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let params = GqlParameters::new();
        let no_graph_records = GqlQueryPolicy::new(0, 16, 100_000, 100_000);
        let before = db.frontier().unwrap();
        for ((call, column, _), expected) in CALLS.into_iter().zip([
            vec!["Alpha", "Unused", "Zulu"],
            vec!["A", "UNUSED_R", "Z"],
            vec!["alpha", "unused", "zeta"],
        ]) {
            assert_eq!(
                db.query(
                    &c.query(),
                    call,
                    &params,
                    Catalog::sample(),
                    no_graph_records
                )
                .unwrap(),
                names(column, &expected),
                "{call}",
            );
            let yielded = format!("{call} YIELD {column} AS name RETURN name ORDER BY name DESC");
            let mut descending = expected;
            descending.reverse();
            assert_eq!(
                db.query(
                    &c.query(),
                    &yielded,
                    &params,
                    Catalog::sample(),
                    no_graph_records
                )
                .unwrap(),
                names("name", &descending),
            );
        }
        assert_eq!(db.frontier().unwrap(), before, "discovery is read-only");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn catalog_yields_compose_with_filters_aggregates_union_and_graph_match() {
    let ((), report) = run_async_under_lab(0xca02, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = empty(&c.commit()).await;
        let mut batch = WriteBatch::new(RelationId(2));
        batch.create_vertex(
            VId(1),
            vec![LabelId(2)],
            vec![(PropertyKeyId(2), CanonicalScalar::Int(7))],
        );
        db.write(&c.commit(), batch).await.unwrap();
        let params = GqlParameters::new();
        let cases = [
            (
                "CALL db.labels() YIELD label AS name WHERE name <> 'Unused' RETURN name ORDER BY name",
                names("name", &["Alpha", "Zulu"]),
            ),
            (
                "CALL db.labels() YIELD label AS first, label AS second WHERE first = 'Alpha' RETURN first, second",
                rows(
                    &["first", "second"],
                    vec![vec![text("Alpha"), text("Alpha")]],
                ),
            ),
            (
                "CALL db.propertyKeys() YIELD propertyKey RETURN count(*) AS count",
                rows(&["count"], vec![vec![QueryValue::Count(3)]]),
            ),
            (
                "CALL db.labels() YIELD label AS name RETURN name UNION ALL CALL db.relationshipTypes() YIELD relationshipType AS name RETURN name",
                names("name", &["A", "Alpha", "UNUSED_R", "Unused", "Z", "Zulu"]),
            ),
            (
                "CALL db.labels() YIELD label WITH label WHERE label = 'Alpha' MATCH (n:Alpha) RETURN label, n.alpha AS value",
                rows(&["label", "value"], vec![vec![text("Alpha"), int(7)]]),
            ),
        ];
        for (statement, expected) in cases {
            assert_eq!(
                db.query(&c.query(), statement, &params, Catalog::sample(), policy())
                    .unwrap(),
                expected,
                "{statement}",
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn catalog_intermediates_preserve_final_row_limits_and_share_cumulative_execution_budgets() {
    let ((), report) = run_async_under_lab(0xca0b, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = empty(&c.commit()).await;
        let params = GqlParameters::new();
        let statement = "CALL db.labels() YIELD label RETURN count(*) AS count";
        let one_row = GqlQueryPolicy::new(0, 1, 100_000, 100_000);
        let expected = rows(&["count"], vec![vec![QueryValue::Count(3)]]);
        let view = db.pinned_read_view().unwrap();
        let txcx = c.txn();
        let transaction = db.begin(&txcx).unwrap();
        // The three private catalog rows are inputs to a single result row,
        // not three deliveries against the caller's one-row allowance.
        for actual in [
            db.query(&c.query(), statement, &params, Catalog::sample(), one_row),
            view.query(&c.query(), statement, &params, Catalog::sample(), one_row),
            transaction.query(
                &db,
                &c.query(),
                statement,
                &params,
                Catalog::sample(),
                one_row,
            ),
        ] {
            assert_eq!(actual.unwrap(), expected);
        }
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
        let issuer = authority(0xca0b);
        let mut all = grant();
        all.labels = Scope::All;
        all.limits.max_nodes = 0;
        all.limits.max_rows = 1;
        let token = issuer.issue_at(&all, 100).unwrap();
        assert_eq!(
            db.query_authorized(
                &c.query(),
                &issuer,
                &token,
                "main",
                statement,
                &params,
                Catalog::sample(),
                one_row,
                || 100,
            )
            .unwrap(),
            expected,
        );

        let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            "CALL db.labels() YIELD label CALL db.relationshipTypes() YIELD relationshipType RETURN label, relationshipType",
            &[], &mut Catalog::sample(),
        ).unwrap().bind_parameters(&params).unwrap();
        let execution = db
            .execute_graph_set_governed(&c.query(), &query, policy())
            .unwrap();
        assert_eq!(execution.rows.snapshot_records, 0);
        assert_eq!(execution.rows.result_rows, 9);
        assert_eq!(execution.value.len(), 9);
        // Each source and the relational operators share one allowance. A
        // fresh allowance for the second CALL would admit an incomplete bill.
        let exact = GqlQueryPolicy::new(
            0,
            9,
            execution.evaluator.work_units,
            execution.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_set_governed(&c.query(), &query, exact)
                .unwrap(),
            execution
        );
        for dimension in [
            GlaLimitDimension::WorkUnits,
            GlaLimitDimension::ScratchEntries,
        ] {
            let mut short = exact;
            match dimension {
                GlaLimitDimension::WorkUnits => short.evaluator.max_work_units -= 1,
                GlaLimitDimension::ScratchEntries => short.evaluator.max_scratch_entries -= 1,
            }
            let error = db
                .execute_graph_set_governed(&c.query(), &query, short)
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Evaluator(limit) if limit.dimension == dimension),
                "{error:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn forward_only_resolvers_refuse_discovery_instead_of_guessing_common_names() {
    let ((), report) = run_async_under_lab(0xca03, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let params = GqlParameters::new();
        for (call, column, _) in CALLS {
            for statement in [
                call.to_owned(),
                format!("{call} YIELD {column} RETURN {column}"),
                format!("{call} YIELD {column} RETURN count(*) AS count"),
            ] {
                let error = db
                    .query(
                        &c.query(),
                        &statement,
                        &params,
                        |kind, name: &str| Catalog::sample().resolve_symbol(kind, name),
                        policy(),
                    )
                    .expect_err("incomplete discovery must not publish partial or empty success");
                assert!(error.to_string().contains("catalog"), "{error:?}");
            }
        }
        for (call, column, _) in CALLS {
            assert_eq!(
                db.query(
                    &c.query(),
                    call,
                    &params,
                    Catalog(ReverseSymbolCatalog::new()),
                    policy()
                )
                .unwrap(),
                names(column, &[]),
                "an explicitly empty catalog is complete",
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn catalog_binding_refusals_preserve_host_constraints_through_composition() {
    let ((), report) = run_async_under_lab(0xca0c, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let params = GqlParameters::new();
        let before = db.frontier().unwrap();
        let oversized = "x".repeat(fgdb_types::MAX_CANONICAL_TEXT_BYTES + 1);
        for ((call, column, _), kind) in CALLS.into_iter().zip([
            GraphSymbolKind::Label,
            GraphSymbolKind::Relation,
            GraphSymbolKind::Property,
        ]) {
            for (entries, constraint) in [
                (vec![(1, "same"), (2, "same")], "distinct catalog names"),
                (vec![(1, oversized.as_str())], "canonical text bound"),
            ] {
                let mut catalog = ReverseSymbolCatalog::new();
                for (id, name) in entries {
                    match kind {
                        GraphSymbolKind::Label => catalog.insert_label(LabelId(id), name),
                        GraphSymbolKind::Relation => catalog.insert_relation(RelationId(id), name),
                        GraphSymbolKind::Property => {
                            catalog.insert_property(PropertyKeyId(id), name)
                        }
                    }
                }
                for statement in [
                    call.to_owned(),
                    format!("{call} YIELD {column} RETURN {column}"),
                    format!("{call} YIELD {column} RETURN count(*) AS count"),
                ] {
                    let error = db
                        .query(
                            &c.query(),
                            &statement,
                            &params,
                            Catalog(catalog.clone()),
                            policy(),
                        )
                        .expect_err("an invalid catalog must retain its binding refusal");
                    assert!(
                        error.to_string().contains(constraint),
                        "{statement}: {error:?}",
                    );
                }
            }
        }
        assert_eq!(db.frontier().unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_discovery_masks_every_symbol_domain_before_filtering_and_aggregation() {
    let ((), report) = run_async_under_lab(0xca04, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let issuer = authority(0xca04);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                Catalog::sample().with_hidden(64),
                policy(),
                || 100,
            )
            .unwrap();
        let params = GqlParameters::new();
        for (call, column, visible) in CALLS {
            assert_eq!(
                session.query(&c.query(), call, &params).unwrap(),
                names(column, &[visible])
            );
            let count = format!("{call} YIELD {column} RETURN count(*) AS count");
            assert_eq!(
                session.query(&c.query(), &count, &params).unwrap(),
                rows(&["count"], vec![vec![QueryValue::Count(1)]]),
            );
            let hidden = format!(
                "{call} YIELD {column} AS name WHERE name STARTS WITH 'Hidden' RETURN name"
            );
            assert_eq!(
                session.query(&c.query(), &hidden, &params).unwrap(),
                names("name", &[])
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_catalog_extensions_do_not_change_work_or_row_budget_refusals() {
    let ((), report) = run_async_under_lab(0xca05, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let issuer = authority(0xca05);
        let params = GqlParameters::new();
        let original = Catalog::sample();
        let extended = original.clone().with_hidden(128);
        for (call, column, visible) in CALLS {
            let mut passed = false;
            let mut refused = false;
            for allowance in [0, 1, 2, 4, 8, 16, 32, 64, 128, 512] {
                let mut limited = grant();
                limited.limits = QueryLimits {
                    max_nodes: 0,
                    max_work: allowance,
                    max_rows: 1,
                };
                let token = issuer.issue_at(&limited, 100).unwrap();
                let query = |catalog| {
                    db.query_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        call,
                        &params,
                        catalog,
                        GqlQueryPolicy::new(0, 1, 100_000, 100_000),
                        || 100,
                    )
                    .map_err(|error| error.to_string())
                };
                let expected = query(original.clone());
                assert_eq!(
                    query(extended.clone()),
                    expected,
                    "{call}, signed work {allowance}"
                );
                if let Ok(answer) = expected {
                    passed = true;
                    assert_eq!(answer, names(column, &[visible]));
                } else {
                    refused = true;
                }
            }
            assert!(
                passed && refused,
                "the work matrix must cross the actual admission boundary"
            );

            let token = issuer.issue_at(&grant(), 100).unwrap();
            for (dimension, work, scratch) in [
                ("native work", 0, 100_000),
                ("native work", 4, 100_000),
                ("native work", 32, 100_000),
                ("scratch", 100_000, 0),
                ("scratch", 100_000, 1),
                ("scratch", 100_000, 4),
                ("scratch", 100_000, 32),
            ] {
                let query = |catalog| {
                    db.query_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        call,
                        &params,
                        catalog,
                        GqlQueryPolicy::new(0, 1, work, scratch),
                        || 100,
                    )
                    .map_err(|error| error.to_string())
                };
                assert_eq!(
                    query(extended.clone()),
                    query(original.clone()),
                    "{call}, {dimension}, work {work}, scratch {scratch}",
                );
            }
        }

        let mut none = grant();
        none.labels = Scope::only([]);
        none.relations = Scope::only([]);
        none.properties = Scope::only([]);
        none.limits = QueryLimits {
            max_nodes: 0,
            max_work: 100_000,
            max_rows: 0,
        };
        let token = issuer.issue_at(&none, 100).unwrap();
        for (call, column, _) in CALLS {
            for catalog in [Catalog(ReverseSymbolCatalog::new()), extended.clone()] {
                assert_eq!(
                    db.query_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        call,
                        &params,
                        catalog,
                        GqlQueryPolicy::new(0, 0, 100_000, 100_000),
                        || 100,
                    )
                    .unwrap(),
                    names(column, &[]),
                    "an entirely hidden catalog cannot exhaust a zero-row allowance",
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn visible_catalog_text_payload_reserves_scratch_while_hidden_payload_does_not() {
    let ((), report) = run_async_under_lab(0xca0a, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let issuer = authority(0xca0a);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let before = db.frontier().unwrap();
        let mut short = ReverseSymbolCatalog::new();
        short.insert_label(LabelId(2), "Alpha");
        let long_name = "x".repeat(64 * 1024);
        let mut hidden = short.clone();
        hidden.insert_label(LabelId(99), long_name.clone());
        let mut visible = short.clone();
        visible.insert_label(LabelId(2), long_name);
        // Sixty-four entries comfortably admit the short result and all
        // surrounding operators; one 64 KiB text payload must exceed it even
        // though work and row allowances are otherwise plentiful.
        let query = |catalog| {
            db.query_authorized(
                &c.query(),
                &issuer,
                &token,
                "main",
                "CALL db.labels()",
                &params,
                Catalog(catalog),
                GqlQueryPolicy::new(0, 16, 1_000_000, 64),
                || 100,
            )
        };
        let expected = names("label", &["Alpha"]);
        assert_eq!(query(short.clone()).unwrap(), expected);
        assert_eq!(
            query(hidden).unwrap(),
            expected,
            "masked payloads spend no scratch"
        );
        let error =
            query(visible).expect_err("the visible text payload must be admitted before copying");
        let QueryError::Set(GqlQueryError::Evaluator(limit)) = error else {
            panic!("expected a native scratch-budget refusal, got {error:?}");
        };
        assert_eq!(limit.dimension, GlaLimitDimension::ScratchEntries);
        assert!(limit.limit <= 64);
        assert_eq!(limit.observed, u128::from(limit.limit) + 1);
        assert_eq!(
            query(short).unwrap(),
            expected,
            "a budget refusal leaves the reader usable"
        );
        assert_eq!(db.frontier().unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_interrupts_a_hidden_catalog_walk_without_publishing_partial_results() {
    let ((), report) = run_async_under_lab(0xca09, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let issuer = authority(0xca09);
        let mut hidden = grant();
        hidden.labels = Scope::only([]);
        hidden.limits = QueryLimits {
            max_nodes: 0,
            max_work: 100_000,
            max_rows: 0,
        };
        let token = issuer.issue_at(&hidden, 100).unwrap();
        let params = GqlParameters::new();
        let before = db.frontier().unwrap();
        let query = |catalog, stop| {
            let probe = Arc::new(SimulationCheckpointProbe::new(stop));
            let observed = c.query().with_checkpoint_probe(Arc::clone(&probe));
            let result = db.query_authorized(
                &observed,
                &issuer,
                &token,
                "main",
                "CALL db.labels()",
                &params,
                catalog,
                GqlQueryPolicy::new(0, 0, 100_000, 100_000),
                || 100,
            );
            (result, probe.calls())
        };
        let (empty_result, empty_calls) = query(Catalog(ReverseSymbolCatalog::new()), None);
        assert_eq!(empty_result.unwrap(), names("label", &[]));
        let catalog = Catalog::sample().with_hidden(256);
        let (complete, full_calls) = query(catalog.clone(), None);
        assert_eq!(complete.unwrap(), names("label", &[]));
        assert!(
            full_calls >= empty_calls + 256,
            "masked names must still be cancellable"
        );
        let stop = empty_calls + 128;
        let (cancelled, observed) = query(catalog.clone(), Some(stop));
        assert!(
            cancelled.is_err(),
            "a cancelled discovery must not return empty success"
        );
        assert!(observed >= stop && observed < full_calls);
        assert_eq!(query(catalog, None).0.unwrap(), names("label", &[]));
        assert_eq!(db.frontier().unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_discovery_freezes_names_but_rebinds_values_and_rechecks_authority() {
    let ((), report) = run_async_under_lab(0xca06, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let issuer = authority(0xca06);
        let mut all = grant();
        all.labels = Scope::All;
        let token = issuer.issue_at(&all, 100).unwrap();
        let current = RefCell::new(Catalog::sample());
        let reverse_calls = Cell::new(0);
        let catalog = ObservedCatalog {
            current: &current,
            reverse_calls: &reverse_calls,
        };
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                policy(),
                || 100,
            )
            .unwrap();
        let first = GqlParameters::new().with_text("name", "Alpha").unwrap();
        let second = GqlParameters::new().with_text("name", "Zulu").unwrap();
        let statement = "CALL db.labels() YIELD label WHERE label = $name RETURN label";
        let prepared = session.prepare(&c.query(), statement, &first).unwrap();
        let at_prepare = reverse_calls.get();
        assert!(at_prepare > 0);
        current.borrow_mut().0.insert_label(LabelId(2), "Changed");
        current.borrow_mut().0.insert_label(LabelId(4), "Later");
        assert_eq!(
            session.execute(&c.query(), &prepared, &first).unwrap(),
            names("label", &["Alpha"])
        );
        assert_eq!(
            session.execute(&c.query(), &prepared, &second).unwrap(),
            names("label", &["Zulu"])
        );
        assert_eq!(
            reverse_calls.get(),
            at_prepare,
            "binding must not reenter the host catalog"
        );
        let changed = GqlParameters::new().with_text("name", "Changed").unwrap();
        assert_eq!(
            session.query(&c.query(), statement, &changed).unwrap(),
            names("label", &["Changed"])
        );
        assert!(reverse_calls.get() > at_prepare);
        issuer.retire();
        assert!(matches!(
            session.execute(&c.query(), &prepared, &first),
            Err(QueryError::Authorization(Error::AuthorityRetired))
        ));
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pinned_and_staged_hosts_execute_the_same_catalog_plan_with_their_own_graph_cut() {
    let ((), report) = run_async_under_lab(0xca07, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = empty(&c.commit()).await;
        let mut first = WriteBatch::new(RelationId(2));
        first.create_vertex(
            VId(1),
            vec![LabelId(2)],
            vec![(PropertyKeyId(2), CanonicalScalar::Int(7))],
        );
        db.write(&c.commit(), first).await.unwrap();
        let view = db.pinned_read_view().unwrap();
        let selected = view.frontier();
        let statement = "CALL db.labels() YIELD label WITH label WHERE label = 'Alpha' MATCH (n:Alpha) RETURN label, n.alpha AS value ORDER BY value";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(statement, &params, Catalog::sample()).unwrap();
        let before = rows(&["label", "value"], vec![vec![text("Alpha"), int(7)]]);
        assert_eq!(
            prepared
                .execute(&db, &c.query(), &params, policy())
                .unwrap(),
            before
        );

        let txcx = c.txn();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(RelationId(2));
        staged.create_vertex(
            VId(2),
            vec![LabelId(2)],
            vec![(PropertyKeyId(2), CanonicalScalar::Int(13))],
        );
        transaction.write(&mut db, staged).unwrap();
        assert_eq!(
            transaction
                .query(
                    &db,
                    &c.query(),
                    statement,
                    &params,
                    Catalog::sample(),
                    policy()
                )
                .unwrap(),
            rows(
                &["label", "value"],
                vec![vec![text("Alpha"), int(7)], vec![text("Alpha"), int(13)]]
            ),
        );
        assert_eq!(
            view.query(&c.query(), statement, &params, Catalog::sample(), policy())
                .unwrap(),
            before
        );
        assert_eq!(view.frontier(), selected);
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
        assert_eq!(
            prepared
                .execute(&db, &c.query(), &params, policy())
                .unwrap(),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_credentials_refuse_before_the_reverse_catalog_callback() {
    struct ForbiddenCatalog;
    impl GraphSymbolResolver for ForbiddenCatalog {
        fn resolve_symbol(&mut self, _: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
            panic!("authentication must precede forward catalog lookup")
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            panic!("authentication must precede reverse catalog discovery")
        }
    }

    let ((), report) = run_async_under_lab(0xca08, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = empty(&c.commit()).await;
        let issuer = authority(0xca08);
        let wrong = authority(0xca09);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for (call, _, _) in CALLS {
            assert!(matches!(
                db.query_authorized(
                    &c.query(),
                    &wrong,
                    &token,
                    "main",
                    call,
                    &GqlParameters::new(),
                    ForbiddenCatalog,
                    policy(),
                    || 100,
                ),
                Err(QueryError::Authorization(Error::Unauthenticated)),
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
