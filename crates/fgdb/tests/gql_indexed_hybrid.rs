//! Explicit indexed CALL execution uses the same native retrieval and scoped
//! source as the typed API. Tests distinguish the execution profiles by their
//! work allowance, not a timing threshold or a planner label alone.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xf2; 32]);
const R: RelationId = RelationId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 100_000_000, 1_000_000)
}

fn scalar_text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn props(id: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![(BODY, scalar_text("graph")), (X, CanonicalScalar::Int(id))]
}

async fn open(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0xf1; 32], NS, [0xf3; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [1, 2, 3, 4, 9] {
        batch.create_vertex(VId(id), vec![LabelId(1)], props(id as i64));
    }
    for (id, from, to) in [(1, 1, 2), (2, 2, 3), (3, 3, 4)] {
        batch.add_edge(EId(id), VId(from), VId(to), vec![]);
    }
    // No seed reaches vertex 9. Eager graph preparation must inspect these;
    // indexed expansion must not copy them or spend their incidence budget.
    for id in 100..612 {
        batch.add_edge(EId(id), VId(9), VId(9), vec![]);
    }
    if hidden {
        batch.create_vertex(VId(90), vec![LabelId(99)], props(0));
        batch.add_edge(EId(90), VId(1), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), VId(3), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut shortcut = WriteBatch::new(RelationId(2));
        shortcut.add_edge(EId(92), VId(1), VId(3), vec![]);
        db.write(cx, shortcut).await.unwrap();
    }
    db
}

fn statement(access: Option<&str>, extra: &str) -> String {
    let access = access.map_or_else(String::new, |access| format!(", graph_access => {access}"));
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(", {extra}")
    };
    format!(
        "CALL hybrid.search(seeds => [1], max_hops => 2, relation => 'R', \
         candidates => 1, k => 1{access}{extra}) \
         YIELD node, score, graph_hops, graph_rank, text_rank, vector_rank, vector_distance, text_score \
         RETURN node, score, graph_hops, graph_rank, text_rank, vector_rank, vector_distance, text_score"
    )
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("retrieval returned a write"),
    }
}

fn int(value: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))
}

#[test]
fn native_indexed_selection_composes_and_matches_every_materialized_lane() {
    let ((), report) = run_async_under_lab(0xbeac_3901, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = open(&c.commit(), false).await;
        let params = GqlParameters::new();
        for extra in [
            "eligible => [3]",
            "eligible => [3], text => 'graph', text_property => 'body'",
            "eligible => [3], vector => [0], vector_properties => ['x']",
            "eligible => [3], text => 'graph', text_property => 'body', vector => [0], vector_properties => ['x']",
        ] {
            let expected = db
                .query(
                    &c.query(),
                    &statement(None, extra),
                    &params,
                    symbols,
                    policy(),
                )
                .unwrap();
            for mode in ["'materialized'", "'indexed'", "NULL"] {
                let result = db
                    .query(
                        &c.query(),
                        &statement(Some(mode), extra),
                        &params,
                        symbols,
                        policy(),
                    )
                    .unwrap();
                assert_eq!(result, expected, "mode={mode}, {extra}");
            }
            let actual = rows(expected);
            assert_eq!(actual.len(), 1);
            assert_eq!(actual[0][0], QueryValue::Value(GraphValue::Vertex(VId(3))));
            assert_eq!(
                actual[0][2],
                int(2),
                "rejected vertex 2 must remain a transit vertex"
            );
            assert_eq!(actual[0][3], int(1), "filter precedes candidate truncation");
        }
        let text = "CALL hybrid.search(seeds => [9], max_hops => 0, include_seeds => true, \
                    graph_access => 'indexed') YIELD node, graph_hops RETURN node, graph_hops";
        assert_eq!(
            rows(
                db.query(&c.query(), text, &params, symbols, policy())
                    .unwrap()
            ),
            vec![vec![QueryValue::Value(GraphValue::Vertex(VId(9))), int(0)]]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_indexed_source_reuses_one_prepared_template_and_pinned_history() {
    let ((), report) = run_async_under_lab(0xbeac_3902, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = open(&c.commit(), false).await;
        let pinned = db.read_session().unwrap();
        let text = "CALL hybrid.search(seeds => [$source], max_hops => 2, \
                    graph_access => 'indexed', eligible => [3], k => 1) \
                    YIELD node, graph_hops RETURN node, graph_hops";
        let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
        for source in [1, 2] {
            let params = GqlParameters::new().with_int64("source", source).unwrap();
            let query = prepared.bind_parameters(&params).unwrap();
            let expected = vec![GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(3)),
                GraphValue::Scalar(CanonicalScalar::Int(3 - source)),
            ])];
            assert_eq!(
                db.execute_graph_set_governed(&c.query(), &query, policy())
                    .unwrap()
                    .value,
                expected
            );
        }
        let params = GqlParameters::new().with_int64("source", 1).unwrap();
        let before = pinned
            .query(&c.query(), text, &params, symbols, policy())
            .unwrap();
        let mut edit = WriteBatch::new(R);
        edit.delete_edge(EId(2));
        db.write(&c.commit(), edit).await.unwrap();
        assert!(
            rows(
                db.query(&c.query(), text, &params, symbols, policy())
                    .unwrap()
            )
            .is_empty()
        );
        db.compact(&c.commit()).await.unwrap();
        assert_eq!(
            pinned
                .query(&c.query(), text, &params, symbols, policy())
                .unwrap(),
            before
        );
        drop(db);
        assert_eq!(
            pinned
                .query(&c.query(), text, &params, symbols, policy())
                .unwrap(),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_tight_native_work_budget_proves_indexed_execution_not_an_eager_fallback() {
    let ((), report) = run_async_under_lab(0xbeac_3903, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = open(&c.commit(), false).await;
        let params = GqlParameters::new();
        let prepare = |mode| {
            PreparedGraphSetText::prepare(&statement(Some(mode), "eligible => [3]"), symbols)
                .unwrap()
                .bind_parameters(&params)
                .unwrap()
        };
        let indexed = prepare("'indexed'");
        let materialized = prepare("'materialized'");
        let actual = db
            .execute_graph_set_governed(&c.query(), &indexed, policy())
            .unwrap();
        let eager = db
            .execute_graph_set_governed(&c.query(), &materialized, policy())
            .unwrap();
        assert_eq!(actual.value, eager.value);
        assert!(
            actual.evaluator.work_units < eager.evaluator.work_units,
            "the disconnected edge population must distinguish the source profiles"
        );
        let exact = GqlQueryPolicy::new(
            actual.rows.snapshot_records,
            actual.rows.result_rows,
            actual.evaluator.work_units,
            actual.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_set_governed(&c.query(), &indexed, exact)
                .unwrap()
                .value,
            actual.value
        );
        assert!(
            db.execute_graph_set_governed(&c.query(), &materialized, exact)
                .is_err()
        );
        let mut short = exact;
        short.evaluator.max_work_units -= 1;
        assert!(
            db.execute_graph_set_governed(&c.query(), &indexed, short)
                .is_err()
        );
        let mut scratch = exact;
        scratch.evaluator.max_scratch_entries -= 1;
        assert!(
            db.execute_graph_set_governed(&c.query(), &indexed, scratch)
                .is_err()
        );
        // A final empty page does not excuse a source or endpoint-filter refusal.
        let empty = PreparedGraphSetText::prepare(
            &format!(
                "{} LIMIT 0",
                statement(Some("'indexed'"), "eligible => [-1]")
            ),
            symbols,
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        assert!(
            db.execute_graph_set_governed(&c.query(), &empty, policy())
                .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_authorized_indexed_calls_mask_hidden_sources_and_recheck_expiry() {
    let ((), report) = run_async_under_lab(0xbeac_3904, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let full = open(&c.commit(), true).await;
        let clean = open(&c.commit(), false).await;
        let authority = Authority::new(
            AuthKey::from_seed(3904),
            NS,
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 100,
                max_work: 10_000_000,
                max_rows: 100,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([BODY, X]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let params = GqlParameters::new();
        let text = statement(Some("'indexed'"),
            "eligible => [3, 90], text => 'graph', text_property => 'body', vector => [0], vector_properties => ['x']")
            .replace(", relation => 'R'", "");
        let expected = clean
            .query(&c.query(), &text, &params, symbols, policy())
            .unwrap();
        let privileged = full
            .query(&c.query(), &text, &params, symbols, policy())
            .unwrap();
        assert_ne!(
            privileged, expected,
            "the hidden shortcut must be an active control"
        );
        assert_eq!(
            full.query_authorized(
                &c.query(),
                &authority,
                &token,
                "main",
                &text,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            expected
        );
        let now = Cell::new(100_u64);
        let mut session = full
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                symbols,
                policy(),
                || now.get(),
            )
            .unwrap();
        let prepared = session.prepare(&c.query(), &text, &params).unwrap();
        assert_eq!(
            session.execute(&c.query(), &prepared, &params).unwrap(),
            expected
        );
        now.set(1100);
        assert!(session.execute(&c.query(), &prepared, &params).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_indexed_access_reads_staged_effects_and_preserves_them_on_failure() {
    let ((), report) = run_async_under_lab(0xbeac_3905, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = open(&c.commit(), false).await;
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.create_vertex(VId(10), vec![LabelId(1)], props(10));
        edit.add_edge(EId(10_000), VId(2), VId(10), vec![]);
        txn.write(&mut db, edit).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let params = GqlParameters::new();
        let text = statement(Some("'materialized'"), "eligible => [10]");
        let staged = rows(
            txn.query(&db, &c.query(), &text, &params, symbols, policy())
                .unwrap(),
        );
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0][0], QueryValue::Value(GraphValue::Vertex(VId(10))));
        assert_eq!(staged[0][2], int(2));
        assert!(
            rows(
                db.query(&c.query(), &text, &params, symbols, policy())
                    .unwrap()
            )
            .is_empty()
        );
        for tail in ["", " LIMIT 0"] {
            let text = format!("{}{tail}", statement(Some("'indexed'"), "eligible => [10]"));
            let actual = rows(
                txn.query(&db, &c.query(), &text, &params, symbols, policy())
                    .unwrap(),
            );
            assert_eq!(
                actual,
                if tail.is_empty() {
                    staged.clone()
                } else {
                    Vec::new()
                }
            );
            for (access, filter) in [("'auto'", "[10]"), ("'indexed'", "[-1]")] {
                let invalid = format!(
                    "{}{tail}",
                    statement(Some(access), &format!("eligible => {filter}"))
                );
                assert!(
                    txn.query(&db, &c.query(), &invalid, &params, symbols, policy())
                        .is_err()
                );
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                assert_eq!(db.frontier().unwrap(), basis);
            }
        }
        txn.abort();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_or_orphan_source_profiles_refuse_even_with_limit_zero() {
    let ((), report) = run_async_under_lab(0xbeac_3906, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = open(&c.commit(), false).await;
        let params = GqlParameters::new();
        for value in ["'auto'", "1", "true", "['indexed']"] {
            let text = format!("{} LIMIT 0", statement(Some(value), ""));
            assert!(
                db.query(&c.query(), &text, &params, symbols, policy())
                    .is_err(),
                "{value}"
            );
        }
        for access in ["'indexed'", "'materialized'"] {
            let text = format!(
                "CALL hybrid.search(text => 'graph', text_property => 'body', \
                graph_access => {access}) YIELD node RETURN node LIMIT 0"
            );
            assert!(
                db.query(&c.query(), &text, &params, symbols, policy())
                    .is_err()
            );
        }
        let duplicate = statement(Some("'indexed'"), "graph_access => 'materialized'");
        assert!(
            db.query(&c.query(), &duplicate, &params, symbols, policy())
                .is_err()
        );
        let late = "CALL hybrid.search(seeds => [1], max_hops => 2, graph_access => 'indexed') \
                    YIELD node RETURN 1 / 0 AS fail LIMIT 0";
        assert!(
            db.query(&c.query(), late, &params, symbols, policy())
                .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Source IDs are rebound against the SAME staged topology and projected
/// properties. The committed graph cannot supply either new endpoint.
#[test]
fn transaction_indexed_parameters_and_all_lanes_follow_staged_topology_and_rollback() {
    let ((), report) = run_async_under_lab(0xbeac_4201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = open(&c.commit(), false).await;
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.delete_edge(EId(2));
        edit.create_vertex(VId(10), vec![LabelId(1)], props(10));
        edit.create_vertex(VId(11), vec![LabelId(1)], props(0));
        edit.add_edge(EId(10_000), VId(2), VId(10), vec![]);
        edit.add_edge(EId(10_001), VId(10), VId(11), vec![]);
        txn.write(&mut db, edit).unwrap();
        txn.savepoint(&db, "linked").unwrap();
        let text = |access, modalities| {
            format!(
                "CALL hybrid.search(seeds => [$source], max_hops => 3, \
             graph_access => '{access}', label => 'Person', relation => 'R', \
             eligible => [11], candidates => 1, k => 1{modalities}) \
             YIELD node, graph_hops, graph_rank, vector_rank, text_rank, vector_distance \
             RETURN node.x AS x, graph_hops, graph_rank, vector_rank, text_rank, vector_distance"
            )
        };
        for (modalities, vector, text_lane) in [
            ("", false, false),
            (", text => 'graph', text_property => 'body'", false, true),
            (", vector => [0], vector_properties => ['x']", true, false),
            (
                ", text => 'graph', text_property => 'body', vector => [0], vector_properties => ['x']",
                true,
                true,
            ),
        ] {
            for (source, hops) in [(1, 3), (2, 2), (10, 1)] {
                let params = GqlParameters::new().with_int64("source", source).unwrap();
                let actual = rows(
                    txn.query(
                        &db,
                        &c.query(),
                        &text("indexed", modalities),
                        &params,
                        symbols,
                        policy(),
                    )
                    .unwrap(),
                );
                assert_eq!(
                    actual,
                    rows(
                        txn.query(
                            &db,
                            &c.query(),
                            &text("materialized", modalities),
                            &params,
                            symbols,
                            policy()
                        )
                        .unwrap()
                    )
                );
                assert_eq!(actual.len(), 1);
                assert_eq!(&actual[0][..3], &[int(0), int(hops), int(1)]);
                let null = QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
                assert_eq!(actual[0][3], if vector { int(1) } else { null.clone() });
                assert_eq!(actual[0][4], if text_lane { int(1) } else { null.clone() });
                if vector {
                    assert_eq!(
                        actual[0][5],
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Float(
                            fgdb_types::CanonicalF64::new(0.0)
                        ),))
                    );
                } else {
                    assert_eq!(actual[0][5], null);
                }
                assert!(
                    rows(
                        db.query(
                            &c.query(),
                            &text("indexed", modalities),
                            &params,
                            symbols,
                            policy()
                        )
                        .unwrap()
                    )
                    .is_empty()
                );
            }
        }
        let params = GqlParameters::new().with_int64("source", 1).unwrap();
        let graph_only = text("indexed", "");
        let before = rows(
            txn.query(&db, &c.query(), &graph_only, &params, symbols, policy())
                .unwrap(),
        );
        let mut exclude = WriteBatch::new(R);
        exclude.set_vertex_label(VId(10), LabelId(1), false);
        txn.write(&mut db, exclude).unwrap();
        assert!(
            rows(
                txn.query(&db, &c.query(), &graph_only, &params, symbols, policy())
                    .unwrap()
            )
            .is_empty()
        );
        txn.rollback_to_savepoint(&db, "linked").unwrap();
        assert_eq!(
            rows(
                txn.query(&db, &c.query(), &graph_only, &params, symbols, policy())
                    .unwrap()
            ),
            before
        );
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &c.commit()).await.unwrap();
        assert_eq!(
            rows(
                db.query(&c.query(), &graph_only, &params, symbols, policy())
                    .unwrap()
            ),
            before
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_indexed_source_fits_reached_record_budget_without_eager_retry() {
    let ((), report) = run_async_under_lab(0xbeac_4202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = open(&c.commit(), false).await;
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.create_vertex(VId(10), vec![LabelId(1)], props(10));
        edit.add_edge(EId(10_000), VId(2), VId(10), vec![]);
        txn.write(&mut db, edit).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let params = GqlParameters::new();
        let indexed = statement(Some("'indexed'"), "eligible => [10]");
        let before = rows(
            txn.query(&db, &c.query(), &indexed, &params, symbols, policy())
                .unwrap(),
        );
        // Six selected vertices plus reached incidences fit; copying the 512
        // unreachable loops cannot. This is an admission control, not timing.
        let reached = GqlQueryPolicy::new(128, 100, 10_000_000, 1_000_000);
        assert_eq!(
            rows(
                txn.query(&db, &c.query(), &indexed, &params, symbols, reached)
                    .unwrap()
            ),
            before
        );
        for access in [None, Some("NULL"), Some("'materialized'")] {
            let eager = statement(access, "eligible => [10]");
            assert_eq!(
                rows(
                    txn.query(&db, &c.query(), &eager, &params, symbols, policy())
                        .unwrap()
                ),
                before
            );
            for tail in ["", " LIMIT 0"] {
                assert!(
                    txn.query(
                        &db,
                        &c.query(),
                        &format!("{eager}{tail}"),
                        &params,
                        symbols,
                        reached
                    )
                    .is_err()
                );
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            }
        }
        // A filter with no successful row cannot skip source admission either.
        let none = statement(Some("'indexed'"), "eligible => []");
        let denied = GqlQueryPolicy::new(0, 100, 10_000_000, 1_000_000);
        assert!(
            txn.query(&db, &c.query(), &none, &params, symbols, denied)
                .is_err()
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), basis);
        txn.abort();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_indexed_empty_pages_and_late_errors_keep_transaction_read_conflicts() {
    let ((), report) =
        run_async_under_lab(0xbeac_4203, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            for case in 0..3 {
                let mut db = open(&c.commit(), false).await;
                let basis = db.frontier().unwrap();
                let mut txn = db.begin(&c.txn()).unwrap();
                txn.savepoint(&db, "before-read").unwrap();
                let params = GqlParameters::new();
                let text = match case {
                0 => statement(Some("'indexed'"), "eligible => []"),
                1 => format!("{} LIMIT 0", statement(Some("'indexed'"), "eligible => [3]")),
                _ => "CALL hybrid.search(seeds => [1], max_hops => 2, graph_access => 'indexed') \
                      YIELD node RETURN 1 / 0 AS failure LIMIT 0".to_owned(),
            };
                let result = txn.query(&db, &c.query(), &text, &params, symbols, policy());
                if case == 2 {
                    assert!(result.is_err(), "the post-retrieval expression must fail");
                } else {
                    assert!(rows(result.unwrap()).is_empty());
                }
                txn.rollback_to_savepoint(&db, "before-read").unwrap();
                assert_eq!(db.frontier().unwrap(), basis);
                let mut concurrent = WriteBatch::new(R);
                concurrent.add_edge(EId(99), VId(1), VId(4), vec![]);
                db.write(&c.commit(), concurrent).await.unwrap();
                let frontier = db.frontier().unwrap();
                assert!(
                    matches!(
                        txn.finish(&mut db, &c.commit()).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ),
                    "case={case}"
                );
                assert_eq!(db.frontier().unwrap(), frontier);
            }
            assert_eq!(c.outstanding_obligations(), 0);
        });
    assert!(report.lab_test_passed(), "{report:?}");
}
