//! End-to-end idempotent graph ingestion: create/find vertices and relationships
//! through native prepared text, one canonical overlay and one atomic program.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateValue, GraphEdgeMergeError,
    GraphEdgeMergeOutcome, GraphMutationProgramDimension, GraphMutationProgramError, GraphSymbol,
    GraphSymbolKind, GraphVertexMergeOutcome, GraphWriteIdentityRequest, GraphWriteProgramError,
    GraphWriteProgramPolicy, PreparedGraphEdgeMergeText, PreparedGraphVertexMergeText,
    PreparedGraphWriteProgram, PreparedGraphWriteProgramTemplate,
};
use fgdb_types::{
    CanonicalList, CanonicalPropertyValue, CanonicalScalar, DatabaseSecurityNamespaceId, EId,
    EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const TAGS: PropertyKeyId = PropertyKeyId(3);
const PERSON: LabelId = LabelId(1);
const EDGE_TEXT: &str =
    "MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "tags") => Some(GraphSymbol::Property(TAGS)),
        _ => None,
    }
}
fn arguments() -> GqlParameters {
    GqlParameters::new()
        .with_int64("left", 7)
        .unwrap()
        .with_int64("right", 8)
        .unwrap()
}
fn program() -> PreparedGraphWriteProgram {
    let left =
        PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, symbols).unwrap();
    let right =
        PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$right})", R, symbols).unwrap();
    let edge = PreparedGraphEdgeMergeText::prepare(EDGE_TEXT, R, symbols).unwrap();
    PreparedGraphWriteProgramTemplate::prepare(vec![
        left.into(),
        right.into(),
        edge.clone().into(),
        edge.into(),
    ])
    .unwrap()
    .bind_parameters(&arguments())
    .unwrap()
}
fn policy(vertices: u64, edges: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        0,
        vertices,
        edges,
    )
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(99))]);
    db.write(cx, batch).await.unwrap();
}

fn pairs_program() -> PreparedGraphWriteProgram {
    let merge = PreparedGraphEdgeMergeText::prepare(
        "MATCH (a:Person)-[:R]->(b:Person) MERGE (a)-[:S {p:7}]->(b)",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    PreparedGraphWriteProgram::prepare(vec![merge.into()]).unwrap()
}

async fn seed_pairs(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 {
        batch.create_vertex(VId(id), vec![PERSON], vec![]);
    }
    // Deliberately different edge and endpoint order, with a repeated pair.
    for (edge, destination) in [(11, 4), (12, 2), (13, 3), (14, 2)] {
        batch.add_edge(EId(edge), VId(1), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    // The MERGE map {p:7} is a match predicate (fgdb-at06q): edge 21 matches
    // on p and keeps its property outside the map unchanged.
    let mut existing = WriteBatch::new(S);
    existing.add_edge(EId(21), VId(1), VId(3), existing_props());
    db.write(cx, existing).await.unwrap();
}

fn existing_props() -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![
        (P, CanonicalScalar::Int(7)),
        (PropertyKeyId(2), CanonicalScalar::Int(99)),
    ]
}

#[test]
fn all_pairs_merge_deduplicates_rows_preserves_pair_slots_and_is_idempotent() {
    let ((), report) = run_async_under_lab(0xed9e_4010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed_pairs(&mut db, &commit).await;
        let program = pairs_program();
        let mut txn = db.begin(&txcx).unwrap();
        let mut requests = Vec::new();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(0, 2),
                |request| {
                    assert_eq!(request.statement, 0);
                    requests.push(request.request);
                    let GraphInsertRequest::Edge { row, edge: 0 } = request.request else {
                        panic!("plain relationship MERGE only allocates its edge slot");
                    };
                    Ok::<_, ()>(ElementId::Edge(EId(30 + row as u128)))
                },
            )
            .unwrap();
        assert_eq!(
            requests,
            [
                GraphInsertRequest::Edge { row: 0, edge: 0 },
                GraphInsertRequest::Edge { row: 2, edge: 0 },
            ]
        );
        assert_eq!(receipt.stats().selection.result_rows, 4);
        assert_eq!(receipt.stats().created_edges, 2);
        assert_eq!(receipt.steps()[0].merged_edge(), None);
        assert_eq!(
            receipt.steps()[0].merged_edges(),
            Some(
                &[
                    GraphEdgeMergeOutcome::Created(EId(30)),
                    GraphEdgeMergeOutcome::Matched(EId(21)),
                    GraphEdgeMergeOutcome::Created(EId(32)),
                ][..]
            )
        );
        assert_eq!(
            receipt.steps()[0].created_edges(),
            Some(&[EId(30), EId(32)][..])
        );
        // The second execution must see the complete staged batch and spend no
        // additional creation allowance, before anything has been committed.
        let staged = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(0, 0),
                |_| -> Result<ElementId, ()> { panic!("staged matches cannot allocate") },
            )
            .unwrap();
        assert_eq!(staged.stats().created_edges, 0);
        assert_eq!(
            staged.steps()[0].merged_edges(),
            Some(
                &[
                    GraphEdgeMergeOutcome::Matched(EId(30)),
                    GraphEdgeMergeOutcome::Matched(EId(21)),
                    GraphEdgeMergeOutcome::Matched(EId(32)),
                ][..]
            )
        );
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        for (edge, destination) in [(30, 2), (32, 4)] {
            let record = db.edge(EId(edge)).unwrap().unwrap();
            assert_eq!(
                (record.entry.src, record.entry.dst, record.entry.relation),
                (VId(1), VId(destination), S)
            );
            assert_eq!(record.props, vec![(P, CanonicalScalar::Int(7).into())]);
        }
        // Edge 21 matched on p and keeps its field outside the map.
        let stored: Vec<(PropertyKeyId, fgdb_types::CanonicalPropertyValue)> = existing_props()
            .into_iter()
            .map(|(key, value)| (key, value.into()))
            .collect();
        assert_eq!(db.edge(EId(21)).unwrap().unwrap().props, stored);
        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let repeated = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(0, 0),
                |_| -> Result<ElementId, ()> { panic!("committed matches cannot allocate") },
            )
            .unwrap();
        assert_eq!(repeated.steps(), staged.steps());
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_pair_creation_limit_preserves_prefix_and_allows_a_complete_retry() {
    let ((), report) = run_async_under_lab(0xed9e_4011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed_pairs(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(S);
        prefix.create_vertex(VId(200), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut requests = Vec::new();
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &pairs_program(),
                policy(0, 1),
                |request| {
                    requests.push(request.request);
                    Ok::<_, ()>(ElementId::Edge(EId(30)))
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::CreationBudget {
                statement: 0,
                dimension: GraphInsertLimitDimension::Edges,
                limit: 1,
                observed: 2,
            }
        ));
        assert_eq!(requests, [GraphInsertRequest::Edge { row: 0, edge: 0 }]);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &pairs_program(),
                policy(0, 2),
                |request| {
                    let GraphInsertRequest::Edge { row, edge: 0 } = request.request else {
                        panic!("edge allocation");
                    };
                    Ok::<_, ()>(ElementId::Edge(EId(40 + row as u128)))
                },
            )
            .unwrap();
        assert_eq!(
            receipt.steps()[0].created_edges(),
            Some(&[EId(40), EId(42)][..])
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(200)).unwrap().is_some());
        assert!(db.edge(EId(30)).unwrap().is_none());
        assert!(db.edge(EId(40)).unwrap().is_some());
        assert!(db.edge(EId(42)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_pair_source_work_and_scratch_share_exact_program_boundaries() {
    let ((), report) = run_async_under_lab(0xed9e_4014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed_pairs(&mut db, &commit).await;
        let program = pairs_program();
        let allocate = |request: fgdb_gql::GraphWriteIdentityRequest| {
            let GraphInsertRequest::Edge { row, edge: 0 } = request.request else {
                panic!("edge allocation");
            };
            Ok::<_, ()>(ElementId::Edge(EId(30 + row as u128)))
        };
        let mut measured = db.begin(&txcx).unwrap();
        let expected = measured
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(0, 2),
                allocate,
            )
            .unwrap()
            .stats();
        measured.abort();
        let frontier = db.frontier().unwrap();
        for short in [
            None,
            Some(GraphMutationProgramDimension::SnapshotRecords),
            Some(GraphMutationProgramDimension::WorkUnits),
            Some(GraphMutationProgramDimension::ScratchEntries),
        ] {
            let records = expected.selection.snapshot_records
                - u64::from(short == Some(GraphMutationProgramDimension::SnapshotRecords));
            let work = expected.evaluator.work_units
                - u64::from(short == Some(GraphMutationProgramDimension::WorkUnits));
            let scratch = expected.evaluator.scratch_entries
                - u64::from(short == Some(GraphMutationProgramDimension::ScratchEntries));
            let exact = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(records, 100_000, work, scratch),
                0,
                0,
                2,
            );
            let mut txn = db.begin(&txcx).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            // Stats-only and returning routes must pay the same batch costs.
            let result = txn
                .execute_graph_write_program_governed(&mut db, &query, &program, exact, allocate);
            if let Some(expected_dimension) = short {
                assert!(matches!(result,
                    Err(GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
                        dimension, ..
                    })) if dimension == expected_dimension));
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
            } else {
                assert_eq!(result.unwrap(), expected);
            }
            txn.abort();
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.edge(EId(30)).unwrap().is_none());
            assert!(db.edge(EId(32)).unwrap().is_none());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A program keeps every parallel match of a pair (fgdb-at06q), so a pair
/// with two matching edges is two matches, not an ambiguity. Either way the
/// later pair's allocator refusal stages nothing and keeps the negative
/// witnesses after rollback.
#[test]
fn parallel_matches_and_a_late_allocator_refusal_keep_negative_witnesses_after_rollback() {
    for parallel in [false, true] {
        let ((), report) =
            run_async_under_lab(0xed9e_4012 + u64::from(parallel), move |root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let query = contexts.query();
                let txcx = contexts.txn();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed_pairs(&mut db, &commit).await;
                if parallel {
                    let mut duplicate = WriteBatch::new(S);
                    duplicate.add_edge(EId(22), VId(1), VId(3), existing_props());
                    db.write(&commit, duplicate).await.unwrap();
                }
                let mut txn = db.begin(&txcx).unwrap();
                let mut prefix = WriteBatch::new(S);
                prefix.create_vertex(VId(200), vec![], vec![]);
                txn.write(&mut db, prefix).unwrap();
                txn.savepoint(&db, "before-pairs").unwrap();
                let before = txn.staged_effect_digest().unwrap();
                let mut requests = Vec::new();
                let error = txn
                    .execute_graph_write_program_governed(
                        &mut db,
                        &query,
                        &pairs_program(),
                        policy(0, 2),
                        |request| {
                            requests.push(request.request);
                            match request.request {
                                GraphInsertRequest::Edge { row: 0, edge: 0 } => {
                                    Ok(ElementId::Edge(EId(30)))
                                }
                                GraphInsertRequest::Edge { row: 2, edge: 0 } => {
                                    Err("late-allocator-refusal")
                                }
                                _ => panic!("matched pair cannot allocate"),
                            }
                        },
                    )
                    .unwrap_err();
                assert!(matches!(
                    error,
                    GraphWriteProgramError::EdgeMerge {
                        statement: 0,
                        source: GqlQueryError::Source(GraphEdgeMergeError::IdentitySource(
                            "late-allocator-refusal"
                        )),
                    }
                ));
                // Only the two missing pairs ask for an identity; the matched
                // pair (one or two parallel edges) never allocates.
                assert_eq!(requests.len(), 2);
                assert_eq!(txn.staged_effect_digest().unwrap(), before);
                txn.rollback_to_savepoint(&db, "before-pairs").unwrap();
                let mut competing = WriteBatch::new(S);
                competing.add_edge(EId(90), VId(1), VId(2), vec![]);
                db.write(&commit, competing).await.unwrap();
                let frontier = db.frontier().unwrap();
                assert!(txn.finish(&mut db, &commit).await.is_err());
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.edge(EId(30)).unwrap().is_none());
                assert!(db.edge(EId(90)).unwrap().is_some());
                assert!(db.vertex(VId(200)).unwrap().is_none());
                assert_eq!(txcx.outstanding_obligations(), 0);
            });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[test]
fn native_vertex_and_relationship_program_is_idempotent_and_returns_every_outcome() {
    let ((), report) = run_async_under_lab(0xed9e_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let program = program();
        let mut txn = db.begin(&txcx).unwrap();
        let mut requests = Vec::new();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(2, 1),
                |request| {
                    requests.push(request);
                    Ok::<_, ()>(match (request.statement, request.request) {
                        (0, GraphInsertRequest::Vertex { row: 0, vertex: 0 }) => {
                            ElementId::Vertex(VId(10))
                        }
                        (1, GraphInsertRequest::Vertex { row: 0, vertex: 0 }) => {
                            ElementId::Vertex(VId(11))
                        }
                        (2, GraphInsertRequest::Edge { row: 0, edge: 0 }) => {
                            ElementId::Edge(EId(12))
                        }
                        _ => panic!("matched relationship must not allocate again"),
                    })
                },
            )
            .unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            (
                receipt.stats().completed_statements,
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (4, 2, 1)
        );
        assert_eq!(
            receipt.steps()[0].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(10)))
        );
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(11)))
        );
        assert_eq!(
            receipt.steps()[2].merged_edge(),
            Some(GraphEdgeMergeOutcome::Created(EId(12)))
        );
        assert_eq!(
            receipt.steps()[3].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(EId(12)))
        );
        assert_eq!(receipt.steps()[2].created_edges(), Some(&[EId(12)][..]));
        assert_eq!(receipt.steps()[3].created_edges(), Some(&[][..]));
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let edge = db.edge(EId(12)).unwrap().unwrap();
        assert_eq!(
            (edge.entry.src, edge.entry.dst, edge.entry.relation),
            (VId(10), VId(11), R)
        );

        let frontier = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(0, 0),
                |_| -> Result<ElementId, ()> { panic!("idempotent ingestion must not allocate") },
            )
            .unwrap();
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (0, 0)
        );
        assert_eq!(
            receipt.steps()[0].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(VId(10)))
        );
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Matched(VId(11)))
        );
        for step in &receipt.steps()[2..] {
            assert_eq!(
                step.merged_edge(),
                Some(GraphEdgeMergeOutcome::Matched(EId(12)))
            );
        }
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relationship_quota_refusal_rolls_back_both_new_vertices_and_preserves_outer_prefix() {
    let ((), report) = run_async_under_lab(0xed9e_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(200), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let mut allocations = 0;
        let error = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program(),
                policy(2, 0),
                |request| {
                    assert!(
                        matches!(request.request, GraphInsertRequest::Vertex { .. }),
                        "edge quota is checked before requesting an edge identity"
                    );
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(10 + request.statement as u128)))
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::CreationBudget {
                statement: 2,
                dimension: GraphInsertLimitDimension::Edges,
                limit: 0,
                observed: 1,
            }
        ));
        assert_eq!(allocations, 2);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(200)).unwrap().is_some());
        assert!(db.vertex(VId(10)).unwrap().is_none());
        assert!(db.vertex(VId(11)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_endpoint_input_has_an_explicit_receipt_and_does_not_stop_later_statements() {
    let ((), report) = run_async_under_lab(0xed9e_4003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let edge = PreparedGraphEdgeMergeText::prepare(EDGE_TEXT, R, symbols).unwrap();
        let vertex =
            PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, symbols)
                .unwrap();
        let program = PreparedGraphWriteProgramTemplate::prepare(vec![edge.into(), vertex.into()])
            .unwrap()
            .bind_parameters(&arguments())
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                policy(1, 0),
                |request| {
                    assert_eq!(request.statement, 1);
                    assert_eq!(
                        request.request,
                        GraphInsertRequest::Vertex { row: 0, vertex: 0 }
                    );
                    Ok::<_, ()>(ElementId::Vertex(VId(10)))
                },
            )
            .unwrap();
        assert_eq!(
            receipt.steps()[0].merged_edge(),
            Some(GraphEdgeMergeOutcome::NoInput)
        );
        assert_eq!(receipt.steps()[0].created_edges(), Some(&[][..]));
        assert_eq!(
            receipt.steps()[1].merged_vertex(),
            Some(GraphVertexMergeOutcome::Created(VId(10)))
        );
        assert_eq!(
            (
                receipt.stats().created_vertices,
                receipt.stats().created_edges
            ),
            (1, 0)
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(10)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relationship_allocator_failure_retains_statement_index_and_rolls_back_graph_prefix() {
    let ((), report) = run_async_under_lab(0xed9e_4004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let error = txn
            .execute_graph_write_program_governed(
                &mut db,
                &query,
                &program(),
                policy(2, 1),
                |request| match request.request {
                    GraphInsertRequest::Vertex { .. } => {
                        Ok(ElementId::Vertex(VId(10 + request.statement as u128)))
                    }
                    GraphInsertRequest::Edge { .. } => Err("allocator-down"),
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramError::EdgeMerge {
                statement: 2,
                source: GqlQueryError::Source(GraphEdgeMergeError::IdentitySource(
                    "allocator-down"
                )),
            }
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        txn.abort();
        assert!(db.vertex(VId(10)).unwrap().is_none());
        assert!(db.vertex(VId(11)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-at06q: the MERGE map filters existing edges, every matching parallel
/// edge is a match, and an undirected MERGE accepts either orientation. A miss
/// creates one edge from source to destination carrying exactly the map.
#[test]
fn merge_maps_filter_matches_keep_parallel_edges_and_undirected_accepts_either_orientation() {
    let ((), report) = run_async_under_lab(0xed9e_4020, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in [1, 2] {
            batch.create_vertex(VId(id), vec![PERSON], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, batch).await.unwrap();
        let mut existing = WriteBatch::new(S);
        for (eid, src, dst, p) in [(40, 1, 2, 7), (41, 1, 2, 8), (42, 1, 2, 7), (43, 2, 1, 9)] {
            existing.add_edge(
                EId(eid),
                VId(src),
                VId(dst),
                vec![(P, CanonicalScalar::Int(p))],
            );
        }
        db.write(&commit, existing).await.unwrap();
        let program = |text: &str| {
            let merge = PreparedGraphEdgeMergeText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            PreparedGraphWriteProgram::prepare(vec![merge.into()]).unwrap()
        };
        let mut txn = db.begin(&txcx).unwrap();
        // Both parallel p=7 edges match; the p=8 edge is filtered out.
        let parallel = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program("MATCH (a:Person)-[:R]->(b:Person) MERGE (a)-[e:S {p: 7}]->(b)"),
                policy(0, 0),
                // A matched pair never allocates; an allocation would refuse.
                |_| Err::<ElementId, ()>(()),
            )
            .unwrap();
        assert_eq!(
            parallel.steps()[0].merged_edges(),
            Some(
                &[
                    GraphEdgeMergeOutcome::Matched(EId(40)),
                    GraphEdgeMergeOutcome::Matched(EId(42)),
                ][..]
            )
        );
        // Undirected: the reverse p=9 edge matches.
        let undirected = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program("MATCH (a:Person)-[:R]->(b:Person) MERGE (a)-[:S {p: 9}]-(b)"),
                policy(0, 0),
                // An undirected match never allocates either.
                |_| Err::<ElementId, ()>(()),
            )
            .unwrap();
        assert_eq!(
            undirected.steps()[0].merged_edge(),
            Some(GraphEdgeMergeOutcome::Matched(EId(43)))
        );
        // Directed: the reverse edge does not match, so one edge is created
        // from a to b with exactly the map.
        let directed = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program("MATCH (a:Person)-[:R]->(b:Person) MERGE (a)-[:S {p: 9}]->(b)"),
                policy(0, 1),
                |_| Ok::<_, ()>(ElementId::Edge(EId(50))),
            )
            .unwrap();
        assert_eq!(
            directed.steps()[0].merged_edge(),
            Some(GraphEdgeMergeOutcome::Created(EId(50)))
        );
        txn.finish(&mut db, &commit).await.unwrap();
        let created = db.edge(EId(50)).unwrap().unwrap();
        assert_eq!(
            (created.entry.src, created.entry.dst, created.entry.relation),
            (VId(1), VId(2), S)
        );
        assert_eq!(created.props, vec![(P, CanonicalScalar::Int(9).into())]);
        for (eid, p) in [(40, 7), (41, 8), (42, 7), (43, 9)] {
            assert_eq!(
                db.edge(EId(eid)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(p).into())]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-at06q: MERGE ... RETURN through the public write dispatch yields one
/// row per selected row and resolved edge. Each parallel match is a row, an
/// undirected match reports the edge's own orientation, and a created edge's
/// properties read back from the staged graph. Autocommit rows follow commit.
#[test]
fn merge_return_rows_cover_every_resolved_edge_through_query_write() {
    type Allocate<'a> = &'a mut dyn FnMut(GraphWriteIdentityRequest) -> Result<ElementId, ()>;
    let ((), report) = run_async_under_lab(0xed9e_4030, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in [1, 2] {
            batch.create_vertex(VId(id), vec![PERSON], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, batch).await.unwrap();
        let mut existing = WriteBatch::new(S);
        for (eid, src, dst, p) in [(40, 1, 2, 7), (41, 1, 2, 8), (42, 1, 2, 7), (43, 2, 1, 9)] {
            existing.add_edge(
                EId(eid),
                VId(src),
                VId(dst),
                vec![(P, CanonicalScalar::Int(p))],
            );
        }
        // fgdb-kr8j1: a stored List field is a RETURN cell like a scalar.
        let tags = CanonicalList::try_new(vec![
            CanonicalScalar::Int(1).into(),
            CanonicalScalar::ucs_basic_text("x").unwrap().into(),
        ])
        .unwrap();
        existing.add_edge_values(
            EId(44),
            VId(1),
            VId(2),
            vec![
                (P, CanonicalScalar::Int(6).into()),
                (TAGS, CanonicalPropertyValue::List(tags)),
            ],
        );
        db.write(&commit, existing).await.unwrap();
        // RETURN rows as graph values; None if the statement was dispatched
        // to the no-result program engine or a cell left the value domain.
        let rows = |result: QueryResult, names: &[&str]| -> Option<Vec<Vec<GraphValue>>> {
            let QueryResult::Rows { columns, rows } = result else {
                return None;
            };
            assert_eq!(columns, names);
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| match cell {
                            GraphAggregateValue::Value(value) => Some(value),
                            _ => None,
                        })
                        .collect()
                })
                .collect()
        };
        let edge = |id| GraphValue::Edge(EId(id));
        let vertex = |id| GraphValue::Vertex(VId(id));
        let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        // A matched pair never allocates; an allocation would refuse.
        let mut none = |_: GraphWriteIdentityRequest| Err::<ElementId, ()>(());
        let arguments = GqlParameters::new();
        let selected = "MATCH (a:Person)-[:R]->(b:Person) MERGE ";

        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = |text: &str, edges: u64, allocate: Allocate<'_>| {
            txn.query_write(
                &mut db,
                &query,
                &format!("{selected}{text}"),
                &arguments,
                symbols,
                R,
                policy(0, edges),
                allocate,
            )
            .unwrap()
        };
        // Both parallel p=7 edges are rows, in identity order.
        assert_eq!(
            rows(
                staged(
                    "(a)-[e:S {p: 7}]->(b) RETURN e, e.p AS p, startNode(e) AS s, endNode(e) AS t",
                    0,
                    &mut none,
                ),
                &["e", "p", "s", "t"],
            ),
            Some(vec![
                vec![edge(40), int(7), vertex(1), vertex(2)],
                vec![edge(42), int(7), vertex(1), vertex(2)],
            ])
        );
        // The reverse p=9 edge matches undirected; its own orientation shows.
        assert_eq!(
            rows(
                staged(
                    "(a)-[e:S {p: 9}]-(b) RETURN startNode(e) AS s, endNode(e) AS t, b",
                    0,
                    &mut none,
                ),
                &["s", "t", "b"],
            ),
            Some(vec![vec![vertex(2), vertex(1), vertex(2)]])
        );
        assert_eq!(
            rows(
                staged("(a)-[e:S {p: 7}]->(b) RETURN count(e) AS n", 0, &mut none),
                &["n"],
            ),
            Some(vec![vec![int(2)]])
        );
        let text = |value| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        assert_eq!(
            rows(
                staged(
                    "(a)-[e:S {p: 6}]->(b) RETURN e.tags AS tags, e.p AS p",
                    0,
                    &mut none,
                ),
                &["tags", "p"],
            ),
            Some(vec![vec![
                GraphValue::List(vec![int(1), text("x")].into_boxed_slice()),
                int(6),
            ]])
        );
        // A miss creates one edge; RETURN reads it from the staged graph.
        let created = staged(
            "(a)-[e:S {p: 5}]->(b) RETURN e.p AS p, e",
            1,
            &mut |request| {
                assert_eq!(request.statement, 0);
                Ok(ElementId::Edge(EId(60)))
            },
        );
        assert_eq!(
            rows(created, &["p", "e"]),
            Some(vec![vec![int(5), edge(60)]])
        );
        txn.finish(&mut db, &commit).await.unwrap();

        let committed = db
            .query_write(
                &txcx,
                &query,
                &commit,
                &format!("{selected}(a)-[e:S {{p: 5}}]->(b) RETURN e"),
                &arguments,
                symbols,
                R,
                policy(0, 0),
                none,
            )
            .await
            .unwrap();
        assert_eq!(rows(committed, &["e"]), Some(vec![vec![edge(60)]]));
        let stored = db.edge(EId(60)).unwrap().unwrap();
        assert_eq!(
            (stored.entry.src, stored.entry.dst, stored.entry.relation),
            (VId(1), VId(2), S)
        );
        assert_eq!(stored.props, vec![(P, CanonicalScalar::Int(5).into())]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-cpny9 (TCK Merge5 [16], [17]): `MATCH ... WITH ... MERGE` resolves
/// every WITH row's endpoint pair, as a MATCH-selected MERGE does. The row
/// is the whole scope: its carried vertices are the endpoints and its hidden
/// reads feed RETURN. Rows that share a pair share its one edge; a second run
/// matches every pair and allocates nothing.
#[test]
fn merge_after_with_resolves_every_with_row() {
    let ((), report) = run_async_under_lab(0xed9e_4031, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, p) in [(1, 10), (2, 20)] {
            batch.create_vertex(VId(id), vec![PERSON], vec![(P, CanonicalScalar::Int(p))]);
        }
        db.write(&commit, batch).await.unwrap();
        let arguments = GqlParameters::new();
        let mut next = 100u128;
        let text = "MATCH (n:Person) MATCH (m:Person) WITH n AS a, m AS b \
                    MERGE (a)-[r:S]->(b) RETURN a.p AS a, b.p AS b";
        let returned = db
            .query_write(
                &txcx,
                &query,
                &commit,
                text,
                &arguments,
                symbols,
                R,
                policy(0, 10),
                |_| {
                    next += 1;
                    Ok::<_, ()>(ElementId::Edge(EId(next)))
                },
            )
            .await
            .unwrap();
        let QueryResult::Rows { columns, rows } = returned else {
            unreachable!("MERGE ... RETURN answers rows")
        };
        assert_eq!(columns, ["a", "b"]);
        let int =
            |value| GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        assert_eq!(rows.len(), 4);
        for (a, b) in [(10, 10), (10, 20), (20, 10), (20, 20)] {
            assert!(rows.contains(&vec![int(a), int(b)]), "{a}, {b}");
        }
        let edges = |db: &Database<MemVfs>| {
            let mut pairs = db
                .edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.relation == S)
                .map(|edge| (edge.entry.src, edge.entry.dst))
                .collect::<Vec<_>>();
            pairs.sort();
            pairs
        };
        let all = vec![
            (VId(1), VId(1)),
            (VId(1), VId(2)),
            (VId(2), VId(1)),
            (VId(2), VId(2)),
        ];
        assert_eq!(edges(&db), all);
        // Every pair now matches; no identity is requested, through the
        // RETURN-less script path as well.
        let frontier = db.frontier().unwrap();
        db.query_write(
            &txcx,
            &query,
            &commit,
            "MATCH (n:Person) MATCH (m:Person) WITH n AS a, m AS b MERGE (a)-[:S]->(b)",
            &arguments,
            symbols,
            R,
            policy(0, 0),
            |_| Err::<ElementId, ()>(()),
        )
        .await
        .unwrap();
        assert_eq!(edges(&db), all);
        assert_eq!(db.frontier().unwrap(), frontier);
        // Four rows carry two distinct pairs: one edge each.
        db.query_write(
            &txcx,
            &query,
            &commit,
            "MATCH (n:Person) MATCH (m:Person) WITH n AS a, m.p AS q MERGE (a)-[:R]->(a)",
            &arguments,
            symbols,
            R,
            policy(0, 10),
            |_| {
                next += 1;
                Ok::<_, ()>(ElementId::Edge(EId(next)))
            },
        )
        .await
        .unwrap();
        let loops = db
            .edges()
            .unwrap()
            .into_iter()
            .filter(|edge| edge.entry.relation == R)
            .count();
        assert_eq!(loops, 2);
        // An endpoint the WITH row does not carry refuses before any write.
        let frontier = db.frontier().unwrap();
        assert!(
            db.query_write(
                &txcx,
                &query,
                &commit,
                "MATCH (n:Person) WITH n.p AS a MERGE (a)-[:S]->(a)",
                &arguments,
                symbols,
                R,
                policy(0, 10),
                |_| Err::<ElementId, ()>(()),
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// TCK Merge6 [2], Merge7 [3], Merge8 [1]: `MERGE (a)-[r]->(b) ON CREATE SET
/// ... ON MATCH SET ... RETURN` runs the branch the MERGE took, then RETURN
/// reads the relationship after every clause. No selected pair is no row.
/// RETURN is over the MERGE relationship only: naming a MATCH binding there
/// refuses before any write.
#[test]
fn relationship_merge_clauses_then_return_read_the_final_relationship() {
    let ((), report) = run_async_under_lab(0xed9e_4032, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, p) in [(1, 10), (2, 20)] {
            batch.create_vertex(VId(id), vec![PERSON], vec![(P, CanonicalScalar::Int(p))]);
        }
        db.write(&commit, batch).await.unwrap();
        let budget = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
            10,
            0,
            10,
        );
        let arguments = GqlParameters::new();
        // Sync, so the lab future that holds `write` across awaits is Send.
        let allocated = std::sync::atomic::AtomicU32::new(0);
        let write = async |db: &mut Database<MemVfs>, text: &str| {
            db.query_write(
                &txcx,
                &query,
                &commit,
                text,
                &arguments,
                symbols,
                R,
                budget,
                |_| {
                    let n = allocated.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    Ok::<_, ()>(ElementId::Edge(EId(100 + u128::from(n))))
                },
            )
            .await
        };
        let pair = "MATCH (a:Person), (b:Person) WHERE a.p = 10 AND b.p = 20 \
                    MERGE (a)-[r:S]->(b)";
        let rows = |result: QueryResult| {
            let QueryResult::Rows { rows, .. } = result else {
                unreachable!("MERGE ... RETURN answers rows")
            };
            rows
        };
        let edges = |db: &Database<MemVfs>| {
            db.edges()
                .unwrap()
                .into_iter()
                .filter(|edge| edge.entry.relation == S)
                .map(|edge| (edge.entry.src, edge.entry.dst, edge.props))
                .collect::<Vec<_>>()
        };
        // The create branch runs; RETURN counts the one relationship.
        let created = write(
            &mut db,
            &format!("{pair} ON CREATE SET r.p = 1 ON MATCH SET r.p = 0 RETURN count(r) AS c"),
        )
        .await
        .unwrap();
        // A write RETURN's count is an Int value, as main's edge upsert
        // RETURN laws pin it.
        let int =
            |value| GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        assert_eq!(rows(created), [vec![int(1)]]);
        assert_eq!(
            edges(&db),
            [(
                VId(1),
                VId(2),
                vec![(P, CanonicalPropertyValue::from(CanonicalScalar::Int(1)))]
            )]
        );
        // The match branch runs, and RETURN reads the value it wrote.
        let matched = write(
            &mut db,
            &format!("{pair} ON CREATE SET r.p = 0 ON MATCH SET r.p = r.p + 1 RETURN r.p AS p"),
        )
        .await
        .unwrap();
        assert_eq!(
            rows(matched),
            [vec![GraphAggregateValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(2)
            ))]]
        );
        assert_eq!(edges(&db).len(), 1);
        assert_eq!(allocated.load(std::sync::atomic::Ordering::Relaxed), 1);
        // No selected pair: no relationship, no row.
        let none = write(
            &mut db,
            "MATCH (a:Person), (b:Person) WHERE a.p = 10 AND b.p = 99 \
             MERGE (a)-[r:S]->(b) ON CREATE SET r.p = 1 RETURN r",
        )
        .await
        .unwrap();
        assert!(rows(none).is_empty());
        assert_eq!(edges(&db).len(), 1);
        // RETURN over a MATCH binding refuses before any write.
        let frontier = db.frontier().unwrap();
        for refused in [
            format!("{pair} ON MATCH SET r.p = 5 RETURN a"),
            format!("{pair} ON MATCH SET r.p = 5 RETURN b.p AS p"),
            format!("{pair} ON MATCH SET r.p = 5 RETURN properties(r) AS m"),
        ] {
            assert!(write(&mut db, &refused).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
