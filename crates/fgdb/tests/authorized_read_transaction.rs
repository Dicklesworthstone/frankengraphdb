//! One read-only owner retains a snapshot and a cumulative signed allowance.
//! These laws cross the public API, real committed generations and Warden.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GlaLimitDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphElementMetadata,
    GraphSymbol, GraphSymbolKind, ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x6b; 32]);
const NOW: u64 = 100;
const READ: &str = "MATCH (n:L) RETURN n.p AS p ORDER BY p";

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0x6b01), NS, "graph", SchemaEpoch(0), 1).unwrap()
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
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x6a; 32], NS, [0x6c; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, labels, value) in [
        (1, vec![LabelId(1), LabelId(99)], 7),
        (2, vec![LabelId(99)], 13),
        (3, vec![LabelId(1)], 19),
    ] {
        batch.create_vertex(
            VId(id),
            labels,
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(value)),
                (PropertyKeyId(2), CanonicalScalar::Int(55)),
            ],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn rows(values: &[i64]) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["p".into()],
        rows: values
            .iter()
            .map(|value| {
                vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                    *value,
                )))]
            })
            .collect(),
    }
}

fn refused<T>(result: Result<T, QueryError>, expected: Error) {
    assert!(matches!(result, Err(QueryError::Authorization(actual)) if actual == expected));
}

fn lab<F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn read_only_transaction_preserves_masking_and_snapshot_across_writes_and_writer_drop() {
    lab(0x6b10, |contexts| async move {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = database(&commit).await;
        let basis = db.frontier().unwrap();
        let retained = db.pinned_read_view().unwrap().downgrade();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut owner = {
            let begin_context = query.clone();
            db.begin_authorized_read(
                &begin_context,
                &issuer,
                &token,
                "main",
                symbols,
                policy(),
                || NOW,
            )
            .unwrap()
        };
        assert_eq!(owner.basis(), basis);
        assert_eq!(owner.completed_statements(), 0);
        let params = GqlParameters::new();
        assert_eq!(owner.query(&query, READ, &params).unwrap(), rows(&[7, 19]));
        assert_eq!(
            owner
                .query(&query, "MATCH (n:Hidden) RETURN n.p AS p", &params)
                .unwrap(),
            rows(&[])
        );
        assert_eq!(
            owner
                .query(&query, "MATCH (n:L) RETURN n.secret AS secret", &params)
                .unwrap(),
            QueryResult::Rows {
                columns: vec!["secret".into()],
                rows: vec![vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null))]; 2],
            }
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        change.create_vertex(
            VId(4),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(29))],
        );
        db.write(&commit, change).await.unwrap();
        let latest = db.frontier().unwrap();
        assert!(latest > basis);
        let mut fresh = db
            .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
            .unwrap();
        assert_eq!(
            fresh.query(&query, READ, &params).unwrap(),
            rows(&[19, 29, 70])
        );
        fresh.rollback();
        assert_eq!(
            db.frontier().unwrap(),
            latest,
            "reads allocate no commit sequence"
        );
        drop(db);
        drop(token);
        assert_eq!(owner.query(&query, READ, &params).unwrap(), rows(&[7, 19]));
        assert_eq!(owner.completed_statements(), 4);
        assert!(owner.is_active());
        assert_eq!(retained.upgrade().unwrap().frontier(), basis);
        assert_eq!(owner.close(&query).unwrap(), basis);
        assert!(
            retained.upgrade().is_none(),
            "successful close releases the last old-generation pin"
        );
    });
}

#[test]
fn signed_node_and_row_allowances_accumulate_across_separately_delivered_statements() {
    lab(0x6b11, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let params = GqlParameters::new();
        for dimension in [LimitDimension::Nodes, LimitDimension::Rows] {
            let mut allowed = grant();
            match dimension {
                LimitDimension::Nodes => allowed.limits.max_nodes = 3,
                LimitDimension::Rows => allowed.limits.max_rows = 3,
                LimitDimension::Work => unreachable!(),
            }
            let token = issuer.issue_at(&allowed, NOW).unwrap();
            let mut owner = db
                .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
                .unwrap();
            let text = "MATCH (n) RETURN n.p AS p";
            assert_eq!(owner.query(&query, text, &params).unwrap(), rows(&[7, 19]));
            let output_called = Cell::new(false);
            refused(
                owner.query_with_output(
                    &query,
                    text,
                    &params,
                    |result, _| {
                        output_called.set(true);
                        Ok(result)
                    },
                    |error| error,
                ),
                Error::LimitExceeded(dimension),
            );
            assert!(
                !output_called.get(),
                "signed row admission precedes output adaptation"
            );
            assert_eq!(owner.completed_statements(), 1);
            assert!(!owner.is_active());
            refused(
                owner.query(&query, "RETURN 1 AS n", &params),
                Error::ExecutionStopped,
            );
            refused(owner.close(&query), Error::ExecutionStopped);
        }
    });
}

#[test]
fn signed_work_includes_input_and_output_and_cannot_be_reset_or_swallowed() {
    lab(0x6b12, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let mut allowed = grant();
        allowed.limits.max_work = 10_000;
        let token = issuer.issue_at(&allowed, NOW).unwrap();
        let mut owner = db
            .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
            .unwrap();
        let params = GqlParameters::new();
        let first: Result<QueryResult, QueryError> = owner.query_with_output(
            &query,
            "RETURN 1 AS n",
            &params,
            |result, work| {
                work(6000)?;
                Ok(result)
            },
            |error| error,
        );
        first.unwrap();
        let called = Cell::new(false);
        let second: Result<QueryResult, QueryError> = owner.query_with_output(
            &query,
            "RETURN 2 AS n",
            &params,
            |result, work| {
                called.set(true);
                refused(work(6000), Error::LimitExceeded(LimitDimension::Work));
                Ok(result) // The host cannot erase a failure by swallowing it.
            },
            |error| error,
        );
        assert!(called.get());
        refused(second, Error::LimitExceeded(LimitDimension::Work));
        assert_eq!(owner.completed_statements(), 1);
        assert!(!owner.is_active());

        // The complete original UTF-8 input must fit the same permit before
        // the host's symbol resolver or graph source can be consulted.
        let text = format!("/*{}*/ {READ}", "雪".repeat(128));
        let mut allowed = grant();
        allowed.limits.max_work = (text.len() + params.canonical_byte_len()) as u64;
        let token = issuer.issue_at(&allowed, NOW).unwrap();
        let calls = Cell::new(0);
        let mut owner = db
            .begin_authorized_read(
                &query,
                &issuer,
                &token,
                "main",
                |kind, name: &str| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                },
                policy(),
                || NOW,
            )
            .unwrap();
        refused(
            owner.query(&query, &text, &params),
            Error::LimitExceeded(LimitDimension::Work),
        );
        assert_eq!(calls.get(), 0);
        assert_eq!(owner.completed_statements(), 0);
        assert!(!owner.is_active());
    });
}

#[test]
fn output_refusal_unwind_expiry_and_retirement_close_the_owner_and_release_its_pin() {
    lab(0x6b13, |contexts| async move {
        let query = contexts.query();
        let params = GqlParameters::new();
        for failure in 0..4 {
            let db = database(&contexts.commit()).await;
            let retained = db.pinned_read_view().unwrap().downgrade();
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let now = Cell::new(NOW);
            let mut owner = db
                .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || {
                    now.get()
                })
                .unwrap();
            drop(db);
            assert!(retained.upgrade().is_some());
            if failure == 1 {
                let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _: Result<(), QueryError> = owner.query_with_output(
                        &query,
                        READ,
                        &params,
                        |_, _| panic!("injected output callback unwind"),
                        |error| error,
                    );
                }));
                assert!(unwind.is_err());
            } else {
                let result: Result<QueryResult, QueryError> = owner.query_with_output(
                    &query,
                    READ,
                    &params,
                    |result, _| match failure {
                        0 => Err(QueryError::Unsupported {
                            diagnostics: vec!["host output refused".into()],
                        }),
                        2 => {
                            now.set(1000);
                            Ok(result)
                        }
                        3 => {
                            issuer.retire();
                            Ok(result)
                        }
                        _ => unreachable!(),
                    },
                    |error| error,
                );
                match failure {
                    0 => assert!(
                        matches!(result, Err(QueryError::Unsupported { diagnostics }) if diagnostics == ["host output refused"])
                    ),
                    2 => refused(result, Error::Expired),
                    3 => refused(result, Error::AuthorityRetired),
                    _ => unreachable!(),
                }
            }
            assert!(!owner.is_active());
            assert_eq!(owner.completed_statements(), 0);
            assert!(
                retained.upgrade().is_none(),
                "a terminal output refusal retains no read pin"
            );
            refused(owner.query(&query, READ, &params), Error::ExecutionStopped);
        }
    });
}

#[test]
fn exact_basis_is_required_for_temporal_reads_and_failed_statements_are_terminal() {
    lab(0x6b14, |contexts| async move {
        let query = contexts.query();
        let mut db = database(&contexts.commit()).await;
        let old = db.frontier().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        db.write(&contexts.commit(), change).await.unwrap();
        let basis = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for text in [
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n.p AS p",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN count(*) AS n",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n.p AS p UNION ALL MATCH (n:L) RETURN n.p AS p",
        ] {
            let view = db.pinned_read_view().unwrap();
            let mut owner = view
                .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
                .unwrap();
            let current = GqlParameters::new().with_uint64("cut", basis.0).unwrap();
            assert!(owner.query(&query, text, &current).is_ok(), "{text}");
            let previous = GqlParameters::new().with_uint64("cut", old.0).unwrap();
            assert!(
                matches!(
                    owner.query(&query, text, &previous),
                    Err(QueryError::Unsupported { .. })
                ),
                "{text}"
            );
            assert_eq!(owner.completed_statements(), 1);
            assert!(!owner.is_active());

            let mut no_source = grant();
            no_source.limits.max_nodes = 0;
            let token = issuer.issue_at(&no_source, NOW).unwrap();
            let mut owner = db
                .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
                .unwrap();
            assert!(
                matches!(
                    owner.query(&query, text, &previous),
                    Err(QueryError::Unsupported { .. })
                ),
                "a different cut is refused before consulting even a zero-node source"
            );
        }
        for text in [
            "AT BRANCH other MATCH (n:L) RETURN n",
            "CREATE (n:L {p:90}) RETURN n",
            "this is not a query",
        ] {
            let mut owner = db
                .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
                .unwrap();
            assert!(
                owner.query(&query, text, &GqlParameters::new()).is_err(),
                "{text}"
            );
            assert!(!owner.is_active());
            assert_eq!(owner.completed_statements(), 0);
            refused(
                owner.query(&query, READ, &GqlParameters::new()),
                Error::ExecutionStopped,
            );
            assert_eq!(db.frontier().unwrap(), basis);
        }
    });
}

#[test]
fn close_checks_live_authority_even_for_an_owner_that_never_executed_a_statement() {
    lab(0x6b15, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let basis = db.frontier().unwrap();
        let mut empty = grant();
        empty.limits.max_nodes = 0;
        empty.limits.max_rows = 0;
        let issuer = authority();
        let token = issuer.issue_at(&empty, NOW).unwrap();
        let owner = db
            .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
            .unwrap();
        assert_eq!(owner.completed_statements(), 0);
        assert_eq!(owner.close(&query).unwrap(), basis);
        assert_eq!(db.frontier().unwrap(), basis);
        for expected in [
            Error::Expired,
            Error::ClockWentBackwards,
            Error::AuthorityRetired,
        ] {
            let issuer = authority();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let now = Cell::new(NOW);
            let owner = db
                .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || {
                    now.get()
                })
                .unwrap();
            match expected {
                Error::Expired => now.set(1000),
                Error::ClockWentBackwards => now.set(NOW - 1),
                Error::AuthorityRetired => {
                    issuer.retire();
                }
                _ => unreachable!(),
            }
            refused(owner.close(&query), expected);
            assert_eq!(db.frontier().unwrap(), basis);
        }
    });
}

#[test]
fn request_cancellation_closes_queries_and_unused_owners_without_retaining_pins() {
    let ((), report) = run_async_under_lab(0x6b16, |root| async move {
        for close in [false, true] {
            let mut child = root
                .spawn(move |child| async move {
                    let contexts = PurposeContexts::narrow_runtime_root(&child);
                    let query = contexts.query();
                    let db = database(&contexts.commit()).await;
                    let retained = db.pinned_read_view().unwrap().downgrade();
                    let issuer = authority();
                    let token = issuer.issue_at(&grant(), NOW).unwrap();
                    let mut owner = db
                        .begin_authorized_read(
                            &query,
                            &issuer,
                            &token,
                            "main",
                            symbols,
                            policy(),
                            || NOW,
                        )
                        .unwrap();
                    drop(db);
                    child.set_cancel_reason(asupersync::CancelReason::user("cancel read owner"));
                    if close {
                        assert!(matches!(
                            owner.close(&query),
                            Err(QueryError::Pattern(GqlQueryError::Interrupted(_)))
                        ));
                    } else {
                        assert!(matches!(
                            owner.query(&query, READ, &GqlParameters::new()),
                            Err(QueryError::Pattern(GqlQueryError::Interrupted(_)))
                        ));
                        assert!(!owner.is_active());
                        refused(
                            owner.query(&query, READ, &GqlParameters::new()),
                            Error::ExecutionStopped,
                        );
                    }
                    assert!(retained.upgrade().is_none());
                })
                .unwrap();
            assert_eq!(child.join(&root).await, Ok(()));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn visible_catalog() -> ReverseSymbolCatalog {
    let mut catalog = ReverseSymbolCatalog::new();
    catalog.insert_label(LabelId(1), "L");
    catalog.insert_relation(RelationId(1), "R");
    catalog.insert_property(PropertyKeyId(1), "p");
    catalog
}

#[test]
fn graph_output_reuses_the_original_cut_and_masks_before_catalog_access() {
    lab(0x6b20, |contexts| async move {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = database(&commit).await;
        let mut edges = WriteBatch::new(RelationId(1));
        edges.add_edge(
            EId(40),
            VId(1),
            VId(3),
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(5)),
                (PropertyKeyId(2), CanonicalScalar::Int(99)),
            ],
        );
        edges.add_edge(EId(41), VId(1), VId(2), vec![]);
        db.write(&commit, edges).await.unwrap();
        let basis = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut owner = db
            .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
            .unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        update.set_edge_property(EId(40), PropertyKeyId(1), Some(CanonicalScalar::Int(50)));
        db.write(&commit, update).await.unwrap();
        drop(db);
        let catalog = visible_catalog(); // Hidden labels/fields have no names.
        let result: Result<(), QueryError> = owner.query_with_graph_output(
            &query,
            "RETURN 1 AS value",
            &GqlParameters::new(),
            |_, work, metadata| {
                work(1)?;
                let vertex = ElementId::Vertex(VId(1));
                let edge = ElementId::Edge(EId(40));
                assert_eq!(
                    metadata.metadata(vertex, GraphElementMetadata::Labels, &catalog)?,
                    Some(GraphValue::List(
                        vec![GraphValue::Scalar(
                            CanonicalScalar::ucs_basic_text("L").unwrap()
                        )]
                        .into_boxed_slice()
                    ))
                );
                assert_eq!(
                    metadata.metadata(vertex, GraphElementMetadata::Properties, &catalog)?,
                    GraphValue::map(vec![(
                        "p".into(),
                        GraphValue::Scalar(CanonicalScalar::Int(7))
                    )])
                );
                assert_eq!(
                    metadata.metadata(edge, GraphElementMetadata::Properties, &catalog)?,
                    GraphValue::map(vec![(
                        "p".into(),
                        GraphValue::Scalar(CanonicalScalar::Int(5))
                    )])
                );
                assert_eq!(
                    metadata.metadata(edge, GraphElementMetadata::Type, &catalog)?,
                    Some(GraphValue::Scalar(
                        CanonicalScalar::ucs_basic_text("R").unwrap()
                    ))
                );
                assert_eq!(
                    metadata.metadata(edge, GraphElementMetadata::StartNode, &catalog)?,
                    Some(GraphValue::Vertex(VId(1)))
                );
                assert_eq!(
                    metadata.metadata(edge, GraphElementMetadata::EndNode, &catalog)?,
                    Some(GraphValue::Vertex(VId(3)))
                );
                for hidden in [
                    ElementId::Vertex(VId(2)),
                    ElementId::Edge(EId(41)),
                    ElementId::Vertex(VId(999)),
                ] {
                    assert_eq!(
                        metadata.metadata(hidden, GraphElementMetadata::Properties, &catalog)?,
                        None
                    );
                }
                Ok(())
            },
            |error| error,
        );
        result.unwrap();
        assert_eq!(owner.completed_statements(), 1);
        assert_eq!(owner.close(&query).unwrap(), basis);
    });
}

#[test]
fn graph_output_spends_the_same_signed_nodes_across_queries_and_latches_refusal() {
    lab(0x6b21, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let pin = db.pinned_read_view().unwrap().downgrade();
        let issuer = authority();
        let mut allowed = grant();
        allowed.limits.max_nodes = 2;
        let token = issuer.issue_at(&allowed, NOW).unwrap();
        let mut owner = db
            .begin_authorized_read(&query, &issuer, &token, "main", symbols, policy(), || NOW)
            .unwrap();
        drop(db);
        let catalog = visible_catalog();
        let first: Result<(), QueryError> = owner.query_with_graph_output(
            &query,
            "RETURN 1 AS value",
            &GqlParameters::new(),
            |_, _, metadata| {
                for id in [VId(1), VId(3)] {
                    assert!(
                        metadata
                            .metadata(
                                ElementId::Vertex(id),
                                GraphElementMetadata::Properties,
                                &catalog
                            )?
                            .is_some()
                    );
                }
                Ok(())
            },
            |error| error,
        );
        first.unwrap();
        let second: Result<(), QueryError> = owner.query_with_graph_output(
            &query,
            "RETURN 2 AS value",
            &GqlParameters::new(),
            |_, _, metadata| {
                refused(
                    metadata.metadata(
                        ElementId::Vertex(VId(1)),
                        GraphElementMetadata::Labels,
                        &catalog,
                    ),
                    Error::LimitExceeded(LimitDimension::Nodes),
                );
                Ok(()) // A host cannot turn failed hydration into partial success.
            },
            |error| error,
        );
        refused(second, Error::LimitExceeded(LimitDimension::Nodes));
        assert!(!owner.is_active());
        assert_eq!(owner.completed_statements(), 1);
        assert!(pin.upgrade().is_none());
    });
}

#[test]
fn graph_output_metadata_phase_limits_and_missing_catalog_fail_terminally() {
    lab(0x6b22, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for missing_catalog in [false, true] {
            let mut owner = db
                .begin_authorized_read(
                    &query,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    GqlQueryPolicy::new(1000, 1000, 1_000_000, 128),
                    || NOW,
                )
                .unwrap();
            let catalog = if missing_catalog {
                ReverseSymbolCatalog::new()
            } else {
                visible_catalog()
            };
            let failed = Cell::new(false);
            let result: Result<(), QueryError> = owner.query_with_graph_output(
                &query, "RETURN 1 AS value", &GqlParameters::new(),
                |_, _, metadata| {
                    for _ in 0..256 {
                        let value = metadata.metadata(ElementId::Vertex(VId(1)), GraphElementMetadata::Properties, &catalog);
                        if let Err(error) = value {
                            if missing_catalog {
                                assert!(matches!(error, QueryError::Pattern(GqlQueryError::Source(
                                    fgdb::GqlError::Read(fgdb::ReadError::UnmappedProperty(PropertyKeyId(1)))
                                ))));
                            } else {
                                assert!(matches!(error, QueryError::Pattern(GqlQueryError::Evaluator(limit))
                                    if limit.dimension == GlaLimitDimension::ScratchEntries));
                            }
                            failed.set(true);
                            refused(metadata.metadata(ElementId::Vertex(VId(1)), GraphElementMetadata::Labels, &catalog),
                                Error::ExecutionStopped);
                            break;
                        }
                    }
                    assert!(failed.get());
                    Ok(())
                }, |error| error,
            );
            refused(result, Error::ExecutionStopped);
            assert!(!owner.is_active());
            assert_eq!(owner.completed_statements(), 0);
        }
    });
}

#[test]
fn session_graph_output_uses_the_selected_history_and_one_pinned_generation() {
    lab(0x6b30, |contexts| async move {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = database(&commit).await;
        let mut edges = WriteBatch::new(RelationId(1));
        edges.add_edge(
            EId(40),
            VId(1),
            VId(3),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(5))],
        );
        edges.add_edge(EId(41), VId(1), VId(2), vec![]);
        let historical = db.write(&commit, edges).await.unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        update.set_edge_property(EId(40), PropertyKeyId(1), Some(CanonicalScalar::Int(50)));
        let frontier = db.write(&commit, update).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&query, &issuer, &token, "main", symbols, policy(), || NOW)
            .unwrap();
        let mut later = WriteBatch::new(RelationId(1));
        later.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(700)));
        db.write(&commit, later).await.unwrap();
        drop(db);
        let catalog = visible_catalog();
        for (at, expected_vertex, expected_edge) in [(historical, 7, 5), (frontier, 70, 50)] {
            let text = format!(
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ {} RETURN n.p AS p ORDER BY p",
                at.0
            );
            let (result, selected) = session
                .query_with_graph_output(
                    &query,
                    &text,
                    &GqlParameters::new(),
                    |result, work, metadata| -> Result<_, QueryError> {
                        work(1)?;
                        for (element, expected) in [
                            (ElementId::Vertex(VId(1)), expected_vertex),
                            (ElementId::Edge(EId(40)), expected_edge),
                        ] {
                            assert_eq!(
                                metadata.metadata(
                                    element,
                                    GraphElementMetadata::Properties,
                                    &catalog
                                )?,
                                GraphValue::map(vec![(
                                    "p".into(),
                                    GraphValue::Scalar(CanonicalScalar::Int(expected))
                                )])
                            );
                        }
                        assert_eq!(
                            metadata.metadata(
                                ElementId::Vertex(VId(1)),
                                GraphElementMetadata::Labels,
                                &catalog
                            )?,
                            Some(GraphValue::List(
                                vec![GraphValue::Scalar(
                                    CanonicalScalar::ucs_basic_text("L").unwrap()
                                )]
                                .into_boxed_slice()
                            ))
                        );
                        assert_eq!(
                            metadata.metadata(
                                ElementId::Edge(EId(40)),
                                GraphElementMetadata::EndNode,
                                &catalog
                            )?,
                            Some(GraphValue::Vertex(VId(3)))
                        );
                        for hidden in [ElementId::Vertex(VId(2)), ElementId::Edge(EId(41))] {
                            assert_eq!(
                                metadata.metadata(
                                    hidden,
                                    GraphElementMetadata::Properties,
                                    &catalog
                                )?,
                                None
                            );
                        }
                        Ok(result)
                    },
                    |error| error,
                )
                .unwrap();
            assert_eq!(selected, at);
            let mut expected = [expected_vertex, 19];
            expected.sort();
            assert_eq!(result, rows(&expected));
        }
        assert_eq!(
            session.query(&query, READ, &GqlParameters::new()).unwrap(),
            rows(&[19, 70])
        );
    });
}

#[test]
fn session_graph_output_shares_nodes_work_and_user_rows_with_the_statement() {
    lab(0x6b31, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let catalog = visible_catalog();
        for dimension in [
            LimitDimension::Nodes,
            LimitDimension::Work,
            LimitDimension::Rows,
        ] {
            let mut allowed = grant();
            if dimension == LimitDimension::Nodes {
                allowed.limits.max_nodes = 2;
            }
            if dimension == LimitDimension::Rows {
                allowed.limits.max_rows = 1;
            }
            let token = issuer.issue_at(&allowed, NOW).unwrap();
            let mut session = db
                .authorized_read_session(&query, &issuer, &token, "main", symbols, policy(), || NOW)
                .unwrap();
            let entered = Cell::new(false);
            let result: Result<(_, _), QueryError> = session.query_with_graph_output(
                &query,
                READ,
                &GqlParameters::new(),
                |_, work, metadata| {
                    entered.set(true);
                    if dimension == LimitDimension::Nodes {
                        // Query admission already spent both visible nodes;
                        // hydration must not replenish that signed allowance.
                        let _ = metadata.metadata(
                            ElementId::Vertex(VId(1)),
                            GraphElementMetadata::Properties,
                            &catalog,
                        );
                    } else if dimension == LimitDimension::Work {
                        let _ = work(allowed.limits.max_work + 1);
                    }
                    Ok(()) // Swallowing a meter refusal cannot release output.
                },
                |error| error,
            );
            refused(result, Error::LimitExceeded(dimension));
            assert_eq!(entered.get(), dimension != LimitDimension::Rows);
            // A new statement deliberately has its own allowance, unlike an
            // explicit transaction; this does not renew the failed execution.
            assert!(!session.is_closed());
            session
                .query(&query, "RETURN 1 AS value", &GqlParameters::new())
                .unwrap();
        }
    });
}

#[test]
fn session_graph_output_cannot_hide_a_metadata_failure_behind_a_host_error_type() {
    lab(0x6b32, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let pin = db.pinned_read_view().unwrap().downgrade();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&query, &issuer, &token, "main", symbols, policy(), || NOW)
            .unwrap();
        drop(db);
        let result: Result<((), _), &'static str> = session.query_with_graph_output(
            &query,
            "RETURN 1 AS value",
            &GqlParameters::new(),
            |_, _, metadata| {
                assert!(
                    metadata
                        .metadata(
                            ElementId::Vertex(VId(1)),
                            GraphElementMetadata::Properties,
                            &ReverseSymbolCatalog::new()
                        )
                        .is_err()
                );
                Ok(())
            },
            |_| "native refusal",
        );
        assert_eq!(result, Err("native refusal"));
        assert!(session.is_closed());
        assert!(pin.upgrade().is_none());
    });
}

#[test]
fn session_graph_output_rechecks_live_authority_after_successful_hydration() {
    lab(0x6b33, |contexts| async move {
        let query = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let now = Cell::new(NOW);
        let mut session = db
            .authorized_read_session(&query, &issuer, &token, "main", symbols, policy(), || {
                now.get()
            })
            .unwrap();
        let result: Result<((), _), QueryError> = session.query_with_graph_output(
            &query,
            "RETURN 1 AS value",
            &GqlParameters::new(),
            |_, _, metadata| {
                assert!(
                    metadata
                        .metadata(
                            ElementId::Vertex(VId(1)),
                            GraphElementMetadata::Properties,
                            &visible_catalog()
                        )?
                        .is_some()
                );
                now.set(1001);
                Ok(())
            },
            |error| error,
        );
        refused(result, Error::Expired);
        assert!(session.is_closed());
    });
}
