//! Final host output admission belongs to the same atomic relationship MERGE.
//! Expected rows and rollback state are independent of the output collector.

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergePolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMergeQuery, PreparedGraphEdgeMergeQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Rights, Scope};
use std::cell::Cell;

const NOW: u64 = 100;
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const KEY: PropertyKeyId = PropertyKeyId(1);
const VALUE: PropertyKeyId = PropertyKeyId(2);
const TEXT: &str = "MATCH (a:Visible {key:1}), (b:Visible {key:2}) \
    MERGE (a)-[r:R {value:7}]->(b)";

fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()));
}

fn prepared(tail: &str) -> PreparedGraphEdgeMergeQuery {
    PreparedGraphEdgeMergeQueryText::prepare(
        &format!("{TEXT} {tail}"),
        R,
        |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
            (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(VALUE)),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn policy() -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 1_000_000))
        .with_creation_limit(10)
}

async fn fixture(commit: &CommitCx, rows: u64) -> (Database<MemVfs>, Authority, CapabilityToken) {
    let namespace = DatabaseSecurityNamespaceId([0xc5; 32]);
    let keys = DatabaseKeys::new([0xc4; 32], namespace, [0xc6; 32]);
    let mut db = Database::open_memory(commit, keys).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for (id, key, label) in [(1, 1, L), (2, 2, L), (3, 2, L), (4, 2, LabelId(2))] {
        seed.create_vertex(VId(id), vec![label], vec![(KEY, CanonicalScalar::Int(key))]);
    }
    db.write(commit, seed).await.unwrap();
    let authority = Authority::new(
        AuthKey::from_seed(0xc501),
        namespace,
        "test",
        SchemaEpoch(0),
        1,
    )
    .unwrap();
    let token = authority
        .issue_at(
            &Grant {
                branch: "main".into(),
                labels: Scope::only([L]),
                relations: Scope::only([R]),
                properties: Scope::All,
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 1_000_000,
                    max_work: 100_000_000,
                    max_rows: rows,
                },
                expires_at_ms: 1000,
            },
            NOW,
        )
        .unwrap();
    (db, authority, token)
}

fn values(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

#[test]
fn refusing_final_rows_or_empty_output_discards_all_pair_creations() {
    run(async |commit, query, txn| {
        for empty in [false, true] {
            let (mut db, authority, token) = fixture(commit, if empty { 0 } else { 2 }).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let prepared = prepared(if empty {
                "RETURN r.value AS value LIMIT 0"
            } else {
                "RETURN r.value AS value"
            });
            let mut calls = 0;
            let result = db
                .execute_graph_edge_merge_query_authorized_checked(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    policy(),
                    || NOW,
                    |rows, work| {
                        calls += 1;
                        assert_eq!(
                            values(rows),
                            if empty {
                                Vec::new()
                            } else {
                                vec![vec![GraphValue::Scalar(CanonicalScalar::Int(7))]; 2]
                            }
                        );
                        work(5)?;
                        Err(WriteTxnError::AuthorizedMutationRefused)
                    },
                )
                .await;
            assert!(result.is_err());
            assert_eq!(calls, 1);
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                before
            );
            assert_eq!(txn.outstanding_obligations(), 0);
            let (_, result, completion) = db
                .execute_graph_edge_merge_query_authorized(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    policy(),
                    || NOW,
                )
                .await
                .unwrap();
            assert_eq!(result.value.is_empty(), empty);
            assert_eq!(db.edges().unwrap().len(), 2);
            assert!(
                db.edges()
                    .unwrap()
                    .iter()
                    .all(|row| row.entry.dst != VId(4))
            );
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
        }
    });
}

#[test]
fn no_op_checked_admission_preserves_native_results_stats_and_clock_charges() {
    run(async |commit, query, txn| {
        let prepared = prepared("RETURN r.value AS value");
        let mut baseline = None;
        for checked in [false, true] {
            let (mut db, authority, token) = fixture(commit, 2).await;
            let clocks = Cell::new(0);
            let clock = || {
                clocks.set(clocks.get() + 1);
                NOW
            };
            let (stats, result, completion) = if checked {
                db.execute_graph_edge_merge_query_authorized_checked(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    policy(),
                    clock,
                    |rows, _| {
                        assert_eq!(
                            values(rows),
                            vec![vec![GraphValue::Scalar(CanonicalScalar::Int(7))]; 2]
                        );
                        Ok(())
                    },
                )
                .await
                .unwrap()
            } else {
                db.execute_graph_edge_merge_query_authorized(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared,
                    policy(),
                    clock,
                )
                .await
                .unwrap()
            };
            let actual = (
                stats,
                result.rows,
                result.evaluator,
                values(&result.value),
                completion,
                clocks.get(),
            );
            if let Some(expected) = &baseline {
                assert_eq!(&actual, expected);
            } else {
                baseline = Some(actual);
            }
            assert_eq!(db.edges().unwrap().len(), 2);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn output_meter_and_final_completion_recheck_the_same_live_permit() {
    run(async |commit, query, txn| {
        for mode in 0..3 {
            let (mut db, authority, token) = fixture(commit, 0).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let now = Cell::new(NOW);
            let mut entered = false;
            let result = db
                .execute_graph_edge_merge_query_authorized_checked(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared("RETURN r.value AS value LIMIT 0"),
                    policy(),
                    || now.get(),
                    |rows, work| {
                        entered = true;
                        assert!(rows.is_empty());
                        match mode {
                            0 => work(u64::MAX),
                            1 => {
                                authority.retire();
                                work(0)
                            }
                            _ => {
                                now.set(1000);
                                Ok(())
                            }
                        }
                    },
                )
                .await;
            assert!(entered && result.is_err());
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
fn native_failures_never_enter_host_admission_or_publish_a_prefix() {
    run(async |commit, query, txn| {
        for (rows, tail) in [
            (1, "RETURN r.value AS value"),
            (2, "RETURN 1/(r.value-7) AS bad"),
        ] {
            let (mut db, authority, token) = fixture(commit, rows).await;
            let before = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            let mut entered = false;
            assert!(
                db.execute_graph_edge_merge_query_authorized_checked(
                    txn,
                    query,
                    commit,
                    &authority,
                    &token,
                    "main",
                    &prepared(tail),
                    policy(),
                    || NOW,
                    |_, _| {
                        entered = true;
                        Ok(())
                    },
                )
                .await
                .is_err()
            );
            assert!(!entered);
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
