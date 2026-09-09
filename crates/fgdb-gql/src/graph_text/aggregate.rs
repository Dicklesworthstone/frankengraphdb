//! Aggregate RETURN and explicit GROUP BY over the shared graph text parser.
//! No source rewriting, second lexer, child-result materialization or catalog
//! re-resolution. Group pagination is never pushed into the matching child.

use super::*;
use crate::{GraphAggregate, GraphAggregateFunction, PreparedGraphAggregate};

/// Position of a textual RETURN item in the typed aggregate result. All keys
/// are retained by that result, even when RETURN interleaves keys and summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphAggregateTextSlot {
    GroupKey(usize),
    Aggregate(usize),
}

#[derive(Clone)]
struct Summary {
    function: GraphAggregateFunction,
    column: Option<usize>,
    alias: String,
}
impl Summary {
    fn declaration(&self) -> GraphAggregate<'_> {
        match (self.function, self.column) {
            (GraphAggregateFunction::CountRows, None) => GraphAggregate::count_rows(&self.alias),
            (GraphAggregateFunction::Count, Some(at)) => GraphAggregate::count(&self.alias, at),
            (GraphAggregateFunction::CountDistinct, Some(at)) => GraphAggregate::count_distinct(&self.alias, at),
            (GraphAggregateFunction::SumInt, Some(at)) => GraphAggregate::sum_int(&self.alias, at),
            (GraphAggregateFunction::Min, Some(at)) => GraphAggregate::min(&self.alias, at),
            (GraphAggregateFunction::Max, Some(at)) => GraphAggregate::max(&self.alias, at),
            _ => unreachable!("private aggregate parser pairs functions and arguments"),
        }
    }
}

/// Parse-once grouped text definition. Binding returns the existing streaming
/// aggregate, with one shared governed source and visitor at execution time.
/// The explicit profile requires every grouping expression in RETURN and
/// every nonaggregate RETURN expression in GROUP BY. Grouping uses expressions,
/// not aliases. SUM/SUM_INT are checked integer-only operations in this profile.
#[derive(Clone)]
pub struct PreparedGraphAggregateText {
    child: PreparedGraphText,
    keys: Vec<usize>,
    summaries: Vec<Summary>,
    names: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
    offset: Number,
    count: Option<Number>,
}
impl core::fmt::Debug for PreparedGraphAggregateText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphAggregateText")
            .field("columns", &self.names.len())
            .field("parameters", &self.child.parameters.len())
            .field("definition", &"[REDACTED]").finish()
    }
}
impl PreparedGraphAggregateText {
    /// Validate the complete bounded grammar and grouping before calling the
    /// host resolver. Numeric operands register with the original parser's
    /// parameter table, including arguments used only in output pagination.
    pub fn prepare(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphPatternTextError> {
        let mut parser = Parser::new(statement)?;
        parser.parse_head()?;
        // All grouping keys are projected; DISTINCT on the aggregate output
        // is therefore redundant. Neither spelling deduplicates child matches.
        if !parser.take_word("DISTINCT")? { parser.take_word("ALL")?; }
        let mut returned = Vec::new();
        loop {
            parser.capacity(returned.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
            let item = parser.aggregate_item()?;
            if returned.iter().any(|previous: &ReturnItem<'_>| previous.alias.text == item.alias.text) {
                return Err(error(item.alias.at, GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection)));
            }
            returned.push(item);
            if !parser.take(b',')? { break; }
        }
        let mut groups = Vec::new();
        if parser.take_word("GROUP")? {
            parser.word("BY")?;
            loop {
                parser.capacity(groups.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
                let expression = parser.aggregate_expression()?;
                if groups.iter().any(|previous: &Expression<'_>| previous.same(expression)) {
                    return Err(error(expression.variable.at, GraphPatternTextErrorKind::Expected("unique GROUP BY expression")));
                }
                groups.push(expression);
                if !parser.take(b',')? { break; }
            }
        }
        parser.parse_pagination()?;
        parser.end()?;
        if !returned.iter().any(|item| item.function.is_some()) {
            return Err(error(parser.syntax.return_at, GraphPatternTextErrorKind::Expected("at least one aggregate expression")));
        }
        for item in &returned {
            if item.function.is_none() && !groups.iter().any(|group| group.same(item.expression.expect("key expression"))) {
                return Err(error(item.alias.at, GraphPatternTextErrorKind::Expected("nonaggregate RETURN expression in GROUP BY")));
            }
        }
        for group in &groups {
            if !returned.iter().any(|item| item.function.is_none() && item.expression.is_some_and(|expression| expression.same(*group))) {
                return Err(error(group.variable.at, GraphPatternTextErrorKind::Expected("GROUP BY expression projected in RETURN")));
            }
        }
        // Emit group input expressions first with their actual public aliases.
        // Repeated aggregate arguments reuse one source column. A grouping
        // expression first mentioned inside SUM cannot steal the key's name.
        let mut inputs: Vec<Expression<'_>> = Vec::new();
        for group in &groups {
            let alias = returned.iter().find(|item| item.function.is_none()
                && item.expression.is_some_and(|expression| expression.same(*group)))
                .expect("group projection checked above").alias;
            inputs.push(*group);
            parser.syntax.columns.push(Column { variable: group.variable, property: group.property, alias });
        }
        let keys = (0..groups.len()).collect();
        let mut summaries = Vec::new();
        let mut slots = Vec::new();
        let mut names = Vec::new();
        for item in &returned {
            names.push(item.alias.text.to_owned());
            if let Some(function) = item.function {
                let column = item.expression.map(|expression| {
                    if let Some(at) = inputs.iter().position(|previous| previous.same(expression)) { return at; }
                    let at = inputs.len();
                    inputs.push(expression);
                    parser.syntax.columns.push(Column { variable: expression.variable, property: expression.property, alias: item.alias });
                    at
                });
                slots.push(GraphAggregateTextSlot::Aggregate(summaries.len()));
                summaries.push(Summary { function, column, alias: item.alias.text.to_owned() });
            } else {
                slots.push(GraphAggregateTextSlot::GroupKey(groups.iter().position(|group|
                    group.same(item.expression.expect("key expression"))).expect("group checked above")));
            }
        }
        if parser.syntax.columns.is_empty() {
            // COUNT(*) alone still needs the existing nonempty child shape.
            // This bound identity is not a group key or a counted argument.
            let variable = parser.syntax.variables[0];
            parser.syntax.columns.push(Column { variable, property: None, alias: Name { text: "__count_source", at: variable.at } });
        }
        let offset = core::mem::replace(&mut parser.syntax.offset, Number::Literal(GqlParameterValue::UInt64(0)));
        let count = parser.syntax.count.take();
        parser.syntax.distinct = false;
        let child = PreparedGraphText::from_syntax(statement, parser.syntax, resolve)?;
        Ok(Self { child, keys, summaries, names, slots, offset, count })
    }

    /// Explicit definition and schema exports; Debug does not expose them.
    #[must_use]
    pub fn statement(&self) -> &str { self.child.statement() }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.child.parameter_schema() }
    #[must_use]
    pub fn columns(&self) -> &[String] { &self.names }
    /// RETURN position i selects keys()[k] or values()[a] in the result row.
    #[must_use]
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] { &self.slots }

    /// One argument validation and one lowering; no parsing, resolving, source
    /// access, or materialization of child matches. Previously bound aggregates
    /// remain immutable. Output pagination does not alter the child pattern.
    pub fn bind_parameters(&self, arguments: &GqlParameters) -> Result<PreparedGraphAggregate, GraphPatternTextError> {
        let values = self.child.checked_arguments(arguments)?;
        let input = self.child.bind_values(&values)?;
        let summaries: Vec<_> = self.summaries.iter().map(Summary::declaration).collect();
        PreparedGraphAggregate::prepare(input, &self.keys, &summaries,
            self.offset.unsigned(&values), self.count.as_ref().map(|count| count.unsigned(&values)))
            .map_err(|kind| error(self.child.return_at, GraphPatternTextErrorKind::AggregateBuild(kind)))
    }
}

#[derive(Clone, Copy)]
struct Expression<'a> { variable: Name<'a>, property: Option<Name<'a>> }
impl Expression<'_> {
    fn same(self, other: Self) -> bool {
        self.variable.text == other.variable.text && self.property.map(|name| name.text) == other.property.map(|name| name.text)
    }
}
struct ReturnItem<'a> {
    expression: Option<Expression<'a>>,
    function: Option<GraphAggregateFunction>,
    alias: Name<'a>,
}
impl<'a> Parser<'a> {
    fn expression_after_name(&mut self, variable: Name<'a>) -> Result<Expression<'a>, GraphPatternTextError> {
        if !self.syntax.variables.iter().any(|name| name.text == variable.text) {
            return Err(error(variable.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        let property = if self.take(b'.')? { Some(self.name()?) } else { None };
        Ok(Expression { variable, property })
    }
    fn aggregate_expression(&mut self) -> Result<Expression<'a>, GraphPatternTextError> {
        let variable = self.name()?;
        self.expression_after_name(variable)
    }
    fn aggregate_item(&mut self) -> Result<ReturnItem<'a>, GraphPatternTextError> {
        let name = self.name()?;
        let (expression, function, default_alias) = if self.take(b'(')? {
            let function = if name.text.eq_ignore_ascii_case("COUNT") { GraphAggregateFunction::Count }
                else if name.text.eq_ignore_ascii_case("SUM") || name.text.eq_ignore_ascii_case("SUM_INT") { GraphAggregateFunction::SumInt }
                else if name.text.eq_ignore_ascii_case("MIN") { GraphAggregateFunction::Min }
                else if name.text.eq_ignore_ascii_case("MAX") { GraphAggregateFunction::Max }
                else { return Err(error(name.at, GraphPatternTextErrorKind::Expected("COUNT, SUM, SUM_INT, MIN or MAX"))); };
            let distinct = self.take_word("DISTINCT")?;
            if !distinct { self.take_word("ALL")?; }
            let (expression, function) = if self.take(b'*')? {
                if function != GraphAggregateFunction::Count || distinct {
                    return Err(error(name.at, GraphPatternTextErrorKind::Expected("COUNT(*) without argument DISTINCT")));
                }
                (None, GraphAggregateFunction::CountRows)
            } else {
                if distinct && function != GraphAggregateFunction::Count {
                    return Err(error(name.at, GraphPatternTextErrorKind::Expected("DISTINCT argument only for COUNT")));
                }
                (Some(self.aggregate_expression()?), if distinct { GraphAggregateFunction::CountDistinct } else { function })
            };
            self.punct(b')', ")")?;
            let alias = match function {
                GraphAggregateFunction::CountRows | GraphAggregateFunction::Count | GraphAggregateFunction::CountDistinct => "count",
                GraphAggregateFunction::SumInt => "sum", GraphAggregateFunction::Min => "min", GraphAggregateFunction::Max => "max",
            };
            (expression, Some(function), Name { text: alias, at: name.at })
        } else {
            let expression = self.expression_after_name(name)?;
            (Some(expression), None, expression.property.unwrap_or(name))
        };
        let alias = if self.take_word("AS")? { self.name()? } else { default_alias };
        Ok(ReturnItem { expression, function, alias })
    }
}
