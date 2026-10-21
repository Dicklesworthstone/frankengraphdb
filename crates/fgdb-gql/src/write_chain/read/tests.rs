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
