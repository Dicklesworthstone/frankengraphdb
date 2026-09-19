//! Native vertex MERGE frontend through the real unique-match/create and commit paths.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    PreparedGraphVertexMergeText,
};
use fgdb_types::{
    CanonicalScalar, CanonicalScalarKind, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        _ => None,
    }
}
fn policy() -> GraphVertexMergePolicy {
    GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000))
}
fn template() -> PreparedGraphVertexMergeText {
    let text_kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("x").unwrap());
    PreparedGraphVertexMergeText::prepare_with_parameter_types(
        "MERGE (n:Person {p:$p,name:$name})",
        R,
        &[("name", GqlParameterType::Scalar(text_kind))],
        symbols,
    )
    .unwrap()
}
fn args(p: i64, name: &str) -> GqlParameters {
    GqlParameters::new()
        .with_int64("p", p)
        .unwrap()
        .with_text("name", name)
        .unwrap()
}

#[test]
fn native_merge_matches_then_creates_then_matches_created_vertex() {
    let ((), report) = run_async_under_lab(0x6e29_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![PERSON],
            vec![
                (P, CanonicalScalar::Int(7)),
                (NAME, CanonicalScalar::ucs_basic_text("Ada").unwrap()),
            ],
        );
        db.write(&commit, seed).await.unwrap();
        let template = template();

        let existing = template.bind_parameters(&args(7, "Ada")).unwrap();
        let before = db.frontier().unwrap();
        let (_, outcome, completion) = db
            .execute_graph_vertex_merge_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &existing,
                policy(),
                |_| -> Result<ElementId, ()> { panic!("matching native MERGE must not allocate") },
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);

        let missing = template.bind_parameters(&args(9, "Grace")).unwrap();
        let (_, outcome, completion) = db
            .execute_graph_vertex_merge_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &missing,
                policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(9))),
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(9)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let created = db.vertex(VId(9)).unwrap().unwrap();
        assert_eq!(created.labels, vec![PERSON]);
        assert_eq!(
            created.props,
            vec![
                (P, CanonicalScalar::Int(9)),
                (NAME, CanonicalScalar::ucs_basic_text("Grace").unwrap()),
            ]
        );

        let rebound = template.bind_parameters(&args(9, "Grace")).unwrap();
        let (_, outcome, completion) = db
            .execute_graph_vertex_merge_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &rebound,
                policy(),
                |_| -> Result<ElementId, ()> { panic!("created vertex must now match") },
            )
            .await
            .unwrap();
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(9)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_merge_ambiguity_aborts_without_allocating() {
    let ((), report) = run_async_under_lab(0x6e29_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let value = CanonicalScalar::ucs_basic_text("Ada").unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [1_u128, 2] {
            seed.create_vertex(
                VId(id),
                vec![PERSON],
                vec![(P, CanonicalScalar::Int(7)), (NAME, value.clone())],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let merge = template().bind_parameters(&args(7, "Ada")).unwrap();
        let result = db
            .execute_graph_vertex_merge_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &merge,
                policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(99))),
            )
            .await;
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(
                GraphVertexMergeError::AmbiguousMatches { observed: 2 }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
