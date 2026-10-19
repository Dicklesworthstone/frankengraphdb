//! Read projection uses the mutation parser's existing scalar operands and
//! precedence compiler. Only terminal semantics differ: no action or write is
//! constructed. Hidden source columns retain every requested property read.

mod aggregate;
pub(super) mod pipeline;
pub(super) mod with_aggregate;

use super::mutation_query::{ReturnLeaves, ReturnMapInput};
use super::*;
use crate::graph_text::parameters::UnresolvedGraphText;
use crate::set_text::{
    BoundSetTextInput, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use crate::{
    GraphSetColumnType, GraphSetProjection, GraphSetTextError, GraphSetTextErrorKind,
    GraphSetValue, PreparedGraphSet,
};

/// Expression leaves and write leaves share one value grammar. Only the
/// owner of a write can bind a parsed map selector to post-statement data.
enum ValueResolver<'r, 'a> {
    /// Leaves of an operand whose enclosing grammar owns the comparisons and
    /// Boolean connectors around it (a WITH WHERE operand, an ORDER BY key):
    /// the value stops before them.
    Columns(&'r mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>),
    /// Leaves of a whole projection item, whose comparisons, IS [NOT] NULL
    /// and Boolean connectors belong to the value (`RETURN count(*) > 0 AS
    /// any`).
    Items(&'r mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>),
    /// A write RETURN item or aggregate argument, a whole value too.
    Write(&'r mut dyn ReturnLeaves<'a>),
}
impl<'a> ValueResolver<'_, 'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        match self {
            Self::Columns(resolve) | Self::Items(resolve) => resolve(parser),
            Self::Write(leaves) => leaves.leaf(parser),
        }
    }

    /// Whether the value is a whole expression, predicates included.
    fn whole(&self) -> bool {
        !matches!(self, Self::Columns(_))
    }

    fn map_input(
        &mut self,
        parser: &mut Parser<'a>,
        source: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        match self {
            Self::Write(leaves) => leaves.map_input(parser, source, input),
            Self::Columns(_) | Self::Items(_) => Err(error(
                source.at,
                GraphPatternTextErrorKind::Expected("a map source in the current projection scope"),
            )),
        }
    }
}

fn expression_error(source: GraphMutationTextError) -> GraphSetTextError {
    let kind = match source.kind {
        GraphMutationTextErrorKind::Query(kind) => GraphSetTextErrorKind::Pattern(kind),
        GraphMutationTextErrorKind::IntegerExpression(kind) => {
            GraphSetTextErrorKind::IntegerExpression(kind)
        }
        GraphMutationTextErrorKind::IntegerOperand => GraphSetTextErrorKind::IntegerOperand,
        GraphMutationTextErrorKind::IntegerNesting { limit } => {
            GraphSetTextErrorKind::IntegerNesting { limit }
        }
        GraphMutationTextErrorKind::Relation(kind) => kind,
        GraphMutationTextErrorKind::Build(_) | GraphMutationTextErrorKind::ReturnBuild(_) => {
            GraphSetTextErrorKind::Expected("read scalar expression")
        }
    };
    GraphSetTextError {
        offset: source.offset,
        kind,
    }
}

/// A scalar body over its composite operands (fgdb-o5o9t). A body that only
/// reads its one slot is that composite, so `(any(...))` stays a quantifier.
fn let_value(mut slots: Vec<ReadValueTemplate>, body: ReadValueTemplate) -> ReadValueTemplate {
    let lone_slot = match &body {
        ReadValueTemplate::Integer { program, .. } => matches!(
            program.as_slice(),
            [MutationIntegerTemplateOp::Bound(
                crate::GraphIntegerOp::Local(0)
            )]
        ),
        _ => false,
    };
    match slots.len() {
        0 => body,
        1 if lone_slot => slots.pop().expect("one slot"),
        _ => ReadValueTemplate::Let {
            values: slots,
            body: Box::new(body),
        },
    }
}

/// Advance past the bracketed group whose opening token was just read. An
/// unbalanced group stops at the end; the parser itself reports it.
pub(super) fn skip_group(lexer: &mut Lexer<'_>) -> Result<(), GraphPatternTextError> {
    let mut depth = 1_usize;
    loop {
        match lexer.next()?.kind {
            TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            TokenKind::End => return Ok(()),
            _ => {}
        }
    }
}

/// The first token after a value's postfix reads (`[...]` subscripts and
/// `.key` entries), with `lexer` just past the value itself.
fn after_postfix<'t>(lexer: &mut Lexer<'t>) -> Result<Token<'t>, GraphPatternTextError> {
    let mut next = lexer.next()?;
    loop {
        match next.kind {
            TokenKind::Punct(b'[') => {
                skip_group(lexer)?;
                next = lexer.next()?;
            }
            TokenKind::Punct(b'.')
                if matches!(
                    lexer.clone().next()?.kind,
                    TokenKind::Word(_) | TokenKind::DelimitedKeyword(_)
                ) =>
            {
                lexer.next()?;
                next = lexer.next()?;
            }
            _ => return Ok(next),
        }
    }
}

/// The first token after the factor `token` starts, with `lexer` just past
/// `token`: a group, a CASE ... END, a call, a name or literal, each with
/// its postfix reads. Any other token is returned unread.
fn skip_factor<'t>(
    token: Token<'t>,
    lexer: &mut Lexer<'t>,
) -> Result<Token<'t>, GraphPatternTextError> {
    match token.kind {
        TokenKind::Punct(b'(' | b'[' | b'{') => skip_group(lexer)?,
        TokenKind::Word(word) if word.eq_ignore_ascii_case("CASE") => {
            let mut depth = 1_usize;
            while depth > 0 {
                match lexer.next()?.kind {
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("CASE") => depth += 1,
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("END") => depth -= 1,
                    TokenKind::End => break,
                    _ => {}
                }
            }
        }
        TokenKind::Word(_) => {
            if matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
                lexer.next()?;
                skip_group(lexer)?;
            }
        }
        TokenKind::Digits(_) => {
            // A float literal spans `digits . digits`.
            let mut ahead = lexer.clone();
            if matches!(ahead.next()?.kind, TokenKind::Punct(b'.'))
                && matches!(ahead.next()?.kind, TokenKind::Digits(_))
            {
                *lexer = ahead;
            }
        }
        TokenKind::Quoted(_) | TokenKind::Parameter(_) | TokenKind::DelimitedKeyword(_) => {}
        _ => return Ok(token),
    }
    after_postfix(lexer)
}

/// Whether `token` (with `lexer` just past it) starts a list operand of a
/// `+` chain (fgdb-c4i0d): a list literal or comprehension, a call of a
/// list-valued function (reverse, split, tail, range, keys), or a
/// list-typed row column read whole.
fn list_operand_at(
    token: Token<'_>,
    lexer: &Lexer<'_>,
    row: Option<&[(Name<'_>, GraphSetColumnType)]>,
) -> Result<bool, GraphPatternTextError> {
    match token.kind {
        TokenKind::Punct(b'[') => Ok(true),
        TokenKind::Word(word) if word.eq_ignore_ascii_case("CASE") => case_has_list(lexer, row),
        TokenKind::Word(word) => Ok(match lexer.clone().next()?.kind {
            TokenKind::Punct(b'(') => ["REVERSE", "SPLIT", "TAIL", "RANGE", "KEYS"]
                .iter()
                .any(|name| word.eq_ignore_ascii_case(name)),
            TokenKind::Punct(b'.' | b'[') => false,
            _ => row.is_some_and(|schema| {
                schema
                    .iter()
                    .any(|(name, kind)| name.text == word && *kind == GraphSetColumnType::List)
            }),
        }),
        _ => Ok(false),
    }
}

/// Whether the CASE that `lexer` is just past the `CASE` of has a THEN or
/// ELSE result that starts with a list operand (list_operand_at), at its own
/// level: such a CASE is read as a value-language Case (fgdb-13l85).
fn case_has_list(
    lexer: &Lexer<'_>,
    row: Option<&[(Name<'_>, GraphSetColumnType)]>,
) -> Result<bool, GraphPatternTextError> {
    let mut lexer = lexer.clone();
    let mut groups = 0_usize;
    let mut cases = 1_usize;
    loop {
        match lexer.next()?.kind {
            TokenKind::End => return Ok(false),
            TokenKind::Punct(b'(' | b'[' | b'{') => groups += 1,
            TokenKind::Punct(b')' | b']' | b'}') => groups = groups.saturating_sub(1),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("CASE") => cases += 1,
            TokenKind::Word(word) if word.eq_ignore_ascii_case("END") => {
                cases -= 1;
                if cases == 0 {
                    return Ok(false);
                }
            }
            TokenKind::Word(word)
                if groups == 0
                    && cases == 1
                    && (word.eq_ignore_ascii_case("THEN") || word.eq_ignore_ascii_case("ELSE")) =>
            {
                let mut ahead = lexer.clone();
                let result = ahead.next()?;
                if list_operand_at(result, &ahead, row)? || dynamic_column_at(result, &ahead, row)?
                {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
}

/// Whether `token` (with `lexer` just past it) starts a read of a dynamically
/// typed row column, bare or through its postfix reads: `input.list` over an
/// UNWIND of maps may be a list or a map as well as a scalar. A CASE that
/// returns one is read in the value language, typed per row, since the scalar
/// program refuses a non-scalar result when it runs (TCK Quantifier9-12's
/// `CASE WHEN input.fixed THEN input.list ELSE null END`).
fn dynamic_column_at(
    token: Token<'_>,
    lexer: &Lexer<'_>,
    row: Option<&[(Name<'_>, GraphSetColumnType)]>,
) -> Result<bool, GraphPatternTextError> {
    let TokenKind::Word(word) = token.kind else {
        return Ok(false);
    };
    if matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
        return Ok(false);
    }
    Ok(row.is_some_and(|schema| {
        schema
            .iter()
            .any(|(name, kind)| name.text == word && *kind == GraphSetColumnType::Any)
    }))
}

/// The column domain of one graph input: a property is a scalar, a bare
/// variable its element, a path function its own result.
fn projection_type(input: &Projection<'_>) -> GraphSetColumnType {
    if input.property.is_some() {
        return GraphSetColumnType::Scalar;
    }
    match input.path {
        None => GraphSetColumnType::Vertex,
        Some(GraphPathFunction::Value) => GraphSetColumnType::Path,
        Some(GraphPathFunction::Length | GraphPathFunction::Type) => GraphSetColumnType::Scalar,
        Some(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
        Some(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
        Some(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
        Some(GraphPathFunction::Labels) => GraphSetColumnType::List,
        Some(GraphPathFunction::Properties) => GraphSetColumnType::Any,
    }
}

/// Whether a value can statically hold a map, so `.key` may read it
/// (fgdb-2jw3z). A row column qualifies only when its type is dynamic
/// (`schema` is the row scope). A graph property-map projection is identified
/// separately by the parser, without admitting an arbitrary graph identity.
fn map_capable(
    value: &ReadValueTemplate,
    key: &str,
    schema: Option<&[(Name<'_>, GraphSetColumnType)]>,
    parameters: &[GqlParameterSpec],
) -> bool {
    match value {
        // A temporal value's `.year` reads as a map entry does.
        ReadValueTemplate::MapLiteral { .. }
        | ReadValueTemplate::MapOverlay { .. }
        | ReadValueTemplate::MapGet { .. }
        | ReadValueTemplate::Local(_)
        | ReadValueTemplate::Index { .. }
        | ReadValueTemplate::Reduce { .. }
        | ReadValueTemplate::Temporal { .. } => true,
        // A scalar column (a stored property, say) may hold a temporal value,
        // so it reads that value's fields. Any other key could only be NULL
        // or an error, and stays refused here.
        ReadValueTemplate::Column(at) => {
            schema
                .and_then(|schema| schema.get(*at))
                .is_some_and(|(_, kind)| match kind {
                    GraphSetColumnType::Any => true,
                    GraphSetColumnType::Scalar => crate::set_ops::temporal_field_name(key),
                    _ => false,
                })
        }
        ReadValueTemplate::Parameter { index, .. } => {
            parameters[*index].parameter_type == GqlParameterType::Map
        }
        _ => false,
    }
}

/// The openCypher list functions parsed as values (fgdb-20foe).
#[derive(Clone, Copy)]
enum ListFunction {
    Head,
    Last,
    Tail,
    Range,
    Reduce,
}

struct GraphProjectionHead<'a> {
    with: bool,
    /// A written WITH that neither groups nor deduplicates. openCypher lets
    /// its own WHERE and pages also read the MATCH bindings it drops.
    plain: bool,
    inputs: Vec<Projection<'a>>,
    outputs: Vec<(Name<'a>, ReadValueTemplate)>,
}
impl<'a> GraphProjectionHead<'a> {
    fn schema(&self, parameters: &[GqlParameterSpec]) -> pipeline::RowSchema<'a> {
        // Match the native source-column domains. A property on an edge is
        // still a scalar; the Edge marker selects its owner, not its value type.
        let types: Vec<_> = self
            .inputs
            .iter()
            .map(|input| {
                if input.property.is_some() {
                    return GraphSetColumnType::Scalar;
                }
                match input.path {
                    Some(GraphPathFunction::Value) => GraphSetColumnType::Path,
                    Some(GraphPathFunction::Length | GraphPathFunction::Type) => {
                        GraphSetColumnType::Scalar
                    }
                    Some(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
                    Some(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
                    Some(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
                    Some(GraphPathFunction::Labels) => GraphSetColumnType::List,
                    Some(GraphPathFunction::Properties) => GraphSetColumnType::Any,
                    None => GraphSetColumnType::Vertex,
                }
            })
            .collect();
        self.outputs
            .iter()
            .map(|(name, operand)| (*name, operand.column_type(&types, parameters)))
            .collect()
    }
}

impl<'a> Parser<'a> {
    /// A WITH item may keep an existing variable without AS. Source reads and
    /// functions also lower to Column, so the lowered value alone cannot prove
    /// that the item names a variable. Retain the token boundary of a possible
    /// variable, allowing grouping parentheses and trivia; the caller checks
    /// that normal expression parsing ends at exactly that boundary.
    fn implicit_with_binding(&self) -> Result<Option<(Name<'a>, usize)>, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut parentheses = 0_usize;
        while matches!(token.kind, TokenKind::Punct(b'(')) {
            // Match the read/integer expression nesting bound. The real parser
            // owns the typed refusal; this lookahead need not scan its suffix.
            if parentheses == 64 {
                return Ok(None);
            }
            parentheses += 1;
            token = lexer.next()?;
        }
        let TokenKind::Word(text) = token.kind else {
            return Ok(None);
        };
        let name = Name { text, at: token.at };
        token = lexer.next()?;
        while parentheses != 0 {
            if !matches!(token.kind, TokenKind::Punct(b')')) {
                return Ok(None);
            }
            parentheses -= 1;
            token = lexer.next()?;
        }
        Ok(Some((name, token.at)))
    }

    /// How many parentheses wrap a lone name here, as in `((n))`, when no
    /// operator, subscript or entry read follows the last one; `None` for
    /// anything else, which keeps its scalar reading (`(x) + 1`, `(n.p)`).
    fn parenthesized_name(&self) -> Result<Option<usize>, GraphPatternTextError> {
        if !self.is_punct(b'(') {
            return Ok(None);
        }
        let mut lexer = self.lexer.clone();
        let mut parentheses = 1_usize;
        let mut token = lexer.next()?;
        while matches!(token.kind, TokenKind::Punct(b'(')) {
            if parentheses == 64 {
                return Ok(None);
            }
            parentheses += 1;
            token = lexer.next()?;
        }
        if !matches!(token.kind, TokenKind::Word(_)) {
            return Ok(None);
        }
        for _ in 0..parentheses {
            if !matches!(lexer.next()?.kind, TokenKind::Punct(b')')) {
                return Ok(None);
            }
        }
        let after = lexer.next()?;
        let continues = match after.kind {
            TokenKind::Punct(byte) => b"=<>!+-*/%^.[".contains(&byte),
            TokenKind::Word(word) => ["AND", "OR", "XOR", "IS", "IN", "STARTS", "ENDS", "CONTAINS"]
                .iter()
                .any(|operator| word.eq_ignore_ascii_case(operator)),
            _ => false,
        };
        Ok((!continues).then_some(parentheses))
    }

    fn property_map_projection(
        &self,
        columns: &mut Vec<Projection<'a>>,
        source: Name<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        self.require_property_variable(source)?;
        self.projection_slot(
            columns,
            Projection {
                variable: source,
                property: None,
                path: Some(GraphPathFunction::Properties),
            },
        )
    }

    /// A function call resolved by a graph/row boundary callback. KEYS uses
    /// the same map source; its list conversion stays in the row evaluator.
    fn property_map_function(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<usize, GraphPatternTextError> {
        self.advance()?;
        self.punct(b'(', "(")?;
        let source = self.property_variable()?;
        self.punct(b')', ")")?;
        self.property_map_projection(columns, source)
    }

    fn boundary_property_map(
        &self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        alias: &str,
    ) -> Option<usize> {
        let boundary = self.boundary_reads.as_ref()?;
        if boundary.width != schema.len() {
            return None;
        }
        boundary
            .reads
            .iter()
            .find(|(name, key, _)| *name == alias && key.text == "*")
            .map(|(_, _, column)| *column)
    }

    /// Parse a MATCH leaf exactly once. Composition owns the enclosing set
    /// delimiters and pagination; this parser owns names, expressions and the
    /// original shared parameter table. No statement substring is rewritten.
    pub(in crate::graph_text) fn parse_return_for_composition(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        if self.is_word("UNWIND")
            || self.is_word("CALL")
            || self.is_word("RETURN")
            || self.is_word("WITH")
        {
            return self.parse_leading_pipeline(statement);
        }
        self.defer_where = true;
        self.parse_match_prefix()?;
        self.defer_where = false;
        let mut head = if let Some((lexer, at_where)) = self.deferred_where.take() {
            // MATCH ... WHERE <over list values> is MATCH ... WITH * WHERE ...
            // (fgdb-1c29d): the row pipeline reads the predicate after an
            // implicit boundary that projects every visible binding.
            if !self.syntax.scopes.is_empty() || !(self.is_word("RETURN") || self.is_word("WITH")) {
                return Err(GraphSetTextError {
                    offset: self.current.at,
                    kind: GraphSetTextErrorKind::Expected(
                        "RETURN or WITH right after a MATCH WHERE over list values",
                    ),
                });
            }
            let head = self.implicit_with_head()?;
            self.lexer = lexer;
            self.current = at_where;
            head
        } else {
            self.graph_projection_head()?
        };
        self.hoist_boundary_reads(&mut head, 0)?;
        let pipeline = if head.with {
            self.row_pipeline(head.schema(&self.syntax.parameters))?
        } else {
            self.graph_expression_page(&mut head, &[], false)?
                .into_iter()
                .collect()
        };
        self.end()?;
        self.finish_graph_projection(statement, head, pipeline)
    }

    fn parse_leading_pipeline(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        let mut schema = Vec::new();
        let mut leading = Vec::new();
        // A procedure is a source: it can only start the pipeline.
        if self.is_word("CALL") {
            let at = self.current.at;
            self.advance()?;
            leading.push(self.call_stage(&mut schema, at)?);
        }
        while self.is_word("UNWIND") {
            let at = self.current.at;
            self.advance()?;
            leading.push(self.unwind_stage(&mut schema, at)?);
        }
        let implied = if self.is_word("MATCH") {
            Vec::new()
        } else {
            self.implied_call_vertices(&schema, &leading)?
        };
        if self.is_word("MATCH") || !implied.is_empty() {
            let (part, _) = self.leading_graph_part(statement, schema, leading, implied, false)?;
            return Ok(part);
        }
        // A standalone `CALL ... YIELD a, b` is a complete read that returns
        // exactly its yielded columns, as `... YIELD a, b RETURN a, b` does
        // (openCypher's standalone CALL).
        let pipeline = if matches!(leading.as_slice(), [ReadStageTemplate::Call { .. }])
            && matches!(self.current.kind, TokenKind::End)
        {
            vec![ReadStageTemplate::Project {
                at: self.current.at,
                projection: (schema.iter().enumerate())
                    .map(|(index, (name, _))| ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value: ReadValueTemplate::Column(index),
                    })
                    .collect(),
                quantifier: crate::GraphSetQuantifier::All,
            }]
        } else {
            self.row_pipeline(schema)?
        };
        self.end()?;
        Ok(UnresolvedGraphText {
            statement,
            syntax: self.syntax,
            projection: None,
            pipeline: leading.into_iter().chain(pipeline).collect(),
            singleton: true,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
        })
    }

    /// The MATCH part after a leading pipeline (`UNWIND ... MATCH ...` or a
    /// read CALL's implied MATCH), joined to the leading rows by its
    /// correlations. A read parses through its RETURN. With `aggregate` the
    /// part stops before the terminal aggregate RETURN and also returns the
    /// row schema there (the first WITH's, or the implicit leading columns
    /// and MATCH vertices): the aggregate owner compiles RETURN over it with
    /// the same parser.
    #[allow(clippy::type_complexity)]
    fn leading_graph_part(
        &mut self,
        statement: &'a str,
        mut schema: Vec<(Name<'a>, GraphSetColumnType)>,
        mut leading: Vec<ReadStageTemplate>,
        implied: Vec<Name<'a>>,
        aggregate: bool,
    ) -> Result<(UnresolvedGraphText<'a>, Option<pipeline::RowSchema<'a>>), GraphSetTextError> {
        self.read_row_bindings = schema.iter().map(|(name, _)| *name).collect();
        if implied.is_empty() {
            self.parse_match_prefix()?;
        } else {
            // The implied `MATCH (n)` for each read CALL output: exactly
            // the root a written one parses, and nothing else.
            for name in implied {
                self.capacity(
                    self.syntax.variables.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Vertices,
                )?;
                self.syntax.variables.push(name);
            }
            self.syntax.root_variables = self.syntax.variables.len();
            self.syntax.return_at = self.current.at;
        }
        let correlations = core::mem::take(&mut self.read_correlations);
        let width = schema.len();
        let rebound = self
            .syntax
            .path
            .into_iter()
            .chain(self.syntax.scope_paths.iter().copied())
            .chain(
                self.syntax
                    .visible_edges()
                    .filter_map(|(edge, _)| edge.variable),
            )
            .find(|name| {
                self.read_row_bindings
                    .iter()
                    .any(|row| row.text == name.text)
            });
        if let Some(name) = rebound {
            return Err(GraphSetTextError {
                offset: name.at,
                kind: GraphSetTextErrorKind::Expected(
                    "a new name for a path or edge after a leading stage",
                ),
            });
        }
        let mut inputs = Vec::new();
        let mut bound_correlations = Vec::new();
        // The graph vertices a WITH-less projection exposes after the
        // leading columns, as (name, combined-row column).
        let mut exposed = Vec::new();
        // (leading column, graph vertex input) pairs equal by the join.
        let mut identity = Vec::new();
        for &variable in &self.syntax.variables {
            let index = self.mutation_projection(&mut inputs, variable, None)?;
            // A MATCH vertex reusing a leading column's name IS that
            // value: an identity correlation, never a second binding that
            // shadows the first and silently crosses every row.
            match (self.read_row_bindings.iter()).position(|row| row.text == variable.text) {
                Some(row) => {
                    bound_correlations.push((row, index));
                    identity.push((row, index));
                }
                None => {
                    schema.push((variable, GraphSetColumnType::Vertex));
                    exposed.push((variable, width + index));
                }
            }
        }
        for (variable, key, row) in correlations {
            let index = self.mutation_projection(&mut inputs, variable, key)?;
            bound_correlations.push((row, index));
        }
        // A CALL output matched as a vertex must be one (fgdb-luq0b):
        // record it, so the host refuses a scalar instead of letting the
        // identity join drop every row.
        if let Some(ReadStageTemplate::Call {
            outputs, vertices, ..
        }) = leading.first_mut()
        {
            let mut rows: Vec<usize> = (identity.iter())
                .map(|&(row, _)| row)
                .filter(|&row| row < outputs.len())
                .collect();
            rows.sort_unstable();
            rows.dedup();
            *vertices = rows;
        }
        // The first WITH is the graph-to-row boundary, exactly as it is
        // for a statement that starts at MATCH: `n.p` there reads a graph
        // property. Later stages see only the projected row.
        let mut tail = None;
        let (projection, pipeline) = if self.is_word("WITH") {
            let (outputs, distinct, at) = self.leading_with_head(&schema, width, &mut inputs)?;
            let mut types: Vec<_> = schema[..width].iter().map(|(_, kind)| *kind).collect();
            types.extend(inputs.iter().map(projection_type));
            let next: pipeline::RowSchema<'a> = outputs
                .iter()
                .map(|(name, value)| (*name, value.column_type(&types, &self.syntax.parameters)))
                .collect();
            let mut pipeline = Vec::new();
            if distinct {
                // Deduplicate the projected rows, not the graph child's.
                pipeline.push(ReadStageTemplate::Project {
                    at,
                    projection: next
                        .iter()
                        .enumerate()
                        .map(|(index, (name, _))| ReadProjectionTemplate {
                            name: name.text.to_owned(),
                            value: ReadValueTemplate::Column(index),
                        })
                        .collect(),
                    quantifier: crate::GraphSetQuantifier::Distinct,
                });
            }
            if aggregate {
                let (stages, schema, _) = self.row_pipeline_prefix(next)?;
                pipeline.extend(stages);
                tail = Some(schema);
            } else {
                pipeline.extend(self.row_pipeline(next)?);
            }
            let projection = outputs
                .into_iter()
                .map(|(name, value)| ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value,
                })
                .collect();
            (projection, pipeline)
        } else {
            // Without a WITH the row is the leading columns, then the new
            // MATCH vertices. A leading column a MATCH vertex correlates
            // with is carried as that vertex's graph column (equal by the
            // join, and typed Vertex), so a later `n.p` reads its
            // properties through hidden boundary reads.
            let outputs = (self.read_row_bindings.iter().copied().enumerate())
                .map(|(row, name)| {
                    let column = identity
                        .iter()
                        .find(|&&(at, _)| at == row)
                        .map_or(row, |&(_, input)| width + input);
                    (name, ReadValueTemplate::Column(column))
                })
                .chain(
                    (exposed.into_iter())
                        .map(|(name, column)| (name, ReadValueTemplate::Column(column))),
                )
                .collect();
            let mut head = GraphProjectionHead {
                with: true,
                plain: false,
                inputs: core::mem::take(&mut inputs),
                outputs,
            };
            self.hoist_boundary_reads(&mut head, width)?;
            inputs = head.inputs;
            let mut types: Vec<_> = schema[..width].iter().map(|(_, kind)| *kind).collect();
            types.extend(inputs.iter().map(projection_type));
            let next: pipeline::RowSchema<'a> = (head.outputs.iter())
                .map(|(name, value)| (*name, value.column_type(&types, &self.syntax.parameters)))
                .collect();
            let projection = (head.outputs.into_iter())
                .map(|(name, value)| ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value,
                })
                .collect();
            // An aggregate RETURN reads this implicit row: the leading
            // columns, the new MATCH vertices and their hidden property reads.
            let pipeline = if aggregate {
                let (stages, schema, _) = self.row_pipeline_prefix(next)?;
                tail = Some(schema);
                stages
            } else {
                self.row_pipeline(next)?
            };
            (projection, pipeline)
        };
        if !aggregate {
            self.end()?;
        }
        self.syntax.columns = inputs
            .into_iter()
            .map(|source| Column {
                variable: source.variable,
                property: source.property,
                path: source.path,
                alias: source.variable,
            })
            .collect();
        let projection = Some(projection);
        Ok((
            UnresolvedGraphText {
                statement,
                syntax: self.take_part_syntax()?,
                projection,
                pipeline,
                singleton: false,
                leading,
                leading_types: vec![GraphSetColumnType::Any; width],
                correlations: bound_correlations,
            },
            tail,
        ))
    }

    /// `UNWIND ... MATCH ...` (with or without a WITH) before a pipeline
    /// aggregate RETURN:
    /// the leading pipeline's graph part, parsed exactly as a read parses it
    /// (one CrossJoin of the unwound rows and the pattern, filtered by the
    /// correlations), stopped before RETURN. `UNWIND $ids AS id MATCH (n {id:
    /// id}) WITH n.p AS p RETURN count(*)`.
    #[allow(clippy::type_complexity)]
    pub(in crate::graph_text) fn leading_aggregate_prefix(
        &mut self,
        statement: &'a str,
    ) -> Result<(UnresolvedGraphText<'a>, pipeline::RowSchema<'a>, usize), GraphSetTextError> {
        let mut schema = Vec::new();
        let mut leading = Vec::new();
        while self.is_word("UNWIND") {
            let at = self.current.at;
            self.advance()?;
            leading.push(self.unwind_stage(&mut schema, at)?);
        }
        let at = self.current.at;
        let (part, tail) = self.leading_graph_part(statement, schema, leading, Vec::new(), true)?;
        let schema = tail.ok_or(GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::Expected(
                "a row schema before a pipeline aggregate RETURN",
            ),
        })?;
        let depth = part.depth();
        Ok((part, schema, depth))
    }

    /// Same public binding order as native RETURN *: named vertices, a
    /// captured path, then named edges. Anonymous compiler slots stay private.
    fn visible_graph_bindings(&self) -> impl Iterator<Item = Name<'a>> + '_ {
        self.syntax
            .variables
            .iter()
            .copied()
            .filter(|name| !name.text.starts_with(Self::ANONYMOUS_PREFIX))
            .chain(self.syntax.path)
            .chain(self.syntax.scope_paths.iter().copied())
            .chain(
                self.syntax
                    .visible_edges()
                    .filter_map(|(edge, _)| edge.variable),
            )
    }

    /// The first WITH after `UNWIND ... MATCH`: each item may read a leading
    /// UNWIND column by name, or a MATCH variable or `variable.property`,
    /// which becomes a hidden graph input column (the same
    /// `mutation_projection` the correlations and the MATCH-first head use).
    /// Column indices address the combined row: leading columns, then graph
    /// inputs. Returns the named items, whether DISTINCT was requested, and
    /// the WITH offset.
    #[allow(clippy::type_complexity)]
    fn leading_with_head(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        width: usize,
        inputs: &mut Vec<Projection<'a>>,
    ) -> Result<(Vec<(Name<'a>, ReadValueTemplate)>, bool, usize), GraphSetTextError> {
        if self.with_has_aggregate()? {
            let at = self.current.at;
            let head = self.with_graph_head(&schema[..width], core::mem::take(inputs), false)?;
            *inputs = head.inputs;
            return Ok((head.outputs, false, at));
        }
        let at = self.current.at;
        self.word("WITH")?;
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_all_quantifier()?;
        }
        let leading: Vec<Name<'a>> = schema[..width].iter().map(|(name, _)| *name).collect();
        let mut outputs = Vec::<(Name<'a>, ReadValueTemplate)>::new();
        if self.take(b'*')? {
            // Leading columns, then the same visible graph bindings as a
            // MATCH-first `WITH *`; anonymous compiler slots stay private.
            for (index, name) in leading.iter().enumerate() {
                // A correlated graph identity retains its source domain even
                // through WITH *. Other procedure/UNWIND values stay ordinary
                // row columns and never acquire graph lookup authority.
                let column = match self
                    .visible_graph_bindings()
                    .find(|binding| binding.text == name.text)
                {
                    Some(variable) => width + self.mutation_projection(inputs, variable, None)?,
                    None => index,
                };
                outputs.push((*name, ReadValueTemplate::Column(column)));
            }
            let visible: Vec<_> = self.visible_graph_bindings().collect();
            for variable in visible {
                // A vertex reusing a leading name is that leading column.
                if leading.iter().any(|name| name.text == variable.text) {
                    continue;
                }
                let index = self.mutation_projection(inputs, variable, None)?;
                outputs.push((variable, ReadValueTemplate::Column(width + index)));
            }
            let mut head = GraphProjectionHead {
                with: true,
                plain: false,
                inputs: core::mem::take(inputs),
                outputs,
            };
            self.hoist_boundary_reads(&mut head, width)?;
            *inputs = head.inputs;
            return Ok((head.outputs, distinct, at));
        }
        loop {
            self.capacity(
                outputs.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let item_at = self.current.at;
            let implicit = self.implicit_with_binding()?;
            let value = self.read_resolved_item(
                &mut |parser| {
                    let TokenKind::Word(word) = parser.current.kind else {
                        return Ok(None);
                    };
                    if (word.eq_ignore_ascii_case("properties")
                        || word.eq_ignore_ascii_case("keys"))
                        && let Some(argument) =
                            pipeline::property_map_source(word, parser.current.at, &parser.lexer)?
                        && parser
                            .visible_graph_bindings()
                            .any(|name| name.text == argument.text)
                    {
                        return parser
                            .property_map_function(inputs)
                            .map(|column| Some(width + column));
                    }
                    if pipeline::metadata_source(word, parser.current.at, &parser.lexer)?.is_some()
                    {
                        let Operand::Column(column) = parser.mutation_operand(inputs)? else {
                            unreachable!("native metadata projects one source column");
                        };
                        return Ok(Some(width + column));
                    }
                    if matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
                        return Ok(None);
                    }
                    let graph = parser.syntax.variables.iter().any(|v| v.text == word)
                        || parser.syntax.names_path(word)
                        || parser.syntax.visible_edge(word).is_some();
                    if graph {
                        let variable = parser.any_variable()?;
                        let property = if parser.take(b'.')? {
                            Some(parser.name()?)
                        } else {
                            None
                        };
                        let index = parser.mutation_projection(inputs, variable, property)?;
                        return Ok(Some(width + index));
                    }
                    if let Some(index) = leading.iter().position(|name| name.text == word) {
                        parser.advance()?;
                        return Ok(Some(index));
                    }
                    Ok(None)
                },
                0,
            )?;
            let end = self.current.at;
            let alias = if self.take_word("AS")? {
                self.name()?
            } else {
                implicit
                    .filter(|(_, boundary)| {
                        *boundary == end && matches!(value, ReadValueTemplate::Column(_))
                    })
                    .map(|(name, _)| name)
                    .ok_or(GraphSetTextError {
                        offset: item_at,
                        kind: GraphSetTextErrorKind::Expected("AS alias for a computed WITH value"),
                    })?
            };
            if outputs.iter().any(|(name, _)| name.text == alias.text) {
                return Err(error(
                    alias.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                )
                .into());
            }
            outputs.push((alias, value));
            if !self.take(b',')? {
                break;
            }
        }
        let mut head = GraphProjectionHead {
            with: true,
            plain: false,
            inputs: core::mem::take(inputs),
            outputs,
        };
        self.hoist_boundary_reads(&mut head, width)?;
        *inputs = head.inputs;
        Ok((head.outputs, distinct, at))
    }

    /// The implicit boundary after a MATCH that a WITH would otherwise write:
    /// every visible binding, unchanged (`MATCH ... UNWIND`, and a MATCH WHERE
    /// over list values, fgdb-1c29d).
    fn implicit_with_head(&mut self) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        for variable in self.visible_graph_bindings() {
            let index = self.mutation_projection(&mut inputs, variable, None)?;
            outputs.push((variable, ReadValueTemplate::Column(index)));
        }
        Ok(GraphProjectionHead {
            with: true,
            plain: false,
            inputs,
            outputs,
        })
    }

    /// Shared graph-to-row boundary. Exact grouped RETURN uses this same first
    /// WITH projection and row-stage parser, not a synthetic RETURN statement.
    fn graph_projection_head(&mut self) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        if self.with_has_aggregate()? {
            return self.with_graph_head(&[], Vec::new(), false);
        }
        if self.is_word("UNWIND") {
            return self.implicit_with_head();
        }
        let with = self.take_word("WITH")?;
        if !with {
            self.word("RETURN")?;
        }
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct {
            self.take_all_quantifier()?;
        }
        let mut inputs = Vec::<Projection<'a>>::new();
        let mut outputs = Vec::<(Name<'a>, ReadValueTemplate)>::new();
        // A DISTINCT WITH's own ORDER BY may repeat an item's expression, as
        // a grouping WITH's may (`WITH DISTINCT a.name AS name ORDER BY
        // a.name`).
        let mut repeatable = Vec::new();
        if self.take(b'*')? {
            for variable in self.visible_graph_bindings() {
                let at = self.mutation_projection(&mut inputs, variable, None)?;
                outputs.push((variable, ReadValueTemplate::Column(at)));
            }
        } else {
            loop {
                self.capacity(
                    outputs.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let at = self.current.at;
                let implicit = if with {
                    self.implicit_with_binding()?
                } else {
                    None
                };
                let operand = self.read_graph_value(&mut inputs, 0)?;
                let end = self.current.at;
                if with && self.syntax.distinct && self.repeatable_item(at, end)? {
                    repeatable.push((at, end, outputs.len()));
                }
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if with {
                    implicit
                        .filter(|(_, boundary)| {
                            *boundary == end && matches!(operand, ReadValueTemplate::Column(_))
                        })
                        .map(|(name, _)| name)
                        .ok_or(GraphSetTextError {
                            offset: at,
                            kind: GraphSetTextErrorKind::Expected(
                                "AS alias for a computed WITH value",
                            ),
                        })?
                } else {
                    // openCypher names an unaliased RETURN item by its exact
                    // text (`a.name`, `labels(n)`, `n.p + 1`).
                    self.source_name(at, end)
                };
                if outputs.iter().any(|(name, _)| name.text == alias.text) {
                    return Err(error(
                        alias.at,
                        GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                    )
                    .into());
                }
                outputs.push((alias, operand));
                if !self.take(b',')? {
                    break;
                }
            }
        }
        self.projected_sources = (!repeatable.is_empty()).then_some((outputs.len(), repeatable));
        Ok(GraphProjectionHead {
            with,
            plain: with && !self.syntax.distinct,
            inputs,
            outputs,
        })
    }

    fn finish_graph_projection(
        mut self,
        statement: &'a str,
        head: GraphProjectionHead<'a>,
        pipeline: Vec<ReadStageTemplate>,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        let GraphProjectionHead {
            with,
            plain: _,
            mut inputs,
            outputs,
        } = head;
        if !with
            && outputs
                .iter()
                .all(|(_, operand)| matches!(operand, ReadValueTemplate::Column(_)))
        {
            // Keep the existing plan/counters/transcript for plain projections,
            // including repeated fields under different public aliases.
            self.syntax.columns = outputs
                .into_iter()
                .map(|(alias, operand)| {
                    let ReadValueTemplate::Column(index) = operand else {
                        unreachable!("plain projection checked above")
                    };
                    let source = inputs[index];
                    Column {
                        variable: source.variable,
                        property: source.property,
                        path: source.path,
                        alias,
                    }
                })
                .collect();
            return Ok(UnresolvedGraphText {
                statement,
                syntax: self.syntax,
                projection: None,
                pipeline,
                singleton: false,
                leading: Vec::new(),
                leading_types: Vec::new(),
                correlations: Vec::new(),
            });
        }
        if inputs.is_empty() {
            let variable = self.syntax.variables[0];
            let _ = self.mutation_projection(&mut inputs, variable, None)?;
        }
        self.syntax.columns = inputs
            .into_iter()
            .map(|source| Column {
                variable: source.variable,
                property: source.property,
                path: source.path,
                alias: source.variable,
            })
            .collect();
        let mut projection = Vec::new();
        for (alias, value) in outputs {
            projection.push(ReadProjectionTemplate {
                name: alias.text.to_owned(),
                value,
            });
        }
        Ok(UnresolvedGraphText {
            statement,
            syntax: self.syntax,
            projection: Some(projection),
            pipeline,
            singleton: false,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
        })
    }

    fn read_value_template(
        &self,
        operand: Operand,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphPatternTextError> {
        Ok(match operand {
            Operand::Column(input) => ReadValueTemplate::Column(input),
            Operand::Literal(value) => ReadValueTemplate::Literal(value),
            Operand::Number(Number::Literal(value)) => {
                ReadValueTemplate::Literal(scalar(value, at)?)
            }
            Operand::Number(Number::Parameter(index)) => ReadValueTemplate::parameter(
                index,
                self.syntax.parameter_offsets[index],
                self.syntax.parameters[index].parameter_type,
            ),
            Operand::Integer { program, at } => {
                if let [MutationIntegerTemplateOp::ParameterField { index, keys, at }] =
                    program.as_slice()
                {
                    // A lone field remains a general value in RETURN/CREATE:
                    // `$payload.meta` may itself be a map or list. Composed
                    // scalar expressions retain the native scalar program.
                    let mut value = ReadValueTemplate::parameter(
                        *index,
                        *at,
                        self.syntax.parameters[*index].parameter_type,
                    );
                    // A parameter never holds a graph element; its fields
                    // need no element key.
                    for key in keys {
                        value = ReadValueTemplate::MapGet {
                            map: Box::new(value),
                            key: key.clone(),
                            property: crate::set_text::ElementKeySlot::default(),
                        };
                    }
                    value
                } else {
                    ReadValueTemplate::Integer { program, at }
                }
            }
        })
    }

    /// openCypher map projection over a graph row (fgdb-20foe): `n{.a}` reads
    /// n's property a, `k: e` is any value, and `v` is the binding v itself.
    /// Keys are sorted and must be unique, as in a map literal. The source is
    /// the map's guard: a NULL n (an OPTIONAL MATCH without a witness) makes
    /// the whole projection NULL, never `{a: NULL}`. `.*` reads the complete
    /// admitted property map, and explicit entries replace its named fields.
    #[allow(clippy::type_complexity)]
    fn map_projection(
        &mut self,
        mut columns: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<ValueResolver<'_, 'a>>,
        depth: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let source = self.name()?;
        let carried = if columns.is_none() {
            self.boundary_property_map(schema, source.text).or_else(|| {
                schema.iter().position(|(name, kind)| {
                    name.text == source.text
                        && matches!(kind, GraphSetColumnType::Any | GraphSetColumnType::Scalar)
                })
            })
        } else {
            None
        };
        if columns.is_none() && carried.is_none() && resolve.is_none() {
            return Err(error(source.at, GraphPatternTextErrorKind::UnknownVariable).into());
        }
        self.punct(b'{', "{")?;
        let mut entries: Vec<(Box<str>, ReadValueTemplate)> = Vec::new();
        let mut all = false;
        if !self.take(b'}')? {
            loop {
                self.capacity(
                    entries.len(),
                    crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let entry = if self.take(b'.')? {
                    if self.take(b'*')? {
                        all = true;
                        if self.take(b'}')? {
                            break;
                        }
                        self.punct(b',', ", or }")?;
                        continue;
                    }
                    let key = self.map_key()?;
                    let value = if let Some(columns) = columns.as_deref_mut() {
                        ReadValueTemplate::Column(self.mutation_projection(
                            columns,
                            source,
                            Some(key),
                        )?)
                    } else if let Some(resolve) = resolve.as_mut() {
                        ReadValueTemplate::Column(resolve.map_input(
                            self,
                            source,
                            ReturnMapInput::Property(key),
                        )?)
                    } else {
                        self.map_get(
                            ReadValueTemplate::Column(carried.expect("carried map source checked")),
                            key.text,
                        )
                    };
                    (key.text.into(), value)
                } else {
                    let key = self.map_key()?;
                    if self.take(b':')? {
                        let value = self.read_recursive_value(
                            columns.as_deref_mut(),
                            schema,
                            resolve,
                            depth + 1,
                        )?;
                        (key.text.into(), value)
                    } else if let Some(index) =
                        schema.iter().position(|(name, _)| name.text == key.text)
                    {
                        (key.text.into(), ReadValueTemplate::Column(index))
                    } else if let Some(resolve) = resolve.as_mut() {
                        let column = resolve.map_input(self, key, ReturnMapInput::Identity)?;
                        (key.text.into(), ReadValueTemplate::Column(column))
                    } else if columns.is_some()
                        && (self
                            .syntax
                            .variables
                            .iter()
                            .any(|name| name.text == key.text)
                            || self.syntax.visible_edge(key.text).is_some())
                    {
                        let column = self.mutation_projection(
                            columns.as_deref_mut().expect("graph columns checked"),
                            key,
                            None,
                        )?;
                        (key.text.into(), ReadValueTemplate::Column(column))
                    } else {
                        return Err(
                            error(key.at, GraphPatternTextErrorKind::UnknownVariable).into()
                        );
                    }
                };
                entries.push(entry);
                if self.take(b'}')? {
                    break;
                }
                self.punct(b',', ", or }")?;
            }
        }
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::Expected("unique keys in a map projection"),
            });
        }
        let (keys, values): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
        if all {
            let column = match columns.as_deref_mut() {
                Some(columns) => self.property_map_projection(columns, source)?,
                None if resolve.is_some() => resolve
                    .as_mut()
                    .expect("resolver checked")
                    .map_input(self, source, ReturnMapInput::Properties)?,
                None => carried.expect("carried map source checked"),
            };
            return Ok(ReadValueTemplate::MapOverlay {
                base: Box::new(ReadValueTemplate::Column(column)),
                keys: keys.into_boxed_slice(),
                values,
            });
        }
        let guard = match columns {
            Some(columns) => {
                ReadValueTemplate::Column(self.mutation_projection(columns, source, None)?)
            }
            None if resolve.is_some() => {
                ReadValueTemplate::Column(resolve.as_mut().expect("resolver checked").map_input(
                    self,
                    source,
                    ReturnMapInput::Guard,
                )?)
            }
            None => ReadValueTemplate::MapOverlay {
                base: Box::new(ReadValueTemplate::Column(
                    carried.expect("carried map source checked"),
                )),
                keys: Box::new([]),
                values: Vec::new(),
            },
        };
        Ok(ReadValueTemplate::MapLiteral {
            keys: keys.into_boxed_slice(),
            values,
            guard: Some(Box::new(guard)),
        })
    }

    pub(super) fn read_graph_value(
        &mut self,
        inputs: &mut Vec<Projection<'a>>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(Some(inputs), &[], &mut None, depth)
    }

    pub(in crate::graph_text) fn read_row_value(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, schema, &mut None, depth)
    }

    /// Resolve grouped keys and aggregate calls as column leaves while using
    /// the same scalar precedence, parameters and list grammar as ordinary rows.
    pub(in crate::graph_text) fn read_resolved_value(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, &[], &mut Some(ValueResolver::Columns(resolve)), depth)
    }

    /// As [`Self::read_resolved_value`] for a whole RETURN or WITH item:
    /// comparisons, IS [NOT] NULL and Boolean connectors are part of it.
    pub(in crate::graph_text) fn read_resolved_item(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, &[], &mut Some(ValueResolver::Items(resolve)), depth)
    }

    pub(super) fn read_write_value(
        &mut self,
        leaves: &mut impl ReturnLeaves<'a>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, &[], &mut Some(ValueResolver::Write(leaves)), depth)
    }

    fn read_recursive_value(
        &mut self,
        inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<ValueResolver<'_, 'a>>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_value_inner(inputs, schema, resolve, depth, true)
    }

    /// The composite value a scalar expression reads through a let slot
    /// (fgdb-o5o9t). It never hands its own head back to the scalar compiler.
    pub(super) fn read_composite_operand(
        &mut self,
        inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_value_inner(inputs, schema, &mut None, depth, false)
    }

    /// A scalar expression in a graph or row scope: whole, or with `operand`
    /// one operand at the concatenation level (the list of an `IN`). Its
    /// composite operands wrap it as a Let.
    fn read_scalar_value(
        &mut self,
        inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        at: usize,
        depth: usize,
        operand: bool,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let base = core::mem::replace(&mut self.let_base, depth);
        let parsed = match (inputs, operand) {
            (Some(columns), false) => self.mutation_expression_with_lets(columns),
            (Some(columns), true) => self.mutation_operand_with_lets(columns),
            (None, false) => self.row_expression_with_lets(schema),
            (None, true) => self.row_operand_with_lets(schema),
        };
        self.let_base = base;
        let (operand, slots) = parsed.map_err(expression_error)?;
        self.scalar_let_value(operand, slots, at)
    }

    /// A parsed scalar operand and the let slots its composites took, as one
    /// value: the body inside a Let when there are slots.
    pub(super) fn scalar_let_value(
        &mut self,
        operand: Operand,
        slots: Vec<ReadValueTemplate>,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let body = self.read_value_template(operand, at)?;
        Ok(let_value(slots, body))
    }

    /// The temporal function called here (fgdb-txbg9), in any case: a
    /// constructor `date(`, `localtime(`, `time(`, `localdatetime(` or
    /// `datetime(`, or a dotted call such as `date.truncate(` or
    /// `datetime.fromepoch(`, which is reported with `true`.
    fn temporal_call(
        &self,
    ) -> Result<Option<(crate::GraphTemporalFunction, bool)>, GraphPatternTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(None);
        };
        let mut lexer = self.lexer.clone();
        match lexer.next()?.kind {
            TokenKind::Punct(b'(') => {
                Ok(crate::GraphTemporalFunction::named(word).map(|function| (function, false)))
            }
            TokenKind::Punct(b'.') => {
                let TokenKind::Word(operation) = lexer.next()?.kind else {
                    return Ok(None);
                };
                if !matches!(lexer.next()?.kind, TokenKind::Punct(b'(')) {
                    return Ok(None);
                }
                Ok(crate::GraphTemporalFunction::dotted(word, operation)
                    .map(|function| (function, true)))
            }
            _ => Ok(None),
        }
    }

    /// Whether a composite value starts here that a scalar expression reads
    /// through a let slot (fgdb-o5o9t): a list comprehension or quantifier,
    /// head/last/tail/range/reduce, keys(), a temporal constructor, or size()
    /// of a list literal, a parameter or a name. The scalar SIZE keeps a
    /// property read and every literal.
    pub(super) fn starts_let_operand(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        if self.is_punct(b'[') {
            return Ok(matches!(lexer.next()?.kind, TokenKind::Word(_))
                && matches!(lexer.next()?.kind,
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")));
        }
        if self.list_quantifier()?.is_some()
            || self.list_function()?.is_some()
            || self.value_function()?.is_some()
            || self.random_call()?
        {
            return Ok(true);
        }
        // A temporal value (fgdb-txbg9) is compared, converted and read in a
        // slot.
        if self.temporal_call()?.is_some() {
            return Ok(true);
        }
        let call = matches!(lexer.next()?.kind, TokenKind::Punct(b'('));
        if !call || !(self.is_word("KEYS") || self.is_word("SIZE")) {
            return Ok(false);
        }
        if self.is_word("KEYS") {
            return Ok(true);
        }
        // A literal keeps the scalar SIZE too, with its preparation-time
        // refusal of a non-text literal.
        let argument = lexer.next()?;
        Ok(match argument.kind {
            TokenKind::Word(word) => {
                !["TRUE", "FALSE", "NULL"]
                    .iter()
                    .any(|literal| word.eq_ignore_ascii_case(literal))
                    && !matches!(lexer.next()?.kind, TokenKind::Punct(b'.'))
            }
            TokenKind::Punct(b'[') | TokenKind::Parameter(_) => true,
            _ => false,
        })
    }

    /// Whether the value here is a let operand, or size(), followed by a
    /// scalar operator, as in `none(...) = all(...)` or `size(l) > 0`
    /// (fgdb-o5o9t). The lookahead skips the value and its postfix reads on
    /// a cloned lexer, so nothing is consumed or registered.
    fn composite_operand_continues(&self) -> Result<bool, GraphPatternTextError> {
        if self.starts_composite_comparison()? {
            return Ok(true);
        }
        // A list literal in a `+` or `-` chain (fgdb-c4i0d).
        if self.is_punct(b'[') {
            let mut lexer = self.lexer.clone();
            skip_group(&mut lexer)?;
            if matches!(
                after_postfix(&mut lexer)?.kind,
                TokenKind::Punct(b'+' | b'-')
            ) {
                return Ok(true);
            }
        }
        let size = self.is_word("SIZE")
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('));
        if !size && !self.starts_let_operand()? {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        if !self.is_punct(b'[') {
            lexer.next()?;
            // A dotted temporal call: step over `.name` to its `(`.
            if matches!(self.temporal_call()?, Some((_, true))) {
                lexer.next()?;
                lexer.next()?;
            }
        }
        skip_group(&mut lexer)?;
        Ok(match after_postfix(&mut lexer)?.kind {
            TokenKind::Punct(byte) => b"=<>!+-*/%^".contains(&byte),
            TokenKind::Word(word) => ["AND", "OR", "XOR", "IS", "IN", "STARTS", "ENDS", "CONTAINS"]
                .iter()
                .any(|operator| word.eq_ignore_ascii_case(operator)),
            _ => false,
        })
    }

    /// Whether a list or map literal starts here and, after its postfix
    /// reads, is compared with `=`, `<>`, `!=` or IN, as in `[1, 2] = [1]`
    /// or `{k: 1} <> {}` (fgdb-9cawv), or a list literal with `<`, `<=`, `>`
    /// or `>=`, as in `[1, 0] >= [1]`. The scalar compiler reads such a
    /// comparison whole in the value language; it never compares composites.
    pub(super) fn starts_composite_comparison(&self) -> Result<bool, GraphPatternTextError> {
        let list = self.is_punct(b'[');
        if !(self.is_punct(b'{') || (list && !self.starts_let_operand()?)) {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        skip_group(&mut lexer)?;
        Ok(match after_postfix(&mut lexer)?.kind {
            TokenKind::Punct(b'=') => !matches!(lexer.next()?.kind, TokenKind::Punct(b'~')),
            TokenKind::Punct(b'<') => list || matches!(lexer.next()?.kind, TokenKind::Punct(b'>')),
            TokenKind::Punct(b'>') => list,
            TokenKind::Punct(b'!') => matches!(lexer.next()?.kind, TokenKind::Punct(b'=')),
            TokenKind::Word(word) => word.eq_ignore_ascii_case("IN"),
            _ => false,
        })
    }

    /// Whether the chain starting here joins two or more operands with `+`
    /// or `-` and one of them is a list (list_operand_at), with nothing
    /// consumed: the lookahead steps over each product of the chain
    /// (fgdb-c4i0d). A lone list operand keeps its own reading.
    pub(super) fn list_chain_follows(
        &self,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut list = false;
        let mut joined = false;
        loop {
            while matches!(token.kind, TokenKind::Punct(b'-' | b'+')) {
                token = lexer.next()?;
            }
            list |= list_operand_at(token, &lexer, row)?;
            loop {
                token = skip_factor(token, &mut lexer)?;
                if !matches!(token.kind, TokenKind::Punct(b'*' | b'/' | b'%' | b'^')) {
                    break;
                }
                token = lexer.next()?;
                while matches!(token.kind, TokenKind::Punct(b'-' | b'+')) {
                    token = lexer.next()?;
                }
            }
            if !matches!(token.kind, TokenKind::Punct(b'+' | b'-')) {
                return Ok(list && joined);
            }
            joined = true;
            token = lexer.next()?;
        }
    }

    /// The visible list-typed row column a bare name here reads, when no
    /// `.`, call or subscript follows it (fgdb-c4i0d). A list sum or CASE
    /// reads it as a column value directly: the value reader would hand a
    /// bare name back to the scalar compiler, which re-reads the whole chain.
    pub(super) fn row_list_column(
        &self,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let (Some(schema), TokenKind::Word(word)) = (row, self.current.kind) else {
            return Ok(None);
        };
        if self.elements.contains(&word)
            || matches!(
                self.lexer.clone().next()?.kind,
                TokenKind::Punct(b'.' | b'(' | b'[')
            )
        {
            return Ok(None);
        }
        let visible = self.visible_width(schema);
        Ok(schema[..visible]
            .iter()
            .position(|(name, kind)| name.text == word && *kind == GraphSetColumnType::List))
    }

    /// Whether a list-typed row column (row_list_column) is compared here
    /// with `=`, `<>`, `!=`, `<`, `<=`, `>`, `>=` or IN, as in TCK
    /// WithOrderBy1 [45] `orderedX = range(0, n - 1)` (fgdb-9cawv): one
    /// comparison in the value language, as a list literal's is.
    pub(super) fn list_column_compared(
        &self,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<bool, GraphPatternTextError> {
        if self.row_list_column(row)?.is_none() {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        Ok(match lexer.next()?.kind {
            TokenKind::Punct(b'=') => !matches!(lexer.next()?.kind, TokenKind::Punct(b'~')),
            TokenKind::Punct(b'<' | b'>') => true,
            TokenKind::Punct(b'!') => matches!(lexer.next()?.kind, TokenKind::Punct(b'=')),
            TokenKind::Word(word) => word.eq_ignore_ascii_case("IN"),
            _ => false,
        })
    }

    /// starts_let_operand, or a list literal read through a subscript or
    /// slice (`[5, [6, 7], 10][1]`), whose element may be a list or a scalar
    /// (fgdb-c4i0d). A literal compared with `=` or IN was already taken by
    /// starts_composite_comparison.
    pub(super) fn let_operand_here(&self) -> Result<bool, GraphPatternTextError> {
        if self.starts_let_operand()? {
            return Ok(true);
        }
        if !self.is_punct(b'[') {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        skip_group(&mut lexer)?;
        Ok(matches!(lexer.next()?.kind, TokenKind::Punct(b'[')))
    }

    /// Whether a list operand of a `+` chain starts here (list_operand_at).
    pub(super) fn list_operand_here(
        &self,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<bool, GraphPatternTextError> {
        list_operand_at(self.current, &self.lexer, row)
    }

    /// Whether a CASE with a list result starts here (case_has_list).
    pub(super) fn list_case_follows(
        &self,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<bool, GraphPatternTextError> {
        Ok(self.is_word("CASE") && case_has_list(&self.lexer, row)?)
    }

    /// Whether a list operand here is a whole CASE result: WHEN, THEN, ELSE
    /// or END follows it.
    pub(super) fn lone_case_result(
        &self,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<bool, GraphPatternTextError> {
        if !list_operand_at(self.current, &self.lexer, row)? {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        Ok(matches!(skip_factor(self.current, &mut lexer)?.kind,
            TokenKind::Word(word) if ["WHEN", "THEN", "ELSE", "END"]
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword))))
    }

    #[allow(clippy::type_complexity)]
    fn read_value_inner(
        &mut self,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<ValueResolver<'_, 'a>>,
        depth: usize,
        operators: bool,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let at = self.current.at;
        // Taken here, so only this read is the bounded IN list (fgdb-klah2).
        let membership_list = core::mem::take(&mut self.membership_list);
        if depth > 64 {
            return Err(GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::IntegerNesting { limit: 64 },
            });
        }
        let mut graph_map = false;
        // A composite that is an operator's left operand is one operand of
        // the scalar expression, not the whole value. The aggregate scope
        // keeps its operand-only grammar.
        let scalar = operators && resolve.is_none() && self.composite_operand_continues()?;
        let comprehension = self.is_punct(b'[') && {
            let mut lexer = self.lexer.clone();
            matches!(lexer.next()?.kind, TokenKind::Word(_))
                && matches!(lexer.next()?.kind,
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("IN"))
        };
        let mut value = if scalar {
            self.read_scalar_value(inputs.as_deref_mut(), schema, at, depth, false)?
        } else if comprehension {
            // openCypher `[x IN list WHERE p | e]` (fgdb-20foe).
            self.advance()?;
            let (list, filter, map) =
                self.element_scope(inputs.as_deref_mut(), schema, resolve, depth, true)?;
            self.punct(b']', "]")?;
            ReadValueTemplate::Comprehension {
                list: Box::new(list),
                filter: filter.map(Box::new),
                map: map.map(Box::new),
            }
        } else if let Some(kind) = self.list_quantifier()? {
            // any/all/none/single(x IN list WHERE p) (fgdb-20foe).
            self.advance()?;
            self.punct(b'(', "(")?;
            let (list, predicate, _) =
                self.element_scope(inputs.as_deref_mut(), schema, resolve, depth, false)?;
            let predicate = predicate.ok_or(GraphSetTextError {
                offset: self.current.at,
                kind: GraphSetTextErrorKind::Expected("WHERE predicate of a list quantifier"),
            })?;
            self.punct(b')', ")")?;
            ReadValueTemplate::Quantifier {
                kind,
                list: Box::new(list),
                predicate: Box::new(predicate),
            }
        } else if self.take(b'[')? {
            let mut items = Vec::new();
            if !self.take(b']')? {
                loop {
                    self.capacity(
                        items.len(),
                        crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    items.push(self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?);
                    if self.take(b']')? {
                        break;
                    }
                    self.punct(b',', ", or ]")?;
                }
            }
            ReadValueTemplate::List(items)
        } else if self.take(b'{')? {
            // A map literal {k: e, ...} (fgdb-2jw3z), keys sorted and unique.
            let mut entries: Vec<(Box<str>, ReadValueTemplate)> = Vec::new();
            if !self.take(b'}')? {
                loop {
                    self.capacity(
                        entries.len(),
                        crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    let key = self.map_key()?;
                    self.punct(b':', ":")?;
                    let value = self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?;
                    entries.push((key.text.into(), value));
                    if self.take(b'}')? {
                        break;
                    }
                    self.punct(b',', ", or }")?;
                }
            }
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return Err(GraphSetTextError {
                    offset: at,
                    kind: GraphSetTextErrorKind::Expected("unique keys in a map literal"),
                });
            }
            let (keys, values): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
            ReadValueTemplate::MapLiteral {
                keys: keys.into_boxed_slice(),
                values,
                guard: None,
            }
        } else if (self.is_word("KEYS") || self.is_word("PROPERTIES"))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let keys = self.is_word("KEYS");
            let resolved = if inputs.is_none() && schema.is_empty() {
                match resolve.as_mut() {
                    Some(resolve) => resolve.leaf(self)?,
                    None => None,
                }
            } else {
                None
            };
            let (map, graph) = if let Some(column) = resolved {
                (ReadValueTemplate::Column(column), true)
            } else {
                self.advance()?;
                self.punct(b'(', "(")?;
                let source = match self.current.kind {
                    TokenKind::Word(word)
                        if !self.elements.contains(&word)
                            && matches!(
                                self.lexer.clone().next()?.kind,
                                TokenKind::Punct(b')')
                            ) =>
                    {
                        Some(Name {
                            text: word,
                            at: self.current.at,
                        })
                    }
                    _ => None,
                };
                let column = if let Some(source) = source {
                    if let Some(columns) = inputs.as_deref_mut()
                        && (self
                            .syntax
                            .variables
                            .iter()
                            .any(|name| name.text == source.text)
                            || self.syntax.visible_edge(source.text).is_some())
                    {
                        Some(self.property_map_projection(columns, source)?)
                    } else {
                        self.boundary_property_map(schema, source.text)
                    }
                } else {
                    None
                };
                let (map, graph) = if let Some(column) = column {
                    self.advance()?;
                    (ReadValueTemplate::Column(column), true)
                } else {
                    (
                        self.read_recursive_value(
                            inputs.as_deref_mut(),
                            schema,
                            resolve,
                            depth + 1,
                        )?,
                        false,
                    )
                };
                self.punct(b')', ")")?;
                (map, graph)
            };
            if keys {
                ReadValueTemplate::Keys(Box::new(map))
            } else if graph {
                graph_map = true;
                map
            } else {
                ReadValueTemplate::MapOverlay {
                    base: Box::new(map),
                    keys: Box::new([]),
                    values: Vec::new(),
                }
            }
        } else if let Some(function) = self.list_function()? {
            // head/last/tail/range/reduce (fgdb-20foe).
            self.advance()?;
            self.punct(b'(', "(")?;
            let value = self.list_function_call(
                function,
                inputs.as_deref_mut(),
                schema,
                resolve,
                depth,
                at,
            )?;
            self.punct(b')', ")")?;
            value
        } else if let Some(function) = self.value_function()? {
            // reverse() and split() (fgdb-13l85).
            self.advance()?;
            self.punct(b'(', "(")?;
            self.value_function_call(function, inputs.as_deref_mut(), schema, resolve, depth)?
        } else if self.composite_coalesce(schema)? {
            // coalesce over a list, map or element: a value function.
            self.advance()?;
            self.punct(b'(', "(")?;
            self.value_function_call(
                crate::GraphValueFunction::Coalesce,
                inputs.as_deref_mut(),
                schema,
                resolve,
                depth,
            )?
        } else if self.random_call()? {
            self.random_value()?
        } else if self.is_word("SIZE")
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            self.advance()?;
            self.punct(b'(', "(")?;
            let inner =
                self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
            self.punct(b')', ")")?;
            ReadValueTemplate::Size(Box::new(inner))
        } else if let Some((function, dotted)) = self.temporal_call()? {
            // A temporal constructor of a map, text, temporal value or NULL,
            // or a dotted call such as date.truncate(unit, value, map) whose
            // argument is the list of its arguments (fgdb-txbg9). An empty
            // constructor reads the statement clock, which no read has yet.
            let at = self.current.at;
            self.advance()?;
            if dotted {
                self.advance()?;
                self.advance()?;
            }
            self.punct(b'(', "(")?;
            let argument = if let Some((least, most)) = function.arity() {
                let mut items = Vec::new();
                loop {
                    items.push(self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?);
                    if self.take(b')')? {
                        break;
                    }
                    if items.len() == most {
                        return Err(GraphSetTextError {
                            offset: self.current.at,
                            kind: GraphSetTextErrorKind::Expected(")"),
                        });
                    }
                    self.punct(b',', ", or )")?;
                }
                if items.len() < least {
                    return Err(GraphSetTextError {
                        offset: at,
                        kind: GraphSetTextErrorKind::Expected("more temporal function arguments"),
                    });
                }
                ReadValueTemplate::List(items)
            } else {
                let argument =
                    self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
                self.punct(b')', ")")?;
                argument
            };
            ReadValueTemplate::Temporal {
                function,
                argument: Box::new(argument),
                zones: self.zones.clone(),
            }
        } else if let Some(parentheses) = self.parenthesized_name()? {
            // `(n)`, `((n))`: parentheses around a lone name that no operator
            // follows group nothing, so the name is read as it would be bare
            // (a vertex is an element, not a scalar operand; a grouped name
            // still resolves as a key).
            for _ in 0..parentheses {
                self.advance()?;
            }
            let inner = self.read_value_inner(
                inputs.as_deref_mut(),
                schema,
                resolve,
                depth + 1,
                operators,
            )?;
            for _ in 0..parentheses {
                self.punct(b')', ")")?;
            }
            inner
        } else if matches!(self.current.kind, TokenKind::Word(word)
                if !self.elements.contains(&word)
                    && (self.syntax.variables.iter().any(|name| name.text == word)
                        || self.syntax.visible_edge(word).is_some_and(|(edge, _)| edge.walk.is_none())
                        || schema.iter().any(|(name, _)| name.text == word)
                        || resolve.is_some()))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'{'))
        {
            self.map_projection(inputs.as_deref_mut(), schema, resolve, depth, at)?
        } else if let Some(resolve) = resolve.as_mut() {
            let operand = if resolve.whole() {
                self.resolved_predicate(&mut |parser| resolve.leaf(parser))
            } else {
                self.resolved_expression(&mut |parser| resolve.leaf(parser))
            }
            .map_err(expression_error)?;
            self.read_value_template(operand, at)?
        } else if let Some(columns) = inputs.as_deref_mut() {
            // `n IS NULL` is a scalar predicate over the element, which the
            // scalar compiler reads (element_null_operand).
            let bare = matches!(self.current.kind, TokenKind::Word(word)
                if !self.elements.contains(&word)
                    && (self.syntax.variables.iter().any(|name| name.text == word)
                    || self.syntax.names_path(word)
                    || self.syntax.visible_edge(word).is_some()))
                // `v:L` is a label test, an expression over the element
                // (fgdb-qo3jv), never the bare element.
                && !matches!(
                    self.lexer.clone().next()?.kind,
                    TokenKind::Punct(b'.' | b'(' | b':')
                )
                && !matches!(self.lexer.clone().next()?.kind,
                    TokenKind::Word(next) if next.eq_ignore_ascii_case("IS"));
            if bare {
                let variable = self.any_variable()?;
                ReadValueTemplate::Column(self.mutation_projection(columns, variable, None)?)
            } else {
                self.read_scalar_value(Some(columns), schema, at, depth, membership_list)?
            }
        } else {
            self.read_scalar_value(None, schema, at, depth, membership_list)?
        };
        // A lone element is the element's whole value, not a scalar program,
        // so a list, vertex or path element passes through intact.
        if let ReadValueTemplate::Integer { program, .. } = &value
            && let [MutationIntegerTemplateOp::Bound(crate::GraphIntegerOp::Local(offset))] =
                program.as_slice()
        {
            value = ReadValueTemplate::Local(*offset);
        }
        loop {
            // `.key` reads a map entry (fgdb-2jw3z), only on a value that can
            // statically be a map, or a temporal field of a scalar column. A
            // graph property read or a WITH-carried vertex's hidden read never
            // reaches this postfix, and on any other value the `.` stays
            // unconsumed, so a vertex column's `.p` past its readable scope
            // still refuses at preparation.
            if self.is_punct(b'.')
                && let TokenKind::Word(key) | TokenKind::DelimitedKeyword(key) =
                    self.lexer.clone().next()?.kind
                && (graph_map
                    || map_capable(
                        &value,
                        key,
                        (inputs.is_none() && resolve.is_none()).then_some(schema),
                        &self.syntax.parameters,
                    ))
            {
                self.advance()?;
                let key = self.map_key()?;
                value = self.map_get(value, key.text);
                graph_map = false;
                continue;
            }
            if !self.take(b'[')? {
                break;
            }
            // `[i]` indexes; `[a..b]`, `[a..]` and `[..b]` slice (fgdb-20foe).
            let from = if self.starts_range_dots()? {
                None
            } else {
                Some(self.read_recursive_value(
                    inputs.as_deref_mut(),
                    schema,
                    resolve,
                    depth + 1,
                )?)
            };
            if self.starts_range_dots()? {
                self.advance()?;
                self.advance()?;
                let to = if self.is_punct(b']') {
                    None
                } else {
                    Some(self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?)
                };
                self.punct(b']', "]")?;
                value = ReadValueTemplate::Slice {
                    list: Box::new(value),
                    from: from.map(Box::new),
                    to: to.map(Box::new),
                };
            } else {
                let index = from.ok_or(GraphSetTextError {
                    offset: self.current.at,
                    kind: GraphSetTextErrorKind::Expected("a list index or slice"),
                })?;
                self.punct(b']', "]")?;
                value = ReadValueTemplate::Index {
                    list: Box::new(value),
                    index: Box::new(index),
                };
            }
        }
        Ok(value)
    }

    fn starts_range_dots(&self) -> Result<bool, GraphPatternTextError> {
        Ok(
            self.is_punct(b'.')
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.')),
        )
    }

    /// RAND directly followed by `(` (fgdb-13l85).
    fn random_call(&self) -> Result<bool, GraphPatternTextError> {
        Ok(self.is_word("RAND")
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')))
    }

    /// `rand()`: the next call site, keyed on the element locals in force.
    /// Out of line, so the value reader's recursive frame stays as narrow as
    /// before.
    #[inline(never)]
    fn random_value(&mut self) -> Result<ReadValueTemplate, GraphSetTextError> {
        let at = self.current.at;
        if self.aggregate_argument {
            return Err(GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::Expected(
                    "a deterministic aggregate argument: rand() inside an aggregate is a non-constant expression",
                ),
            });
        }
        self.advance()?;
        self.punct(b'(', "(")?;
        self.punct(b')', "() without arguments")?;
        let overflow = || GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::Expected("fewer rand() calls"),
        };
        let site = self.random_sites;
        self.random_sites = site.checked_add(1).ok_or_else(overflow)?;
        let scope = u32::try_from(self.elements.len()).map_err(|_| overflow())?;
        Ok(ReadValueTemplate::Random { site, scope })
    }

    /// REVERSE or SPLIT directly followed by `(` (fgdb-13l85).
    fn value_function(&self) -> Result<Option<crate::GraphValueFunction>, GraphPatternTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(None);
        };
        let Some(function) = crate::GraphValueFunction::named(word) else {
            return Ok(None);
        };
        Ok(matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')).then_some(function))
    }

    /// `coalesce(` with a composite argument: a list or map literal, a call
    /// that makes a list, or a row column that is not a scalar. That call is
    /// the value function; the scalar COALESCE keeps every all-scalar call.
    /// Only lexical lookahead: nothing is read or registered.
    fn composite_coalesce(
        &self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("COALESCE") {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        if !matches!(lexer.next()?.kind, TokenKind::Punct(b'(')) {
            return Ok(false);
        }
        let mut depth = 1_usize;
        let mut first = true;
        loop {
            let token = lexer.next()?;
            if first && depth == 1 {
                first = false;
                match token.kind {
                    TokenKind::Punct(b'[' | b'{') => return Ok(true),
                    TokenKind::Word(word) => {
                        let next = lexer.clone().next()?.kind;
                        let bare = matches!(next, TokenKind::Punct(b',' | b')'));
                        if bare
                            && schema.iter().any(|(name, kind)| {
                                name.text == word && *kind != GraphSetColumnType::Scalar
                            })
                        {
                            return Ok(true);
                        }
                        let listing = [
                            "COLLECT",
                            "REVERSE",
                            "SPLIT",
                            "RANGE",
                            "TAIL",
                            "KEYS",
                            "LABELS",
                            "NODES",
                            "RELATIONSHIPS",
                        ];
                        if matches!(next, TokenKind::Punct(b'('))
                            && listing.iter().any(|name| word.eq_ignore_ascii_case(name))
                        {
                            return Ok(true);
                        }
                    }
                    _ => {}
                }
            }
            match token.kind {
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(false);
                    }
                }
                TokenKind::Punct(b',') if depth == 1 => first = true,
                TokenKind::End => return Ok(false),
                _ => {}
            }
        }
    }

    /// The arguments and closing parenthesis of a value function call, after
    /// its `(`: one argument is the value, more are the list of them. Out of
    /// line, so the value reader's recursive frame stays as narrow as before.
    #[inline(never)]
    fn value_function_call(
        &mut self,
        function: crate::GraphValueFunction,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<ValueResolver<'_, 'a>>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let mut arguments = Vec::new();
        loop {
            arguments.push(self.read_recursive_value(
                inputs.as_deref_mut(),
                schema,
                resolve,
                depth + 1,
            )?);
            match function.arity() {
                Some(arity) if arguments.len() == arity => break,
                None if !self.is_punct(b',') => break,
                _ => self.punct(b',', ", and the next argument")?,
            }
        }
        self.punct(b')', ")")?;
        let argument = if arguments.len() == 1 {
            arguments.pop().expect("one argument")
        } else {
            ReadValueTemplate::List(arguments)
        };
        Ok(ReadValueTemplate::Function {
            function,
            argument: Box::new(argument),
        })
    }

    /// HEAD, LAST, TAIL, RANGE or REDUCE directly followed by `(`.
    fn list_function(&self) -> Result<Option<ListFunction>, GraphPatternTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(None);
        };
        let function = [
            ("HEAD", ListFunction::Head),
            ("LAST", ListFunction::Last),
            ("TAIL", ListFunction::Tail),
            ("RANGE", ListFunction::Range),
            ("REDUCE", ListFunction::Reduce),
        ]
        .into_iter()
        .find(|(name, _)| word.eq_ignore_ascii_case(name))
        .map(|(_, function)| function);
        let Some(function) = function else {
            return Ok(None);
        };
        Ok(matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')).then_some(function))
    }

    /// The arguments of a list function, after `(` and before `)`. head and
    /// last are index desugars, tail a slice from 1; reduce binds its
    /// accumulator then its element, innermost last.
    #[allow(clippy::type_complexity)]
    fn list_function_call(
        &mut self,
        function: ListFunction,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<ValueResolver<'_, 'a>>,
        depth: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let integer = |value: i64| -> Result<ReadValueTemplate, GraphSetTextError> {
            Ok(ReadValueTemplate::Literal(scalar(
                GqlParameterValue::Int64(value),
                at,
            )?))
        };
        let next = |parser: &mut Self,
                    inputs: Option<&mut Vec<Projection<'a>>>,
                    resolve: &mut Option<_>| {
            parser.read_recursive_value(inputs, schema, resolve, depth + 1)
        };
        Ok(match function {
            ListFunction::Head | ListFunction::Last | ListFunction::Tail => {
                let list = Box::new(next(self, inputs, resolve)?);
                match function {
                    ListFunction::Head => ReadValueTemplate::Index {
                        list,
                        index: Box::new(integer(0)?),
                    },
                    ListFunction::Last => ReadValueTemplate::Index {
                        list,
                        index: Box::new(integer(-1)?),
                    },
                    _ => ReadValueTemplate::Slice {
                        list,
                        from: Some(Box::new(integer(1)?)),
                        to: None,
                    },
                }
            }
            ListFunction::Range => {
                let start = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                self.punct(b',', ",")?;
                let end = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                let step = if self.take(b',')? {
                    Some(Box::new(next(self, inputs, resolve)?))
                } else {
                    None
                };
                ReadValueTemplate::Range { start, end, step }
            }
            ListFunction::Reduce => {
                let accumulator = self.name()?;
                self.punct(b'=', "=")?;
                let init = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                self.punct(b',', ",")?;
                let element = self.name()?;
                self.word("IN")?;
                let list = Box::new(next(self, inputs.as_deref_mut(), resolve)?);
                self.punct(b'|', "|")?;
                self.elements.push(accumulator.text);
                self.elements.push(element.text);
                let expr = next(self, inputs, resolve);
                self.elements.truncate(self.elements.len() - 2);
                ReadValueTemplate::Reduce {
                    init,
                    list,
                    expr: Box::new(expr?),
                }
            }
        })
    }

    /// `x IN list (WHERE p)? (| e)?`, with `x` in scope only for `p` and `e`
    /// (fgdb-20foe). The scope is popped on every exit, errors included.
    #[allow(clippy::type_complexity)]
    fn element_scope(
        &mut self,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<ValueResolver<'_, 'a>>,
        depth: usize,
        projection: bool,
    ) -> Result<
        (
            ReadValueTemplate,
            Option<ReadValueTemplate>,
            Option<ReadValueTemplate>,
        ),
        GraphSetTextError,
    > {
        let element = self.name()?;
        self.word("IN")?;
        let list = self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
        self.elements.push(element.text);
        let parts = self.element_parts(inputs, schema, resolve, depth, projection);
        self.elements.pop();
        let (filter, map) = parts?;
        Ok((list, filter, map))
    }

    #[allow(clippy::type_complexity)]
    fn element_parts(
        &mut self,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<ValueResolver<'_, 'a>>,
        depth: usize,
        projection: bool,
    ) -> Result<(Option<ReadValueTemplate>, Option<ReadValueTemplate>), GraphSetTextError> {
        let filter = if self.take_word("WHERE")? {
            // A predicate parses the whole Boolean expression in every scope,
            // including the operand-only aggregate-resolution scope.
            let at = self.current.at;
            Some(if let Some(resolve) = resolve.as_mut() {
                // Composite operands and element entries (`x.age`) take let
                // slots here too, as read_scalar_value's do (fgdb-1c29d).
                let base = core::mem::replace(&mut self.let_base, depth + 1);
                let parsed = self.resolved_predicate_with_lets(&mut |parser| resolve.leaf(parser));
                self.let_base = base;
                let (operand, slots) = parsed.map_err(expression_error)?;
                self.scalar_let_value(operand, slots, at)?
            } else {
                self.read_scalar_value(inputs.as_deref_mut(), schema, at, depth + 1, false)?
            })
        } else {
            None
        };
        let map = if projection && self.take(b'|')? {
            Some(self.read_recursive_value(inputs, schema, resolve, depth + 1)?)
        } else {
            None
        };
        Ok((filter, map))
    }

    /// `ANY`, `ALL`, `NONE` or `SINGLE` opening `(x IN ...`: the same
    /// lookahead that keeps RETURN/WITH from reading `all(` as `ALL`.
    fn list_quantifier(&self) -> Result<Option<crate::GraphListQuantifier>, GraphPatternTextError> {
        use crate::GraphListQuantifier as Quantifier;
        if !self.starts_list_quantifier()? {
            return Ok(None);
        }
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(None);
        };
        let kind = [
            ("ANY", Quantifier::Any),
            ("ALL", Quantifier::All),
            ("NONE", Quantifier::None),
            ("SINGLE", Quantifier::Single),
        ]
        .into_iter()
        .find(|(name, _)| word.eq_ignore_ascii_case(name))
        .map(|(_, kind)| kind);
        Ok(kind)
    }
}

impl BoundSetTextInput {
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }
    pub(crate) fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphPatternTextError> {
        if let Some(selection) = &self.selection {
            return selection.checked_arguments(arguments);
        }
        // Signed client integers for SKIP/LIMIT parameters (fgdb-3dnpu).
        let admitted = arguments.admit_unsigned(&self.parameters);
        let arguments: &GqlParameters = &admitted;
        let mut values = Vec::new();
        for (index, spec) in self.parameters.iter().enumerate() {
            let at = self.parameter_offsets[index];
            let value = arguments
                .get(&spec.name)
                .ok_or_else(|| error(at, GraphPatternTextErrorKind::MissingParameter))?;
            if !spec.parameter_type.accepts(value.parameter_type()) {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::ParameterTypeMismatch {
                        expected: spec.parameter_type,
                        found: value.parameter_type(),
                    },
                ));
            }
            values.push(value);
        }
        if arguments.len() != values.len() {
            return Err(error(
                self.return_at,
                GraphPatternTextErrorKind::UnexpectedArguments,
            ));
        }
        Ok(values)
    }

    /// Only callers that checked the COMPLETE native argument table may use
    /// this path. Shared grouped terminals need that same table for HAVING/page.
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut input = if self.leading.is_empty() && !self.singleton {
            self.selection
                .as_ref()
                .expect("graph source")
                .bind_values(values)?
                .into()
        } else {
            let mut leading = bind_stages(PreparedGraphSet::singleton(), &self.leading, values)?;
            if let Some(selection) = &self.selection {
                let width = leading.column_types().len();
                leading = leading
                    .cross_join(selection.bind_values(values)?.into())
                    .map_err(|kind| GraphSetTextError {
                        offset: self.return_at,
                        kind: GraphSetTextErrorKind::SetBuild(kind),
                    })?;
                let mut code = Vec::new();
                for &(left, right) in &self.correlations {
                    code.push(crate::GraphSetPredicateOp::Compare {
                        left: crate::GraphSetOperand::Column(left),
                        comparison: IntegerComparison::Equal,
                        right: crate::GraphSetOperand::Column(width + right),
                    });
                    if code.len() > 1 {
                        code.push(crate::GraphSetPredicateOp::And);
                    }
                }
                if !code.is_empty() {
                    leading = leading.filter(&code).map_err(|kind| GraphSetTextError {
                        offset: self.return_at,
                        kind: GraphSetTextErrorKind::FilterBuild(kind),
                    })?;
                }
            }
            leading
        };
        if let Some(projection) = &self.projection {
            input = bind_projection(input, projection, self.quantifier, values, self.return_at)?;
        }
        bind_stages(input, &self.pipeline, values)
    }
}

pub(super) fn bind_stages(
    mut input: PreparedGraphSet,
    stages: &[ReadStageTemplate],
    values: &[GqlParameterValue],
) -> Result<PreparedGraphSet, GraphSetTextError> {
    for stage in stages {
        input = match stage {
            ReadStageTemplate::Aggregate { at, stage } => stage.bind(input, values, *at)?,
            ReadStageTemplate::Unwind { at, name, value } => input
                .unwind(name.clone(), bind_unwind_value(value, values)?)
                .map_err(|kind| GraphSetTextError {
                    offset: *at,
                    kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                })?,
            ReadStageTemplate::Call {
                at,
                namespace,
                name,
                arguments,
                names,
                outputs,
                vertices,
                graph_projection,
                ..
            } => {
                let mut bound = Vec::with_capacity(arguments.len());
                for argument in arguments {
                    bound.push(bind_read_value(argument, values)?);
                }
                input
                    .procedure_call_with_projection(
                        namespace.clone(),
                        name.clone(),
                        bound,
                        names.clone(),
                        outputs.clone(),
                        vertices.clone(),
                        *graph_projection,
                    )
                    .map_err(|kind| GraphSetTextError {
                        offset: *at,
                        kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                    })?
            }
            ReadStageTemplate::Project {
                at,
                projection,
                quantifier,
            } => bind_projection(input, projection, *quantifier, values, *at)?,
            ReadStageTemplate::Filter { at, code } => {
                let code = pipeline::bind_filter(code, Some(values))?;
                input.filter(&code).map_err(|kind| GraphSetTextError {
                    offset: *at,
                    kind: GraphSetTextErrorKind::FilterBuild(kind),
                })?
            }
            ReadStageTemplate::Page {
                at,
                order,
                offset,
                count,
                computed,
            } => {
                let visible = computed.as_ref().map(|projection| projection.visible);
                if let Some(computed) = computed {
                    let mut projection: Vec<_> = input
                        .columns()
                        .iter()
                        .enumerate()
                        .map(|(column, name)| {
                            GraphSetProjection::new(name.clone(), GraphSetValue::Column(column))
                        })
                        .collect();
                    for value in &computed.values {
                        let mut ordinal = projection.len();
                        let name = loop {
                            let name = format!("__fg_order_{ordinal}");
                            if projection.iter().all(|column| column.name() != name) {
                                break name;
                            }
                            ordinal += 1;
                        };
                        projection.push(GraphSetProjection::new(
                            name,
                            bind_read_value(value, values)?,
                        ));
                    }
                    input = input
                        .project(projection, crate::GraphSetQuantifier::All)
                        .map_err(|kind| GraphSetTextError {
                            offset: *at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        })?;
                }
                if !order.is_empty() {
                    input = input
                        .with_order_by(order)
                        .map_err(|kind| GraphSetTextError {
                            offset: *at,
                            kind: GraphSetTextErrorKind::OrderBuild(kind),
                        })?;
                }
                input = input.with_page(
                    pipeline::page_value(offset, values),
                    count
                        .as_ref()
                        .map(|count| pipeline::page_value(count, values)),
                );
                if let Some(visible) = visible {
                    let projection = input.columns()[..visible]
                        .iter()
                        .enumerate()
                        .map(|(column, name)| {
                            GraphSetProjection::new(name.clone(), GraphSetValue::Column(column))
                        })
                        .collect();
                    input = input.project_ordered_output(projection).map_err(|kind| {
                        GraphSetTextError {
                            offset: *at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        }
                    })?;
                }
                input
            }
        };
    }
    Ok(input)
}

fn bind_projection(
    input: PreparedGraphSet,
    projection: &[ReadProjectionTemplate],
    quantifier: crate::GraphSetQuantifier,
    values: &[GqlParameterValue],
    at: usize,
) -> Result<PreparedGraphSet, GraphSetTextError> {
    let mut columns = Vec::new();
    for output in projection {
        let value = bind_read_value(&output.value, values)?;
        columns.push(GraphSetProjection::new(output.name.clone(), value));
    }
    input
        .project(columns, quantifier)
        .map_err(|kind| GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
        })
}

/// Preserve the parameter's declared nullable domain instead of deriving its
/// schema from a NULL payload. Both row and aggregate projections share this
/// lowering after their complete argument maps have passed type admission.
pub(in crate::graph_text) fn bind_read_parameter(
    value: &GqlParameterValue,
    at: usize,
    null_type: GraphSetColumnType,
) -> Result<GraphSetValue, GraphPatternTextError> {
    Ok(match value {
        GqlParameterValue::List(value) => {
            if value.canonical_bytes().len() > crate::algebra::MAX_SCALAR_PREDICATE_BYTES {
                return Err(error(at, GraphPatternTextErrorKind::ScalarLiteral));
            }
            GraphSetValue::Value(value.value().clone())
        }
        GqlParameterValue::Map(value) => GraphSetValue::Value(value.value().clone()),
        value => {
            let value = scalar(value.clone(), at)?;
            if null_type != GraphSetColumnType::Scalar
                && matches!(value.value(), CanonicalScalar::Null)
            {
                GraphSetValue::TypedNull(null_type)
            } else {
                GraphSetValue::Literal(value)
            }
        }
    })
}

/// A direct UNWIND list argument is a row source, not one predicate operand.
/// Only that source position may consume the larger admitted document. Every
/// nested expression and every other use of the same parameter still binds
/// through `bind_read_parameter`, which checks its cap before copying it.
fn bind_unwind_value(
    value: &ReadValueTemplate,
    values: &[GqlParameterValue],
) -> Result<GraphSetValue, GraphSetTextError> {
    if let ReadValueTemplate::Parameter { index, .. } = value
        && let GqlParameterValue::List(value) = &values[*index]
    {
        return Ok(GraphSetValue::Value(value.value().clone()));
    }
    bind_read_value(value, values)
}

pub(in crate::graph_text) fn bind_read_value(
    value: &ReadValueTemplate,
    values: &[GqlParameterValue],
) -> Result<GraphSetValue, GraphSetTextError> {
    Ok(match value {
        ReadValueTemplate::Column(input) => GraphSetValue::Column(*input),
        ReadValueTemplate::Literal(value) => GraphSetValue::Literal(value.clone()),
        ReadValueTemplate::Parameter {
            index,
            at,
            null_type,
        } => bind_read_parameter(&values[*index], *at, *null_type)?,
        ReadValueTemplate::Integer { program, at } => GraphSetValue::Integer(
            integer::bind_integer(program, values, *at).map_err(expression_error)?,
        ),
        ReadValueTemplate::List(items) => GraphSetValue::List(
            items
                .iter()
                .map(|value| bind_read_value(value, values))
                .collect::<Result<_, _>>()?,
        ),
        ReadValueTemplate::Index { list, index } => GraphSetValue::Index {
            list: Box::new(bind_read_value(list, values)?),
            index: Box::new(bind_read_value(index, values)?),
        },
        ReadValueTemplate::In { value, list } => GraphSetValue::In {
            value: Box::new(bind_read_value(value, values)?),
            list: Box::new(bind_read_value(list, values)?),
        },
        ReadValueTemplate::Equal { left, right } => GraphSetValue::Equal {
            left: Box::new(bind_read_value(left, values)?),
            right: Box::new(bind_read_value(right, values)?),
        },
        ReadValueTemplate::Ordered {
            comparison,
            left,
            right,
        } => GraphSetValue::Ordered {
            comparison: *comparison,
            left: Box::new(bind_read_value(left, values)?),
            right: Box::new(bind_read_value(right, values)?),
        },
        ReadValueTemplate::Temporal {
            function,
            argument,
            zones,
        } => GraphSetValue::Temporal {
            function: *function,
            argument: Box::new(bind_read_value(argument, values)?),
            zones: zones.clone(),
        },
        ReadValueTemplate::Function { function, argument } => GraphSetValue::Function {
            function: *function,
            argument: Box::new(bind_read_value(argument, values)?),
        },
        ReadValueTemplate::Case { arms, otherwise } => GraphSetValue::Case {
            arms: arms
                .iter()
                .map(|(condition, value)| -> Result<_, GraphSetTextError> {
                    Ok((
                        bind_read_value(condition, values)?,
                        bind_read_value(value, values)?,
                    ))
                })
                .collect::<Result<_, _>>()?,
            otherwise: otherwise
                .as_deref()
                .map(|value| bind_read_value(value, values).map(Box::new))
                .transpose()?,
        },
        ReadValueTemplate::Size(value) => {
            GraphSetValue::Size(Box::new(bind_read_value(value, values)?))
        }
        ReadValueTemplate::Local(offset) => GraphSetValue::Local(*offset),
        ReadValueTemplate::Random { site, scope } => GraphSetValue::Random {
            site: *site,
            scope: *scope,
        },
        ReadValueTemplate::Comprehension { list, filter, map } => {
            let part = |part: &Option<Box<ReadValueTemplate>>| {
                part.as_deref()
                    .map(|part| bind_read_value(part, values).map(Box::new))
                    .transpose()
            };
            GraphSetValue::Comprehension {
                list: Box::new(bind_read_value(list, values)?),
                filter: part(filter)?,
                map: part(map)?,
            }
        }
        ReadValueTemplate::Quantifier {
            kind,
            list,
            predicate,
        } => GraphSetValue::Quantifier {
            kind: *kind,
            list: Box::new(bind_read_value(list, values)?),
            predicate: Box::new(bind_read_value(predicate, values)?),
        },
        ReadValueTemplate::Slice { list, from, to } => {
            let part = |part: &Option<Box<ReadValueTemplate>>| {
                part.as_deref()
                    .map(|part| bind_read_value(part, values).map(Box::new))
                    .transpose()
            };
            GraphSetValue::Slice {
                list: Box::new(bind_read_value(list, values)?),
                from: part(from)?,
                to: part(to)?,
            }
        }
        ReadValueTemplate::Range { start, end, step } => GraphSetValue::Range {
            start: Box::new(bind_read_value(start, values)?),
            end: Box::new(bind_read_value(end, values)?),
            step: step
                .as_deref()
                .map(|step| bind_read_value(step, values).map(Box::new))
                .transpose()?,
        },
        ReadValueTemplate::Reduce { init, list, expr } => GraphSetValue::Reduce {
            init: Box::new(bind_read_value(init, values)?),
            list: Box::new(bind_read_value(list, values)?),
            expr: Box::new(bind_read_value(expr, values)?),
        },
        ReadValueTemplate::MapLiteral {
            keys,
            values: entries,
            guard,
        } => GraphSetValue::MapLiteral {
            keys: keys.clone(),
            values: entries
                .iter()
                .map(|value| bind_read_value(value, values))
                .collect::<Result<_, _>>()?,
            guard: guard
                .as_deref()
                .map(|guard| bind_read_value(guard, values).map(Box::new))
                .transpose()?,
        },
        ReadValueTemplate::MapGet { map, key, property } => GraphSetValue::MapGet {
            map: Box::new(bind_read_value(map, values)?),
            key: key.clone(),
            property: property.bind(),
        },
        ReadValueTemplate::MapOverlay {
            base,
            keys,
            values: entries,
        } => GraphSetValue::MapOverlay {
            base: Box::new(bind_read_value(base, values)?),
            keys: keys.clone(),
            values: entries
                .iter()
                .map(|value| bind_read_value(value, values))
                .collect::<Result<_, _>>()?,
        },
        ReadValueTemplate::Keys(map) => {
            GraphSetValue::Keys(Box::new(bind_read_value(map, values)?))
        }
        ReadValueTemplate::Let {
            values: slots,
            body,
        } => GraphSetValue::Let {
            values: slots
                .iter()
                .map(|value| bind_read_value(value, values))
                .collect::<Result<_, _>>()?,
            body: Box::new(bind_read_value(body, values)?),
        },
    })
}

#[cfg(test)]
mod graph_scope_tests {
    use super::*;
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::{
        GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, PreparedGraphSetText,
    };

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        }
    }

    fn check_schema(text: &str, expected: &[GraphSetColumnType]) {
        let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(prepared.column_types(), expected, "{text}");
        assert_eq!(
            prepared
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .column_types(),
            expected,
            "{text}"
        );
    }

    #[test]
    fn metadata_scalars_keep_their_types_through_filters_and_alias_shadowing() {
        for text in [
            "MATCH p = (a)-[:R]->{1,2}(b) WITH path_length(p) AS hops \
             WHERE hops > 0 ORDER BY hops DESC LIMIT 1 RETURN hops + 1 AS score",
            "MATCH p = (a)-[:R]->{1,2}(b) WITH path_length(p) AS p RETURN p + 1 AS score",
            "MATCH (a)-[e:R]->(b) WITH type(e) AS name RETURN upper(name) AS value",
        ] {
            check_schema(text, &[GraphSetColumnType::Scalar]);
        }
        check_schema(
            "MATCH p = (a)-[:R]->{1,2}(b) WITH p AS route, nodes(p) AS vertices, \
             edges(p) AS relationships RETURN route, vertices, relationships",
            &[
                GraphSetColumnType::Path,
                GraphSetColumnType::Vertices,
                GraphSetColumnType::Edges,
            ],
        );
        check_schema(
            "MATCH (a) WITH labels(a) AS names RETURN names",
            &[GraphSetColumnType::List],
        );
    }

    #[test]
    fn wildcards_and_implicit_unwind_preserve_named_graph_bindings() {
        for terminal in ["RETURN *", "WITH * RETURN *"] {
            check_schema(
                &format!("MATCH p = (a)-[:R]->{{1,2}}(b) {terminal}"),
                &[
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Path,
                ],
            );
            check_schema(
                &format!("MATCH (a)-[e:R]->(b) {terminal}"),
                &[
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Vertex,
                    GraphSetColumnType::Edge,
                ],
            );
        }
        check_schema(
            "MATCH (a)-[e:R]->(b) UNWIND [1] AS value RETURN e, value",
            &[GraphSetColumnType::Edge, GraphSetColumnType::Any],
        );
        check_schema(
            "MATCH p = (a)-[:R]->{1,2}(b) UNWIND [1] AS value RETURN p, value",
            &[GraphSetColumnType::Path, GraphSetColumnType::Any],
        );
        let prepared =
            PreparedGraphSetText::prepare("MATCH ()-[e:R]->() WITH * RETURN *", symbols).unwrap();
        assert_eq!(prepared.columns(), &["e".to_owned()]);
        assert_eq!(prepared.column_types(), &[GraphSetColumnType::Edge]);
        assert!(prepared.bind_parameters(&GqlParameters::new()).is_ok());
    }

    #[test]
    fn bulk_list_sources_do_not_enlarge_predicate_or_expression_operands() {
        let values: Vec<_> = (0..5_000)
            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
            .collect();
        let arguments = GqlParameters::new().with_list("rows", values).unwrap();
        let small = GqlParameters::new()
            .with_list("rows", vec![GraphValue::Scalar(CanonicalScalar::Int(1))])
            .unwrap();
        let types = [("rows", GqlParameterType::List)];

        for text in [
            "UNWIND $rows AS row RETURN row",
            "UNWIND $rows AS row WITH row WHERE row > 2 RETURN row LIMIT 1",
        ] {
            let prepared =
                PreparedGraphSetText::prepare_with_parameter_types(text, &types, symbols).unwrap();
            let bound = prepared.bind_parameters(&arguments).unwrap();
            let output = bound
                .execute_governed(
                    GqlQueryPolicy::new(0, 10_000, 1_000_000, 1_000_000),
                    |_, _| -> Result<_, GqlQueryError<(), ()>> {
                        panic!("a parameter-only UNWIND must not open a graph source")
                    },
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            let expected: Vec<_> = if text.ends_with("LIMIT 1") {
                vec![3]
            } else {
                (0..5_000).collect()
            };
            assert_eq!(
                output.value,
                expected
                    .into_iter()
                    .map(
                        |value| GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                            CanonicalScalar::Int(value)
                        )])
                    )
                    .collect::<Vec<_>>(),
                "{text}"
            );
        }
        for text in [
            "RETURN $rows AS value",
            "RETURN 1 IN $rows AS value",
            "RETURN $rows = [1] AS value",
            "RETURN size($rows) AS value",
            "UNWIND $rows AS row WITH row WHERE row IN $rows RETURN row",
            "UNWIND [$rows] AS row RETURN row",
        ] {
            let prepared =
                PreparedGraphSetText::prepare_with_parameter_types(text, &types, symbols).unwrap();
            assert!(
                prepared.bind_parameters(&small).is_ok(),
                "small control: {text}"
            );
            let error = prepared.bind_parameters(&arguments).unwrap_err();
            assert!(
                matches!(
                    error.kind,
                    GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::ScalarLiteral)
                ),
                "large operand: {text}: {error:?}"
            );
        }
    }

    #[test]
    fn list_expression_cap_is_exact_and_checked_independently_of_transport() {
        let value = |key_bytes| {
            vec![
                GraphValue::map(vec![(
                    "x".repeat(key_bytes).into(),
                    GraphValue::Scalar(CanonicalScalar::Null),
                )])
                .unwrap(),
            ]
        };
        let framing = crate::GqlListParameter::new(value(0))
            .unwrap()
            .canonical_bytes()
            .len();
        let plan = PreparedGraphSetText::prepare_with_parameter_types(
            "RETURN $rows AS value",
            &[("rows", GqlParameterType::List)],
            symbols,
        )
        .unwrap();
        for extra in [0, 1] {
            let argument = crate::GqlListParameter::new(value(
                crate::algebra::MAX_SCALAR_PREDICATE_BYTES - framing + extra,
            ))
            .unwrap();
            assert_eq!(
                argument.canonical_bytes().len(),
                crate::algebra::MAX_SCALAR_PREDICATE_BYTES + extra
            );
            let mut arguments = GqlParameters::new();
            arguments
                .insert("rows", GqlParameterValue::List(argument))
                .unwrap();
            assert_eq!(plan.bind_parameters(&arguments).is_ok(), extra == 0);
        }
    }

    #[test]
    fn metadata_pipeline_executes_filter_page_and_arithmetic_in_written_order() {
        let query = PreparedGraphSetText::prepare(
            "MATCH p = (a)-[:R]->{0,3}(b) WITH path_length(p) AS hops \
             WHERE hops > 0 ORDER BY hops DESC SKIP 1 LIMIT 1 RETURN hops + 10 AS score",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        // Inject the native source's single path-length column to isolate the
        // row stages from graph traversal. All post-source work stays governed.
        let mut calls = 0;
        let result = query
            .execute_governed(
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                |_, _| {
                    calls += 1;
                    Ok::<_, GqlQueryError<(), ()>>(GqlQueryExecution {
                        value: [0, 1, 3, 2]
                            .into_iter()
                            .map(|value| {
                                GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                                    CanonicalScalar::Int(value),
                                )])
                            })
                            .collect(),
                        rows: GqlExecutionStats {
                            snapshot_records: 4,
                            result_rows: 4,
                        },
                        evaluator: Default::default(),
                    })
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            result.value,
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::Int(12),
            )])]
        );
        assert_eq!(result.rows.result_rows, 1);
    }

    #[test]
    fn graph_identity_aliases_are_not_reclassified_as_scalars() {
        for text in [
            "MATCH p = (a)-[:R]->{1,2}(b) WITH p AS route RETURN route + 1 AS bad",
            "MATCH (a)-[e:R]->(b) WITH e AS edge RETURN edge + 1 AS bad",
            "MATCH p = (a)-[:R]->{1,2}(b) WITH nodes(p) AS vertices \
             RETURN vertices + 1 AS bad",
        ] {
            let mut calls = 0;
            let result = PreparedGraphSetText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            });
            assert!(result.is_err(), "{text}");
            assert_eq!(calls, 0, "{text}");
        }
    }
}
