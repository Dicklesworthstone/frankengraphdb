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
    PreparedGraphWriteProgram, PreparedGraphWriteScript,
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
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
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

fn frozen_program(text: &str, arguments: &[GqlParameters]) -> PreparedGraphWriteProgram {
    PreparedGraphWriteScript::prepare(text, R, symbols)
        .unwrap()
        .bind_parameter_sets(arguments)
        .unwrap()
        .program()
        .clone()
}

fn keyed_arguments(keys: impl IntoIterator<Item = i64>) -> Vec<GqlParameters> {
    keys.into_iter()
        .map(|key| {
            GqlParameters::new()
                .with_int64("key", key)
                .unwrap()
                .with_int64("value", key * 10)
                .unwrap()
        })
        .collect()
}

fn batch_policy() -> GraphWriteProgramPolicy {
    // The server host's ordinary policy, not a benchmark-only larger ceiling.
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        100_000,
        100_000,
        100_000,
    )
}

#[test]
fn numerically_equal_batch_keys_keep_sequential_merge_visibility() {
    run(async |commit, query, txn| {
        let arguments = GqlParameters::new()
            .with_scalar("integer", CanonicalScalar::Int(100))
            .unwrap()
            .with_scalar(
                "float",
                CanonicalScalar::Float(fgdb_types::CanonicalF64::new(100.0)),
            )
            .unwrap();
        let declarations: Vec<_> = arguments.parameter_types().collect();
        let bind = |text: &str| {
            PreparedGraphWriteScript::prepare_with_parameter_types(text, R, &declarations, symbols)
                .unwrap()
                .bind_parameter_sets(std::slice::from_ref(&arguments))
                .unwrap()
                .into_program()
        };
        let program = bind(
            "MERGE (n:Item {key:$integer}) SET n.value=7; \
             MERGE (n:Item {key:$float}) SET n.value=8",
        );
        assert!(!program.freezes_vertex_upsert_input());
        let (mut db, authority) = fixture(commit).await;
        let token = authority.issue_at(&grant(100), NOW).unwrap();
        let (receipt, _) = execute(
            &mut db,
            commit,
            query,
            txn,
            &authority,
            &token,
            &program,
            policy(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(receipt.stats().created_vertices, 1);
        let created = receipt.steps()[0].merged_vertex().unwrap().vertex();
        assert_eq!(
            receipt.steps()[1].merged_vertex().unwrap().vertex(),
            created
        );
        assert!(
            db.vertex(created)
                .unwrap()
                .unwrap()
                .props
                .contains(&(VALUE, CanonicalScalar::Int(8).into()))
        );
        assert_eq!(db.vertices().unwrap().len(), 4);

        let program = bind(
            "MATCH (a:Item {key:1}), (b:Item {key:$integer}) MERGE (a)-[:R]->(b); \
             MATCH (a:Item {key:1}), (b:Item {key:$float}) MERGE (a)-[:R]->(b)",
        );
        assert!(!program.freezes_edge_merge_input());
        let (receipt, _) = execute(
            &mut db,
            commit,
            query,
            txn,
            &authority,
            &token,
            &program,
            policy(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(receipt.stats().created_edges, 2);
        assert_eq!(db.edges().unwrap().len(), 2);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn frozen_vertex_merges_equal_sequential_authorized_images_and_receipts() {
    run(async |commit, query, txn| {
        for text in [
            "MERGE (n:Item {key:$key}) SET n.value=$value",
            "MERGE (n:Item {key:$key}) ON CREATE SET n.value=$value \
             ON MATCH SET n.value=70 SET n.value=$value",
        ] {
            let frozen = frozen_program(text, &keyed_arguments([1, 2, 3, 4]));
            assert!(frozen.freezes_vertex_upsert_input());
            // The same bound statements through the existing sequential path
            // are the live oracle. No optimized output is used as a golden.
            let sequential =
                PreparedGraphWriteProgram::prepare(frozen.statements().to_vec()).unwrap();
            assert!(!sequential.freezes_vertex_upsert_input());
            let mut expected = None;
            for program in [&sequential, &frozen] {
                let (mut db, authority) = fixture(commit).await;
                let hidden = db.vertex(HIDDEN_ID).unwrap();
                let token = authority.issue_at(&grant(100), NOW).unwrap();
                let (receipt, end) = execute(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    program,
                    policy(),
                    true,
                )
                .await
                .unwrap();
                assert_eq!(receipt.stats().completed_statements, 4);
                assert_eq!(receipt.stats().created_vertices, 3);
                assert_eq!(receipt.steps()[0].merged_vertices().unwrap().len(), 2);
                assert_eq!(db.vertex(HIDDEN_ID).unwrap(), hidden);
                for vid in [FIRST, SECOND] {
                    let row = db.vertex(vid).unwrap().unwrap();
                    assert!(
                        row.props
                            .contains(&(SECRET, CanonicalScalar::Int(99).into()))
                    );
                    assert!(
                        row.props
                            .contains(&(VALUE, CanonicalScalar::Int(10).into()))
                    );
                }
                let actual = (db.vertices().unwrap(), receipt.steps().to_vec(), end);
                if let Some(expected) = &expected {
                    assert_eq!(&actual, expected, "{text}");
                } else {
                    expected = Some(actual);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn frozen_relationship_merges_equal_sequential_pairs_parallel_matches_and_births() {
    run(async |commit, query, txn| {
        let arguments: Vec<_> = [(1, 2), (2, 3), (3, 1), (3, 3)]
            .into_iter()
            .map(|(source, target)| {
                GqlParameters::new()
                    .with_int64("source", source)
                    .unwrap()
                    .with_int64("target", target)
                    .unwrap()
            })
            .collect();
        let frozen = frozen_program(
            "MATCH (a:Item {key:$source}), (b:Item {key:$target}) MERGE (a)-[:R]->(b)",
            &arguments,
        );
        assert!(frozen.freezes_edge_merge_input());
        let sequential = PreparedGraphWriteProgram::prepare(frozen.statements().to_vec()).unwrap();
        let mut expected = None;
        for program in [&sequential, &frozen] {
            let (mut db, authority) = fixture(commit).await;
            let mut seed = WriteBatch::new(R);
            for key in [2_i64, 3] {
                seed.create_vertex(
                    VId(key as u128),
                    vec![L],
                    vec![(KEY, CanonicalScalar::Int(key))],
                );
            }
            for eid in [1_u128, 2] {
                seed.add_edge(fgdb_types::EId(eid), FIRST, VId(2), vec![]);
            }
            db.write(commit, seed).await.unwrap();
            let token = authority.issue_at(&grant(100), NOW).unwrap();
            let (receipt, end) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                program,
                policy(),
                true,
            )
            .await
            .unwrap();
            assert_eq!(receipt.stats().created_edges, 5);
            assert_eq!(receipt.stats().completed_statements, 4);
            let actual = (
                db.vertices().unwrap(),
                db.edges().unwrap(),
                receipt.steps().to_vec(),
                end,
            );
            if let Some(expected) = &expected {
                assert_eq!(&actual, expected);
            } else {
                expected = Some(actual);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn frozen_merges_refuse_forbidden_original_tail_and_publish_nothing() {
    run(async |commit, query, txn| {
        let frozen = frozen_program(
            "MERGE (n:Item {key:$key}) ON CREATE SET n.value=$value \
             ON MATCH SET n.secret=$value SET n.value=10",
            &keyed_arguments([2, 1]),
        );
        assert!(frozen.freezes_vertex_upsert_input());
        let sequential = PreparedGraphWriteProgram::prepare(frozen.statements().to_vec()).unwrap();
        for program in [&sequential, &frozen] {
            let (mut db, authority) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let token = authority.issue_at(&grant(100), NOW).unwrap();
            let error = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                program,
                policy(),
                true,
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                GraphWriteProgramError::VertexUpsert { statement: 1, .. }
            ));
            let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
            let mut refusal = None;
            while let Some(error) = source {
                if let Some(WriteTxnError::Authorization(error)) =
                    error.downcast_ref::<WriteTxnError>()
                {
                    refusal = Some(*error);
                }
                source = error.source();
            }
            assert_eq!(refusal, Some(fgdb_warden::Error::ScopeDenied));
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn authorized_session_distinct_upserts_scale_to_four_thousand_under_host_limits() {
    run(async |commit, query, txn| {
        let mut measurements = Vec::new();
        for count in [1_000_usize, 2_000, 4_000] {
            let (mut db, authority) = fixture(commit).await;
            let before = db.frontier().unwrap();
            let mut limits = grant(0);
            limits.limits.max_work = 10_000_000;
            let token = authority.issue_at(&limits, NOW).unwrap();
            let arguments = keyed_arguments(100..100 + count as i64);
            let mut session = db
                .authorized_write_session(
                    txn,
                    commit,
                    &authority,
                    &token,
                    "main",
                    symbols,
                    R,
                    batch_policy(),
                    4_000,
                    || NOW,
                )
                .unwrap();
            let (created, created_end) = session
                .query_batch_stats(
                    query,
                    "MERGE (n:Item {key:$key}) SET n.value=$value",
                    &arguments,
                )
                .await
                .unwrap();
            let (matched, matched_end) = session
                .query_batch_stats(
                    query,
                    "MERGE (n:Item {key:$key}) SET n.value=$value",
                    &arguments,
                )
                .await
                .unwrap();
            drop(session);
            assert_eq!(created.created_vertices, count as u64);
            assert_eq!(matched.created_vertices, 0);
            assert_eq!(matched.target_vertex_visits, count as u64);
            assert_eq!(matched.selection.snapshot_records, count as u64);
            assert_eq!(db.vertices().unwrap().len(), count + 3);
            assert!(matches!(
                created_end,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            // Reapplying SET stages a write even when its final image is unchanged.
            assert!(matches!(
                matched_end,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(db.frontier().unwrap().0, before.0 + 2);
            measurements.push(matched.selection.snapshot_records);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        assert_eq!(measurements, [1_000, 2_000, 4_000]);
    });
}

#[test]
fn authorized_session_four_thousand_distinct_relationship_merges_fit_host_limits() {
    run(async |commit, query, txn| {
        let (mut db, authority) = fixture(commit).await;
        let mut seed = WriteBatch::new(R);
        for key in 100_i64..164 {
            seed.create_vertex(
                VId(key as u128),
                vec![L],
                vec![(KEY, CanonicalScalar::Int(key))],
            );
        }
        db.write(commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let arguments: Vec<_> = (0..4_000_i64)
            .map(|pair| {
                GqlParameters::new()
                    .with_int64("source", 100 + pair / 64)
                    .unwrap()
                    .with_int64("target", 100 + pair % 64)
                    .unwrap()
            })
            .collect();
        let text = "MATCH (a:Item {key:$source}), (b:Item {key:$target}) MERGE (a)-[:R]->(b)";
        let mut limits = grant(0);
        limits.limits.max_work = 10_000_000;
        let token = authority.issue_at(&limits, NOW).unwrap();
        let mut session = db
            .authorized_write_session(
                txn,
                commit,
                &authority,
                &token,
                "main",
                symbols,
                R,
                batch_policy(),
                4_000,
                || NOW,
            )
            .unwrap();
        let (created, _) = session
            .query_batch_stats(query, text, &arguments)
            .await
            .unwrap();
        let (matched, _) = session
            .query_batch_stats(query, text, &arguments)
            .await
            .unwrap();
        drop(session);
        assert_eq!(created.created_edges, 4_000);
        assert_eq!(matched.created_edges, 0);
        assert_eq!(matched.completed_statements, 4_000);
        assert_eq!(db.edges().unwrap().len(), 4_000);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[path = "authorized_vertex_merge_program/upserts.rs"]
mod upserts;
