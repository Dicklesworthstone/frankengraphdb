//! Shared pagination preparation and bounded, source-free parameter binding.
//! Only the native value program survives preparation; binding never reparses
//! the statement or executes syntax. A literal page keeps its original bytes.

use super::*;
use crate::set_text::{ReadPageNumber, ReadValueTemplate};

const PAGE_EXPRESSION_EVENTS: u32 = 65_536;

impl Parser<'_> {
    /// A page is fixed before source access. Bare UInt64 operands keep their
    /// full-width contract. Expressions use the native scalar/value compiler:
    /// literals fold during preparation, parameters bind for each execution.
    pub(super) fn page_number(&mut self) -> Result<ReadPageNumber, GraphPatternTextError> {
        if matches!(
            self.current.kind,
            TokenKind::Parameter(_) | TokenKind::Digits(_)
        ) {
            let next = self.lexer.clone().next()?.kind;
            if !matches!(
                next,
                TokenKind::Punct(b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'[' | b'.')
            ) {
                let at = self.current.at;
                let parameter = match self.current.kind {
                    TokenKind::Parameter(name) => Some(name),
                    _ => None,
                };
                let declared = parameter.and_then(|name| self.parameter_types.get(name).copied());
                let existing = parameter
                    .and_then(|name| self.syntax.parameters.iter().find(|spec| spec.name == name));
                let kind = declared
                    .or_else(|| {
                        existing
                            .filter(|spec| self.page_expression_parameters.contains(&spec.name))
                            .map(|spec| spec.parameter_type)
                    })
                    .unwrap_or(GqlParameterType::UInt64);
                if !matches!(
                    kind,
                    GqlParameterType::UInt64
                        | GqlParameterType::Int64
                        | GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Int)
                ) {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::ConflictingParameterTypes,
                    ));
                }
                let flexible = kind == GqlParameterType::UInt64
                    && declared.is_none()
                    && (existing.is_none()
                        || parameter
                            .is_some_and(|name| self.flexible_page_parameters.contains(name)));
                let number = self.number(kind)?;
                if flexible && let Some(name) = parameter {
                    self.flexible_page_parameters.insert(name.to_owned());
                }
                return Ok(match number {
                    Number::Literal(GqlParameterValue::UInt64(value)) => {
                        ReadPageNumber::Literal(value)
                    }
                    Number::Parameter(index) => ReadPageNumber::Parameter { index, at },
                    _ => unreachable!("UInt64 page parser returns its declared domain"),
                });
            }
        }
        let at = self.current.at;
        let invalid = || {
            error(
                at,
                GraphPatternTextErrorKind::Expected(
                    "bounded constant nonnegative integer page expression",
                ),
            )
        };
        let uses: Vec<_> = self
            .syntax
            .parameters
            .iter()
            .map(|spec| spec.occurrences)
            .collect();
        // Property predicates and ordinary scalar results must not refine a
        // bare page's unsigned default. This context spans just this native
        // value program and is restored even when parsing returns an error.
        let previous = core::mem::replace(&mut self.in_page_expression, true);
        let value = self.read_row_value(&[], 0);
        self.in_page_expression = previous;
        let value = value.map_err(|failure| match failure.kind {
            crate::GraphSetTextErrorKind::Pattern(kind) => error(failure.offset, kind),
            _ => invalid(),
        })?;
        if !value.row_independent() {
            return Err(invalid());
        }
        let mut parameterized = false;
        for (index, spec) in self.syntax.parameters.iter().enumerate() {
            if uses.get(index).copied() != Some(spec.occurrences) {
                self.page_expression_parameters.insert(spec.name.clone());
                parameterized = true;
            }
        }
        let page = ReadPageNumber::Expression {
            value: Box::new(value),
            at,
        };
        if parameterized {
            Ok(page)
        } else {
            // Preserve both the existing folded template and the existing
            // preparation-time refusal for wholly literal expressions.
            page.bind_values(&[])
                .map(ReadPageNumber::Literal)
                .map_err(|_| invalid())
        }
    }
}

impl ReadPageNumber {
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<u64, GraphPatternTextError> {
        match self {
            Self::Literal(value) => Ok(*value),
            Self::Parameter { index, at } => bound_operand(&values[*index], *at),
            Self::Expression { value, at } => evaluate_page(value, values, *at),
        }
    }

    /// Graph-pattern and aggregate pages formerly used Number's encoding.
    /// Retain those literal/parameter tags exactly; only expressions add a tag.
    pub(super) fn append_pattern_transcript(&self, bytes: &mut Vec<u8>) {
        match self {
            Self::Literal(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            Self::Parameter { index, .. } => {
                bytes.push(2);
                bytes.extend_from_slice(&(*index as u64).to_be_bytes());
            }
            Self::Expression { value, .. } => {
                bytes.push(3);
                value.append_template_transcript(bytes);
            }
        }
    }
}

/// A bare page may share the signed scalar declaration of an expression, or
/// keep its original UInt64 declaration and complete range. Both paths still
/// refuse NULL, negative and nonintegral values before source access.
pub(crate) fn bound_operand(
    value: &GqlParameterValue,
    at: usize,
) -> Result<u64, GraphPatternTextError> {
    let invalid = || error(at, GraphPatternTextErrorKind::InvalidPageValue);
    match value {
        GqlParameterValue::UInt64(value) => Ok(*value),
        GqlParameterValue::Int64(value) => u64::try_from(*value).map_err(|_| invalid()),
        GqlParameterValue::Scalar(value) => match value.value() {
            fgdb_types::CanonicalScalar::Int(value) => u64::try_from(*value).map_err(|_| invalid()),
            _ => Err(invalid()),
        },
        _ => Err(invalid()),
    }
}

fn evaluate_page(
    value: &ReadValueTemplate,
    values: &[GqlParameterValue],
    at: usize,
) -> Result<u64, GraphPatternTextError> {
    let invalid = || error(at, GraphPatternTextErrorKind::InvalidPageValue);
    let mut remaining = PAGE_EXPRESSION_EVENTS;
    // Binding collection arguments copies their payloads before the evaluator
    // can charge work. Admit every retained occurrence first, including holes
    // inside scalar bytecode. Literal folding keeps its original allowance.
    if !values.is_empty() && !admit_page_binding(value, values, &mut remaining) {
        return Err(error(at, GraphPatternTextErrorKind::PageExpressionLimit));
    }
    let value = scoped::mutation::return_projection::bind_read_value(value, values)
        .map_err(|_| invalid())?;
    let row = GraphValueRow::from_owned_values(Vec::new());
    // There is no query Cx at preparation/binding. Keep the existing finite
    // native work/scratch allowance, charged before each allocation, including
    // discarded values such as size(range(...)). No graph reader is installed.
    let mut exhausted = false;
    let result = crate::set_ops::evaluate_value(&value, &row, 0, &mut |_| {
        remaining = remaining.checked_sub(1).ok_or_else(|| {
            exhausted = true;
        })?;
        Ok::<_, ()>(())
    });
    let value = result.map_err(|_| {
        if exhausted {
            error(at, GraphPatternTextErrorKind::PageExpressionLimit)
        } else {
            invalid()
        }
    })?;
    let Some(fgdb_types::CanonicalScalar::Int(value)) = value.as_scalar() else {
        return Err(invalid());
    };
    u64::try_from(*value).map_err(|_| invalid())
}

/// Inspect native program structure, without evaluating or cloning arguments.
/// Cached canonical lengths bound copied argument payloads across all uses,
/// even if a lowering retains one argument more often than its source text.
/// Template nodes and instructions share the evaluator's event allowance.
fn admit_page_binding(
    value: &ReadValueTemplate,
    values: &[GqlParameterValue],
    remaining_events: &mut u32,
) -> bool {
    use crate::mutation_text::MutationIntegerTemplateOp as Integer;
    use crate::set_text::ReadValueTemplate as Value;

    fn event(remaining: &mut u32) -> bool {
        let Some(next) = remaining.checked_sub(1) else {
            return false;
        };
        *remaining = next;
        true
    }
    fn argument(index: usize, values: &[GqlParameterValue], remaining: &mut usize) -> bool {
        let bytes = match values.get(index) {
            Some(GqlParameterValue::Int64(_) | GqlParameterValue::UInt64(_)) => 9,
            Some(GqlParameterValue::Scalar(value)) => value.canonical_bytes().len(),
            Some(GqlParameterValue::List(value)) => value.canonical_bytes().len(),
            Some(GqlParameterValue::Map(value)) => value.canonical_bytes().len(),
            None => return false,
        };
        let Some(next) = remaining.checked_sub(bytes) else {
            return false;
        };
        *remaining = next;
        true
    }
    fn visit(
        value: &Value,
        parameters: &[GqlParameterValue],
        events: &mut u32,
        bytes: &mut usize,
    ) -> bool {
        if !event(events) {
            return false;
        }
        match value {
            Value::Literal(_) | Value::Local(_) => true,
            Value::Column(_) | Value::Random { .. } => false,
            Value::Parameter { index, .. } => argument(*index, parameters, bytes),
            Value::Integer { program, .. } => program.iter().all(|op| {
                event(events)
                    && match op {
                        Integer::Bound(_) => true,
                        Integer::Parameter { index, .. }
                        | Integer::ParameterField { index, .. }
                        | Integer::ListMembers { index, .. } => argument(*index, parameters, bytes),
                    }
            }),
            Value::List(items) => items
                .iter()
                .all(|value| visit(value, parameters, events, bytes)),
            Value::Index { list, index } => {
                visit(list, parameters, events, bytes) && visit(index, parameters, events, bytes)
            }
            Value::Size(value) | Value::Keys(value) => visit(value, parameters, events, bytes),
            Value::In { value, list } => {
                visit(value, parameters, events, bytes) && visit(list, parameters, events, bytes)
            }
            Value::Equal { left, right } | Value::Ordered { left, right, .. } => {
                visit(left, parameters, events, bytes) && visit(right, parameters, events, bytes)
            }
            Value::Temporal { argument, .. }
            | Value::Function { argument, .. }
            | Value::ElementMetadata {
                value: argument, ..
            } => visit(argument, parameters, events, bytes),
            Value::Comprehension { list, filter, map } => {
                visit(list, parameters, events, bytes)
                    && filter
                        .as_deref()
                        .is_none_or(|value| visit(value, parameters, events, bytes))
                    && map
                        .as_deref()
                        .is_none_or(|value| visit(value, parameters, events, bytes))
            }
            Value::Quantifier {
                list, predicate, ..
            } => {
                visit(list, parameters, events, bytes)
                    && visit(predicate, parameters, events, bytes)
            }
            Value::Slice { list, from, to } => {
                visit(list, parameters, events, bytes)
                    && from
                        .as_deref()
                        .is_none_or(|value| visit(value, parameters, events, bytes))
                    && to
                        .as_deref()
                        .is_none_or(|value| visit(value, parameters, events, bytes))
            }
            Value::Range { start, end, step } => {
                visit(start, parameters, events, bytes)
                    && visit(end, parameters, events, bytes)
                    && step
                        .as_deref()
                        .is_none_or(|value| visit(value, parameters, events, bytes))
            }
            Value::Reduce { init, list, expr } => {
                visit(init, parameters, events, bytes)
                    && visit(list, parameters, events, bytes)
                    && visit(expr, parameters, events, bytes)
            }
            Value::MapLiteral { values, guard, .. } => {
                values
                    .iter()
                    .all(|value| visit(value, parameters, events, bytes))
                    && guard
                        .as_deref()
                        .is_none_or(|value| visit(value, parameters, events, bytes))
            }
            Value::MapGet { map, .. } => visit(map, parameters, events, bytes),
            Value::MapOverlay { base, values, .. } => {
                visit(base, parameters, events, bytes)
                    && values
                        .iter()
                        .all(|value| visit(value, parameters, events, bytes))
            }
            Value::Let { values, body } => {
                values
                    .iter()
                    .all(|value| visit(value, parameters, events, bytes))
                    && visit(body, parameters, events, bytes)
            }
            Value::Case { arms, otherwise } => {
                arms.iter().all(|(condition, value)| {
                    visit(condition, parameters, events, bytes)
                        && visit(value, parameters, events, bytes)
                }) && otherwise
                    .as_deref()
                    .is_none_or(|value| visit(value, parameters, events, bytes))
            }
        }
    }
    let mut remaining_bytes = crate::algebra::MAX_SCALAR_PREDICATE_BYTES;
    visit(value, values, remaining_events, &mut remaining_bytes)
}

/// Compound pages use local parameter ordinals while retaining their exact
/// names for binding and template identity. All offsets are in original text.
pub(crate) struct PreparedPageNumber {
    pub(crate) page: ReadPageNumber,
    pub(crate) parameters: Vec<GqlParameterSpec>,
    pub(crate) end: usize,
}

/// All compound pages share one inference scope, just as native WITH/RETURN
/// pages do. In particular, a name shared by arithmetic and a bare page has
/// one declaration regardless of whether SKIP or LIMIT mentions it first.
#[derive(Default)]
pub(crate) struct PageParameters {
    parameters: Vec<GqlParameterSpec>,
    offsets: Vec<usize>,
    flexible: BTreeSet<String>,
    expressions: BTreeSet<String>,
}

impl PageParameters {
    pub(crate) fn into_uses(self) -> Vec<(usize, GqlParameterSpec)> {
        self.offsets.into_iter().zip(self.parameters).collect()
    }
}

pub(crate) fn prepare(
    statement: &str,
    at: usize,
    declarations: &[(&str, GqlParameterType)],
    parameters: &mut PageParameters,
) -> Result<PreparedPageNumber, GraphPatternTextError> {
    let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
    parser.lexer.at = at;
    parser.current = parser.lexer.next()?;
    parser.syntax.parameters.clone_from(&parameters.parameters);
    parser
        .syntax
        .parameter_offsets
        .clone_from(&parameters.offsets);
    parser
        .flexible_page_parameters
        .clone_from(&parameters.flexible);
    parser
        .page_expression_parameters
        .clone_from(&parameters.expressions);
    let page = parser.page_number()?;
    parameters.parameters.clone_from(&parser.syntax.parameters);
    parameters.offsets = parser.syntax.parameter_offsets;
    parameters.flexible = parser.flexible_page_parameters;
    parameters.expressions = parser.page_expression_parameters;
    Ok(PreparedPageNumber {
        page,
        parameters: parser.syntax.parameters,
        end: parser.current.at,
    })
}
