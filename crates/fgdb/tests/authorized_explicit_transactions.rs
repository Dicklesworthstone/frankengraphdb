//! Explicit native ownership shares one masked overlay, allowance and commit.
//! Statements may release the database borrow without publishing their prefix.

#![recursion_limit = "256"]

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphElementMetadata, GraphSymbol, GraphSymbolKind,
    PreparedGraphSet, PreparedGraphText, PreparedGraphWriteChain, PreparedGraphWriteChainText,
    ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Rights, Scope};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x71; 32]);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NOW: u64 = 100;

fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()));
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x70; 32], NS, [0x72; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0x711), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rights: Rights) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P]),
        rights,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 1000,
    }
}

fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(100_000, 100, 1_000_000, 100_000),
        100,
        100,
    )
    .with_mutation_limit(100)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn chain(text: &str) -> PreparedGraphWriteChain {
    PreparedGraphWriteChainText::prepare_statement(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn read(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}

fn integer(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn names() -> ReverseSymbolCatalog {
    let mut names = ReverseSymbolCatalog::new();
    names.insert_label(L, "Visible");
    names.insert_property(P, "p");
    names.insert_relation(R, "R");
    names
}

fn has_error(
    error: &(dyn core::error::Error + 'static),
    predicate: impl Fn(&WriteTxnError) -> bool,
) -> bool {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(error) = error.downcast_ref::<WriteTxnError>() {
            return predicate(error);
        }
        cause = error.source();
    }
    false
}

#[test]
fn ordinary_statements_and_reads_share_unpublished_effects_and_one_commit() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let before = db.frontier().unwrap();
        let mut owner = {
            let begin_context = commit.clone();
            db.begin_authorized_write(
                txn,
                &begin_context,
                &issuer,
                &token,
                "main",
                policy(),
                10,
                || NOW,
            )
            .unwrap()
        };
        assert_eq!(owner.basis(), before);
        assert_eq!(txn.outstanding_obligations(), 1);
        owner
            .stage_chain(
                &mut db,
                query,
                &chain("CREATE (n:Visible {p:1})"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        let set = owner
            .stage_chain(
                &mut db,
                query,
                &chain("MATCH (n:Visible) SET n.p=2 RETURN n.p AS p"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert_eq!(set.value[0].values(), &[integer(2)]);
        owner
            .stage_chain(
                &mut db,
                query,
                &chain("MERGE (n:Visible {p:2}) ON MATCH SET n.p=3 RETURN n"),
                |rows, _, output| {
                    let [GraphValue::Vertex(vertex)] = rows[0].values() else {
                        panic!("vertex result")
                    };
                    assert_eq!(
                        output.metadata(
                            ElementId::Vertex(*vertex),
                            GraphElementMetadata::Properties,
                            &names(),
                        )?,
                        Some(GraphValue::map(vec![("p".into(), integer(3))]).unwrap())
                    );
                    Ok(())
                },
            )
            .unwrap();
        let result = owner
            .read(
                &mut db,
                query,
                &read("MATCH (n:Visible) RETURN n.p AS p"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].values(), &[integer(3)]);
        let statistics = owner.statistics().unwrap();
        assert_eq!(statistics.completed_statements, 4);
        assert_eq!(statistics.created_vertices, 1);
        assert_eq!(statistics.selection.result_rows, 3);
        assert!(statistics.mutation_effects >= 2);
        assert!(db.vertices().unwrap().is_empty());
        assert!(matches!(owner.commit(&mut db).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } if commit_seq.0 == before.0 + 1));
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(
            db.vertices().unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(3).into())]
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn rollback_drop_and_delete_release_the_whole_prefix() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let issuer = authority();
        // Native vertex deletion must see all incidence and fields, including
        // when its target was created earlier in this explicit transaction.
        let mut scope = grant(Rights::ReadWrite);
        scope.labels = Scope::All;
        scope.relations = Scope::All;
        scope.properties = Scope::All;
        let token = issuer.issue_at(&scope, NOW).unwrap();
        let before = db.frontier().unwrap();
        for explicit in [false, true] {
            let mut owner = db
                .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:1})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("MATCH (n:Visible) DELETE n"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            let result = owner
                .read(
                    &mut db,
                    query,
                    &read("MATCH (n:Visible) RETURN n"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            assert!(result.value.is_empty());
            if explicit {
                owner.rollback();
            } else {
                drop(owner);
            }
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn overlay_reads_mask_hidden_records_and_fields_and_require_read_rights() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![L],
            vec![
                (P, CanonicalScalar::Int(7)),
                (SECRET, CanonicalScalar::Int(91)),
            ],
        );
        seed.create_vertex(VId(2), vec![HIDDEN], vec![(P, CanonicalScalar::Int(92))]);
        db.write(commit, seed).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let mut owner = db
            .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
            .unwrap();
        owner
            .stage_chain(
                &mut db,
                query,
                &chain("MATCH (n:Visible) SET n.p=8"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        let result = owner
            .read(
                &mut db,
                query,
                &read("MATCH (n) RETURN n"),
                |rows, _, output| {
                    assert_eq!(rows.len(), 1);
                    assert_eq!(
                        output.metadata(
                            ElementId::Vertex(VId(1)),
                            GraphElementMetadata::Properties,
                            &names(),
                        )?,
                        Some(GraphValue::map(vec![("p".into(), integer(8))]).unwrap())
                    );
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(result.value[0].values(), &[GraphValue::Vertex(VId(1))]);
        owner.commit(&mut db).await.unwrap();
        let visible = db
            .vertices()
            .unwrap()
            .into_iter()
            .find(|v| v.vid == VId(1))
            .unwrap();
        assert_eq!(
            visible.props,
            vec![
                (P, CanonicalScalar::Int(8).into()),
                (SECRET, CanonicalScalar::Int(91).into())
            ]
        );

        for returning in [false, true] {
            let token = issuer.issue_at(&grant(Rights::Write), NOW).unwrap();
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let mut owner = db
                .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:4})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            let mut reached = false;
            let error = if returning {
                owner
                    .stage_chain(
                        &mut db,
                        query,
                        &chain("CREATE (n:Visible {p:5}) RETURN n"),
                        |_, _, _| {
                            reached = true;
                            Ok(())
                        },
                    )
                    .unwrap_err()
            } else {
                owner
                    .read(
                        &mut db,
                        query,
                        &read("MATCH (n:Visible) RETURN n"),
                        |_, _, _| {
                            reached = true;
                            Ok(())
                        },
                    )
                    .unwrap_err()
            };
            assert!(!reached);
            assert!(has_error(&error, |e| matches!(
                e,
                WriteTxnError::Authorization(Error::PermissionDenied)
            )));
            assert!(!owner.is_active());
            assert!(matches!(
                owner.commit(&mut db).await,
                Err(WriteTxnError::Finished)
            ));
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn native_rows_signed_rows_statements_and_creations_are_cumulative() {
    run(async |commit, query, txn| {
        for dimension in 0..4 {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let issuer = authority();
            let mut grant = grant(Rights::ReadWrite);
            let mut allowance = policy();
            let max_statements = if dimension == 2 { 1 } else { 10 };
            if dimension == 0 {
                allowance.query = GqlQueryPolicy::new(100_000, 1, 1_000_000, 100_000);
            }
            if dimension == 1 {
                grant.limits.max_rows = 1;
            }
            if dimension == 3 {
                allowance.max_vertices = 1;
            }
            let token = issuer.issue_at(&grant, NOW).unwrap();
            let before = db.frontier().unwrap();
            let mut owner = db
                .begin_authorized_write(
                    txn,
                    commit,
                    &issuer,
                    &token,
                    "main",
                    allowance,
                    max_statements,
                    || NOW,
                )
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:1}) RETURN n"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            let refused = if dimension == 3 {
                owner
                    .stage_chain(
                        &mut db,
                        query,
                        &chain("CREATE (n:Visible {p:2})"),
                        |_, _, _| Ok(()),
                    )
                    .is_err()
            } else {
                owner
                    .read(
                        &mut db,
                        query,
                        &read("MATCH (n:Visible) RETURN n"),
                        |_, _, _| Ok(()),
                    )
                    .is_err()
            };
            assert!(refused, "dimension {dimension}");
            assert!(!owner.is_active());
            assert!(matches!(
                owner.commit(&mut db).await,
                Err(WriteTxnError::Finished)
            ));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn native_evaluator_and_mutation_limits_accumulate_across_statements() {
    run(async |commit, query, txn| {
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut measured = db
            .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
            .unwrap();
        measured
            .stage_chain(
                &mut db,
                query,
                &chain("CREATE (n:Visible {p:7})"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        measured
            .read(
                &mut db,
                query,
                &read("MATCH (n:Visible) RETURN n.p AS p"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        let costs = measured.statistics().unwrap().evaluator;
        measured.rollback();
        assert!(costs.work_units > 1 && costs.scratch_entries > 1);
        for scratch in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let mut allowance = policy();
            allowance.query = GqlQueryPolicy::new(
                100_000,
                100,
                costs.work_units - u64::from(!scratch),
                costs.scratch_entries - u64::from(scratch),
            );
            let mut owner = db
                .begin_authorized_write(txn, commit, &issuer, &token, "main", allowance, 10, || NOW)
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:7})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            assert!(
                owner
                    .read(
                        &mut db,
                        query,
                        &read("MATCH (n:Visible) RETURN n.p AS p"),
                        |_, _, _| Ok(())
                    )
                    .is_err()
            );
            assert!(!owner.is_active());
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut owner = db
            .begin_authorized_write(
                txn,
                commit,
                &issuer,
                &token,
                "main",
                policy().with_mutation_limit(1),
                10,
                || NOW,
            )
            .unwrap();
        owner
            .stage_chain(
                &mut db,
                query,
                &chain("CREATE (n:Visible {p:1})"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        owner
            .stage_chain(
                &mut db,
                query,
                &chain("MATCH (n:Visible) SET n.p=2"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert!(
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("MATCH (n:Visible) SET n.p=3"),
                    |_, _, _| Ok(())
                )
                .is_err()
        );
        assert!(!owner.is_active());
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn signed_work_does_not_restart_at_statement_boundaries() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let mut owner = db
            .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
            .unwrap();
        for _ in 0..2 {
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:1})"),
                    |_, work, _| work(400_000),
                )
                .unwrap();
        }
        assert!(
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:2})"),
                    |_, work, _| work(400_000)
                )
                .is_err()
        );
        assert!(!owner.is_active());
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn output_errors_swallowed_meter_failures_and_unwinds_abort_earlier_statements() {
    run(async |commit, query, txn| {
        for failure in 0..4 {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
            let before = db.frontier().unwrap();
            let mut owner = db
                .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:1})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                owner.stage_chain(
                    &mut db,
                    query,
                    &chain("MATCH (n:Visible) SET n.p=2 RETURN n"),
                    |rows, work, output| {
                        match failure {
                            0 => return Err(WriteTxnError::AuthorizedMutationRefused),
                            1 => {
                                assert!(work(u64::MAX).is_err());
                            }
                            2 => {
                                let [GraphValue::Vertex(vertex)] = rows[0].values() else {
                                    panic!("vertex result")
                                };
                                assert!(
                                    output
                                        .metadata(
                                            ElementId::Vertex(*vertex),
                                            GraphElementMetadata::Properties,
                                            &ReverseSymbolCatalog::new()
                                        )
                                        .is_err()
                                );
                            }
                            _ => panic!("host output failed"),
                        }
                        Ok(())
                    },
                )
            }));
            if failure == 3 {
                assert!(attempt.is_err());
            } else {
                assert!(attempt.unwrap().is_err());
            }
            assert!(!owner.is_active());
            assert!(matches!(
                owner.commit(&mut db).await,
                Err(WriteTxnError::Finished)
            ));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn foreign_handle_and_advanced_basis_refuse_before_continuing_the_prefix() {
    run(async |commit, query, txn| {
        for foreign in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let mut other = Database::open_memory(commit, keys()).await.unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
            let mut owner = db
                .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:1})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            if !foreign {
                let mut update = WriteBatch::new(R);
                update.create_vertex(VId(999), vec![L], vec![(P, CanonicalScalar::Int(9))]);
                db.write(commit, update).await.unwrap();
            }
            let mut reached = false;
            let target = if foreign { &mut other } else { &mut db };
            let error = owner
                .read(
                    target,
                    query,
                    &read("MATCH (n:Visible) RETURN n"),
                    |_, _, _| {
                        reached = true;
                        Ok(())
                    },
                )
                .unwrap_err();
            assert!(!reached);
            assert!(has_error(&error, |e| if foreign {
                matches!(e, WriteTxnError::WrongDatabase)
            } else {
                matches!(e, WriteTxnError::SnapshotAdvanced { .. })
            }));
            assert!(!owner.is_active());
            assert!(other.vertices().unwrap().is_empty());
            assert_eq!(db.vertices().unwrap().len(), usize::from(!foreign));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn retirement_and_expiry_between_calls_cannot_publish_an_earlier_prefix() {
    run(async |commit, query, txn| {
        for retire in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
            let now = Arc::new(AtomicU64::new(NOW));
            let clock = Arc::clone(&now);
            let before = db.frontier().unwrap();
            let mut owner = db
                .begin_authorized_write(
                    txn,
                    commit,
                    &issuer,
                    &token,
                    "main",
                    policy(),
                    10,
                    move || clock.load(Ordering::Relaxed),
                )
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (n:Visible {p:1})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            if retire {
                issuer.retire();
            } else {
                now.store(1000, Ordering::Relaxed);
            }
            let error = owner.commit(&mut db).await.unwrap_err();
            assert!(matches!(error, WriteTxnError::Authorization(_)));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn cancellation_of_a_later_statement_aborts_the_entire_owner() {
    use fgdb_types::context::SimulationCheckpointProbe;
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let mut owner = db
            .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
            .unwrap();
        owner
            .stage_chain(
                &mut db,
                query,
                &chain("CREATE (n:Visible {p:1})"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        let interrupted =
            query.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
        let mut reached = false;
        let error = owner
            .read(
                &mut db,
                &interrupted,
                &read("MATCH (n:Visible) RETURN n"),
                |_, _, _| {
                    reached = true;
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(has_error(&error, |e| matches!(
            e,
            WriteTxnError::Interrupted(_)
        )));
        assert!(!reached);
        assert!(!owner.is_active());
        assert!(matches!(
            owner.commit(&mut db).await,
            Err(WriteTxnError::Finished)
        ));
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
