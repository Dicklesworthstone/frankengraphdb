//! One integer data-exception profile for every native read family
//! (fgdb-div-zero-families-iq02p). The same expression must produce the same
//! value, or the same `GraphIntegerErrorKind`, whether a graph-pattern WHERE,
//! a row WHERE after WITH, a computed RETURN, a set operand or an aggregate
//! argument evaluates it. GQL makes division by zero and overflow data
//! exceptions (class 22); none may read as NULL and silently drop a row.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    Database, DatabaseKeys, MemVfs, NativeReadClass, PreparedNativeRead, QueryError, QueryResult,
    WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphIntegerErrorKind,
    GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

/// The five statement shapes, each with the expression over `n.p` (graph
/// WHERE) or over the carried row column `p`, and the class that owns it.
fn shapes(expression: &str) -> [(NativeReadClass, String); 5] {
    let row = expression.replace("n.p", "p");
    [
        (
            NativeReadClass::Pattern,
            format!("MATCH (n:Person) WHERE {expression} = 1 RETURN n.p AS p"),
        ),
        (
            NativeReadClass::Set,
            format!("MATCH (n:Person) WITH n.p AS p WHERE {row} = 1 RETURN p"),
        ),
        (
            NativeReadClass::Set,
            format!("MATCH (n:Person) WITH n.p AS p RETURN {row} AS v"),
        ),
        (
            NativeReadClass::Set,
            format!(
                "MATCH (n:Person) RETURN n.p AS v UNION ALL \
                 MATCH (n:Person) WITH n.p AS p RETURN {row} AS v"
            ),
        ),
        (
            NativeReadClass::PipelineAggregate,
            format!("MATCH (n:Person) WITH n.p AS p RETURN SUM({row}) AS s"),
        ),
    ]
}

/// The data-exception kind a failed read carries, in whichever family
/// shape raised it. Any other failure is `None`, which no law expects.
fn data_kind(error: &QueryError) -> Option<GraphIntegerErrorKind> {
    match error {
        QueryError::Pattern(GqlQueryError::Data(error)) => Some(error.kind),
        QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Projection {
            error,
            ..
        }))
        | QueryError::Aggregate(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Projection { error, .. },
        ))) => Some(error.kind),
        _ => None,
    }
}

#[test]
fn every_family_raises_the_same_data_exception_and_agrees_on_valid_values() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(async {
        let keys = DatabaseKeys::new(
            [0x4a; 32],
            DatabaseSecurityNamespaceId([0x4b; 32]),
            [0x4c; 32],
        );
        let mut db = Database::<MemVfs>::open_memory(&commit, keys)
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(5))]);
        db.write(&commit, batch).await.unwrap();
        let policy = GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000);
        let params = GqlParameters::new();
        let run = |text: &str| db.query(&cx, text, &params, symbols, policy);

        // p = 5 throughout.
        for (expression, kind) in [
            ("n.p / (n.p - n.p)", GraphIntegerErrorKind::DivisionByZero),
            ("n.p * 9223372036854775807", GraphIntegerErrorKind::Overflow),
            ("9223372036854775807 + n.p", GraphIntegerErrorKind::Overflow),
            (
                "(0 - 9223372036854775807) - n.p - n.p",
                GraphIntegerErrorKind::Overflow,
            ),
        ] {
            for (class, text) in shapes(expression) {
                assert_eq!(
                    PreparedNativeRead::prepare(&text, &params, symbols)
                        .unwrap()
                        .facade_class(),
                    class,
                    "{text}"
                );
                let result = run(&text);
                assert_eq!(
                    result.as_ref().err().and_then(data_kind),
                    Some(kind),
                    "{text} => {result:?}"
                );
            }
        }

        // Control: a valid expression succeeds in every family, and the two
        // WHERE families select the same row by the same value.
        let [pattern, set, computed, union, aggregate] = shapes("n.p / 5");
        let rows = |result: Result<QueryResult, QueryError>| match result.unwrap() {
            QueryResult::Rows { rows, .. } => rows,
            _ => Vec::new(),
        };
        let selected = rows(run(&pattern.1));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected, rows(run(&set.1)));
        assert_eq!(rows(run(&computed.1)).len(), 1);
        assert_eq!(rows(run(&union.1)).len(), 2);
        assert_eq!(rows(run(&aggregate.1)).len(), 1);
    });
}
