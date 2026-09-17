//! Element names through the native text facade, including historical rows.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, CanonicalText, EId, VId};

const RELATION: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const AGENT: LabelId = LabelId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(RELATION)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Agent") => Some(GraphSymbol::Label(AGENT)),
        _ => None,
    }
}

fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Text(
        CanonicalText::new_ucs_basic(value).expect("small catalog name"),
    ))
}

#[test]
fn element_names_include_empty_labels_and_preserve_historical_rows() {
    for seed in [0xE101_u64, 0xE102, 0xE103] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let directory = std::env::temp_dir()
                .join(format!("fgdb-element-names-{seed}-{}", std::process::id()));
            let mut database = Database::create(
                &commit,
                &directory,
                DatabaseKeys::new(
                    [0x6c; 32],
                    DatabaseSecurityNamespaceId([0x79; 32]),
                    [0x3e; 32],
                ),
            )
            .await
            .expect("create element-name database");
            let mut batch = WriteBatch::new(RELATION);
            batch.create_vertex(VId(1), vec![PERSON, AGENT], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(1), VId(1), VId(2), vec![]);
            let historical = database.write(&commit, batch).await.expect("initial graph");
            let mut later = WriteBatch::new(RELATION);
            later.create_vertex(VId(3), vec![AGENT], vec![]);
            database.write(&commit, later).await.expect("later graph");

            let vertices = database.vertices_at(historical).expect("historical scan");
            let expected_labels: Vec<_> = vertices
                .iter()
                .map(|vertex| {
                    let names = vertex
                        .labels
                        .iter()
                        .map(|id| match *id {
                            PERSON => text("Person"),
                            AGENT => text("Agent"),
                            other => panic!("unexpected fixture label {other:?}"),
                        })
                        .collect::<Vec<_>>();
                    vec![
                        GraphValue::Vertex(vertex.vid),
                        GraphValue::List(names.into_boxed_slice()),
                    ]
                })
                .collect();
            assert_eq!(expected_labels[1][1], GraphValue::List(Box::new([])));
            assert_eq!(
                expected_labels[0][1],
                GraphValue::List(vec![text("Person"), text("Agent")].into_boxed_slice())
            );
            let expected_types: Vec<_> = database
                .edges_at(historical)
                .expect("historical edges")
                .iter()
                .map(|edge| {
                    assert_eq!(edge.entry.relation, RELATION);
                    vec![GraphValue::Vertex(edge.entry.src), text("KNOWS")]
                })
                .collect();
            for (query, expected) in [
                (
                    format!(
                        "MATCH (p) FOR SYSTEM_TIME AS OF SEQ {} RETURN p, labels(p) AS names ORDER BY p",
                        historical.0
                    ),
                    expected_labels,
                ),
                (
                    format!(
                        "MATCH (p)-[r:KNOWS]->(q) FOR SYSTEM_TIME AS OF SEQ {} RETURN p, type(r) AS name ORDER BY p",
                        historical.0
                    ),
                    expected_types,
                ),
            ] {
                let result = database
                    .query(
                        &cx,
                        &query,
                        &GqlParameters::new(),
                        symbols,
                        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
                    )
                    .unwrap_or_else(|error| panic!("seed={seed} query={query}: {error:?}"));
                let QueryResult::Rows { rows, .. } = result else {
                    panic!("expected element-name rows");
                };
                let actual: Vec<Vec<_>> = rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| match cell {
                                GraphAggregateValue::Value(value) => value,
                                other => panic!("unexpected element-name cell {other:?}"),
                            })
                            .collect()
                    })
                    .collect();
                assert_eq!(actual, expected, "seed={seed} query={query}");
            }
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}
