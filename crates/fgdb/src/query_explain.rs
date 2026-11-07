//! Shared native read classification and governed execution.

use super::{QueryError, QueryResult, aggregates, values};
use crate::gql_cert::{
    NativeCertificatePlan, NativePlanCertificate, NativeReadClass, NativeResultCertificate,
};
use crate::{Database, EmbeddedReadView};
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
    PipelineAggregate(Box<PreparedGraphPipelineAggregateText>),
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

    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        self.resolver.time_zones()
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

    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        (**self).time_zones()
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

/// One optional trailing `;` terminates a read, as it separates write-script
/// statements, so a statement copied from a script or the README runs as
/// written. Only that final terminator is removed: `;;` or an interior `;`
/// still reaches the grammar and refuses there. No other offset moves.
/// The shared lexer of fgdb-one-lexer-one-dispatch-285i2 is its final home.
pub(crate) fn without_terminator(text: &str) -> &str {
    text.trim_end().strip_suffix(';').unwrap_or(text)
}

/// Catalog binding follows complete native syntax validation. An unrelated
/// grammar's later syntax failure cannot explain that accepted statement.
fn catalog_binding_refusal(error: &QueryError) -> bool {
    use GraphPatternTextErrorKind::Catalog;
    match error {
        QueryError::PatternText(error) => matches!(error.kind, Catalog(_)),
        QueryError::SetText(error) => {
            matches!(error.kind, GraphSetTextErrorKind::Pattern(Catalog(_)))
        }
        QueryError::PipelineText(error) => matches!(
            error.kind,
            GraphPipelineAggregateTextErrorKind::Input(GraphSetTextErrorKind::Pattern(Catalog(_)))
        ),
        QueryError::TemporalText(error) => {
            matches!(error.kind, GraphTemporalTextErrorKind::Query(Catalog(_)))
        }
        QueryError::TemporalSetText(error) => matches!(
            error.kind,
            GraphTemporalSetTextErrorKind::Set(GraphSetTextErrorKind::Pattern(Catalog(_)))
        ),
        _ => false,
    }
}

impl PreparedNativeRead {
    /// Classify using native parsers, accepting a complete ordinary pattern
    /// before probing the temporal, aggregate and composed grammars. Its
    /// grammar cannot consume their required selector, summary or set tail,
    /// so a successful pattern retains the original facade and certificate.
    /// Resolution, including misses, is frozen across all grammar probes.
    /// Catalog binding refusals from a validated grammar take precedence over
    /// unrelated syntax failures. Other refusals select the largest typed byte
    /// offset. Ties prefer temporal
    /// pattern, aggregate, then set when the native selector parser recognizes
    /// a system-time clause; otherwise pattern, aggregate, pipeline, then set.
    /// MissingSystemTimeClause is not a candidate. A failed pattern probe is
    /// retained at its original refusal priority, and selection never parses
    /// diagnostic strings. Other successful probes keep their original order.
    /// A validated UNION schema or quantifier conflict is authoritative: an
    /// unrelated grammar stopping at the same set token cannot replace it.
    pub fn prepare(
        text: &str,
        params: &GqlParameters,
        mut resolver: impl GraphSymbolResolver,
    ) -> Result<Self, QueryError> {
        let text = without_terminator(text);
        let mut resolve = CachingResolver {
            resolver: &mut resolver,
            symbols: BTreeMap::new(),
        };
        // Numeric arguments keep native inference; explicit scalar and list
        // declarations reach every prepare_with_parameter_types facade.
        let declarations: Vec<(&str, GqlParameterType)> = params
            .parameter_types()
            .filter(|(_, kind)| {
                matches!(
                    kind,
                    GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
                )
            })
            .collect();
        // The ordinary pattern grammar validates its complete syntax before
        // catalog resolution. Both aggregate grammars require a real summary,
        // even when HAVING or ORDER BY supplies it, and temporal grammars need
        // a selector that this grammar cannot consume. A successful pattern
        // thus skips five unsuccessful grammar probes without changing its
        // facade or certificate. Keep its exact error for the original refusal
        // slot below: neither syntax nor catalog misses are retried.
        let pattern_error = match PreparedGraphText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::Pattern(prepared)),
            Err(error) => error,
        };
        let mut best = None;
        let mut consider = |offset, priority, facade, source: QueryError| {
            let rank = (
                catalog_binding_refusal(&source),
                offset,
                std::cmp::Reverse(priority),
            );
            if best
                .as_ref()
                .is_none_or(|(old_rank, _, _)| rank > *old_rank)
            {
                best = Some((rank, facade, source));
            }
        };
        match PreparedTemporalGraphAggregateText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
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
        match PreparedTemporalGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::TemporalSet(prepared)),
            Err(error)
                if matches!(
                    error.kind,
                    GraphTemporalSetTextErrorKind::Set(
                        GraphSetTextErrorKind::DifferentColumnsInUnion
                            | GraphSetTextErrorKind::InvalidClauseComposition
                    )
                ) =>
            {
                return Err(QueryError::Refused {
                    facade: NativeReadClass::TemporalSet,
                    source: Box::new(QueryError::TemporalSetText(error)),
                });
            }
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
        match PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::PipelineAggregate(Box::new(prepared))),
            Err(error) => consider(
                error.offset,
                5,
                NativeReadClass::PipelineAggregate,
                QueryError::PipelineText(error),
            ),
        }
        match PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::Aggregate(prepared)),
            Err(error) => consider(
                error.offset,
                4,
                NativeReadClass::Aggregate,
                QueryError::PatternText(error),
            ),
        }
        consider(
            pattern_error.offset,
            3,
            NativeReadClass::Pattern,
            QueryError::PatternText(pattern_error),
        );
        match PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(Self::Set(prepared)),
            Err(error)
                if matches!(
                    error.kind,
                    GraphSetTextErrorKind::DifferentColumnsInUnion
                        | GraphSetTextErrorKind::InvalidClauseComposition
                ) =>
            {
                return Err(QueryError::Refused {
                    facade: NativeReadClass::Set,
                    source: Box::new(QueryError::SetText(error)),
                });
            }
            Err(error) => consider(
                error.offset,
                6,
                NativeReadClass::Set,
                QueryError::SetText(error),
            ),
        }
        let (_, facade, source) = best.expect("non-temporal parsers always supply a refusal");
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
        let captured = db.capture_statement_parameters(cx, self.statement(), params)?;
        let params = captured.as_ref();
        let as_of = self.snapshot_seq(db, params)?;
        self.execute_at_seq(db, cx, params, budget, as_of, &mut Vec::new())
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
    /// The replay authority is the security namespace. Together with the
    /// certified snapshot's chain commitment it names the evidence closure
    /// (FG-INV-19). The directory spelling is not part of it: v1 hashed the
    /// open path, so reopening via another spelling, moving or copying a
    /// database refused every certificate it had issued
    /// (fgdb-replay-path-identity-p3lsc).
    fn replay_database_identity(&self) -> Digest {
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(b"fgdb:local-replay-authority:v2");
        hasher.update(&self.keys.namespace.0);
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
        let captured = self.capture_statement_parameters(cx, text, params)?;
        let params = captured.as_ref();
        // A record field keying a pattern is the constant it holds
        // (fgdb-8srlm). The certificate keeps the caller's statement and
        // values; replay lowers them again, to the same plan.
        let lowered = GqlMapArgumentText::lower_fields(text, params);
        let (statement, bound) = lowered
            .as_ref()
            .map_or((text, params), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let prepared = PreparedNativeRead::prepare(statement, bound, resolver)?;
        let snapshot_seq = prepared.snapshot_seq(self, bound)?;
        let plan_certificate = NativePlanCertificate::new(&prepared, snapshot_seq);
        let mut evidence = Vec::new();
        let result =
            prepared.execute_at_seq(self, cx, bound, budget, snapshot_seq, &mut evidence)?;
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
                statement_time: params.statement_time().cloned(),
                procedures: evidence
                    .iter()
                    .map(crate::gql_cert::NativeProcedureEvidence::from_fnx)
                    .collect(),
            },
        ))
    }

    /// Re-execute a certified native read at its certified snapshot and prove
    /// byte-identical results (FG-INV-19 local grade).
    /// The authority is the security namespace plus the chain commitment at
    /// the certified seq, so a reopened, moved or copied database with that
    /// history replays; a different history or namespace refuses.
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
        // Statement time is retained execution input. A replay with ordinary
        // caller arguments restores it from the certificate; an explicit
        // conflicting context is refused before any graph observation.
        let captured = match (&certificate.statement_time, params.statement_time()) {
            (Some(certified), Some(supplied)) if certified != supplied => {
                return Err(ReplayRefusal::ParameterValuesMismatch);
            }
            (None, Some(_)) => return Err(ReplayRefusal::ParameterValuesMismatch),
            (Some(certified), None) => std::borrow::Cow::Owned(
                params
                    .clone()
                    .with_statement_time(certified.clone())
                    .map_err(|error| ReplayRefusal::Execution {
                        reason: format!("certified statement time refused: {error}"),
                    })?,
            ),
            _ => std::borrow::Cow::Borrowed(params),
        };
        let params = captured.as_ref();
        if crate::gql_cert::native_values_digest(params) != certificate.values_digest {
            return Err(ReplayRefusal::ParameterValuesMismatch);
        }
        // The certified statement lowers as execute_certified lowered it.
        let lowered = GqlMapArgumentText::lower_fields(&certificate.statement, params);
        let (statement, bound) = lowered.as_ref().map_or(
            (certificate.statement.as_str(), params),
            |(lowered, derived)| (lowered.text(), derived),
        );
        let prepared =
            PreparedNativeRead::prepare(statement, bound, resolver).map_err(|error| {
                ReplayRefusal::Execution {
                    reason: error.to_string(),
                }
            })?;
        if certificate.statement_time.is_none()
            && prepared
                .parameter_schema()
                .iter()
                .any(GqlParameterSpec::is_statement_time)
        {
            return Err(ReplayRefusal::ParameterValuesMismatch);
        }
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
            .snapshot_seq(self, bound)
            .map_err(|error| ReplayRefusal::Execution {
                reason: error.to_string(),
            })?
            != certificate.plan.snapshot_seq
        {
            return Err(ReplayRefusal::PlanMismatch);
        }
        let mut evidence = Vec::new();
        let result = prepared
            .execute_at_seq(
                self,
                cx,
                bound,
                budget,
                certificate.plan.snapshot_seq,
                &mut evidence,
            )
            .map_err(|error| ReplayRefusal::Execution {
                reason: error.to_string(),
            })?;
        // Procedure evidence first: a different kernel is refused even when
        // it happens to produce the certified rows (fgdb-3b1v7).
        let replayed: Vec<_> = evidence
            .iter()
            .map(crate::gql_cert::NativeProcedureEvidence::from_fnx)
            .collect();
        for index in 0..certificate.procedures.len().max(replayed.len()) {
            match (certificate.procedures.get(index), replayed.get(index)) {
                (Some(certified), Some(fresh)) if certified == fresh => {}
                (Some(certified), Some(fresh))
                    if certified.call_digest == fresh.call_digest
                        && certified.kernel_digest != fresh.kernel_digest =>
                {
                    return Err(ReplayRefusal::ProcedureKernelMismatch { index });
                }
                _ => return Err(ReplayRefusal::ProcedureMismatch { index }),
            }
        }
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
    /// The procedure at `index` ran under a different kernel, registry or
    /// numeric profile than certified (fgdb-3b1v7).
    ProcedureKernelMismatch { index: usize },
    /// The procedure at `index` is missing, extra, a different call, or
    /// produced a different Prism certificate (projection, result, witness).
    ProcedureMismatch { index: usize },
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
            Self::ProcedureKernelMismatch { index } => {
                write!(f, "procedure {index} kernel differs from the certified one")
            }
            Self::ProcedureMismatch { index } => {
                write!(
                    f,
                    "procedure {index} evidence differs from the certified one"
                )
            }
        }
    }
}
impl core::error::Error for ReplayRefusal {}

impl PreparedNativeRead {
    /// Resolve the snapshot selected by this template's bound parameters.
    ///
    /// Temporal templates return their exact AS OF sequence; other read
    /// classes use the caller's already admitted generation. This binds the
    /// existing native template without reparsing text, resolving symbols or
    /// acquiring a new snapshot. Branch-only arguments must first be consumed
    /// by the branch selector, just as they are before execution.
    ///
    /// The returned sequence is request metadata, not proof of authorization
    /// or retained history. Execution still validates both before returning
    /// rows. A server must report it only together with successful execution.
    pub fn bound_snapshot_seq(
        &self,
        params: &GqlParameters,
        default: CommitSeq,
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
                .bind_source_parameters(params)
                .map_err(QueryError::TemporalText)?
                .as_of()),
            _ => Ok(default),
        }
    }

    fn snapshot_seq<V: Vfs + Clone>(
        &self,
        db: &Database<V>,
        params: &GqlParameters,
    ) -> Result<CommitSeq, QueryError> {
        match self {
            // Temporal binding does not consult the default frontier.
            Self::TemporalPattern(_) | Self::TemporalSet(_) | Self::TemporalAggregate(_) => {
                self.bound_snapshot_seq(params, CommitSeq(0))
            }
            _ => db.frontier().map_err(|error| QueryError::Unsupported {
                diagnostics: vec![error.to_string()],
            }),
        }
    }
    /// Bind and execute through the existing governed as-of engines.
    /// `evidence` receives each executed procedure's Prism certificate, in
    /// execution order; only set and relational-aggregate reads host CALL.
    fn execute_at_seq<V: Vfs + Clone>(
        &self,
        db: &Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        budget: GqlQueryPolicy,
        as_of: CommitSeq,
        evidence: &mut Vec<fgdb_prism::FnxCertificate>,
    ) -> Result<QueryResult, QueryError> {
        match self {
            Self::TemporalAggregate(prepared) => {
                let query = prepared
                    .bind_source_parameters(params)
                    .map_err(QueryError::TemporalText)?;
                let result = match query.aggregate() {
                    TemporalAggregatePlan::Graph(aggregate) => {
                        db.execute_graph_aggregate_governed_at(cx, aggregate, query.as_of(), budget)
                    }
                    TemporalAggregatePlan::Relation(aggregate) => db
                        .execute_graph_set_aggregate_evidenced_at(
                            cx,
                            aggregate,
                            query.as_of(),
                            budget,
                            evidence,
                        ),
                }
                .map_err(QueryError::Aggregate)?;
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
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
                    .execute_graph_set_evidenced_at(
                        cx,
                        query.query(),
                        query.as_of(),
                        budget,
                        evidence,
                    )
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
                    db.execute_graph_set_aggregate_evidenced_at(cx, &query, as_of, budget, evidence)
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
                    .execute_graph_set_evidenced_at(cx, &query, as_of, budget, evidence)
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
        explain_at(text, params, resolver, certificate, || {
            // EXPLAIN is a successful top-level call: the live frontier read
            // has no failure mode left here beyond the plain read guard.
            self.frontier().map_err(|_| QueryError::Unsupported {
                diagnostics: vec!["snapshot frontier unavailable for certificate".to_owned()],
            })
        })
    }
}

impl EmbeddedReadView {
    /// EXPLAIN a native read statement against this view, exactly as
    /// [`Database::explain`] does against the live handle: the same
    /// preparation and listing, and a certificate bound to this view's
    /// frontier. Nothing is bound or executed.
    pub fn explain(
        &self,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        certificate: bool,
    ) -> Result<(Vec<ExplainRow>, Option<NativeExplainCertificate>), QueryError> {
        explain_at(text, params, resolver, certificate, || Ok(self.frontier()))
    }
}

/// EXPLAIN preparation shared by a database and its views. `snapshot_seq` is
/// read only when a certificate is asked for.
fn explain_at(
    text: &str,
    params: &GqlParameters,
    resolver: impl GraphSymbolResolver,
    certificate: bool,
    snapshot_seq: impl FnOnce() -> Result<CommitSeq, QueryError>,
) -> Result<(Vec<ExplainRow>, Option<NativeExplainCertificate>), QueryError> {
    let prepared = PreparedNativeRead::prepare(text, params, resolver)?;
    let rows = explain_rows(&prepared);
    let cert = if certificate {
        Some(NativeExplainCertificate::new(&prepared, snapshot_seq()?))
    } else {
        None
    };
    Ok((rows, cert))
}

/// `Some((statement, certificate))` when `text` is `EXPLAIN [(CERTIFICATE)]
/// <statement>`, a refusal for a malformed option, and `None` for any other
/// text. Shared by the database's and the view's `query`, so both classify and
/// refuse the prefix identically.
pub(super) fn explain_prefix(text: &str) -> Option<Result<(&str, bool), QueryError>> {
    let trimmed = text.trim_start();
    let explain = trimmed
        .get(..7)
        .is_some_and(|word| word.eq_ignore_ascii_case("EXPLAIN"))
        && trimmed
            .as_bytes()
            .get(7)
            .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'(');
    if !explain {
        return None;
    }
    let statement = trimmed[7..].trim_start();
    let Some(options) = statement.strip_prefix('(') else {
        return Some(Ok((statement, false)));
    };
    let Some((option, rest)) = options.split_once(')') else {
        return Some(Err(QueryError::Unsupported {
            diagnostics: vec!["unclosed EXPLAIN option".to_owned()],
        }));
    };
    if !option.trim().eq_ignore_ascii_case("CERTIFICATE") {
        return Some(Err(QueryError::Unsupported {
            diagnostics: vec!["expected EXPLAIN (CERTIFICATE)".to_owned()],
        }));
    }
    Some(Ok((rest.trim_start(), true)))
}

/// The EXPLAIN answer as `operator`/`detail` rows: the listing, then the
/// certificate's digest and snapshot when one was asked for.
pub(super) fn explain_result(
    listing: Vec<ExplainRow>,
    certificate: Option<NativeExplainCertificate>,
) -> Result<QueryResult, QueryError> {
    let mut rows = Vec::with_capacity(listing.len() + usize::from(certificate.is_some()) * 2);
    let cell = |text: &str| {
        fgdb_types::CanonicalScalar::ucs_basic_text(text)
            .map(|value| GraphAggregateValue::Value(fgdb_gql::algebra::GraphValue::Scalar(value)))
            .map_err(|_| QueryError::Unsupported {
                diagnostics: vec!["EXPLAIN text exceeds canonical scalar bounds".to_owned()],
            })
    };
    for row in listing {
        rows.push(vec![cell(&row.operator)?, cell(&row.detail)?]);
    }
    if let Some(certificate) = certificate {
        rows.push(vec![
            cell("Certificate")?,
            cell(&format!("{:?}", certificate.digest()))?,
        ]);
        rows.push(vec![
            cell("Snapshot")?,
            cell(&certificate.snapshot_seq().0.to_string())?,
        ]);
    }
    Ok(QueryResult::Rows {
        columns: vec!["operator".to_owned(), "detail".to_owned()],
        rows,
    })
}

#[cfg(test)]
mod temporal_aggregate_resolver_tests {
    use super::*;
    use crate::{PinnedTzdb, TzdbZone};
    use std::sync::Arc;

    struct ZonedResolver(GqlTimeZones);

    impl GraphSymbolResolver for ZonedResolver {
        fn resolve_symbol(&mut self, _kind: GraphSymbolKind, _name: &str) -> Option<GraphSymbol> {
            None
        }

        fn time_zones(&self) -> Option<GqlTimeZones> {
            Some(self.0.clone())
        }
    }

    fn zones(offset: i32) -> GqlTimeZones {
        let artifact = Arc::new(
            PinnedTzdb::new(vec![TzdbZone {
                identifier: "Test/Fixed".to_owned(),
                initial_offset_seconds: offset,
                transitions: vec![],
            }])
            .unwrap(),
        );
        let id = artifact.object_id();
        GqlTimeZones::new(artifact, id).unwrap()
    }

    const QUERIES: [&str; 2] = [
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN MAX(datetime({year:2020,month:1,day:1,timezone:'Test/Fixed'})) AS stamp",
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WITH datetime({year:2020,month:1,day:1,timezone:'Test/Fixed'}) AS stamp RETURN MAX(stamp) AS latest",
    ];

    #[test]
    fn historical_aggregate_dispatch_pins_tzdb_in_plan_certificates() {
        let params = GqlParameters::new().with_uint64("at", 7).unwrap();
        for text in QUERIES {
            let first =
                PreparedNativeRead::prepare(text, &params, ZonedResolver(zones(3600))).unwrap();
            let same =
                PreparedNativeRead::prepare(text, &params, ZonedResolver(zones(3600))).unwrap();
            let changed =
                PreparedNativeRead::prepare(text, &params, ZonedResolver(zones(7200))).unwrap();
            assert_eq!(first.facade_class(), NativeReadClass::TemporalAggregate);
            assert_eq!(
                first.bound_snapshot_seq(&params, CommitSeq(99)).unwrap(),
                CommitSeq(7)
            );
            let certificate = NativeExplainCertificate::new(&first, CommitSeq(7));
            assert!(certificate.verifies_at(&same, CommitSeq(7)));
            assert!(!certificate.verifies_at(&changed, CommitSeq(7)));
            assert!(!certificate.verifies_at(&same, CommitSeq(8)));
            assert_ne!(
                certificate.canonical_bytes(),
                NativeExplainCertificate::new(&changed, CommitSeq(7)).canonical_bytes()
            );
        }
    }

    #[test]
    fn explain_temporal_aggregates_retains_artifact_and_snapshot_identity() {
        let params = GqlParameters::new().with_uint64("at", 7).unwrap();
        for text in QUERIES {
            let (rows, certificate) =
                explain_at(text, &params, ZonedResolver(zones(3600)), true, || {
                    Ok(CommitSeq(7))
                })
                .unwrap();
            assert!(
                rows.iter()
                    .any(|row| row.operator == "TemporalSnapshotSelect")
            );
            let prepared =
                PreparedNativeRead::prepare(text, &params, ZonedResolver(zones(3600))).unwrap();
            assert!(certificate.unwrap().verifies_at(&prepared, CommitSeq(7)));
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use std::cell::{Cell, RefCell};

    // The pre-change dispatcher is retained as the test oracle. Its grammar
    // order, typed refusal filtering and UNION authority rules are independent
    // of the common-pattern-first path under test.
    fn legacy(
        text: &str,
        params: &GqlParameters,
        mut resolver: impl GraphSymbolResolver,
    ) -> Result<PreparedNativeRead, QueryError> {
        let text = without_terminator(text);
        let mut resolve = CachingResolver {
            resolver: &mut resolver,
            symbols: BTreeMap::new(),
        };
        // Numeric arguments keep native inference; explicit scalar and list
        // declarations reach every prepare_with_parameter_types facade.
        let declarations: Vec<(&str, GqlParameterType)> = params
            .parameter_types()
            .filter(|(_, kind)| {
                matches!(
                    kind,
                    GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
                )
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
        match PreparedTemporalGraphAggregateText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(PreparedNativeRead::TemporalAggregate(prepared)),
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
            Ok(prepared) => return Ok(PreparedNativeRead::TemporalPattern(prepared)),
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
        match PreparedTemporalGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(PreparedNativeRead::TemporalSet(prepared)),
            Err(error)
                if matches!(
                    error.kind,
                    GraphTemporalSetTextErrorKind::Set(
                        GraphSetTextErrorKind::DifferentColumnsInUnion
                            | GraphSetTextErrorKind::InvalidClauseComposition
                    )
                ) =>
            {
                return Err(QueryError::Refused {
                    facade: NativeReadClass::TemporalSet,
                    source: Box::new(QueryError::TemporalSetText(error)),
                });
            }
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
        match PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(PreparedNativeRead::PipelineAggregate(Box::new(prepared))),
            Err(error) => consider(
                error.offset,
                5,
                NativeReadClass::PipelineAggregate,
                QueryError::PipelineText(error),
            ),
        }
        match PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(PreparedNativeRead::Aggregate(prepared)),
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
            Ok(prepared) => return Ok(PreparedNativeRead::Pattern(prepared)),
            Err(error) => consider(
                error.offset,
                3,
                NativeReadClass::Pattern,
                QueryError::PatternText(error),
            ),
        }
        match PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &declarations,
            &mut resolve,
        ) {
            Ok(prepared) => return Ok(PreparedNativeRead::Set(prepared)),
            Err(error)
                if matches!(
                    error.kind,
                    GraphSetTextErrorKind::DifferentColumnsInUnion
                        | GraphSetTextErrorKind::InvalidClauseComposition
                ) =>
            {
                return Err(QueryError::Refused {
                    facade: NativeReadClass::Set,
                    source: Box::new(QueryError::SetText(error)),
                });
            }
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

    #[derive(Clone, Copy)]
    enum Catalog {
        Present,
        Missing,
        WrongKind,
    }

    struct Resolver<'a> {
        catalog: Catalog,
        calls: &'a RefCell<Vec<(GraphSymbolKind, String)>>,
        zone_reads: &'a Cell<usize>,
    }

    impl GraphSymbolResolver for Resolver<'_> {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            self.calls.borrow_mut().push((kind, name.to_owned()));
            match self.catalog {
                Catalog::Present => symbols(kind, name),
                Catalog::Missing => None,
                Catalog::WrongKind => Some(match kind {
                    GraphSymbolKind::Label | GraphSymbolKind::Relation => {
                        GraphSymbol::Property(PropertyKeyId(1))
                    }
                    GraphSymbolKind::Property => GraphSymbol::Label(LabelId(1)),
                }),
            }
        }

        fn time_zones(&self) -> Option<GqlTimeZones> {
            self.zone_reads.set(self.zone_reads.get() + 1);
            None
        }
    }

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Prepared(NativeReadClass, Vec<u8>),
        Refused(String, String),
    }

    fn outcome(result: Result<PreparedNativeRead, QueryError>) -> Outcome {
        match result {
            Ok(prepared) => Outcome::Prepared(
                prepared.facade_class(),
                NativeExplainCertificate::new(&prepared, CommitSeq(7)).canonical_bytes(),
            ),
            Err(error) => Outcome::Refused(format!("{error:?}"), error.to_string()),
        }
    }

    fn compare(text: &str, params: &GqlParameters, catalog: Catalog) -> (Outcome, usize, usize) {
        let (calls, old_calls) = (RefCell::new(Vec::new()), RefCell::new(Vec::new()));
        let (zone_reads, old_zone_reads) = (Cell::new(0), Cell::new(0));
        let current = outcome(PreparedNativeRead::prepare(
            text,
            params,
            Resolver {
                catalog,
                calls: &calls,
                zone_reads: &zone_reads,
            },
        ));
        let original = outcome(legacy(
            text,
            params,
            Resolver {
                catalog,
                calls: &old_calls,
                zone_reads: &old_zone_reads,
            },
        ));
        assert_eq!(current, original, "{text}");
        assert_eq!(
            *calls.borrow(),
            *old_calls.borrow(),
            "{text}: catalog order"
        );
        let calls = calls.into_inner();
        let unique: std::collections::BTreeSet<_> = calls.iter().collect();
        assert_eq!(calls.len(), unique.len(), "{text}: retried catalog lookup");
        (current, zone_reads.get(), old_zone_reads.get())
    }

    const PATTERNS: &[&str] = &[
        "MATCH (n) RETURN n",
        "MATCH (n:L) RETURN DISTINCT n.p AS p ORDER BY p DESC SKIP 1 LIMIT 2",
        "MATCH (a:L)-[r:R]->(b:L) RETURN a.p AS p, b.p AS q",
        "MATCH (n:L {p: 'count(*) FOR SYSTEM_TIME UNION'}) RETURN n",
        "/* FOR SYSTEM_TIME */ MATCH (n:L) // count(*)\n RETURN n",
        "MATCH (n:L) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
        "MATCH (n:L) OPTIONAL MATCH (n)-[:R]->(m) RETURN n, m",
        "MATCH p = (a:L)-[:R*0..2]->(b) RETURN p",
        "MATCH (n:L) RETURN [(n)-[r:R]->(m) | r] AS rows",
    ];

    #[test]
    fn complete_patterns_are_disjoint_from_earlier_successful_facades() {
        let params = GqlParameters::new();
        for &text in PATTERNS {
            assert!(
                PreparedGraphText::prepare_with_parameter_types_and_resolver(text, &[], symbols)
                    .is_ok(),
                "{text}: expected a complete native pattern"
            );
            assert_eq!(
                [
                    PreparedTemporalGraphAggregateText::prepare_with_parameter_types_and_resolver(
                        text,
                        &[],
                        symbols,
                    )
                    .is_ok(),
                    PreparedTemporalGraphText::prepare_with_parameter_types_and_resolver(
                        text,
                        &[],
                        symbols,
                    )
                    .is_ok(),
                    PreparedTemporalGraphSetText::prepare_with_parameter_types_and_resolver(
                        text,
                        &[],
                        &mut symbols,
                    )
                    .is_ok(),
                    PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
                        text,
                        &[],
                        symbols,
                    )
                    .is_ok(),
                    PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
                        text,
                        &[],
                        symbols,
                    )
                    .is_ok(),
                ],
                [false; 5],
                "{text}: an earlier facade would change precedence"
            );
            let (result, current, original) = compare(text, &params, Catalog::Present);
            assert!(matches!(
                result,
                Outcome::Prepared(NativeReadClass::Pattern, _)
            ));
            // Each real grammar attempt asks for its parsing environment.
            // Ordinary success now reaches exactly one native parser.
            assert_eq!(current, 1, "{text}: additional grammar attempt");
            assert!(
                original >= 3,
                "{text}: oracle did not exercise the old probes"
            );
        }
    }

    #[test]
    fn every_native_family_keeps_its_facade_and_template_certificate() {
        let params = GqlParameters::new();
        for (text, class) in [
            (
                "MATCH (n) RETURN n.p AS p ORDER BY p",
                NativeReadClass::Pattern,
            ),
            (
                "MATCH (n) RETURN COUNT(*) AS total, SUM(n.p) AS amount",
                NativeReadClass::Aggregate,
            ),
            (
                "MATCH (n) WITH n.p AS p RETURN COUNT(*) AS total, SUM(p) AS amount",
                NativeReadClass::PipelineAggregate,
            ),
            (
                "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
                NativeReadClass::Set,
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p ORDER BY p",
                NativeReadClass::TemporalPattern,
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS total, SUM(n.p) AS amount",
                NativeReadClass::TemporalAggregate,
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
                NativeReadClass::TemporalSet,
            ),
        ] {
            let (result, _, _) = compare(text, &params, Catalog::Present);
            assert!(
                matches!(result, Outcome::Prepared(found, _) if found == class),
                "{text}"
            );
        }
    }

    #[test]
    fn hidden_summaries_cannot_be_misclassified_as_plain_patterns() {
        let params = GqlParameters::new();
        for text in [
            "MATCH (n) RETURN n GROUP BY n HAVING COUNT(*) > 0",
            "MATCH (n) RETURN n GROUP BY n ORDER BY COUNT(*) DESC",
            "MATCH (n) RETURN n HAVING COUNT(*) > 0",
            "MATCH (n) RETURN n ORDER BY COUNT(*) DESC",
            "MATCH (n) WITH n AS n RETURN n HAVING COUNT(*) > 0",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n GROUP BY n HAVING COUNT(*) > 0",
        ] {
            assert!(
                PreparedGraphText::prepare_with_parameter_types_and_resolver(text, &[], symbols)
                    .is_err(),
                "{text}: pattern parser consumed a hidden summary"
            );
            let (result, _, _) = compare(text, &params, Catalog::Present);
            assert!(!matches!(
                result,
                Outcome::Prepared(NativeReadClass::Pattern, _)
            ));
        }
    }

    #[test]
    fn refusals_keep_offsets_ties_union_authority_and_catalog_misses() {
        let params = GqlParameters::new();
        for text in [
            "",
            "MATCH",
            "MATCH (n:L) RETURN",
            "MATCH (n:L) RETURN n, n",
            "MATCH (n:L) RETURN n HAVING",
            "MATCH (n:L {p: 'unterminated}) RETURN n",
            "MATCH (n:L) RETURN n;;",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ RETURN n",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN",
            "MATCH (n:L) RETURN n AS x UNION MATCH (m:L) RETURN m AS y",
            "MATCH (n:L) RETURN n AS x UNION ALL MATCH (m:L) RETURN m AS x UNION MATCH (k:L) RETURN k AS x",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n AS x UNION MATCH (m:L) RETURN m AS y",
        ] {
            let (result, _, _) = compare(text, &params, Catalog::Present);
            assert!(matches!(result, Outcome::Refused(_, _)), "{text}");
        }
        for catalog in [Catalog::Missing, Catalog::WrongKind] {
            for &text in PATTERNS.iter().filter(|text| text.contains(":L")) {
                let (result, _, _) = compare(text, &params, catalog);
                assert!(matches!(result, Outcome::Refused(_, _)), "{text}");
            }
        }
    }

    #[test]
    fn parameter_declarations_and_statement_terminators_keep_native_dispatch() {
        let params = GqlParameters::new()
            .with_scalar("value", fgdb_types::CanonicalScalar::Bool(true))
            .unwrap();
        for text in [
            "MATCH (n:L {p: $value}) RETURN n.p AS p",
            "MATCH (n:L {p: $value}) RETURN n.p AS p; \n",
            "MATCH (n:L {p: $value}) RETURN COUNT(*) AS total",
            "MATCH (n:L {p: $value}) WITH n.p AS p RETURN COUNT(*) AS total",
            "MATCH (n:L {p: $value}) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
            "MATCH (n:L) RETURN n",
        ] {
            compare(text, &params, Catalog::Present);
        }
    }
}
