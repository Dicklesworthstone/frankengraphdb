//! Shared native read classification and governed execution.

use super::{QueryError, QueryResult, aggregates, values};
use crate::Database;
use asupersync::fs::Vfs;
use fgdb_gql::*;
use fgdb_types::QueryCx;
use crate::gql_cert::{NativeCertificatePlan, NativePlanCertificate, NativeReadClass};
use fgdb_crypto::Digest;
use fgdb_types::CommitSeq;
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

/// Digest of the resolved native template plus facade class. It binds exact
/// statement structure, parameter names, declared/inferred types, positivity
/// and occurrence counts, and the native read class. Argument values and the
/// selected snapshot are deliberately excluded: types shape the plan, values
/// do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeExplainCertificate {
    certificate: NativePlanCertificate,
}

impl PreparedNativeRead {
    /// Deterministic class label for the EXPLAIN rows and certificate domain.
    #[must_use]
    pub fn facade_class(&self) -> NativeReadClass {
        match self {
            Self::Pattern(_) => NativeReadClass::Pattern,
            Self::Aggregate(_) => NativeReadClass::Aggregate,
            Self::PipelineAggregate(_) => NativeReadClass::PipelineAggregate,
            Self::Set(_) => NativeReadClass::Set,
            Self::TemporalPattern(_) => NativeReadClass::TemporalPattern,
            Self::TemporalSet(_) => NativeReadClass::TemporalSet,
            Self::TemporalAggregate(_) => NativeReadClass::TemporalAggregate,
        }
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        match self {
            Self::Pattern(prepared) => prepared.statement(),
            Self::Aggregate(prepared) => prepared.statement(),
            Self::PipelineAggregate(prepared) => prepared.statement(),
            Self::Set(prepared) => prepared.statement(),
            Self::TemporalPattern(prepared) => prepared.statement(),
            Self::TemporalSet(prepared) => prepared.statement(),
            Self::TemporalAggregate(prepared) => prepared.statement(),
        }
    }
}

impl NativeCertificatePlan for PreparedNativeRead {
    fn facade_class(&self) -> NativeReadClass {
        PreparedNativeRead::facade_class(self)
    }
    fn canonical_bytes(&self) -> Vec<u8> {
        // v1 template identity: exact resolved statement structure and the
        // normalized parameter table. Values are never present here because
        // preparation precedes binding.
        let mut bytes = b"fgdb:native-template-identity:v1\0".to_vec();
        let statement = self.statement().as_bytes();
        bytes.extend_from_slice(&(statement.len() as u64).to_be_bytes());
        bytes.extend_from_slice(statement);
        bytes
    }
    fn parameter_schema(&self) -> &[GqlParameterSpec] {
        PreparedNativeRead::parameter_schema(self)
    }
}

impl NativeExplainCertificate {
    #[must_use]
    pub fn new(prepared: &PreparedNativeRead, snapshot_seq: CommitSeq) -> Self {
        Self { certificate: NativePlanCertificate::new(prepared, snapshot_seq) }
    }

    #[must_use]
    pub fn verifies(&self, prepared: &PreparedNativeRead) -> bool {
        self.certificate.verifies(prepared)
    }

    #[must_use]
    pub fn verifies_at(&self, prepared: &PreparedNativeRead, snapshot_seq: CommitSeq) -> bool {
        self.certificate.verifies_at(prepared, snapshot_seq)
    }

    #[must_use]
    pub fn digest(&self) -> Digest {
        self.certificate.digest
    }

    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.certificate.snapshot_seq
    }
}

/// One EXPLAIN result row: ordered operator listing for the native read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainRow {
    pub operator: String,
    pub detail: String,
}


/// Deterministic human-readable operator rows derived from the resolved
/// template, never from an executed snapshot read.
#[must_use]
pub fn explain_rows(prepared: &PreparedNativeRead) -> Vec<ExplainRow> {
    let class = prepared.facade_class();
    let mut rows = vec![ExplainRow {
        operator: "NativeRead".to_owned(),
        detail: format!(
            "class={class:?} parameters={}",
            prepared.parameter_schema().len()
        ),
    }];
    let statement = prepared.statement();
    rows.push(ExplainRow {
        operator: "Template".to_owned(),
        detail: format!("{} bound columns", statement.len()),
    });
    for spec in prepared.parameter_schema() {
        rows.push(ExplainRow {
            operator: "Parameter".to_owned(),
            detail: format!(
                "name={} type={:?} occurrences={} positive={}",
                spec.name, spec.parameter_type, spec.occurrences, spec.requires_positive
            ),
        });
    }
    rows
}

impl<V: Vfs + Clone> Database<V> {
    /// EXPLAIN a native read statement without executing it.

    ///
    /// Preparation runs the exact native classification used by
    /// [`Database::query`], but binding and execution never happen: no
    /// snapshot record is read and no budget is consumed. Write statements
    /// refuse through the same typed classification error as reads.
    pub fn explain(
        &self,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        certificate: bool,
    ) -> Result<(Vec<ExplainRow>, Option<NativeExplainCertificate>), QueryError> {
        let prepared = PreparedNativeRead::prepare(text, params, resolver)?;
        let rows = explain_rows(&prepared);
        let cert = certificate.then(|| {
            // EXPLAIN is a successful top-level call: the live frontier read
            // has no failure mode left here beyond the plain read guard.
            let snapshot_seq = self.frontier().map_err(|_| QueryError::Unsupported {
                diagnostics: vec!["snapshot frontier unavailable for certificate".to_owned()],
            })?;
            Ok(NativeExplainCertificate::new(&prepared, snapshot_seq))
        });
        let cert = match cert {
            Some(result) => Some(result?),
            None => None,
        };
        Ok((rows, cert))
    }
}
