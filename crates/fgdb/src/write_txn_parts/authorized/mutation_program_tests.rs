//! Public authorized programs exercise the real collectors, overlay and commit.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError,
    GraphMutationProgramDimension, GraphSymbol, GraphSymbolKind, PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const L: LabelId = LabelId(1);
const ACTIVE: LabelId = LabelId(2);
const HIDDEN: LabelId = LabelId(3);
const R: RelationId = RelationId(1);
const H: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x75; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x74; 32], NS, [0x76; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9975), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L, ACTIVE]),
        relations: Scope::only([R]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 0,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000), 100)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Active") => Some(GraphSymbol::Label(ACTIVE)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn program(statements: &[&str]) -> PreparedGraphMutationProgram {
    PreparedGraphMutationProgram::prepare(
        statements
            .iter()
            .map(|text| {
                PreparedGraphMutationText::prepare(text, R, symbols)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
            })
            .collect(),
    )
    .unwrap()
}
fn dependent() -> PreparedGraphMutationProgram {
    program(&[
        "MATCH (a:Visible) WHERE a.p = 10 SET a.p = 11, a.q = a.p, a:Active",
        "MATCH (a:Active)-[e:R]->(b:Visible) WHERE a.p = 11 \
         SET e.p = a.p, b.q = CASE WHEN a.secret IS NULL THEN a.q ELSE 999 END",
        "MATCH (b:Visible) WHERE b.q = 10 AND b.p = 20 SET b.p = b.q + 100",
    ])
}
fn auth_error(error: Fault) -> Error {
    match error {
        Fault::Preflight(WriteTxnError::Authorization(error))
        | Fault::Interrupted {
            source: WriteTxnError::Authorization(error),
            ..
        }
        | Fault::Statement {
            source: GqlQueryError::Interrupted(WriteTxnError::Authorization(error)),
            ..
        }
        | Fault::Statement {
            source:
                GqlQueryError::Source(GraphMutationError::Source(WriteTxnError::Authorization(error))),
            ..
        } => error,
        other => panic!("unexpected error: {other:?}"),
    }
}
fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool) {
    let mut batch = WriteBatch::new(R);
    let mut props = vec![(P, CanonicalScalar::Int(10))];
    if hidden {
        props.push((SECRET, CanonicalScalar::Int(77)));
    }
    batch.create_vertex(VId(1), vec![L], props);
    batch.create_vertex(
        VId(2),
        if hidden { vec![L, HIDDEN] } else { vec![L] },
        vec![(P, CanonicalScalar::Int(20))],
    );
    for id in [11, 12] {
        batch.add_edge(EId(id), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
    }
    if hidden {
        batch.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(10))]);
        batch.add_edge(EId(13), VId(1), VId(3), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut batch = WriteBatch::new(H);
        batch.add_edge(EId(21), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.unwrap();
    }
}

#[test]
fn dependent_statements_see_canonical_masked_effects_and_publish_once() {
    lab(0xa975, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit, true).await;
        let before = db.frontier().unwrap();
        let hidden = (
            db.vertex(VId(3)).unwrap(),
            db.edge(EId(13)).unwrap(),
            db.edge(EId(21)).unwrap(),
        );
        let (stats, completion) = db
            .execute_graph_mutation_program_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &dependent(),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            (
                stats.completed_statements,
                stats.effects,
                stats.selection.result_rows
            ),
            (3, 7, 4)
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(11)),
                (Q, CanonicalScalar::Int(10)),
                (SECRET, CanonicalScalar::Int(77))
            ]
        );
        assert_eq!(
            db.vertex(VId(2)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(110)),
                (Q, CanonicalScalar::Int(10))
            ]
        );
        for id in [11, 12] {
            assert_eq!(
                db.edge(EId(id)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(11))]
            );
        }
        assert_eq!(
            (
                db.vertex(VId(3)).unwrap(),
                db.edge(EId(13)).unwrap(),
                db.edge(EId(21)).unwrap()
            ),
            hidden
        );
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
        );
        let rows = (db.vertices().unwrap(), db.edges().unwrap());
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), rows);
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn forbidden_or_conflicting_tail_discards_prior_statements() {
    lab(0xa976, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for tail in [
            "MATCH (a:Visible) WHERE a.p = 11 REMOVE a.secret",
            "MATCH (a:Visible) WHERE a.p = 11 SET a.p = 12, a.p = 13",
            "MATCH (a:Visible) WHERE a.p = 11 REMOVE a:Visible",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let program = program(&["MATCH (a:Visible) WHERE a.p = 10 SET a.p = 11", tail]);
            let error = db
                .execute_graph_mutation_program_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, Fault::Statement { statement: 1, .. }));
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn native_program_quotas_accumulate_even_when_effects_cancel() {
    lab(0xa977, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let program = program(&[
            "MATCH (a:Visible) WHERE a.p = 10 SET a.p = 11",
            "MATCH (a:Visible) WHERE a.p = 11 SET a.p = 10",
        ]);
        for selected_limit in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let limited = if selected_limit {
                GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 1, 1_000_000, 100_000), 100)
            } else {
                GraphMutationPolicy {
                    max_effects: 1,
                    ..policy()
                }
            };
            let error = db
                .execute_graph_mutation_program_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    limited,
                    || NOW,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                Fault::Budget {
                    statement: 1,
                    dimension: GraphMutationProgramDimension::Effects
                        | GraphMutationProgramDimension::SelectedRows,
                    ..
                }
            ));
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn deleted_incidence_never_reappears_in_a_later_match() {
    lab(0xa978, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut full = grant();
        full.labels = Scope::All;
        full.relations = Scope::All;
        full.properties = Scope::All;
        let token = authority.issue_at(&full, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let before = db.vertex(VId(2)).unwrap();
        let program = program(&[
            "MATCH (a:Visible) WHERE a.p = 10 DETACH DELETE a",
            "MATCH (a:Visible)-[e:R]->(b:Visible) SET b.q = 999",
        ]);
        let (stats, _) = db
            .execute_graph_mutation_program_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &program,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.completed_statements, stats.effects), (2, 1));
        assert!(db.vertex(VId(1)).unwrap().is_none() && db.edges().unwrap().is_empty());
        assert_eq!(db.vertex(VId(2)).unwrap(), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn empty_program_execution_read_closes_but_still_requires_readwrite() {
    lab(0xa979, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let program = program(&[
            "MATCH (a:Visible) WHERE a.p = 999 SET a.p = 1",
            "MATCH (a:Hidden) SET a.p = 2",
        ]);
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        for rights in [Rights::Read, Rights::Write] {
            let mut grant = grant();
            grant.rights = rights;
            grant.limits.max_work = 0;
            let token = authority.issue_at(&grant, NOW).unwrap();
            let error = db
                .execute_graph_mutation_program_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(auth_error(error), Error::PermissionDenied);
        }
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (stats, completion) = db
            .execute_graph_mutation_program_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &program,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!((stats.completed_statements, stats.effects), (2, 0));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn hidden_overlay_data_changes_neither_signed_thresholds_nor_visible_stats() {
    lab(0xa97a, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for dimension in [LimitDimension::Work, LimitDimension::Nodes] {
            let mut floors = Vec::new();
            let mut successful_stats = Vec::new();
            for hidden in [false, true] {
                let (mut low, mut high) = (0_u64, 8192_u64);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let restriction = match dimension {
                        LimitDimension::Work => Restriction::MaxWork(middle),
                        LimitDimension::Nodes => Restriction::MaxNodes(middle),
                        _ => unreachable!(),
                    };
                    let limited = token.attenuate(restriction).unwrap();
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    seed(&mut db, &commit, hidden).await;
                    let before = (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap(),
                    );
                    match db
                        .execute_graph_mutation_program_authorized(
                            &txn,
                            &query,
                            &commit,
                            &authority,
                            &limited,
                            "main",
                            &dependent(),
                            policy(),
                            || NOW,
                        )
                        .await
                    {
                        Ok((stats, _)) => {
                            high = middle;
                            successful_stats.push(stats);
                        }
                        Err(error) => {
                            assert_eq!(auth_error(error), Error::LimitExceeded(dimension));
                            assert_eq!(
                                (
                                    db.frontier().unwrap(),
                                    db.vertices().unwrap(),
                                    db.edges().unwrap()
                                ),
                                before
                            );
                            low = middle + 1;
                        }
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
                assert!(low > 0 && low < 8192);
                floors.push(low);
            }
            assert_eq!(floors[0], floors[1], "hidden overlay changed {dimension:?}");
            assert!(
                successful_stats
                    .iter()
                    .all(|stats| *stats == successful_stats[0])
            );
        }
    });
}

#[test]
fn expiry_during_program_or_final_validation_never_commits_a_prefix() {
    lab(0xa97b, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let calls = AtomicU64::new(0);
        db.execute_graph_mutation_program_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            &dependent(),
            policy(),
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        )
        .await
        .unwrap();
        let count = calls.load(Ordering::Relaxed);
        assert!(count > 10);
        for cutoff in [1, count / 2, count - 1] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let calls = AtomicU64::new(0);
            let error = db
                .execute_graph_mutation_program_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &dependent(),
                    policy(),
                    || {
                        if calls.fetch_add(1, Ordering::Relaxed) < cutoff {
                            NOW
                        } else {
                            10_000
                        }
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(auth_error(error), Error::Expired);
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
