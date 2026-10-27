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
fn vertex_merge_with_unwind_feeds_a_matched_relationship_merge_without_rebinding() {
    use crate::{GqlParameterType, GraphVertexMergeRowsBinding, GraphWriteChainStep};

    let text = "MERGE (a {p:$created}) WITH a UNWIND $keys AS k \
        MATCH (b {p:k}) MERGE (a)-[r:R]->(b) \
        ON CREATE SET r.p=b.p ON MATCH SET r.p=r.p+$bump \
        RETURN b.p AS target,r.p AS weight ORDER BY target,weight";
    let calls = Cell::new(0);
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        text,
        RelationId(1),
        &[
            ("created", GqlParameterType::Int64),
            ("keys", GqlParameterType::List),
            ("bump", GqlParameterType::Int64),
        ],
        |kind, name: &str| {
            calls.set(calls.get() + 1);
            text_symbols(kind, name)
        },
    )
    .unwrap();
    let prepared_calls = calls.get();
    let parameters = |values: &[i64]| {
        GqlParameters::new()
            .with_int64("created", 7)
            .unwrap()
            .with_list("keys", values.iter().copied().map(int).collect())
            .unwrap()
            .with_int64("bump", 1)
            .unwrap()
    };
    let first = template.bind_parameters(&parameters(&[2, 1, 2])).unwrap();
    let empty = template.bind_parameters(&parameters(&[])).unwrap();
    assert_eq!(calls.get(), prepared_calls);
    assert_ne!(first, empty);
    assert_eq!(first.columns(), &["target", "weight"]);
    assert!(matches!(
        first.steps(),
        [
            GraphWriteChainStep::VertexMergeQuery(_),
            GraphWriteChainStep::EdgeUpsertQuery { .. }
        ]
    ));

    for (chain, expected) in [
        (
            &first,
            vec![
                vec![vertex(WIDE), int(2)],
                vec![vertex(WIDE), int(1)],
                vec![vertex(WIDE), int(2)],
            ],
        ),
        (&empty, Vec::new()),
    ] {
        let GraphWriteChainStep::VertexMergeQuery(query) = &chain.steps()[0] else {
            unreachable!("the WITH head belongs to the first vertex MERGE")
        };
        assert_eq!(query.bindings(), &[GraphVertexMergeRowsBinding::Vertex]);
        assert_eq!(query.columns(), &["a", "k"]);
        let finished = query
            .finish_governed::<(), ()>(
                vec![vec![vertex(WIDE)]],
                GqlExecutionStats {
                    snapshot_records: 5,
                    result_rows: 1,
                },
                GlaExecutionStats {
                    work_units: 11,
                    scratch_entries: 7,
                },
                GqlQueryPolicy::new(5, expected.len() as u64, HIGH, HIGH),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(
            finished
                .value
                .iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            expected,
        );
        assert_eq!(finished.rows.snapshot_records, 5);
        assert_eq!(finished.rows.result_rows, expected.len() as u64);
        assert!(finished.evaluator.work_units > 11);
        assert!(finished.evaluator.scratch_entries > 7);
    }
}

#[test]
fn vertex_merge_tail_filters_final_properties_and_preserves_the_native_budget() {
    use crate::{GraphVertexMergeRowsBinding, GraphWriteChainStep};

    let text = "UNWIND [1,2,3] AS input MERGE (n {p:input}) \
        WITH n WHERE n.p > 1 UNWIND [n.p,n.p+10] AS value \
        WITH sum(value) AS total RETURN total";
    let chain = PreparedGraphWriteChainText::prepare(text, RelationId(1), text_symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let [GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
        unreachable!("one MERGE and its native row tail")
    };
    assert_eq!(
        query.bindings(),
        &[
            GraphVertexMergeRowsBinding::Vertex,
            GraphVertexMergeRowsBinding::Property(PropertyKeyId(1)),
        ],
    );
    assert_eq!(query.columns(), &["total"]);
    let rows = || {
        [1_u128, 2, 3]
            .into_iter()
            .map(|id| vec![vertex(VId(id)), int(id as i64)])
            .collect()
    };
    let selected = GqlExecutionStats {
        snapshot_records: 3,
        result_rows: 3,
    };
    let before = GlaExecutionStats {
        work_units: 13,
        scratch_entries: 5,
    };
    let finished = query
        .finish_governed::<(), ()>(
            rows(),
            selected,
            before,
            GqlQueryPolicy::new(3, 1, HIGH, HIGH),
            || Ok(()),
        )
        .unwrap();
    assert_eq!(finished.value.len(), 1);
    assert_eq!(finished.value[0].values(), &[int(30)]);
    assert_eq!(finished.rows.snapshot_records, 3);
    assert_eq!(finished.rows.result_rows, 1);
    assert!(finished.evaluator.work_units > before.work_units);
    assert!(finished.evaluator.scratch_entries > before.scratch_entries);
    for budget in [
        GqlQueryPolicy::new(3, 1, finished.evaluator.work_units - 1, HIGH),
        GqlQueryPolicy::new(3, 1, HIGH, finished.evaluator.scratch_entries - 1),
        GqlQueryPolicy::new(3, 0, HIGH, HIGH),
    ] {
        assert!(
            query
                .finish_governed::<(), ()>(rows(), selected, before, budget, || Ok(()))
                .is_err(),
        );
    }
    let mut checkpoints = 0;
    assert!(matches!(
        query.finish_governed::<(), &str>(
            rows(),
            selected,
            before,
            GqlQueryPolicy::new(3, 1, HIGH, HIGH),
            || {
                checkpoints += 1;
                if checkpoints >= 20 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            },
        ),
        Err(GqlQueryError::Interrupted("cancelled")),
    ));
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
