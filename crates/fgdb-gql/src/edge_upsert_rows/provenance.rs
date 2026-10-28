//! Preparation-time provenance for the two cuts a row MERGE action can read.
//! A direct newly bound endpoint's property reads the current occurrence;
//! carried values read the statement input. Dynamic transport is conservatively
//! refused when a later dereference could no longer name its original column.

use crate::{GraphSetValue, GraphValueFunction};

pub(super) fn direct_endpoint_reads(value: &GraphSetValue, endpoints: &[usize]) -> bool {
    // At most two endpoint declarations exist, but the analysis is expressed
    // as a bit set so recursive list/Let scopes cannot confuse distinct inputs.
    fn visit(value: &GraphSetValue, endpoints: &[usize], locals: &mut Vec<u8>) -> Option<u8> {
        let sequence = |values: &[GraphSetValue], locals: &mut Vec<u8>| {
            values.iter().try_fold(0, |origin, value| {
                Some(origin | visit(value, endpoints, locals)?)
            })
        };
        Some(match value {
            GraphSetValue::Column(column) => endpoints
                .iter()
                .position(|endpoint| endpoint == column)
                .map_or(0, |at| 1_u8 << at),
            GraphSetValue::Local(offset) => *locals.get(locals.len().checked_sub(offset + 1)?)?,
            GraphSetValue::Literal(_)
            | GraphSetValue::TypedNull(_)
            | GraphSetValue::Integer(_)
            | GraphSetValue::Value(_)
            | GraphSetValue::Random { .. } => 0,
            GraphSetValue::List(values) => sequence(values, locals)?,
            GraphSetValue::Index { list, index } => {
                let origin = visit(list, endpoints, locals)?;
                visit(index, endpoints, locals)?;
                origin
            }
            GraphSetValue::MapGet { map, .. } => {
                let origin = visit(map, endpoints, locals)?;
                if origin != 0
                    && !matches!(map.as_ref(), GraphSetValue::Column(column) if endpoints.contains(column))
                {
                    return None;
                }
                // A stored property cannot contain graph identities. A map
                // carrying an endpoint is refused above before losing origin.
                0
            }
            GraphSetValue::ElementMetadata { value, .. } => {
                if visit(value, endpoints, locals)? != 0 {
                    return None;
                }
                0
            }
            GraphSetValue::Size(value)
            | GraphSetValue::Keys(value)
            | GraphSetValue::Temporal {
                argument: value, ..
            } => {
                visit(value, endpoints, locals)?;
                0
            }
            GraphSetValue::Function { function, argument } => {
                let origin = visit(argument, endpoints, locals)?;
                match function {
                    GraphValueFunction::Reverse
                    | GraphValueFunction::Add
                    | GraphValueFunction::Coalesce
                    | GraphValueFunction::PathNodes
                    | GraphValueFunction::PathRelationships => origin,
                    GraphValueFunction::Split
                    | GraphValueFunction::Subtract
                    | GraphValueFunction::PercentileDisc
                    | GraphValueFunction::PercentileCont
                    | GraphValueFunction::StDev
                    | GraphValueFunction::StDevP => 0,
                }
            }
            GraphSetValue::In { value, list } => {
                visit(value, endpoints, locals)?;
                visit(list, endpoints, locals)?;
                0
            }
            GraphSetValue::Equal { left, right } | GraphSetValue::Ordered { left, right, .. } => {
                visit(left, endpoints, locals)?;
                visit(right, endpoints, locals)?;
                0
            }
            GraphSetValue::Comprehension { list, filter, map } => {
                let origin = visit(list, endpoints, locals)?;
                let start = locals.len();
                locals.push(origin);
                let result = (|| {
                    if let Some(filter) = filter {
                        visit(filter, endpoints, locals)?;
                    }
                    map.as_deref()
                        .map_or(Some(origin), |map| visit(map, endpoints, locals))
                })();
                locals.truncate(start);
                result?
            }
            GraphSetValue::Quantifier {
                list, predicate, ..
            } => {
                let origin = visit(list, endpoints, locals)?;
                let start = locals.len();
                locals.push(origin);
                let result = visit(predicate, endpoints, locals);
                locals.truncate(start);
                result?;
                0
            }
            GraphSetValue::Slice { list, from, to } => {
                let origin = visit(list, endpoints, locals)?;
                for bound in [from, to].into_iter().flatten() {
                    visit(bound, endpoints, locals)?;
                }
                origin
            }
            GraphSetValue::Range { start, end, step } => {
                visit(start, endpoints, locals)?;
                visit(end, endpoints, locals)?;
                if let Some(step) = step {
                    visit(step, endpoints, locals)?;
                }
                0
            }
            GraphSetValue::Reduce { init, list, expr } => {
                let accumulator = visit(init, endpoints, locals)?;
                let element = visit(list, endpoints, locals)?;
                let start = locals.len();
                // A later iteration may receive any newly resolved endpoint
                // returned by an earlier body. Keep that uncertain origin
                // conservative instead of recursively revisiting the body to
                // a fixed point: nested reducers must remain linear visits,
                // not double their preparation cost at every nesting level.
                let uncertain = (1_u8 << endpoints.len()) - 1;
                locals.extend([accumulator | uncertain, element]);
                let result = visit(expr, endpoints, locals);
                locals.truncate(start);
                accumulator | result?
            }
            GraphSetValue::MapLiteral { values, guard, .. } => {
                if let Some(guard) = guard {
                    visit(guard, endpoints, locals)?;
                }
                sequence(values, locals)?
            }
            GraphSetValue::MapOverlay { base, values, .. } => {
                visit(base, endpoints, locals)? | sequence(values, locals)?
            }
            GraphSetValue::Let { values, body } => {
                let mut origins = Vec::with_capacity(values.len());
                for value in values {
                    origins.push(visit(value, endpoints, locals)?);
                }
                let start = locals.len();
                locals.extend(origins);
                let result = visit(body, endpoints, locals);
                locals.truncate(start);
                result?
            }
            GraphSetValue::Case { arms, otherwise } => {
                let mut origin = 0;
                for (condition, value) in arms {
                    visit(condition, endpoints, locals)?;
                    origin |= visit(value, endpoints, locals)?;
                }
                if let Some(value) = otherwise {
                    origin |= visit(value, endpoints, locals)?;
                }
                origin
            }
        })
    }
    endpoints.is_empty() || visit(value, endpoints, &mut Vec::new()).is_some()
}
