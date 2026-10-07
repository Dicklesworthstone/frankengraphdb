//! A fixed choice of an existing vertex or identified-edge pull operator.
//!
//! Native text preparation can choose a source shape without exposing two
//! unrelated return types. This sum does not prepare, retry, meter, prefetch or
//! collect anything: the chosen cursor keeps its source and cumulative policy.

use crate::algebra::GraphValueRow;
use crate::edge_stream::{EdgeScanCursor, EdgeScanError, EdgeScanSource, EdgeScanState};
use crate::stream::{VertexScanCursor, VertexScanError, VertexScanSource};
use crate::{GlaExecutionStats, GqlExecutionStats, GqlQueryError};
use fgdb_types::CommitSeq;

/// Compiler-owned terminal clauses deferred by a blocking scan consumer.
/// A sort-input cursor is NOT a completed query: the consumer must order all
/// admitted occurrences, apply DISTINCT when requested, then SKIP/LIMIT and
/// retain only the visible prefix. Hidden sort cells remain through comparison
/// and pagination; DISTINCT is admitted only when every evaluated cell is visible.
/// This contains no source, authority, mutable plan or caller-asserted order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanSortTail {
    evaluation_width: usize,
    visible_width: usize,
    order: Vec<crate::algebra::GraphValueOrder>,
    distinct: bool,
    offset: u64,
    count: Option<u64>,
}

impl ScanSortTail {
    /// Complete row width emitted by the private sort-input cursor.
    /// The trailing cells, if any, are compiler-owned hidden ORDER BY keys.
    pub fn evaluation_width(&self) -> usize {
        self.evaluation_width
    }
    /// Public result width after ordering, DISTINCT and pagination.
    pub fn visible_width(&self) -> usize {
        self.visible_width
    }
    pub fn order(&self) -> &[crate::algebra::GraphValueOrder] {
        &self.order
    }
    pub fn distinct(&self) -> bool {
        self.distinct
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn count(&self) -> Option<u64> {
        self.count
    }

    // Only physical compilers call this. They must still audit EVERY source,
    // predicate and projection instruction, not just recognize the final tail.
    pub(crate) fn compile(plan: &crate::algebra::GlaPlan<GraphValueRow>) -> Result<Self, usize> {
        use crate::algebra::{GlaOperator, GraphValueOrder, MAX_PATTERN_VERTICES};
        let ops = plan.operators();
        let at = ops.len().checked_sub(2).ok_or(0_usize)?;
        let Some(GlaOperator::Limit { offset, count }) = ops.last() else {
            return Err(at + 1);
        };
        let preceding = at.checked_sub(1).ok_or(at)?;
        let distinct = matches!(ops.get(preceding), Some(GlaOperator::Distinct));
        let projection = if distinct {
            preceding.checked_sub(1).ok_or(preceding)?
        } else {
            preceding
        };
        let Some(GlaOperator::ProjectValues { columns }) = ops.get(projection) else {
            return Err(projection);
        };
        if columns.is_empty() || columns.len() > MAX_PATTERN_VERTICES {
            return Err(projection);
        }
        let evaluation_width = columns.len();
        let visible_width = plan.visible_columns.unwrap_or(evaluation_width);
        if visible_width == 0
            || visible_width > evaluation_width
            || (distinct && plan.visible_columns.is_some())
        {
            // Hidden cells must never change DISTINCT's equivalence classes.
            // The text compiler already refuses this combination; audit the
            // logical metadata here too, before any source or LIMIT is driven.
            return Err(projection);
        }
        let order = match &ops[at] {
            // Implicit whole-row order is canonical GraphValue order, whose
            // Null scalar is least. Explicit ASC's default NULLS LAST differs.
            GlaOperator::OrderByValues => (0..columns.len())
                .map(|column| GraphValueOrder::ascending(column).with_nulls_first(true))
                .collect(),
            GlaOperator::OrderByValueColumns { columns: keys } => {
                if keys.is_empty() || keys.len() > MAX_PATTERN_VERTICES {
                    return Err(at);
                }
                for (index, key) in keys.iter().enumerate() {
                    if key.column >= columns.len()
                        || keys[..index].iter().any(|other| other.column == key.column)
                    {
                        return Err(at);
                    }
                }
                keys.to_vec()
            }
            _ => return Err(at),
        };
        Ok(Self {
            evaluation_width,
            visible_width,
            order,
            distinct,
            offset: *offset,
            count: *count,
        })
    }
}

// Both physical cursors implement the same lifecycle. Reuse its existing type
// so consumers of the native vertex stream retain their state comparisons.
pub use crate::stream::VertexScanState as ScanState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanKind {
    Vertex,
    Edge,
}

/// Preserve the original operator/source error rather than stringifying it.
/// Resource and interruption errors stay outside this sum in GqlQueryError.
#[derive(Debug)]
pub enum ScanError<E> {
    Vertex(VertexScanError<E>),
    Edge(EdgeScanError<E>),
}
impl<E: core::fmt::Display> core::fmt::Display for ScanError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vertex(error) => error.fmt(f),
            Self::Edge(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for ScanError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Vertex(error) => Some(error),
            Self::Edge(error) => Some(error),
        }
    }
}

/// One already-open physical scan, not an iterator over an eager result bag.
/// Both variants emit GraphValueRow and preserve their native order, counters,
/// late-error behavior and source lifetime. Switching physical strategies is
/// never an error-recovery mechanism; the host chooses before opening a source.
/// Source errors have one shared domain; checkpoint closures may differ while
/// retaining the same interruption type. No extra allocation or meter is added.
pub enum ScanCursor<VS, VF, ES, EF> {
    Vertex(VertexScanCursor<VS, VF, GraphValueRow>),
    Edge(EdgeScanCursor<ES, EF>),
}
impl<VS, VF, ES, EF> ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
{
    #[must_use]
    pub fn kind(&self) -> ScanKind {
        match self {
            Self::Vertex(_) => ScanKind::Vertex,
            Self::Edge(_) => ScanKind::Edge,
        }
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        match self {
            Self::Vertex(cursor) => cursor.snapshot_seq(),
            Self::Edge(cursor) => cursor.snapshot_seq(),
        }
    }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        match self {
            Self::Vertex(cursor) => cursor.row_stats(),
            Self::Edge(cursor) => cursor.row_stats(),
        }
    }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        match self {
            Self::Vertex(cursor) => cursor.evaluator_stats(),
            Self::Edge(cursor) => cursor.evaluator_stats(),
        }
    }
    #[must_use]
    pub fn state(&self) -> ScanState {
        match self {
            Self::Vertex(cursor) => cursor.state(),
            Self::Edge(cursor) => match cursor.state() {
                EdgeScanState::Open => ScanState::Open,
                EdgeScanState::Exhausted => ScanState::Exhausted,
                EdgeScanState::Closed => ScanState::Closed,
                EdgeScanState::Failed => ScanState::Failed,
            },
        }
    }
    /// Idempotent; the selected cursor releases its pin without another pull.
    pub fn close(&mut self) {
        match self {
            Self::Vertex(cursor) => cursor.close(),
            Self::Edge(cursor) => cursor.close(),
        }
    }
}
impl<VS, VF, ES, EF, C> Iterator for ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
    VF: FnMut() -> Result<(), C>,
    EF: FnMut() -> Result<(), C>,
{
    type Item = Result<GraphValueRow, GqlQueryError<ScanError<VS::Error>, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Vertex(cursor) => cursor
                .next()
                .map(|row| row.map_err(|error| error.map_source(ScanError::Vertex))),
            Self::Edge(cursor) => cursor
                .next()
                .map(|row| row.map_err(|error| error.map_source(ScanError::Edge))),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Vertex(cursor) => cursor.size_hint(),
            Self::Edge(cursor) => cursor.size_hint(),
        }
    }
}
impl<VS, VF, ES, EF, C> std::iter::FusedIterator for ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
    VF: FnMut() -> Result<(), C>,
    EF: FnMut() -> Result<(), C>,
{
}
impl<VS, VF, ES, EF> core::fmt::Debug for ScanCursor<VS, VF, ES, EF> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vertex(cursor) => f.debug_tuple("ScanCursor::Vertex").field(cursor).finish(),
            Self::Edge(cursor) => f.debug_tuple("ScanCursor::Edge").field(cursor).finish(),
        }
    }
}

#[cfg(test)]
mod sort_tail_tests {
    use super::*;
    use crate::algebra::{
        BindingSlot, EdgeRelation, GlaDirection, GlaOperator, GlaPlan, GraphColumn,
        GraphPatternBuilder, GraphValueOrder, ValueProjection,
    };
    use crate::edge_stream::EdgeScanPlan;
    use crate::stream::VertexScanPlan;
    use fgdb_delta_types::{PropertyKeyId, RelationId};

    fn plan(
        edge: bool,
        distinct: bool,
        visible: Option<usize>,
        count: Option<u64>,
    ) -> GlaPlan<GraphValueRow> {
        let mut operators = vec![
            if edge {
                GlaOperator::ScanEdges {
                    relation: EdgeRelation::One(RelationId(1)),
                    direction: GlaDirection::Forward,
                }
            } else {
                GlaOperator::ScanVertices
            },
            GlaOperator::ProjectValues {
                columns: vec![
                    ValueProjection::Vertex {
                        slot: BindingSlot(0),
                    },
                    ValueProjection::Property {
                        slot: BindingSlot(u32::from(edge)),
                        key: PropertyKeyId(1),
                    },
                    ValueProjection::Property {
                        slot: BindingSlot(0),
                        key: PropertyKeyId(2),
                    },
                ],
            },
        ];
        if distinct {
            operators.push(GlaOperator::Distinct);
        }
        operators.push(GlaOperator::OrderByValueColumns {
            columns: vec![
                GraphValueOrder::descending(1).with_nulls_first(true),
                GraphValueOrder::ascending(2),
            ]
            .into(),
        });
        operators.push(GlaOperator::Limit { offset: 7, count });
        let mut plan = GlaPlan::from_operators(operators);
        plan.visible_columns = visible;
        plan
    }

    #[test]
    fn hidden_key_widths_and_windows_come_from_the_complete_vertex_or_edge_plan() {
        for edge in [false, true] {
            for count in [None, Some(0), Some(3)] {
                let logical = plan(edge, false, Some(1), count);
                let tail = if edge {
                    assert!(EdgeScanPlan::compile(&logical).is_err());
                    EdgeScanPlan::compile_sort_input(&logical).unwrap().1
                } else {
                    assert!(VertexScanPlan::<GraphValueRow>::compile(&logical).is_err());
                    VertexScanPlan::compile_sort_input(&logical).unwrap().1
                };
                assert_eq!(tail.evaluation_width(), 3);
                assert_eq!(tail.visible_width(), 1);
                assert_eq!(tail.offset(), 7);
                assert_eq!(tail.count(), count);
                assert!(!tail.distinct());
                assert_eq!(
                    tail.order(),
                    &[
                        GraphValueOrder::descending(1).with_nulls_first(true),
                        GraphValueOrder::ascending(2),
                    ],
                );
                assert_eq!(tail, ScanSortTail::compile(&logical).unwrap());
            }
        }
    }

    #[test]
    fn hidden_distinct_and_invalid_visible_widths_refuse_even_at_limit_zero() {
        for edge in [false, true] {
            for (distinct, visible) in [(false, 0), (false, 4), (true, 1), (true, 2), (true, 3)] {
                let logical = plan(edge, distinct, Some(visible), Some(0));
                assert!(ScanSortTail::compile(&logical).is_err());
                if edge {
                    assert!(EdgeScanPlan::compile_sort_input(&logical).is_err());
                } else {
                    assert!(VertexScanPlan::compile_sort_input(&logical).is_err());
                }
            }
            let logical = plan(edge, true, None, Some(0));
            let tail = ScanSortTail::compile(&logical).unwrap();
            assert_eq!(tail.evaluation_width(), 3);
            assert_eq!(tail.visible_width(), 3);
            assert!(tail.distinct());
        }
    }

    #[test]
    fn ordinary_vertex_compilation_keeps_the_visible_row_contract() {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let complete = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("id", "n"),
                    GraphColumn::property("score", "n", PropertyKeyId(1)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        assert!(VertexScanPlan::<GraphValueRow>::compile(complete.plan()).is_ok());
        let hidden = complete.with_visible_columns(1);
        assert_eq!(hidden.columns(), &["id"]);
        assert!(VertexScanPlan::<GraphValueRow>::compile(hidden.plan()).is_err());
        let (_, tail) = VertexScanPlan::compile_sort_input(hidden.plan()).unwrap();
        assert_eq!(tail.evaluation_width(), 2);
        assert_eq!(tail.visible_width(), 1);
    }
}
