//! Native authorized program/session calls, not a duplicate graph evaluator.
//! MATCH occurrence count, parallel matches and modified EIds are distinct.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphWriteProgramPolicy,
    GraphWriteProgramStats, PreparedGraphWriteProgram, PreparedGraphWriteScript,
};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use fgdb_warden::{
    Authority, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope,
};
use std::future::Future;
use std::path::PathBuf;

const NOW: u64 = 100;
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x65; 32]);
const VISIBLE: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const FEED: RelationId = RelationId(1);
const R: RelationId = RelationId(2);
const KEY: PropertyKeyId = PropertyKeyId(1);
const VALUE: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const E1: EId = EId(u128::MAX - 2);
const E2: EId = EId(u128::MAX - 1);
const E3: EId = EId(u128::MAX);
const UPSERT: &str = "MATCH (a)-[:Feed]->(b) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.value=10 ON MATCH SET r.value=r.value+1";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x64; 32], NS, [0x66; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(6501), NS, "test", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([VISIBLE]),
        relations: Scope::only([FEED, R]),
        properties: Scope::only([KEY, VALUE]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 10_000_000,
            max_rows: 1000,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 1_000_000),
        100,
        100,
        100,
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(VISIBLE)),
        (GraphSymbolKind::Relation, "Feed") => Some(GraphSymbol::Relation(FEED)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
        (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(VALUE)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn program(text: &str) -> PreparedGraphWriteProgram {
    PreparedGraphWriteScript::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn lab<F: Future<Output = ()> + Send + 'static>(
    seed: u64,
    body: impl FnOnce(PurposeContexts) -> F + Send + 'static,
) {
    let ((), report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn fixture(
    cx: &CommitCx,
    targets: bool,
    hidden: bool,
) -> (Database<MemVfs>, MemVfs, PathBuf) {
    let vfs = MemVfs::new().unwrap();
    let path = vfs.database_dir();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), &path, keys())
        .await
        .unwrap();
    let mut feed = WriteBatch::new(FEED);
    for vertex in 1..=4 {
        feed.create_vertex(VId(vertex), vec![VISIBLE], vec![]);
    }
    feed.add_edge(EId(101), VId(1), VId(2), vec![]);
    feed.add_edge(EId(102), VId(1), VId(2), vec![]);
    feed.add_edge(EId(103), VId(3), VId(4), vec![]);
    if hidden {
        // This is a REAL MATCH occurrence in the privileged graph, not a
        // nonmatching label used as a misleading authorization control.
        feed.create_vertex(VId(9), vec![HIDDEN], vec![]);
        feed.add_edge(EId(901), VId(9), VId(2), vec![]);
    }
    db.write(cx, feed).await.unwrap();
    let mut edges = WriteBatch::new(R);
    if targets {
        for (id, src, dst, value) in [(E1, 1, 2, 10), (E2, 1, 2, 20), (E3, 3, 4, 30)] {
            edges.add_edge(
                id,
                VId(src),
                VId(dst),
                vec![
                    (KEY, CanonicalScalar::Int(1)),
                    (VALUE, CanonicalScalar::Int(value)),
                    (SECRET, CanonicalScalar::Int(7)),
                ],
            );
        }
    }
    if hidden {
        edges.add_edge(
            EId(902),
            VId(9),
            VId(2),
            vec![(VALUE, CanonicalScalar::Int(777))],
        );
    }
    if targets || hidden {
        db.write(cx, edges).await.unwrap();
    }
    (db, vfs, path)
}
fn value(db: &Database<MemVfs>, edge: EId) -> CanonicalScalar {
    db.edge(edge)
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find(|(key, _)| *key == VALUE)
        .unwrap()
        .1
        .as_scalar()
        .expect("scalar property")
        .clone()
}
fn authorization(error: &(dyn std::error::Error + 'static)) -> Option<Error> {
    let mut current = Some(error);
    while let Some(cause) = current {
        if let Some(WriteTxnError::Authorization(error)) = cause.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        current = cause.source();
    }
    None
}

#[test]
fn repeated_pairs_keep_every_parallel_match_and_reopen_the_one_commit() {
    lab(0x6501, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let (mut db, vfs, path) = fixture(&cx, true, true).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let before = db.frontier().unwrap();
        let hidden = db.edge(EId(902)).unwrap();
        let (receipt, completion) = db
            .execute_graph_write_program_returning_authorized(
                &txn,
                &query,
                &cx,
                &issuer,
                &token,
                "main",
                &program(UPSERT),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        let expected: Vec<_> = [E1, E2, E1, E2, E3]
            .into_iter()
            .map(GraphEdgeMergeOutcome::Matched)
            .collect();
        assert_eq!(receipt.steps()[0].merged_edges(), Some(expected.as_slice()));
        assert_eq!(receipt.steps()[0].created_edges(), Some(&[][..]));
        assert_eq!(receipt.stats().selection.result_rows, 3);
        assert_eq!(receipt.stats().mutation_effects, 5);
        assert_eq!(receipt.stats().target_edge_visits, 3);
        assert_eq!(receipt.stats().created_edges, 0);
        assert_eq!(
            [value(&db, E1), value(&db, E2), value(&db, E3)],
            [
                CanonicalScalar::Int(12),
                CanonicalScalar::Int(22),
                CanonicalScalar::Int(31)
            ]
        );
        assert_eq!(db.edge(EId(902)).unwrap(), hidden);
        for edge in [E1, E2, E3] {
            assert!(
                db.edge(edge)
                    .unwrap()
                    .unwrap()
                    .props
                    .contains(&(SECRET, CanonicalScalar::Int(7).into()))
            );
        }
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, before.0 + 1);
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq }
        );
        let expected = db.edges().unwrap();
        db.compact(&cx).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap(), seq);
        assert_eq!(reopened.edges().unwrap(), expected);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn zero_row_stats_and_hidden_graphs_have_the_same_native_usage() {
    lab(0x6502, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxRows(0))
            .unwrap();
        let mut seen = Vec::new();
        for hidden in [false, true] {
            let (mut db, _, _) = fixture(&cx, true, hidden).await;
            let (stats, _) = db
                .execute_graph_write_program_authorized(
                    &txn,
                    &query,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &program(UPSERT),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!((stats.mutation_effects, stats.target_edge_visits), (5, 3));
            seen.push(stats);
            assert_eq!(value(&db, E1), CanonicalScalar::Int(12));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        assert_eq!(seen[0], seen[1]);
    });
}

#[test]
fn creations_are_seen_by_later_occurrences_but_key_changes_force_new_matches() {
    lab(0x6503, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for change_key in [false, true] {
            let (mut db, _, _) = fixture(&cx, false, true).await;
            let text = if change_key {
                "MATCH (a)-[:Feed]->(b) MERGE (a)-[r:R {key:1}]->(b) ON CREATE SET r.value=10 SET r.key=2"
            } else {
                UPSERT
            };
            let (receipt, _) = db
                .execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &program(text),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
            let outcomes = receipt.steps()[0].merged_edges().unwrap();
            assert_eq!(outcomes.len(), 3);
            assert_eq!(
                receipt.stats().created_edges,
                if change_key { 3 } else { 2 }
            );
            assert!(outcomes[0].created() && outcomes[2].created());
            if change_key {
                assert!(outcomes[1].created());
                assert_ne!(outcomes[0].edge(), outcomes[1].edge());
            } else {
                assert!(!outcomes[1].created());
                assert_eq!(outcomes[0].edge(), outcomes[1].edge());
                assert_eq!(
                    value(&db, outcomes[0].edge().unwrap()),
                    CanonicalScalar::Int(11)
                );
            }
            assert_eq!(value(&db, EId(902)), CanonicalScalar::Int(777));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn denied_fields_late_expressions_and_receipt_exhaustion_discard_the_whole_prefix() {
    lab(0x6504, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for (tail, rows, expected_auth) in [
            (
                "MATCH (a)-[:Feed]->(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.value=100/(r.value-30)",
                100,
                None,
            ),
            (
                "MATCH (a)-[:Feed]->(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.value=99,r.secret=7",
                100,
                Some(Error::ScopeDenied),
            ),
            (UPSERT, 5, Some(Error::LimitExceeded(LimitDimension::Rows))),
        ] {
            let (mut db, _, _) = fixture(&cx, true, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let limited = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            // Prefix costs one row; the five upsert occurrences cannot fit in
            // the remaining four. No receipt or created vertex may escape.
            let text = format!("CREATE (n:Visible {{key:999}}); {tail}");
            let error = db
                .execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &cx,
                    &issuer,
                    &limited,
                    "main",
                    &program(&text),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
            if let Some(expected) = expected_auth {
                assert_eq!(authorization(&error), Some(expected));
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
fn action_and_creation_caps_are_statement_wide_not_refreshed_per_occurrence() {
    lab(0x6505, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for targets in [false, true] {
            let (mut db, _, _) = fixture(&cx, targets, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut limited = policy();
            if targets {
                limited = GraphWriteProgramPolicy::new(policy().mutations.query, 4, 100, 100);
            } else {
                limited.max_created_edges = 1;
            }
            assert!(
                db.execute_graph_write_program_authorized(
                    &txn,
                    &query,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &program(UPSERT),
                    limited,
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            let (stats, _) = db
                .execute_graph_write_program_authorized(
                    &txn,
                    &query,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &program(UPSERT),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(stats.created_edges, if targets { 0 } else { 2 });
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn empty_input_still_checks_relation_and_hidden_merge_fields_before_lookup() {
    lab(0x6506, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let (stats, completion) = db
            .execute_graph_write_program_returning_authorized(
                &txn,
                &query,
                &cx,
                &issuer,
                &zero,
                "main",
                &program(UPSERT),
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.stats().mutation_effects, 0);
        assert_eq!(
            stats.steps()[0].merged_edge(),
            Some(GraphEdgeMergeOutcome::NoInput)
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        let hidden_map = program(
            "MATCH (a)-[:Feed]->(b) MERGE (a)-[r:R {secret:7}]->(b) ON MATCH SET r.value=1",
        );
        let error = db
            .execute_graph_write_program_authorized(
                &txn,
                &query,
                &cx,
                &issuer,
                &token,
                "main",
                &hidden_map,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        let mut denied = grant();
        denied.relations = Scope::only([FEED]);
        let token = issuer.issue_at(&denied, NOW).unwrap();
        let error = db
            .execute_graph_write_program_authorized(
                &txn,
                &query,
                &cx,
                &issuer,
                &token,
                "main",
                &program(UPSERT),
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn native_limits_accumulate_across_occurrences_and_statement_boundaries() {
    lab(0x6507, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let input = program(&format!("{UPSERT}; {UPSERT}"));
        for returning in [false, true] {
            let mut measured: Option<GraphWriteProgramStats> = None;
            for dimension in 0..=5 {
                let (mut db, _, _) = fixture(&cx, true, true).await;
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let mut allowance = policy();
                if let Some(stats) = measured {
                    let mut limits = [
                        stats.selection.snapshot_records,
                        stats.selection.result_rows,
                        stats.evaluator.work_units,
                        stats.evaluator.scratch_entries,
                    ];
                    if dimension <= 4 {
                        limits[dimension - 1] -= 1;
                    }
                    allowance = GraphWriteProgramPolicy::new(
                        GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                        if dimension == 5 {
                            stats.mutation_effects - 1
                        } else {
                            stats.mutation_effects
                        },
                        100,
                        100,
                    );
                }
                let result = if returning {
                    db.execute_graph_write_program_returning_authorized(
                        &txn,
                        &query,
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        &input,
                        allowance,
                        || NOW,
                    )
                    .await
                    .map(|(receipt, _)| receipt.stats())
                } else {
                    db.execute_graph_write_program_authorized(
                        &txn,
                        &query,
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        &input,
                        allowance,
                        || NOW,
                    )
                    .await
                    .map(|(stats, _)| stats)
                };
                if dimension == 0 {
                    let stats = result.unwrap();
                    assert_eq!((stats.mutation_effects, stats.target_edge_visits), (10, 6));
                    measured = Some(stats);
                } else {
                    assert!(
                        result.is_err(),
                        "one-less dimension {dimension}, returning={returning}"
                    );
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            let stats = measured.unwrap();
            let exact = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(
                    stats.selection.snapshot_records,
                    stats.selection.result_rows,
                    stats.evaluator.work_units,
                    stats.evaluator.scratch_entries,
                ),
                stats.mutation_effects,
                0,
                0,
            );
            let (mut db, _, _) = fixture(&cx, true, true).await;
            let actual = if returning {
                db.execute_graph_write_program_returning_authorized(
                    &txn,
                    &query,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &input,
                    exact,
                    || NOW,
                )
                .await
                .unwrap()
                .0
                .stats()
            } else {
                db.execute_graph_write_program_authorized(
                    &txn,
                    &query,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &input,
                    exact,
                    || NOW,
                )
                .await
                .unwrap()
                .0
            };
            assert_eq!(actual, stats);
        }
    });
}

#[test]
fn prepared_parameter_batches_rebind_without_refreshing_the_program_or_permit() {
    lab(0x6508, |contexts| async move {
        let (cx, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = authority();
        let token = issuer
            .issue_at(&grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxRows(0))
            .unwrap();
        let text = "MATCH (a)-[:Feed]->(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.value=r.value+$inc";
        let args = |value| GqlParameters::new().with_int64("inc", value).unwrap();
        for fail_tail in [false, true] {
            let (mut db, _, _) = fixture(&cx, true, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut session = db
                .authorized_write_session(
                    &txn,
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    R,
                    policy(),
                    64,
                    || NOW,
                )
                .unwrap();
            let prepared = session.prepare(&query, text, &args(1)).unwrap();
            let sets = [args(1), args(if fail_tail { i64::MAX } else { 2 })];
            let result = session.execute_batch_stats(&query, &prepared, &sets).await;
            assert_eq!(session.is_closed(), fail_tail);
            drop(session);
            if fail_tail {
                assert!(result.is_err());
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            } else {
                let (stats, _) = result.unwrap();
                assert_eq!(
                    (stats.completed_statements, stats.mutation_effects),
                    (2, 10)
                );
                assert_eq!(
                    [value(&db, E1), value(&db, E2), value(&db, E3)],
                    [
                        CanonicalScalar::Int(16),
                        CanonicalScalar::Int(26),
                        CanonicalScalar::Int(33)
                    ]
                );
                assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[path = "authorized_edge_upsert_program/returning.rs"]
mod returning;
