//! Compound query preparation over the shared graph-text lexer and compiler.
//! Syntax is parsed before catalog access; execution sees only PreparedGraphSet.

use crate::algebra::{GraphOrderError, GraphValueOrder};
use crate::set_ops::MAX_GRAPH_SET_DEPTH;
use crate::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters, GraphPatternTextError,
    GraphPatternTextErrorKind, GraphSetBuildError, GraphSetColumnType, GraphSetOperation,
    GraphSetQuantifier, GraphSymbol, GraphSymbolKind, MAX_GRAPH_SET_OPERANDS, PreparedGraphSet,
    PreparedGraphText,
};
use std::collections::{BTreeMap, BTreeSet};

/// Original byte offsets, never query fragments, names, or argument payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSetTextError {
    pub offset: usize,
    pub kind: GraphSetTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetTextErrorKind {
    Expected(&'static str),
    Pattern(GraphPatternTextErrorKind),
    SetBuild(GraphSetBuildError),
    OrderBuild(GraphOrderError),
    ProjectionBuild(crate::GraphSetProjectionError),
    FilterBuild(crate::GraphSetFilterError),
    IntegerExpression(crate::GraphIntegerBuildError),
    IntegerOperand,
    IntegerNesting { limit: usize },
}
impl core::fmt::Display for GraphSetTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph set text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphSetTextError {}
impl From<GraphPatternTextError> for GraphSetTextError {
    fn from(error: GraphPatternTextError) -> Self {
        pattern_error(0, error)
    }
}
fn fail(offset: usize, kind: GraphSetTextErrorKind) -> GraphSetTextError {
    GraphSetTextError { offset, kind }
}
fn expected(offset: usize, item: &'static str) -> GraphSetTextError {
    fail(offset, GraphSetTextErrorKind::Expected(item))
}
fn pattern_error(base: usize, error: GraphPatternTextError) -> GraphSetTextError {
    fail(
        base + error.offset,
        GraphSetTextErrorKind::Pattern(error.kind),
    )
}
fn rebase_error(base: usize, mut error: GraphSetTextError) -> GraphSetTextError {
    error.offset += base;
    error
}
fn pattern_kind(offset: usize, kind: GraphPatternTextErrorKind) -> GraphSetTextError {
    fail(offset, GraphSetTextErrorKind::Pattern(kind))
}

// Native leaf preparation owns these immutable templates. Only shared scalar
// bytecode and catalog-bound GLA projections reach the execution boundary.
#[derive(Clone)]
pub(crate) enum ReadValueTemplate {
    Column(usize),
    Literal(crate::GqlScalarParameter),
    Parameter {
        index: usize,
        at: usize,
    },
    Integer {
        program: Vec<crate::mutation_text::MutationIntegerTemplateOp>,
        at: usize,
    },
}
#[derive(Clone)]
pub(crate) struct ReadProjectionTemplate {
    pub(crate) name: String,
    pub(crate) value: ReadValueTemplate,
}
#[derive(Clone)]
pub(crate) enum ReadPageNumber {
    Literal(u64),
    Parameter(usize),
}
#[derive(Clone)]
pub(crate) enum ReadFilterOperand {
    Column(usize),
    Literal(crate::GqlScalarParameter),
    Parameter { index: usize, at: usize },
}
#[derive(Clone)]
pub(crate) enum ReadFilterOp {
    Compare {
        left: ReadFilterOperand,
        comparison: crate::algebra::IntegerComparison,
        right: ReadFilterOperand,
    },
    IsNull {
        operand: ReadFilterOperand,
        is_null: bool,
    },
    Truth(Option<bool>),
    Not,
    And,
    Or,
}
#[derive(Clone)]
pub(crate) enum ReadStageTemplate {
    Project {
        at: usize,
        projection: Vec<ReadProjectionTemplate>,
        quantifier: GraphSetQuantifier,
    },
    Filter {
        at: usize,
        code: Vec<ReadFilterOp>,
    },
    Page {
        at: usize,
        order: Vec<GraphValueOrder>,
        offset: ReadPageNumber,
        count: Option<ReadPageNumber>,
    },
}
#[derive(Clone)]
pub(crate) struct BoundSetTextInput {
    pub(crate) selection: PreparedGraphText,
    pub(crate) projection: Option<Vec<ReadProjectionTemplate>>,
    pub(crate) quantifier: GraphSetQuantifier,
    pub(crate) pipeline: Vec<ReadStageTemplate>,
}

// A token view, not a lexer. Only graph_text's existing Lexer constructs these.
// Quoted strings remain opaque tokens and cannot introduce set delimiters.
#[derive(Clone, Copy)]
pub(crate) enum TextKind<'a> {
    Word(&'a str),
    Digits(&'a str),
    Parameter(&'a str),
    Quoted,
    Punct(u8),
    End,
}
#[derive(Clone, Copy)]
pub(crate) struct TextToken<'a> {
    pub(crate) kind: TextKind<'a>,
    pub(crate) at: usize,
}
impl TextToken<'_> {
    fn word(self, expected: &str) -> bool {
        matches!(self.kind, TextKind::Word(word) if word.eq_ignore_ascii_case(expected))
    }
    fn punct(self, expected: u8) -> bool {
        matches!(self.kind, TextKind::Punct(ch) if ch == expected)
    }
}

#[derive(Clone)]
enum PageNumber {
    Literal(u64),
    Parameter(String),
}
impl PageNumber {
    fn value(&self, arguments: &GqlParameters) -> u64 {
        match self {
            Self::Literal(value) => *value,
            Self::Parameter(name) => match arguments.get(name) {
                Some(GqlParameterValue::UInt64(value)) => value,
                _ => unreachable!("the complete argument schema was validated before binding"),
            },
        }
    }
}
#[derive(Clone)]
struct OrderKey {
    name: String,
    at: usize,
    order: GraphValueOrder,
}
#[derive(Clone)]
enum NodeKind {
    Leaf(usize),
    Scope(Box<Node>),
    Binary {
        operation: GraphSetOperation,
        quantifier: GraphSetQuantifier,
        left: Box<Node>,
        right: Box<Node>,
    },
}
#[derive(Clone)]
struct Node {
    kind: NodeKind,
    at: usize,
    depth: usize,
    order: Vec<OrderKey>,
    offset: PageNumber,
    count: Option<PageNumber>,
}
impl Node {
    fn new(kind: NodeKind, at: usize, depth: usize) -> Result<Self, GraphSetTextError> {
        if depth > MAX_GRAPH_SET_DEPTH {
            return Err(fail(
                at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep {
                    limit: MAX_GRAPH_SET_DEPTH,
                    observed: depth,
                }),
            ));
        }
        Ok(Self {
            kind,
            at,
            depth,
            order: Vec::new(),
            offset: PageNumber::Literal(0),
            count: None,
        })
    }
    fn validate(&mut self, schemas: &[Schema]) -> Result<usize, GraphSetTextError> {
        let first = match &mut self.kind {
            NodeKind::Leaf(at) => {
                self.depth = schemas[*at].depth;
                *at
            }
            NodeKind::Scope(input) => {
                let first = input.validate(schemas)?;
                self.depth = input.depth + 1;
                first
            }
            NodeKind::Binary { left, right, .. } => {
                let l = left.validate(schemas)?;
                let r = right.validate(schemas)?;
                self.depth = 1 + left.depth.max(right.depth);
                let (left, right) = (&schemas[l].types, &schemas[r].types);
                if left.len() != right.len() {
                    return Err(fail(
                        self.at,
                        GraphSetTextErrorKind::SetBuild(GraphSetBuildError::ColumnCount {
                            left: left.len(),
                            right: right.len(),
                        }),
                    ));
                }
                for (column, (&left, &right)) in left.iter().zip(right).enumerate() {
                    if left != right {
                        return Err(fail(
                            self.at,
                            GraphSetTextErrorKind::SetBuild(GraphSetBuildError::ColumnType {
                                column,
                                left,
                                right,
                            }),
                        ));
                    }
                }
                l
            }
        };
        if self.depth > MAX_GRAPH_SET_DEPTH {
            return Err(fail(
                self.at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep {
                    limit: MAX_GRAPH_SET_DEPTH,
                    observed: self.depth,
                }),
            ));
        }
        let schema = &schemas[first];
        let mut used = BTreeSet::new();
        for key in &mut self.order {
            let column = schema
                .columns
                .iter()
                .position(|name| *name == key.name)
                .ok_or_else(|| expected(key.at, "a leftmost output column name"))?;
            if !used.insert(column) {
                return Err(fail(
                    key.at,
                    GraphSetTextErrorKind::OrderBuild(GraphOrderError::DuplicateColumn { column }),
                ));
            }
            key.order.column = column;
        }
        Ok(first)
    }
    fn bind(
        &self,
        inputs: &mut [Option<PreparedGraphSet>],
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut bound = match &self.kind {
            NodeKind::Leaf(at) => inputs[*at].take().expect("each syntax leaf is unique"),
            NodeKind::Scope(input) => input
                .bind(inputs, arguments)?
                .nested()
                .map_err(|kind| fail(self.at, GraphSetTextErrorKind::SetBuild(kind)))?,
            NodeKind::Binary {
                operation,
                quantifier,
                left,
                right,
            } => {
                let left = left.bind(inputs, arguments)?;
                let right = right.bind(inputs, arguments)?;
                left.combine(*operation, *quantifier, right)
                    .map_err(|kind| fail(self.at, GraphSetTextErrorKind::SetBuild(kind)))?
            }
        };
        if !self.order.is_empty() {
            let order: Vec<_> = self.order.iter().map(|key| key.order).collect();
            bound = bound
                .with_order_by(&order)
                .map_err(|kind| fail(self.at, GraphSetTextErrorKind::OrderBuild(kind)))?;
        }
        Ok(bound.with_page(
            self.offset.value(arguments),
            self.count.as_ref().map(|n| n.value(arguments)),
        ))
    }
}
struct Schema {
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    depth: usize,
}
struct Span {
    start: usize,
    end: usize,
    first_token: usize,
    last_token: usize,
}
struct Composition<'a> {
    tokens: Vec<TextToken<'a>>,
    at: usize,
    spans: Vec<Span>,
    page_parameters: Vec<(usize, GqlParameterSpec)>,
}
impl<'a> Composition<'a> {
    fn current(&self) -> TextToken<'a> {
        self.tokens[self.at]
    }
    fn advance(&mut self) {
        if !matches!(self.current().kind, TextKind::End) {
            self.at += 1;
        }
    }
    fn take_word(&mut self, word: &str) -> bool {
        if !self.current().word(word) {
            return false;
        }
        self.advance();
        true
    }
    fn take(&mut self, ch: u8) -> bool {
        if !self.current().punct(ch) {
            return false;
        }
        self.advance();
        true
    }
    fn require(&mut self, ch: u8, item: &'static str) -> Result<(), GraphSetTextError> {
        if self.take(ch) {
            Ok(())
        } else {
            Err(expected(self.current().at, item))
        }
    }
    fn expression(&mut self, depth: usize) -> Result<Node, GraphSetTextError> {
        if depth >= MAX_GRAPH_SET_DEPTH {
            return Err(fail(
                self.current().at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep {
                    limit: MAX_GRAPH_SET_DEPTH,
                    observed: depth + 1,
                }),
            ));
        }
        let mut node = self.intersection(depth)?;
        while self.current().word("UNION") || self.current().word("EXCEPT") {
            let token = self.current();
            let operation = if token.word("UNION") {
                GraphSetOperation::Union
            } else {
                GraphSetOperation::Except
            };
            self.advance();
            let quantifier = self.quantifier();
            let right = self.intersection(depth)?;
            let height = 1 + node.depth.max(right.depth);
            node = Node::new(
                NodeKind::Binary {
                    operation,
                    quantifier,
                    left: Box::new(node),
                    right: Box::new(right),
                },
                token.at,
                height,
            )?;
        }
        self.tail(&mut node)?;
        Ok(node)
    }
    fn intersection(&mut self, depth: usize) -> Result<Node, GraphSetTextError> {
        let mut node = self.term(depth)?;
        while self.current().word("INTERSECT") {
            let at = self.current().at;
            self.advance();
            let quantifier = self.quantifier();
            let right = self.term(depth)?;
            let height = 1 + node.depth.max(right.depth);
            node = Node::new(
                NodeKind::Binary {
                    operation: GraphSetOperation::Intersect,
                    quantifier,
                    left: Box::new(node),
                    right: Box::new(right),
                },
                at,
                height,
            )?;
        }
        Ok(node)
    }
    fn quantifier(&mut self) -> GraphSetQuantifier {
        if self.take_word("ALL") {
            GraphSetQuantifier::All
        } else {
            self.take_word("DISTINCT");
            GraphSetQuantifier::Distinct
        }
    }
    fn term(&mut self, depth: usize) -> Result<Node, GraphSetTextError> {
        let at = self.current().at;
        if self.take(b'(') {
            let input = self.expression(depth + 1)?;
            self.require(b')', "closing set-expression parenthesis")?;
            let height = input.depth + 1;
            return Node::new(NodeKind::Scope(Box::new(input)), at, height);
        }
        if !self.current().word("MATCH") {
            return Err(expected(at, "MATCH or parenthesized set expression"));
        }
        if self.spans.len() == MAX_GRAPH_SET_OPERANDS {
            return Err(fail(
                at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooManyOperands {
                    limit: MAX_GRAPH_SET_OPERANDS,
                    observed: self.spans.len() + 1,
                }),
            ));
        }
        let first_token = self.at;
        let mut delimiters = Vec::new();
        let mut returned = false;
        loop {
            let token = self.current();
            if matches!(token.kind, TextKind::End) {
                break;
            }
            if delimiters.is_empty() {
                if token.punct(b')') {
                    break;
                }
                if returned
                    && (token.word("ORDER")
                        || token.word("SKIP")
                        || token.word("LIMIT")
                        || self.set_separator())
                {
                    break;
                }
                if token.word("RETURN") {
                    returned = true;
                }
            }
            match token.kind {
                TextKind::Punct(ch @ (b'(' | b'[' | b'{')) => {
                    if delimiters.len() == MAX_GRAPH_SET_DEPTH {
                        return Err(expected(token.at, "bounded graph-expression nesting"));
                    }
                    delimiters.push(ch);
                }
                TextKind::Punct(ch @ (b')' | b']' | b'}')) => {
                    let opening = match ch {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if delimiters.pop() != Some(opening) {
                        return Err(expected(token.at, "matching graph-expression delimiter"));
                    }
                }
                _ => {}
            }
            self.advance();
        }
        if !delimiters.is_empty() {
            return Err(expected(
                self.current().at,
                "closing graph-expression delimiter",
            ));
        }
        let leaf = self.spans.len();
        self.spans.push(Span {
            start: at,
            end: self.current().at,
            first_token,
            last_token: self.at,
        });
        Node::new(NodeKind::Leaf(leaf), at, 1)
    }
    fn set_separator(&self) -> bool {
        let token = self.current();
        if !(token.word("UNION") || token.word("INTERSECT") || token.word("EXCEPT")) {
            return false;
        }
        if self.at > 0
            && (self.tokens[self.at - 1].punct(b'.') || self.tokens[self.at - 1].word("AS"))
        {
            return false;
        }
        let mut next = self.at + 1;
        if self
            .tokens
            .get(next)
            .is_some_and(|token| token.word("ALL") || token.word("DISTINCT"))
        {
            next += 1;
        }
        self.tokens
            .get(next)
            .is_some_and(|token| token.word("MATCH") || token.punct(b'('))
    }
    fn tail(&mut self, node: &mut Node) -> Result<(), GraphSetTextError> {
        if self.take_word("ORDER") {
            if !self.take_word("BY") {
                return Err(expected(self.current().at, "BY"));
            }
            loop {
                let at = self.current().at;
                let TextKind::Word(name) = self.current().kind else {
                    return Err(expected(at, "output column name"));
                };
                let name = name.to_owned();
                self.advance();
                let descending = self.take_word("DESC");
                if !descending {
                    self.take_word("ASC");
                }
                let nulls_first = if self.take_word("NULLS") {
                    if self.take_word("FIRST") {
                        true
                    } else if self.take_word("LAST") {
                        false
                    } else {
                        return Err(expected(self.current().at, "FIRST or LAST"));
                    }
                } else {
                    false
                };
                if node.order.len() == crate::algebra::MAX_PATTERN_VERTICES {
                    return Err(fail(
                        at,
                        GraphSetTextErrorKind::OrderBuild(GraphOrderError::TooManyColumns {
                            limit: crate::algebra::MAX_PATTERN_VERTICES,
                            observed: node.order.len() + 1,
                        }),
                    ));
                }
                node.order.push(OrderKey {
                    name,
                    at,
                    order: GraphValueOrder {
                        column: 0,
                        descending,
                        nulls_first,
                    },
                });
                if !self.take(b',') {
                    break;
                }
            }
        }
        if self.take_word("SKIP") {
            node.offset = self.page_number()?;
        }
        if self.take_word("LIMIT") {
            node.count = Some(self.page_number()?);
        }
        Ok(())
    }
    fn page_number(&mut self) -> Result<PageNumber, GraphSetTextError> {
        let token = self.current();
        let result = match token.kind {
            TextKind::Digits(digits) => {
                PageNumber::Literal(digits.parse::<u64>().map_err(|_| {
                    pattern_kind(token.at, GraphPatternTextErrorKind::IntegerOutOfRange)
                })?)
            }
            TextKind::Parameter(name) => {
                let name = name.to_owned();
                self.page_parameters.push((
                    token.at,
                    GqlParameterSpec {
                        name: name.clone(),
                        parameter_type: GqlParameterType::UInt64,
                        requires_positive: false,
                        occurrences: 1,
                    },
                ));
                PageNumber::Parameter(name)
            }
            _ => return Err(expected(token.at, "unsigned integer or UInt64 parameter")),
        };
        self.advance();
        Ok(result)
    }
}

/// One immutable compound text definition. MATCH operands retain their own
/// variables and scopes; positional set output names come from the left side.
/// Binding never reparses, calls the catalog, reads a database or substitutes
/// argument values into text. The resulting PreparedGraphSet uses all existing
/// live/historical/pinned/transaction set-execution entrypoints.
#[derive(Clone)]
pub struct PreparedGraphSetText {
    statement: String,
    root: Node,
    inputs: Vec<(usize, BoundSetTextInput)>,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
}
impl core::fmt::Debug for PreparedGraphSetText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphSetText")
            .field("operands", &self.inputs.len())
            .field("columns", &self.columns.len())
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphSetText {
    /// UNION/EXCEPT associate left; INTERSECT binds more tightly. Every set
    /// operator defaults to DISTINCT and may explicitly select ALL/DISTINCT.
    /// Parentheses preserve operand-local order/page. An unparenthesized final
    /// ORDER BY/SKIP/LIMIT applies to the complete set, never only its last arm.
    /// ORDER BY accepts leftmost output names, ASC/DESC and NULLS FIRST/LAST.
    /// Use parentheses around an operand with a local order or page.
    ///
    /// Leaf syntax is the shared graph-pattern profile, including WALK,
    /// OPTIONAL and EXISTS. RETURN also accepts scalar literals, parameters and
    /// checked nullable i64 arithmetic (+ - * / %, signs, ABS, NULLIF, COALESCE).
    /// Computed expressions require AS aliases. They evaluate before DISTINCT,
    /// ordering and pagination; all selected property inputs retain eager source
    /// error behavior. Constants preserve match multiplicity. Use this same
    /// entrypoint for a single MATCH with computed outputs or a compound set.
    ///
    /// WITH adds bounded row stages after MATCH: projection/renaming (including
    /// the same checked arithmetic/CASE), optional DISTINCT, ORDER BY/SKIP/LIMIT,
    /// then optional WHERE over the projected aliases. Repeated WITH stages end
    /// in RETURN. A stage's page precedes its following WHERE and next stage;
    /// only explicitly projected names survive. WHERE supports comparisons,
    /// IS [NOT] NULL and NOT/AND/OR with three-valued semantics. Project computed
    /// predicate operands first. Later row expressions never dereference graph
    /// properties, reopen MATCH, aggregate, UNWIND or write. Those forms refuse
    /// rather than silently escaping the row scope. Every leaf still executes
    /// once under the existing materialized set engine and cumulative budget.
    /// Aggregate RETURN operands remain unsupported. Byte/token admission is
    /// definition-wide; no branch resets those caps.
    pub fn prepare(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphSetTextError> {
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }

    /// Declarations are global. A declaration used only in one arm is valid;
    /// unused declarations and conflicting uses across arms or pages refuse
    /// before catalog access. Each unique (domain, name) resolves once across
    /// all arms and their internal MATCH scopes.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphSetTextError> {
        let tokens = PreparedGraphText::composition_tokens(statement, declarations)
            .map_err(|error| pattern_error(0, error))?;
        let mut parser = Composition {
            tokens,
            at: 0,
            spans: Vec::new(),
            page_parameters: Vec::new(),
        };
        let mut root = parser.expression(0)?;
        if !matches!(parser.current().kind, TextKind::End) {
            return Err(expected(parser.current().at, "end of compound statement"));
        }
        let mut pending = Vec::new();
        let mut schemas = Vec::new();
        let mut uses = parser.page_parameters;
        for span in &parser.spans {
            let names: BTreeSet<_> = parser.tokens[span.first_token..span.last_token]
                .iter()
                .filter_map(|token| match token.kind {
                    TextKind::Parameter(name) => Some(name),
                    _ => None,
                })
                .collect();
            let local: Vec<_> = declarations
                .iter()
                .copied()
                .filter(|(name, _)| names.contains(name))
                .collect();
            let input = PreparedGraphText::unresolved_for_composition(
                &statement[span.start..span.end],
                &local,
            )
            .map_err(|error| rebase_error(span.start, error))?;
            let (columns, types) = input.column_schema();
            schemas.push(Schema {
                columns,
                types,
                depth: input.depth(),
            });
            for (spec, offset) in input
                .parameter_schema()
                .iter()
                .zip(input.parameter_offsets())
            {
                uses.push((span.start + *offset, spec.clone()));
            }
            pending.push(input);
        }
        let first = root.validate(&schemas)?;
        uses.sort_by_key(|(offset, _)| *offset);
        let mut parameters: Vec<GqlParameterSpec> = Vec::new();
        let mut parameter_offsets = Vec::new();
        let mut parameter_index = BTreeMap::new();
        for (offset, spec) in uses {
            if let Some(&at) = parameter_index.get(&spec.name) {
                let previous: &mut GqlParameterSpec = &mut parameters[at];
                if previous.parameter_type != spec.parameter_type {
                    return Err(pattern_kind(
                        offset,
                        GraphPatternTextErrorKind::ConflictingParameterTypes,
                    ));
                }
                previous.occurrences += spec.occurrences;
                previous.requires_positive |= spec.requires_positive;
            } else {
                parameter_index.insert(spec.name.clone(), parameters.len());
                parameter_offsets.push(offset);
                parameters.push(spec);
            }
        }
        for &(name, kind) in declarations {
            let Some(&at) = parameter_index.get(name) else {
                return Err(pattern_kind(
                    statement.len(),
                    GraphPatternTextErrorKind::UnusedParameterDeclaration,
                ));
            };
            if parameters[at].parameter_type != kind {
                return Err(pattern_kind(
                    parameter_offsets[at],
                    GraphPatternTextErrorKind::ConflictingParameterTypes,
                ));
            }
        }
        // No catalog calls occurred above. Keep one domain-aware cache around
        // the existing per-pattern resolver; errors retain original offsets.
        let mut cache = BTreeMap::new();
        let mut symbols = |kind, name: &str| {
            let key = (kind, name.to_owned());
            if let Some(value) = cache.get(&key) {
                return Some(*value);
            }
            let value = resolve(kind, name)?;
            cache.insert(key, value);
            Some(value)
        };
        let mut inputs = Vec::new();
        for (input, span) in pending.into_iter().zip(&parser.spans) {
            let input = input
                .resolve(&mut symbols)
                .map_err(|error| pattern_error(span.start, error))?;
            inputs.push((span.start, input));
        }
        Ok(Self {
            statement: statement.to_owned(),
            root,
            inputs,
            columns: schemas[first].columns.clone(),
            types: schemas[first].types.clone(),
            parameters,
            parameter_offsets,
        })
    }
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        // Reject the complete argument map before constructing any leaf plan.
        for (spec, &offset) in self.parameters.iter().zip(&self.parameter_offsets) {
            let value = arguments
                .get(&spec.name)
                .ok_or_else(|| pattern_kind(offset, GraphPatternTextErrorKind::MissingParameter))?;
            if !spec.parameter_type.accepts(value.parameter_type()) {
                return Err(pattern_kind(
                    offset,
                    GraphPatternTextErrorKind::ParameterTypeMismatch {
                        expected: spec.parameter_type,
                        found: value.parameter_type(),
                    },
                ));
            }
        }
        if arguments.len() != self.parameters.len() {
            return Err(pattern_kind(
                self.statement.len(),
                GraphPatternTextErrorKind::UnexpectedArguments,
            ));
        }
        let mut inputs = Vec::new();
        for (offset, input) in &self.inputs {
            let mut local = GqlParameters::new();
            for spec in input.parameter_schema() {
                local
                    .insert(
                        spec.name.clone(),
                        arguments.get(&spec.name).expect("validated argument"),
                    )
                    .expect("prepared local names are valid and unique");
            }
            let bound = input
                .bind_parameters(&local)
                .map_err(|error| rebase_error(*offset, error))?;
            inputs.push(Some(bound));
        }
        self.root.bind(&mut inputs, arguments)
    }
}
