//! Authorized rowless programs retain the complete visible MERGE target set.
//! These tests use real tokens, the native transaction and one completion.

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphVertexMergeOutcome,
    GraphWriteProgramError, GraphWriteProgramPolicy, GraphWriteProgramReceipt, GraphWriteStatement,
    GraphWriteStepReceipt, PreparedGraphInsertText, PreparedGraphVertexMergeText,
    PreparedGraphWriteProgram,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
    QueryCx, TxnCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const KEY: PropertyKeyId = PropertyKeyId(1);
const VALUE: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const FIRST: VId = VId(1);
const SECOND: VId = VId((1_u128 << 96) | 1);
const HIDDEN_ID: VId = VId((1_u128 << 100) | 1);
const NOW: u64 = 100;

type Fault = GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
        (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(VALUE)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 1_000_000),
        100,
        100,
        100,
    )
}

fn grant(max_rows: u64) -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::All,
        properties: Scope::only([KEY, VALUE]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 100_000,
            max_work: 100_000_000,
            max_rows,
        },
        expires_at_ms: 10_000,
    }
}

fn run(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(test(&contexts.commit(), &contexts.query(), &contexts.txn()));
}

async fn fixture(commit: &CommitCx) -> (Database<MemVfs>, Authority) {
    let namespace = DatabaseSecurityNamespaceId([0x49; 32]);
    let keys = DatabaseKeys::new([0x48; 32], namespace, [0x50; 32]);
    let mut db = Database::open_memory(commit, keys).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, label, value) in [(FIRST, L, 10), (SECOND, L, 20), (HIDDEN_ID, HIDDEN, 900)] {
        // The hidden vertex really satisfies the property-only MERGE predicate.
        batch.create_vertex(
            id,
            vec![label],
            vec![
                (KEY, CanonicalScalar::Int(1)),
                (VALUE, CanonicalScalar::Int(value)),
                (SECRET, CanonicalScalar::Int(99)),
            ],
        );
    }
    db.write(commit, batch).await.unwrap();
    let authority = Authority::new(
        AuthKey::from_seed(0x491),
        namespace,
        "test",
        SchemaEpoch(0),
        1,
    )
    .unwrap();
    (db, authority)
}

fn merge(text: &str) -> GraphWriteStatement {
    PreparedGraphVertexMergeText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}

fn insert(text: &str) -> GraphWriteStatement {
    PreparedGraphInsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    db: &mut Database<MemVfs>,
    commit: &CommitCx,
    query: &QueryCx,
    txn: &TxnCx,
    authority: &Authority,
    token: &CapabilityToken,
    program: &PreparedGraphWriteProgram,
    policy: GraphWriteProgramPolicy,
    returning: bool,
) -> Result<(GraphWriteProgramReceipt, EmbeddedTxnCompletion), Fault> {
    if returning {
        db.execute_graph_write_program_returning_authorized(
            txn,
            query,
            commit,
            authority,
            token,
            "main",
            program,
            policy,
            || NOW,
        )
        .await
    } else {
        db.execute_graph_write_program_authorized(
            txn,
            query,
            commit,
            authority,
            token,
            "main",
            program,
            policy,
            || NOW,
        )
        .await
        .map(|(stats, end)| (GraphWriteProgramReceipt::new(stats, Vec::new()), end))
    }
}

#[test]
fn plain_merge_keeps_full_visible_ids_and_stats_need_no_signed_rows() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            let (mut db, authority) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let token = authority
                .issue_at(&grant(if returning { 2 } else { 0 }), NOW)
                .unwrap();
            let program =
                PreparedGraphWriteProgram::prepare(vec![merge("MERGE (n {key:1})")]).unwrap();
            let (receipt, end) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &program,
                policy(),
                returning,
            )
            .await
            .unwrap();
            assert!(matches!(end, EmbeddedTxnCompletion::ReadClosed { .. }));
            assert_eq!(receipt.stats().completed_statements, 1);
            assert_eq!(receipt.stats().selection.result_rows, 2);
            assert_eq!(receipt.stats().created_vertices, 0);
            assert_eq!(receipt.stats().target_vertex_visits, 0);
            if returning {
                assert!(matches!(
                    receipt.steps(),
                    [GraphWriteStepReceipt::VertexMergeBatch { .. }]
                ));
                assert_eq!(
                    receipt.steps()[0].merged_vertices(),
                    Some(
                        &[
                            GraphVertexMergeOutcome::Matched(FIRST),
                            GraphVertexMergeOutcome::Matched(SECOND),
                        ][..]
                    )
                );
                assert_eq!(receipt.steps()[0].created_vertices(), Some(&[][..]));
                assert_eq!(receipt.steps()[0].merged_vertex(), None);
            } else {
                assert!(receipt.steps().is_empty());
            }
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn late_receipt_refusal_discards_the_creation_prefix_and_exact_retry_succeeds() {
    run(async |commit, query, txn| {
        let (mut db, authority) = fixture(commit).await;
        let before = (db.frontier().unwrap(), db.vertices().unwrap());
        let program = PreparedGraphWriteProgram::prepare(vec![
            insert("CREATE (n:Item {key:9})"),
            merge("MERGE (n {key:1})"),
        ])
        .unwrap();
        // The insertion spends one row; the two-match receipt must cost TWO.
        let short = authority.issue_at(&grant(2), NOW).unwrap();
        assert!(
            execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &short,
                &program,
                policy(),
                true,
            )
            .await
            .is_err()
        );
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
        let exact = authority.issue_at(&grant(3), NOW).unwrap();
        let (receipt, end) = execute(
            &mut db,
            commit,
            query,
            txn,
            &authority,
            &exact,
            &program,
            policy(),
            true,
        )
        .await
        .unwrap();
        assert!(matches!(end, EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
        assert_eq!(receipt.stats().created_vertices, 1);
        assert_eq!(receipt.steps()[1].merged_vertices().unwrap().len(), 2);
        assert_eq!(
            db.vertex(HIDDEN_ID).unwrap(),
            before.1.into_iter().find(|v| v.vid == HIDDEN_ID)
        );
    });
}

#[test]
fn missing_merge_creates_once_and_the_next_step_keeps_singular_receipts() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            let (mut db, authority) = fixture(commit).await;
            let before = db.frontier().unwrap();
            let token = authority
                .issue_at(&grant(if returning { 2 } else { 0 }), NOW)
                .unwrap();
            let program = PreparedGraphWriteProgram::prepare(vec![
                merge("MERGE (n:Item {key:2})"),
                merge("MERGE (n:Item {key:2})"),
            ])
            .unwrap();
            let (receipt, end) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &program,
                policy(),
                returning,
            )
            .await
            .unwrap();
            assert!(matches!(end, EmbeddedTxnCompletion::WriteCommitted { .. }));
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(db.vertices().unwrap().len(), 4);
            assert_eq!(receipt.stats().created_vertices, 1);
            assert_eq!(receipt.stats().selection.result_rows, 1);
            if returning {
                let created = receipt.steps()[0].merged_vertex().unwrap();
                assert!(matches!(created, GraphVertexMergeOutcome::Created(_)));
                assert_eq!(
                    receipt.steps()[1].merged_vertex(),
                    Some(GraphVertexMergeOutcome::Matched(created.vertex()))
                );
                assert!(
                    receipt
                        .steps()
                        .iter()
                        .all(|step| step.merged_vertices().is_none())
                );
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn plain_merge_source_and_evaluator_limits_accumulate_across_steps() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            let (mut db, authority) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let token = authority
                .issue_at(&grant(if returning { 4 } else { 0 }), NOW)
                .unwrap();
            let program = PreparedGraphWriteProgram::prepare(vec![
                merge("MERGE (n {key:1})"),
                merge("MERGE (n {key:1})"),
            ])
            .unwrap();
            let (receipt, _) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &program,
                policy(),
                returning,
            )
            .await
            .unwrap();
            let used = receipt.stats();
            let limits = [
                used.selection.snapshot_records,
                used.selection.result_rows,
                used.evaluator.work_units,
                used.evaluator.scratch_entries,
            ];
            assert!(limits.iter().all(|&limit| limit > 0));
            for reduced in 0..=4 {
                let mut allowed = limits;
                if reduced < 4 {
                    allowed[reduced] -= 1;
                }
                let [records, rows, work, scratch] = allowed;
                let bounded = GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(records, rows, work, scratch),
                    0,
                    0,
                    0,
                );
                let result = execute(
                    &mut db, commit, query, txn, &authority, &token, &program, bounded, returning,
                )
                .await;
                assert_eq!(
                    result.is_ok(),
                    reduced == 4,
                    "dimension {reduced}, receipts {returning}"
                );
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[path = "authorized_vertex_merge_program/upserts.rs"]
mod upserts;
