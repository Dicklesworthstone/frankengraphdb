//! Authorized occurrence-preserving RETURN over the real native engine.
//! Literal row bags and complete reopened state are independent expectations.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeUpsertPolicy, GraphSymbol,
    GraphSymbolKind, PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x85; 32]);
const FIRST: EId = EId(10);
const SECOND: EId = EId((1_u128 << 96) | 10);
const NOW: u64 = 100;
const UPSERT: &str = "MATCH (a)-[:S]->(b) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p=7 ON MATCH SET r.p=r.p+1";

macro_rules! state {
    ($db:expr) => {
        (
            $db.frontier().unwrap(),
            $db.vertices().unwrap(),
            $db.edges().unwrap(),
        )
    };
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x84; 32], NS, [0x86; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xed92), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant(rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 10_000_000,
            max_rows: rows,
        },
        expires_at_ms: 10_000,
    }
}
fn policy(rows: u64, actions: u64, edges: u64) -> GraphEdgeUpsertPolicy {
    GraphEdgeUpsertPolicy::new(
        GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100_000, rows, 10_000_000, 1_000_000))
            .with_creation_limit(edges),
        actions,
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn query(text: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn rows(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
async fn seed(
    db: &mut Database<MemVfs>,
    cx: &CommitCx,
    existing: bool,
    other_pair: bool,
    hidden: bool,
) {
    let mut seed = WriteBatch::new(S);
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    for id in [1, 2] {
        seed.add_edge(EId(id), VId(1), VId(2), vec![]);
    }
    if other_pair {
        seed.add_edge(EId(3), VId(1), VId(3), vec![]);
    }
    db.write(cx, seed).await.unwrap();
    if existing {
        let mut seed = WriteBatch::new(R);
        for (edge, p) in [(FIRST, 10), (SECOND, 20)] {
            let mut props = vec![(P, CanonicalScalar::Int(p))];
            if hidden {
                props.push((SECRET, CanonicalScalar::Int(99)));
            }
            seed.add_edge(edge, VId(1), VId(2), props);
        }
        db.write(cx, seed).await.unwrap();
    }
    if hidden {
        let mut seed = WriteBatch::new(S);
        seed.create_vertex(
            VId(90),
            vec![LabelId(2)],
            vec![(P, CanonicalScalar::Int(1))],
        );
        seed.add_edge(EId(90), VId(90), VId(2), vec![]);
        db.write(cx, seed).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.add_edge(
            EId(91),
            VId(90),
            VId(2),
            vec![(P, CanonicalScalar::Int(40))],
        );
        db.write(cx, seed).await.unwrap();
    }
}
fn value(db: &Database<MemVfs>, edge: EId) -> CanonicalScalar {
    db.edge(edge)
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find(|(key, _)| *key == P)
        .unwrap()
        .1
        .as_scalar()
        .expect("scalar property")
        .clone()
}

#[test]
fn final_projection_retains_the_occurrence_bag_then_groups_filters_and_pages() {
    let ((), report) = run_async_under_lab(0xed92_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        for (tail, expected) in [
            (
                "RETURN r.p AS p ORDER BY p",
                vec![vec![int(12)], vec![int(12)], vec![int(22)], vec![int(22)]],
            ),
            ("RETURN max(r.p) AS p", vec![vec![int(22)]]),
            (
                "RETURN DISTINCT r.p AS p ORDER BY p",
                vec![vec![int(12)], vec![int(22)]],
            ),
            (
                "RETURN r.p AS p ORDER BY p SKIP 1 LIMIT 2",
                vec![vec![int(12)], vec![int(22)]],
            ),
            (
                "WITH r.p AS p WHERE p>15 RETURN p ORDER BY p",
                vec![vec![int(22)], vec![int(22)]],
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true, false, false).await;
            let before = db.frontier().unwrap();
            let limit = expected.len() as u64;
            let token = authority.issue_at(&grant(limit), NOW).unwrap();
            let definition = query(&format!("{UPSERT} {tail}"));
            // The explicit singular API still refuses these parallel matches.
            assert!(
                db.execute_graph_edge_upsert_query_authorized(
                    &txn,
                    &cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &definition,
                    policy(limit, 4, 0),
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), before);
            let (stats, result, _) = db
                .execute_graph_edge_upserts_query_authorized(
                    &txn,
                    &cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &definition,
                    policy(limit, 4, 0),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(rows(&result.value), expected, "{tail}");
            assert_eq!(stats.match_selection.result_rows, 2);
            assert_eq!(
                (
                    stats.matched_occurrences,
                    stats.action_effects,
                    stats.created_edges
                ),
                (4, 4, 0)
            );
            assert_eq!(value(&db, FIRST), CanonicalScalar::Int(12));
            assert_eq!(value(&db, SECOND), CanonicalScalar::Int(22));
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multi_pair_return_reads_final_state_without_repeating_or_losing_effects() {
    let ((), report) = run_async_under_lab(0xed92_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let token = authority.issue_at(&grant(3), NOW).unwrap();
        for (text, created, expected) in [
            (format!("{UPSERT} RETURN r.p AS p ORDER BY p"), 2, vec![vec![int(7)], vec![int(8)], vec![int(8)]]),
            ("MATCH (a)-[:S]->(b) MERGE (a)-[r:R {p:7}]->(b) ON CREATE SET r.p=8 RETURN r.p AS p".into(),
             3, vec![vec![int(8)], vec![int(8)], vec![int(8)]]),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false, true, false).await;
            let before = db.frontier().unwrap();
            let (stats, result, _) = db.execute_graph_edge_upserts_query_authorized(
                &txn, &cx, &commit, &authority, &token, "main", &query(&text),
                policy(3, 3, created), || NOW,
            ).await.unwrap();
            assert_eq!(rows(&result.value), expected);
            assert_eq!(stats.created_edges, created);
            assert_eq!(stats.action_effects, 3);
            assert_eq!(stats.match_selection.result_rows, 3);
            assert_eq!(db.edges().unwrap().len() as u64, 3 + created);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_result_and_execution_limits_refuse_atomically_and_limit_zero_still_evaluates() {
    let ((), report) = run_async_under_lab(0xed92_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true, false, false).await;
        let before = state!(db);
        let definition = query(&format!("{UPSERT} RETURN r.p AS p"));
        for (native, signed) in [
            (policy(3, 4, 0), 4),
            (policy(4, 4, 0), 3),
            (policy(4, 3, 0), 4),
        ] {
            let token = authority.issue_at(&grant(signed), NOW).unwrap();
            assert!(
                db.execute_graph_edge_upserts_query_authorized(
                    &txn,
                    &cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &definition,
                    native,
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(state!(db), before);
        }
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        for limits in [
            [0, 4, 10_000_000, 1_000_000],
            [100_000, 4, 0, 1_000_000],
            [100_000, 4, 10_000_000, 0],
        ] {
            let [records, rows, work, scratch] = limits;
            let mut native = policy(4, 4, 0);
            native.merge.query = GqlQueryPolicy::new(records, rows, work, scratch);
            assert!(
                db.execute_graph_edge_upserts_query_authorized(
                    &txn,
                    &cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &definition,
                    native,
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(state!(db), before);
        }
        let zero = authority.issue_at(&grant(0), NOW).unwrap();
        let bad = query(&format!("{UPSERT} RETURN 1/(r.p-22) AS bad LIMIT 0"));
        let error = db
            .execute_graph_edge_upserts_query_authorized(
                &txn,
                &cx,
                &commit,
                &authority,
                &zero,
                "main",
                &bad,
                policy(0, 4, 0),
                || NOW,
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("DivisionByZero") || message.contains("division by zero"),
            "{message}"
        );
        assert_eq!(state!(db), before);
        let (_, result, _) = db
            .execute_graph_edge_upserts_query_authorized(
                &txn,
                &cx,
                &commit,
                &authority,
                &zero,
                "main",
                &query(&format!("{UPSERT} RETURN r.p AS p LIMIT 0")),
                policy(0, 4, 0),
                || NOW,
            )
            .await
            .unwrap();
        assert!(result.value.is_empty());
        assert_eq!(value(&db, FIRST), CanonicalScalar::Int(12));
        assert_eq!(value(&db, SECOND), CanonicalScalar::Int(22));
        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_fields_and_endpoints_stay_private_and_committed_results_survive_reopen() {
    let ((), report) = run_async_under_lab(0xed92_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let authority = authority();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit, true, false, true).await;
        let before = state!(db);
        let mut denied = grant(0);
        denied.rights = Rights::Write;
        let token = authority.issue_at(&denied, NOW).unwrap();
        assert!(
            db.execute_graph_edge_upserts_query_authorized(
                &txn,
                &cx,
                &commit,
                &authority,
                &token,
                "main",
                &query(&format!("{UPSERT} RETURN r.p AS p LIMIT 0")),
                policy(0, 4, 0),
                || NOW,
            )
            .await
            .is_err()
        );
        assert_eq!(state!(db), before);
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        for text in [
            "MATCH (a)-[:S]->(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.secret=99 RETURN r.p",
            "MATCH (a)-[:S]->(b) WHERE a.p=999 MERGE (a)-[r:R {secret:99}]->(b) ON MATCH SET r.p=1 RETURN r.p",
        ] {
            assert!(
                db.execute_graph_edge_upserts_query_authorized(
                    &txn,
                    &cx,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &query(text),
                    policy(4, 4, 0),
                    || NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(state!(db), before);
        }
        let (stats, result, _) = db.execute_graph_edge_upserts_query_authorized(
            &txn, &cx, &commit, &authority, &token, "main",
            &query("MATCH (a)-[:S]->(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.p=r.secret RETURN r.secret AS secret,r.p AS p"),
            policy(4, 4, 0), || NOW,
        ).await.unwrap();
        assert_eq!(stats.matched_occurrences, 4);
        assert_eq!(
            rows(&result.value),
            vec![vec![GraphValue::Scalar(CanonicalScalar::Null); 2]; 4]
        );
        assert_eq!(db.vertices().unwrap(), before.1);
        assert_eq!(value(&db, EId(91)), CanonicalScalar::Int(40));
        for edge in [FIRST, SECOND] {
            assert!(
                db.edge(edge)
                    .unwrap()
                    .unwrap()
                    .props
                    .contains(&(SECRET, CanonicalScalar::Int(99).into()))
            );
        }
        let after = state!(db);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(state!(db), after);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
