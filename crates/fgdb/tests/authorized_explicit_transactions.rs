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

struct Catalog;

impl fgdb_gql::GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = names();
        catalog.insert_label(HIDDEN, "Hidden");
        catalog.insert_property(SECRET, "secret");
        Some(catalog)
    }
}

fn aggregate(text: &str) -> fgdb_gql::PreparedGraphSetAggregate {
    let params = GqlParameters::new();
    match fgdb::PreparedNativeRead::prepare(text, &params, Catalog).unwrap() {
        fgdb::PreparedNativeRead::Aggregate(query) => {
            fgdb_gql::PreparedGraphSetAggregate::from_complete_input(
                query.bind_parameters(&params).unwrap(),
            )
            .unwrap()
        }
        fgdb::PreparedNativeRead::PipelineAggregate(query) if query.requires_relational_input() => {
            query.bind_relation_parameters(&params).unwrap()
        }
        fgdb::PreparedNativeRead::PipelineAggregate(query) => {
            fgdb_gql::PreparedGraphSetAggregate::from_complete_input(
                query.bind_parameters(&params).unwrap(),
            )
            .unwrap()
        }
        _ => panic!("an exact aggregate definition"),
    }
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
fn retirement_and_expiry_during_output_admission_abort_before_returning_rows() {
    run(async |commit, query, txn| {
        for writing in [false, true] {
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
                let mut admitted = false;
                let admit =
                    |rows: &[fgdb_gql::algebra::GraphValueRow],
                     _: &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
                     output: &mut dyn fgdb::AuthorizedWriteOutput| {
                        let [GraphValue::Vertex(vertex)] = rows[0].values() else {
                            panic!("vertex result")
                        };
                        assert_eq!(
                            output.metadata(
                                ElementId::Vertex(*vertex),
                                GraphElementMetadata::Properties,
                                &names(),
                            )?,
                            Some(
                                GraphValue::map(vec![(
                                    "p".into(),
                                    integer(if writing { 2 } else { 1 }),
                                )])
                                .unwrap()
                            )
                        );
                        admitted = true;
                        // Hydration succeeded, but authority ceased to be
                        // live before this successful host callback returned.
                        if retire {
                            issuer.retire();
                        } else {
                            now.store(1000, Ordering::Relaxed);
                        }
                        Ok(())
                    };
                let error = if writing {
                    owner.stage_chain(
                        &mut db,
                        query,
                        &chain("MATCH (n:Visible) SET n.p=2 RETURN n"),
                        admit,
                    )
                } else {
                    owner.read(&mut db, query, &read("MATCH (n:Visible) RETURN n"), admit)
                }
                .unwrap_err();
                assert!(admitted);
                assert!(has_error(&error, |error| match error {
                    WriteTxnError::Authorization(Error::AuthorityRetired) => retire,
                    WriteTxnError::Authorization(Error::Expired) => !retire,
                    _ => false,
                }));
                assert!(!owner.is_active());
                assert!(owner.statistics().is_none());
                assert!(matches!(
                    owner.commit(&mut db).await,
                    Err(WriteTxnError::Finished)
                ));
                assert!(db.vertices().unwrap().is_empty());
                assert_eq!(db.frontier().unwrap(), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn cancellation_after_output_admission_aborts_reads_writes_and_the_retained_prefix() {
    use fgdb_types::context::SimulationCheckpointProbe;
    run(async |commit, query, txn| {
        for writing in [false, true] {
            let mut output_checkpoint = None;
            for interrupt in [false, true] {
                let mut db = Database::open_memory(commit, keys()).await.unwrap();
                let issuer = authority();
                let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
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
                let stop = if interrupt {
                    Some(output_checkpoint.expect("the successful run observes hydration") + 1)
                } else {
                    None
                };
                let probe = Arc::new(SimulationCheckpointProbe::new(stop));
                let query = query.with_checkpoint_probe(Arc::clone(&probe));
                let mut admitted = false;
                let mut calls_at_output = 0;
                let admit =
                    |rows: &[fgdb_gql::algebra::GraphValueRow],
                     _: &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
                     output: &mut dyn fgdb::AuthorizedWriteOutput| {
                        let [GraphValue::Vertex(vertex)] = rows[0].values() else {
                            panic!("vertex result")
                        };
                        assert!(
                            output
                                .metadata(
                                    ElementId::Vertex(*vertex),
                                    GraphElementMetadata::Properties,
                                    &names(),
                                )?
                                .is_some()
                        );
                        admitted = true;
                        calls_at_output = probe.calls();
                        Ok(())
                    };
                let result = if writing {
                    owner.stage_chain(
                        &mut db,
                        &query,
                        &chain("MATCH (n:Visible) SET n.p=2 RETURN n"),
                        admit,
                    )
                } else {
                    owner.read(&mut db, &query, &read("MATCH (n:Visible) RETURN n"), admit)
                };
                assert!(admitted, "interruption must follow successful hydration");
                if interrupt {
                    assert_eq!(Some(calls_at_output), output_checkpoint);
                    let error = result.unwrap_err();
                    assert!(has_error(&error, |error| matches!(
                        error,
                        WriteTxnError::Interrupted(_)
                    )));
                    assert!(!owner.is_active());
                    assert!(owner.statistics().is_none());
                    assert!(matches!(
                        owner.commit(&mut db).await,
                        Err(WriteTxnError::Finished)
                    ));
                } else {
                    assert_eq!(result.unwrap().value.len(), 1);
                    assert!(
                        probe.calls() > calls_at_output,
                        "output admission must be followed by a live cancellation check"
                    );
                    output_checkpoint = Some(calls_at_output);
                    owner.rollback();
                }
                assert!(db.vertices().unwrap().is_empty());
                assert_eq!(db.frontier().unwrap(), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
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

#[test]
fn preparation_uses_fresh_host_time_and_the_original_scope_without_publishing() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&calls);
        db.set_statement_clock(Some(fgdb::StatementClock::new(move |_| {
            let next = observed.fetch_add(1, Ordering::SeqCst) + 1;
            Some(
                fgdb_types::CanonicalTimestamp::offset_only(i128::from(next) * 1_000_000, 0)
                    .unwrap(),
            )
        })));
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let before = db.frontier().unwrap();
        let mut owner = db
            .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
            .unwrap();
        let text = "CREATE (n:Visible {p:datetime().epochMillis})";
        let untrusted = GqlParameters::new()
            .with_statement_time(
                fgdb_types::CanonicalTimestamp::offset_only(99_000_000, 0).unwrap(),
            )
            .unwrap();
        for expected in [1, 2] {
            let prepared = owner
                .prepare_statement(&db, query, text, &untrusted, |params, scope, work| {
                    assert!(scope.allows_label(L));
                    assert!(!scope.allows_label(HIDDEN));
                    assert!(scope.allows_property(P));
                    assert!(!scope.allows_property(SECRET));
                    assert_eq!(
                        params.statement_time(),
                        Some(
                            &fgdb_types::CanonicalTimestamp::offset_only(expected * 1_000_000, 0)
                                .unwrap(),
                        )
                    );
                    let declarations: Vec<_> = params.parameter_types().collect();
                    work(1)?;
                    let template =
                        PreparedGraphWriteChainText::prepare_statement_with_parameter_types(
                            text,
                            R,
                            &declarations,
                            |kind, name: &str| {
                                work(1).ok()?;
                                symbols(kind, name).filter(|symbol| match symbol {
                                    GraphSymbol::Label(id) => scope.allows_label(*id),
                                    GraphSymbol::Relation(id) => scope.allows_relation(*id),
                                    GraphSymbol::Property(id) => scope.allows_property(*id),
                                })
                            },
                        )
                        .map_err(|_| WriteTxnError::AuthorizedMutationRefused)?;
                    work(1)?;
                    template
                        .bind_parameters(params)
                        .map_err(|_| WriteTxnError::AuthorizedMutationRefused)
                })
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), expected as u64);
            assert_eq!(db.frontier().unwrap(), before);
            owner
                .stage_chain(&mut db, query, &prepared, |_, _, _| Ok(()))
                .unwrap();
        }
        assert_eq!(owner.statistics().unwrap().completed_statements, 2);
        assert!(db.vertices().unwrap().is_empty());
        owner.commit(&mut db).await.unwrap();
        let mut values: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|row| row.props)
            .collect();
        values.sort();
        assert_eq!(
            values,
            vec![
                vec![(P, CanonicalScalar::Int(1).into())],
                vec![(P, CanonicalScalar::Int(2).into())],
            ]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn original_text_and_compiler_work_share_the_transaction_allowance() {
    run(async |commit, query, txn| {
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
        // Original ingress bytes are reserved before any parser or catalog
        // callback, even when most bytes are an ignorable comment.
        let text = format!("/*{}*/ RETURN 1", "x".repeat(300_000));
        for _ in 0..2 {
            owner
                .prepare_statement(&db, query, &text, &GqlParameters::new(), |_, _, work| {
                    work(100_000)
                })
                .unwrap();
        }
        let mut reached = false;
        let refused =
            owner.prepare_statement(&db, query, &text, &GqlParameters::new(), |_, _, _| {
                reached = true;
                Ok(())
            });
        assert!(matches!(refused, Err(WriteTxnError::Authorization(_))));
        assert!(!reached);
        assert!(!owner.is_active());
        assert!(matches!(
            owner.commit(&mut db).await,
            Err(WriteTxnError::Finished)
        ));
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn preparation_errors_swallowed_meters_and_unwinds_abort_the_prefix() {
    run(async |commit, query, txn| {
        for fault in 0..5 {
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                owner.prepare_statement(
                    &db,
                    query,
                    "RETURN 1",
                    &GqlParameters::new(),
                    |_, _, work| {
                        match fault {
                            0 => return Err(WriteTxnError::AuthorizedMutationRefused),
                            1 => {
                                assert!(work(u64::MAX).is_err());
                            }
                            2 => issuer.retire(),
                            3 => {
                                assert!(work(u64::MAX).is_err());
                                return Err(WriteTxnError::AuthorizedMutationRefused);
                            }
                            _ => panic!("compiler callback failed"),
                        }
                        Ok(())
                    },
                )
            }));
            if fault == 4 {
                assert!(result.is_err());
            } else {
                let error = result.unwrap().unwrap_err();
                if matches!(fault, 1 | 3) {
                    assert!(matches!(
                        error,
                        WriteTxnError::Authorization(fgdb_warden::Error::LimitExceeded(
                            fgdb_warden::LimitDimension::Work
                        ))
                    ));
                }
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
fn preparation_refuses_zero_statements_forbidden_identity_and_foreign_handles_before_callbacks() {
    run(async |commit, query, txn| {
        for fault in 0..3 {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let foreign = Database::open_memory(commit, keys()).await.unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
            let mut owner = db
                .begin_authorized_write(
                    txn,
                    commit,
                    &issuer,
                    &token,
                    "main",
                    policy(),
                    if fault == 0 { 0 } else { 10 },
                    || NOW,
                )
                .unwrap();
            let text = if fault == 1 {
                "MATCH (n) RETURN elementId(n)"
            } else {
                "RETURN 1"
            };
            let database = if fault == 2 { &foreign } else { &db };
            let mut reached = false;
            let error = owner
                .prepare_statement(database, query, text, &GqlParameters::new(), |_, _, _| {
                    reached = true;
                    Ok(())
                })
                .unwrap_err();
            assert!(!reached);
            assert!(match fault {
                0 => matches!(error, WriteTxnError::StatementLimit { limit: 0 }),
                1 => matches!(error, WriteTxnError::Authorization(Error::ScopeDenied)),
                _ => matches!(error, WriteTxnError::WrongDatabase),
            });
            assert!(!owner.is_active());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn exact_aggregate_reads_include_staged_rows_without_narrowing_wide_sums_or_means() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let before = db.frontier().unwrap();
        let mut owner = db
            .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
            .unwrap();
        let count = aggregate("MATCH (n:Visible) RETURN count(*) AS c");
        let empty = owner
            .read_aggregate(&mut db, query, &count, |_, _, _| Ok(()))
            .unwrap();
        assert_eq!(empty.value.len(), 1);
        assert_eq!(empty.value[0].values()[0].as_count(), Some(0));
        owner.stage_chain(&mut db, query,
            &chain("CREATE (:Visible {p:1}), (:Visible {p:2}), (:Visible {p:9223372036854775807}), (:Visible {p:9223372036854775807})"),
            |_, _, _| Ok(())).unwrap();
        let exact =
            aggregate("MATCH (n:Visible) RETURN count(*) AS c, sum(n.p) AS s, avg(n.p) AS a");
        let result = owner
            .read_aggregate(&mut db, query, &exact, |rows, _, _| {
                assert_eq!(rows.len(), 1);
                Ok(())
            })
            .unwrap();
        let cells = result.value[0].values();
        let sum = i128::from(i64::MAX) * 2 + 3;
        assert_eq!(cells[0].as_count(), Some(4));
        assert_eq!(cells[1].as_integer(), Some(sum));
        let average = cells[2].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (sum, 4));
        assert_eq!(owner.statistics().unwrap().completed_statements, 3);
        assert_eq!(owner.statistics().unwrap().selection.result_rows, 2);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        owner.commit(&mut db).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 4);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn aggregate_sources_preserve_masking_pipeline_filters_and_exact_staged_hydration() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(99), vec![HIDDEN], vec![(P, CanonicalScalar::Int(900))]);
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
                &chain("CREATE (:Visible {p:1}), (:Visible {p:2}), (:Visible {p:3})"),
                |_, _, _| Ok(()),
            )
            .unwrap();
        let query_plan = aggregate(
            "MATCH (n) WITH n.p AS p WHERE p >= 2 RETURN count(*) AS c, sum(p) AS s, avg(p) AS a",
        );
        let result = owner
            .read_aggregate(&mut db, query, &query_plan, |_, _, _| Ok(()))
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].values()[0].as_count(), Some(2));
        assert_eq!(result.value[0].values()[1].as_integer(), Some(5));
        let mean = result.value[0].values()[2].as_average().unwrap();
        assert_eq!((mean.numerator(), mean.denominator()), (5, 2));
        let collect = aggregate("MATCH (n) RETURN collect(n) AS nodes");
        owner
            .read_aggregate(&mut db, query, &collect, |rows, _, output| {
                let Some(GraphValue::List(nodes)) = rows[0].values()[0].as_value() else {
                    panic!("the native collection domain")
                };
                assert_eq!(nodes.len(), 3);
                let mut properties = Vec::new();
                for node in nodes.iter() {
                    let GraphValue::Vertex(vertex) = node else {
                        panic!("a collected vertex")
                    };
                    properties.push(
                        output
                            .metadata(
                                ElementId::Vertex(*vertex),
                                GraphElementMetadata::Properties,
                                &names(),
                            )?
                            .unwrap(),
                    );
                }
                properties.sort();
                assert_eq!(
                    properties,
                    [1, 2, 3]
                        .map(|value| GraphValue::map(vec![("p".into(), integer(value))]).unwrap())
                );
                Ok(())
            })
            .unwrap();
        owner.rollback();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn aggregate_output_refusals_and_cumulative_row_limits_abort_the_entire_prefix() {
    run(async |commit, query, txn| {
        for refusal in 0..3 {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
            let mut allowance = policy();
            if refusal == 0 {
                allowance.query = GqlQueryPolicy::new(100_000, 1, 1_000_000, 100_000);
            }
            let mut owner = db
                .begin_authorized_write(txn, commit, &issuer, &token, "main", allowance, 10, || NOW)
                .unwrap();
            owner
                .stage_chain(
                    &mut db,
                    query,
                    &chain("CREATE (:Visible {p:1})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            let count = aggregate("MATCH (n:Visible) RETURN count(*) AS c");
            owner
                .read_aggregate(&mut db, query, &count, |_, _, _| Ok(()))
                .unwrap();
            let failed = owner.read_aggregate(&mut db, query, &count, |_, work, _| {
                match refusal {
                    1 => return Err(WriteTxnError::AuthorizedMutationRefused),
                    2 => {
                        assert!(work(u64::MAX).is_err());
                    }
                    _ => panic!("the second group must not pass cumulative row admission"),
                }
                Ok(())
            });
            assert!(failed.is_err());
            assert!(!owner.is_active());
            assert!(matches!(
                owner.commit(&mut db).await,
                Err(WriteTxnError::Finished)
            ));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn aggregate_output_rechecks_authority_and_cancellation_before_returning_rows() {
    use fgdb_types::context::SimulationCheckpointProbe;
    run(async |commit, query, txn| {
        let mut output_checkpoint = None;
        for fault in 0..4 {
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
                    &chain("CREATE (:Visible {p:1})"),
                    |_, _, _| Ok(()),
                )
                .unwrap();
            let stop =
                (fault == 3).then(|| output_checkpoint.expect("the baseline observes output") + 1);
            let probe = Arc::new(SimulationCheckpointProbe::new(stop));
            let query = query.with_checkpoint_probe(Arc::clone(&probe));
            let mut observed = None;
            let result = owner.read_aggregate(
                &mut db,
                &query,
                &aggregate("MATCH (n:Visible) RETURN count(*) AS c"),
                |rows, _, _| {
                    assert_eq!(rows[0].values()[0].as_count(), Some(1));
                    observed = Some(probe.calls());
                    match fault {
                        1 => issuer.retire(),
                        2 => now.store(1000, Ordering::Relaxed),
                        _ => {}
                    }
                    Ok(())
                },
            );
            assert!(observed.is_some(), "the aggregate reached output admission");
            if fault == 0 {
                result.unwrap();
                output_checkpoint = observed;
                assert!(probe.calls() > observed.unwrap());
                owner.rollback();
            } else {
                let error = result.unwrap_err();
                assert!(has_error(&error, |error| match error {
                    WriteTxnError::Authorization(Error::AuthorityRetired) => fault == 1,
                    WriteTxnError::Authorization(Error::Expired) => fault == 2,
                    WriteTxnError::Interrupted(_) => fault == 3,
                    _ => false,
                }));
                if fault == 3 {
                    assert_eq!(observed, output_checkpoint);
                }
                assert!(!owner.is_active());
                assert!(matches!(
                    owner.commit(&mut db).await,
                    Err(WriteTxnError::Finished)
                ));
            }
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn catalog_calls_use_the_same_scope_in_explicit_rows_and_aggregate_sources() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite), NOW).unwrap();
        let mut owner = db
            .begin_authorized_write(txn, commit, &issuer, &token, "main", policy(), 10, || NOW)
            .unwrap();
        let statement = fgdb_gql::PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            "CALL db.labels()",
            &[],
            &mut Catalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let rows = owner
            .read(&mut db, query, &statement, |_, _, _| Ok(()))
            .unwrap();
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text("Visible").unwrap()
            )]
        );
        assert_eq!(rows.rows.snapshot_records, 0);
        let count = aggregate("CALL db.labels() YIELD label RETURN count(*) AS c");
        let result = owner
            .read_aggregate(&mut db, query, &count, |_, _, _| Ok(()))
            .unwrap();
        assert_eq!(result.value[0].values()[0].as_count(), Some(1));
        owner.rollback();
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn canonical_parameter_bytes_accumulate_and_refusal_aborts_the_unpublished_prefix() {
    run(async |commit, query, txn| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let clock_calls = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&clock_calls);
        db.set_statement_clock(Some(fgdb::StatementClock::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Some(fgdb_types::CanonicalTimestamp::offset_only(1_000_000, 0).unwrap())
        })));
        let issuer = authority();
        let mut scope = grant(Rights::ReadWrite);
        scope.limits.max_work = 6000;
        let token = issuer.issue_at(&scope, NOW).unwrap();
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
        let text = "CREATE (n:Visible {p:datetime().epochMillis})";
        let parameters = GqlParameters::new()
            .with_text("unused", &"雪".repeat(1000))
            .unwrap();
        // One original transcript fits after staging; two do not. No native
        // operation reads the unused value, so only ingress can impose this bill.
        owner
            .prepare_statement(&db, query, text, &parameters, |params, _, work| {
                assert!(params.statement_time().is_some());
                work(1)
            })
            .unwrap();
        assert_eq!(clock_calls.load(Ordering::SeqCst), 1);
        let mut reached = false;
        let error = owner
            .prepare_statement(&db, query, text, &parameters, |_, _, _| {
                reached = true;
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(
            error,
            WriteTxnError::Authorization(Error::LimitExceeded(fgdb_warden::LimitDimension::Work))
        ));
        assert!(!reached);
        assert_eq!(
            clock_calls.load(Ordering::SeqCst),
            1,
            "input admission precedes the second host clock capture"
        );
        assert!(!owner.is_active());
        assert!(matches!(
            owner.commit(&mut db).await,
            Err(WriteTxnError::Finished)
        ));
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
