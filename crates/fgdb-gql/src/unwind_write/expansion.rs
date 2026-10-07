//! Bounded parameter-only nested UNWIND expansion. Frames retain references to
//! earlier aliases, not cloned JSON trees or graph rows. All metadata remains
//! private until the ordinary whole-batch binder accepts the final program.

use super::*;
use crate::GqlParameterValue;

#[derive(Clone)]
pub(crate) struct UnwindSource {
    /// An earlier alias: zero is the root, one is the first nested source.
    /// Used only when `parameter` is None.
    pub(crate) source: usize,
    /// An independent named list source; alias coordinates still count clauses.
    pub(crate) parameter: Option<String>,
    pub(crate) path: Box<[UnwindFieldAccess]>,
    pub(crate) offset: usize,
}

pub(super) enum InputRows<'a> {
    // Preserve the original single-source path without extra frame allocation.
    Roots(&'a [GraphValue]),
    Expanded(Vec<Box<[&'a GraphValue]>>),
}
impl<'a> InputRows<'a> {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Roots(rows) => rows.len(),
            Self::Expanded(rows) => rows.len(),
        }
    }
    pub(super) fn at(&self, row: usize, source: usize) -> &'a GraphValue {
        match self {
            Self::Roots(rows) => {
                debug_assert_eq!(source, 0);
                &rows[row]
            }
            Self::Expanded(rows) => rows[row][source],
        }
    }
}

fn work<C>(
    control: &mut impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
    units: u64,
) -> Result<(), GraphUnwindBindError<C>> {
    control(GraphUnwindBindEvent::Work(units)).map_err(GraphUnwindBindError::Interrupted)
}

#[cfg(test)]
pub(super) fn expand<'a, C>(
    definition: &GraphUnwindWriteText,
    roots: &'a [GraphValue],
    limit: usize,
    control: &mut impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
) -> Result<InputRows<'a>, GraphUnwindBindError<C>> {
    expand_with_parameters(definition, roots, None, limit, control)
}

pub(super) fn expand_with_parameters<'a, C>(
    definition: &GraphUnwindWriteText,
    roots: &'a [GraphValue],
    arguments: Option<&'a GqlParameters>,
    limit: usize,
    control: &mut impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
) -> Result<InputRows<'a>, GraphUnwindBindError<C>> {
    if definition.sources.is_empty() {
        return Ok(InputRows::Roots(roots));
    }
    // At most eight sources: alias dependencies point strictly backwards.
    // Admit metadata and ALL independent sources before traversing a product,
    // so an empty earlier source cannot conceal a malformed later parameter.
    work(control, (3 * definition.sources.len() + 2) as u64)?;
    let mut parameter_rows = Vec::with_capacity(definition.sources.len());
    for (clause, source) in definition.sources.iter().enumerate() {
        let Some(name) = &source.parameter else {
            parameter_rows.push(None);
            continue;
        };
        work(control, 1)?;
        let Some(GqlParameterValue::List(values)) = arguments.and_then(|args| args.get(name)) else {
            return Err(GraphUnwindWriteError::Expansion {
                row: 0,
                clause: clause + 1,
                offset: source.offset,
                kind: GraphUnwindRowError::ExpectedListField,
            }.into());
        };
        let values = values.values();
        if values.len() > limit {
            return Err(GraphUnwindWriteError::TooManyRows {
                limit,
                observed: values.len(),
            }.into());
        }
        parameter_rows.push(Some(values));
    }
    let mut state = Expansion {
        sources: &definition.sources,
        parameter_rows,
        limit,
        counts: vec![0; definition.sources.len()],
        rows: Vec::new(),
    };
    let mut frame = Vec::with_capacity(definition.sources.len() + 1);
    for (root, value) in roots.iter().enumerate() {
        work(control, 1)?;
        frame.push(value);
        state.visit(root, &mut frame, control)?;
        frame.pop();
    }
    work(control, 1)?;
    if state.rows.is_empty() {
        // Do not invent a successfully bound empty native write program. Its
        // statement/schema/authorization contract is not the nonempty binder's.
        return Err(GraphUnwindWriteError::Empty.into());
    }
    Ok(InputRows::Expanded(state.rows))
}

struct Expansion<'a, 'd> {
    sources: &'d [UnwindSource],
    parameter_rows: Vec<Option<&'a [GraphValue]>>,
    limit: usize,
    counts: Vec<usize>,
    rows: Vec<Box<[&'a GraphValue]>>,
}
impl<'a> Expansion<'a, '_> {
    fn visit<C>(
        &mut self,
        root: usize,
        frame: &mut Vec<&'a GraphValue>,
        control: &mut impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
    ) -> Result<(), GraphUnwindBindError<C>> {
        let at = frame.len() - 1;
        if at == self.sources.len() {
            // Copy only bounded binding references, never their payloads. The
            // last source's counter has already admitted this final row.
            work(control, (frame.len() + 1) as u64)?;
            self.rows.push(frame.as_slice().into());
            return Ok(());
        }
        let source = &self.sources[at];
        let offset = source.offset;
        let refusal = |kind| GraphUnwindWriteError::Expansion {
            row: root,
            clause: at + 1,
            offset,
            kind,
        };
        let items = if let Some(items) = self.parameter_rows[at] {
            // Copy a borrowed slice, never the source list or its payloads.
            items
        } else {
            let value = field_value(frame[source.source], &source.path, offset, root, control)
                .map_err(|error| match error {
                    GraphUnwindBindError::Binding(GraphUnwindWriteError::Row { kind, .. }) => {
                        GraphUnwindBindError::Binding(refusal(kind))
                    }
                    error => error,
                })?;
            let Some(value) = value.filter(|value| !value.is_null()) else {
                return Ok(());
            };
            let GraphValue::List(items) = value else {
                return Err(refusal(GraphUnwindRowError::ExpectedListField).into());
            };
            items.as_ref()
        };
        for item in items.iter() {
            work(control, 1)?;
            // Bound EVERY prefix, not just final rows. Empty deeper lists must
            // not permit an exponential amount of unaccounted expansion work.
            if self.counts[at] == self.limit {
                return Err(GraphUnwindWriteError::TooManyRows {
                    limit: self.limit,
                    // The caller clamps the limit to 65,536 before expansion.
                    observed: self.limit + 1,
                }.into());
            }
            self.counts[at] += 1;
            // UNWIND binds values, not just documents. Subsequent sources
            // validate list shape; mutation operands validate scalar shape.
            // Retain each item by reference, including scalar/null/list items.
            frame.push(item);
            self.visit(root, frame, control)?;
            frame.pop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
