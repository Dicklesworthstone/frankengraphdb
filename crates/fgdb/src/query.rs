//! Text entrypoints over the existing governed engines. Name resolution remains
//! caller supplied; this module neither invents a catalog nor grants authority.

use crate::{Database, GqlError, WriteTxn, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::*;
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use std::collections::BTreeMap;

type Cancel = Box<asupersync::error::Error>;

/// Lossless cells: identity/scalar values, counts, wide integer sums and exact
/// averages retain their native domains instead of narrowing to scalar Int.
pub type QueryValue = GraphAggregateValue;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryResult {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<QueryValue>>,
    },
    /// None means staged in the caller's still-open transaction, not committed.
    Write {
        receipt: GraphWriteProgramReceipt,
        completion: Option<EmbeddedTxnCompletion>,
    },
}

/// Original engine errors remain inspectable, including budget and source
/// refusal. Unsupported includes structural parser diagnostics, never values.
#[derive(Debug)]
pub enum QueryError {
    Unsupported { diagnostics: Vec<String> },
    PatternText(GraphPatternTextError),
    SetText(GraphSetTextError),
    PipelineText(GraphPipelineAggregateTextError),
    TemporalText(GraphTemporalTextError),
    TemporalSetText(GraphTemporalSetTextError),
    Pattern(GqlQueryError<GqlError, Cancel>),
    Aggregate(GqlQueryError<GraphAggregateError<GqlError>, Cancel>),
    Set(GqlQueryError<GraphSetExecutionError<GqlError>, Cancel>),
}
impl core::fmt::Display for QueryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported { diagnostics } => {
                write!(f, "unsupported query construct: {}", diagnostics.join("; "))
            }
            Self::PatternText(e) => e.fmt(f),
            Self::SetText(e) => e.fmt(f),
            Self::PipelineText(e) => e.fmt(f),
            Self::TemporalText(e) => e.fmt(f),
            Self::TemporalSetText(e) => e.fmt(f),
            Self::Pattern(e) => e.fmt(f),
            Self::Aggregate(e) => e.fmt(f),
            Self::Set(e) => e.fmt(f),
        }
    }
}
impl core::error::Error for QueryError {}

#[derive(Debug)]
pub enum QueryWriteError<A> {
    Prepare(GraphWriteScriptError),
    Execute(GraphWriteScriptExecutionError<WriteTxnError, A, Cancel>),
}
impl<A: core::fmt::Display> core::fmt::Display for QueryWriteError<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Prepare(e) => e.fmt(f),
            Self::Execute(e) => e.fmt(f),
        }
    }
}
impl<A: core::error::Error + 'static> core::error::Error for QueryWriteError<A> {}

fn values(columns: Vec<String>, rows: Vec<GraphValueRow>) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .into_iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(GraphAggregateValue::Value)
                    .collect()
            })
            .collect(),
    }
}
fn aggregates(
    columns: Vec<String>,
    slots: &[GraphAggregateTextSlot],
    rows: Vec<GraphAggregateRow>,
) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .into_iter()
            .map(|row| {
                slots
                    .iter()
                    .map(|slot| match *slot {
                        GraphAggregateTextSlot::GroupKey(i) => {
                            GraphAggregateValue::Value(row.keys()[i].clone())
                        }
                        GraphAggregateTextSlot::Aggregate(i) => row.values()[i].clone(),
                    })
                    .collect()
            })
            .collect(),
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a read using the native parsers as the classification authority.
    /// More specific grammars precede their general forms. A successful prepare
    /// commits the classification: binding/execution failures never try another
    /// engine. Resolution (including misses) is frozen across grammar probes.
    pub fn query(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        mut resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        budget: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        let mut symbols = BTreeMap::new();
        let mut resolve = |kind, name: &str| {
            *symbols
                .entry((kind, name.to_owned()))
                .or_insert_with(|| resolver(kind, name))
        };
        // Numeric arguments keep native inference; explicit scalar declarations
        // reach every prepare_with_parameter_types facade uniformly.
        let declarations: Vec<(&str, GqlParameterType)> = params
            .parameter_types()
            .filter(|(_, kind)| matches!(kind, GqlParameterType::Scalar(_)))
            .collect();
        let mut diagnostics = Vec::new();
        match PreparedTemporalGraphAggregateText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                let columns = prepared.columns().to_vec();
                let result = self
                    .execute_temporal_graph_aggregate_text_governed(cx, &query, budget)
                    .map_err(QueryError::Aggregate)?;
                return Ok(aggregates(columns, prepared.output_slots(), result.value));
            }
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedTemporalGraphText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                let columns = query.pattern().columns().to_vec();
                let result = self
                    .execute_graph_pattern_governed_at(cx, query.pattern(), query.as_of(), budget)
                    .map_err(QueryError::Pattern)?;
                return Ok(values(columns, result.value));
            }
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedTemporalGraphSetText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalSetText)?;
                let result = self
                    .execute_graph_set_governed_at(cx, query.query(), query.as_of(), budget)
                    .map_err(QueryError::Set)?;
                return Ok(values(prepared.columns().to_vec(), result.value));
            }
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphPipelineAggregateText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PipelineText)?;
                let result = self
                    .execute_graph_aggregate_governed(cx, &query, budget)
                    .map_err(QueryError::Aggregate)?;
                return Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ));
            }
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphText::prepare_with_parameter_types(text, &declarations, &mut resolve) {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = self
                    .execute_graph_aggregate_governed(cx, &query, budget)
                    .map_err(QueryError::Aggregate)?;
                return Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ));
            }
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphText::prepare_with_parameter_types(text, &declarations, &mut resolve) {
            Ok(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = self
                    .execute_graph_pattern_governed(cx, &query, budget)
                    .map_err(QueryError::Pattern)?;
                return Ok(values(query.columns().to_vec(), result.value));
            }
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphSetText::prepare_with_parameter_types(text, &declarations, &mut resolve)
        {
            Ok(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::SetText)?;
                let result = self
                    .execute_graph_set_governed(cx, &query, budget)
                    .map_err(QueryError::Set)?;
                Ok(values(prepared.columns().to_vec(), result.value))
            }
            Err(error) => {
                diagnostics.push(error.to_string());
                Err(QueryError::Unsupported { diagnostics })
            }
        }
    }

    /// Autocommit counterpart. Purpose contexts, relation coordinate and identity
    /// allocator remain explicit, exactly as in the existing native script API.
    /// A single statement is a one-step program; scripts share one work budget.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_write<A>(
        &mut self,
        txcx: &TxnCx,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        relation: RelationId,
        budget: GraphWriteProgramPolicy,
        allocate: impl FnMut(GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<QueryResult, QueryWriteError<A>> {
        let declarations: Vec<(&str, GqlParameterType)> = params
            .parameter_types()
            .filter(|(_, kind)| matches!(kind, GqlParameterType::Scalar(_)))
            .collect();
        let script = PreparedGraphWriteScript::prepare_with_parameter_types(
            text,
            relation,
            &declarations,
            resolver,
        )
        .map_err(QueryWriteError::Prepare)?;
        let (receipt, completion) = self
            .execute_graph_write_script_autocommit_governed(
                txcx, cx, commit_cx, &script, params, budget, allocate,
            )
            .await
            .map_err(QueryWriteError::Execute)?;
        Ok(QueryResult::Write {
            receipt,
            completion: Some(completion),
        })
    }
}

impl WriteTxn {
    /// Stage a native statement/script atomically inside this transaction.
    /// The caller alone decides when to finish the outer transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn query_write<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        relation: RelationId,
        budget: GraphWriteProgramPolicy,
        allocate: impl FnMut(GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<QueryResult, QueryWriteError<A>> {
        let declarations: Vec<(&str, GqlParameterType)> = params
            .parameter_types()
            .filter(|(_, kind)| matches!(kind, GqlParameterType::Scalar(_)))
            .collect();
        let script = PreparedGraphWriteScript::prepare_with_parameter_types(
            text,
            relation,
            &declarations,
            resolver,
        )
        .map_err(QueryWriteError::Prepare)?;
        let receipt = self
            .execute_graph_write_script_governed(database, cx, &script, params, budget, allocate)
            .map_err(QueryWriteError::Execute)?;
        Ok(QueryResult::Write {
            receipt,
            completion: None,
        })
    }
}
