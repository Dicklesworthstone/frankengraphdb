//! Query-bound Prism projections. Schema names are resolved by GQL before
//! this layer; no catalog lookup or authorization is performed here.

use super::*;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{FnxWeightSpec, MissingWeightPolicy};

pub(crate) struct BoundProcedure {
    spec: FnxCallSpec,
    options: FnxReadOptions,
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
        return Err(invalid(index, "a catalog-bound projection identity is required"));
    };
    T::try_from(*value).map_err(|_| invalid(index, "projection identity is out of range"))
}

/// Positional calls retain their existing signature. Named calls supply the
/// positional kernel list through args, alongside explicit projection fields.
pub(crate) fn bind_procedure<C>(
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
) -> Result<BoundProcedure, GqlQueryError<GqlError, C>> {
    const FIELDS: [&str; 8] = [
        "args", "label", "relation", "weight", "direction", "parallel", "self_loops", "missing",
    ];
    let names = call.argument_names();
    let named = !names.is_empty();
    let mut kernel_arguments = if named { &[][..] } else { arguments };
    let mut args_index = None;
    if named {
        if names.len() != arguments.len() {
            return Err(invalid(0, "projection argument names and values disagree"));
        }
        let mut seen = 0_u16;
        for (index, (name, value)) in names.iter().zip(arguments).enumerate() {
            let Some(field) = FIELDS.iter().position(|field| *field == name.as_str()) else {
                return Err(invalid(index, "unknown Prism projection argument"));
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
    let mut supplied = Vec::with_capacity(kernel_arguments.len());
    for (index, value) in kernel_arguments.iter().enumerate() {
        let argument = match value {
            GraphValue::Scalar(CanonicalScalar::Null) => FnxArgument::Null,
            GraphValue::Scalar(CanonicalScalar::Bool(value)) => FnxArgument::Boolean(*value),
            GraphValue::Scalar(CanonicalScalar::Int(value)) => FnxArgument::Integer(*value),
            GraphValue::Scalar(CanonicalScalar::Float(value)) => FnxArgument::Float(value.get()),
            GraphValue::Vertex(vertex) => FnxArgument::Vertex(*vertex),
            _ => return Err(invalid(args_index.unwrap_or(index), "no Prism argument domain")),
        };
        supplied.push(argument);
    }
    let yields: Vec<(&str, &str)> = call
        .outputs()
        .iter()
        .map(|name| (name.as_str(), name.as_str()))
        .collect();
    let spec = FnxCallSpec::from_arguments(call.namespace(), call.name(), &supplied, &yields)
        .map_err(|mut error| {
            if let Some(index) = args_index
                && matches!(error.site, FnxCallSite::Argument(_))
            {
                error.site = FnxCallSite::Argument(index);
            }
            refuse(ProcedureError::Bind(error))
        })?;
    for &index in call.vertex_outputs() {
        let vertex = spec
            .outputs()
            .get(index)
            .is_some_and(|column| matches!(column.field, FnxOutput::Vertex | FnxOutput::Component));
        if !vertex {
            return Err(refuse(ProcedureError::Bind(FnxCallError {
                site: FnxCallSite::Yield(index),
                kind: FnxBindErrorKind::Expected(
                    "a vertex output where the statement reads a vertex",
                ),
            })));
        }
    }
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
            _ => return Err(invalid(index, "unknown Prism projection argument")),
        }
    }
    if let Some(key) = weight {
        options.selection.weight = FnxWeightSpec::Property { key, missing };
    } else if let Some(index) = missing_index {
        return Err(invalid(index, "missing requires an explicit weight property"));
    }
    Ok(BoundProcedure { spec, options })
}

/// Pin the projection to the enclosing read and retain its existing ceilings.
pub(crate) fn procedure_options(
    spec: &BoundProcedure,
    as_of: CommitSeq,
    remaining: GqlQueryPolicy,
) -> FnxReadOptions {
    let mut options = spec.options;
    options.as_of = Some(as_of);
    if let Some(records) = remaining.rows.max_snapshot_records() {
        options.source_limits.max_work_units = options.source_limits.max_work_units.min(records);
    }
    options.execution_limits.max_estimated_work = options
        .execution_limits
        .max_estimated_work
        .min(usize::try_from(remaining.evaluator.max_work_units).unwrap_or(usize::MAX));
    options
}
