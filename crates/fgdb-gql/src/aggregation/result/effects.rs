//! Preparation-time dependency admission for owners without an element reader.
//! The rank evaluator itself remains shared. Maps are values, so a catalog
//! property token on a proven map receiver does not require a graph read.

use super::*;
use crate::{GraphElementKey, GraphSetColumnType, GraphSetProjection, GraphSetValue};

fn element_type(kind: GraphSetColumnType) -> bool {
    matches!(
        kind,
        GraphSetColumnType::Vertex | GraphSetColumnType::Edge | GraphSetColumnType::Any
    )
}

/// (may read an element property, may return a vertex/edge). Unknown dynamic
/// receivers stay conservative; lazy branches participate before data exists.
fn effects(
    value: &GraphSetValue,
    columns: &[bool],
    types: &[GraphSetColumnType],
    locals: &mut Vec<bool>,
) -> (bool, bool) {
    use GraphSetValue as V;
    match value {
        V::Column(at) => (false, columns.get(*at).copied().unwrap_or(true)),
        V::Local(at) => (
            false,
            locals
                .len()
                .checked_sub(at + 1)
                .and_then(|at| locals.get(at))
                .copied()
                .unwrap_or(true),
        ),
        V::Value(value) => (
            false,
            matches!(value, GraphValue::Vertex(_) | GraphValue::Edge(_)),
        ),
        V::Literal(_) | V::TypedNull(_) | V::Integer(_) | V::Random { .. } => (false, false),
        V::MapGet { map, property, .. } => {
            let (read, element) = effects(map, columns, types, locals);
            (
                read || (element && !matches!(property, GraphElementKey::Missing)),
                true,
            )
        }
        V::List(values) => (
            values
                .iter()
                .any(|value| effects(value, columns, types, locals).0),
            false,
        ),
        V::Size(value)
        | V::Keys(value)
        | V::Temporal {
            argument: value, ..
        } => (effects(value, columns, types, locals).0, false),
        V::Function { argument, .. } => (
            effects(argument, columns, types, locals).0,
            GraphSetProjection::admit_output(value, types, 0).map_or(true, element_type),
        ),
        V::Index { list, index } => (
            effects(list, columns, types, locals).0 || effects(index, columns, types, locals).0,
            true,
        ),
        V::In {
            value: left,
            list: right,
        }
        | V::Equal { left, right }
        | V::Ordered { left, right, .. } => (
            effects(left, columns, types, locals).0 || effects(right, columns, types, locals).0,
            false,
        ),
        V::Comprehension { list, filter, map } => {
            let mut read = effects(list, columns, types, locals).0;
            locals.push(true);
            for value in [filter, map].into_iter().flatten() {
                read |= effects(value, columns, types, locals).0;
            }
            locals.pop();
            (read, false)
        }
        V::Quantifier {
            list, predicate, ..
        } => {
            let read = effects(list, columns, types, locals).0;
            locals.push(true);
            let nested = effects(predicate, columns, types, locals).0;
            locals.pop();
            (read || nested, false)
        }
        V::Slice { list, from, to } => (
            effects(list, columns, types, locals).0
                || from
                    .as_deref()
                    .is_some_and(|value| effects(value, columns, types, locals).0)
                || to
                    .as_deref()
                    .is_some_and(|value| effects(value, columns, types, locals).0),
            false,
        ),
        V::Range { start, end, step } => (
            effects(start, columns, types, locals).0
                || effects(end, columns, types, locals).0
                || step
                    .as_deref()
                    .is_some_and(|value| effects(value, columns, types, locals).0),
            false,
        ),
        V::Reduce { init, list, expr } => {
            let read =
                effects(init, columns, types, locals).0 || effects(list, columns, types, locals).0;
            locals.extend([true, true]);
            let nested = effects(expr, columns, types, locals).0;
            locals.truncate(locals.len() - 2);
            (read || nested, true)
        }
        V::MapLiteral { values, guard, .. } => (
            guard
                .as_deref()
                .is_some_and(|value| effects(value, columns, types, locals).0)
                || values
                    .iter()
                    .any(|value| effects(value, columns, types, locals).0),
            false,
        ),
        V::MapOverlay { base, values, .. } => (
            effects(base, columns, types, locals).0
                || values
                    .iter()
                    .any(|value| effects(value, columns, types, locals).0),
            false,
        ),
        V::Let { values, body } => {
            let mut read = false;
            let mut scope = Vec::new();
            for value in values {
                let (nested, element) = effects(value, columns, types, locals);
                read |= nested;
                scope.push(element);
            }
            let depth = locals.len();
            locals.extend(scope);
            let (nested, element) = effects(body, columns, types, locals);
            locals.truncate(depth);
            (read || nested, element)
        }
        V::Case { arms, otherwise } => {
            let (mut read, mut element) = (false, false);
            for (condition, value) in arms {
                read |= effects(condition, columns, types, locals).0;
                let (nested, result) = effects(value, columns, types, locals);
                read |= nested;
                element |= result;
            }
            if let Some(value) = otherwise {
                let (nested, result) = effects(value, columns, types, locals);
                read |= nested;
                element |= result;
            }
            (read, element)
        }
    }
}

impl PreparedGraphAggregate {
    /// Reader-free owners must refuse an untracked graph property dependency
    /// before observing source cardinality. Follow visible aliases' static
    /// result evidence, without executing or re-evaluating their expressions.
    pub(crate) fn computed_order_requires_element_reads(&self) -> bool {
        if self.computed_ordering.is_empty() {
            return false;
        }
        let Ok(mut types) = self.output_input_types() else {
            return true;
        };
        let mut columns = types.iter().copied().map(element_type).collect::<Vec<_>>();
        let Some(projection) = &self.output_projection else {
            return true;
        };
        let mut visible = Vec::new();
        let mut visible_types = Vec::new();
        for output in projection {
            let (read, element) = effects(output.value(), &columns, &types, &mut Vec::new());
            if read {
                return true;
            }
            visible.push(element);
            visible_types.push(
                GraphSetProjection::admit_output(output.value(), &types, 0)
                    .unwrap_or(GraphSetColumnType::Any),
            );
        }
        columns.extend(visible);
        types.extend(visible_types);
        self.computed_ordering
            .iter()
            .any(|value| effects(value, &columns, &types, &mut Vec::new()).0)
    }
}
