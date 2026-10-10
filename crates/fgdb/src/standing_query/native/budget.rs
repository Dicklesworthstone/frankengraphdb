//! A conservative pre-admission footprint for the native maintained compiler.
//! This counts its private nodes, not query result rows. The compiler remains
//! the sole admission authority; this pass never reads a graph or registers a
//! handle. Extra terminal-window reservations are intentionally conservative.

use super::*;
use fgdb_gql::PreparedGraphSet;

#[derive(Clone, Copy)]
struct Footprint {
    nodes: u64,
    source_width: u64,
}

impl Footprint {
    fn append(self, count: u64) -> Result<Self, StandingQueryError> {
        Ok(Self {
            nodes: self
                .nodes
                .checked_add(count)
                .ok_or(StandingQueryError::Unsupported)?,
            source_width: self.source_width,
        })
    }

    fn combine(self, other: Self, count: u64) -> Result<Self, StandingQueryError> {
        Self {
            nodes: self
                .nodes
                .checked_add(other.nodes)
                .ok_or(StandingQueryError::Unsupported)?,
            source_width: self.source_width.max(other.source_width),
        }
        .append(count)
    }
}

fn aggregate_width(definition: &fgdb_gql::PreparedGraphAggregate) -> u64 {
    super::super::aggregate::subscription_input_width(definition)
        .and_then(|width| u64::try_from(width).ok())
        .unwrap_or(fgdb_gql::algebra::MAX_PATTERN_BINDINGS as u64)
}

fn pattern_width(
    pattern: &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
) -> u64 {
    pattern
        .incremental_row_source_definition()
        .as_ref()
        .map_or(fgdb_gql::algebra::MAX_PATTERN_BINDINGS as u64, aggregate_width)
}

fn circuit(
    cx: &QueryCx,
    query: &PreparedGraphSet,
) -> Result<Footprint, StandingQueryError> {
    cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
    // Match Staging::compile's dispatch order. A complete source-free subtree
    // is folded into one immutable node, even if its syntax contains joins.
    if query.operand_count() == 0 {
        return Ok(Footprint { nodes: 1, source_width: 0 });
    }
    if let Some((input, _, _, _)) = query.incremental_ordered_window() {
        return circuit(cx, &input)?.append(1);
    }
    if let Some((input, _)) = query
        .incremental_window()
        .map_err(StandingQueryError::WindowSchema)?
    {
        return circuit(cx, &input)?.append(1);
    }
    if let Some(pattern) = query.incremental_pattern() {
        return Ok(Footprint { nodes: 1, source_width: pattern_width(pattern) });
    }
    if let Some(input) = query.incremental_scope() {
        return circuit(cx, input);
    }
    if let Some((input, _, _)) = query.incremental_projection() {
        return circuit(cx, input)?.append(1);
    }
    if let Some((left, right, _, _)) = query.incremental_selected_join_with_control(
        &mut |_| cx.checkpoint().map_err(StandingQueryError::Interrupted),
    )? {
        // A selected join restores its public schema with a second node.
        return circuit(cx, left)?.combine(circuit(cx, right)?, 2);
    }
    if let Some((input, _)) = query.incremental_filter() {
        return circuit(cx, input)?.append(1);
    }
    if let Some((input, _, _)) = query.incremental_unwind() {
        return circuit(cx, input)?.append(1);
    }
    if let Some((left, right, _)) = query.incremental_join() {
        return circuit(cx, left)?.combine(circuit(cx, right)?, 1);
    }
    if let Some((left, right)) = query.incremental_cross_join() {
        return circuit(cx, left)?.combine(circuit(cx, right)?, 1);
    }
    if let Some((_, _, left, right)) = query.incremental_binary() {
        return circuit(cx, left)?.combine(circuit(cx, right)?, 1);
    }
    Err(StandingQueryError::Unsupported)
}

impl PreparedNativeRead {
    /// Return an upper bound on private maintained nodes and the largest
    /// admitted source binding width (zero for a source-free definition). Binding and this compiler-shaped
    /// prepass run before any source scan or registry mutation. The bound is
    /// for one registration or one maintenance tick, not a lifetime allowance.
    ///
    /// Every accepted set compiler branch is counted, including its private
    /// selected-join projection and an optional terminal rank-restoring node.
    /// Unsupported descendants refuse even beneath a zero selected limit.
    pub fn subscription_footprint(
        &self,
        cx: &QueryCx,
        params: &GqlParameters,
    ) -> Result<(u64, u64), StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        let footprint = match self {
            Self::Set(prepared) => {
                let bound = prepared
                    .bind_parameters(params)
                    .map_err(|error| prepare_error(QueryError::SetText(error)))?;
                if bound.operand_count() != 0
                    && bound.incremental_result_order().is_none()
                    && !bound.incremental_window_sequence_compatible()
                {
                    return Err(StandingQueryError::Unsupported);
                }
                let footprint = circuit(cx, &bound)?;
                // compile_root may append a final window after the recursive
                // compiler. Reserving it even when unnecessary cannot exceed
                // the caller's total grant.
                if footprint.source_width != 0 { footprint.append(1)? } else { footprint }
            }
            Self::Pattern(prepared) => {
                let bound = prepared
                    .bind_parameters(params)
                    .map_err(|error| prepare_error(QueryError::PatternText(error)))?;
                Footprint { nodes: 1, source_width: pattern_width(&bound) }
            }
            Self::Aggregate(prepared) => {
                let bound = prepared
                    .bind_parameters(params)
                    .map_err(|error| prepare_error(QueryError::PatternText(error)))?;
                match bound.input_relation() {
                    Some(input) => circuit(cx, input)?.append(1)?,
                    None => Footprint { nodes: 1, source_width: aggregate_width(&bound) },
                }
            }
            Self::PipelineAggregate(prepared) => {
                let bound = prepared
                    .bind_parameters(params)
                    .map_err(|error| prepare_error(QueryError::PipelineText(error)))?;
                match bound.input_relation() {
                    Some(input) => circuit(cx, input)?.append(1)?,
                    None => Footprint { nodes: 1, source_width: aggregate_width(&bound) },
                }
            }
            _ => {
                return Err(StandingQueryError::NativeClassUnsupported {
                    facade: self.facade_class(),
                });
            }
        };
        Ok((footprint.nodes, footprint.source_width))
    }
}
