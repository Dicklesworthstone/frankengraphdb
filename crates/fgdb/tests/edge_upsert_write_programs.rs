//! Conditional relationship ingestion through the canonical atomic workspace.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergeError, GraphEdgeMergeOutcome,
    GraphEdgeUpsertError, GraphMutationProgramDimension, GraphMutationProgramError,
    GraphSymbol, GraphSymbolKind, GraphWriteProgramError, GraphWriteProgramPolicy,
    PreparedGraphEdgeUpsertText, PreparedGraphInsertText, PreparedGraphVertexMergeText,
    PreparedGraphWriteProgramTemplate,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn template(repeat_edge: bool) -> PreparedGraphWriteProgramTemplate {
    let left = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$left})", R, symbols).unwrap();
    let right = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$right})", R, symbols).unwrap();
    let edge = PreparedGraphEdgeUpsertText::prepare(
        "MATCH (a:Person),(b:Person) WHERE a.p=$left AND b.p=$right MERGE (a)-[e:R]->(b) ON CREATE SET e.w=$fresh ON MATCH SET e.w=$seen",
        R, symbols,
    ).unwrap();
    let mut steps = vec![left.into(), right.into(), edge.clone().into()];
    if repeat_edge { steps.push(edge.into()); }
    PreparedGraphWriteProgramTemplate::prepare(steps).unwrap()
}
fn arguments() -> GqlParameters {
    GqlParameters::new().with_int64("left", 1).unwrap().with_int64("right", 2).unwrap()
        .with_int64("fresh", 200).unwrap().with_int64("seen", 100).unwrap()
}
fn policy(effects: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(20_000, 20_000, 5_000_000, 5_000_000), effects, 2, 1)
}

#[test]
fn one_atomic_ingestion_creates_vertices_decorates_edge_and_reads_its_own_actions() {
    let ((), report) = run_async_under_lab(0xed9e_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let program = template(true).bind_parameters(&arguments()).unwrap();
        let allocations = Cell::new(0);
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program, policy(2), |request| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(match request.statement {
                    0 => ElementId::Vertex(VId(1)),
                    1 => ElementId::Vertex(VId(2)),
                    2 => ElementId::Edge(EId(10)),
                    _ => panic!("repeated MERGE must match the staged edge"),
                })
            },
        ).unwrap();
        assert_eq!(allocations.get(), 3);
        assert_eq!(receipt.stats().completed_statements, 4);
        assert_eq!((receipt.stats().created_vertices, receipt.stats().created_edges, receipt.stats().mutation_effects), (2, 1, 2));
        assert_eq!(receipt.stats().target_vertex_visits, 0, "edge actions are not vertex updates");
        assert_eq!(receipt.steps()[2].merged_edge(), Some(GraphEdgeMergeOutcome::Created(EId(10))));
        assert_eq!(receipt.steps()[3].merged_edge(), Some(GraphEdgeMergeOutcome::Matched(EId(10))));
        assert_eq!(txn.edge(&db, EId(10)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(100))]);
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.edge(EId(10)).unwrap().is_none());
        txn.finish(&mut db, &commit).await.unwrap();

        let mut no_creations = policy(2);
        no_creations.max_created_vertices = 0;
        no_creations.max_created_edges = 0;
        let mut repeated = db.begin(&txcx).unwrap();
        let stats = repeated.execute_graph_write_program_governed(
            &mut db, &query, &program, no_creations,
            |_| -> Result<ElementId, ()> { panic!("repeated ingestion must not allocate") },
        ).unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges, stats.mutation_effects), (0, 0, 2));
        repeated.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.edges().unwrap().len(), 1);
        assert_eq!(db.edge(EId(10)).unwrap().unwrap().props, vec![(W, CanonicalScalar::Int(100))]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_action_quota_rolls_back_the_whole_program_but_preserves_outer_prefix() {
    let ((), report) = run_async_under_lab(0xed9e_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let program = template(true).bind_parameters(&arguments()).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let allocations = Cell::new(0);
        let result = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program, policy(1), |request| {
                allocations.set(allocations.get() + 1);
                Ok::<_, ()>(match request.statement {
                    0 => ElementId::Vertex(VId(1)),
                    1 => ElementId::Vertex(VId(2)),
                    2 => ElementId::Edge(EId(10)),
                    _ => panic!("late matched branch may not allocate"),
                })
            },
        );
        assert!(matches!(result, Err(GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
            statement: 3, dimension: GraphMutationProgramDimension::Effects, limit: 1, observed: 2,
        }))));
        assert_eq!(allocations.get(), 3);
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
        assert!(txn.edge(&db, EId(10)).unwrap().is_none());
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert!(db.vertex(VId(1)).unwrap().is_none());
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn missing_endpoints_skip_actions_and_do_not_stop_later_program_steps() {
    let ((), report) = run_async_under_lab(0xed9e_4003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let edge = PreparedGraphEdgeUpsertText::prepare(
            "MATCH (a:Person),(b:Person) MERGE (a)-[e:R]->(b) ON CREATE SET e.w=1 ON MATCH SET e.w=2",
            R, symbols,
        ).unwrap();
        let vertex = PreparedGraphInsertText::prepare("CREATE (n {p:7})", R, symbols).unwrap();
        let program = PreparedGraphWriteProgramTemplate::prepare(vec![edge.into(), vertex.into()]).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        let mut limits = policy(0);
        limits.max_created_edges = 0;
        limits.max_created_vertices = 1;
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program, limits, |request| {
                assert_eq!(request.statement, 1);
                Ok::<_, ()>(ElementId::Vertex(VId(7)))
            },
        ).unwrap();
        assert_eq!(receipt.steps()[0].merged_edge(), Some(GraphEdgeMergeOutcome::NoInput));
        assert_eq!(receipt.stats().mutation_effects, 0);
        assert_eq!(receipt.stats().created_edges, 0);
        assert_eq!(receipt.stats().created_vertices, 1);
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(7)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_relationship_allocator_failure_returns_no_prefix_receipt_or_staged_vertices() {
    let ((), report) = run_async_under_lab(0xed9e_4004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let program = template(false).bind_parameters(&arguments()).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let result = txn.execute_graph_write_program_returning_governed(
            &mut db, &query, &program, policy(1), |request| match request.statement {
                0 => Ok(ElementId::Vertex(VId(1))),
                1 => Ok(ElementId::Vertex(VId(2))),
                _ => Err("identity service refused"),
            },
        );
        assert!(matches!(result, Err(GraphWriteProgramError::EdgeUpsert {
            statement: 2,
            source: GqlQueryError::Source(GraphEdgeUpsertError::Merge(
                GraphEdgeMergeError::IdentitySource("identity service refused"))),
        })));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
        assert!(txn.vertex(&db, VId(2)).unwrap().is_none());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
