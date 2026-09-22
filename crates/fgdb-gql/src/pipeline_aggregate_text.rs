//! Parse-once exact aggregation after native WITH row stages.
//!
//! This is preparation metadata. Binding emits the existing
//! PreparedGraphAggregate with its real source leaf and relational input.

use crate::set_text::{BoundSetTextInput, ReadFilterOp, ReadPageNumber, ReadStageTemplate};
use crate::{
    GqlParameterSpec, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateFunction,
    GraphAggregateOrder, GraphAggregateTextSlot, GraphHavingError, GraphNullPlacement,
    GraphPatternTextError, GraphSetTextError, GraphSetTextErrorKind,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphPipelineAggregateTextErrorKind {
    Input(GraphSetTextErrorKind),
    Build(GraphAggregateBuildError),
    Having(GraphHavingError),
}

/// Original UTF-8 byte coordinate; neither source fragments nor parameter
/// payloads are included in diagnostics. Execution retains ordinary aggregate
/// errors rather than converting data failures into parser errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPipelineAggregateTextError {
    pub offset: usize,
    pub kind: GraphPipelineAggregateTextErrorKind,
}
impl core::fmt::Display for GraphPipelineAggregateTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "aggregate pipeline at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphPipelineAggregateTextError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.kind {
            GraphPipelineAggregateTextErrorKind::Build(error) => Some(error),
            GraphPipelineAggregateTextErrorKind::Having(error) => Some(error),
            GraphPipelineAggregateTextErrorKind::Input(_) => None,
        }
    }
}
impl From<GraphSetTextError> for GraphPipelineAggregateTextError {
    fn from(error: GraphSetTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphPipelineAggregateTextErrorKind::Input(error.kind),
        }
    }
}
impl From<GraphPatternTextError> for GraphPipelineAggregateTextError {
    fn from(error: GraphPatternTextError) -> Self {
        GraphSetTextError::from(error).into()
    }
}

#[derive(Clone)]
pub(crate) struct PipelineSummary {
    pub(crate) name: String,
    pub(crate) function: GraphAggregateFunction,
    pub(crate) column: Option<usize>,
}

/// Native MATCH ... WITH ... RETURN aggregate [GROUP BY ...] [HAVING ...]
/// [ORDER BY ...] [SKIP ...] [LIMIT ...]. WITH expressions/filters/pages retain
/// their original boundaries. Terminal keys and aggregate arguments refer to
/// completed row aliases, not discarded graph variables or property lookups.
/// Grouping keys and aggregate arguments accept the shared row expressions:
/// typed parameters, literals, checked integer arithmetic, conditionals and
/// lists. COUNT(*), COUNT, SUM/SUM_INT, AVG/AVG_INT, MIN, MAX, COLLECT and argument
/// DISTINCT use the existing exact engine. AS is optional for direct aggregates;
/// native default names are count/sum/avg/min/max/collect and must be unique.
///
/// Without GROUP BY, distinct whole nonaggregate RETURN expressions are keys
/// in first-use order. Explicit GROUP BY remains authoritative and may include
/// hidden keys. It accepts input expressions and unshadowed RETURN key aliases;
/// existing WITH aliases retain their meaning. Computed RETURN keys require AS.
/// Repeated key expressions share storage while retaining their output slots.
/// HAVING is three-valued Boolean over returned aliases and
/// literals/parameters; ORDER BY also uses returned aliases. Keys may be renamed
/// in RETURN. output_slots() maps written column order to GraphAggregateRow's
/// separate key/summary arrays. No cast to a narrower scalar domain occurs.
///
/// One frozen parameter/catalog contract spans the complete statement. Binding
/// neither reparses text nor reopens a catalog. The result executes through all
/// existing Database, historical, pinned-view and WriteTxn aggregate APIs.
/// Identical terminal expressions share one late, compact ProjectValues stage,
/// after every preceding filter, DISTINCT, UNWIND and page. Plain-column inputs
/// retain their original definitions and execution traces. Projection consumes
/// the same depth/work/scratch budgets; output LIMIT never hides input errors.
/// This bounded single-source profile does not implement aggregate WITH stages,
/// binary set inputs, expressions combining aggregate results, or writes after WITH.
#[derive(Clone)]
pub struct PreparedGraphPipelineAggregateText {
    pub(crate) statement: String,
    pub(crate) input: BoundSetTextInput,
    pub(crate) keys: Vec<usize>,
    pub(crate) output_keys: Vec<usize>,
    pub(crate) summaries: Vec<PipelineSummary>,
    pub(crate) output_distinct: bool,
    pub(crate) names: Vec<String>,
    pub(crate) slots: Vec<GraphAggregateTextSlot>,
    pub(crate) having: Vec<ReadFilterOp>,
    pub(crate) having_columns: Vec<GraphAggregateColumn>,
    pub(crate) ordering: Vec<GraphAggregateOrder>,
    pub(crate) offset: ReadPageNumber,
    pub(crate) count: Option<ReadPageNumber>,
    pub(crate) aggregate_at: usize,
    pub(crate) having_at: usize,
}
impl core::fmt::Debug for PreparedGraphPipelineAggregateText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphPipelineAggregateText")
            .field("columns", &self.names.len())
            .field("parameters", &self.input.parameter_schema().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphPipelineAggregateText {
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.names
    }
    #[must_use]
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        &self.slots
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.input.parameter_schema()
    }

    /// Versioned, value-independent template transcript: WITH pipeline stages
    /// (unwind/project/filter/page in declaration order), correlations,
    /// aggregate keys/summaries, HAVING program, ordering and paging shape.
    /// Statement text, byte offsets and parameter values never enter; each
    /// parameter appears as its declared argument index.
    #[must_use]
    pub fn canonical_template_bytes(&self) -> Vec<u8> {
        fn ordinal(bytes: &mut Vec<u8>, value: usize) {
            bytes.extend_from_slice(&(value as u64).to_be_bytes());
        }
        let mut bytes = b"fgdb:gql:pipeline-aggregate-text-template:v1\0".to_vec();
        self.input.append_template_transcript(&mut bytes);
        ordinal(&mut bytes, self.keys.len());
        for key in &self.keys {
            ordinal(&mut bytes, *key);
        }
        ordinal(&mut bytes, self.output_keys.len());
        for key in &self.output_keys {
            ordinal(&mut bytes, *key);
        }
        ordinal(&mut bytes, self.summaries.len());
        for summary in &self.summaries {
            bytes.push(match summary.function {
                GraphAggregateFunction::CountRows => 0,
                GraphAggregateFunction::Count => 1,
                GraphAggregateFunction::CountDistinct => 2,
                GraphAggregateFunction::SumInt => 3,
                GraphAggregateFunction::SumIntDistinct => 4,
                GraphAggregateFunction::AverageInt => 5,
                GraphAggregateFunction::AverageIntDistinct => 6,
                GraphAggregateFunction::Min => 7,
                GraphAggregateFunction::Max => 8,
                GraphAggregateFunction::Collect => 9,
                GraphAggregateFunction::CollectDistinct => 10,
            });
            match summary.column {
                None => bytes.push(0),
                Some(column) => {
                    bytes.push(1);
                    ordinal(&mut bytes, column);
                }
            }
        }
        bytes.push(u8::from(self.output_distinct));
        ordinal(&mut bytes, self.names.len());
        for column in &self.names {
            ordinal(&mut bytes, column.len());
            bytes.extend_from_slice(column.as_bytes());
        }
        ordinal(&mut bytes, self.slots.len());
        for slot in &self.slots {
            match slot {
                GraphAggregateTextSlot::GroupKey(at) => {
                    bytes.push(0);
                    ordinal(&mut bytes, *at);
                }
                GraphAggregateTextSlot::Aggregate(at) => {
                    bytes.push(1);
                    ordinal(&mut bytes, *at);
                }
            }
        }
        ordinal(&mut bytes, self.having.len());
        for op in &self.having {
            op.append_template_transcript(&mut bytes);
        }
        ordinal(&mut bytes, self.having_columns.len());
        for column in &self.having_columns {
            encode_aggregate_column(&mut bytes, column);
        }
        ordinal(&mut bytes, self.ordering.len());
        for order in &self.ordering {
            encode_aggregate_column(&mut bytes, &order.column);
            bytes.push(u8::from(order.descending));
            bytes.push(match order.nulls {
                GraphNullPlacement::First => 0,
                GraphNullPlacement::Last => 1,
            });
        }
        self.offset.append_template_transcript(&mut bytes);
        match &self.count {
            None => bytes.push(0),
            Some(count) => {
                bytes.push(1);
                count.append_template_transcript(&mut bytes);
            }
        }
        bytes
    }

    /// Logical template operators in evaluation order: the WITH row stages,
    /// then grouping/summaries, optional HAVING, ordering and output shaping.
    #[must_use]
    pub fn template_operators(&self) -> Vec<&'static str> {
        fn stages(operators: &mut Vec<&'static str>, stages: &[ReadStageTemplate]) {
            for stage in stages {
                operators.push(match stage {
                    ReadStageTemplate::Unwind { .. } => "Unwind",
                    ReadStageTemplate::Project { .. } => "ProjectValues",
                    ReadStageTemplate::Filter { .. } => "Select",
                    ReadStageTemplate::Page { .. } => "OrderByPage",
                });
                if matches!(
                    stage,
                    ReadStageTemplate::Project {
                        quantifier: crate::GraphSetQuantifier::Distinct,
                        ..
                    }
                ) {
                    operators.push("Distinct");
                }
            }
        }
        let mut operators = Vec::new();
        stages(&mut operators, &self.input.leading);
        if let Some(selection) = &self.input.selection {
            operators.push("ScanGraphText");
            operators.extend(selection.template_operators());
            if !self.input.leading.is_empty() || self.input.singleton {
                operators.push("CrossJoin");
                if !self.input.correlations.is_empty() {
                    operators.push("Select");
                }
            }
        }
        if self.input.projection.is_some() {
            operators.push("ProjectValues");
            if self.input.quantifier == crate::GraphSetQuantifier::Distinct {
                operators.push("Distinct");
            }
        }
        stages(&mut operators, &self.input.pipeline);
        operators.push("Aggregate");
        if !self.having.is_empty() {
            operators.push("SelectHaving");
        }
        if !self.ordering.is_empty() {
            operators.push("OrderByAggregate");
        }
        if self.output_distinct {
            operators.push("Distinct");
        }
        operators.push("Limit");
        operators
    }
}

fn encode_aggregate_column(bytes: &mut Vec<u8>, column: &GraphAggregateColumn) {
    match column {
        GraphAggregateColumn::GroupKey(at) => {
            bytes.push(0);
            bytes.extend_from_slice(&(*at as u64).to_be_bytes());
        }
        GraphAggregateColumn::Aggregate(at) => {
            bytes.push(1);
            bytes.extend_from_slice(&(*at as u64).to_be_bytes());
        }
    }
}
