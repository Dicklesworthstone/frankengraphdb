//! `query 'CALL fnx.<procedure>(...) YIELD ...'`: registered Prism analytics
//! over the admitted generation. The default uses the decoded adapter on a
//! read-only open; --buffered faults the source through authenticated checkpoint
//! cursors; --sealed executes compressed cursors over the selected relation.
//! Combining them keeps source payloads in the extent pool. --sealed-quantum
//! additionally yields between bounded steps of a supported compressed kernel.
//!
//! A standalone procedure sees an EXPLICIT projection chosen by flags, never
//! an implicit collapse of the stored multigraph:
//! - `--graph-label` / `--graph-relation` select the induced vertices and edges
//!   (default: every vertex, every relation);
//! - `--weight <property>` reads edge weights, with `--missing-weight` choosing
//!   reject, unit or zero for an absent value (default: unit weights, no read);
//! - `--direction directed|reversed|undirected` (default directed),
//!   `--parallel-edges reject|collapse|min|max|sum` (default reject) and
//!   `--self-loops keep|drop|reject` (default keep) choose the graph laws;
//! - `--as-of <seq>` pins a committed sequence (time-travel analytics).
//!
//! A procedure whose graph laws do not match the projection (for example
//! `fnx.connected_components` over a directed projection) is refused by Prism,
//! never silently converted. Arguments are typed `--param` values:
//! `int:`, `float:`, `bool:`, `null` and `vertex:<id>` for a source vertex.
//! Composed native reads use their query-local projections and native parameter
//! types; --sealed selects a compressed relation without overriding either.
use super::{Failure, Options, decimal_usize, float_text, render_rows_iter};
use asupersync::fs::Vfs;
use fgdb::{
    BufferedReadView, Database, DatabaseKeys, EmbeddedReadView, PreparedNativeRead, QueryError,
    SealedProjectionOpenError, SealedQueryError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxArgument, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadError,
    FnxReadOptions, FnxReadResult, FnxRowCursor, FnxSealedExecutionError, FnxSelection, FnxValue,
    FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy, ProjectionSpec, SelfLoopPolicy,
};
use fgdb_strata::{store::RootReadLimits, tiered::sealed::SealedLimits};
use fgdb_types::{CommitCx, CommitSeq, QueryCx, VId};
use std::io::Write;
use std::num::NonZeroUsize;

/// Is this `query` text a standalone Prism call? `CALL` is a keyword (any
/// case); the procedure namespace `fnx.` is case-sensitive, like the
/// registry. A call that continues past its YIELD list (WHERE, WITH, MATCH,
/// RETURN, ...) or supplies a native projection is an ordinary native read.
/// Kernel-only named arguments use the same standalone route as positional
/// arguments, regardless of whether the subsequent typed binding succeeds.
pub(super) fn is_call(text: &str) -> bool {
    if fgdb_gql::starts_fnx_graph_projection(text) {
        // The shared lexer also recognizes every named argument envelope.
        // The registry's parser distinguishes kernel names from projection
        // fields/maps and pipelines without substituting parameter values.
        // A bad value must reach the same pre-open binder as a good one.
        return FnxCallSpec::is_standalone_kernel_call(text);
    }
    let text = text.trim_start();
    text.get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("CALL"))
        && text[4..].starts_with(char::is_whitespace)
        && text[4..].trim_start().starts_with("fnx.")
        && !fgdb_prism::FnxCallSpec::continues_past_call(text)
}

/// Projection flags as given; symbol names resolve against the bindings once
/// every flag is parsed.
#[derive(Default)]
pub(super) struct ProjectionFlags {
    label: Option<String>,
    relation: Option<String>,
    weight: Option<String>,
    missing: Option<MissingWeightPolicy>,
    direction: Option<Directedness>,
    parallel: Option<ParallelEdgePolicy>,
    self_loops: Option<SelfLoopPolicy>,
    as_of: Option<CommitSeq>,
    graph_staging_bytes: Option<usize>,
    graph_workspace_bytes: Option<usize>,
    sealed: bool,
    sealed_kernel_bytes: Option<usize>,
    sealed_result_bytes: Option<usize>,
    sealed_image_bytes: Option<usize>,
    sealed_property_bytes: Option<usize>,
    sealed_rows: Option<usize>,
    sealed_incidences: Option<usize>,
    sealed_source_incidences: Option<usize>,
    sealed_quantum: Option<NonZeroUsize>,
}

impl ProjectionFlags {
    pub(super) const FLAGS: [&str; 18] = [
        "--graph-label",
        "--graph-relation",
        "--weight",
        "--missing-weight",
        "--direction",
        "--parallel-edges",
        "--self-loops",
        "--as-of",
        "--graph-staging-bytes",
        "--graph-workspace-bytes",
        "--sealed-kernel-bytes",
        "--sealed-result-bytes",
        "--sealed-image-bytes",
        "--sealed-property-bytes",
        "--sealed-max-rows",
        "--sealed-max-incidences",
        "--sealed-source-incidences",
        "--sealed-quantum",
    ];

    pub(super) fn is_empty(&self) -> bool {
        self.label.is_none()
            && self.relation.is_none()
            && self.weight.is_none()
            && self.missing.is_none()
            && self.direction.is_none()
            && self.parallel.is_none()
            && self.self_loops.is_none()
            && self.as_of.is_none()
            && self.graph_staging_bytes.is_none()
            && self.graph_workspace_bytes.is_none()
            && !self.sealed
            && !self.has_sealed_limits()
    }

    pub(super) fn enable_sealed(&mut self) -> Result<(), Failure> {
        if self.sealed {
            return Err(Failure::usage("--sealed is allowed once"));
        }
        self.sealed = true;
        Ok(())
    }

    pub(super) fn uses_sealed_quantum(&self) -> bool {
        self.sealed && self.sealed_quantum.is_some()
    }

    pub(super) fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// The selected compressed relation is an admission boundary. Query-local
    /// projections still determine each CALL's graph laws; flags cannot replace
    /// those definitions or silently change an ordinary native read.
    pub(super) fn validate_composed(&self) -> Result<(), Failure> {
        if !self.sealed {
            return Err(Failure::usage(
                "projection flags on a composed CALL require --sealed; otherwise use a query-local projection map",
            ));
        }
        if self.relation.is_none() {
            return Err(Failure::usage(
                "composed --sealed queries require --graph-relation and the same relation in each CALL's query-local projection",
            ));
        }
        if self.label.is_some()
            || self.weight.is_some()
            || self.missing.is_some()
            || self.direction.is_some()
            || self.parallel.is_some()
            || self.self_loops.is_some()
            || self.graph_staging_bytes.is_some()
            || self.graph_workspace_bytes.is_some()
            || self.sealed_quantum.is_some()
        {
            return Err(Failure::usage(
                "composed --sealed queries use query-local labels, weights and graph laws; --graph-staging-bytes, --graph-workspace-bytes and --sealed-quantum are unsupported",
            ));
        }
        Ok(())
    }

    fn has_sealed_limits(&self) -> bool {
        self.sealed_kernel_bytes.is_some()
            || self.sealed_result_bytes.is_some()
            || self.sealed_image_bytes.is_some()
            || self.sealed_property_bytes.is_some()
            || self.sealed_rows.is_some()
            || self.sealed_incidences.is_some()
            || self.sealed_source_incidences.is_some()
            || self.sealed_quantum.is_some()
    }

    pub(super) fn validate(&self) -> Result<(), Failure> {
        if !self.sealed && self.has_sealed_limits() {
            return Err(Failure::usage("--sealed-* limits require --sealed"));
        }
        Ok(())
    }

    fn sealed_limits(&self) -> Option<(FnxMemoryLimits, SealedLimits)> {
        if !self.sealed {
            return None;
        }
        let defaults = SealedLimits::default();
        Some((
            FnxMemoryLimits {
                max_kernel_workspace_bytes: self.sealed_kernel_bytes.unwrap_or(64 << 20),
                max_result_bytes: self.sealed_result_bytes.unwrap_or(64 << 20),
            },
            SealedLimits {
                max_rows: self.sealed_rows.unwrap_or(defaults.max_rows),
                max_incidences: self.sealed_incidences.unwrap_or(defaults.max_incidences),
                max_image_bytes: self.sealed_image_bytes.unwrap_or(defaults.max_image_bytes),
                max_property_bytes: self
                    .sealed_property_bytes
                    .unwrap_or(defaults.max_property_bytes),
            },
        ))
    }

    /// Each flag is accepted once; an unknown value is a usage error.
    pub(super) fn set(&mut self, flag: &str, value: &str) -> Result<(), Failure> {
        fn once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), Failure> {
            if slot.replace(value).is_some() {
                return Err(Failure::usage(format!("{flag} is allowed once")));
            }
            Ok(())
        }
        let bad = || Failure::usage(format!("invalid {flag} value"));
        match flag {
            "--graph-label" => once(&mut self.label, value.to_owned(), flag),
            "--graph-relation" => once(&mut self.relation, value.to_owned(), flag),
            "--weight" => once(&mut self.weight, value.to_owned(), flag),
            "--missing-weight" => {
                let policy = match value {
                    "reject" => MissingWeightPolicy::Reject,
                    "unit" => MissingWeightPolicy::Unit,
                    "zero" => MissingWeightPolicy::Zero,
                    _ => return Err(bad()),
                };
                once(&mut self.missing, policy, flag)
            }
            "--direction" => {
                let direction = match value {
                    "directed" => Directedness::Directed,
                    "reversed" => Directedness::Reversed,
                    "undirected" => Directedness::Undirected,
                    _ => return Err(bad()),
                };
                once(&mut self.direction, direction, flag)
            }
            "--parallel-edges" => {
                let policy = match value {
                    "reject" => ParallelEdgePolicy::Reject,
                    "collapse" => ParallelEdgePolicy::CollapseUnit,
                    "min" => ParallelEdgePolicy::Minimum,
                    "max" => ParallelEdgePolicy::Maximum,
                    "sum" => ParallelEdgePolicy::Sum,
                    _ => return Err(bad()),
                };
                once(&mut self.parallel, policy, flag)
            }
            "--self-loops" => {
                let policy = match value {
                    "keep" => SelfLoopPolicy::Keep,
                    "drop" => SelfLoopPolicy::Drop,
                    "reject" => SelfLoopPolicy::Reject,
                    _ => return Err(bad()),
                };
                once(&mut self.self_loops, policy, flag)
            }
            "--as-of" => {
                let seq = value.parse().map_err(|_| bad())?;
                once(&mut self.as_of, CommitSeq(seq), flag)
            }
            "--graph-staging-bytes" => once(
                &mut self.graph_staging_bytes,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--graph-workspace-bytes" => once(
                &mut self.graph_workspace_bytes,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--sealed-kernel-bytes" => once(
                &mut self.sealed_kernel_bytes,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--sealed-result-bytes" => once(
                &mut self.sealed_result_bytes,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--sealed-image-bytes" => once(
                &mut self.sealed_image_bytes,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--sealed-property-bytes" => once(
                &mut self.sealed_property_bytes,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--sealed-max-rows" => once(&mut self.sealed_rows, decimal_usize(flag, value)?, flag),
            "--sealed-max-incidences" => once(
                &mut self.sealed_incidences,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--sealed-source-incidences" => once(
                &mut self.sealed_source_incidences,
                decimal_usize(flag, value)?,
                flag,
            ),
            "--sealed-quantum" => once(
                &mut self.sealed_quantum,
                NonZeroUsize::new(decimal_usize(flag, value)?)
                    .ok_or_else(|| Failure::usage("--sealed-quantum must be positive"))?,
                flag,
            ),
            _ => Err(Failure::usage("unknown analytics flag")),
        }
    }

    fn options(&self, bindings: &Options) -> Result<FnxReadOptions, Failure> {
        let lookup =
            |name: &Option<String>, table: &std::collections::BTreeMap<String, u64>, what: &str| {
                name.as_ref()
                    .map(|name| {
                        table.get(name).copied().ok_or_else(|| {
                            Failure::usage(format!("{what} {name:?} has no binding"))
                        })
                    })
                    .transpose()
            };
        let weight = match lookup(&self.weight, &bindings.properties, "weight property")? {
            Some(key) => FnxWeightSpec::Property {
                key: PropertyKeyId(key),
                missing: self.missing.unwrap_or(MissingWeightPolicy::Reject),
            },
            None if self.missing.is_some() => {
                return Err(Failure::usage("--missing-weight requires --weight"));
            }
            None => FnxWeightSpec::Unit,
        };
        // Unset flags keep the canonical whole graph, including its refusal
        // of an implicit multigraph collapse, and its default admission.
        let mut base = FnxReadOptions::whole_graph(self.as_of);
        if let Some(limit) = self.graph_staging_bytes {
            base.source_limits.max_staging_bytes = limit;
        }
        if let Some(limit) = self.graph_workspace_bytes {
            base.projection_limits.max_workspace_bytes = limit;
        }
        Ok(FnxReadOptions {
            selection: FnxSelection {
                vertex_label: lookup(&self.label, &bindings.labels, "label")?.map(LabelId),
                relation: lookup(&self.relation, &bindings.relations, "relation")?.map(RelationId),
                weight,
            },
            projection: ProjectionSpec {
                directedness: self.direction.unwrap_or(base.projection.directedness),
                parallel_edges: self.parallel.unwrap_or(base.projection.parallel_edges),
                self_loops: self.self_loops.unwrap_or(base.projection.self_loops),
            },
            ..base
        })
    }
}

/// Typed procedure arguments. A vertex is a stable identity, never an int.
fn arguments(raw: &[(String, String)]) -> Result<FnxParameters, Failure> {
    let mut parameters = FnxParameters::new();
    for (name, value) in raw {
        let argument = if let Some(v) = value.strip_prefix("int:") {
            FnxArgument::Integer(
                v.parse()
                    .map_err(|_| Failure::usage("invalid int parameter"))?,
            )
        } else if let Some(v) = value.strip_prefix("float:") {
            FnxArgument::Float(
                v.parse()
                    .map_err(|_| Failure::usage("invalid float parameter"))?,
            )
        } else if let Some(v) = value.strip_prefix("vertex:") {
            FnxArgument::Vertex(VId(v
                .parse()
                .map_err(|_| Failure::usage("invalid vertex parameter"))?))
        } else {
            match value.as_str() {
                "bool:true" => FnxArgument::Boolean(true),
                "bool:false" => FnxArgument::Boolean(false),
                "null" => FnxArgument::Null,
                _ => {
                    return Err(Failure::usage(
                        "analytics parameters are int:, float:, bool:, null or vertex:<id>",
                    ));
                }
            }
        };
        if parameters.insert(name.clone(), argument).is_some() {
            return Err(Failure::usage(format!("duplicate parameter {name:?}")));
        }
    }
    Ok(parameters)
}

/// A call's typed arguments and resolved projection, bound before the
/// database opens so a refused input never touches storage.
pub(super) struct Prepared {
    call: FnxCallSpec,
    read: FnxReadOptions,
    sealed: Option<(FnxMemoryLimits, SealedLimits)>,
    source_incidences: usize,
    quantum: Option<NonZeroUsize>,
}

impl Prepared {
    pub(super) fn is_sealed(&self) -> bool {
        self.sealed.is_some()
    }
}

pub(super) fn prepare(options: &Options) -> Result<Prepared, Failure> {
    options.fnx.validate()?;
    let parameters = arguments(&options.raw_params)?;
    let read = options.fnx.options(options)?;
    let call = FnxCallSpec::bind(&options.text, &parameters).map_err(Failure::statement)?;
    let sealed = options.fnx.sealed_limits();
    if options.buffered.enabled()
        && (options.budget.records.is_some()
            || options.budget.rows.is_some()
            || options.budget.scratch.is_some())
    {
        return Err(Failure::usage(
            "native --max-snapshot-records, --max-result-rows and --max-scratch-entries \
                 do not apply to buffered analytics; use --buffer-*, --graph-staging-bytes \
                 and --graph-workspace-bytes limits",
        ));
    }
    if sealed.is_some() {
        // All graph-law and scheduling checks precede either read-only open.
        // Unsupported algorithms never fall back to decoded adjacency.
        if read.selection.relation.is_none() {
            return Err(Failure::usage("--sealed requires --graph-relation"));
        }
        if !options.buffered.enabled() && options.budget != super::QueryBudget::default() {
            return Err(Failure::usage(
                "native --max-snapshot-records, --max-result-rows, --max-work-units \
                 and --max-scratch-entries do not apply to --sealed; use --sealed-* limits",
            ));
        }
        call.validate_sealed_projection(read.projection.directedness)
            .map_err(Failure::query)?;
        if options.fnx.sealed_quantum.is_some() && !call.supports_cooperative_sealed_execution() {
            return Err(Failure::query(
                FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(call.algorithm()),
            ));
        }
    }
    let source_incidences = options.fnx.sealed_source_incidences.unwrap_or_else(|| {
        RootReadLimits::default()
            .max_incidences
            .max(sealed.map_or(0, |(_, limits)| limits.max_incidences))
    });
    Ok(Prepared {
        call,
        read,
        sealed,
        source_incidences,
        quantum: options.fnx.sealed_quantum,
    })
}

/// A native read bound before storage opens. The relation selects the sealed
/// source; it never supplies an omitted relation in the query itself.
pub(super) struct PreparedSealedRead {
    native: PreparedNativeRead,
    lowered_params: Option<fgdb_gql::GqlParameters>,
    relation: RelationId,
    as_of: Option<CommitSeq>,
    memory: FnxMemoryLimits,
    sealing: SealedLimits,
    source_limits: RootReadLimits,
}

/// Bind the explicit external owner without evaluating a reached procedure's
/// arguments early. The ordinary buffered compiler never admits procedures.
pub(super) fn prepare_buffered_sealed_order(
    options: &Options,
    native: &PreparedNativeRead,
    params: &fgdb_gql::GqlParameters,
) -> Result<fgdb::PreparedBufferedSealedOrder, Failure> {
    options.fnx.validate_composed()?;
    let name = options
        .fnx
        .relation
        .as_ref()
        .ok_or_else(|| Failure::usage("--sealed requires --graph-relation"))?;
    let relation = options
        .relations
        .get(name)
        .copied()
        .map(RelationId)
        .ok_or_else(|| Failure::usage(format!("relation {name:?} has no binding")))?;
    let (memory, sealing) = options
        .fnx
        .sealed_limits()
        .ok_or_else(|| Failure::usage("compressed native reads require --sealed"))?;
    let incidences = options.fnx.sealed_source_incidences.unwrap_or_else(|| {
        RootReadLimits::default()
            .max_incidences
            .max(sealing.max_incidences)
    });
    let prepared = native
        .prepare_buffered_sealed_order(params, relation, memory, sealing, incidences)
        .map_err(super::execution_failure)?;
    match options.fnx.as_of {
        Some(snapshot) => prepared
            .with_as_of(snapshot)
            .map_err(|error| Failure::coded(super::refusal::Code::InvalidValue, error)),
        None => Ok(prepared),
    }
}

pub(super) fn prepare_sealed_read(options: &Options) -> Result<PreparedSealedRead, Failure> {
    options.fnx.validate_composed()?;
    let name = options
        .fnx
        .relation
        .as_ref()
        .ok_or_else(|| Failure::usage("--sealed requires --graph-relation"))?;
    let relation = options
        .relations
        .get(name)
        .map(|id| RelationId(*id))
        .ok_or_else(|| Failure::usage(format!("relation {name:?} has no binding")))?;
    // Preserve the ordinary one-shot read contract for map fields used as
    // pattern keys. Keep the derived values with their prepared template.
    let lowered = fgdb_gql::GqlMapArgumentText::lower_fields(&options.text, &options.params);
    let (statement, params) = lowered.as_ref().map_or(
        (options.text.as_str(), &options.params),
        |(text, params)| (text.text(), params),
    );
    let native = PreparedNativeRead::prepare(statement, params, options)
        .map_err(|error| super::read_failure(options, error))?;
    // Bind values now, including parameters in a page or kernel argument, so
    // malformed input cannot reach the filesystem. Execution reuses the same
    // immutable template and parameter set for the selected historical cut.
    let mut as_of = options.fnx.as_of;
    let calls_procedure = match &native {
        PreparedNativeRead::Set(prepared) => prepared
            .bind_parameters(params)
            .map_err(|error| super::read_failure(options, QueryError::SetText(error)))?
            .calls_procedure(),
        PreparedNativeRead::PipelineAggregate(prepared) if prepared.requires_relational_input() => {
            prepared
                .bind_relation_parameters(params)
                .map_err(|error| super::read_failure(options, QueryError::PipelineText(error)))?
                .input()
                .calls_procedure()
        }
        PreparedNativeRead::TemporalSet(prepared) => {
            let query = prepared.bind_parameters(params).map_err(|error| {
                super::read_failure(options, QueryError::TemporalSetText(error))
            })?;
            let requested = query.as_of();
            if let Some(retained) = as_of
                && retained != requested
            {
                return Err(Failure::coded(
                    super::refusal::Code::InvalidValue,
                    SealedQueryError::SnapshotMismatch {
                        requested,
                        retained,
                    },
                ));
            }
            as_of = Some(requested);
            query.query().calls_procedure()
        }
        other => {
            return Err(Failure::coded(
                super::refusal::Code::Unsupported,
                format!(
                    "native {:?} read has no composed procedure source",
                    other.facade_class()
                ),
            ));
        }
    };
    if !calls_procedure {
        return Err(Failure::coded(
            super::refusal::Code::Unsupported,
            "composed --sealed execution requires a procedure call",
        ));
    }
    let (memory, sealing) = options
        .fnx
        .sealed_limits()
        .ok_or_else(|| Failure::usage("compressed native reads require --sealed"))?;
    let source_limits = RootReadLimits {
        max_incidences: options.fnx.sealed_source_incidences.unwrap_or_else(|| {
            RootReadLimits::default()
                .max_incidences
                .max(sealing.max_incidences)
        }),
        ..RootReadLimits::default()
    };
    Ok(PreparedSealedRead {
        native,
        lowered_params: lowered.map(|(_, params)| params),
        relation,
        as_of,
        memory,
        sealing,
        source_limits,
    })
}

/// Keep analytics, native MATCH and property reads on one published generation.
/// The whole native result completes before its first output event is written.
pub(super) async fn run_sealed_read(
    open_cx: &CommitCx,
    cx: &QueryCx,
    options: &Options,
    keys: DatabaseKeys,
    prepared: &PreparedSealedRead,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let view = Database::open_sealed_read(
        open_cx,
        cx,
        &options.db,
        keys,
        prepared.as_of,
        prepared.relation,
        prepared.sealing,
        prepared.source_limits,
    )
    .await
    .map_err(|error| match error {
        SealedProjectionOpenError::Open(error) => super::open_failure(error),
        SealedProjectionOpenError::Read(error) => super::execution_failure(error),
        SealedProjectionOpenError::RecoveryRequired => Failure::open(error),
    })?;
    let result = view
        .execute_prepared(
            cx,
            &prepared.native,
            prepared.lowered_params.as_ref().unwrap_or(&options.params),
            options.budget.policy(),
            prepared.memory,
        )
        .map_err(|error| match error {
            SealedQueryError::Query(error) => super::read_failure(options, error),
            error @ SealedQueryError::SnapshotMismatch { .. } => {
                Failure::coded(super::refusal::Code::InvalidValue, error)
            }
        })?;
    super::render(result, view.snapshot_seq().0, "rows", robot, out)
}

pub(super) fn run(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    if robot {
        let mut rows = cx.with_restriction(|| {
            let graph = view
                .prism_projection_at(
                    cx,
                    prepared.read.as_of.unwrap_or(view.frontier()),
                    prepared.read.selection,
                    prepared.read.projection,
                    prepared.read.projection_limits,
                    prepared.read.source_limits,
                )
                .map_err(Failure::query)?;
            prepared
                .call
                .execute_rows(&graph, prepared.read.execution_limits, || cx.checkpoint())
                .map_err(Failure::query)
        })?;
        return render_robot_rows(&prepared.call, &mut rows, out, || cx.checkpoint());
    }
    let result = view
        .execute_fnx(cx, &prepared.call, prepared.read)
        .map_err(Failure::query)?;
    render(result, robot, out)
}

/// The extent cache and graph source use the buffered view's allowances;
/// selected staging, compressed or decoded projection, and execution retain
/// their Prism limits. No resident database or writer lease is constructed.
pub(super) async fn run_buffered<V: Vfs + Clone>(
    view: &mut BufferedReadView<V>,
    cx: &QueryCx,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    if robot {
        let mut rows = cx
            .with_restriction_async(async {
                if let Some((memory, sealing)) = prepared.sealed {
                    cx.checkpoint().map_err(|error| {
                        super::execution_failure(
                            FnxReadError::<fgdb::BufferedReadError, _>::Cancelled(error),
                        )
                    })?;
                    if prepared.quantum.is_some()
                        && !prepared.call.supports_cooperative_sealed_execution()
                    {
                        return Err(super::execution_failure(
                            FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(
                                prepared.call.algorithm(),
                            ),
                        ));
                    }
                    prepared
                        .call
                        .validate_sealed_projection(prepared.read.projection.directedness)
                        .map_err(super::execution_failure)?;
                    let graph = match prepared.quantum {
                        Some(quantum) => {
                            view.prism_sealed_projection_cooperative_at(
                                cx,
                                prepared.read.as_of.unwrap_or(view.frontier()),
                                prepared.read.selection,
                                prepared.read.projection,
                                prepared.read.projection_limits,
                                prepared.read.source_limits,
                                sealing,
                                prepared.source_incidences,
                                quantum,
                            )
                            .await
                        }
                        None => {
                            view.prism_sealed_projection_at(
                                cx,
                                prepared.read.as_of.unwrap_or(view.frontier()),
                                prepared.read.selection,
                                prepared.read.projection,
                                prepared.read.projection_limits,
                                prepared.read.source_limits,
                                sealing,
                                prepared.source_incidences,
                            )
                            .await
                        }
                    }
                    .map_err(super::execution_failure)?;
                    match prepared.quantum {
                        Some(quantum) => {
                            prepared
                                .call
                                .execute_sealed_cooperative_rows(
                                    cx,
                                    &graph,
                                    prepared.read.execution_limits,
                                    memory,
                                    quantum,
                                    asupersync::runtime::yield_now::yield_now,
                                )
                                .await
                        }
                        None => prepared.call.execute_sealed_rows(
                            cx,
                            &graph,
                            prepared.read.execution_limits,
                            memory,
                        ),
                    }
                    .map_err(super::execution_failure)
                } else {
                    let graph = view
                        .prism_projection_at(
                            cx,
                            prepared.read.as_of.unwrap_or(view.frontier()),
                            prepared.read.selection,
                            prepared.read.projection,
                            prepared.read.projection_limits,
                            prepared.read.source_limits,
                        )
                        .await
                        .map_err(super::execution_failure)?;
                    prepared
                        .call
                        .execute_rows(&graph, prepared.read.execution_limits, || cx.checkpoint())
                        .map_err(super::execution_failure)
                }
            })
            .await?;
        return render_robot_rows(&prepared.call, &mut rows, out, || cx.checkpoint());
    }
    let result = if let Some((memory, sealing)) = prepared.sealed {
        match prepared.quantum {
            Some(quantum) => {
                view.execute_fnx_sealed_cooperative(
                    cx,
                    &prepared.call,
                    prepared.read,
                    memory,
                    sealing,
                    prepared.source_incidences,
                    quantum,
                )
                .await
            }
            None => {
                view.execute_fnx_sealed(
                    cx,
                    &prepared.call,
                    prepared.read,
                    memory,
                    sealing,
                    prepared.source_incidences,
                )
                .await
            }
        }
        .map_err(super::execution_failure)?
    } else {
        view.execute_fnx(cx, &prepared.call, prepared.read)
            .await
            .map_err(super::execution_failure)?
    };
    render(result, robot, out)
}

/// Prepare directly from the current authenticated checkpoint. No writer or
/// resident database is opened, and source payloads are read only once. The
/// temporary image remains derived, with separate source/projection limits.
pub(super) async fn run_sealed(
    open_cx: &CommitCx,
    cx: &QueryCx,
    options: &Options,
    keys: DatabaseKeys,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let (memory, sealing) = prepared
        .sealed
        .ok_or_else(|| Failure::usage("compressed analytics requires --sealed"))?;
    let root_limits = RootReadLimits {
        max_incidences: prepared.source_incidences,
        ..RootReadLimits::default()
    };
    let graph = match prepared.quantum {
        Some(quantum) => {
            Database::open_prism_sealed_projection_cooperative(
                open_cx,
                cx,
                &options.db,
                keys,
                prepared.read,
                sealing,
                root_limits,
                quantum,
            )
            .await
        }
        None => {
            Database::open_prism_sealed_projection(
                open_cx,
                cx,
                &options.db,
                keys,
                prepared.read,
                sealing,
                root_limits,
            )
            .await
        }
    }
    .map_err(|error| match error {
        SealedProjectionOpenError::Open(error) => super::open_failure(error),
        SealedProjectionOpenError::Read(error) => super::execution_failure(error),
        SealedProjectionOpenError::RecoveryRequired => Failure::open(error),
    })?;
    if robot {
        let mut rows = match prepared.quantum {
            Some(quantum) => {
                prepared
                    .call
                    .execute_sealed_cooperative_rows(
                        cx,
                        &graph,
                        prepared.read.execution_limits,
                        memory,
                        quantum,
                        asupersync::runtime::yield_now::yield_now,
                    )
                    .await
            }
            None => prepared.call.execute_sealed_rows(
                cx,
                &graph,
                prepared.read.execution_limits,
                memory,
            ),
        }
        .map_err(super::execution_failure)?;
        return render_robot_rows(&prepared.call, &mut rows, out, || cx.checkpoint());
    }
    let analytics = match prepared.quantum {
        Some(quantum) => {
            prepared
                .call
                .execute_sealed_cooperative(
                    cx,
                    &graph,
                    prepared.read.execution_limits,
                    memory,
                    quantum,
                    asupersync::runtime::yield_now::yield_now,
                )
                .await
        }
        None => prepared
            .call
            .execute_sealed(cx, &graph, prepared.read.execution_limits, memory),
    }
    .map_err(super::execution_failure)?;
    render(
        FnxReadResult::bind_selection(analytics, prepared.read.selection),
        robot,
        out,
    )
}

/// Deliver a fully validated, certified answer without building a typed row
/// table. The projection and completed kernel arrays remain owned by the cursor;
/// one reusable row buffer holds the current output. Ordinary robot framing and
/// YIELD order are unchanged. Human tables retain their replayable width pass.
fn render_robot_rows<C: std::fmt::Display>(
    call: &FnxCallSpec,
    rows: &mut FnxRowCursor,
    out: &mut impl Write,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<(), Failure> {
    let count = rows.remaining();
    let seq = rows.certificate().snapshot.as_of.0;
    // Empty projections do not enter next_row's per-vertex checkpoint.
    checkpoint().map_err(Failure::query)?;
    super::emit(
        out,
        &format!(
            r#"{{"v":1,"event":"columns","columns":[{}]}}"#,
            call.outputs()
                .iter()
                .map(|column| super::quoted(&column.name))
                .collect::<Vec<_>>()
                .join(",")
        ),
    )?;
    let mut row = Vec::new();
    let mut encoded = String::new();
    // Exhaust the cursor even after its last emitted row: sparse traversals can
    // still have unreachable ordinals that must observe live cancellation.
    while rows
        .next_row(&mut row, &mut checkpoint)
        .map_err(Failure::query)?
    {
        encoded.clear();
        encoded.push_str(r#"{"v":1,"event":"row","cells":["#);
        for (index, value) in row.iter().enumerate() {
            if index != 0 {
                encoded.push(',');
            }
            encoded.push_str(&cell(*value, true));
        }
        encoded.push_str("]}");
        super::emit(out, &encoded)?;
    }
    // A buffered output failure must be observed before announcing success.
    // This is ordinary robot output, so it does not flush once per vertex.
    out.flush().map_err(Failure::io)?;
    checkpoint().map_err(Failure::query)?;
    super::emit(
        out,
        &format!(r#"{{"v":1,"event":"result","kind":"rows","seq":{seq},"count":{count}}}"#),
    )?;
    // A failure here cannot retract bytes already written, but still makes the
    // command fail instead of reporting a successful delivery to its caller.
    out.flush().map_err(Failure::io)
}

fn render(result: FnxReadResult, robot: bool, out: &mut impl Write) -> Result<(), Failure> {
    let analytics = result.analytics;
    let cells = analytics.rows.iter().map(|row| {
        row.iter()
            .map(|value| cell(*value, robot))
            .collect::<Vec<_>>()
    });
    render_rows_iter(
        &analytics.columns,
        cells,
        analytics.certificate.snapshot.as_of.0,
        "rows",
        robot,
        out,
    )
}

/// Exact domains: identities and counts are decimal text, scores and
/// distances shortest-round-trip floats, matching the query cell grammar.
fn cell(value: FnxValue, robot: bool) -> String {
    match (value, robot) {
        (FnxValue::Vertex(v), true) => format!(r#"{{"type":"vertex","value":"{}"}}"#, v.0),
        (FnxValue::Vertex(v), false) => format!("vertex {}", v.0),
        (FnxValue::Integer(v), true) => match i64::try_from(v) {
            Ok(v) => format!(r#"{{"type":"int","value":"{v}"}}"#),
            Err(_) => format!(r#"{{"type":"wideint","value":"{v}"}}"#),
        },
        (FnxValue::Integer(v), false) => v.to_string(),
        (FnxValue::Score(v) | FnxValue::Float(v), true) => {
            format!(r#"{{"type":"float","value":"{}"}}"#, float_text(v))
        }
        (FnxValue::Score(v) | FnxValue::Float(v), false) => float_text(v),
    }
}

#[cfg(test)]
mod tests {
    use super::{Failure, arguments, is_call};
    use fgdb_prism::{FnxCallSpec, FnxParameters};

    fn composed_options(text: &str, flags: &[&str]) -> Result<super::Options, super::Failure> {
        let mut args: Vec<String> = [
            "--db",
            "never-opened-database",
            "--key-file",
            "never-opened-key",
            "--relation",
            "KNOWS=1",
            text,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        args.extend(flags.iter().map(|flag| (*flag).to_owned()));
        super::super::parse(&args, "query")
    }

    #[test]
    fn composed_sealed_preflight_binds_native_queries_and_keeps_both_budget_domains() {
        for text in [
            "CALL fnx.weakly_connected_components({relations:['KNOWS'],parallel:'collapse'}) \
             YIELD vertex, component RETURN vertex, component ORDER BY vertex LIMIT $top",
            "CALL fnx.weakly_connected_components({relations:['KNOWS'],parallel:'collapse'}) \
             YIELD vertex, component RETURN component, COUNT(*) AS total \
             GROUP BY component ORDER BY component LIMIT $top",
        ] {
            let mut options = composed_options(
                text,
                &[
                    "--sealed",
                    "--graph-relation",
                    "KNOWS",
                    "--as-of",
                    "7",
                    "--max-result-rows",
                    "9",
                    "--max-work-units",
                    "10000",
                    "--sealed-result-bytes",
                    "2048",
                    "--sealed-source-incidences",
                    "123",
                ],
            )
            .unwrap();
            assert!(!options.fnx_call);
            options.params = fgdb_gql::GqlParameters::new().with_int64("top", 2).unwrap();
            let prepared = super::prepare_sealed_read(&options).unwrap();
            assert_eq!(prepared.relation, fgdb_delta_types::RelationId(1));
            assert_eq!(prepared.as_of, Some(fgdb_types::CommitSeq(7)));
            assert_eq!(prepared.memory.max_result_bytes, 2048);
            assert_eq!(prepared.source_limits.max_incidences, 123);
            assert_eq!(options.budget.rows, Some(9));
            assert_eq!(options.budget.work, Some(10000));

            options.params = fgdb_gql::GqlParameters::new();
            let failure = super::prepare_sealed_read(&options).err().unwrap();
            assert_eq!(
                failure.code, 3,
                "missing page argument must bind before open"
            );
        }
    }

    #[test]
    fn temporal_sealed_preflight_selects_the_bound_cut_and_refuses_conflicting_flags() {
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WITH n \
                    CALL fnx.weakly_connected_components(relation => 'KNOWS') \
                    YIELD vertex RETURN vertex LIMIT 0";
        for requested in [0, 7] {
            for explicit in [false, true] {
                let rendered = requested.to_string();
                let mut flags = vec!["--sealed", "--graph-relation", "KNOWS"];
                if explicit {
                    flags.extend(["--as-of", rendered.as_str()]);
                }
                let mut options = composed_options(text, &flags).unwrap();
                options.params = fgdb_gql::GqlParameters::new()
                    .with_uint64("at", requested)
                    .unwrap();
                let prepared = super::prepare_sealed_read(&options).unwrap();
                assert_eq!(prepared.as_of, Some(fgdb_types::CommitSeq(requested)));
                options.params = fgdb_gql::GqlParameters::new();
                assert_eq!(super::prepare_sealed_read(&options).err().unwrap().code, 3);
            }
        }
        let mut options = composed_options(
            text,
            &["--sealed", "--graph-relation", "KNOWS", "--as-of", "8"],
        )
        .unwrap();
        options.params = fgdb_gql::GqlParameters::new().with_uint64("at", 7).unwrap();
        let failure = super::prepare_sealed_read(&options).err().unwrap();
        assert_eq!(failure.code, 3);
        assert_eq!(failure.refusal, super::super::refusal::Code::InvalidValue);
    }

    #[test]
    fn composed_sealed_preflight_retains_map_fields_used_by_pattern_keys() {
        let text = "CALL fnx.weakly_connected_components({relations:['KNOWS'],parallel:'collapse'}) \
                    YIELD vertex MATCH (vertex:Person {id: $filter.id}) \
                    RETURN vertex.id AS id";
        let mut options = composed_options(
            text,
            &[
                "--sealed",
                "--graph-relation",
                "KNOWS",
                "--label",
                "Person=1",
                "--property",
                "id=1",
            ],
        )
        .unwrap();
        options
            .params
            .insert(
                "filter",
                super::super::json_parameter(r#"{"id":10}"#).unwrap(),
            )
            .unwrap();
        let prepared = super::prepare_sealed_read(&options).unwrap();
        let derived = prepared.lowered_params.as_ref().unwrap();
        assert_eq!(derived.len(), 1);
        assert!(derived.get("filter").is_none());
        let super::PreparedNativeRead::Set(native) = &prepared.native else {
            panic!("composed query must retain its native set template");
        };
        assert_eq!(native.columns(), &["id"]);
        assert!(native.bind_parameters(derived).unwrap().calls_procedure());
        assert!(native.bind_parameters(&options.params).is_err());
    }

    #[test]
    fn composed_sealed_preflight_refuses_ignored_flags_and_unsupported_native_classes() {
        let text = "CALL fnx.weakly_connected_components({relations:['KNOWS']}) \
                    YIELD vertex RETURN vertex";
        for flags in [vec!["--sealed"], vec!["--graph-relation", "KNOWS"]] {
            assert_eq!(composed_options(text, &flags).err().unwrap().code, 2);
        }
        for extra in [
            &["--direction", "directed"][..],
            &["--graph-label", "Person"][..],
            &["--weight", "weight"][..],
            &["--missing-weight", "unit"][..],
            &["--parallel-edges", "collapse"][..],
            &["--self-loops", "drop"][..],
            &["--graph-staging-bytes", "1"][..],
            &["--graph-workspace-bytes", "1"][..],
            &["--sealed-quantum", "1"][..],
            &["--buffered"][..],
            &["--spill-dir", "never-opened-spill"][..],
            &["--stream"][..],
            &["--certify-to", "never-created-certificate"][..],
        ] {
            let mut flags = vec!["--sealed", "--graph-relation", "KNOWS"];
            flags.extend_from_slice(extra);
            let failure = composed_options(text, &flags).err().unwrap();
            assert_eq!(failure.code, 2, "{extra:?}");
        }
        for text in [
            "MATCH (n) RETURN n",
            "RETURN 1 AS n UNION ALL RETURN 2 AS n",
        ] {
            let options =
                composed_options(text, &["--sealed", "--graph-relation", "KNOWS"]).unwrap();
            let failure = super::prepare_sealed_read(&options).err().unwrap();
            assert_eq!(failure.code, 3);
            assert_eq!(failure.refusal, super::super::refusal::Code::Unsupported);
        }
    }

    #[test]
    fn composed_buffered_sealed_preflight_selects_the_explicit_external_owner() {
        let text = "CALL fnx.weakly_connected_components({relations:['KNOWS'],parallel:'collapse'}) \
                    YIELD node,component RETURN node,component ORDER BY component,node LIMIT 2";
        for flags in [
            vec![
                "--sealed",
                "--graph-relation",
                "KNOWS",
                "--buffered",
                "--spill-dir",
                "never-opened-spill",
            ],
            vec![
                "--spill-dir",
                "never-opened-spill",
                "--buffered",
                "--sealed",
                "--graph-relation",
                "KNOWS",
                "--max-result-rows",
                "2",
                "--max-snapshot-records",
                "1000",
                "--max-work-units",
                "100000",
                "--max-scratch-entries",
                "10000",
                "--as-of",
                "7",
            ],
        ] {
            let options = composed_options(text, &flags).unwrap();
            assert!(options.buffered.enabled());
            assert!(options.spill.enabled());
            assert!(!options.fnx_call);
            let prepared = super::super::spill::prepare_buffered(&options).unwrap();
            let super::super::spill::PreparedBuffered::SealedOrdered(prepared) = prepared else {
                panic!("explicit composed mode must retain its sealed external owner");
            };
            assert!(prepared.stage_count() > 0);
        }
    }

    #[test]
    fn composed_buffered_sealed_preflight_refuses_unsupported_shapes_before_open() {
        let flags = [
            "--sealed",
            "--graph-relation",
            "KNOWS",
            "--buffered",
            "--spill-dir",
            "never-opened-spill",
        ];
        for text in [
            "MATCH (n) RETURN n",
            "CALL fnx.weakly_connected_components({relations:['KNOWS']}) YIELD node RETURN COUNT(*) AS count",
            "(CALL fnx.weakly_connected_components({relations:['KNOWS']}) YIELD node RETURN node) \
             UNION ALL (CALL fnx.weakly_connected_components({relations:['KNOWS']}) YIELD node RETURN node)",
        ] {
            let options = composed_options(text, &flags).unwrap();
            assert!(
                super::super::spill::prepare_buffered(&options).is_err(),
                "{text}"
            );
        }
        let text = "CALL fnx.weakly_connected_components({relations:['KNOWS']}) YIELD node \
                    MATCH (node) FOR SYSTEM_TIME AS OF SEQ 2 RETURN node";
        let mut conflicting = flags.to_vec();
        conflicting.extend_from_slice(&["--as-of", "3"]);
        let options = composed_options(text, &conflicting).unwrap();
        let error = super::super::spill::prepare_buffered(&options)
            .err()
            .unwrap();
        assert_eq!(error.refusal, super::super::refusal::Code::InvalidValue);
    }

    #[test]
    fn sealed_core_and_eigenvector_preflight_accept_only_undirected_projections() {
        for call in [
            "CALL fnx.core_number() YIELD core AS level, node AS id",
            "CALL fnx.eigenvector_centrality() YIELD score AS rank, node AS id",
            "CALL fnx.load_centrality() YIELD score AS rank, node AS id",
        ] {
            for direction in ["directed", "reversed", "undirected"] {
                let args: Vec<String> = [
                    "--db",
                    "never-opened-database",
                    "--key-file",
                    "never-opened-key",
                    "--sealed",
                    "--relation",
                    "KNOWS=1",
                    "--graph-relation",
                    "KNOWS",
                    "--direction",
                    direction,
                    call,
                ]
                .into_iter()
                .map(str::to_owned)
                .collect();
                let options = super::super::parse(&args, "query").unwrap();
                assert!(options.fnx_call);
                match super::prepare(&options) {
                    Ok(prepared) => {
                        assert_eq!(direction, "undirected");
                        assert!(prepared.is_sealed());
                        assert!(prepared.call.supports_sealed_execution());
                    }
                    Err(failure) => {
                        assert_ne!(direction, "undirected");
                        assert_eq!(failure.code, 3);
                        assert_eq!(failure.class, "query");
                    }
                }
            }
        }
    }

    #[test]
    fn sealed_hits_preflight_accepts_all_explicit_graph_kinds_and_both_scores() {
        for direction in ["directed", "reversed", "undirected"] {
            let args: Vec<String> = [
                "--db",
                "never-opened-database",
                "--key-file",
                "never-opened-key",
                "--sealed",
                "--relation",
                "KNOWS=1",
                "--graph-relation",
                "KNOWS",
                "--direction",
                direction,
                "CALL fnx.hits_centrality() YIELD authority AS a,node AS id,hub AS h",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            let options = super::super::parse(&args, "query").unwrap();
            assert!(options.fnx_call);
            let prepared = super::prepare(&options).unwrap();
            assert!(prepared.is_sealed());
            assert!(prepared.call.supports_sealed_execution());
            assert_eq!(
                prepared
                    .call
                    .outputs()
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>(),
                ["a", "id", "h"],
            );
        }
    }

    #[test]
    fn projection_envelopes_use_the_native_host_and_positional_calls_keep_their_route() {
        assert!(is_call(
            "CALL fnx.connected_components() YIELD vertex, component",
        ));
        for text in [
            "CALL fnx.connected_components() YIELD vertex RETURN vertex",
            "CALL fnx.connected_components({}) YIELD vertex",
            "CALL fnx.connected_components({}) YIELD vertex RETURN vertex",
            "CALL fnx.connected_components(args => []) YIELD vertex",
            "CALL fnx.connected_components(args => []) YIELD vertex RETURN vertex",
            "CALL fnx.pagerank(label => 'L', iterations => 2) YIELD vertex,score",
            "CALL fnx.pagerank(iterations => 2, weighted => false) YIELD score RETURN score",
            "CALL fnx.degree(label => 'L') YIELD vertex RETURN vertex",
            "/* leading */ CALL fnx.degree ( args /* split */ => [] ) YIELD vertex RETURN vertex",
            "CALL fnx.degree({unknown: 'invalid'}) YIELD vertex RETURN vertex",
        ] {
            assert!(!is_call(text), "{text}");
        }
    }

    #[test]
    fn kernel_named_calls_classify_before_binding_actual_parameter_values() {
        for text in [
            "CALL fnx.pagerank(iterations => 2, weighted => false) YIELD vertex,score",
            "/* leading */ cAlL fnx.pagerank ( weighted => false, iterations /* count */ => 2 ) YIELD score AS rank",
            "CALL fnx.pagerank(alpha => 0.85, max_iter => 100, tol => 1e-6, weighted => false)",
            "CALL fnx.pagerank(l1_tol => 1e-9, max_iter => 1000, weighted => false)",
        ] {
            assert!(is_call(text), "{text}");
            FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
        }
        let text =
            "CALL fnx.pagerank(iterations => $steps, weighted => $weighted) YIELD vertex,score";
        for steps in [1, 2, 5] {
            let raw = [
                ("steps".to_owned(), format!("int:{steps}")),
                ("weighted".to_owned(), "bool:false".to_owned()),
            ];
            assert!(is_call(text), "steps={steps}");
            FnxCallSpec::bind(text, &arguments(&raw).unwrap()).unwrap();
            assert!(!is_call(&format!("{text} RETURN vertex,score")));
        }
        assert!(is_call(text), "missing values do not change the route");
        assert!(FnxCallSpec::bind(text, &FnxParameters::new()).is_err());
        assert!(is_call(
            "CALL fnx.pagerank(l1_tol => $accuracy) YIELD vertex,score",
        ));
        let ambiguous = "CALL fnx.pagerank(l1_tol => 1e-9, tol => 1e-6) YIELD vertex,score";
        assert!(is_call(ambiguous));
        assert!(FnxCallSpec::bind(ambiguous, &FnxParameters::new()).is_err());
        for value in ["int:0", "int:-1", "float:2", "null", "int:overflow"] {
            let raw = [
                ("steps".to_owned(), value.to_owned()),
                ("weighted".to_owned(), "bool:false".to_owned()),
            ];
            assert!(is_call(text), "{value}: binding cannot change the route");
            assert!(
                arguments(&raw)
                    .and_then(|parameters| {
                        FnxCallSpec::bind(text, &parameters).map_err(Failure::statement)
                    })
                    .is_err(),
                "{value}: invalid values still refuse"
            );
        }
        let source_call = "CALL fnx.single_source_shortest_path_length(source => $source, cutoff => 2) YIELD vertex,distance";
        assert!(is_call(source_call));
        FnxCallSpec::bind(
            source_call,
            &arguments(&[("source".to_owned(), format!("vertex:{}", u128::MAX))]).unwrap(),
        )
        .unwrap();
        assert!(!is_call(
            "CALL fnx.pagerank(label => $label, iterations => 2) YIELD vertex,score",
        ));
    }

    #[test]
    fn native_limits_keep_named_calls_native_or_refuse_projection_flags_before_open() {
        let parse_query = |text: &str, flags: &[&str]| {
            let mut args: Vec<String> = [
                "--db",
                "never-opened-database",
                "--key-file",
                "never-opened-key",
                "--param",
                "steps=int:2",
                text,
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            args.extend(flags.iter().map(|flag| (*flag).to_owned()));
            super::super::parse(&args, "query")
        };
        for text in [
            "CALL fnx.pagerank(iterations => 2, weighted => false) YIELD vertex,score",
            "CALL fnx.pagerank(iterations => $steps, weighted => false) YIELD vertex,score",
        ] {
            assert!(parse_query(text, &[]).unwrap().fnx_call);
            for flag in [
                "--max-snapshot-records",
                "--max-result-rows",
                "--max-work-units",
                "--max-scratch-entries",
            ] {
                let options = parse_query(text, &[flag, "0"]).unwrap();
                assert!(!options.fnx_call, "{flag}: native limits remain active");
                assert!(options.budget != super::super::QueryBudget::default());
                for projection in [
                    vec!["--graph-relation", "KNOWS"],
                    vec!["--sealed", "--graph-relation", "KNOWS"],
                ] {
                    let mut flags = projection;
                    flags.extend_from_slice(&[flag, "0"]);
                    let Err(failure) = parse_query(text, &flags) else {
                        panic!("incompatible native/Prism limits admitted: {flags:?}");
                    };
                    assert_eq!(failure.code, 2);
                    assert_eq!(failure.class, "usage");
                }
            }
            // Buffered analytics admits a physical work cap, including zero,
            // and refuses native row/record/scratch domains before storage.
            for projection in [
                vec![],
                vec!["--relation", "KNOWS=1", "--graph-relation", "KNOWS"],
            ] {
                for work in ["0", "10000000"] {
                    let mut flags = vec!["--buffered", "--max-work-units", work];
                    flags.extend_from_slice(&projection);
                    let options = parse_query(text, &flags).unwrap();
                    assert!(options.fnx_call, "{flags:?}");
                    super::prepare(&options).unwrap();
                    let (_, limits) = options.buffered.admission(options.budget.policy()).unwrap();
                    assert_eq!(limits.max_work, work.parse::<usize>().unwrap());
                }
                for flag in [
                    "--max-snapshot-records",
                    "--max-result-rows",
                    "--max-scratch-entries",
                ] {
                    let mut flags = vec!["--buffered", flag, "0"];
                    flags.extend_from_slice(&projection);
                    let options = parse_query(text, &flags).unwrap();
                    assert!(options.fnx_call, "{flags:?}");
                    let Err(failure) = super::prepare(&options) else {
                        panic!("incompatible buffered analytics limits admitted: {flags:?}");
                    };
                    assert_eq!(failure.code, 2);
                    assert_eq!(failure.class, "usage");
                    assert!(
                        failure
                            .message
                            .contains("do not apply to buffered analytics")
                    );
                }
            }
        }
        // This routing correction leaves the established positional path alone.
        assert!(
            parse_query("CALL fnx.pagerank()", &["--max-result-rows", "0"])
                .unwrap()
                .fnx_call
        );
    }
}

#[cfg(test)]
mod completed_rows_tests {
    use super::*;
    use asupersync::{Budget, runtime::RuntimeBuilder};
    use fgdb::{BufferedReadView, WriteBatch};
    use fgdb_prism::{ProjectionEdge, SnapshotBinding, SnapshotGraphView};
    use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts, ids::ObjectId};
    use std::convert::Infallible;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn proceed() -> Result<(), Infallible> {
        Ok(())
    }

    fn bind(text: &str) -> FnxCallSpec {
        FnxCallSpec::bind(text, &FnxParameters::new()).unwrap()
    }

    fn graph(call: &FnxCallSpec, empty: bool) -> SnapshotGraphView {
        let options = FnxReadOptions::whole_graph_for(call, None);
        let vertices = [VId(1), VId(7), VId(1 << 100), VId(u128::MAX)];
        let edges = [(1, 7, 1.25), (7, 1 << 100, 2.5), (1 << 100, 1, 4.0)]
            .into_iter()
            .enumerate()
            .map(|(index, (source, target, weight))| ProjectionEdge {
                eid: EId(index as u128),
                source: VId(source),
                target: VId(target),
                weight,
            })
            .collect::<Vec<_>>();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([0x83; 32]),
                as_of: CommitSeq(17),
            },
            if empty { &[] } else { &vertices },
            if empty { &[] } else { &edges },
            options.projection,
            options.projection_limits,
        )
        .unwrap()
    }

    fn cursor(call: &FnxCallSpec, graph: &SnapshotGraphView) -> FnxRowCursor {
        call.execute_rows(graph, FnxReadOptions::DEFAULT_EXECUTION_LIMITS, proceed)
            .unwrap()
    }

    #[test]
    fn completed_robot_rows_match_collected_bytes_and_entire_certificate() {
        for text in [
            "CALL fnx.pagerank(iterations => 3, weighted => true) YIELD score AS rank,vertex AS id",
            "CALL fnx.single_source_shortest_path_length(1) YIELD distance AS hops,vertex",
            "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD distance,vertex",
            "CALL fnx.weakly_connected_components() YIELD component AS group_id,vertex",
            "CALL fnx.strongly_connected_components() YIELD vertex,component",
            "CALL fnx.connected_components() YIELD vertex,component",
            "CALL fnx.triangles() YIELD triangles AS total,vertex",
            "CALL fnx.clustering_coefficient() YIELD score,vertex",
            "CALL fnx.cdlp(3) YIELD component,vertex",
        ] {
            let call = bind(text);
            let graph = graph(&call, false);
            let collected = call
                .execute(&graph, FnxReadOptions::DEFAULT_EXECUTION_LIMITS, proceed)
                .unwrap();
            let mut rows = cursor(&call, &graph);
            // The cursor is completed before any output: the kernel witness,
            // value digest, projection identity and historical cut all agree.
            assert_eq!(rows.certificate(), &collected.certificate, "{text}");
            assert_eq!(rows.remaining(), collected.rows.len(), "{text}");
            let mut expected = Vec::new();
            render(
                FnxReadResult::bind_selection(
                    collected,
                    FnxReadOptions::whole_graph(None).selection,
                ),
                true,
                &mut expected,
            )
            .unwrap();
            let mut actual = Vec::new();
            render_robot_rows(&call, &mut rows, &mut actual, proceed).unwrap();
            assert_eq!(actual, expected, "{text}");
            assert_eq!(rows.remaining(), 0);
            assert!(
                String::from_utf8(actual)
                    .unwrap()
                    .ends_with(if text.contains("source_") {
                        "\"seq\":17,\"count\":3}\n"
                    } else {
                        "\"seq\":17,\"count\":4}\n"
                    })
            );
        }

        let call =
            bind("CALL fnx.weakly_connected_components() YIELD component AS group_id,vertex");
        let graph = graph(&call, true);
        let mut rows = cursor(&call, &graph);
        let mut actual = Vec::new();
        render_robot_rows(&call, &mut rows, &mut actual, proceed).unwrap();
        assert_eq!(
            actual,
            b"{\"v\":1,\"event\":\"columns\",\"columns\":[\"group_id\",\"vertex\"]}\n\
              {\"v\":1,\"event\":\"result\",\"kind\":\"rows\",\"seq\":17,\"count\":0}\n"
        );
    }

    #[test]
    fn cancellation_checks_empty_rows_sparse_suffixes_and_final_completion() {
        let call = bind("CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance");
        let projection = graph(&call, false);
        // Before columns, each of four vertex ordinals (three reachable), and
        // after the flushed body. In particular remaining()==0 is not EOF.
        for stop in 1_usize..=6 {
            let mut rows = cursor(&call, &projection);
            let mut actual = Vec::new();
            let mut checkpoints = 0;
            let failure = render_robot_rows(&call, &mut rows, &mut actual, || {
                checkpoints += 1;
                if checkpoints == stop {
                    Err("delivery cancelled")
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(failure.class, "query");
            assert_eq!(failure.code, 3);
            assert!(failure.message.contains("delivery cancelled"));
            assert_eq!(checkpoints, stop);
            let actual = String::from_utf8(actual).unwrap();
            assert!(!actual.contains("\"event\":\"result\""), "stop {stop}");
            let delivered = actual.matches("\"event\":\"row\"").count();
            assert_eq!(delivered, (stop - 1).saturating_sub(1).min(3));
            assert_eq!(rows.remaining(), 3 - delivered);
            if stop == 1 {
                assert!(actual.is_empty());
            }
        }

        let call = FnxCallSpec::weakly_connected_components();
        let graph = graph(&call, true);
        for stop in 1..=2 {
            let mut rows = cursor(&call, &graph);
            let mut actual = Vec::new();
            let mut checkpoints = 0;
            render_robot_rows(&call, &mut rows, &mut actual, || {
                checkpoints += 1;
                if checkpoints == stop {
                    Err("cancel")
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(checkpoints, stop);
            let actual = String::from_utf8(actual).unwrap();
            assert!(!actual.contains("\"event\":\"result\""));
            assert_eq!(actual.is_empty(), stop == 1);
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum OutputFault {
        None,
        Columns,
        SecondRow,
        PrefixFlush,
        Completion,
        CompletionFlush,
    }

    struct Output {
        fault: OutputFault,
        bytes: Vec<u8>,
        rows: usize,
        flushes: usize,
    }

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let contains = |marker: &[u8]| bytes.windows(marker.len()).any(|part| part == marker);
            if contains(b"\"event\":\"row\"") {
                self.rows += 1;
                if matches!(self.fault, OutputFault::SecondRow) && self.rows == 2 {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
            }
            if (matches!(self.fault, OutputFault::Columns) && contains(b"\"event\":\"columns\""))
                || (matches!(self.fault, OutputFault::Completion)
                    && contains(b"\"event\":\"result\""))
            {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if (matches!(self.fault, OutputFault::PrefixFlush) && self.flushes == 1)
                || (matches!(self.fault, OutputFault::CompletionFlush) && self.flushes == 2)
            {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            Ok(())
        }
    }

    #[test]
    fn output_failures_stop_pulls_and_never_report_success_to_the_caller() {
        let call = FnxCallSpec::weakly_connected_components();
        let graph = graph(&call, false);
        for fault in [
            OutputFault::Columns,
            OutputFault::SecondRow,
            OutputFault::PrefixFlush,
            OutputFault::Completion,
            OutputFault::CompletionFlush,
            OutputFault::None,
        ] {
            let mut rows = cursor(&call, &graph);
            let mut out = Output {
                fault,
                bytes: Vec::new(),
                rows: 0,
                flushes: 0,
            };
            let result = render_robot_rows(&call, &mut rows, &mut out, proceed);
            if matches!(fault, OutputFault::None) {
                result.unwrap();
                assert_eq!(out.flushes, 2);
            } else {
                let failure = result.unwrap_err();
                assert_eq!((failure.code, failure.class), (5, "io"), "{fault:?}");
            }
            let actual = String::from_utf8(out.bytes).unwrap();
            assert_eq!(
                actual.contains("\"event\":\"result\""),
                matches!(fault, OutputFault::None | OutputFault::CompletionFlush),
                "only a failed terminal flush may already have written the result: {fault:?}"
            );
            match fault {
                OutputFault::Columns => {
                    assert_eq!(rows.remaining(), 4);
                    assert_eq!(out.rows, 0);
                    assert_eq!(out.flushes, 0);
                }
                OutputFault::SecondRow => {
                    assert_eq!(rows.remaining(), 2);
                    assert_eq!(out.rows, 2);
                    assert_eq!(actual.matches("\"event\":\"row\"").count(), 1);
                    assert_eq!(out.flushes, 0);
                }
                _ => assert_eq!(rows.remaining(), 0),
            }
        }
    }

    fn database_path(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "fgdb-cli-completed-{name}-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        directory.join("db")
    }

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        )
    }

    async fn seed(
        commit: &CommitCx,
        path: &Path,
        vertices: &[VId],
        edges: &[(VId, VId)],
    ) -> Database {
        let mut db = Database::create(commit, path, keys()).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for vertex in vertices {
            batch.create_vertex(*vertex, vec![LabelId(1)], vec![]);
        }
        for (index, (source, target)) in edges.iter().enumerate() {
            batch.add_edge(EId(index as u128), *source, *target, vec![]);
        }
        db.write(commit, batch).await.unwrap();
        db
    }

    fn options(path: &Path, text: &str, flags: &[&str]) -> (Options, Prepared) {
        let mut arguments = vec![
            "--db",
            path.to_str().unwrap(),
            "--key-file",
            "unused",
            "--relation",
            "KNOWS=1",
            "--graph-relation",
            "KNOWS",
            text,
        ];
        arguments.extend_from_slice(flags);
        let arguments = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let options = super::super::parse(&arguments, "query").unwrap();
        let prepared = prepare(&options).unwrap();
        (options, prepared)
    }

    async fn buffered(commit: &CommitCx, path: &Path, options: &Options) -> BufferedReadView {
        let (pool, limits) = options.buffered.admission(options.budget.policy()).unwrap();
        Database::open_buffered_read_view(commit, path, keys(), pool, limits)
            .await
            .unwrap()
    }

    #[test]
    fn resident_buffered_and_sealed_delivery_preserve_collected_historical_answers() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        runtime.block_on(async {
            let commit = contexts.commit();
            let cx = contexts.query();
            let path = database_path("historical-oracle");
            let mut db = seed(
                &commit,
                &path,
                &[VId(1), VId(7), VId(1 << 100), VId(u128::MAX)],
                &[
                    (VId(1), VId(7)),
                    (VId(7), VId(1 << 100)),
                    (VId(1 << 100), VId(1)),
                ],
            )
            .await;
            let pinned = db.pinned_read_view().unwrap();
            let mut second = WriteBatch::new(RelationId(1));
            second.create_vertex(VId(99), vec![LabelId(1)], vec![]);
            second.add_edge(EId(99), VId(7), VId(99), vec![]);
            db.write(&commit, second).await.unwrap();
            let current = db.pinned_read_view().unwrap();
            for (text, direction) in [
                (
                    "CALL fnx.single_source_shortest_path_length(1) YIELD distance AS hops,vertex",
                    "directed",
                ),
                (
                    "CALL fnx.weakly_connected_components() YIELD component AS group_id,vertex",
                    "directed",
                ),
                (
                    "CALL fnx.pagerank(iterations => 3) YIELD score AS rank,vertex",
                    "directed",
                ),
                ("CALL fnx.triangles() YIELD triangles,vertex", "undirected"),
            ] {
                for cut in ["1", "2"] {
                    let flags = ["--direction", direction, "--as-of", cut];
                    let (_, prepared) = options(&path, text, &flags);
                    let incumbent = current
                        .execute_fnx(&cx, &prepared.call, prepared.read)
                        .unwrap();
                    let mut expected = Vec::new();
                    render(incumbent.clone(), true, &mut expected).unwrap();
                    let mut actual = Vec::new();
                    run(&current, &cx, &prepared, true, &mut actual).unwrap();
                    assert_eq!(actual, expected, "resident cut {cut}: {text}");
                    if cut == "1" {
                        actual.clear();
                        run(&pinned, &cx, &prepared, true, &mut actual).unwrap();
                        assert_eq!(
                            actual, expected,
                            "writer advancement must not retarget a pinned view"
                        );
                    }
                    let mut human_expected = Vec::new();
                    render(incumbent, false, &mut human_expected).unwrap();
                    let mut human_actual = Vec::new();
                    run(&current, &cx, &prepared, false, &mut human_actual).unwrap();
                    assert_eq!(human_actual, human_expected);

                    let mut mode = flags.to_vec();
                    mode.push("--buffered");
                    let (configuration, prepared) = options(&path, text, &mode);
                    let mut view = buffered(&commit, &path, &configuration).await;
                    let incumbent = view
                        .execute_fnx(&cx, &prepared.call, prepared.read)
                        .await
                        .unwrap();
                    let mut buffered_expected = Vec::new();
                    render(incumbent, true, &mut buffered_expected).unwrap();
                    assert_eq!(buffered_expected, expected);
                    actual.clear();
                    run_buffered(&mut view, &cx, &prepared, true, &mut actual)
                        .await
                        .unwrap();
                    assert_eq!(actual, buffered_expected, "buffered cut {cut}: {text}");

                    for quantum in [false, true] {
                        let mut mode = flags.to_vec();
                        mode.push("--sealed");
                        if quantum {
                            mode.extend_from_slice(&["--sealed-quantum", "1"]);
                        }
                        let (configuration, prepared) = options(&path, text, &mode);
                        let mut view = buffered(&commit, &path, &configuration).await;
                        let (memory, sealing) = prepared.sealed.unwrap();
                        let incumbent = if let Some(quantum) = prepared.quantum {
                            view.execute_fnx_sealed_cooperative(
                                &cx,
                                &prepared.call,
                                prepared.read,
                                memory,
                                sealing,
                                prepared.source_incidences,
                                quantum,
                            )
                            .await
                        } else {
                            view.execute_fnx_sealed(
                                &cx,
                                &prepared.call,
                                prepared.read,
                                memory,
                                sealing,
                                prepared.source_incidences,
                            )
                            .await
                        }
                        .unwrap();
                        let projection = view
                            .prism_sealed_projection_at(
                                &cx,
                                prepared.read.as_of.unwrap(),
                                prepared.read.selection,
                                prepared.read.projection,
                                prepared.read.projection_limits,
                                prepared.read.source_limits,
                                sealing,
                                prepared.source_incidences,
                            )
                            .await
                            .unwrap();
                        let rows = if let Some(quantum) = prepared.quantum {
                            prepared
                                .call
                                .execute_sealed_cooperative_rows(
                                    &cx,
                                    &projection,
                                    prepared.read.execution_limits,
                                    memory,
                                    quantum,
                                    asupersync::runtime::yield_now::yield_now,
                                )
                                .await
                        } else {
                            prepared.call.execute_sealed_rows(
                                &cx,
                                &projection,
                                prepared.read.execution_limits,
                                memory,
                            )
                        }
                        .unwrap();
                        assert_eq!(rows.certificate(), &incumbent.analytics.certificate);
                        let mut sealed_expected = Vec::new();
                        render(incumbent, true, &mut sealed_expected).unwrap();
                        assert_eq!(sealed_expected, expected);
                        actual.clear();
                        run_buffered(&mut view, &cx, &prepared, true, &mut actual)
                            .await
                            .unwrap();
                        assert_eq!(
                            actual, sealed_expected,
                            "buffered sealed quantum {quantum} cut {cut}: {text}"
                        );
                        actual.clear();
                        run_sealed(
                            &commit,
                            &cx,
                            &configuration,
                            keys(),
                            &prepared,
                            true,
                            &mut actual,
                        )
                        .await
                        .unwrap();
                        assert_eq!(
                            actual, sealed_expected,
                            "sealed reopen quantum {quantum} cut {cut}: {text}"
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn source_and_kernel_refusals_precede_columns_in_every_delivery_mode() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        runtime.block_on(async {
            let commit = contexts.commit();
            let cx = contexts.query();
            let path = database_path("refusal-oracle");
            let db = seed(&commit, &path, &[VId(1), VId(2)], &[(VId(1), VId(2))]).await;
            let resident = db.pinned_read_view().unwrap();
            let text = "CALL fnx.weakly_connected_components() YIELD vertex,component";
            for refused in 0..7 {
                let (configuration, mut prepared) = options(&path, text, &[]);
                match refused {
                    0 => prepared.read.source_limits.max_work_units = 0,
                    1 => prepared.read.source_limits.max_staging_bytes = 0,
                    2 => prepared.read.projection_limits.max_workspace_bytes = 0,
                    3 => prepared.read.execution_limits.max_result_rows = 0,
                    4 => prepared.read.execution_limits.max_estimated_work = 0,
                    5 => prepared.read.as_of = Some(CommitSeq(99)),
                    _ => {
                        prepared.read.selection.weight = FnxWeightSpec::Property {
                            key: PropertyKeyId(9),
                            missing: MissingWeightPolicy::Reject,
                        }
                    }
                }
                let expected = resident
                    .execute_fnx(&cx, &prepared.call, prepared.read)
                    .unwrap_err();
                let mut out = Vec::new();
                let actual = run(&resident, &cx, &prepared, true, &mut out).unwrap_err();
                assert!(out.is_empty(), "resident refusal {refused}");
                assert_eq!((actual.code, actual.class), (3, "query"));
                assert_eq!(actual.message, expected.to_string());

                let mut view = buffered(&commit, &path, &configuration).await;
                let expected = view
                    .execute_fnx(&cx, &prepared.call, prepared.read)
                    .await
                    .unwrap_err();
                let actual = run_buffered(&mut view, &cx, &prepared, true, &mut out)
                    .await
                    .unwrap_err();
                assert!(out.is_empty(), "buffered refusal {refused}");
                assert_eq!((actual.code, actual.class), (3, "query"));
                assert_eq!(actual.message, expected.to_string());
            }

            for quantum in [false, true] {
                for flag in [
                    "--sealed-result-bytes",
                    "--sealed-kernel-bytes",
                    "--graph-staging-bytes",
                    "--sealed-source-incidences",
                ] {
                    let mut flags = vec!["--sealed", flag, "0"];
                    if quantum {
                        flags.extend_from_slice(&["--sealed-quantum", "1"]);
                    }
                    let (configuration, prepared) = options(&path, text, &flags);
                    let mut view = buffered(&commit, &path, &configuration).await;
                    let mut out = Vec::new();
                    let actual = run_buffered(&mut view, &cx, &prepared, true, &mut out)
                        .await
                        .unwrap_err();
                    assert!(out.is_empty(), "buffered sealed {flag}, quantum {quantum}");
                    assert_eq!((actual.code, actual.class), (3, "query"));
                    let mut incumbent_output = Vec::new();
                    let expected = run_sealed(
                        &commit,
                        &cx,
                        &configuration,
                        keys(),
                        &prepared,
                        false,
                        &mut incumbent_output,
                    )
                    .await
                    .unwrap_err();
                    assert!(incumbent_output.is_empty());
                    let actual = run_sealed(
                        &commit,
                        &cx,
                        &configuration,
                        keys(),
                        &prepared,
                        true,
                        &mut out,
                    )
                    .await
                    .unwrap_err();
                    assert!(out.is_empty(), "sealed reopen {flag}, quantum {quantum}");
                    assert_eq!(
                        (actual.code, actual.class, actual.message),
                        (expected.code, expected.class, expected.message)
                    );
                }
            }
        });
    }

    /// Same-invocation incumbent comparison over an already admitted durable
    /// view. Includes projection lookup, kernel, certification, row delivery and
    /// result destruction. It excludes database create/open and ingest, and is
    /// not a comparison against GAPBS's kernel-only runner.
    #[test]
    #[ignore = "paired query plus robot-delivery measurement; run with --ignored --nocapture"]
    fn paired_robot_analytics_query_and_delivery() {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        runtime.block_on(async {
            let commit = contexts.commit();
            let cx = contexts.query();
            for vertices in [8_usize, 4096, 32768] {
                let path = database_path(&format!("paired-{vertices}"));
                let ids = (1..=vertices).map(|id| VId(id as u128)).collect::<Vec<_>>();
                let mut edges = (1..vertices)
                    .map(|id| (VId(id as u128), VId(id as u128 + 1)))
                    .collect::<Vec<_>>();
                for id in (8..vertices.saturating_sub(1)).step_by(8) {
                    edges.push((VId(id as u128), VId(id as u128 + 2)));
                }
                let db = seed(&commit, &path, &ids, &edges).await;
                let view = db.pinned_read_view().unwrap();
                for (name, text) in [
                    ("bfs", "CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance"),
                    ("wcc", "CALL fnx.weakly_connected_components() YIELD component,vertex"),
                    ("pagerank", "CALL fnx.pagerank(iterations => 5) YIELD score,vertex"),
                ] {
                    let (_, prepared) = options(&path, text, &[]);
                    let incumbent = view.execute_fnx(&cx, &prepared.call, prepared.read).unwrap();
                    let mut expected = Vec::new();
                    render(incumbent, true, &mut expected).unwrap();
                    let mut actual = Vec::new();
                    run(&view, &cx, &prepared, true, &mut actual).unwrap();
                    assert_eq!(actual, expected, "paired oracle {name}, {vertices}");
                    // Both paths now have the identical projection cache hit.
                    // Alternate their order so one route does not always get
                    // the earlier allocation/cache/CPU scheduling position.
                    let mut elapsed = [Vec::new(), Vec::new()];
                    for round in 0..9 {
                        for route in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
                            let start = std::time::Instant::now();
                            let mut bytes = Vec::new();
                            if route == 0 {
                                let collected = view.execute_fnx(&cx, &prepared.call, prepared.read).unwrap();
                                render(collected, true, &mut bytes).unwrap();
                            } else {
                                run(&view, &cx, &prepared, true, &mut bytes).unwrap();
                            }
                            bytes.flush().unwrap();
                            elapsed[route].push(start.elapsed());
                            // Every measured invocation is checked outside the
                            // clock against the complete incumbent NDJSON.
                            assert_eq!(bytes, expected, "{name}, {vertices}, route {route}, round {round}");
                        }
                    }
                    for samples in &mut elapsed { samples.sort_unstable(); }
                    let incumbent = elapsed[0][4].as_nanos();
                    let completed = elapsed[1][4].as_nanos();
                    eprintln!(
                        "paired_robot_analytics algorithm={name} vertices={vertices} edges={} bytes={} samples=9 collected_median_ns={incumbent} cursor_median_ns={completed} collected_p90_ns={} cursor_p90_ns={} ratio={:.4}",
                        edges.len(), expected.len(), elapsed[0][8].as_nanos(), elapsed[1][8].as_nanos(),
                        incumbent as f64 / completed as f64,
                    );
                }
            }
        });
    }
}
