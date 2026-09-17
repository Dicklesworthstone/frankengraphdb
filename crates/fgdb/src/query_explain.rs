//! Shared native read classification and governed execution.

use super::{QueryError, QueryResult, aggregates, values};
use crate::Database;
use asupersync::fs::Vfs;
use fgdb_gql::*;
use fgdb_types::QueryCx;
use std::collections::BTreeMap;

/// A complete native read template, classified without binding parameter values.
pub enum PreparedNativeRead {
    Pattern(PreparedGraphText),
    Aggregate(PreparedGraphAggregateText),
    PipelineAggregate(PreparedGraphPipelineAggregateText),
    Set(PreparedGraphSetText),
    TemporalPattern(PreparedTemporalGraphText),
    TemporalSet(PreparedTemporalGraphSetText),
    TemporalAggregate(PreparedTemporalGraphAggregateText),
}

impl PreparedNativeRead {
    /// Classify using native parsers, from more specific grammars to general ones.
    /// Resolution, including misses, is frozen across all grammar probes.
    pub fn prepare(
        text: &str,
        params: &GqlParameters,
        mut resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, QueryError> {
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
            Ok(prepared) => return Ok(Self::TemporalAggregate(prepared)),
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedTemporalGraphText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::TemporalPattern(prepared)),
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedTemporalGraphSetText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::TemporalSet(prepared)),
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphPipelineAggregateText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::PipelineAggregate(prepared)),
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphAggregateText::prepare_with_parameter_types(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::Aggregate(prepared)),
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphText::prepare_with_parameter_types(text, &declarations, &mut resolve) {
            Ok(prepared) => return Ok(Self::Pattern(prepared)),
            Err(error) => diagnostics.push(error.to_string()),
        }
        match PreparedGraphSetText::prepare_with_parameter_types(text, &declarations, &mut resolve) {
            Ok(prepared) => Ok(Self::Set(prepared)),
            Err(error) => {
                diagnostics.push(error.to_string());
                Err(QueryError::Unsupported { diagnostics })
            }
        }
    }

    /// Return the native template's inferred and explicitly declared parameters.
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        match self {
            Self::Pattern(prepared) => prepared.parameter_schema(),
            Self::Aggregate(prepared) => prepared.parameter_schema(),
            Self::PipelineAggregate(prepared) => prepared.parameter_schema(),
            Self::Set(prepared) => prepared.parameter_schema(),
            Self::TemporalPattern(prepared) => prepared.parameter_schema(),
            Self::TemporalSet(prepared) => prepared.parameter_schema(),
            Self::TemporalAggregate(prepared) => prepared.parameter_schema(),
        }
    }

    /// Bind and execute the selected native engine with the caller's policy.
    /// Binding or execution errors never fall through to another classification.
    pub fn execute<V: Vfs + Clone>(
        &self,
        db: &Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        budget: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        match self {
            Self::TemporalAggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                let columns = prepared.columns().to_vec();
                let result = db
                    .execute_temporal_graph_aggregate_text_governed(cx, &query, budget)
                    .map_err(QueryError::Aggregate)?;
                Ok(aggregates(columns, prepared.output_slots(), result.value))
            }
            Self::TemporalPattern(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                let columns = query.pattern().columns().to_vec();
                let result = db
                    .execute_graph_pattern_governed_at(cx, query.pattern(), query.as_of(), budget)
                    .map_err(QueryError::Pattern)?;
                Ok(values(columns, result.value))
            }
            Self::TemporalSet(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::TemporalSetText)?;
                let result = db
                    .execute_graph_set_governed_at(cx, query.query(), query.as_of(), budget)
                    .map_err(QueryError::Set)?;
                Ok(values(prepared.columns().to_vec(), result.value))
            }
            Self::PipelineAggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PipelineText)?;
                let result = db
                    .execute_graph_aggregate_governed(cx, &query, budget)
                    .map_err(QueryError::Aggregate)?;
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            Self::Aggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = db
                    .execute_graph_aggregate_governed(cx, &query, budget)
                    .map_err(QueryError::Aggregate)?;
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            Self::Pattern(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = db
                    .execute_graph_pattern_governed(cx, &query, budget)
                    .map_err(QueryError::Pattern)?;
                Ok(values(query.columns().to_vec(), result.value))
            }
            Self::Set(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::SetText)?;
                let result = db
                    .execute_graph_set_governed(cx, &query, budget)
                    .map_err(QueryError::Set)?;
                Ok(values(prepared.columns().to_vec(), result.value))
            }
        }
    }
}
