//! Real authorized transactions over duplicate MATCH occurrences and parallel
//! relationships. Expected values and cardinalities are hand-enumerated; no
//! reference to the implementation's own result construction is an oracle.

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeUpsertPolicy,
    GraphEdgeUpsertQueryError, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeUpsertQuery,
    PreparedGraphEdgeUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CanonicalText, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};
use std::sync::atomic::{AtomicUsize, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const H: RelationId = RelationId(9);
const VISIBLE: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const KEY: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xab; 32]);
const NOW: u64 = 100;
const FIRST: EId = EId((1_u128 << 96) + 13);
const SECOND: EId = EId((1_u128 << 112) + 17);
const MATCH: &str = "MATCH (a:Visible)-[s:S]->(b:Visible)";
const UPDATE: &str = "MERGE (a)-[r:R]-(b) ON MATCH SET r.p=r.p+1";

fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()));
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xaa; 32], NS, [0xac; 32])
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xab01), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([VISIBLE]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P, KEY]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: rows,
        },
        expires_at_ms: 1_000,
    }
}

fn policy(rows: u64, actions: u64, creations: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100_000, rows, 10_000_000, 1_000_000))
            .with_creation_limit(creations),
        actions,
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(VISIBLE)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
        _ => None,
    }
}

fn prepared(text: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

/// A hidden payload far larger than the visible "private" one, yet one edge
/// row under the 16 KiB per-object ceiling (eb4bf6ec): 1000 repetitions made a
/// 16,933-byte row the store refuses, so every law here failed at setup.
const HIDDEN_PAYLOAD_REPEATS: usize = 900;

async fn fixture(commit: &CommitCx, existing: bool, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let mut seed = WriteBatch::new(S);
    for (id, p) in [(1, 10), (2, 20)] {
        seed.create_vertex(VId(id), vec![VISIBLE], vec![(P, CanonicalScalar::Int(p))]);
    }
    for id in [11, 12] {
        seed.add_edge(EId(id), VId(1), VId(2), vec![]);
    }
    if hidden {
        seed.create_vertex(VId(3), vec![HIDDEN], vec![(P, CanonicalScalar::Int(20))]);
        seed.add_edge(EId(90), VId(1), VId(3), vec![]);
        seed.add_edge(EId(91), VId(3), VId(2), vec![]);
    }
    db.write(commit, seed).await.unwrap();
    if existing {
        let mut seed = WriteBatch::new(R);
        // The reverse edge must be found for an undirected MERGE, with its
        // actual identity and endpoints preserved through every occurrence.
        for (edge, src, dst, p) in [(FIRST, 1, 2, 10), (SECOND, 2, 1, 20)] {
            seed.add_edge(
                edge,
                VId(src),
                VId(dst),
                vec![
                    (P, CanonicalScalar::Int(p)),
                    (
                        SECRET,
                        CanonicalScalar::Text(if hidden {
                            CanonicalText::new_ucs_basic(
                                &"private payload".repeat(HIDDEN_PAYLOAD_REPEATS),
                            )
                            .unwrap()
                        } else {
                            CanonicalText::new_ucs_basic("private").unwrap()
                        }),
                    ),
                ],
            );
        }
        db.write(commit, seed).await.unwrap();
    }
    if hidden {
        let mut seed = WriteBatch::new(H);
        seed.add_edge(
            EId(92),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(100))],
        );
        db.write(commit, seed).await.unwrap();
    }
    db
}

fn int(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}

fn p(db: &Database<MemVfs>, edge: EId) -> CanonicalScalar {
    db.edge(edge)
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find(|(key, _)| *key == P)
        .unwrap()
        .1
}

#[test]
fn duplicate_inputs_keep_parallel_full_identities_final_values_and_hidden_fields() {
    run(async |commit, cx, txcx| {
        let mut db = fixture(commit, true, true).await;
        let authority = authority();
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        let query = prepared(&format!(
            "{MATCH} {UPDATE} RETURN r AS edge,r.p AS p,r.secret AS hidden ORDER BY edge"
        ));
        let before = db.frontier().unwrap();
        let original = db.edges().unwrap();
        assert!(
            db.execute_graph_edge_upsert_query_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &query,
                policy(4, 4, 0),
                || NOW,
            )
            .await
            .is_err(),
            "the explicitly singular API must remain singular"
        );
        assert_eq!(db.edges().unwrap(), original);
        let (stats, result, _) = db
            .execute_graph_edge_upserts_query_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &query,
                policy(4, 4, 0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.match_selection.result_rows, 2);
        assert_eq!(stats.matched_occurrences, 4);
        assert_eq!(stats.created_edges, 0);
        assert_eq!(stats.action_effects, 4);
        let rows: Vec<_> = result
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect();
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        assert_eq!(
            rows,
            vec![
                vec![GraphValue::Edge(FIRST), int(12), null.clone()],
                vec![GraphValue::Edge(FIRST), int(12), null.clone()],
                vec![GraphValue::Edge(SECOND), int(22), null.clone()],
                vec![GraphValue::Edge(SECOND), int(22), null],
            ]
        );
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        for edge in [FIRST, SECOND] {
            let record = db.edge(edge).unwrap().unwrap();
            assert_eq!(
                (record.entry.src, record.entry.dst),
                if edge == FIRST {
                    (VId(1), VId(2))
                } else {
                    (VId(2), VId(1))
                },
            );
            assert_eq!(record.props[1].0, SECRET);
            assert_eq!(
                record.props[1].1,
                CanonicalScalar::Text(
                    CanonicalText::new_ucs_basic(&"private payload".repeat(HIDDEN_PAYLOAD_REPEATS))
                        .unwrap()
                )
            );
        }
        assert_eq!(p(&db, EId(92)), CanonicalScalar::Int(100));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn creation_is_visible_to_later_occurrences_and_changed_merge_keys_create_again() {
    run(async |commit, cx, txcx| {
        let authority = authority();
        let token = authority.issue_at(&grant(2), NOW).unwrap();
        for changed_key in [false, true] {
            let mut db = fixture(commit, false, false).await;
            let statement = if changed_key {
                "MERGE (a)-[r:R {key:7}]->(b) ON CREATE SET r.key=8,r.p=1 ON MATCH SET r.p=99"
            } else {
                "MERGE (a)-[r:R]->(b) ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1"
            };
            let query = prepared(&format!(
                "{MATCH} {statement} RETURN r AS edge,r.p AS p ORDER BY edge"
            ));
            let before = db.frontier().unwrap();
            let edges = db.edges().unwrap();
            assert!(
                db.execute_graph_edge_upserts_query_authorized(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(2, 4, u64::from(changed_key)),
                    || NOW,
                )
                .await
                .is_err(),
                "the creation cap must span both occurrences"
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.edges().unwrap(), edges);
            let (stats, result, _) = db
                .execute_graph_edge_upserts_query_authorized(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(2, 4, 2),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(result.value.len(), 2);
            let first = result.value[0].values()[0].as_edge().unwrap();
            let second = result.value[1].values()[0].as_edge().unwrap();
            assert_eq!(stats.created_edges, if changed_key { 2 } else { 1 });
            assert_eq!(stats.matched_occurrences, u64::from(!changed_key));
            assert_eq!(first == second, !changed_key);
            let expected = if changed_key { 1 } else { 8 };
            for row in &result.value {
                assert_eq!(row.values()[1], int(expected));
            }
            assert_eq!(p(&db, first), CanonicalScalar::Int(expected));
            assert_eq!(p(&db, second), CanonicalScalar::Int(expected));
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn one_final_aggregate_row_does_not_limit_four_action_occurrences() {
    run(async |commit, cx, txcx| {
        let mut db = fixture(commit, true, false).await;
        let authority = authority();
        let token = authority.issue_at(&grant(1), NOW).unwrap();
        let query = prepared(&format!(
            "{MATCH} {UPDATE} WITH r.p AS p RETURN count(*) AS n,sum(p) AS total"
        ));
        let (stats, result, _) = db
            .execute_graph_edge_upserts_query_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &query,
                policy(1, 4, 0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.action_effects, 4);
        assert_eq!(result.rows.result_rows, 1);
        assert_eq!(result.value[0].values(), &[int(4), int(68)]);
        assert_eq!(p(&db, FIRST), CanonicalScalar::Int(12));
        assert_eq!(p(&db, SECOND), CanonicalScalar::Int(22));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn later_actions_and_limit_zero_projection_errors_restore_the_whole_graph() {
    run(async |commit, cx, txcx| {
        let authority = authority();
        for late_return in [false, true] {
            let mut db = fixture(commit, true, true).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let query = prepared(&format!(
                "{MATCH} {}",
                if late_return {
                    "MERGE (a)-[r:R]-(b) ON MATCH SET r.p=r.p+1 RETURN 1/(r.p-22) AS bad LIMIT 0"
                } else {
                    "MERGE (a)-[r:R]-(b) ON MATCH SET r.p=10/(r.p-20) RETURN r"
                }
            ));
            let token = authority
                .issue_at(&grant(if late_return { 0 } else { 4 }), NOW)
                .unwrap();
            let result = db
                .execute_graph_edge_upserts_query_authorized(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(if late_return { 0 } else { 4 }, 4, 0),
                    || NOW,
                )
                .await;
            if late_return {
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Returning(
                        _
                    )))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Upsert(_)))
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
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn each_native_allowance_is_cumulative_and_refusal_never_publishes_a_prefix() {
    run(async |commit, cx, txcx| {
        let authority = authority();
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        let query = prepared(&format!("{MATCH} {UPDATE} RETURN r"));
        let mut baseline = fixture(commit, true, false).await;
        let (_, used, _) = baseline
            .execute_graph_edge_upserts_query_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &query,
                policy(4, 4, 0),
                || NOW,
            )
            .await
            .unwrap();
        let records = used.rows.snapshot_records;
        let work = used.evaluator.work_units;
        let scratch = used.evaluator.scratch_entries;
        assert!(records > 0 && work > 0 && scratch > 0);
        for (at, native) in [
            GqlQueryPolicy::new(records, 4, work, scratch),
            GqlQueryPolicy::new(records - 1, 4, work, scratch),
            GqlQueryPolicy::new(records, 3, work, scratch),
            GqlQueryPolicy::new(records, 4, work - 1, scratch),
            GqlQueryPolicy::new(records, 4, work, scratch - 1),
        ]
        .into_iter()
        .enumerate()
        {
            let mut db = fixture(commit, true, false).await;
            let before = (db.frontier().unwrap(), db.edges().unwrap());
            let result = db
                .execute_graph_edge_upserts_query_authorized(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    GraphEdgeUpsertPolicy::new(
                        GraphEdgeMergePolicy::new(native).with_creation_limit(0),
                        4,
                    ),
                    || NOW,
                )
                .await;
            if at == 0 {
                let (_, exact, _) = result.unwrap();
                assert_eq!(exact.value, used.value);
                assert_eq!(exact.rows, used.rows);
                assert_eq!(exact.evaluator, used.evaluator);
            } else {
                assert!(
                    matches!(
                        result,
                        Err(GqlQueryError::Rows(_)) | Err(GqlQueryError::Evaluator(_))
                    ),
                    "dimension {at}"
                );
                assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        let mut db = fixture(commit, true, false).await;
        let before = (db.frontier().unwrap(), db.edges().unwrap());
        assert!(
            db.execute_graph_edge_upserts_query_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &query,
                policy(4, 3, 0),
                || NOW,
            )
            .await
            .is_err()
        );
        assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);
        let narrow = authority.issue_at(&grant(3), NOW).unwrap();
        assert!(
            db.execute_graph_edge_upserts_query_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &narrow,
                "main",
                &query,
                policy(4, 4, 0),
                || NOW,
            )
            .await
            .is_err(),
            "native admission cannot bypass the signed row grant"
        );
        assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);
    });
}

#[test]
fn hidden_payloads_endpoints_and_relations_do_not_change_logical_query_usage() {
    run(async |commit, cx, txcx| {
        let authority = authority();
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        let query = prepared(&format!(
            "{MATCH} {UPDATE} RETURN r,r.p AS p,r.secret AS hidden ORDER BY r"
        ));
        let mut outputs = Vec::new();
        for hidden in [false, true] {
            let mut db = fixture(commit, true, hidden).await;
            let (stats, result, _) = db
                .execute_graph_edge_upserts_query_authorized(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(4, 4, 0),
                    || NOW,
                )
                .await
                .unwrap();
            outputs.push((stats, result.value, result.rows, result.evaluator));
        }
        assert_eq!(outputs[0], outputs[1]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn checked_output_refusal_or_expiry_after_actions_rolls_back_before_completion() {
    run(async |commit, cx, txcx| {
        let authority = authority();
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        let query = prepared(&format!("{MATCH} {UPDATE} RETURN r"));
        for expiry in [false, true] {
            let mut db = fixture(commit, true, false).await;
            let before = (db.frontier().unwrap(), db.edges().unwrap());
            let now = AtomicUsize::new(NOW as usize);
            let calls = AtomicUsize::new(0);
            let result = db
                .execute_graph_edge_upserts_query_authorized_checked(
                    txcx,
                    cx,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &query,
                    policy(4, 4, 0),
                    || now.load(Ordering::Relaxed) as u64,
                    |rows, meter| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(rows.len(), 4);
                        if expiry {
                            now.store(1001, Ordering::Relaxed);
                        }
                        meter(1)?;
                        Err(WriteTxnError::AuthorizedMutationRefused)
                    },
                )
                .await;
            assert!(result.is_err());
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!((db.frontier().unwrap(), db.edges().unwrap()), before);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn denied_fields_rights_and_empty_input_existence_predicates_never_stage_writes() {
    run(async |commit, cx, txcx| {
        let authority = authority();
        for (statement, write_only) in [
            (format!("{MATCH} {UPDATE} RETURN r"), true),
            ("MATCH (a:Visible),(b:Visible) WHERE a.p < 0 MERGE (a)-[r:R {secret:1}]->(b) ON CREATE SET r.p=1 RETURN r".into(), false),
            (format!("{MATCH} MERGE (a)-[r:R]-(b) ON MATCH SET r.p=1,r.secret=NULL RETURN r"), false),
        ] {
            let mut db = fixture(commit, true, true).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let mut capability = grant(4);
            if write_only { capability.rights = Rights::Write; }
            let token = authority.issue_at(&capability, NOW).unwrap();
            let query = prepared(&statement);
            assert!(db.execute_graph_edge_upserts_query_authorized(
                txcx, cx, commit, &authority, &token, "main", &query, policy(4, 10, 1), || NOW,
            ).await.is_err());
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn zero_final_rows_keep_creations_and_empty_match_does_not_invent_effects() {
    run(async |commit, cx, txcx| {
        let authority = authority();
        let token = authority.issue_at(&grant(0), NOW).unwrap();
        let mut db = fixture(commit, false, false).await;
        let query = prepared(&format!(
            "{MATCH} MERGE (a)-[r:R]->(b) ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1 \
             WITH r.p AS p WHERE p < 0 RETURN p"
        ));
        let before = db.frontier().unwrap();
        let calls = AtomicUsize::new(0);
        let (stats, result, _) = db
            .execute_graph_edge_upserts_query_authorized_checked(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &query,
                policy(0, 2, 1),
                || NOW,
                |rows, meter| {
                    assert!(rows.is_empty());
                    calls.fetch_add(1, Ordering::Relaxed);
                    meter(0)
                },
            )
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(stats.created_edges, 1);
        assert_eq!(stats.matched_occurrences, 1);
        assert!(result.value.is_empty());
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        let edges = db.edges().unwrap();
        let created = edges
            .iter()
            .filter(|edge| edge.entry.relation == R)
            .collect::<Vec<_>>();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].props, vec![(P, CanonicalScalar::Int(8))]);
        let query = prepared(
            "MATCH (a:Visible),(b:Visible) WHERE a.p < 0 \
             MERGE (a)-[r:R]->(b) ON CREATE SET r.p=99 RETURN r",
        );
        let before = db.frontier().unwrap();
        let (stats, result, _) = db
            .execute_graph_edge_upserts_query_authorized(
                txcx,
                cx,
                commit,
                &authority,
                &token,
                "main",
                &query,
                policy(0, 0, 0),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.action_effects, 0);
        assert_eq!(stats.created_edges, 0);
        assert_eq!(stats.matched_occurrences, 0);
        assert!(result.value.is_empty());
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.edges().unwrap(), edges);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
