//! All-target vertex MERGE must be reachable without RETURN through the actual
//! script, UNWIND and public transaction owners, with one cumulative meter.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    BoundNativeGraphWrite, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphMutationProgramDimension, GraphMutationProgramError, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeOutcome, GraphVertexUpsertError, GraphWriteIdentityRequest,
    GraphWriteProgramError, GraphWriteProgramPolicy, GraphWriteStepReceipt,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const IDS: [VId; 2] = [VId(1), VId((1_u128 << 100) | 1)];
const SCRIPT: &str = "MERGE (n:N {p:1}); \
    MERGE (n:N {p:1}) ON MATCH SET n.p=2,n.q=n.q+1 SET n.q=n.q*2; \
    MERGE (n:N {p:2}) ON MATCH SET n.q=n.q+5";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "N") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(rows: u64, effects: u64, creates: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, rows, 5_000_000, 2_000_000),
        effects,
        creates,
        0,
    )
}

fn no_allocate(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("nonempty vertex matches must not allocate")
}

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, p, q) in [(IDS[0], 1, 3), (IDS[1], 1, 7), (VId(90), 9, 20)] {
        batch.create_vertex(
            id,
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
        );
    }
    batch
}

fn assert_targets(db: &Database<MemVfs>, key: i64, values: [i64; 2]) {
    for (id, value) in IDS.into_iter().zip(values) {
        assert_eq!(
            db.vertex(id).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(key).into()),
                (Q, CanonicalScalar::Int(value).into())
            ],
        );
    }
    assert_eq!(
        db.vertex(VId(90)).unwrap().unwrap().props,
        vec![
            (P, CanonicalScalar::Int(9).into()),
            (Q, CanonicalScalar::Int(20).into())
        ],
    );
}

#[test]
fn mixed_scripts_keep_complete_receipts_and_stats_and_publish_one_durable_commit() {
    let ((), report) = run_async_under_lab(0xc9e4_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let params = GqlParameters::new();
        let bound = BoundNativeGraphWrite::bind_with_resolver(SCRIPT, &params, R, symbols).unwrap();
        let budget = policy(6, 8, 0);
        let mut txn = db.begin(&txcx).unwrap();
        let plain_stats = txn
            .execute_graph_write_program_governed(
                &mut db,
                &cx,
                bound.program(),
                budget,
                no_allocate,
            )
            .unwrap();
        txn.abort();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                bound.program(),
                budget,
                no_allocate,
            )
            .unwrap();
        assert_eq!(receipt.stats(), plain_stats);
        assert_eq!(receipt.stats().completed_statements, 3);
        assert_eq!(receipt.stats().selection.result_rows, 6);
        assert_eq!(receipt.stats().mutation_effects, 8);
        assert_eq!(receipt.stats().target_vertex_visits, 4);
        assert_eq!(receipt.stats().created_vertices, 0);
        assert!(matches!(
            &receipt.steps()[0],
            GraphWriteStepReceipt::VertexMergeBatch { .. }
        ));
        assert!(matches!(
            &receipt.steps()[1],
            GraphWriteStepReceipt::VertexUpsertBatch { .. }
        ));
        assert!(matches!(
            &receipt.steps()[2],
            GraphWriteStepReceipt::VertexUpsertBatch { .. }
        ));
        let outcomes = IDS.map(GraphVertexMergeOutcome::Matched);
        for step in receipt.steps() {
            assert_eq!(step.merged_vertices(), Some(outcomes.as_slice()));
            assert_eq!(step.created_vertices(), Some(&[][..]));
            assert_eq!(step.merged_vertex(), None);
        }
        txn.abort();
        assert_eq!(db.frontier().unwrap(), before);
        assert_targets(&db, 1, [3, 7]);
        let actual = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                SCRIPT,
                &params,
                symbols,
                R,
                budget,
                no_allocate,
            )
            .await
            .unwrap();
        let QueryResult::Write {
            receipt: actual,
            completion,
        } = actual
        else {
            panic!("no-RETURN script must expose a write receipt")
        };
        assert_eq!(actual, receipt);
        assert!(completion.is_some());
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_targets(&db, 2, [13, 21]);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_targets(&db, 2, [13, 21]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_script_budgets_and_expression_failure_restore_the_whole_program_prefix() {
    const TWO: &str = "MERGE (n:N {p:1}) ON MATCH SET n.q=n.q+1; \
        MERGE (n:N {p:1}) ON MATCH SET n.q=n.q+1";
    let ((), report) = run_async_under_lab(0xc9e4_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let bound =
            BoundNativeGraphWrite::bind_with_resolver(TWO, &GqlParameters::new(), R, symbols)
                .unwrap();
        let prefix = || {
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(91), vec![], vec![]);
            batch
        };
        // Successful execution is the control for all quota-refusal cases.
        let mut preview = db.begin(&txcx).unwrap();
        preview.write(&mut db, prefix()).unwrap();
        let observed = preview
            .execute_graph_write_program_governed(
                &mut db,
                &cx,
                bound.program(),
                policy(4, 4, 0),
                no_allocate,
            )
            .unwrap();
        preview.abort();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, prefix()).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let mut work = policy(4, 4, 0);
        work.mutations.query.evaluator.max_work_units = observed.evaluator.work_units - 1;
        let mut scratch = policy(4, 4, 0);
        scratch.mutations.query.evaluator.max_scratch_entries =
            observed.evaluator.scratch_entries - 1;
        for (budget, dimension) in [
            (policy(4, 3, 0), GraphMutationProgramDimension::Effects),
            (policy(3, 4, 0), GraphMutationProgramDimension::SelectedRows),
            (work, GraphMutationProgramDimension::WorkUnits),
            (scratch, GraphMutationProgramDimension::ScratchEntries),
        ] {
            let error = txn
                .execute_graph_write_program_returning_governed(
                    &mut db,
                    &cx,
                    bound.program(),
                    budget,
                    no_allocate,
                )
                .unwrap_err();
            assert!(matches!(
                error,
                GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                    dimension: actual, ..
                }) if actual == dimension
            ));
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), before);
            assert_targets(&db, 1, [3, 7]);
        }
        let failing = BoundNativeGraphWrite::bind_with_resolver(
            "MERGE (n:N {p:1}) ON MATCH SET n.q=n.q+1; \
             MERGE (n:N {p:1}) ON MATCH SET n.q=n.q-8 SET n.q=10/n.q",
            &GqlParameters::new(),
            R,
            symbols,
        )
        .unwrap();
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                failing.program(),
                policy(4, 6, 0),
                no_allocate,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::VertexUpsert {
                statement: 1,
                source: GqlQueryError::Source(GraphVertexUpsertError::Expression { .. }),
            }
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert!(db.vertex(VId(91)).unwrap().is_some());
        assert_targets(&db, 1, [3, 7]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_return_creations_keep_statement_coordinates_and_cumulative_creation_caps() {
    const CREATES: &str = "MERGE (n:N {p:1}); \
        MERGE (n:N {p:7}) ON CREATE SET n.q=4; \
        MERGE (n:N {p:7}) ON MATCH SET n.q=n.q+1; \
        MERGE (n:N {p:8})";
    let ((), report) = run_async_under_lab(0xc9e4_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let bound =
            BoundNativeGraphWrite::bind_with_resolver(CREATES, &GqlParameters::new(), R, symbols)
                .unwrap();
        let next = AtomicU64::new(100);
        let calls = AtomicU64::new(0);
        let mut allocate = |request: GraphWriteIdentityRequest| -> Result<ElementId, ()> {
            let call = calls.fetch_add(1, Ordering::Relaxed);
            // The failed program spends one ID. Successful retry gets two new
            // IDs, never reusing the first or allocating at a matched step.
            assert_eq!(request.statement, if call == 2 { 3 } else { 1 });
            assert!(matches!(
                request.request,
                GraphInsertRequest::Vertex { row: 0, vertex: 0 }
            ));
            Ok(ElementId::Vertex(VId(u128::from(
                next.fetch_add(1, Ordering::Relaxed),
            ))))
        };
        let mut txn = db.begin(&txcx).unwrap();
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                bound.program(),
                policy(10, 2, 1),
                &mut allocate,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::CreationBudget {
                statement: 3,
                dimension: GraphInsertLimitDimension::Vertices,
                limit: 1,
                observed: 2,
            }
        ));
        assert_eq!(next.load(Ordering::Relaxed), 101);
        assert_eq!(txn.vertices(&db).unwrap().len(), 3);
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                bound.program(),
                policy(10, 2, 2),
                &mut allocate,
            )
            .unwrap();
        assert_eq!(receipt.stats().created_vertices, 2);
        assert_eq!(receipt.stats().mutation_effects, 2);
        assert_eq!(receipt.stats().target_vertex_visits, 2);
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(101)))
        );
        assert_eq!(
            receipt.steps()[2].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(VId(101)))
        );
        assert_eq!(
            receipt.steps()[3].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(102)))
        );
        assert_eq!(receipt.steps()[1].created_vertices(), Some(&[VId(101)][..]));
        assert_eq!(receipt.steps()[2].created_vertices(), Some(&[][..]));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 5);
        assert!(db.vertex(VId(100)).unwrap().is_none());
        assert_eq!(
            db.vertex(VId(101)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(7).into()),
                (Q, CanonicalScalar::Int(5).into())
            ],
        );
        assert_targets(&db, 1, [3, 7]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unwind_occurrences_each_update_the_complete_target_set_without_per_row_commits() {
    const UNWIND: &str = "UNWIND $steps AS delta \
        MERGE (n:N {p:1}) ON MATCH SET n.q=n.q+delta";
    let ((), report) = run_async_under_lab(0xc9e4_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let params = GqlParameters::new()
            .with_list(
                "steps",
                vec![
                    GraphValue::Scalar(CanonicalScalar::Int(1)),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                ],
            )
            .unwrap();
        let bound = BoundNativeGraphWrite::bind_with_resolver(UNWIND, &params, R, symbols).unwrap();
        assert_eq!(bound.program().statements().len(), 2);
        let mut txn = db.begin(&txcx).unwrap();
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &cx,
                bound.program(),
                policy(4, 3, 0),
                no_allocate,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                statement: 1,
                dimension: GraphMutationProgramDimension::Effects,
                limit: 3,
                observed: 4,
            })
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_targets(&db, 1, [3, 7]);
        let actual = txn
            .query_write(
                &mut db,
                &cx,
                UNWIND,
                &params,
                symbols,
                R,
                policy(4, 4, 0),
                no_allocate,
            )
            .unwrap();
        let QueryResult::Write {
            receipt,
            completion,
        } = actual
        else {
            panic!("UNWIND must retain its native program receipt")
        };
        assert!(completion.is_none());
        assert_eq!(receipt.stats().completed_statements, 2);
        assert_eq!(receipt.stats().selection.result_rows, 4);
        assert_eq!(receipt.stats().mutation_effects, 4);
        assert_eq!(receipt.stats().target_vertex_visits, 4);
        let outcomes = IDS.map(GraphVertexMergeOutcome::Matched);
        for step in receipt.steps() {
            assert_eq!(step.merged_vertices(), Some(outcomes.as_slice()));
        }
        assert_eq!(db.frontier().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_targets(&db, 1, [6, 10]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
