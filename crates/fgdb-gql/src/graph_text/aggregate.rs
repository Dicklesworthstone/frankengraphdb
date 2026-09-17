//! Aggregate RETURN and explicit GROUP BY over the shared graph text parser.
//! No source rewriting, second lexer or catalog re-resolution. Ordinary inputs
//! keep streaming; computed input uses the existing bounded projection path.
//! Group pagination is never pushed into the matching child.

mod having;

use super::*;
use crate::set_text::{ReadProjectionTemplate, ReadValueTemplate};
use crate::{
    GraphAggregate, GraphAggregateColumn, GraphAggregateFilter, GraphAggregateFunction,
    GraphAggregateOrder, GraphAggregateTest, GraphNullPlacement, MAX_AGGREGATE_FILTERS,
    PreparedGraphAggregate,
};

/// Position of a textual RETURN item in the public aggregate result. Hidden
/// grouping keys have no slot; clause indices use the separate evaluation schema.
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

/// Parse-once grouped text definition. Binding returns the existing aggregate
/// with one shared governed source at execution time. The explicit profile
/// requires every nonaggregate RETURN expression in GROUP BY, but grouping
/// expressions may be omitted from RETURN. Grouping uses expressions, not aliases.
/// SUM/SUM_INT and AVG/AVG_INT accept integer/null arguments; argument DISTINCT
/// follows expression evaluation. AVG returns an exact reduced fraction.
///
/// Arguments and grouping keys support the same literals, typed parameters,
/// checked i64 arithmetic, ABS, NULLIF and COALESCE as computed read projections.
/// A computed nonaggregate RETURN key requires AS. Repeated expressions share
/// a projection slot; repeated aggregate calls share their existing summary.
/// Plain column inputs retain the ordinary streaming path and transcript.
/// Computed inputs use bounded materialization, not spill or numeric coercion.
///
/// HAVING and ORDER BY may compute aggregates omitted from RETURN. Hidden keys
/// and summaries retain all reads, errors and resource costs. ALL preserves
/// distinct groups with equal visible cells; DISTINCT removes duplicate visible
/// rows after filtering/ranking and before pagination, never from child matches.
#[derive(Clone)]
pub struct PreparedGraphAggregateText {
    child: PreparedGraphText,
    input_projection: Option<Vec<ReadProjectionTemplate>>,
    keys: Vec<usize>,
    output_keys: Vec<usize>,
    summaries: Vec<Summary>,
    output_aggregates: usize,
    output_distinct: bool,
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

    /// One parameter table spans MATCH, computed grouping/arguments, HAVING
    /// and pagination. Every explicit occurrence is registered once, even when
    /// repeated scalar programs or hidden aggregate calls share storage.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphPatternTextError> {
        // Generated names live until from_syntax has made the child owned.
        // They are private metadata, never inserted into statement text or
        // the namespace used to resolve HAVING/ORDER BY aliases.
        let mut hidden_aliases: Vec<String> = Vec::new();
        let mut key_aliases: Vec<(String, usize)> = Vec::new();
        let mut source_aliases: Vec<String> = Vec::new();
        let mut computed = ComputedInputs::default();
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        parser.parse_head()?;
        let distinct = parser.take_word("DISTINCT")?;
        if !distinct {
            parser.take_word("ALL")?;
        }
        let mut returned = Vec::new();
        loop {
            parser.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let item = parser.aggregate_item(&mut computed)?;
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
                let expression = parser.aggregate_expression(&mut computed)?;
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
        // Hidden grouping keys consume real evaluation columns. Bound their
        // combined width with returned summaries before any catalog access.
        for (at, _) in returned
            .iter()
            .filter(|item| item.function.is_some())
            .enumerate()
        {
            parser.capacity(
                groups.len() + at,
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
        }
        let mut hidden = Vec::new();
        let (having, having_expression) =
            having::parse(&mut parser, &returned, &groups, &mut hidden, &mut computed)?;
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
                let column = parser.result_column(&returned, &groups, &mut hidden, &mut computed)?;
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
        let output_aggregates = returned
            .iter()
            .filter(|item| item.function.is_some())
            .count();
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
        // Preserve the original GROUP BY order, including hidden leading keys.
        // Public key slots are a projection of this evaluation order, not a
        // renumbering of the HAVING/ORDER BY namespace. Private key names use
        // a disjoint prefix from private summary names and skip public aliases.
        let mut output_keys = Vec::new();
        let mut candidate = 0;
        for (at, group) in groups.iter().enumerate() {
            if let Some(item) = returned.iter().find(|item| {
                item.function.is_none()
                    && item
                        .expression
                        .is_some_and(|expression| expression.same(*group))
            }) {
                output_keys.push(at);
                key_aliases.push((item.alias.text.to_owned(), item.alias.at));
            } else {
                loop {
                    let alias = format!("__fgdb_group_{candidate}");
                    candidate += 1;
                    if !returned.iter().any(|item| item.alias.text == alias) {
                        key_aliases.push((alias, group.variable.at));
                        break;
                    }
                }
            }
        }
        // With every grouping key visible, output DISTINCT is redundant.
        // Preserve that existing profile's exact bound definitions/transcripts.
        let output_distinct = distinct && output_keys.len() != groups.len();
        // Emit all logical group inputs before aggregate arguments. Until the
        // computed/source split below, these Columns are only schema metadata.
        let mut inputs: Vec<Expression<'_>> = Vec::new();
        for (group, (text, at)) in groups.iter().zip(&key_aliases) {
            let alias = Name {
                text: text.as_str(),
                at: *at,
            };
            inputs.push(*group);
            parser.syntax.columns.push(Column {
                variable: group.variable,
                property: group.property,
                path: None,
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
                        path: None,
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
                    output_keys
                        .iter()
                        .position(|at| groups[*at].same(item.expression.expect("key expression")))
                        .expect("group checked above"),
                ));
            }
        }
        // Computed summaries form one returned prefix followed by a private
        // suffix. Repeated argument expressions retain one logical input slot.
        for (item, text) in hidden.iter().zip(&hidden_aliases) {
            let alias = Name {
                text: text.as_str(),
                at: item.at,
            };
            let column = item.expression.map(|expression| {
                if let Some(at) = inputs.iter().position(|previous| previous.same(expression)) {
                    return at;
                }
                let at = inputs.len();
                inputs.push(expression);
                parser.syntax.columns.push(Column {
                    variable: expression.variable,
                    property: expression.property,
                    path: None,
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
        let input_projection = if computed.operands.is_empty() {
            None
        } else {
            let mut projection = Vec::new();
            for (expression, column) in inputs.iter().zip(&parser.syntax.columns) {
                let value = match expression.computed {
                    Some(index) => computed.operands[index].clone(),
                    None => {
                        let index = computed.sources.iter().position(|(variable, property)| {
                            variable.text == expression.variable.text
                                && property.map(|name| name.text) == expression.property.map(|name| name.text)
                        }).expect("every plain argument registered its source");
                        ReadValueTemplate::Column(index)
                    }
                };
                projection.push(ReadProjectionTemplate { name: column.alias.text.to_owned(), value });
            }
            if computed.sources.is_empty() {
                // SUM(1), COUNT(NULL), and constant grouping still visit every
                // match, including isolates and duplicate WALK occurrences.
                computed.sources.push((parser.syntax.variables[0], None));
            }
            source_aliases.extend((0..computed.sources.len()).map(|index| format!("__aggregate_source_{index}")));
            parser.syntax.columns = computed.sources.iter().zip(&source_aliases).map(|(&(variable, property), alias)| Column {
                variable, property, alias: Name { text: alias.as_str(), at: variable.at },
                path: None,
            }).collect();
            Some(projection)
        };
        if parser.syntax.columns.is_empty() {
            // COUNT(*) alone still needs the existing nonempty child shape.
            // This bound identity is not a group key or a counted argument.
            let variable = parser.syntax.variables[0];
            parser.syntax.columns.push(Column {
                variable,
                property: None,
                path: None,
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
            input_projection,
            keys,
            output_keys,
            summaries,
            output_aggregates,
            output_distinct,
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
        let offset = self.offset.unsigned(&values);
        let count = self.count.as_ref().map(|count| count.unsigned(&values));
        let prepared = match &self.input_projection {
            Some(projection) => PreparedGraphAggregate::prepare_projected(
                input, Self::bind_input_projection(projection, &values)?, &self.keys, &summaries, offset, count,
            ),
            None => PreparedGraphAggregate::prepare(input, &self.keys, &summaries, offset, count),
        };
        let aggregate = prepared
        .and_then(|aggregate| aggregate.with_key_output_columns(&self.output_keys))
        .and_then(|aggregate| aggregate.with_aggregate_output_prefix(self.output_aggregates))
        .map(|aggregate| aggregate.with_distinct_output(self.output_distinct))
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

/// Parse-only registry shared across RETURN, GROUP BY, HAVING and ORDER BY.
/// The scalar compiler owns registration and ignores diagnostic offsets when
/// interning programs; parameters retain their actual explicit-use counts.
#[derive(Default)]
pub(in crate::graph_text) struct ComputedInputs<'a> {
    pub(in crate::graph_text) sources: Vec<(Name<'a>, Option<Name<'a>>)>,
    pub(in crate::graph_text) operands: Vec<ReadValueTemplate>,
}

#[derive(Clone, Copy)]
pub(in crate::graph_text) struct Expression<'a> {
    pub(in crate::graph_text) variable: Name<'a>,
    pub(in crate::graph_text) property: Option<Name<'a>>,
    pub(in crate::graph_text) computed: Option<usize>,
}
impl Expression<'_> {
    fn same(self, other: Self) -> bool {
        match (self.computed, other.computed) {
            (Some(left), Some(right)) => left == right,
            (None, None) => self.variable.text == other.variable.text
                && self.property.map(|name| name.text) == other.property.map(|name| name.text),
            _ => false,
        }
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
    fn aggregate_expression(&mut self, computed: &mut ComputedInputs<'a>) -> Result<Expression<'a>, GraphPatternTextError> {
        self.aggregate_scalar_expression(computed)
    }

    fn starts_aggregate_call(&self) -> Result<bool, GraphPatternTextError> {
        let TokenKind::Word(word) = self.current.kind else { return Ok(false); };
        Ok(["COUNT", "SUM", "SUM_INT", "AVG", "AVG_INT", "MIN", "MAX"].iter()
            .any(|name| word.eq_ignore_ascii_case(name))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')))
    }

    fn aggregate_item(&mut self, computed: &mut ComputedInputs<'a>) -> Result<ReturnItem<'a>, GraphPatternTextError> {
        let at = self.current.at;
        let (expression, function, default_alias) = if self.starts_aggregate_call()? {
            let name = self.name()?;
            self.punct(b'(', "(")?;
            let (expression, function) = self.aggregate_call(name, computed)?;
            let alias = match function {
                GraphAggregateFunction::CountRows
                | GraphAggregateFunction::Count
                | GraphAggregateFunction::CountDistinct => "count",
                GraphAggregateFunction::SumInt | GraphAggregateFunction::SumIntDistinct => "sum",
                GraphAggregateFunction::AverageInt | GraphAggregateFunction::AverageIntDistinct => "avg",
                GraphAggregateFunction::Min => "min",
                GraphAggregateFunction::Max => "max",
            };
            (expression, Some(function), Some(Name { text: alias, at: name.at }))
        } else {
            let expression = self.aggregate_expression(computed)?;
            let default = expression.computed.is_none().then_some(expression.property.unwrap_or(expression.variable));
            (Some(expression), None, default)
        };
        let alias = if self.take_word("AS")? {
            self.name()?
        } else {
            default_alias.ok_or_else(|| error(at,
                GraphPatternTextErrorKind::Expected("AS alias for computed grouping output")))?
        };
        Ok(ReturnItem { expression, function, alias })
    }

    /// Called after the opening parenthesis. RETURN and post-aggregate
    /// references share exactly one aggregate-function grammar.
    fn aggregate_call(
        &mut self,
        name: Name<'a>,
        computed: &mut ComputedInputs<'a>,
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
                GraphPatternTextErrorKind::Expected(
                    "COUNT, SUM, SUM_INT, AVG, AVG_INT, MIN or MAX",
                ),
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
                    _ => {
                        return Err(error(
                            name.at,
                            GraphPatternTextErrorKind::Expected(
                                "DISTINCT argument for COUNT, SUM or AVG",
                            ),
                        ));
                    }
                }
            } else {
                function
            };
            (Some(self.aggregate_expression(computed)?), function)
        };
        self.punct(b')', ")")?;
        Ok(result)
    }

    /// Resolve public aliases and expressions. Explicit aggregate calls first
    /// reuse returned summaries, then registered private summaries. Computed
    /// grouping expressions share the same input registry as their RETURN use.
    fn result_column(
        &mut self,
        returned: &[ReturnItem<'a>],
        groups: &[Expression<'a>],
        hidden: &mut Vec<HiddenSummary<'a>>,
        computed: &mut ComputedInputs<'a>,
    ) -> Result<GraphAggregateColumn, GraphPatternTextError> {
        let at = self.current.at;
        if self.starts_aggregate_call()? {
            let name = self.name()?;
            self.punct(b'(', "(")?;
            let (expression, function) = self.aggregate_call(name, computed)?;
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
            self.capacity(
                groups.len() + prefix + hidden.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let at = prefix + hidden.len();
            hidden.push(HiddenSummary { expression, function, at: name.at });
            return Ok(GraphAggregateColumn::Aggregate(at));
        }
        if let TokenKind::Word(word) = self.current.kind
            && !matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.' | b'('))
            && let Some(selected) = returned.iter().position(|item| item.alias.text == word)
        {
            self.advance()?;
            let item = &returned[selected];
            if item.function.is_some() {
                return Ok(GraphAggregateColumn::Aggregate(
                    returned[..selected].iter().filter(|item| item.function.is_some()).count(),
                ));
            }
            let expression = item.expression.expect("nonaggregate return expression");
            return Ok(GraphAggregateColumn::GroupKey(
                groups.iter().position(|group| group.same(expression))
                    .expect("nonaggregate outputs were validated as keys"),
            ));
        }
        let expression = self.aggregate_expression(computed)?;
        groups.iter().position(|group| group.same(expression)).map(GraphAggregateColumn::GroupKey)
            .ok_or_else(|| error(at, GraphPatternTextErrorKind::Expected("GROUP BY expression or aggregate")))
    }
}

fn same_expression(left: Option<Expression<'_>>, right: Option<Expression<'_>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.same(right),
        (None, None) => true,
        _ => false,
    }
}
