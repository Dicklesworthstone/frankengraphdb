//! Real directed MERGE through capabilities, the canonical overlay and Chronicle.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GqlScalarParameter, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramError, GraphWriteProgramPolicy, GraphWriteStepReceipt,
    PreparedGraphEdgeMergeText, PreparedGraphWriteProgram,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const H: RelationId = RelationId(9);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x91; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x90; 32], NS, [0x92; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9991), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000))
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Hidden") => Some(GraphSymbol::Label(HIDDEN)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "Hidden") => Some(GraphSymbol::Relation(H)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn definition(text: &str) -> PreparedGraphEdgeMerge {
    PreparedGraphEdgeMergeText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn merge() -> PreparedGraphEdgeMerge {
    definition("MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[:S]->(b)")
}
fn authorization(error: Fault) -> Error {
    match error {
        GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        | GqlQueryError::Source(GraphEdgeMergeError::Source(WriteTxnError::Authorization(error)))
        | GqlQueryError::Source(GraphEdgeMergeError::IdentitySource(
            WriteTxnError::Authorization(error),
        )) => error,
        other => panic!("expected authorization refusal, got {other:?}"),
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
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool, matched: bool) {
    let mut batch = WriteBatch::new(R);
    for (id, p) in [(1, 10), (2, 20)] {
        let mut props = vec![(P, CanonicalScalar::Int(p))];
        if hidden {
            props.push((SECRET, CanonicalScalar::Int(99)));
        }
        batch.create_vertex(VId(id), vec![L], props);
    }
    for id in [11, 12] {
        batch.add_edge(EId(id), VId(1), VId(2), vec![]);
    }
    if hidden {
        for id in 3..8 {
            batch.create_vertex(VId(id), vec![HIDDEN], vec![]);
            batch.add_edge(EId(30 + id), VId(1), VId(id), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if matched {
        let mut batch = WriteBatch::new(S);
        batch.add_edge(
            EId(21),
            VId(1),
            VId(2),
            if hidden {
                vec![(SECRET, CanonicalScalar::Int(101))]
            } else {
                vec![]
            },
        );
        db.write(cx, batch).await.unwrap();
    }
    if hidden {
        let mut batch = WriteBatch::new(H);
        for id in 50..55 {
            batch.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
        db.write(cx, batch).await.unwrap();
    }
}

#[test]
fn creates_once_reduces_duplicate_pairs_and_reopens() {
    lab(0xab01, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit, true, false).await;
        let vertices = db.vertices().unwrap();
        let before = db.frontier().unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let (stats, outcome, completion) = db
            .execute_graph_edge_merge_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &merge(),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        let GraphEdgeMergeOutcome::Created(edge) = outcome else {
            panic!("missing creation")
        };
        assert_eq!(
            (
                stats.match_selection.result_rows,
                stats.overlay_edges,
                stats.created_edges
            ),
            (2, 0, 1)
        );
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
        );
        let record = db.edge(edge).unwrap().unwrap();
        assert_eq!(
            (record.entry.src, record.entry.relation, record.entry.dst),
            (VId(1), S, VId(2))
        );
        let (stats, outcome, completion) = db
            .execute_graph_edge_merge_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &merge(),
                policy().with_creation_limit(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(edge));
        assert_eq!((stats.created_edges, stats.overlay_edges), (0, 1));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), vertices);
        let edges = db.edges().unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), vertices);
        assert_eq!(db.edges().unwrap(), edges);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn ambiguous_pairs_parallel_eids_and_null_endpoints_never_allocate_or_commit() {
    lab(0xab02, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for mode in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false, mode == 1).await;
            if mode == 1 {
                let mut extra = WriteBatch::new(S);
                extra.add_edge(EId(22), VId(1), VId(2), vec![]);
                db.write(&commit, extra).await.unwrap();
            }
            let statement = match mode {
                0 => definition("MATCH (a:Visible) MERGE (a)-[:S]->(a)"),
                1 => merge(),
                _ => definition(
                    "MATCH (a:Visible) WHERE a.p = 10 OPTIONAL MATCH (a)-[:S]->(b) MERGE (a)-[:R]->(b)",
                ),
            };
            let request = GraphInsertRequest::Edge { row: 0, edge: 0 };
            let ElementId::Edge(reserved) = db.allocate_identity(&query, request).unwrap() else {
                panic!("edge id")
            };
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let error = db
                .execute_graph_edge_merge_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &statement,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            match (mode, error) {
                (
                    0,
                    GqlQueryError::Source(GraphEdgeMergeError::AmbiguousEndpointPairs {
                        observed: 2,
                    }),
                )
                | (
                    1,
                    GqlQueryError::Source(GraphEdgeMergeError::AmbiguousRelationships {
                        observed: 2,
                    }),
                )
                | (2, GqlQueryError::Source(GraphEdgeMergeError::NullEndpoint { .. })) => {}
                other => panic!("unexpected ambiguity result: {other:?}"),
            }
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            // The probe took edge counter 1; the refused merge took none.
            assert_eq!(reserved, crate::write_txn::engine_edge(&keys(), 1));
            assert_eq!(
                db.allocate_identity(&query, request).unwrap(),
                ElementId::Edge(crate::write_txn::engine_edge(&keys(), 2))
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn empty_input_has_no_receipt_or_creation_and_self_loop_is_one_edge() {
    lab(0xab03, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, false).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let empty = definition("MATCH (a:Hidden) MERGE (a)-[:S]->(a)");
        let (stats, outcome, completion) = db
            .execute_graph_edge_merge_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &zero,
                "main",
                &empty,
                policy().with_creation_limit(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::NoInput);
        assert_eq!(
            (
                stats.match_selection.result_rows,
                stats.overlay_edges,
                stats.created_edges
            ),
            (0, 0, 0)
        );
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
        let looped = definition("MATCH (a:Visible) WHERE a.p = 10 MERGE (a)-[:S]->(a)");
        let (_, outcome, _) = db
            .execute_graph_edge_merge_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &looped,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        let edge = db.edge(outcome.edge().unwrap()).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(1)));
        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn rights_relation_creation_fields_and_receipt_limits_refuse_before_publication() {
    lab(0xab04, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        for matched in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, matched).await;
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
                    .execute_graph_edge_merge_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &token,
                        "main",
                        &merge(),
                        policy(),
                        || NOW,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(authorization(error), Error::PermissionDenied);
            }
            let token = authority.issue_at(&grant(), NOW).unwrap();
            let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
            let error = db
                .execute_graph_edge_merge_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &zero,
                    "main",
                    &merge(),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(error),
                Error::LimitExceeded(LimitDimension::Rows)
            );
            let forbidden = definition("MATCH (a:Hidden) MERGE (a)-[:Hidden]->(a)");
            let error = db
                .execute_graph_edge_merge_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &forbidden,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(error), Error::ScopeDenied);
            let basic = merge();
            let fields = PreparedGraphEdgeMerge::prepare(
                basic.selection().expect("a MATCH-prefixed MERGE").clone(),
                S,
                basic.source_column(),
                basic.destination_column(),
                vec![(
                    SECRET,
                    GqlScalarParameter::new(CanonicalScalar::Int(999)).unwrap(),
                )],
            )
            .unwrap();
            let result = db
                .execute_graph_edge_merge_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &fields,
                    policy(),
                    || NOW,
                )
                .await;
            // The map is an existence predicate (fgdb-at06q): a hidden key
            // would let its stored value decide the match, so it refuses
            // before selection whether or not a visible edge exists.
            assert_eq!(authorization(result.unwrap_err()), Error::ScopeDenied);
            if !matched {
                let error = db
                    .execute_graph_edge_merge_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &token,
                        "main",
                        &merge(),
                        policy().with_creation_limit(0),
                        || NOW,
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    GqlQueryError::Source(GraphEdgeMergeError::CreationLimit {
                        limit: 0,
                        observed: 1
                    })
                ));
            }
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
fn hidden_graph_changes_no_signed_or_native_refusal_threshold() {
    lab(0xab05, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for matched in [false, true] {
            // Signed work/nodes and native records/work/scratch share the same
            // logical visible domain. This catches charge-before-mask mutants.
            for dimension in 0..5 {
                let mut floors = Vec::new();
                for hidden in [false, true] {
                    let (mut low, mut high) = (0_u64, 4096_u64);
                    while low < high {
                        let middle = low + (high - low) / 2;
                        let limited = match dimension {
                            0 => token.attenuate(Restriction::MaxWork(middle)).unwrap(),
                            1 => token.attenuate(Restriction::MaxNodes(middle)).unwrap(),
                            _ => token.clone(),
                        };
                        let mut policy = policy();
                        match dimension {
                            2 => {
                                policy.query.rows =
                                    fgdb_gql::GqlExecutionBudget::snapshot_records(middle)
                            }
                            3 => policy.query.evaluator.max_work_units = middle,
                            4 => policy.query.evaluator.max_scratch_entries = middle,
                            _ => {}
                        }
                        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                        seed(&mut db, &commit, hidden, matched).await;
                        let before = (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap(),
                        );
                        match db
                            .execute_graph_edge_merge_authorized(
                                &txn,
                                &query,
                                &commit,
                                &authority,
                                &limited,
                                "main",
                                &merge(),
                                policy,
                                || NOW,
                            )
                            .await
                        {
                            Ok((stats, outcome, _)) => {
                                assert_eq!(stats.created_edges, u64::from(!matched));
                                assert_eq!(outcome.created(), !matched);
                                high = middle;
                            }
                            Err(error) => {
                                match dimension {
                                    0 => assert_eq!(
                                        authorization(error),
                                        Error::LimitExceeded(LimitDimension::Work)
                                    ),
                                    1 => assert_eq!(
                                        authorization(error),
                                        Error::LimitExceeded(LimitDimension::Nodes)
                                    ),
                                    2 => assert!(matches!(error, GqlQueryError::Rows(_))),
                                    _ => assert!(matches!(error, GqlQueryError::Evaluator(_))),
                                }
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
                    assert!(low > 0 && low < 4096);
                    floors.push(low);
                }
                assert_eq!(
                    floors[0], floors[1],
                    "matched={matched}, dimension={dimension}"
                );
            }
        }
    });
}

#[test]
fn every_live_expiry_boundary_discards_creation_and_releases_the_pin() {
    lab(0xab06, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, false).await;
        let calls = AtomicU64::new(0);
        db.execute_graph_edge_merge_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            &merge(),
            policy(),
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        )
        .await
        .unwrap();
        let total = calls.load(Ordering::Relaxed);
        assert!(total > 10);
        for cutoff in 0..total {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, false).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let calls = AtomicU64::new(0);
            let error = db
                .execute_graph_edge_merge_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &merge(),
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
            assert_eq!(authorization(error), Error::Expired, "cutoff={cutoff}");
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
fn shared_native_collector_retains_negative_edge_conflict_witnesses() {
    lab(0xab07, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false, false).await;
        let mut transaction = db.begin(&txn_cx).unwrap();
        let (_, outcome) = transaction
            .execute_graph_edge_merge_governed(&mut db, &query, &merge(), policy(), |_| {
                Ok::<_, ()>(ElementId::Edge(EId(80)))
            })
            .unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(80)));
        let mut competing = WriteBatch::new(S);
        competing.add_edge(EId(81), VId(1), VId(2), vec![]);
        db.write(&commit, competing).await.unwrap();
        let seq = db.frontier().unwrap();
        assert!(transaction.finish(&mut db, &commit).await.is_err());
        assert_eq!(db.frontier().unwrap(), seq);
        assert!(db.edge(EId(80)).unwrap().is_none());
        assert!(db.edge(EId(81)).unwrap().is_some());
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
}

async fn seed_multiple_pairs(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 {
        batch.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(10))]);
    }
    // Five MATCH occurrences select three pairs. The middle pair already has
    // an S edge; duplicate occurrences of each missing pair must create once.
    for (eid, src, dst) in [(11, 1, 2), (12, 1, 2), (13, 1, 3), (14, 3, 4), (15, 3, 4)] {
        batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    if hidden {
        for id in [8, 9] {
            batch.create_vertex(
                VId(id),
                vec![HIDDEN],
                vec![(SECRET, CanonicalScalar::Int(99))],
            );
        }
        for (eid, src, dst) in [(81, 1, 8), (82, 8, 2), (83, 8, 9), (84, 8, 9)] {
            batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    // The MERGE map {p: 7} is a match predicate (fgdb-at06q): edge 21 matches
    // on its visible p and keeps its hidden SECRET unchanged.
    let mut existing = WriteBatch::new(S);
    existing.add_edge(
        EId(21),
        VId(1),
        VId(3),
        vec![
            (P, CanonicalScalar::Int(7)),
            (SECRET, CanonicalScalar::Int(101)),
        ],
    );
    if hidden {
        for (eid, src, dst) in [(91, 8, 9), (92, 8, 9), (93, 1, 8)] {
            existing.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
    }
    db.write(cx, existing).await.unwrap();
    if hidden {
        let mut unrelated = WriteBatch::new(H);
        for eid in [41, 42] {
            unrelated.add_edge(EId(eid), VId(1), VId(2), vec![]);
        }
        db.write(cx, unrelated).await.unwrap();
    }
}

fn multiple_pair_program(statements: usize) -> PreparedGraphWriteProgram {
    let merge = definition("MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[:S {p: 7}]->(b)");
    PreparedGraphWriteProgram::prepare((0..statements).map(|_| merge.clone().into()).collect())
        .unwrap()
}

fn multiple_pair_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(policy().query, 0, 0, 2)
}

#[test]
fn authorized_multi_pair_merge_masks_sources_and_preserves_existing_middle_pair() {
    lab(0xab08, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        // Three distinct pairs cost three delivered rows, even with five MATCH
        // occurrences and only two creations.
        let token = authority
            .issue_at(&grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxRows(3))
            .unwrap();
        let program = multiple_pair_program(1);
        let mut visible_stats = Vec::new();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed_multiple_pairs(&mut db, &commit, hidden).await;
            let before = db.frontier().unwrap();
            let vertices = db.vertices().unwrap();
            let existing = db.edge(EId(21)).unwrap().unwrap();
            let (receipt, completion) = db
                .execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    multiple_pair_policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(receipt.stats().selection.result_rows, 5);
            assert_eq!(receipt.stats().created_edges, 2);
            assert_eq!(receipt.stats().created_vertices, 0);
            assert_eq!(receipt.steps().len(), 1);
            let first = crate::write_txn::engine_edge(&keys(), 1);
            let last = crate::write_txn::engine_edge(&keys(), 2);
            assert_eq!(
                receipt.steps(),
                &[GraphWriteStepReceipt::EdgeMergeBatch {
                    outcomes: vec![
                        GraphEdgeMergeOutcome::Created(first),
                        GraphEdgeMergeOutcome::Matched(EId(21)),
                        GraphEdgeMergeOutcome::Created(last),
                    ],
                    created: vec![first, last],
                }]
            );
            for (eid, src, dst) in [(first, VId(1), VId(2)), (last, VId(3), VId(4))] {
                let edge = db.edge(eid).unwrap().unwrap();
                assert_eq!(
                    (edge.entry.src, edge.entry.relation, edge.entry.dst),
                    (src, S, dst)
                );
                assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(7).into())]);
            }
            assert_eq!(db.edge(EId(21)).unwrap().unwrap().props, existing.props);
            assert_eq!(db.vertices().unwrap(), vertices);
            let sequence = db.frontier().unwrap();
            assert_eq!(sequence.0, before.0 + 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: sequence
                }
            );
            visible_stats.push(receipt.stats());

            // Repeating the same program sees all three existing edges without
            // consuming creation allowance or publishing another marker.
            let edges = db.edges().unwrap();
            let mut matched_policy = multiple_pair_policy();
            matched_policy.max_created_edges = 0;
            let (receipt, completion) = db
                .execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    matched_policy,
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(receipt.stats().created_edges, 0);
            assert_eq!(
                receipt.steps()[0].merged_edges(),
                Some(
                    &[
                        GraphEdgeMergeOutcome::Matched(first),
                        GraphEdgeMergeOutcome::Matched(EId(21)),
                        GraphEdgeMergeOutcome::Matched(last),
                    ][..]
                )
            );
            assert_eq!(receipt.steps()[0].created_edges(), Some(&[][..]));
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::ReadClosed { .. }
            ));
            assert_eq!(db.frontier().unwrap(), sequence);
            assert_eq!(db.edges().unwrap(), edges);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        // Hidden endpoints, hidden parallel relationships and unrelated edge
        // types do not change the counted selection or existence domain.
        assert_eq!(visible_stats[0], visible_stats[1]);
    });
}

#[test]
fn authorized_multi_pair_merge_late_row_refusal_discards_every_staged_edge() {
    lab(0xab09, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        // The first case refuses after staging both new edges. The second also
        // accepts a complete earlier step before its repeated matched receipts
        // exceed the same permit's cumulative row allowance.
        for (statements, rows) in [(1, 2), (2, 5)] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed_multiple_pairs(&mut db, &commit, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let program = multiple_pair_program(statements);
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let error = db
                .execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &limited,
                    "main",
                    &program,
                    multiple_pair_policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            match error {
                GraphWriteProgramError::EdgeMerge {
                    statement,
                    source:
                        GqlQueryError::Source(GraphEdgeMergeError::Source(
                            WriteTxnError::Authorization(Error::LimitExceeded(
                                LimitDimension::Rows,
                            )),
                        )),
                } => assert_eq!(statement, statements - 1),
                other => panic!("expected late receipt refusal, got {other:?}"),
            }
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
            // Both missing pairs were actually visited: their reserved engine
            // identities remain spent despite the complete workspace rollback.
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Edge { row: 0, edge: 0 })
                    .unwrap(),
                ElementId::Edge(crate::write_txn::engine_edge(&keys(), 3)),
            );
            let (receipt, completion) = db
                .execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &program,
                    multiple_pair_policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(receipt.stats().created_edges, 2);
            assert_eq!(receipt.stats().completed_statements, statements);
            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

fn returning(text: &str) -> PreparedGraphEdgeMergeQuery {
    let resolve = |kind: GraphSymbolKind, name: &str| match (kind, name) {
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => symbols(kind, name),
    };
    fgdb_gql::PreparedGraphEdgeMergeQueryText::prepare(text, R, resolve)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
/// The authorization refusal a MERGE RETURN fault carries, if it is one.
fn query_authorization(error: QueryFault) -> Option<Error> {
    match error {
        GqlQueryError::Source(GraphEdgeMergeQueryError::Merge(GraphEdgeMergeError::Source(
            WriteTxnError::Authorization(error),
        ))) => Some(error),
        _ => None,
    }
}

/// fgdb-at06q: MERGE ... RETURN under a capability. An undirected MERGE whose
/// pattern runs against the stored edge matches it rather than creating a
/// duplicate, a hidden property reads NULL, endpoint and MATCH properties are
/// the visible ones, and every selected row yields its row. A MERGE map naming
/// a hidden key refuses before publication. A miss commits its edge and
/// returns it.
#[test]
fn authorized_merge_return_masks_hidden_fields_and_matches_either_orientation() {
    lab(0xab0a, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let cells = |result: &GqlQueryExecution<GraphValueRow>| {
            result
                .value
                .iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>()
        };
        let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));

        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, true).await;
        let before = (db.frontier().unwrap(), db.edges().unwrap());
        // Edge 21 runs 1 -> 2; the pattern names it from b (2) to a (1).
        let (stats, result, completion) = db
            .execute_graph_edge_merge_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &returning(
                    "MATCH (a:Visible)-[:R]->(b:Visible) MERGE (b)-[r:S]-(a) \
                     RETURN r, r.secret AS hidden, startNode(r) AS s, endNode(r).p AS q, a.p AS ap",
                ),
                policy().with_creation_limit(0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_edges, 0);
        let row = vec![
            GraphValue::Edge(EId(21)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Vertex(VId(1)),
            int(20),
            int(10),
        ];
        // Both selected R edges (11, 12) name the same pair: two rows.
        assert_eq!(cells(&result), vec![row.clone(), row]);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);

        let error = db
            .execute_graph_edge_merge_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &returning(
                    "MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[r:S {secret: 101}]->(b) \
                     RETURN r",
                ),
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(query_authorization(error), Some(Error::ScopeDenied));
        assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);

        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, false).await;
        let (stats, result, completion) = db
            .execute_graph_edge_merge_query_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &returning(
                    "MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[r:S {p: 3}]->(b) \
                     RETURN r.p AS p",
                ),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_edges, 1);
        assert_eq!(cells(&result), vec![vec![int(3)], vec![int(3)]]);
        // A created relationship commits, at the new frontier.
        let frontier = db.frontier().unwrap();
        assert!(
            matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { commit_seq } if commit_seq == frontier
            ),
            "{completion:?}"
        );
        let created: Vec<_> = db
            .edges()
            .unwrap()
            .into_iter()
            .filter(|edge| edge.entry.relation == S)
            .collect();
        assert_eq!(created.len(), 1);
        assert_eq!(
            (created[0].entry.src, created[0].entry.dst),
            (VId(1), VId(2))
        );
        assert_eq!(created[0].props, vec![(P, CanonicalScalar::Int(3).into())]);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
