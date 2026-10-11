//! Host result admission shares the native transaction's rollback and permit.
//! The callback sees final projected rows, not a rematch or a committed read.

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergePolicy, GraphVertexUpsertPolicy, PreparedGraphInsertQueryText,
    PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Rights, Scope};

const NOW: u64 = 100;
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(K)),
        (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}

fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()));
}

async fn fixture(commit: &CommitCx) -> (Database<MemVfs>, Authority, CapabilityToken) {
    let namespace = DatabaseSecurityNamespaceId([0x39; 32]);
    let keys = DatabaseKeys::new([0x38; 32], namespace, [0x40; 32]);
    let mut db = Database::open_memory(commit, keys).await.unwrap();
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(
        VId(1),
        vec![L],
        vec![(K, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(10))],
    );
    db.write(commit, batch).await.unwrap();
    let authority = Authority::new(
        AuthKey::from_seed(0x391),
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
                labels: Scope::All,
                relations: Scope::All,
                properties: Scope::All,
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 1_000_000,
                    max_work: 100_000_000,
                    max_rows: 100_000,
                },
                expires_at_ms: 1000,
            },
            NOW,
        )
        .unwrap();
    (db, authority, token)
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Create,
    Mutation,
    Merge,
}

impl Kind {
    fn statement(self) -> &'static str {
        match self {
            Self::Create => "CREATE (n:Item {key:2,value:20}) RETURN n.value AS value",
            Self::Mutation => "MATCH (n:Item {key:1}) SET n.value=20 RETURN n.value AS value",
            Self::Merge => "MERGE (n:Item {key:1}) SET n.value=20 RETURN n.value AS value",
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn checked(
    db: &mut Database<MemVfs>,
    commit: &CommitCx,
    query: &QueryCx,
    txn: &TxnCx,
    authority: &Authority,
    token: &CapabilityToken,
    kind: Kind,
    limit_zero: bool,
    admit: impl FnOnce(
        &[GraphValueRow],
        &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
    ) -> Result<(), WriteTxnError>,
) -> Result<(Vec<GraphValueRow>, EmbeddedTxnCompletion), String> {
    let text = format!(
        "{}{}",
        kind.statement(),
        if limit_zero { " LIMIT 0" } else { "" }
    );
    let parameters = GqlParameters::new();
    match kind {
        Kind::Create => {
            let prepared = PreparedGraphInsertQueryText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&parameters)
                .unwrap();
            db.execute_graph_insert_query_authorized_checked(
                txn,
                query,
                commit,
                authority,
                token,
                "main",
                &prepared,
                GraphInsertPolicy::new(policy(), 100, 100),
                || NOW,
                admit,
            )
            .await
            .map(|(_, rows, completion)| (rows.value, completion))
            .map_err(|e| e.to_string())
        }
        Kind::Mutation => {
            let prepared = PreparedGraphMutationQueryText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&parameters)
                .unwrap();
            db.execute_graph_mutation_query_authorized_checked(
                txn,
                query,
                commit,
                authority,
                token,
                "main",
                &prepared,
                GraphMutationPolicy::new(policy(), 100),
                || NOW,
                admit,
            )
            .await
            .map(|(_, rows, completion)| (rows.value, completion))
            .map_err(|e| e.to_string())
        }
        Kind::Merge => {
            let prepared = PreparedGraphVertexUpsertQueryText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&parameters)
                .unwrap();
            db.execute_graph_vertex_upsert_query_authorized_checked(
                txn,
                query,
                commit,
                authority,
                token,
                "main",
                &prepared,
                GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy()), 100),
                || NOW,
                admit,
            )
            .await
            .map(|(_, _, rows, completion)| (rows.value, completion))
            .map_err(|e| e.to_string())
        }
    }
}

#[test]
fn refusing_any_native_result_discards_all_private_effects_and_pins() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            let (mut db, authority, token) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let mut calls = 0;
            let result = checked(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                kind,
                false,
                |rows, work| {
                    calls += 1;
                    assert_eq!(rows.len(), 1);
                    assert_eq!(
                        rows[0].values(),
                        &[GraphValue::Scalar(CanonicalScalar::Int(20))]
                    );
                    work(7)?;
                    Err(WriteTxnError::AuthorizedMutationRefused)
                },
            )
            .await;
            assert!(result.is_err(), "{kind:?}");
            assert_eq!(calls, 1);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn successful_admission_preserves_exact_rows_and_limit_zero_still_checks() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            for empty in [false, true] {
                let (mut db, authority, token) = fixture(commit).await;
                let before = db.frontier().unwrap();
                let mut admitted = None;
                let (rows, completion) = checked(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    kind,
                    empty,
                    |rows, work| {
                        assert!(admitted.is_none());
                        work(1)?;
                        admitted = Some(rows.to_vec());
                        Ok(())
                    },
                )
                .await
                .unwrap();
                assert_eq!(Some(rows.clone()), admitted);
                assert_eq!(rows.is_empty(), empty);
                assert!(matches!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted { .. }
                ));
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn admission_uses_the_same_live_work_budget_even_for_an_empty_result() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            let (mut db, authority, token) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let mut entered = false;
            assert!(
                checked(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    kind,
                    true,
                    |rows, work| {
                        entered = true;
                        assert!(rows.is_empty());
                        work(u64::MAX)
                    },
                )
                .await
                .is_err()
            );
            assert!(entered);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn issuer_retirement_inside_result_admission_is_not_followed_by_commit() {
    run(async |commit, query, txn| {
        for kind in [Kind::Create, Kind::Mutation, Kind::Merge] {
            let (mut db, authority, token) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            assert!(
                checked(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    kind,
                    false,
                    |_, work| {
                        authority.retire();
                        work(0)
                    },
                )
                .await
                .is_err()
            );
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}
