//! Compound write summaries accept the same stored-property domain as a
//! standalone summary. The native insertion and aggregation owners execute
//! these tests; no graph source or alternate evaluator supplies the answer.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphInsertQueryBatch,
    GraphInsertQueryError, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeMergeQueryText,
    PreparedGraphEdgeUpsertQueryText, PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
    PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{CanonicalF64, CanonicalScalar, CanonicalText, EId, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
type ResultBatch = Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<(), ()>, ()>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn boolean(value: bool) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Bool(value))
}

fn float(value: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
}

fn prepare(text: &str) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn execute(query: &PreparedGraphInsertQuery, rows: u64) -> ResultBatch {
    query.execute_governed(
        GraphInsertPolicy::new(GqlQueryPolicy::new(0, rows, 2_000_000, 1_000_000), 100, 100),
        |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
            panic!("source-free CREATE must not scan the graph")
        },
        |request| {
            Ok(match request {
                GraphInsertRequest::Vertex { row, vertex } => {
                    ElementId::Vertex(VId(100 + row as u128 * 16 + vertex as u128))
                }
                GraphInsertRequest::Edge { row, edge } => {
                    ElementId::Edge(EId(1_000 + row as u128 * 16 + edge as u128))
                }
            })
        },
        || Ok(()),
    )
}

fn rows(batch: &GraphInsertQueryBatch) -> Vec<Vec<GraphValue>> {
    batch
        .returning()
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn stored_numeric_compound_summaries_preserve_occurrences_distinct_and_nulls() {
    let query = prepare(
        "UNWIND [1,3,3,NULL] AS x CREATE (n {p:x}) \
         RETURN sum(n.p)+count(*) AS total, \
         sum(DISTINCT n.p)+count(n.p) AS unique_total",
    );
    let batch = execute(&query, 1).unwrap();
    assert_eq!(rows(&batch), vec![vec![int(11), int(7)]]);
    assert_eq!(batch.insertion().stats().created_vertices, 4);
    assert_eq!(batch.returning().rows.result_rows, 1);
    assert_eq!(batch.returning().rows.snapshot_records, 0);
    // Binary64 means keep the occurrence, DISTINCT and NULL laws: 7/3 > 2,
    // while the distinct mean is exactly 2.
    let floating = prepare(
        "UNWIND [1.0,3.0,3.0,NULL] AS x CREATE (n {p:x}) \
         RETURN avg(n.p)>2 AS above, avg(DISTINCT n.p)=2 AS exact",
    );
    assert_eq!(
        rows(&execute(&floating, 1).unwrap()),
        vec![vec![boolean(true), boolean(true)]]
    );
    // An AVG of integers is an exact rational, which has no ordinary row
    // value: the write group's row carries the binary64 nearest it, 7/3 and
    // 2 here, before the compound expressions compare it (fgdb-y255t).
    let exact = prepare(
        "UNWIND [1,3,3,NULL] AS x CREATE (n {p:x}) \
         RETURN sum(n.p)+count(*) AS total, \
         sum(DISTINCT n.p)+count(n.p) AS unique_total, \
         avg(n.p)>2 AS above, avg(DISTINCT n.p)=2 AS exact, \
         avg(n.p)*3 AS tripled",
    );
    assert_eq!(
        rows(&execute(&exact, 1).unwrap()),
        vec![vec![
            int(11),
            int(7),
            boolean(true),
            boolean(true),
            float(7.0 / 3.0 * 3.0)
        ]]
    );
}

#[test]
fn compound_stored_float_summaries_keep_exact_native_accumulation_and_aliases() {
    let query = prepare(
        "UNWIND [1.0,9007199254740992.0,-9007199254740992.0] AS x CREATE (n {p:x}) \
         RETURN sum(n.p)+1.0 AS total, avg(n.p)>0 AS positive",
    );
    assert_eq!(
        rows(&execute(&query, 1).unwrap()),
        vec![vec![float(2.0), boolean(true)]]
    );
    let aliases = prepare(
        "UNWIND [1.0,2.0,NULL] AS x CREATE (n {p:x}) \
         RETURN SUM_INT(n.p)+1 AS total, AVG_INT(n.p)+1.0 AS mean",
    );
    assert_eq!(
        rows(&execute(&aliases, 1).unwrap()),
        vec![vec![float(4.0), float(2.5)]]
    );
}

#[test]
fn compound_stored_write_with_feeds_native_tail_without_limiting_creations() {
    // Binary64 inputs, then integer ones: an integer AVG reaches the tail
    // as its nearest binary64, as above.
    let query = prepare(
        "UNWIND [1.0,3.0,3.0,NULL] AS x CREATE (n {p:x}) \
         WITH sum(n.p)+count(*) AS total, avg(n.p)>2 AS high \
         WHERE total>0 RETURN total*2 AS doubled, high",
    );
    let batch = execute(&query, 1).unwrap();
    assert_eq!(rows(&batch), vec![vec![float(22.0), boolean(true)]]);
    assert_eq!(batch.insertion().stats().created_vertices, 4);
    let query = prepare(
        "UNWIND [1,3,3,NULL] AS x CREATE (n {p:x}) \
         WITH sum(n.p)+count(*) AS total, avg(n.p) AS mean \
         WHERE total>0 RETURN total*2 AS doubled, mean>2 AS high, mean",
    );
    let batch = execute(&query, 1).unwrap();
    assert_eq!(
        rows(&batch),
        vec![vec![int(22), boolean(true), float(7.0 / 3.0)]]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 4);
    let filtered = prepare(
        "UNWIND [1,3,3,NULL] AS x CREATE (n {p:x}) \
         WITH sum(n.p)+count(*) AS total WHERE total<0 RETURN total",
    );
    let batch = execute(&filtered, 0).unwrap();
    assert!(batch.returning().value.is_empty());
    assert_eq!(batch.insertion().stats().created_vertices, 4);
}

#[test]
fn every_write_family_admits_stored_numeric_arguments_in_compound_results() {
    macro_rules! admit {
        ($kind:ty, $prefix:literal, $function:expr) => {{
            let text = format!("{} RETURN {}(DISTINCT n.p)+1 AS value", $prefix, $function);
            let _query = <$kind>::prepare(&text, R, symbols)
                .expect(&text)
                .bind_parameters(&GqlParameters::new())
                .expect(&text);
        }};
    }
    for function in ["sum", "sum_int", "avg", "avg_int"] {
        admit!(PreparedGraphInsertQueryText, "CREATE (n {p:1})", function);
        admit!(
            PreparedGraphMutationQueryText,
            "MATCH (n) SET n.p=n.p",
            function
        );
        admit!(
            PreparedGraphVertexUpsertQueryText,
            "MERGE (n {p:1}) ON MATCH SET n.p=n.p",
            function
        );
        admit!(
            PreparedGraphEdgeMergeQueryText,
            "MATCH (a),(b) MERGE (a)-[n:R]->(b)",
            function
        );
        admit!(
            PreparedGraphEdgeUpsertQueryText,
            "MATCH (a),(b) MERGE (a)-[n:R]->(b) ON MATCH SET n.p=n.p",
            function
        );
    }
}

#[test]
fn static_nonnumeric_aggregate_domains_still_refuse_before_catalog_access() {
    for argument in ["n", "[1,2]"] {
        for function in ["sum", "avg"] {
            let text = format!("CREATE (n {{p:1}}) RETURN {function}({argument})+1 AS value");
            let mut calls = 0;
            let result = PreparedGraphInsertQueryText::prepare(&text, R, |kind, name: &str| {
                calls += 1;
                symbols(kind, name)
            });
            assert!(result.is_err(), "{text}");
            assert_eq!(calls, 0, "{text}");
        }
    }
    let mut calls = 0;
    let admitted = PreparedGraphInsertQueryText::prepare(
        "CREATE (n {p:1}) RETURN sum(n.p)+1 AS value",
        R,
        |kind, name: &str| {
            calls += 1;
            symbols(kind, name)
        },
    );
    assert!(admitted.is_ok());
    assert!(
        calls > 0,
        "the positive control must actually resolve its property"
    );
}

#[test]
fn stored_nonnumeric_values_fail_at_execution_even_beneath_limit_zero() {
    // The control must succeed on the same template, so each refusal below is
    // caused by its late value. An integer mean is no refusal: it converts
    // to its nearest binary64 (fgdb-y255t), under LIMIT 0 too.
    for (function, numeric) in [("sum", int(3)), ("avg", float(3.0))] {
        let text = format!(
            "UNWIND $items AS x CREATE (n {{p:x}}) RETURN {function}(n.p)+1 AS value LIMIT 0"
        );
        let template = PreparedGraphInsertQueryText::prepare(&text, R, symbols).unwrap();
        let bind = |last| {
            let arguments = GqlParameters::new()
                .with_list("items", vec![int(1), last])
                .unwrap();
            template.bind_parameters(&arguments).unwrap()
        };
        for bad in [
            GraphValue::Scalar(CanonicalScalar::Text(
                CanonicalText::new_ucs_basic("not numeric").unwrap(),
            )),
            boolean(true),
            GraphValue::List(vec![int(2)].into_boxed_slice()),
        ] {
            assert!(matches!(
                execute(&bind(bad), 0),
                Err(GqlQueryError::Source(GraphInsertQueryError::Returning(_)))
            ));
        }
        let batch = execute(&bind(numeric), 0).unwrap();
        assert!(batch.returning().value.is_empty());
        assert_eq!(batch.insertion().stats().created_vertices, 2);
        if function == "avg" {
            let batch = execute(&bind(int(3)), 0).unwrap();
            assert!(batch.returning().value.is_empty());
            assert_eq!(batch.insertion().stats().created_vertices, 2);
        }
    }
}

#[test]
fn empty_and_all_null_stored_groups_keep_native_global_aggregate_behavior() {
    for (input, count) in [("[]", 0), ("[NULL,NULL]", 2)] {
        let query = prepare(&format!(
            "UNWIND {input} AS x CREATE (n {{p:x}}) \
             RETURN coalesce(sum(n.p),0)+count(*) AS total, \
             coalesce(avg(n.p),0)+count(*) AS mean"
        ));
        let batch = execute(&query, 1).unwrap();
        assert_eq!(rows(&batch), vec![vec![int(count), int(count)]]);
        assert_eq!(batch.insertion().stats().created_vertices, count as u64);
    }
}
