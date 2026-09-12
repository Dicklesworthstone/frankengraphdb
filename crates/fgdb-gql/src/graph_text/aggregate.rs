//! Aggregate RETURN and explicit GROUP BY over the shared graph text parser.
//! No source rewriting, second lexer, child-result materialization or catalog
//! re-resolution. Group pagination is never pushed into the matching child.

mod having;

use super::*;
use crate::{
    GraphAggregate, GraphAggregateColumn, GraphAggregateFilter, GraphAggregateFunction,
    GraphAggregateOrder, GraphAggregateTest, GraphNullPlacement, MAX_AGGREGATE_FILTERS,
    PreparedGraphAggregate,
};

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

#[derive(Clone)]
enum HavingTest {
    Integer {
        comparison: IntegerComparison,
        value: Number,
    },
    IsNull,
    IsNotNull,
}
#[derive(Clone)]
struct Having {
    column: GraphAggregateColumn,
    test: HavingTest,
}
impl Summary {
    fn declaration(&self) -> GraphAggregate<'_> {
        match (self.function, self.column) {
            (GraphAggregateFunction::CountRows, None) => GraphAggregate::count_rows(&self.alias),
            (GraphAggregateFunction::Count, Some(at)) => GraphAggregate::count(&self.alias, at),
            (GraphAggregateFunction::CountDistinct, Some(at)) => {
                GraphAggregate::count_distinct(&self.alias, at)
            }
            (GraphAggregateFunction::SumInt, Some(at)) => GraphAggregate::sum_int(&self.alias, at),
            (GraphAggregateFunction::SumIntDistinct, Some(at)) => {
                GraphAggregate::sum_int_distinct(&self.alias, at)
            }
            (GraphAggregateFunction::AverageInt, Some(at)) => {
                GraphAggregate::average_int(&self.alias, at)
            }
            (GraphAggregateFunction::AverageIntDistinct, Some(at)) => {
                GraphAggregate::average_int_distinct(&self.alias, at)
            }
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
/// not aliases. SUM/SUM_INT and AVG/AVG_INT accept integer/null arguments only.
/// SUM and AVG accept ALL or DISTINCT. AVG returns an exact reduced fraction;
/// it is never converted to floating point or truncated to an integer. This
/// bounded profile does not implement floating/decimal AVG coercion rules.
/// HAVING and ORDER BY may compute aggregates omitted from RETURN. Repeated
/// calls share one internal summary; only returned summaries appear in rows.
/// Grouping keys must still be returned, and bare aliases name only RETURN
/// items. Internal summaries retain their reads, errors and resource costs.
#[derive(Clone)]
pub struct PreparedGraphAggregateText {
    child: PreparedGraphText,
    keys: Vec<usize>,
    summaries: Vec<Summary>,
    output_aggregates: usize,
    names: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
    offset: Number,
    count: Option<Number>,
    having: Vec<Having>,
    having_expression: Option<having::HavingTemplate>,
    ordering: Vec<GraphAggregateOrder>,
}
impl core::fmt::Debug for PreparedGraphAggregateText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphAggregateText")
            .field("columns", &self.names.len())
            .field("parameters", &self.child.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
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
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }

    /// The same aggregate grammar with explicit canonical scalar argument
    /// kinds in MATCH/WHERE and HAVING. Compound HAVING accepts NOT/AND/OR,
    /// parentheses, group/aggregate comparisons and canonical scalar values.
    /// Explicit aggregate calls may be absent from RETURN; their computed
    /// values stay internal unless the same summary is returned explicitly.
    /// Undeclared HAVING parameters remain Int64; pagination remains UInt64.
    /// Declared types participate in the same definition-wide argument table.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphPatternTextError> {
        // Generated names live until from_syntax has made the child owned.
        // They are private metadata, never inserted into statement text or
        // the namespace used to resolve HAVING/ORDER BY aliases.
        let mut hidden_aliases: Vec<String> = Vec::new();
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        parser.parse_head()?;
        // All grouping keys are projected; DISTINCT on the aggregate output
        // is therefore redundant. Neither spelling deduplicates child matches.
        if !parser.take_word("DISTINCT")? {
            parser.take_word("ALL")?;
        }
        let mut returned = Vec::new();
        loop {
            parser.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let item = parser.aggregate_item()?;
            if returned
                .iter()
                .any(|previous: &ReturnItem<'_>| previous.alias.text == item.alias.text)
            {
                return Err(error(
                    item.alias.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                ));
            }
            returned.push(item);
            if !parser.take(b',')? {
                break;
            }
        }
        let mut groups = Vec::new();
        if parser.take_word("GROUP")? {
            parser.word("BY")?;
            loop {
                parser.capacity(
                    groups.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let expression = parser.aggregate_expression()?;
                if groups
                    .iter()
                    .any(|previous: &Expression<'_>| previous.same(expression))
                {
                    return Err(error(
                        expression.variable.at,
                        GraphPatternTextErrorKind::Expected("unique GROUP BY expression"),
                    ));
                }
                groups.push(expression);
                if !parser.take(b',')? {
                    break;
                }
            }
        }
        for item in &returned {
            if item.function.is_none()
                && !groups
                    .iter()
                    .any(|group| group.same(item.expression.expect("key expression")))
            {
                return Err(error(
                    item.alias.at,
                    GraphPatternTextErrorKind::Expected(
                        "nonaggregate RETURN expression in GROUP BY",
                    ),
                ));
            }
        }
        for group in &groups {
            if !returned.iter().any(|item| {
                item.function.is_none()
                    && item
                        .expression
                        .is_some_and(|expression| expression.same(*group))
            }) {
                return Err(error(
                    group.variable.at,
                    GraphPatternTextErrorKind::Expected("GROUP BY expression projected in RETURN"),
                ));
            }
        }
        let mut hidden = Vec::new();
        let (having, having_expression) =
            having::parse(&mut parser, &returned, &groups, &mut hidden)?;
        let mut ordering: Vec<GraphAggregateOrder> = Vec::new();
        if parser.take_word("ORDER")? {
            parser.word("BY")?;
            loop {
                parser.capacity(
                    ordering.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let at = parser.current.at;
                let column = parser.result_column(&returned, &groups, &mut hidden)?;
                if ordering.iter().any(|previous| previous.column == column) {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("unique ORDER BY column"),
                    ));
                }
                let descending = parser.take_word("DESC")?;
                if !descending {
                    parser.take_word("ASC")?;
                }
                let nulls = if parser.take_word("NULLS")? {
                    if parser.take_word("FIRST")? {
                        GraphNullPlacement::First
                    } else {
                        parser.word("LAST")?;
                        GraphNullPlacement::Last
                    }
                } else {
                    GraphNullPlacement::Last
                };
                ordering.push(GraphAggregateOrder {
                    column,
                    descending,
                    nulls,
                });
                if !parser.take(b',')? {
                    break;
                }
            }
        }
        parser.parse_pagination()?;
        parser.end()?;
        let output_aggregates = returned.iter().filter(|item| item.function.is_some()).count();
        if output_aggregates == 0 && hidden.is_empty() {
            return Err(error(
                parser.syntax.return_at,
                GraphPatternTextErrorKind::Expected("at least one aggregate expression"),
            ));
        }
        // A caller may choose any valid public alias, including our usual
        // internal spelling. Choose collision-free names without changing or
        // reinterpreting any of those public aliases. The bounded registry
        // limits this search to at most RETURN width + hidden width choices.
        let mut candidate = 0;
        for _ in &hidden {
            loop {
                let alias = format!("__fgdb_hidden_{candidate}");
                candidate += 1;
                if !returned.iter().any(|item| item.alias.text == alias) {
                    hidden_aliases.push(alias);
                    break;
                }
            }
        }
        // Emit group input expressions first with their actual public aliases.
        // Repeated aggregate arguments reuse one source column. A grouping
        // expression first mentioned inside SUM cannot steal the key's name.
        let mut inputs: Vec<Expression<'_>> = Vec::new();
        for group in &groups {
            let alias = returned
                .iter()
                .find(|item| {
                    item.function.is_none()
                        && item
                            .expression
                            .is_some_and(|expression| expression.same(*group))
                })
                .expect("group projection checked above")
                .alias;
            inputs.push(*group);
            parser.syntax.columns.push(Column {
                variable: group.variable,
                property: group.property,
                alias,
            });
        }
        let keys = (0..groups.len()).collect();
        let mut summaries = Vec::new();
        let mut slots = Vec::new();
        let mut names = Vec::new();
        for item in &returned {
            names.push(item.alias.text.to_owned());
            if let Some(function) = item.function {
                let column = item.expression.map(|expression| {
                    if let Some(at) = inputs.iter().position(|previous| previous.same(expression)) {
                        return at;
                    }
                    let at = inputs.len();
                    inputs.push(expression);
                    parser.syntax.columns.push(Column {
                        variable: expression.variable,
                        property: expression.property,
                        alias: item.alias,
                    });
                    at
                });
                slots.push(GraphAggregateTextSlot::Aggregate(summaries.len()));
                summaries.push(Summary {
                    function,
                    column,
                    alias: item.alias.text.to_owned(),
                });
            } else {
                slots.push(GraphAggregateTextSlot::GroupKey(
                    groups
                        .iter()
                        .position(|group| group.same(item.expression.expect("key expression")))
                        .expect("group checked above"),
                ));
            }
        }
        // Computed summaries form one returned prefix followed by a private
        // suffix. All inputs enter the ordinary owned child, including those
        // used only by a hidden summary, and reuse an existing source column.
        for (item, text) in hidden.iter().zip(&hidden_aliases) {
            let alias = Name { text: text.as_str(), at: item.at };
            let column = item.expression.map(|expression| {
                if let Some(at) = inputs.iter().position(|previous| previous.same(expression)) {
                    return at;
                }
                let at = inputs.len();
                inputs.push(expression);
                parser.syntax.columns.push(Column {
                    variable: expression.variable,
                    property: expression.property,
                    alias,
                });
                at
            });
            summaries.push(Summary {
                function: item.function,
                column,
                alias: text.clone(),
            });
        }
        if parser.syntax.columns.is_empty() {
            // COUNT(*) alone still needs the existing nonempty child shape.
            // This bound identity is not a group key or a counted argument.
            let variable = parser.syntax.variables[0];
            parser.syntax.columns.push(Column {
                variable,
                property: None,
                alias: Name {
                    text: "__count_source",
                    at: variable.at,
                },
            });
        }
        let offset = core::mem::replace(
            &mut parser.syntax.offset,
            Number::Literal(GqlParameterValue::UInt64(0)),
        );
        let count = parser.syntax.count.take();
        parser.syntax.distinct = false;
        let child = PreparedGraphText::from_syntax(statement, parser.syntax, resolve)?;
        Ok(Self {
            child,
            keys,
            summaries,
            output_aggregates,
            names,
            slots,
            offset,
            count,
            having,
            having_expression,
            ordering,
        })
    }

    /// Explicit definition and schema exports; Debug does not expose them.
    #[must_use]
    pub fn statement(&self) -> &str {
        self.child.statement()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.child.parameter_schema()
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.names
    }
    /// RETURN position i selects keys()[k] or values()[a] in the result row.
    #[must_use]
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        &self.slots
    }

    /// One argument validation and one lowering; no parsing, resolving, source
    /// access, or materialization of child matches. Previously bound aggregates
    /// remain immutable. Output pagination does not alter the child pattern.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphAggregate, GraphPatternTextError> {
        let values = self.child.checked_arguments(arguments)?;
        let input = self.child.bind_values(&values)?;
        let summaries: Vec<_> = self.summaries.iter().map(Summary::declaration).collect();
        let having: Vec<_> = self
            .having
            .iter()
            .map(|filter| GraphAggregateFilter {
                column: filter.column,
                test: match &filter.test {
                    HavingTest::Integer { comparison, value } => GraphAggregateTest::Integer {
                        comparison: *comparison,
                        value: i128::from(value.signed(&values)),
                    },
                    HavingTest::IsNull => GraphAggregateTest::IsNull,
                    HavingTest::IsNotNull => GraphAggregateTest::IsNotNull,
                },
            })
            .collect();
        let aggregate = PreparedGraphAggregate::prepare(
            input,
            &self.keys,
            &summaries,
            self.offset.unsigned(&values),
            self.count.as_ref().map(|count| count.unsigned(&values)),
        )
        .and_then(|aggregate| aggregate.with_aggregate_output_prefix(self.output_aggregates))
        .and_then(|aggregate| aggregate.with_result_clauses(&having, &self.ordering))
        .map_err(|kind| {
            error(
                self.child.return_at,
                GraphPatternTextErrorKind::AggregateBuild(kind),
            )
        })?;
        match &self.having_expression {
            Some(expression) => expression.attach(aggregate, &values),
            None => Ok(aggregate),
        }
    }
}

#[derive(Clone, Copy)]
struct Expression<'a> {
    variable: Name<'a>,
    property: Option<Name<'a>>,
}
impl Expression<'_> {
    fn same(self, other: Self) -> bool {
        self.variable.text == other.variable.text
            && self.property.map(|name| name.text) == other.property.map(|name| name.text)
    }
}
struct ReturnItem<'a> {
    expression: Option<Expression<'a>>,
    function: Option<GraphAggregateFunction>,
    alias: Name<'a>,
}
/// Shared first-use registry for explicit nonreturned aggregate calls. Its
/// entries retain source offsets; aliases are assigned only after parsing.
struct HiddenSummary<'a> {
    expression: Option<Expression<'a>>,
    function: GraphAggregateFunction,
    at: usize,
}

impl<'a> Parser<'a> {
    fn expression_after_name(
        &mut self,
        variable: Name<'a>,
    ) -> Result<Expression<'a>, GraphPatternTextError> {
        if !self
            .syntax
            .variables
            .iter()
            .any(|name| name.text == variable.text)
        {
            return Err(error(
                variable.at,
                GraphPatternTextErrorKind::UnknownVariable,
            ));
        }
        let property = if self.take(b'.')? {
            Some(self.name()?)
        } else {
            None
        };
        Ok(Expression { variable, property })
    }
    fn aggregate_expression(&mut self) -> Result<Expression<'a>, GraphPatternTextError> {
        let variable = self.name()?;
        self.expression_after_name(variable)
    }
    fn aggregate_item(&mut self) -> Result<ReturnItem<'a>, GraphPatternTextError> {
        let name = self.name()?;
        let (expression, function, default_alias) = if self.take(b'(')? {
            let (expression, function) = self.aggregate_call(name)?;
            let alias = match function {
                GraphAggregateFunction::CountRows
                | GraphAggregateFunction::Count
                | GraphAggregateFunction::CountDistinct => "count",
                GraphAggregateFunction::SumInt
                | GraphAggregateFunction::SumIntDistinct => "sum",
                GraphAggregateFunction::AverageInt
                | GraphAggregateFunction::AverageIntDistinct => "avg",
                GraphAggregateFunction::Min => "min",
                GraphAggregateFunction::Max => "max",
            };
            (
                expression,
                Some(function),
                Name {
                    text: alias,
                    at: name.at,
                },
            )
        } else {
            let expression = self.expression_after_name(name)?;
            (Some(expression), None, expression.property.unwrap_or(name))
        };
        let alias = if self.take_word("AS")? {
            self.name()?
        } else {
            default_alias
        };
        Ok(ReturnItem {
            expression,
            function,
            alias,
        })
    }

    /// Called after the opening parenthesis. RETURN and post-aggregate
    /// references share exactly one aggregate-function grammar.
    fn aggregate_call(
        &mut self,
        name: Name<'a>,
    ) -> Result<(Option<Expression<'a>>, GraphAggregateFunction), GraphPatternTextError> {
        let function = if name.text.eq_ignore_ascii_case("COUNT") {
            GraphAggregateFunction::Count
        } else if name.text.eq_ignore_ascii_case("SUM") || name.text.eq_ignore_ascii_case("SUM_INT")
        {
            GraphAggregateFunction::SumInt
        } else if name.text.eq_ignore_ascii_case("AVG") || name.text.eq_ignore_ascii_case("AVG_INT")
        {
            GraphAggregateFunction::AverageInt
        } else if name.text.eq_ignore_ascii_case("MIN") {
            GraphAggregateFunction::Min
        } else if name.text.eq_ignore_ascii_case("MAX") {
            GraphAggregateFunction::Max
        } else {
            return Err(error(
                name.at,
                GraphPatternTextErrorKind::Expected("COUNT, SUM, SUM_INT, AVG, AVG_INT, MIN or MAX"),
            ));
        };
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_word("ALL")?;
        }
        let result = if self.take(b'*')? {
            if function != GraphAggregateFunction::Count || distinct {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("COUNT(*) without argument DISTINCT"),
                ));
            }
            (None, GraphAggregateFunction::CountRows)
        } else {
            let function = if distinct {
                match function {
                    GraphAggregateFunction::Count => GraphAggregateFunction::CountDistinct,
                    GraphAggregateFunction::SumInt => GraphAggregateFunction::SumIntDistinct,
                    GraphAggregateFunction::AverageInt => GraphAggregateFunction::AverageIntDistinct,
                    _ => return Err(error(
                        name.at,
                        GraphPatternTextErrorKind::Expected("DISTINCT argument for COUNT, SUM or AVG"),
                    )),
                }
            } else {
                function
            };
            (
                Some(self.aggregate_expression()?),
                function,
            )
        };
        self.punct(b')', ")")?;
        Ok(result)
    }

    /// Resolve public aliases and expressions. An explicit aggregate call
    /// first reuses a returned summary, then a registered private summary.
    /// HAVING and ORDER BY share this registry; generated internal names never
    /// enter alias resolution. Plain properties must remain projected keys.
    fn result_column(
        &mut self,
        returned: &[ReturnItem<'a>],
        groups: &[Expression<'a>],
        hidden: &mut Vec<HiddenSummary<'a>>,
    ) -> Result<GraphAggregateColumn, GraphPatternTextError> {
        let name = self.name()?;
        if self.take(b'(')? {
            let (expression, function) = self.aggregate_call(name)?;
            let visible = returned.iter().filter(|item| item.function.is_some());
            if let Some(at) = visible.clone().position(|item| {
                item.function == Some(function) && same_expression(item.expression, expression)
            }) {
                return Ok(GraphAggregateColumn::Aggregate(at));
            }
            let prefix = visible.count();
            if let Some(at) = hidden.iter().position(|item| {
                item.function == function && same_expression(item.expression, expression)
            }) {
                return Ok(GraphAggregateColumn::Aggregate(prefix + at));
            }
            // Bound ALL computed columns before catalog resolution, not just
            // the visible schema. Distinct function/argument pairs consume
            // state even when no output row copies their results.
            self.capacity(
                groups.len() + prefix + hidden.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let at = prefix + hidden.len();
            hidden.push(HiddenSummary { expression, function, at: name.at });
            return Ok(GraphAggregateColumn::Aggregate(at));
        }
        let selected = if !self.is_punct(b'.') && returned.iter().any(|item| item.alias.text == name.text) {
            returned
                .iter()
                .position(|item| item.alias.text == name.text)
        } else {
            let expression = self.expression_after_name(name)?;
            returned.iter().position(|item| {
                item.function.is_none()
                    && item
                        .expression
                        .is_some_and(|previous| previous.same(expression))
            })
        }
        .ok_or_else(|| {
            error(
                name.at,
                GraphPatternTextErrorKind::Expected("projected group key or aggregate"),
            )
        })?;
        let item = &returned[selected];
        if item.function.is_some() {
            Ok(GraphAggregateColumn::Aggregate(
                returned[..selected]
                    .iter()
                    .filter(|item| item.function.is_some())
                    .count(),
            ))
        } else {
            let expression = item
                .expression
                .expect("a nonaggregate return item has an expression");
            Ok(GraphAggregateColumn::GroupKey(
                groups
                    .iter()
                    .position(|group| group.same(expression))
                    .expect("all nonaggregate return expressions were validated as keys"),
            ))
        }
    }
}

fn same_expression(left: Option<Expression<'_>>, right: Option<Expression<'_>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.same(right),
        (None, None) => true,
        _ => false,
    }
}
