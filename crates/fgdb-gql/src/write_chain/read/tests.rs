use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, GraphValue, PreparedGraphPattern};
use crate::row_join::{RowJoinKind, RowJoinSpec};
use crate::{GqlParameters, PreparedGraphWriteChainText};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

const HIGH: u64 = 1_000_000;
const WIDE: VId = VId((1_u128 << 96) | 1);

fn chain() -> PreparedGraphWriteChain {
    PreparedGraphWriteChainText::prepare(
        "UNWIND [1,1,2] AS k MERGE (n) RETURN n",
        RelationId(1),
        |_, _: &str| None,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn vertex(value: VId) -> GraphValue {
    GraphValue::Vertex(value)
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn head() -> GqlQueryExecution<GraphValueRow> {
    GqlQueryExecution {
        value: [VId(1), VId(1), VId(2), WIDE]
            .into_iter()
            .map(|id| GraphValueRow::from_owned_values(vec![vertex(id)]))
            .collect(),
        rows: GqlExecutionStats {
            snapshot_records: 5,
            result_rows: 4,
        },
        evaluator: GlaExecutionStats {
            work_units: 11,
            scratch_entries: 7,
        },
    }
}

fn tail(chain: &PreparedGraphWriteChain) -> PreparedGraphSet {
    let mut graph = GraphPatternBuilder::new();
    graph.vertex("r").unwrap();
    let right: PreparedGraphSet = graph
        .prepare_values(
            &[
                GraphColumn::vertex("r", "r"),
                GraphColumn::property("p", "r", PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into();
    let left = chain.read_input().unwrap();
    let join = RowJoinSpec::new(left.column_types(), right.column_types(), &[(0, 0)])
        .unwrap()
        .with_kind(RowJoinKind::Left);
    left.join(right, join).unwrap()
}

fn source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    Ok(GqlQueryExecution {
        value: vec![
            GraphValueRow::from_owned_values(vec![vertex(VId(1)), int(7)]),
            GraphValueRow::from_owned_values(vec![vertex(WIDE), int(9)]),
        ],
        rows: GqlExecutionStats {
            snapshot_records: 2,
            result_rows: 2,
        },
        evaluator: GlaExecutionStats {
            work_units: 3,
            scratch_entries: 2,
        },
    })
}

#[test]
fn optional_continuation_preserves_occurrences_and_reads_each_graph_operand_once() {
    let chain = chain();
    let tail = tail(&chain);
    let calls = Cell::new(0);
    let result = chain
        .finish_read_continuation::<_, (), _>(
            &tail,
            head(),
            GqlQueryPolicy::new(7, 4, HIGH, HIGH),
            |pattern: &PreparedGraphPattern<GraphValueRow>, budget: GqlQueryPolicy| {
                calls.set(calls.get() + 1);
                assert_eq!(budget.rows.max_snapshot_records(), Some(2));
                assert!(budget.evaluator.max_work_units < HIGH - 11);
                source(pattern, budget)
            },
            || Ok::<(), usize>(()),
        )
        .unwrap();
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    assert_eq!(
        result
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        vec![
            vec![vertex(VId(1)), vertex(VId(1)), int(7)],
            vec![vertex(VId(1)), vertex(VId(1)), int(7)],
            vec![vertex(VId(2)), null.clone(), null],
            vec![vertex(WIDE), vertex(WIDE), int(9)],
        ]
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(
        result.rows,
        GqlExecutionStats {
            snapshot_records: 7,
            result_rows: 4,
        }
    );
    assert!(result.evaluator.work_units > 14);
    assert!(result.evaluator.scratch_entries > 9);
}

fn text_symbols(kind: crate::GraphSymbolKind, name: &str) -> Option<crate::GraphSymbol> {
    match (kind, name) {
        (crate::GraphSymbolKind::Property, "p") => {
            Some(crate::GraphSymbol::Property(PropertyKeyId(1)))
        }
        (crate::GraphSymbolKind::Relation, "R") => {
            Some(crate::GraphSymbol::Relation(RelationId(1)))
        }
        _ => None,
    }
}

#[test]
fn terminal_match_text_binds_one_private_input_and_the_final_schema() {
    for text in [
        "CREATE (n {p:1}) WITH n MATCH (n) RETURN n.p AS value",
        "CREATE (n {p:1}) MATCH (n) RETURN n.p AS value",
        "MERGE (n {p:1}) ON CREATE SET n.p=2 WITH n MATCH (n) RETURN n.p AS value",
        "UNWIND [1,1,2] AS k MERGE (n {p:k}) OPTIONAL MATCH (n)-[:R]->(m) RETURN count(m) AS value",
        "CREATE (a),(b) MERGE (a)-[r:R]->(b) MATCH (a)-[r:R]->(b) RETURN count(*) AS value",
        "MATCH (n) SET n.p=2 WITH n MATCH (n) RETURN n.p AS value",
    ] {
        assert!(
            PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
        let template =
            PreparedGraphWriteChainText::prepare(text, RelationId(1), text_symbols).expect(text);
        assert!(template.returns_rows(), "{text}");
        let chain = template.bind_parameters(&GqlParameters::new()).expect(text);
        let read = chain.read_continuation().expect("terminal read retained");
        assert!(chain.accepts_read_continuation(read), "{text}");
        assert_eq!(chain.columns(), &["value"], "{text}");
        assert_ne!(
            chain.read_input().unwrap().columns(),
            chain.columns(),
            "{text}"
        );
    }
}

#[test]
fn terminal_read_parameters_rebind_without_catalog_reentry_or_template_mutation() {
    use crate::GqlParameterType;
    let text = "CREATE (n {p:$created}) WITH n \
        OPTIONAL MATCH (m {p:$lookup}) RETURN n.p AS created,m.p AS found";
    let calls = Cell::new(0);
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        text,
        RelationId(1),
        &[
            ("created", GqlParameterType::Int64),
            ("lookup", GqlParameterType::Int64),
        ],
        |kind, name: &str| {
            calls.set(calls.get() + 1);
            text_symbols(kind, name)
        },
    )
    .unwrap();
    let prepared_calls = calls.get();
    assert!(prepared_calls > 0);
    let parameters = |created, lookup| {
        GqlParameters::new()
            .with_int64("created", created)
            .unwrap()
            .with_int64("lookup", lookup)
            .unwrap()
    };
    let first = template.bind_parameters(&parameters(1, 1)).unwrap();
    let second = template.bind_parameters(&parameters(1, 2)).unwrap();
    assert_ne!(first, second);
    assert_eq!(first, template.bind_parameters(&parameters(1, 1)).unwrap());
    assert_eq!(calls.get(), prepared_calls);
    assert_eq!(first.columns(), &["created", "found"]);
    assert!(
        template
            .bind_parameters(&GqlParameters::new().with_int64("created", 1).unwrap())
            .is_err()
    );
    assert!(
        template
            .bind_parameters(&parameters(1, 1).with_int64("extra", 3).unwrap())
            .is_err()
    );
}

#[test]
fn terminal_read_keeps_dropped_scope_and_refuses_another_write_or_missing_return() {
    for text in [
        "CREATE (n {p:1}) WITH n.p AS p MATCH (m) RETURN n.p",
        "CREATE (n) WITH n MATCH (m)",
        "CREATE (n) WITH n OPTIONAL MATCH (m) MERGE (x) RETURN x",
    ] {
        assert!(
            PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
        assert!(
            PreparedGraphWriteChainText::prepare(text, RelationId(1), text_symbols).is_err(),
            "{text}"
        );
    }
    // A MATCH that ends at CREATE is no terminal read but that CREATE's
    // prefix (LDBC SNB IU1's shape), so this chain prepares.
    let text = "CREATE (n) WITH n MATCH (m) CREATE (x) RETURN x";
    assert!(PreparedGraphWriteChainText::is_chain(text).unwrap());
    assert!(
        PreparedGraphWriteChainText::prepare(text, RelationId(1), text_symbols).is_ok(),
        "{text}"
    );
    for text in [
        "CREATE (n {p:'MATCH (x) RETURN x'}) RETURN n",
        "MERGE (n) ON MATCH SET n.p=1 RETURN n",
        "CREATE (n) RETURN n; MATCH (m) RETURN m",
    ] {
        assert!(
            !PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
    }
}
