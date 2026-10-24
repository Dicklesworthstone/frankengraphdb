//! `query 'CALL fnx.<procedure>(...) YIELD ...'`: registered Prism analytics
//! over the admitted generation. The default uses the decoded adapter on a
//! read-only open; --buffered builds only the selected decoded projection
//! through authenticated checkpoint cursors; --sealed explicitly seals one
//! pinned source in one read-only pass and executes compressed cursors.
//!
//! The graph a procedure sees is an EXPLICIT projection chosen by flags, never
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
use super::{Failure, Options, decimal_usize, float_text, render_rows_iter};
use asupersync::fs::Vfs;
use fgdb::{BufferedReadView, Database, DatabaseKeys, EmbeddedReadView, SealedProjectionOpenError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxArgument, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadOptions,
    FnxReadResult, FnxSelection, FnxValue, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy,
    ProjectionSpec, SelfLoopPolicy,
};
use fgdb_strata::{store::RootReadLimits, tiered::sealed::SealedLimits};
use fgdb_types::{CommitCx, CommitSeq, QueryCx, VId};
use std::io::Write;

/// Is this `query` text a standalone Prism call? `CALL` is a keyword (any
/// case); the procedure namespace `fnx.` is case-sensitive, like the
/// registry. A call that continues past its YIELD list (WHERE, WITH, MATCH,
/// RETURN, ...) or supplies a native projection is an ordinary native read.
/// Kernel-only named arguments use the same standalone route as positional
/// arguments, bound against the actual typed parameters before opening storage.
pub(super) fn is_call(text: &str, raw_params: &[(String, String)]) -> bool {
    if fgdb_gql::starts_fnx_graph_projection(text) {
        // The shared lexer also recognizes every named argument envelope.
        // Only the registry's binder can distinguish a complete kernel call
        // from native projection fields/maps or a continuing read pipeline.
        return arguments(raw_params)
            .is_ok_and(|parameters| FnxCallSpec::bind(text, &parameters).is_ok());
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
}

impl ProjectionFlags {
    pub(super) const FLAGS: [&str; 16] = [
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

    fn has_sealed_limits(&self) -> bool {
        self.sealed_kernel_bytes.is_some()
            || self.sealed_result_bytes.is_some()
            || self.sealed_image_bytes.is_some()
            || self.sealed_property_bytes.is_some()
            || self.sealed_rows.is_some()
            || self.sealed_incidences.is_some()
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
            _ => Err(Failure::usage("unknown analytics flag")),
        }
    }

    fn options(&self, bindings: &Options) -> Result<FnxReadOptions, Failure> {
        let lookup =
            |name: &Option<String>, table: &std::collections::BTreeMap<String, u32>, what: &str| {
                name.as_ref()
                    .map(|name| {
                        table.get(name).map(|id| u64::from(*id)).ok_or_else(|| {
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
    if options.buffered.enabled() {
        if sealed.is_some() {
            return Err(Failure::usage(
                "--buffered cannot be combined with --sealed",
            ));
        }
        if options.budget.records.is_some()
            || options.budget.rows.is_some()
            || options.budget.scratch.is_some()
        {
            return Err(Failure::usage(
                "native --max-snapshot-records, --max-result-rows and --max-scratch-entries \
                 do not apply to buffered analytics; use --buffer-*, --graph-staging-bytes \
                 and --graph-workspace-bytes limits",
            ));
        }
    }
    if sealed.is_some() {
        // These checks must precede the writable open needed for the temporary
        // seal. Unsupported algorithms never fall back to decoded adjacency.
        if read.selection.relation.is_none() {
            return Err(Failure::usage("--sealed requires --graph-relation"));
        }
        if options.budget != super::QueryBudget::default() {
            return Err(Failure::usage(
                "native --max-snapshot-records, --max-result-rows, --max-work-units \
                 and --max-scratch-entries do not apply to --sealed; use --sealed-* limits",
            ));
        }
        call.validate_sealed_projection(read.projection.directedness)
            .map_err(Failure::query)?;
    }
    Ok(Prepared { call, read, sealed })
}

pub(super) fn run(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let result = view
        .execute_fnx(cx, &prepared.call, prepared.read)
        .map_err(Failure::query)?;
    render(result, robot, out)
}

/// The extent cache and graph source use the buffered view's allowances;
/// selected staging, decoded CSR and execution retain their Prism limits.
pub(super) async fn run_buffered<V: Vfs + Clone>(
    view: &mut BufferedReadView<V>,
    cx: &QueryCx,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let result = view
        .execute_fnx(cx, &prepared.call, prepared.read)
        .await
        .map_err(super::execution_failure)?;
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
    let mut root_limits = RootReadLimits::default();
    root_limits.max_incidences = root_limits.max_incidences.max(sealing.max_incidences);
    let graph = Database::open_prism_sealed_projection(
        open_cx,
        cx,
        &options.db,
        keys,
        prepared.read,
        sealing,
        root_limits,
    )
    .await
    .map_err(|error| match error {
        SealedProjectionOpenError::Open(error) => super::open_failure(error),
        SealedProjectionOpenError::Read(error) => super::execution_failure(error),
        SealedProjectionOpenError::RecoveryRequired => Failure::open(error),
    })?;
    let analytics = prepared
        .call
        .execute_sealed(cx, &graph, prepared.read.execution_limits, memory)
        .map_err(super::execution_failure)?;
    render(
        FnxReadResult::bind_selection(analytics, prepared.read.selection),
        robot,
        out,
    )
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
    use super::is_call;

    #[test]
    fn sealed_core_and_eigenvector_preflight_accept_only_undirected_projections() {
        for call in [
            "CALL fnx.core_number() YIELD core AS level, node AS id",
            "CALL fnx.eigenvector_centrality() YIELD score AS rank, node AS id",
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
    fn projection_envelopes_use_the_native_host_and_positional_calls_keep_their_route() {
        assert!(is_call(
            "CALL fnx.connected_components() YIELD vertex, component",
            &[],
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
            assert!(!is_call(text, &[]), "{text}");
        }
    }

    #[test]
    fn kernel_named_calls_keep_the_standalone_route_with_actual_rebound_parameters() {
        for text in [
            "CALL fnx.pagerank(iterations => 2, weighted => false) YIELD vertex,score",
            "/* leading */ cAlL fnx.pagerank ( weighted => false, iterations /* count */ => 2 ) YIELD score AS rank",
            "CALL fnx.pagerank(alpha => 0.85, max_iter => 100, tol => 1e-6, weighted => false)",
            "CALL fnx.pagerank(l1_tol => 1e-9, max_iter => 1000, weighted => false)",
        ] {
            assert!(is_call(text, &[]), "{text}");
        }
        let text =
            "CALL fnx.pagerank(iterations => $steps, weighted => $weighted) YIELD vertex,score";
        for steps in [1, 2, 5] {
            let raw = [
                ("steps".to_owned(), format!("int:{steps}")),
                ("weighted".to_owned(), "bool:false".to_owned()),
            ];
            assert!(is_call(text, &raw), "steps={steps}");
            assert!(!is_call(&format!("{text} RETURN vertex,score"), &raw));
        }
        assert!(!is_call(text, &[]), "routing must not invent parameters");
        assert!(is_call(
            "CALL fnx.pagerank(l1_tol => $accuracy) YIELD vertex,score",
            &[("accuracy".to_owned(), "float:1e-9".to_owned())],
        ));
        assert!(!is_call(
            "CALL fnx.pagerank(l1_tol => 1e-9, tol => 1e-6) YIELD vertex,score",
            &[],
        ));
        for value in ["int:0", "int:-1", "float:2", "null", "int:overflow"] {
            let raw = [
                ("steps".to_owned(), value.to_owned()),
                ("weighted".to_owned(), "bool:false".to_owned()),
            ];
            assert!(!is_call(text, &raw), "{value}");
        }
        assert!(is_call(
            "CALL fnx.single_source_shortest_path_length(source => $source, cutoff => 2) YIELD vertex,distance",
            &[("source".to_owned(), format!("vertex:{}", u128::MAX))],
        ));
        assert!(!is_call(
            "CALL fnx.pagerank(label => $label, iterations => 2) YIELD vertex,score",
            &[("label".to_owned(), "text:Person".to_owned())],
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
