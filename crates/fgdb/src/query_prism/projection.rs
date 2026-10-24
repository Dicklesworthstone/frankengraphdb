//! Query-bound Prism projections. Schema names are resolved by GQL before
//! this layer; no catalog lookup or authorization is performed here.

use super::*;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    ProcedureGraphDirection, ProcedureGraphWeight, ProcedureMissingWeight, ProcedureParallelEdges,
    ProcedureSelfLoops,
};
use fgdb_prism::{FnxExecutionError, FnxGraphKind, FnxWeightSpec, MissingWeightPolicy};

/// A registered kernel and its inseparable input recipe. Existing executors
/// borrow the kernel, and obtain options from this complete binding.
pub(crate) struct BoundProcedure {
    spec: FnxCallSpec,
    options: FnxReadOptions,
}

impl BoundProcedure {
    /// Keep the query's selected graph and policies while pinning its host's
    /// snapshot. Transaction sources meter records and work independently.
    pub(crate) fn options_at(&self, as_of: CommitSeq) -> FnxReadOptions {
        let mut options = self.options;
        options.as_of = Some(as_of);
        options
    }
}

impl core::ops::Deref for BoundProcedure {
    type Target = FnxCallSpec;

    fn deref(&self) -> &Self::Target {
        &self.spec
    }
}

fn invalid<C>(index: usize, message: &'static str) -> GqlQueryError<GqlError, C> {
    refuse(ProcedureError::Bind(FnxCallError {
        site: FnxCallSite::Argument(index),
        kind: FnxBindErrorKind::InvalidArgument(message),
    }))
}

fn text<C>(value: &GraphValue, index: usize) -> Result<&str, GqlQueryError<GqlError, C>> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Text(value)) => Ok(value.as_str()),
        _ => Err(invalid(index, "a projection policy must be text")),
    }
}

fn symbol_id<T: TryFrom<i64>, C>(
    value: &GraphValue,
    index: usize,
) -> Result<T, GqlQueryError<GqlError, C>> {
    let GraphValue::Scalar(CanonicalScalar::Int(value)) = value else {
        return Err(invalid(
            index,
            "a catalog-bound projection identity is required",
        ));
    };
    T::try_from(*value).map_err(|_| invalid(index, "projection identity is out of range"))
}

/// Named calls select registered kernel arguments alongside projection fields,
/// or supply the original positional kernel list through `args`. Mixing those
/// two kernel forms refuses instead of silently choosing one.
pub(crate) fn bind_procedure<C>(
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
) -> Result<BoundProcedure, GqlQueryError<GqlError, C>> {
    const FIELDS: [&str; 8] = [
        "args",
        "label",
        "relation",
        "weight",
        "direction",
        "parallel",
        "self_loops",
        "missing",
    ];
    let names = call.argument_names();
    let named = !names.is_empty();
    if named && call.graph_projection().is_some() {
        return Err(invalid(
            0,
            "inline and named projections cannot be combined",
        ));
    }
    let mut kernel_arguments = if named { &[][..] } else { arguments };
    let mut args_index = None;
    let mut named_kernel_arguments = Vec::new();
    let mut kernel_positions = Vec::new();
    if named {
        if names.len() != arguments.len() {
            return Err(invalid(0, "projection argument names and values disagree"));
        }
        let mut seen = 0_u16;
        for (index, (name, value)) in names.iter().zip(arguments).enumerate() {
            let Some(field) = FIELDS.iter().position(|field| *field == name.as_str()) else {
                // The kernel registry validates names, duplicates and exact
                // argument domains after the projection recipe is separated.
                named_kernel_arguments.push((name.as_str(), value));
                kernel_positions.push(index);
                continue;
            };
            let bit = 1_u16 << field;
            if seen & bit != 0 {
                return Err(invalid(index, "duplicate Prism projection argument"));
            }
            seen |= bit;
            if name == "args" {
                let GraphValue::List(values) = value else {
                    return Err(invalid(index, "args must be a list of kernel arguments"));
                };
                kernel_arguments = values;
                args_index = Some(index);
            }
        }
    }
    if args_index.is_some() && !named_kernel_arguments.is_empty() {
        return Err(invalid(
            kernel_positions[0],
            "args cannot be combined with named kernel arguments",
        ));
    }
    let bound = if named_kernel_arguments.is_empty() {
        bind_kernel(call, kernel_arguments)
    } else {
        bind_named_kernel(call, &named_kernel_arguments)
    };
    let spec = bound.map_err(|error| match error {
        GqlQueryError::Source(GqlError::Procedure(ProcedureError::Bind(mut error))) => {
            if let FnxCallSite::Argument(index) = error.site {
                if let Some(args_index) = args_index {
                    error.site = FnxCallSite::Argument(args_index);
                } else if let Some(&original) = kernel_positions.get(index) {
                    error.site = FnxCallSite::Argument(original);
                }
            }
            refuse(ProcedureError::Bind(error))
        }
        other => other,
    })?;
    let mut options = FnxReadOptions::whole_graph_for(&spec, None);
    let mut weight = None;
    let mut missing = MissingWeightPolicy::Reject;
    let mut missing_index = None;
    for (index, (name, value)) in names.iter().zip(arguments).enumerate() {
        match name.as_str() {
            "args" => {}
            "label" => options.selection.vertex_label = Some(LabelId(symbol_id(value, index)?)),
            "relation" => options.selection.relation = Some(RelationId(symbol_id(value, index)?)),
            "weight" => weight = Some(PropertyKeyId(symbol_id(value, index)?)),
            "direction" => {
                options.projection.directedness = match text(value, index)? {
                    "directed" => Directedness::Directed,
                    "reversed" => Directedness::Reversed,
                    "undirected" => Directedness::Undirected,
                    _ => return Err(invalid(index, "unknown projection direction")),
                };
            }
            "parallel" => {
                options.projection.parallel_edges = match text(value, index)? {
                    "reject" => ParallelEdgePolicy::Reject,
                    "collapse" | "collapse_unit" => ParallelEdgePolicy::CollapseUnit,
                    "min" => ParallelEdgePolicy::Minimum,
                    "max" => ParallelEdgePolicy::Maximum,
                    "sum" => ParallelEdgePolicy::Sum,
                    _ => return Err(invalid(index, "unknown parallel-edge policy")),
                };
            }
            "self_loops" => {
                options.projection.self_loops = match text(value, index)? {
                    "keep" => SelfLoopPolicy::Keep,
                    "drop" => SelfLoopPolicy::Drop,
                    "reject" => SelfLoopPolicy::Reject,
                    _ => return Err(invalid(index, "unknown self-loop policy")),
                };
            }
            "missing" => {
                missing_index = Some(index);
                missing = match text(value, index)? {
                    "reject" => MissingWeightPolicy::Reject,
                    "unit" => MissingWeightPolicy::Unit,
                    "zero" => MissingWeightPolicy::Zero,
                    _ => return Err(invalid(index, "unknown missing-weight policy")),
                };
            }
            // Already admitted as a registered named kernel parameter above.
            _ => {}
        }
    }
    if let Some(key) = weight {
        options.selection.weight = FnxWeightSpec::Property { key, missing };
    } else if let Some(index) = missing_index {
        return Err(invalid(
            index,
            "missing requires an explicit weight property",
        ));
    }
    if let Some(projection) = call.graph_projection() {
        options.selection.vertex_label = projection.vertex_label;
        options.selection.relation = projection.relation;
        options.selection.weight = match projection.weight {
            ProcedureGraphWeight::Unit => FnxWeightSpec::Unit,
            ProcedureGraphWeight::Property { key, missing } => FnxWeightSpec::Property {
                key,
                missing: match missing {
                    ProcedureMissingWeight::Reject => MissingWeightPolicy::Reject,
                    ProcedureMissingWeight::Unit => MissingWeightPolicy::Unit,
                    ProcedureMissingWeight::Zero => MissingWeightPolicy::Zero,
                },
            },
        };
        if let Some(direction) = projection.direction {
            options.projection.directedness = match direction {
                ProcedureGraphDirection::Directed => Directedness::Directed,
                ProcedureGraphDirection::Reversed => Directedness::Reversed,
                ProcedureGraphDirection::Undirected => Directedness::Undirected,
            };
        }
        options.projection.parallel_edges = match projection.parallel_edges {
            ProcedureParallelEdges::Reject => ParallelEdgePolicy::Reject,
            ProcedureParallelEdges::CollapseUnit => ParallelEdgePolicy::CollapseUnit,
            ProcedureParallelEdges::Minimum => ParallelEdgePolicy::Minimum,
            ProcedureParallelEdges::Maximum => ParallelEdgePolicy::Maximum,
            ProcedureParallelEdges::Sum => ParallelEdgePolicy::Sum,
        };
        options.projection.self_loops = match projection.self_loops {
            ProcedureSelfLoops::Keep => SelfLoopPolicy::Keep,
            ProcedureSelfLoops::Drop => SelfLoopPolicy::Drop,
            ProcedureSelfLoops::Reject => SelfLoopPolicy::Reject,
        };
    }
    // Reject incompatible graph laws before any host scans its source.
    // Reversal still supplies a directed graph; only Undirected changes kind.
    let required = spec.signature().graph_kind;
    let compatible = match required {
        FnxGraphKind::Any => true,
        FnxGraphKind::Directed => options.projection.directedness != Directedness::Undirected,
        FnxGraphKind::Undirected => options.projection.directedness == Directedness::Undirected,
    };
    if !compatible {
        return Err(procedure_failure(FnxReadError::Execution(
            FnxExecutionError::GraphKind { required },
        )));
    }
    Ok(BoundProcedure { spec, options })
}

/// Pin the projection to the enclosing read and retain its existing ceilings.
pub(crate) fn procedure_options(
    spec: &BoundProcedure,
    as_of: CommitSeq,
    remaining: GqlQueryPolicy,
) -> FnxReadOptions {
    let mut options = kernel_options(&spec.spec, as_of, remaining);
    options.selection = spec.options.selection;
    options.projection = spec.options.projection;
    options
}
