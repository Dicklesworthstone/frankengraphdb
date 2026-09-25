//! Shared native read classification and governed execution.

use super::{QueryError, QueryResult, aggregates, values};
use crate::Database;
use crate::gql_cert::{
    NativeCertificatePlan, NativePlanCertificate, NativeReadClass, NativeResultCertificate,
};
use asupersync::fs::Vfs;
use fgdb_crypto::Digest;
use fgdb_gql::*;
use fgdb_types::CommitSeq;
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

struct CachingResolver<'a, R: ?Sized> {
    resolver: &'a mut R,
    symbols: BTreeMap<(GraphSymbolKind, String), Option<GraphSymbol>>,
}

impl<R: GraphSymbolResolver + ?Sized> GraphSymbolResolver for CachingResolver<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        *self
            .symbols
            .entry((kind, name.to_owned()))
            .or_insert_with(|| self.resolver.resolve_symbol(kind, name))
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        self.resolver.reverse_catalog()
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.resolver.reverse_label(id)
    }

    fn reverse_relation(&self, id: fgdb_delta_types::RelationId) -> Option<String> {
        self.resolver.reverse_relation(id)
    }
}

impl<R: GraphSymbolResolver + ?Sized> GraphSymbolResolver for &mut CachingResolver<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        (**self).resolve_symbol(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        (**self).reverse_catalog()
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        (**self).reverse_label(id)
    }

    fn reverse_relation(&self, id: fgdb_delta_types::RelationId) -> Option<String> {
        (**self).reverse_relation(id)
    }
}

impl PreparedNativeRead {
    /// Classify using native parsers, from more specific grammars to general ones.
    /// Resolution, including misses, is frozen across all grammar probes.
    /// Refusals select the largest typed byte offset. Ties prefer temporal
    /// pattern, aggregate, then set when the native selector parser recognizes
    /// a system-time clause; otherwise pattern, aggregate, pipeline, then set.
    /// MissingSystemTimeClause is not a candidate. Successful probe order is
    /// unchanged, and selection never parses diagnostic strings.
    pub fn prepare(
        text: &str,
        params: &GqlParameters,
        mut resolver: impl GraphSymbolResolver,
    ) -> Result<Self, QueryError> {
        let mut resolve = CachingResolver {
            resolver: &mut resolver,
            symbols: BTreeMap::new(),
        };
        // Numeric arguments keep native inference; explicit scalar and list
        // declarations reach every prepare_with_parameter_types facade.
        let declarations: Vec<(&str, GqlParameterType)> = params
            .parameter_types()
            .filter(|(_, kind)| {
                matches!(kind, GqlParameterType::Scalar(_) | GqlParameterType::List)
            })
            .collect();
        let mut best = None;
        let mut consider = |offset, priority, facade, source| {
            if best
                .as_ref()
                .is_none_or(|(old_offset, old_priority, _, _)| {
                    offset > *old_offset || (offset == *old_offset && priority < *old_priority)
                })
            {
                best = Some((offset, priority, facade, source));
            }
        };
        match PreparedTemporalGraphAggregateText::prepare_with_parameter_types(
            text,
            &declarations,
            |kind, name| resolve.resolve_symbol(kind, name),
        ) {
            Ok(prepared) => return Ok(Self::TemporalAggregate(prepared)),
            Err(error) => {
                if !matches!(
                    error.kind,
                    GraphTemporalTextErrorKind::MissingSystemTimeClause
                        | GraphTemporalTextErrorKind::Query(
                            GraphPatternTextErrorKind::DefinitionTooLarge
                                | GraphPatternTextErrorKind::TooManyTokens
                        )
                ) {
                    consider(
                        error.offset,
                        1,
                        NativeReadClass::TemporalAggregate,
                        QueryError::TemporalText(error),
                    );
                }
            }
        }
        match PreparedTemporalGraphText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::TemporalPattern(prepared)),
            Err(error) => {
                if !matches!(
                    error.kind,
                    GraphTemporalTextErrorKind::MissingSystemTimeClause
                        | GraphTemporalTextErrorKind::Query(
                            GraphPatternTextErrorKind::DefinitionTooLarge
                                | GraphPatternTextErrorKind::TooManyTokens
                        )
                ) {
                    consider(
                        error.offset,
                        0,
                        NativeReadClass::TemporalPattern,
                        QueryError::TemporalText(error),
                    );
                }
            }
        }
        match PreparedTemporalGraphSetText::prepare_with_parameter_types(
            text,
            &declarations,
            |kind, name| resolve.resolve_symbol(kind, name),
        ) {
            Ok(prepared) => return Ok(Self::TemporalSet(prepared)),
            Err(error) => {
                if !matches!(
                    error.kind,
                    GraphTemporalSetTextErrorKind::MissingSystemTimeClause
                        | GraphTemporalSetTextErrorKind::Set(GraphSetTextErrorKind::Pattern(
                            GraphPatternTextErrorKind::DefinitionTooLarge
                                | GraphPatternTextErrorKind::TooManyTokens
                        ))
                ) {
                    consider(
                        error.offset,
                        2,
                        NativeReadClass::TemporalSet,
                        QueryError::TemporalSetText(error),
                    );
                }
            }
        }
        match PreparedGraphPipelineAggregateText::prepare_with_parameter_types(
            text,
            &declarations,
            |kind, name| resolve.resolve_symbol(kind, name),
        ) {
            Ok(prepared) => return Ok(Self::PipelineAggregate(prepared)),
            Err(error) => consider(
                error.offset,
                5,
                NativeReadClass::PipelineAggregate,
                QueryError::PipelineText(error),
            ),
        }
        match PreparedGraphAggregateText::prepare_with_parameter_types(
            text,
            &declarations,
            |kind, name| resolve.resolve_symbol(kind, name),
        ) {
            Ok(prepared) => return Ok(Self::Aggregate(prepared)),
            Err(error) => consider(
                error.offset,
                4,
                NativeReadClass::Aggregate,
                QueryError::PatternText(error),
            ),
        }
        match PreparedGraphText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::Pattern(prepared)),
            Err(error) => consider(
                error.offset,
                3,
                NativeReadClass::Pattern,
                QueryError::PatternText(error),
            ),
        }
        match PreparedGraphSetText::prepare_with_parameter_types(
            text,
            &declarations,
            |kind, name| resolve.resolve_symbol(kind, name),
        ) {
            Ok(prepared) => return Ok(Self::Set(prepared)),
            Err(error) => consider(
                error.offset,
                6,
                NativeReadClass::Set,
                QueryError::SetText(error),
            ),
        }
        let (_, _, facade, source) = best.expect("non-temporal parsers always supply a refusal");
        Err(QueryError::Refused {
            facade,
            source: Box::new(source),
        })
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
        let as_of = self.snapshot_seq(db, params)?;
        self.execute_at_seq(db, cx, params, budget, as_of)
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
        let mut bytes = b"fgdb:native-template-identity:v2\0".to_vec();
        let encoded = match self {
            Self::Pattern(prepared) => prepared.template_bytes(),
            Self::Aggregate(prepared) => prepared.canonical_template_bytes(),
            Self::PipelineAggregate(prepared) => prepared.canonical_template_bytes(),
            Self::Set(prepared) => prepared.canonical_template_bytes(),
            Self::TemporalPattern(prepared) => prepared.canonical_template_bytes(),
            Self::TemporalSet(prepared) => prepared.canonical_template_bytes(),
            Self::TemporalAggregate(prepared) => prepared.canonical_template_bytes(),
        };
        bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&encoded);
        bytes
    }
    fn parameter_schema(&self) -> &[GqlParameterSpec] {
        PreparedNativeRead::parameter_schema(self)
    }
}

impl NativeExplainCertificate {
    #[must_use]
    pub fn new(prepared: &PreparedNativeRead, snapshot_seq: CommitSeq) -> Self {
        Self {
            certificate: NativePlanCertificate::new(prepared, snapshot_seq),
        }
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

    /// Versioned certificate bytes binding the template digest and snapshot.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.certificate.canonical_bytes()
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
/// template, never from statement text or an executed snapshot read.
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
    let operators = match prepared {
        PreparedNativeRead::Pattern(plan) => plan.template_operators(),
        PreparedNativeRead::Aggregate(plan) => plan.template_operators(),
        PreparedNativeRead::PipelineAggregate(plan) => plan.template_operators(),
        PreparedNativeRead::Set(plan) => plan.template_operators(),
        PreparedNativeRead::TemporalPattern(plan) => plan.template_operators(),
        PreparedNativeRead::TemporalSet(plan) => plan.template_operators(),
        PreparedNativeRead::TemporalAggregate(plan) => plan.template_operators(),
    };
    for operator in operators {
        rows.push(ExplainRow {
            operator: operator.to_owned(),
            detail: "resolved template".to_owned(),
        });
    }
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
    fn replay_database_identity(&self) -> Digest {
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(b"fgdb:local-replay-authority:v1");
        hasher.update(&self.keys.namespace.0);
        hasher.update(self.path().as_os_str().as_encoded_bytes());
        hasher.finalize()
    }
    /// Execute a native read at one selected snapshot and certify its ordered
    /// result. Temporal selectors name that snapshot, not the live frontier.
    pub fn execute_certified(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        budget: GqlQueryPolicy,
    ) -> Result<(QueryResult, NativeResultCertificate), QueryError> {
        let prepared = PreparedNativeRead::prepare(text, params, resolver)?;
        let snapshot_seq = prepared.snapshot_seq(self, params)?;
        let plan_certificate = NativePlanCertificate::new(&prepared, snapshot_seq);
        let result = prepared.execute_at_seq(self, cx, params, budget, snapshot_seq)?;
        let snapshot_identity = crate::chain_commitment_at(self.coordinator.chain(), snapshot_seq)
            .ok_or_else(|| QueryError::Unsupported {
                diagnostics: vec!["certified history unavailable".to_owned()],
            })?;
        let QueryResult::Rows {
            ref columns,
            ref rows,
        } = result
        else {
            return Err(QueryError::Unsupported {
                diagnostics: vec!["certified execution covers reads only".to_owned()],
            });
        };
        let result_digest =
            crate::gql_cert::native_result_digest(&plan_certificate, params, columns, rows)
                .map_err(|error| QueryError::Unsupported {
                    diagnostics: vec![format!("result digest refused: {error}")],
                })?;
        Ok((
            result,
            NativeResultCertificate {
                plan: plan_certificate,
                values_digest: crate::gql_cert::native_values_digest(params),
                result_digest,
                statement: text.to_owned(),
                facade_class: prepared.facade_class(),
                snapshot_identity,
                database_identity: self.replay_database_identity(),
            },
        ))
    }

    /// Re-execute a certified native read at its certified snapshot and prove
    /// byte-identical results (FG-INV-19 local grade).
    /// Local authority includes namespace and the database path supplied at
    /// open. Reopen with the same path spelling; relocation is not supported.
    /// No archive leases or nondeterministic-operator seeds are claimed.
    ///
    /// The statement re-prepares through the exact native classification; the
    /// fresh plan certificate must match the certified one (statement, class,
    /// parameter types), the bound values must match the certified values
    /// digest, and execution runs through the existing as-of machinery at the
    /// certified seq — never a second evaluator and never the live frontier.
    /// A seq beyond the frontier (or retired) refuses typed.
    pub fn replay(
        &self,
        cx: &QueryCx,
        certificate: &NativeResultCertificate,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        budget: GqlQueryPolicy,
    ) -> Result<QueryResult, ReplayRefusal> {
        if crate::gql_cert::native_values_digest(params) != certificate.values_digest {
            return Err(ReplayRefusal::ParameterValuesMismatch);
        }
        let prepared = PreparedNativeRead::prepare(&certificate.statement, params, resolver)
            .map_err(|error| ReplayRefusal::Execution {
                reason: error.to_string(),
            })?;
        if prepared.facade_class() != certificate.facade_class {
            return Err(ReplayRefusal::FacadeClassMismatch);
        }
        let fresh = NativePlanCertificate::new(&prepared, certificate.plan.snapshot_seq);
        if fresh.digest != certificate.plan.digest {
            return Err(ReplayRefusal::PlanMismatch);
        }
        let frontier = self.frontier().map_err(|_| ReplayRefusal::Snapshot)?;
        if certificate.plan.snapshot_seq > frontier {
            return Err(ReplayRefusal::Snapshot);
        }
        let identity =
            crate::chain_commitment_at(self.coordinator.chain(), certificate.plan.snapshot_seq)
                .ok_or(ReplayRefusal::Snapshot)?;
        if identity != certificate.snapshot_identity
            || self.replay_database_identity() != certificate.database_identity
        {
            return Err(ReplayRefusal::SnapshotIdentityMismatch);
        }
        if matches!(
            prepared,
            PreparedNativeRead::TemporalPattern(_)
                | PreparedNativeRead::TemporalSet(_)
                | PreparedNativeRead::TemporalAggregate(_)
        ) && prepared
            .snapshot_seq(self, params)
            .map_err(|error| ReplayRefusal::Execution {
                reason: error.to_string(),
            })?
            != certificate.plan.snapshot_seq
        {
            return Err(ReplayRefusal::PlanMismatch);
        }
        let result = prepared
            .execute_at_seq(self, cx, params, budget, certificate.plan.snapshot_seq)
            .map_err(|error| ReplayRefusal::Execution {
                reason: error.to_string(),
            })?;
        let QueryResult::Rows {
            ref columns,
            ref rows,
        } = result
        else {
            return Err(ReplayRefusal::Execution {
                reason: "replay covers reads only".to_owned(),
            });
        };
        let digest =
            crate::gql_cert::native_result_digest(&certificate.plan, params, columns, rows)
                .map_err(|error| ReplayRefusal::Execution {
                    reason: format!("result digest refused: {error}"),
                })?;
        if digest != certificate.result_digest {
            return Err(ReplayRefusal::ResultMismatch {
                certified: certificate.result_digest,
                replayed: digest,
            });
        }
        Ok(result)
    }
}

/// Typed replay refusals: every mismatch is explicit, never a panic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayRefusal {
    /// Statement, class or parameter types no longer produce this plan.
    PlanMismatch,
    /// The classification changed between certification and replay.
    FacadeClassMismatch,
    /// Bound parameter values differ from the certified values.
    ParameterValuesMismatch,
    /// The certified seq is beyond the frontier or no longer retained.
    Snapshot,
    /// The database's authoritative history differs at the certified cut.
    SnapshotIdentityMismatch,
    /// Binding or execution failed under the certified plan.
    Execution { reason: String },
    /// The re-executed result bytes differ from the certified digest.
    ResultMismatch { certified: Digest, replayed: Digest },
}
impl core::fmt::Display for ReplayRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PlanMismatch => f.write_str("plan certificate mismatch"),
            Self::FacadeClassMismatch => f.write_str("facade class mismatch"),
            Self::ParameterValuesMismatch => f.write_str("parameter values mismatch"),
            Self::Snapshot => f.write_str("certified snapshot refused or unretained"),
            Self::SnapshotIdentityMismatch => f.write_str("certified history identity mismatch"),
            Self::Execution { reason } => write!(f, "replay execution failed: {reason}"),
            Self::ResultMismatch {
                certified,
                replayed,
            } => write!(
                f,
                "result mismatch: certified {certified:?}, replayed {replayed:?}"
            ),
        }
    }
}
impl core::error::Error for ReplayRefusal {}

impl PreparedNativeRead {
    fn snapshot_seq<V: Vfs + Clone>(
        &self,
        db: &Database<V>,
        params: &GqlParameters,
    ) -> Result<CommitSeq, QueryError> {
        match self {
            Self::TemporalPattern(prepared) => Ok(prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?
                .as_of()),
            Self::TemporalSet(prepared) => Ok(prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)?
                .as_of()),
            Self::TemporalAggregate(prepared) => Ok(prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?
                .as_of()),
            _ => db.frontier().map_err(|error| QueryError::Unsupported {
                diagnostics: vec![error.to_string()],
            }),
        }
    }
    /// Bind and execute through the existing governed as-of engines.
    fn execute_at_seq<V: Vfs + Clone>(
        &self,
        db: &Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        budget: GqlQueryPolicy,
        as_of: CommitSeq,
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
                // Same lane selection as the read-view and transaction hosts:
                // from the admitted definition, never from a failed execution.
                let result = if prepared.requires_relational_input() {
                    let query = prepared
                        .bind_relation_parameters(params)
                        .map_err(QueryError::PipelineText)?;
                    db.execute_graph_set_aggregate_governed_at(cx, &query, as_of, budget)
                        .map_err(QueryError::Aggregate)?
                } else {
                    let query = prepared
                        .bind_parameters(params)
                        .map_err(QueryError::PipelineText)?;
                    db.execute_graph_aggregate_governed_at(cx, &query, as_of, budget)
                        .map_err(QueryError::Aggregate)?
                };
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
                    .execute_graph_aggregate_governed_at(cx, &query, as_of, budget)
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
                    .execute_graph_pattern_governed_at(cx, &query, as_of, budget)
                    .map_err(QueryError::Pattern)?;
                Ok(values(query.columns().to_vec(), result.value))
            }
            Self::Set(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::SetText)?;
                let result = db
                    .execute_graph_set_governed_at(cx, &query, as_of, budget)
                    .map_err(QueryError::Set)?;
                Ok(values(prepared.columns().to_vec(), result.value))
            }
        }
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// EXPLAIN a native read statement without executing it.
    /// Preparation runs the exact native classification used by
    /// [`Database::query`], but binding and execution never happen: no
    /// snapshot record is read and no budget is consumed. Write statements
    /// refuse through the same typed classification error as reads.
    pub fn explain(
        &self,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
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
