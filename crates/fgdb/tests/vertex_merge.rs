//! Unique vertex MERGE composes the canonical transaction MATCH overlay with the
//! existing standalone insertion path. These tests exercise both branches and
//! the scan witness that prevents a zero-match decision racing a matching create.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertVertex, PreparedGraphInsert};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GqlScalarParameter, GraphMutationValue,
    GraphSymbol, GraphSymbolKind, GraphVertexMergeError, GraphVertexMergeOutcome,
    GraphVertexMergePolicy, PreparedGraphText, PreparedGraphVertexMerge,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const MERGED: LabelId = LabelId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xf1; 32],
        DatabaseSecurityNamespaceId([0xf2; 32]),
        [0xf3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn scalar(value: i64) -> GqlScalarParameter {
    GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap()
}
fn creation(value: i64) -> PreparedGraphInsert {
    PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: vec![MERGED],
            properties: vec![(P, GraphMutationValue::Literal(scalar(value)))],
        }],
        vec![],
    )
    .unwrap()
}
fn merge(text: &str, value: i64) -> PreparedGraphVertexMerge {
    let selection = PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    PreparedGraphVertexMerge::prepare(selection, R, 0, creation(value)).unwrap()
}
fn policy() -> GraphVertexMergePolicy {
    GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000))
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
    batch.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(9))]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch.add_edge(EId(11), VId(1), VId(2), vec![]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn closed_time_keys_share_native_limits_and_preserve_unique_match_policy() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GlaLimitDimension, PreparedGraphVertexMergeText};
    use fgdb_types::CanonicalTimestamp;

    let ((), report) = run_async_under_lab(0x6e29_0048, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let key = GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(1))].into())
            .to_written_property()
            .unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex_values(VId(7), vec![], vec![(P, key.clone())]);
        db.write(&commit, batch).await.unwrap();
        let template =
            PreparedGraphVertexMergeText::prepare("MERGE (n {p:[1/timestamp()]})", R, symbols)
                .unwrap();
        let definition = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_statement_time(CanonicalTimestamp::offset_only(1_000_000, 0).unwrap())
                    .unwrap(),
            )
            .unwrap();
        let allocations = Cell::new(0);
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(100), vec![], vec![(P, CanonicalScalar::Int(77))]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        for (query_policy, dimension) in [
            (
                GqlQueryPolicy::new(0, 100, 0, 1_000_000),
                GlaLimitDimension::WorkUnits,
            ),
            (
                GqlQueryPolicy::new(0, 100, 1_000_000, 0),
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            let error = txn
                .execute_graph_vertex_merge_governed(
                    &mut db,
                    &query,
                    &definition,
                    GraphVertexMergePolicy::new(query_policy),
                    |_| {
                        allocations.set(allocations.get() + 1);
                        Ok::<_, ()>(ElementId::Vertex(VId(1000)))
                    },
                )
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Evaluator(error)
                if error.dimension == dimension && error.limit == 0 && error.observed == 1),
                "closed-key admission precedes the zero-record source allowance: {error:?}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
        }
        let (stats, outcome) = txn
            .execute_graph_vertex_merge_governed(&mut db, &query, &definition, policy(), |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Vertex(VId(1000)))
            })
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(7)));
        assert!(stats.evaluator.work_units > 0 && stats.evaluator.scratch_entries > 0);
        for (work, scratch, dimension, limit) in [
            (
                stats.evaluator.work_units - 1,
                stats.evaluator.scratch_entries,
                GlaLimitDimension::WorkUnits,
                stats.evaluator.work_units - 1,
            ),
            (
                stats.evaluator.work_units,
                stats.evaluator.scratch_entries - 1,
                GlaLimitDimension::ScratchEntries,
                stats.evaluator.scratch_entries - 1,
            ),
        ] {
            let error = txn
                .execute_graph_vertex_merge_governed(
                    &mut db,
                    &query,
                    &definition,
                    GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, work, scratch)),
                    |_| {
                        allocations.set(allocations.get() + 1);
                        Ok::<_, ()>(ElementId::Vertex(VId(1000)))
                    },
                )
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Evaluator(error)
                if error.dimension == dimension && error.limit == limit
                    && error.observed == u128::from(limit) + 1),
                "{error:?}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
        }
        assert_eq!(allocations.get(), 0);
        txn.abort();
        assert!(db.vertex(VId(100)).unwrap().is_none());
        let mut duplicate = WriteBatch::new(R);
        duplicate.create_vertex_values(VId(8), vec![], vec![(P, key)]);
        db.write(&commit, duplicate).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let error = txn
            .execute_graph_vertex_merge_governed(&mut db, &query, &definition, policy(), |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Vertex(VId(1000)))
            })
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphVertexMergeError::AmbiguousMatches { observed: 2 })
        ));
        assert_eq!(allocations.get(), 0);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_match_occurrences_collapse_to_one_existing_vertex_without_allocation() {
    let ((), report) = run_async_under_lab(0x6e29_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let definition = merge("MATCH (n)-[:R]->(m) WHERE n.p = 7 RETURN ALL n", 7);
        let allocations = Cell::new(0);
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_merge_governed(&mut db, &query, &definition, policy(), |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Vertex(VId(100)))
            })
            .unwrap();
        assert_eq!(
            stats.match_selection.result_rows, 2,
            "parallel edges preserve MATCH multiplicity"
        );
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(outcome.vertex(), VId(1));
        assert!(!outcome.created());
        assert_eq!(
            allocations.get(),
            0,
            "matched MERGE must not ask for an identity"
        );
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn two_distinct_matches_refuse_before_allocator_or_workspace_change() {
    let ((), report) = run_async_under_lab(0x6e29_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut duplicate = WriteBatch::new(R);
        duplicate.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(7)));
        db.write(&commit, duplicate).await.unwrap();
        let definition = merge("MATCH (n) WHERE n.p = 7 RETURN ALL n", 7);
        let allocations = Cell::new(0);
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let result =
            txn.execute_graph_vertex_merge_governed(&mut db, &query, &definition, policy(), |_| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(ElementId::Vertex(VId(100)))
            });
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(
                GraphVertexMergeError::AmbiguousMatches { observed: 2 }
            ))
        ));
        assert_eq!(allocations.get(), 0);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_match_creates_exactly_one_vertex_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0x6e29_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let before = db.frontier().unwrap();
        let definition = merge("MATCH (n) WHERE n.p = 42 RETURN ALL n", 42);
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, outcome) = txn
            .execute_graph_vertex_merge_governed(
                &mut db,
                &query,
                &definition,
                policy(),
                |request| {
                    assert_eq!(
                        request,
                        fgdb_gql::insertion::GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    );
                    Ok::<_, ()>(ElementId::Vertex(VId(100)))
                },
            )
            .unwrap();
        assert_eq!(stats.match_selection.result_rows, 0);
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(100)));
        let staged = txn.vertex(&db, VId(100)).unwrap().unwrap();
        assert_eq!(staged.labels, vec![MERGED]);
        assert_eq!(staged.props, vec![(P, CanonicalScalar::Int(42).into())]);
        assert!(db.vertex(VId(100)).unwrap().is_none());
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.frontier().unwrap().0 > before.0);
        assert!(db.vertex(VId(100)).unwrap().is_some());
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let row = db.vertex(VId(100)).unwrap().unwrap();
        assert_eq!(row.props, vec![(P, CanonicalScalar::Int(42).into())]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn allocator_collision_refuses_without_partial_creation() {
    let ((), report) = run_async_under_lab(0x6e29_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let definition = merge("MATCH (n) WHERE n.p = 42 RETURN ALL n", 42);
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let result =
            txn.execute_graph_vertex_merge_governed(&mut db, &query, &definition, policy(), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(1)))
            });
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphVertexMergeError::Creation(_)))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(1)).unwrap().is_some());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn concurrent_matching_create_invalidates_zero_match_merge_at_completion() {
    let ((), report) = run_async_under_lab(0x6e29_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let definition = merge("MATCH (n) WHERE n.p = 42 RETURN ALL n", 42);

        let mut first = db.begin(&txcx).unwrap();
        let (_, outcome) = first
            .execute_graph_vertex_merge_governed(&mut db, &query, &definition, policy(), |_| {
                Ok::<_, ()>(ElementId::Vertex(VId(100)))
            })
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(100)));

        let mut competitor = db.begin(&txcx).unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(200), vec![MERGED], vec![(P, CanonicalScalar::Int(42))]);
        competitor.write(&mut db, batch).unwrap();
        competitor.commit(&mut db, &commit).await.unwrap();

        assert!(
            first.commit(&mut db, &commit).await.is_err(),
            "zero-match scan witness must prevent a concurrent matching create from serializing before MERGE"
        );
        assert!(db.vertex(VId(100)).unwrap().is_none());
        assert!(db.vertex(VId(200)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
