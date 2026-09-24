//! `query 'CALL fnx.<procedure>(...) YIELD ...'`: registered Prism analytics
//! over the admitted generation, through the embedded `Database::call_fnx`.
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
use super::{Failure, Options, float_text, render_rows};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxArgument, FnxExecutionLimits, FnxParameters, FnxReadOptions, FnxSelection,
    FnxSourceLimits, FnxValue, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy,
};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::io::Write;

/// Admission for one analytics call. Finite, explicit, and documented in
/// `fgdb help`; these bound logical work and staging, not allocator bytes.
const SOURCE_LIMITS: FnxSourceLimits = FnxSourceLimits {
    max_work_units: 100_000_000,
    max_scratch_entries: 10_000_000,
    max_staging_bytes: 1 << 30,
};
const PROJECTION_LIMITS: ProjectionLimits = ProjectionLimits {
    max_vertices: 10_000_000,
    max_input_edges: 100_000_000,
    max_adjacency_entries: 200_000_000,
    max_workspace_bytes: 4 << 30,
};
const EXECUTION_LIMITS: FnxExecutionLimits = FnxExecutionLimits {
    max_iterations: 100_000,
    max_result_rows: 10_000_000,
    max_estimated_work: 1 << 40,
};

/// Is this `query` text a Prism call? `CALL` is a keyword (any case); the
/// procedure namespace `fnx.` is case-sensitive, like the registry.
pub(super) fn is_call(text: &str) -> bool {
    let text = text.trim_start();
    text.get(..4)
        .is_some_and(|word| word.eq_ignore_ascii_case("CALL"))
        && text[4..].starts_with(char::is_whitespace)
        && text[4..].trim_start().starts_with("fnx.")
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
}

impl ProjectionFlags {
    pub(super) const FLAGS: [&str; 8] = [
        "--graph-label",
        "--graph-relation",
        "--weight",
        "--missing-weight",
        "--direction",
        "--parallel-edges",
        "--self-loops",
        "--as-of",
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
        Ok(FnxReadOptions {
            as_of: self.as_of,
            selection: FnxSelection {
                vertex_label: lookup(&self.label, &bindings.labels, "label")?.map(LabelId),
                relation: lookup(&self.relation, &bindings.relations, "relation")?.map(RelationId),
                weight,
            },
            projection: ProjectionSpec {
                directedness: self.direction.unwrap_or(Directedness::Directed),
                // No implicit multigraph collapse: a simple graph projects
                // unchanged and a multigraph is refused until a law is chosen.
                parallel_edges: self.parallel.unwrap_or(ParallelEdgePolicy::Reject),
                self_loops: self.self_loops.unwrap_or(SelfLoopPolicy::Keep),
            },
            source_limits: SOURCE_LIMITS,
            projection_limits: PROJECTION_LIMITS,
            execution_limits: EXECUTION_LIMITS,
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
    parameters: FnxParameters,
    read: FnxReadOptions,
}

pub(super) fn prepare(options: &Options) -> Result<Prepared, Failure> {
    Ok(Prepared {
        parameters: arguments(&options.raw_params)?,
        read: options.fnx.options(options)?,
    })
}

pub(super) fn run<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    text: &str,
    prepared: Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let result = db
        .call_fnx(cx, text, &prepared.parameters, prepared.read)
        .map_err(Failure::query)?;
    let analytics = result.analytics;
    let cells = analytics
        .rows
        .iter()
        .map(|row| row.iter().map(|value| cell(*value, robot)).collect())
        .collect();
    render_rows(
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
