//! fgdb-cpny9: a relationship MERGE's native WITH tail is inside the same
//! durable statement and rollback boundary, never a second MATCH or commit.

#![recursion_limit = "256"]

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergePolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMergeQueryText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

#[test]
fn merge_tail_filtering_preserves_effects_and_duplicate_match_occurrences() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let txcx = contexts.txn();
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=3 {
            batch.create_vertex(VId(id as u128), vec![], vec![(P, CanonicalScalar::Int(id))]);
        }
        db.write(&commit, batch).await.unwrap();
        let filtered = PreparedGraphEdgeMergeQueryText::prepare(
            "MATCH (a {p:1}),(b {p:2}) MERGE (a)-[r:R {p:7}]->(b) \
             WITH r AS edge WHERE edge.p = 0 RETURN edge",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let before = db.frontier().unwrap();
        let (_, result, _) = db
            .execute_graph_edge_merge_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &filtered,
                GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100, 0, 100_000, 100_000)),
                |_| Ok::<_, ()>(ElementId::Edge(EId((1_u128 << 100) + 1))),
            )
            .await
            .unwrap();
        assert!(result.value.is_empty());
        assert_ne!(
            db.frontier().unwrap(),
            before,
            "filtering is not write suppression"
        );
        let QueryResult::Rows { rows, .. } = db
            .query(
                &cx,
                "MATCH (a)-[r:R]->(b) RETURN r.p",
                &GqlParameters::new(),
                symbols,
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
            )
            .unwrap()
        else {
            panic!("row query");
        };
        assert_eq!(rows, vec![vec![QueryValue::Value(int(7))]]);

        // Three occurrences of one endpoint pair resolve to the same edge.
        // The WITH must not collapse their bag multiplicity or use the final
        // one-row allowance to refuse these three private head rows.
        let count = PreparedGraphEdgeMergeQueryText::prepare(
            "MATCH (a {p:1}),(b {p:2}),(duplicate) MERGE (a)-[r:R]->(b) \
             WITH r RETURN count(r) AS occurrences",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let (_, result, _) = db
            .execute_graph_edge_merge_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &count,
                GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100, 1, 100_000, 100_000)),
                |_| -> Result<ElementId, ()> { panic!("existing edge must not allocate") },
            )
            .await
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].values(), &[int(3)]);
    });
}

#[test]
fn a_late_tail_failure_rolls_back_creation_even_with_limit_zero() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let txcx = contexts.txn();
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=2 {
            batch.create_vertex(VId(id as u128), vec![], vec![(P, CanonicalScalar::Int(id))]);
        }
        db.write(&commit, batch).await.unwrap();
        let query = PreparedGraphEdgeMergeQueryText::prepare(
            "MATCH (a {p:1}),(b {p:2}) MERGE (a)-[r:R {p:2}]->(b) \
             WITH r.p AS p RETURN 10 / (p - 2) AS x LIMIT 0",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let before = db.frontier().unwrap();
        let mut allocated = 0;
        let result = db
            .execute_graph_edge_merge_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &query,
                GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100, 0, 100_000, 100_000)),
                |_| {
                    allocated += 1;
                    Ok::<_, ()>(ElementId::Edge(EId(99)))
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(
            allocated, 1,
            "the error is after creation, not a parser refusal"
        );
        assert_eq!(db.frontier().unwrap(), before);
        let QueryResult::Rows { rows, .. } = db
            .query(
                &cx,
                "MATCH (a)-[r:R]->(b) RETURN r",
                &GqlParameters::new(),
                symbols,
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
            )
            .unwrap()
        else {
            panic!("row query");
        };
        assert!(
            rows.is_empty(),
            "failed statement leaked a created relationship"
        );
    });
}

#[test]
fn computed_relationship_merge_heads_and_tails_preserve_occurrences() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let txcx = contexts.txn();
    runtime.block_on(async {
        for (tail, expected) in [
            (
                "RETURN [e IN collect(r) | e.p] AS values",
                vec![GraphValue::List(vec![int(7); 3].into_boxed_slice())],
            ),
            (
                "WITH collect(r) AS edges RETURN [e IN edges | e.p] AS values",
                vec![GraphValue::List(vec![int(7); 3].into_boxed_slice())],
            ),
            (
                "WITH [r][0] AS edge RETURN sum(edge.p) AS total",
                vec![int(21)],
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            for id in 1..=3 {
                batch.create_vertex(VId(id as u128), vec![], vec![(P, CanonicalScalar::Int(id))]);
            }
            db.write(&commit, batch).await.unwrap();
            let query = PreparedGraphEdgeMergeQueryText::prepare(
                &format!(
                    "MATCH (a {{p:1}}),(b {{p:2}}),(duplicate) \
                     MERGE (a)-[r:R {{p:7}}]->(b) {tail}"
                ),
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let mut allocated = 0;
            let (_, output, _) = db
                .execute_graph_edge_merge_query_autocommit_governed(
                    &txcx,
                    &cx,
                    &commit,
                    &query,
                    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(100, 1, 100_000, 100_000)),
                    |_| {
                        allocated += 1;
                        Ok::<_, ()>(ElementId::Edge(EId((1_u128 << 100) + 1)))
                    },
                )
                .await
                .unwrap();
            assert_eq!(allocated, 1);
            assert_eq!(output.value.len(), 1);
            assert_eq!(output.value[0].values(), expected.as_slice(), "{tail}");
        }
    });
}
