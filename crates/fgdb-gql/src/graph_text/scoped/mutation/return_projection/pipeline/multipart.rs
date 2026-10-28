//! Parse a complete multi-part read with one lexer and parameter table.
//! WITH is a real relational boundary. A later MATCH becomes a new governed
//! graph leaf joined BEFORE its projection/filter/page, never an AST evaluator.
//! OPTIONAL continuations use the same left-join kernel: correlation is part
//! of matching, and only new bindings are null-extended. Carried values stay
//! on the left, including property reads through their pinned identities.

use super::*;
use crate::row_join::{RowJoinKind, RowJoinSpec};
use crate::set_text::TextToken;
use crate::set_text::multipart::{
    BoundContinuation, BoundContinuationOperation, BoundReadInput, RowPatternResult,
    has_continuation,
};

struct UnresolvedContinuation<'a> {
    input: UnresolvedGraphText<'a>,
    operation: UnresolvedContinuationOperation<'a>,
}

enum UnresolvedContinuationOperation<'a> {
    Match(RowJoinSpec),
    Pattern {
        pattern: Box<UnresolvedGraphText<'a>>,
        join: RowJoinSpec,
        result: RowPatternResult,
    },
    ExistsBody {
        body: Box<UnresolvedReadInput<'a>>,
        imports: Vec<usize>,
        negated: Option<bool>,
    },
    List {
        body: Box<UnresolvedReadInput<'a>>,
        imports: Vec<usize>,
        list_column: usize,
    },
    Apply {
        body: Box<UnresolvedReadInput<'a>>,
        imports: Vec<usize>,
        optional: bool,
    },
}

pub(crate) struct UnresolvedReadInput<'a> {
    first: UnresolvedGraphText<'a>,
    continuations: Vec<UnresolvedContinuation<'a>>,
}

impl<'a> UnresolvedReadInput<'a> {
    /// Every element property slot the read's parts recorded, including
    /// private graph-expression sources and nested Apply bodies. Write-chain
    /// admission must see named map reads before any write, even when their
    /// value templates are evaluated by another part after correlation.
    pub(in crate::graph_text) fn element_key_slots(&self) -> Vec<crate::set_text::ElementKeySlot> {
        let mut slots: Vec<_> = self
            .first
            .syntax
            .element_keys
            .iter()
            .map(|(_, slot)| slot.clone())
            .collect();
        for part in &self.continuations {
            slots.extend(
                part.input
                    .syntax
                    .element_keys
                    .iter()
                    .map(|(_, slot)| slot.clone()),
            );
            match &part.operation {
                UnresolvedContinuationOperation::Pattern { pattern, .. } => {
                    slots.extend(
                        pattern
                            .syntax
                            .element_keys
                            .iter()
                            .map(|(_, slot)| slot.clone()),
                    );
                }
                UnresolvedContinuationOperation::Apply { body, .. }
                | UnresolvedContinuationOperation::ExistsBody { body, .. }
                | UnresolvedContinuationOperation::List { body, .. } => {
                    slots.extend(body.element_key_slots());
                }
                UnresolvedContinuationOperation::Match(_) => {}
            }
        }
        slots
    }

    pub(in crate::graph_text) fn from_first(
        first: UnresolvedGraphText<'_>,
    ) -> UnresolvedReadInput<'_> {
        UnresolvedReadInput {
            first,
            continuations: Vec::new(),
        }
    }

    pub(in crate::graph_text) fn append_stages(&mut self, stages: Vec<ReadStageTemplate>) {
        self.continuations
            .last_mut()
            .map_or(&mut self.first, |part| &mut part.input)
            .pipeline
            .extend(stages);
    }

    /// The graph parts have already lent their syntax. Terminal expressions
    /// (a write tail's rows, an aggregate RETURN's keys and arguments) add
    /// property slots afterwards; resolve them with the final part so carried
    /// and optional element reads bind once with the template.
    pub(in crate::graph_text) fn append_element_keys(
        &mut self,
        keys: &mut Vec<(&'a str, crate::set_text::ElementKeySlot)>,
    ) {
        self.continuations
            .last_mut()
            .map_or(&mut self.first, |part| &mut part.input)
            .syntax
            .element_keys
            .append(keys);
    }

    pub(in crate::graph_text) fn share_parameters(
        &mut self,
        parameters: &[GqlParameterSpec],
        offsets: &[usize],
    ) {
        self.first.syntax.parameters = parameters.to_vec();
        self.first.syntax.parameter_offsets = offsets.to_vec();
        for part in &mut self.continuations {
            part.input.syntax.parameters = parameters.to_vec();
            part.input.syntax.parameter_offsets = offsets.to_vec();
            if let UnresolvedContinuationOperation::Apply { body, .. }
            | UnresolvedContinuationOperation::ExistsBody { body, .. }
            | UnresolvedContinuationOperation::List { body, .. } = &mut part.operation
            {
                body.share_parameters(parameters, offsets);
            }
            if let UnresolvedContinuationOperation::Pattern { pattern, .. } = &mut part.operation {
                pattern.syntax.parameters = parameters.to_vec();
                pattern.syntax.parameter_offsets = offsets.to_vec();
            }
        }
    }
    pub(crate) fn column_schema(&self) -> (Vec<String>, Vec<GraphSetColumnType>) {
        self.continuations
            .last()
            .map_or(&self.first, |part| &part.input)
            .column_schema()
    }

    pub(crate) fn depth(&self) -> usize {
        // Each continuation contributes one Join plus its post-join stages.
        // Its hidden source is a depth-one graph leaf, not a second pipeline.
        self.first.depth()
            + self
                .continuations
                .iter()
                .map(|part| {
                    let source_depth = match &part.operation {
                        UnresolvedContinuationOperation::Match(_) => 1,
                        // Arguments + graph join + witness projection, then
                        // Apply + filter + ordered removal of the witness.
                        UnresolvedContinuationOperation::Pattern { result, .. } => match result {
                            RowPatternResult::Filter { .. } => 6,
                            RowPatternResult::Exists
                            | RowPatternResult::Count
                            | RowPatternResult::Collect { .. } => 4,
                        },
                        UnresolvedContinuationOperation::ExistsBody { body, negated, .. } => {
                            body.depth() + if negated.is_some() { 4 } else { 2 }
                        }
                        UnresolvedContinuationOperation::Apply { body, .. }
                        | UnresolvedContinuationOperation::List { body, .. } => 1 + body.depth(),
                    };
                    source_depth
                        + part
                            .input
                            .leading
                            .iter()
                            .map(ReadStageTemplate::depth)
                            .sum::<usize>()
                        + usize::from(part.input.projection.is_some())
                        + 2 * usize::from(!part.input.row_fields.is_empty())
                        + part
                            .input
                            .pipeline
                            .iter()
                            .map(ReadStageTemplate::depth)
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    pub(crate) fn operand_count(&self) -> usize {
        usize::from(!self.first.singleton)
            + self
                .continuations
                .iter()
                .map(|part| match &part.operation {
                    UnresolvedContinuationOperation::Match(_) => 1,
                    UnresolvedContinuationOperation::Pattern { .. } => 1,
                    UnresolvedContinuationOperation::Apply { body, .. }
                    | UnresolvedContinuationOperation::ExistsBody { body, .. }
                    | UnresolvedContinuationOperation::List { body, .. } => body.operand_count(),
                })
                .sum::<usize>()
    }

    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.first.parameter_schema()
    }

    pub(crate) fn parameter_offsets(&self) -> &[usize] {
        self.first.parameter_offsets()
    }

    /// Every part reads the lender's reverse catalog for `labels(n)` and
    /// `type(r)`, not only the names its own text mentions.
    pub(crate) fn resolve<R: crate::graph_text::GraphSymbolResolver + ?Sized>(
        self,
        resolve: &mut R,
    ) -> Result<BoundReadInput, GraphPatternTextError> {
        let first = self
            .first
            .resolve(crate::graph_text::LentResolver(resolve))?;
        let mut continuations = Vec::new();
        for part in self.continuations {
            continuations.push(BoundContinuation {
                input: part
                    .input
                    .resolve(crate::graph_text::LentResolver(resolve))?,
                operation: match part.operation {
                    UnresolvedContinuationOperation::Match(join) => {
                        BoundContinuationOperation::Match(join)
                    }
                    UnresolvedContinuationOperation::Pattern {
                        pattern,
                        join,
                        result,
                    } => BoundContinuationOperation::Pattern {
                        pattern: Box::new(
                            pattern.resolve(crate::graph_text::LentResolver(resolve))?,
                        ),
                        join,
                        result,
                    },
                    UnresolvedContinuationOperation::Apply {
                        body,
                        imports,
                        optional,
                    } => BoundContinuationOperation::Apply {
                        body: Box::new(body.resolve(resolve)?),
                        imports,
                        optional,
                    },
                    UnresolvedContinuationOperation::ExistsBody {
                        body,
                        imports,
                        negated,
                    } => BoundContinuationOperation::ExistsBody {
                        body: Box::new(body.resolve(resolve)?),
                        imports,
                        negated,
                    },
                    UnresolvedContinuationOperation::List {
                        body,
                        imports,
                        list_column,
                    } => BoundContinuationOperation::List {
                        body: Box::new(body.resolve(resolve)?),
                        imports,
                        list_column,
                    },
                },
            });
        }
        Ok(BoundReadInput {
            first,
            continuations,
        })
    }
}

/// Whether completed row clauses contain graph expressions that need an
/// Apply owner. Read and aggregate facades share this native-lexer probe;
/// ordinary aggregate calls and list-local pattern expressions keep their
/// existing parser owners.
pub(in crate::graph_text) fn has_row_graph_expressions(
    statement: &str,
    tokens: &[TextToken<'_>],
) -> Result<bool, GraphSetTextError> {
    let mut list_probe = Lexer {
        text: statement,
        at: 0,
        tokens: 0,
    };
    // A graph-only projection keeps its stronger graph owner, including
    // aggregate arguments. Broader list routing begins at a real row boundary.
    let first = list_probe.clone().next()?;
    let mut row_scope = !matches!(first.kind, TokenKind::Word(word)
        if word.eq_ignore_ascii_case("MATCH") || word.eq_ignore_ascii_case("OPTIONAL"));
    let mut depth = 0_usize;
    let mut previous = None;
    loop {
        let token = list_probe.next()?;
        if matches!(token.kind, TokenKind::End) {
            break;
        }
        let key = matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
            || matches!(previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
        if depth == 0
            && !key
            && matches!(token.kind, TokenKind::Word(word)
                if word.eq_ignore_ascii_case("UNWIND")
                    || word.eq_ignore_ascii_case("CALL")
                    || (word.eq_ignore_ascii_case("WITH")
                        && !matches!(previous, Some(TokenKind::Word(word))
                            if word.eq_ignore_ascii_case("STARTS")
                                || word.eq_ignore_ascii_case("ENDS"))))
        {
            row_scope = true;
        }
        let graph_read = if row_scope {
            row_list_graph_expression_follows(token.kind, &list_probe, |_| false)?
        } else {
            list_graph_expression_follows(token.kind, &list_probe)?
        };
        if graph_read {
            return Ok(true);
        }
        match token.kind {
            TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ => {}
        }
        previous = Some(token.kind);
    }
    for at in crate::set_text::multipart::row_existence_offsets(tokens) {
        // Compound leaves borrow a slice of the statement but retain the
        // original composition-token offsets.
        let at = at - tokens.first().map_or(0, |token| token.at);
        let mut lexer = Lexer {
            text: statement,
            at,
            tokens: 0,
        };
        lexer.next()?;
        if row_pattern_filter_starts(&mut lexer.clone(), |_| false)?
            || !row_pattern_expressions(&mut lexer, |_| false)?.is_empty()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl PreparedGraphText {
    pub(crate) fn unresolved_read_input<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        tokens: &[TextToken<'_>],
        zones: Option<&crate::GqlTimeZones>,
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        let row_graph_filter = has_row_graph_expressions(statement, tokens)?;
        let multipart = has_continuation(tokens)
            || row_graph_filter
            || Parser::deferred_where_continues(statement, declarations, tokens, zones);
        let read = |deferred: &[usize],
                    implicit: &[usize]|
         -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
            if !multipart && implicit.is_empty() {
                return Ok(UnresolvedReadInput {
                    first: Self::unresolved_for_composition(
                        statement,
                        declarations,
                        zones,
                        deferred,
                    )?,
                    continuations: Vec::new(),
                });
            }
            let mut parser = Parser::new_with_parameter_types(statement, declarations)?
                .with_zones(zones.cloned());
            parser.deferred_wheres = deferred.to_vec();
            parser.implicit_withs = implicit.to_vec();
            parser.read_elements = true;
            parser.multipart_read(statement)
        };
        let error = match read(&[], &[]) {
            Err(error) => error,
            read => return read,
        };
        // A required MATCH's WHERE over imported rows that the graph predicate
        // refused, such as a computed graph operand against an imported
        // column (`WITH 0 AS e MATCH (n) WHERE toInteger(n.id) = e`, or the
        // same after a leading UNWIND), means what `MATCH ... WITH * WHERE ...`
        // means: the same rows, filtered after the join. So does a required
        // MATCH's WHERE whose existence predicate the graph predicate cannot
        // place (`MATCH (n) WHERE (n)-[:A]-() OR (n)-[:B]-()`): the row filter
        // reads each one from a hidden column (hoist_existence_reads). And a
        // later MATCH clause that reads a name its child scope cannot see, an earlier
        // clause's relationship (`MATCH (a)-[r]->(b) MATCH (b)-[s]->(c)
        // WHERE r.w < s.w`) or an imported column, means what it means after
        // an explicit `WITH *`: openCypher keeps relationship uniqueness per
        // clause, so the boundary changes no row. Failing both, an OPTIONAL
        // continuation's refused WHERE filters inside an optional Apply,
        // before the clause's null extension (multipart_optional_apply).
        // Re-read one refusal at a time. A statement still refused reports
        // its first error.
        let (mut deferred, mut implicit) = (Vec::new(), Vec::new());
        let mut offset = error.offset;
        loop {
            if let Some(at) = crate::set_text::multipart::continuation_where_at(tokens, offset)
                .or_else(|| crate::set_text::multipart::existence_where_at(tokens, offset))
                .filter(|at| !deferred.contains(at))
            {
                deferred.push(at);
            } else if let Some(at) = crate::set_text::multipart::implicit_with_at(tokens, offset)
                .filter(|at| !implicit.contains(at))
            {
                implicit.push(at);
            } else if let Some(at) = crate::set_text::multipart::optional_where_at(tokens, offset)
                .filter(|at| !deferred.contains(at))
            {
                deferred.push(at);
            } else {
                break;
            }
            match read(&deferred, &implicit) {
                Ok(read) => return Ok(read),
                Err(next) => offset = next.offset,
            }
        }
        Err(error)
    }
}

// Validate expression-column domains before the first catalog callback. These
// typed NULL/empty-list witnesses only compile shape; no expression executes.
fn admit_head(
    head: &GraphProjectionHead<'_>,
    types: &[GraphSetColumnType],
    parameters: &[GqlParameterSpec],
) -> Result<(), GraphSetTextError> {
    admit_values(
        head.outputs.iter().map(|(name, value)| (name.at, value)),
        types,
        parameters,
    )
}

fn admit_values<'a>(
    outputs: impl IntoIterator<Item = (usize, &'a ReadValueTemplate)>,
    types: &[GraphSetColumnType],
    parameters: &[GqlParameterSpec],
) -> Result<(), GraphSetTextError> {
    let values: Vec<_> = parameters
        .iter()
        .map(|spec| match spec.parameter_type {
            GqlParameterType::Int64 => GqlParameterValue::Int64(0),
            GqlParameterType::UInt64 => GqlParameterValue::UInt64(0),
            GqlParameterType::Scalar(_) => GqlParameterValue::Scalar(
                GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
            ),
            GqlParameterType::List => GqlParameterValue::List(
                crate::GqlListParameter::new(Vec::new()).expect("bounded empty list"),
            ),
            GqlParameterType::Map => GqlParameterValue::Map(
                crate::GqlMapParameter::new(Vec::new()).expect("bounded empty map"),
            ),
        })
        .collect();
    for (column, (at, value)) in outputs.into_iter().enumerate() {
        let value = super::super::bind_read_value(value, &values)?;
        GraphSetProjection::admit_output(&value, types, column).map_err(|kind| {
            GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::ProjectionBuild(kind),
            }
        })?;
    }
    Ok(())
}

fn join_spec(
    left: &[GraphSetColumnType],
    inputs: &[Projection<'_>],
    keys: &[(usize, usize)],
    comparisons: &[(usize, usize, IntegerComparison)],
    at: usize,
) -> Result<RowJoinSpec, GraphSetTextError> {
    let right: Vec<_> = inputs.iter().map(projection_type).collect();
    let typed_keys = !keys.is_empty()
        && keys.iter().all(|&(l, r)| {
            left[l] == right[r]
                && (left[l].is_scalar_like()
                    || matches!(
                        left[l],
                        GraphSetColumnType::Vertex | GraphSetColumnType::Edge
                    ))
        });
    let spec = if typed_keys {
        RowJoinSpec::new(left, &right, keys)
    } else {
        RowJoinSpec::cross(left, &right).and_then(|spec| {
            if keys.is_empty() {
                return Ok(spec);
            }
            // UNWIND has the Any domain. The checked ordinary comparison
            // predicate handles it without coercion or a guessed scalar type.
            let mut code = Vec::new();
            for &(l, r) in keys {
                code.push(GraphSetPredicateOp::Compare {
                    left: GraphSetOperand::Column(l),
                    comparison: IntegerComparison::Equal,
                    right: GraphSetOperand::Column(left.len() + r),
                });
                if code.len() > 1 {
                    code.push(GraphSetPredicateOp::And);
                }
            }
            spec.with_predicate(&code)
        })
    };
    let spec = spec.map_err(|_| expected(at, "bounded, type-compatible MATCH correlations"))?;
    if comparisons.is_empty() {
        return Ok(spec);
    }
    // Keep typed equality keys selective. An Any-domain equality may already
    // be ON bytecode; append to it, never overwrite it with the theta test.
    let mut code = spec.predicate().unwrap_or_default().to_vec();
    for &(l, r, comparison) in comparisons {
        let conjunction = !code.is_empty();
        code.push(GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(l),
            comparison,
            right: GraphSetOperand::Column(left.len() + r),
        });
        if conjunction {
            code.push(GraphSetPredicateOp::And);
        }
    }
    spec.with_predicate(&code)
        .map_err(|_| expected(at, "bounded, type-compatible MATCH comparisons"))
}

fn row_pattern_filter_starts(
    lexer: &mut Lexer<'_>,
    mut hoisted: impl FnMut(usize) -> bool,
) -> Result<bool, GraphPatternTextError> {
    let mut token = lexer.next()?;
    let mut wrappers = 0_usize;
    for _ in 0..MAX_FILTER_NESTING {
        if hoisted(token.at) {
            return Ok(false);
        }
        match token.kind {
            TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT") => {
                token = lexer.next()?;
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS") => {
                if !crate::graph_text::scoped::exists_introducer_follows(&mut lexer.clone())? {
                    return Ok(false);
                }
                let opening = lexer.next()?;
                let TokenKind::Punct(opening @ (b'(' | b'{')) = opening.kind else {
                    return Ok(false);
                };
                let Some(after) = after_balanced(lexer, opening)? else {
                    return Ok(false);
                };
                return row_pattern_ends(lexer, after, wrappers);
            }
            TokenKind::Punct(b'(') => {
                if crate::graph_text::scoped::pattern_predicate_follows(
                    token.kind,
                    &mut lexer.clone(),
                )? {
                    let Some((after, explicit_relationship)) = after_row_pattern(lexer)? else {
                        return Ok(false);
                    };
                    // `(x)<--(y)` also spells the valid scalar comparison
                    // `(x) < - -(y)`. Unhoisted bare row predicates therefore
                    // require relationship brackets; EXISTS has no ambiguity.
                    if !explicit_relationship {
                        return Ok(false);
                    }
                    return row_pattern_ends(lexer, after, wrappers);
                }
                wrappers += 1;
                token = lexer.next()?;
            }
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// Look ahead through one row WHERE or RETURN using the native lexer. Pattern bodies
/// are consumed whole, so their local vertices cannot become additional
/// expressions. Per-element list scopes retain their existing typed owner.
fn list_comprehension_follows(
    token: TokenKind<'_>,
    lexer: &Lexer<'_>,
) -> Result<bool, GraphPatternTextError> {
    let mut lexer = lexer.clone();
    Ok(matches!(token, TokenKind::Punct(b'['))
        && matches!(lexer.next()?.kind, TokenKind::Word(_))
        && matches!(lexer.next()?.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")))
}

fn row_list_quantifier_follows(
    token: TokenKind<'_>,
    lexer: &Lexer<'_>,
) -> Result<Option<crate::GraphListQuantifier>, GraphPatternTextError> {
    let TokenKind::Word(word) = token else {
        return Ok(None);
    };
    use crate::GraphListQuantifier as Quantifier;
    let Some(kind) = [
        ("ANY", Quantifier::Any),
        ("ALL", Quantifier::All),
        ("NONE", Quantifier::None),
        ("SINGLE", Quantifier::Single),
    ]
    .into_iter()
    .find_map(|(name, kind)| word.eq_ignore_ascii_case(name).then_some(kind)) else {
        return Ok(None);
    };
    let mut lexer = lexer.clone();
    Ok((matches!(lexer.next()?.kind, TokenKind::Punct(b'('))
        && matches!(lexer.next()?.kind, TokenKind::Word(_))
        && matches!(lexer.next()?.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")))
    .then_some(kind))
}

/// A graph head defers only reads of an active list local. Independent
/// reads retain its existing owner, including nested graph ancestors.
fn list_graph_expression_follows<'a>(
    token: TokenKind<'a>,
    lexer: &Lexer<'a>,
) -> Result<bool, GraphPatternTextError> {
    list_graph_expression_in_scope(token, lexer, false, |_| false)
}

/// A completed row can own any graph-reading list. An expression already
/// supplied by the graph head keeps that owner and its exact source span.
fn row_list_graph_expression_follows<'a>(
    token: TokenKind<'a>,
    lexer: &Lexer<'a>,
    hoisted: impl FnMut(usize) -> bool,
) -> Result<bool, GraphPatternTextError> {
    list_graph_expression_in_scope(token, lexer, true, hoisted)
}

fn list_graph_expression_in_scope<'a>(
    token: TokenKind<'a>,
    lexer: &Lexer<'a>,
    row: bool,
    mut hoisted: impl FnMut(usize) -> bool,
) -> Result<bool, GraphPatternTextError> {
    if !list_comprehension_follows(token, lexer)?
        && row_list_quantifier_follows(token, lexer)?.is_none()
    {
        return Ok(false);
    }
    let mut lexer = lexer.clone();
    let mut depth = 0_usize;
    let mut previous = None;
    let mut locals = ReadLocals::default();
    let mut current = Token {
        kind: token,
        at: lexer.at.saturating_sub(1),
    };
    loop {
        let key = matches!(previous, Some(TokenKind::Punct(b'.' | b':')));
        let mut ahead = lexer.clone();
        let end = if super::super::pattern_comprehension_follows(current.kind, &lexer)? {
            after_balanced(&mut ahead, b'[')?
        } else if !key
            && matches!(current.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("COUNT"))
            && matches!(ahead.clone().next()?.kind, TokenKind::Punct(b'{'))
        {
            ahead.next()?;
            after_balanced(&mut ahead, b'{')?
        } else if !key
            && matches!(current.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS"))
            && crate::graph_text::scoped::exists_introducer_follows(&mut ahead.clone())?
        {
            let opening = ahead.next()?;
            match opening.kind {
                TokenKind::Punct(opening @ (b'(' | b'{')) => after_balanced(&mut ahead, opening)?,
                _ => None,
            }
        } else if matches!(current.kind, TokenKind::Punct(b'('))
            && crate::graph_text::scoped::pattern_predicate_follows(
                current.kind,
                &mut ahead.clone(),
            )?
        {
            after_row_pattern(&mut ahead)?.map(|(after, _)| after)
        } else {
            None
        };
        if let Some(end) = end {
            if hoisted(current.at) {
                lexer = ahead;
                previous = Some(TokenKind::Punct(b')'));
                current = end;
                continue;
            }
            if row {
                return Ok(true);
            }
            let mut scan = lexer.clone();
            let mut read = current;
            let mut prior = None;
            let mut nested = ReadLocals::default();
            let mut nested_depth = 0_usize;
            while read.at < end.at {
                let declaration = nested.observe(read, prior, &scan, nested_depth)?;
                if let TokenKind::Word(word) = read.kind
                    && !declaration
                    && !matches!(prior, Some(TokenKind::Punct(b'.' | b':')))
                    && !matches!(prior, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"))
                    && !(matches!(prior, Some(TokenKind::Punct(b'{' | b',')))
                        && matches!(scan.clone().next()?.kind, TokenKind::Punct(b':')))
                    && locals.shadows(word)
                    && !nested.shadows(word)
                {
                    return Ok(true);
                }
                match read.kind {
                    TokenKind::Punct(b'(' | b'[' | b'{') => nested_depth += 1,
                    TokenKind::Punct(b')' | b']' | b'}') => {
                        nested_depth = nested_depth.saturating_sub(1)
                    }
                    TokenKind::End => break,
                    _ => {}
                }
                prior = Some(read.kind);
                read = scan.next()?;
            }
        }
        locals.observe(current, previous, &lexer, depth)?;
        match current.kind {
            TokenKind::Punct(b'(' | b'[' | b'{') => {
                depth += 1;
                if depth > MAX_FILTER_NESTING {
                    return Ok(false);
                }
            }
            TokenKind::Punct(b')' | b']' | b'}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(false);
                }
            }
            TokenKind::End => return Ok(false),
            _ => {}
        }
        previous = Some(current.kind);
        current = lexer.next()?;
    }
}

fn row_pattern_expressions<'a>(
    lexer: &mut Lexer<'a>,
    mut hoisted: impl FnMut(usize) -> bool,
) -> Result<Vec<(Token<'a>, Lexer<'a>)>, GraphPatternTextError> {
    let token = lexer.next()?;
    row_pattern_expressions_from(token, lexer, &mut hoisted, false)
}

fn row_pattern_expressions_from<'a>(
    mut token: Token<'a>,
    lexer: &mut Lexer<'a>,
    mut hoisted: impl FnMut(usize) -> bool,
    source: bool,
) -> Result<Vec<(Token<'a>, Lexer<'a>)>, GraphPatternTextError> {
    let mut expressions = Vec::new();
    let mut previous = None;
    let mut depth = 0_usize;
    let mut locals = ReadLocals::default();
    loop {
        let key = matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
            || matches!(previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
        let group_by = depth == 0
            && !key
            && matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("GROUP"))
            && matches!(lexer.clone().next()?.kind,
                TokenKind::Word(word) if word.eq_ignore_ascii_case("BY"));
        if matches!(token.kind, TokenKind::End)
            || (depth == 0 && matches!(token.kind, TokenKind::Punct(b')' | b'}' | b']' | b'|')))
            || group_by
            || (source
                && depth == 0
                && !key
                && matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("WHERE")))
            || matches!(token.kind, TokenKind::Word(word)
                if depth == 0 && !key && Parser::ends_where(word, previous))
        {
            return Ok(expressions);
        }
        // Gather the complete quantifier, retaining its local scope and UNKNOWNs.
        if !key
            && locals.scopes.is_empty()
            && !hoisted(token.at)
            && row_list_quantifier_follows(token.kind, lexer)?.is_some()
            && row_list_graph_expression_follows(token.kind, lexer, &mut hoisted)?
        {
            expressions.push((token, lexer.clone()));
            let mut ahead = lexer.clone();
            ahead.next()?; // opening parenthesis
            let Some(after) = after_balanced(&mut ahead, b'(')? else {
                return Ok(expressions);
            };
            *lexer = ahead;
            previous = Some(TokenKind::Punct(b')'));
            token = after;
            continue;
        }
        // A complete graph comprehension belongs to this row. Its own body
        // stays inside the original native Collect scope. Ordinary lists and
        // per-element list scopes keep their existing owner and are skipped.
        if matches!(token.kind, TokenKind::Punct(b'[')) {
            let graph = super::super::pattern_comprehension_follows(token.kind, lexer)?
                || row_list_graph_expression_follows(token.kind, lexer, &mut hoisted)?;
            if locals.scopes.is_empty() && !hoisted(token.at) && graph {
                expressions.push((token, lexer.clone()));
            }
            // A source may wrap its graph value in an ordinary list literal.
            // Traverse that container; complete graph/list scopes stay private.
            if !source || graph || list_comprehension_follows(token.kind, lexer)? {
                let Some(after) = after_balanced(lexer, b'[')? else {
                    return Ok(expressions);
                };
                previous = Some(TokenKind::Punct(b']'));
                token = after;
                continue;
            }
        }
        let mut ahead = lexer.clone();
        let after = if matches!(token.kind, TokenKind::Word(word)
            if !key && word.eq_ignore_ascii_case("COUNT"))
            && matches!(ahead.clone().next()?.kind, TokenKind::Punct(b'{'))
        {
            ahead.next()?;
            after_balanced(&mut ahead, b'{')?
        } else if matches!(token.kind, TokenKind::Word(word)
            if !key && word.eq_ignore_ascii_case("EXISTS"))
            && crate::graph_text::scoped::exists_introducer_follows(&mut ahead.clone())?
        {
            let opening = ahead.next()?;
            match opening.kind {
                TokenKind::Punct(opening @ (b'(' | b'{')) => after_balanced(&mut ahead, opening)?,
                _ => None,
            }
        } else if matches!(token.kind, TokenKind::Punct(b'('))
            && crate::graph_text::scoped::pattern_predicate_follows(token.kind, &mut ahead.clone())?
        {
            after_row_pattern(&mut ahead)?.and_then(|(after, explicit)| explicit.then_some(after))
        } else {
            None
        };
        if let Some(after) = after {
            if locals.scopes.is_empty() && !hoisted(token.at) {
                expressions.push((token, lexer.clone()));
            }
            *lexer = ahead;
            token = after;
            previous = Some(TokenKind::Punct(b')'));
            continue;
        }
        locals.observe(token, previous, lexer, depth)?;
        match token.kind {
            TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ => {}
        }
        previous = Some(token.kind);
        token = lexer.next()?;
    }
}

/// Consume one balanced token group, returning its first following token.
/// This is a conservative routing probe: the normal parser still owns every
/// symbol, bound, property expression and actual pattern admission.
fn after_balanced<'a>(
    lexer: &mut Lexer<'a>,
    opening: u8,
) -> Result<Option<Token<'a>>, GraphPatternTextError> {
    let close = |opening| match opening {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    };
    let mut stack = vec![close(opening)];
    loop {
        let token = lexer.next()?;
        match token.kind {
            TokenKind::Punct(opening @ (b'(' | b'[' | b'{')) => {
                if stack.len() == MAX_FILTER_NESTING {
                    return Ok(None);
                }
                stack.push(close(opening));
            }
            TokenKind::Punct(closing @ (b')' | b']' | b'}')) => {
                if stack.pop() != Some(closing) {
                    return Ok(None);
                }
                if stack.is_empty() {
                    return Ok(Some(lexer.next()?));
                }
            }
            TokenKind::End => return Ok(None),
            _ => {}
        }
    }
}

fn after_row_pattern<'a>(
    lexer: &mut Lexer<'a>,
) -> Result<Option<(Token<'a>, bool)>, GraphPatternTextError> {
    // The current token is the first node's opening parenthesis. Requiring a
    // complete node after every edge AND a clause boundary after the pattern
    // distinguishes `(x)-->()` from `(x) - -1` and `(x) - -(y) > 2`.
    let Some(mut token) = after_balanced(lexer, b'(')? else {
        return Ok(None);
    };
    let mut explicit_relationship = false;
    while matches!(token.kind, TokenKind::Punct(b'-' | b'<')) {
        loop {
            token = match token.kind {
                // `<--` also spells `< - -`, a comparison with a double
                // negation; `>` never starts an operand, so it is explicit.
                TokenKind::Punct(b'-' | b'<') => lexer.next()?,
                TokenKind::Punct(b'>') => {
                    explicit_relationship = true;
                    lexer.next()?
                }
                TokenKind::Punct(b'[') => {
                    explicit_relationship = true;
                    let Some(after) = after_balanced(lexer, b'[')? else {
                        return Ok(None);
                    };
                    after
                }
                _ => break,
            };
        }
        if !matches!(token.kind, TokenKind::Punct(b'(')) {
            return Ok(None);
        }
        let Some(after) = after_balanced(lexer, b'(')? else {
            return Ok(None);
        };
        token = after;
    }
    Ok(Some((token, explicit_relationship)))
}

fn row_pattern_ends<'a>(
    lexer: &mut Lexer<'a>,
    mut token: Token<'a>,
    wrappers: usize,
) -> Result<bool, GraphPatternTextError> {
    for _ in 0..wrappers {
        if !matches!(token.kind, TokenKind::Punct(b')')) {
            return Ok(false);
        }
        token = lexer.next()?;
    }
    Ok(matches!(token.kind, TokenKind::End)
        || matches!(token.kind, TokenKind::Word(word)
            if ["WITH", "RETURN", "MATCH", "OPTIONAL", "UNWIND", "CALL", "ORDER", "SKIP", "OFFSET", "LIMIT"]
                .iter().any(|clause| word.eq_ignore_ascii_case(clause))))
}

/// A brace body containing relational clauses belongs to the returning read
/// compiler. Pattern-only bodies keep their established native scope owner.
pub(super) fn returning_existence_body(
    token: TokenKind<'_>,
    lexer: &Lexer<'_>,
) -> Result<bool, GraphPatternTextError> {
    if !matches!(token, TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS")) {
        return Ok(false);
    }
    let mut lexer = lexer.clone();
    if !matches!(lexer.next()?.kind, TokenKind::Punct(b'{')) {
        return Ok(false);
    }
    let mut depth = 0_usize;
    let mut previous = None;
    loop {
        let token = lexer.next()?;
        match token.kind {
            TokenKind::End => return Ok(false),
            TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => {
                let Some(outer) = depth.checked_sub(1) else {
                    return Ok(false);
                };
                depth = outer;
            }
            TokenKind::Word(word)
                if depth == 0
                    && !matches!(previous, Some(TokenKind::Punct(b'.')))
                    && !matches!(previous, Some(TokenKind::Word(name)) if name.eq_ignore_ascii_case("AS"))
                    && (word.eq_ignore_ascii_case("RETURN")
                        || (word.eq_ignore_ascii_case("WITH")
                            && !matches!(previous, Some(TokenKind::Word(name))
                                if name.eq_ignore_ascii_case("STARTS") || name.eq_ignore_ascii_case("ENDS")))) =>
            {
                return Ok(true);
            }
            _ => {}
        }
        previous = Some(token.kind);
    }
}

impl<'a> Parser<'a> {
    /// An unhoisted graph test of a completed row belongs to the multipart
    /// graph owner. Existing graph-head reads retain their hidden columns.
    pub(super) fn row_existence_filter(&self, width: usize) -> Result<bool, GraphPatternTextError> {
        if !self.read_elements || !self.is_word("WHERE") {
            return Ok(false);
        }
        Ok(self.whole_row_existence_filter(width)?
            || !row_pattern_expressions(&mut self.lexer.clone(), |at| {
                hoisted_span(self.existence_reads.as_ref(), width, at).is_some()
                    || hoisted_span(self.comprehension_reads.as_ref(), width, at).is_some()
            })?
            .is_empty())
    }

    /// An unhoisted graph value after UNWIND or a later WITH needs the same
    /// Apply owner as a row WHERE. An aggregating RETURN reads its gathered
    /// scalar values through the terminal aggregate compiler.
    fn row_list_projection(&self) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("WITH") && !self.is_word("RETURN") {
            return Ok(false);
        }
        for (token, lexer) in row_pattern_expressions(&mut self.lexer.clone(), |_| false)? {
            if list_graph_expression_follows(token.kind, &lexer)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn row_existence_projection(
        &self,
        width: usize,
    ) -> Result<bool, GraphPatternTextError> {
        if !self.read_elements || !(self.is_word("RETURN") || self.is_word("WITH")) {
            return Ok(false);
        }
        let hoisted = |at| {
            hoisted_span(self.existence_reads.as_ref(), width, at).is_some()
                || hoisted_span(self.comprehension_reads.as_ref(), width, at).is_some()
        };
        let expressions = row_pattern_expressions(&mut self.lexer.clone(), hoisted)?;
        if self.is_word("WITH") {
            for (token, lexer) in expressions {
                if row_list_graph_expression_follows(token.kind, &lexer, hoisted)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        Ok(!expressions.is_empty())
    }

    fn returning_existence_projection(&self) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("RETURN") {
            return Ok(false);
        }
        for (token, lexer) in row_pattern_expressions(&mut self.lexer.clone(), |_| false)? {
            if returning_existence_body(token.kind, &lexer)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn whole_row_existence_filter(&self, width: usize) -> Result<bool, GraphPatternTextError> {
        row_pattern_filter_starts(&mut self.lexer.clone(), |at| {
            hoisted_span(self.existence_reads.as_ref(), width, at).is_some()
                || hoisted_span(self.comprehension_reads.as_ref(), width, at).is_some()
        })
    }

    /// Lower a complete row WHERE to an optional Apply with one non-NULL
    /// witness column. Only identity-correlated positive patterns are admitted;
    /// user row expressions and enclosing comprehension locals stay with their
    /// existing owners. The body never exposes its graph bindings to the tail.
    fn multipart_existence_part(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        aggregate: bool,
        call_depth: usize,
    ) -> Result<(UnresolvedContinuation<'a>, RowSchema<'a>, bool), GraphSetTextError> {
        let at = self.current.at;
        self.word("WHERE")?;
        let mut negated = false;
        let mut wrappers = 0_usize;
        let mut prefix = 0_usize;
        loop {
            prefix += 1;
            if prefix > MAX_FILTER_NESTING {
                return Err(expected(at, "a bounded row-scope existence predicate"));
            }
            if self.take_word("NOT")? {
                negated = !negated;
            } else if self.is_word("EXISTS")
                || crate::graph_text::scoped::pattern_predicate_follows(
                    self.current.kind,
                    &mut self.lexer.clone(),
                )?
            {
                break;
            } else {
                self.punct(b'(', "a whole pattern existence predicate after WHERE")?;
                wrappers += 1;
            }
        }
        let operation = self.multipart_existence_operation(
            statement,
            incoming,
            self.visible_width(incoming),
            call_depth,
            RowPatternResult::Filter { negated },
        )?;
        for _ in 0..wrappers {
            self.punct(b')', ") after a row-scope existence predicate")?;
        }
        if !matches!(self.current.kind, TokenKind::End)
            && ![
                "WITH", "RETURN", "MATCH", "OPTIONAL", "UNWIND", "CALL", "ORDER", "SKIP", "OFFSET",
                "LIMIT",
            ]
            .iter()
            .any(|clause| self.is_word(clause))
        {
            return Err(expected(
                self.current.at,
                "a whole pattern existence predicate before the next clause",
            ));
        }
        let types: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();

        let projection = incoming
            .iter()
            .enumerate()
            .map(|(column, (name, _))| ReadProjectionTemplate {
                name: name.text.to_owned(),
                value: ReadValueTemplate::Column(column),
            })
            .collect();
        let (pipeline, next, terminal) = self.multipart_row_tail(incoming.clone(), aggregate)?;
        let mut syntax = self.take_part_syntax()?;
        syntax.return_at = at;
        Ok((
            UnresolvedContinuation {
                input: UnresolvedGraphText {
                    statement,
                    syntax,
                    projection: Some(projection),
                    pipeline,
                    singleton: true,
                    leading: Vec::new(),
                    leading_types: types,
                    correlations: Vec::new(),
                    row_fields: Vec::new(),
                },
                operation,
            },
            next,
            terminal,
        ))
    }

    /// Append private Boolean witnesses or counts for a row WHERE or RETURN. The
    /// ordinary scalar/value evaluator owns Boolean precedence, three-valued
    /// logic and lazy CASE; graph sources retain the existing eager governed
    /// Apply contract. A public projection never exports the private suffix.
    #[allow(clippy::type_complexity)]
    fn multipart_existence_expression(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        aggregate: bool,
        call_depth: usize,
    ) -> Result<(Vec<UnresolvedContinuation<'a>>, RowSchema<'a>, bool), GraphSetTextError> {
        let at = self.current.at;
        let returning = self.is_word("RETURN");
        let with_projection = self.is_word("WITH");
        let projecting = returning || with_projection;
        let width = incoming.len();
        let visible = self.visible_width(incoming);
        let expressions = row_pattern_expressions(&mut self.lexer.clone(), |at| {
            hoisted_span(self.existence_reads.as_ref(), width, at).is_some()
                || hoisted_span(self.comprehension_reads.as_ref(), width, at).is_some()
        })?;
        let saved = (self.lexer.clone(), self.current);
        let (mut parts, schema, spans, counts) = self.gather_row_expression_sources(
            statement,
            incoming,
            visible,
            call_depth,
            expressions,
        )?;
        let expanded = schema.len();
        let had_boundary = self
            .boundary_reads
            .as_ref()
            .is_some_and(|b| b.width == width);
        let detached_boundary = if had_boundary {
            self.boundary_reads
                .as_mut()
                .expect("matching boundary")
                .width = expanded;
            None
        } else {
            let previous = self.boundary_reads.take();
            self.boundary_reads = Some(BoundaryReads {
                visible,
                width: expanded,
                reads: Vec::new(),
                functions: Vec::new(),
                grouped: Vec::new(),
                scoped: Vec::new(),
            });
            previous
        };
        // Existing reads still address their original prefix. Only their
        // exact row-width guard changes while the private suffix is present.
        let previous_existence = self.existence_reads.take();
        let mut combined = previous_existence
            .as_ref()
            .filter(|(guard, _)| *guard == width)
            .map_or_else(Vec::new, |(_, reads)| reads.clone());
        combined.extend(spans);
        self.existence_reads = Some((expanded, combined));
        let previous_comprehensions = self.comprehension_reads.take();
        let mut combined_counts = previous_comprehensions
            .as_ref()
            .filter(|(guard, _)| *guard == width)
            .map_or_else(Vec::new, |(_, reads)| reads.clone());
        combined_counts.extend(counts);
        self.comprehension_reads = Some((expanded, combined_counts));
        let previous_grouped = self.grouped_comprehensions.clone();
        let previous_projected = self.projected_sources.clone();
        for reads in [
            &mut self.grouped_comprehensions,
            &mut self.projected_sources,
        ] {
            if let Some((guard, _)) = reads
                && *guard == width
            {
                *guard = expanded;
            }
        }
        (self.lexer, self.current) = saved;
        if returning && aggregate {
            // The terminal aggregate compiler owns RETURN and consumes these
            // values as grouping keys or aggregate arguments. Keep the exact
            // expanded schema and private-span guards until that owner has
            // read the clause; restoring the old maps here loses its slots.
            return Ok((parts, schema, true));
        }
        let mut pipeline = Vec::new();
        let mut depth = 0;
        let mut projected = None;
        let selected = if projecting {
            // The spans address the expanded row until RETURN has consumed
            // every expression, including its ordinary ORDER BY/page.
            self.multipart_row_tail(schema, aggregate)
                .map(|(stages, next, terminal)| {
                    pipeline = stages;
                    projected = Some((next, terminal));
                })
        } else {
            self.word("WHERE")?;
            self.row_expression_selection(&schema, &mut pipeline, &mut depth, at)
        };
        if with_projection {
            self.existence_reads = None;
            self.comprehension_reads = None;
        } else if returning {
            // A terminal projection already ended its row scope.
            self.boundary_reads = detached_boundary;
        } else if had_boundary {
            self.boundary_reads
                .as_mut()
                .expect("matching boundary")
                .width = width;
        } else {
            self.boundary_reads = detached_boundary;
        }
        if !with_projection {
            self.existence_reads = previous_existence;
            self.comprehension_reads = previous_comprehensions;
            self.grouped_comprehensions = previous_grouped;
            self.projected_sources = previous_projected;
        }
        selected?;
        let (next, terminal) = if let Some(projected) = projected {
            projected
        } else {
            append_stage(
                &mut pipeline,
                ReadStageTemplate::Project {
                    at,
                    projection: incoming
                        .iter()
                        .enumerate()
                        .map(|(column, (name, _))| ReadProjectionTemplate {
                            name: name.text.to_owned(),
                            value: ReadValueTemplate::Column(column),
                        })
                        .collect(),
                    quantifier: GraphSetQuantifier::All,
                },
                &mut depth,
            )?;
            let (tail, next, terminal) = self.multipart_row_tail(incoming.clone(), aggregate)?;
            pipeline.extend(tail);
            (next, terminal)
        };
        let mut syntax = self.take_part_syntax()?;
        syntax.return_at = at;
        let last = parts
            .last_mut()
            .expect("row existence expression has a graph test");
        syntax
            .element_keys
            .append(&mut last.input.syntax.element_keys);
        last.input.syntax = syntax;
        last.input.pipeline = pipeline;
        Ok((parts, next, terminal))
    }

    #[allow(clippy::type_complexity)]
    fn gather_row_expression_sources(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        visible: usize,
        call_depth: usize,
        expressions: Vec<(Token<'a>, Lexer<'a>)>,
    ) -> Result<
        (
            Vec<UnresolvedContinuation<'a>>,
            RowSchema<'a>,
            Vec<(usize, usize, usize)>,
            Vec<(usize, usize, usize)>,
        ),
        GraphSetTextError,
    > {
        let at = self.current.at;
        let mut schema = incoming.clone();
        let mut parts = Vec::new();
        let mut spans = Vec::new();
        let mut counts = Vec::new();
        for (token, lexer) in expressions {
            self.current = token;
            self.lexer = lexer;
            let count = self.is_word("COUNT");
            let quantifier = self.list_quantifier()?;
            let list =
                row_list_graph_expression_follows(self.current.kind, &self.lexer, |_| false)?;
            let collected = list || self.starts_pattern_comprehension()?;
            let (operation, mapped, leading) = if list {
                let (operation, leading) = self.multipart_list_comprehension(
                    statement,
                    &mut schema,
                    &mut parts,
                    visible,
                    incoming.len(),
                    call_depth,
                )?;
                let list = Box::new(ReadValueTemplate::Column(schema.len() + 1));
                let value = match quantifier {
                    Some(kind) => ReadValueTemplate::Quantifier {
                        kind,
                        list,
                        predicate: Box::new(ReadValueTemplate::Local(0)),
                    },
                    None => *list,
                };
                (operation, Some(value), leading)
            } else if collected {
                let (pattern, join, column, value) =
                    self.multipart_comprehension_pattern(statement, &schema, visible, token.at)?;
                (
                    UnresolvedContinuationOperation::Pattern {
                        pattern: Box::new(pattern),
                        join,
                        result: RowPatternResult::Collect { column },
                    },
                    Some(value),
                    Vec::new(),
                )
            } else {
                (
                    self.multipart_existence_operation(
                        statement,
                        &schema,
                        visible,
                        call_depth,
                        if count {
                            RowPatternResult::Count
                        } else {
                            RowPatternResult::Exists
                        },
                    )?,
                    None,
                    Vec::new(),
                )
            };
            self.capacity(
                schema.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let Some(&name) = BOUNDARY_READ_NAMES
                .iter()
                .find(|name| schema.iter().all(|(binding, _)| binding.text != **name))
            else {
                return Err(expected(token.at, "bounded private row existence columns"));
            };
            let column = schema.len();
            if count || collected {
                counts.push((token.at, self.current.at, column));
            } else {
                spans.push((token.at, self.current.at, column));
            }
            let mut projection: Vec<_> = schema
                .iter()
                .enumerate()
                .map(|(column, (name, _))| ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                })
                .collect();
            projection.push(ReadProjectionTemplate {
                name: name.to_owned(),
                value: if let Some(value) = mapped {
                    value
                } else if count {
                    ReadValueTemplate::Column(column)
                } else {
                    ReadValueTemplate::Integer {
                        program: vec![
                            crate::mutation_text::MutationIntegerTemplateOp::Bound(
                                crate::GraphIntegerOp::ScalarColumn(column),
                            ),
                            crate::mutation_text::MutationIntegerTemplateOp::Bound(
                                crate::GraphIntegerOp::IsNull(false),
                            ),
                        ],
                        at: token.at,
                    }
                },
            });
            schema.push((
                Name {
                    text: name,
                    at: token.at,
                },
                if collected && quantifier.is_none() {
                    GraphSetColumnType::List
                } else {
                    GraphSetColumnType::Scalar
                },
            ));
            let mut syntax = self.take_part_syntax()?;
            syntax.return_at = at;
            parts.push(UnresolvedContinuation {
                input: UnresolvedGraphText {
                    statement,
                    syntax,
                    projection: Some(projection),
                    pipeline: Vec::new(),
                    singleton: true,
                    leading,
                    leading_types: schema
                        .iter()
                        .map(|(_, kind)| *kind)
                        .chain(list.then_some(GraphSetColumnType::List))
                        .collect(),
                    correlations: Vec::new(),
                    row_fields: Vec::new(),
                },
                operation,
            });
        }
        Ok((parts, schema, spans, counts))
    }

    /// Gather an IN expression before its local exists. Source reads keep the
    /// incoming namespace and the graph head's existing hidden values; only
    /// exact spans can address newly appended private source columns.
    fn read_list_source(
        &mut self,
        statement: &'a str,
        incoming: &mut RowSchema<'a>,
        parts: &mut Vec<UnresolvedContinuation<'a>>,
        captures: usize,
        read_width: usize,
        call_depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let width = incoming.len();
        let boundary = self.boundary_reads.take();
        let existence = self.existence_reads.take();
        let comprehensions = self.comprehension_reads.take();
        let grouped = self.grouped_comprehensions.take();
        let projected = self.projected_sources.take();
        let widen = |reads: &Option<HoistedReads>| {
            reads
                .as_ref()
                .filter(|(guard, _)| *guard == read_width)
                .map(|(_, spans)| (width, spans.clone()))
        };
        let mut source_boundary = boundary
            .as_ref()
            .filter(|boundary| boundary.width == read_width)
            .cloned()
            .unwrap_or_else(|| BoundaryReads {
                visible: captures,
                width,
                reads: Vec::new(),
                functions: Vec::new(),
                grouped: Vec::new(),
                scoped: Vec::new(),
            });
        source_boundary.visible = captures;
        source_boundary.width = width;
        self.boundary_reads = Some(source_boundary);
        self.existence_reads = widen(&existence);
        self.comprehension_reads = widen(&comprehensions);
        self.grouped_comprehensions = widen(&grouped);
        self.projected_sources = widen(&projected);
        let parsed = (|| {
            let saved = (self.lexer.clone(), self.current);
            let expressions = row_pattern_expressions_from(
                self.current,
                &mut self.lexer.clone(),
                |at| {
                    hoisted_span(self.existence_reads.as_ref(), width, at).is_some()
                        || hoisted_span(self.comprehension_reads.as_ref(), width, at).is_some()
                },
                true,
            )?;
            let (prefix, expanded, spans, counts) = self.gather_row_expression_sources(
                statement,
                incoming,
                captures,
                call_depth + 1,
                expressions,
            )?;
            let expanded_width = expanded.len();
            self.boundary_reads
                .as_mut()
                .expect("the source owns its incoming row")
                .width = expanded_width;
            for (guard, _) in [
                &mut self.existence_reads,
                &mut self.comprehension_reads,
                &mut self.grouped_comprehensions,
                &mut self.projected_sources,
            ]
            .into_iter()
            .flatten()
            {
                *guard = expanded_width;
            }
            self.existence_reads
                .get_or_insert_with(|| (expanded_width, Vec::new()))
                .1
                .extend(spans);
            self.comprehension_reads
                .get_or_insert_with(|| (expanded_width, Vec::new()))
                .1
                .extend(counts);
            (self.lexer, self.current) = saved;
            let value = self.read_row_value(&expanded, 0)?;
            Ok::<_, GraphSetTextError>((prefix, expanded, value))
        })();
        self.boundary_reads = boundary;
        self.existence_reads = existence;
        self.comprehension_reads = comprehensions;
        self.grouped_comprehensions = grouped;
        self.projected_sources = projected;
        let (prefix, expanded, value) = parsed?;
        parts.extend(prefix);
        *incoming = expanded;
        Ok(value)
    }

    /// Lower each list occurrence through native UNWIND and correlated row stages.
    /// The source evaluates before its local shadows an incoming name.
    fn multipart_list_comprehension(
        &mut self,
        statement: &'a str,
        incoming: &mut RowSchema<'a>,
        parts: &mut Vec<UnresolvedContinuation<'a>>,
        captures: usize,
        read_width: usize,
        call_depth: usize,
    ) -> Result<(UnresolvedContinuationOperation<'a>, Vec<ReadStageTemplate>), GraphSetTextError>
    {
        let at = self.current.at;
        if call_depth >= 8 {
            return Err(expected(
                at,
                "at most eight nested correlated list/read bodies",
            ));
        }
        let quantifier = self.list_quantifier()?;
        if quantifier.is_some() {
            self.advance()?;
            self.punct(b'(', "( before a list quantifier")?;
        } else {
            self.punct(b'[', "[ before a list comprehension")?;
        }
        let element = self.name()?;
        self.word("IN")?;
        let list =
            self.read_list_source(statement, incoming, parts, captures, read_width, call_depth)?;
        let types: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
        let list_type = list.column_type(&types, &self.syntax.parameters);
        let mut private = BOUNDARY_READ_NAMES.iter().copied().filter(|name| {
            *name != element.text && incoming.iter().all(|(binding, _)| binding.text != *name)
        });
        let (Some(list_name), Some(output_name)) = (private.next(), private.next()) else {
            return Err(expected(at, "bounded private list comprehension columns"));
        };
        self.capacity(
            incoming.len() + 1,
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let mut leading_projection: Vec<_> = incoming
            .iter()
            .enumerate()
            .map(|(column, (name, _))| ReadProjectionTemplate {
                name: name.text.to_owned(),
                value: ReadValueTemplate::Column(column),
            })
            .collect();
        leading_projection.push(ReadProjectionTemplate {
            name: list_name.to_owned(),
            value: list,
        });
        let leading = vec![ReadStageTemplate::Project {
            at,
            projection: leading_projection,
            quantifier: GraphSetQuantifier::All,
        }];
        let mut imports = Vec::new();
        let mut schema = Vec::new();
        for (column, &(name, kind)) in incoming[..captures].iter().enumerate() {
            if name.text != element.text {
                imports.push(column);
                schema.push((name, kind));
            }
        }
        let local = schema.len();
        let mut arguments = schema.clone();
        arguments.push((
            Name {
                text: list_name,
                at,
            },
            list_type,
        ));
        imports.push(incoming.len());
        schema.push((element, GraphSetColumnType::Any));
        let initial = vec![
            ReadStageTemplate::Arguments {
                columns: arguments
                    .iter()
                    .map(|(name, _)| name.text.to_owned())
                    .collect(),
                types: arguments.iter().map(|(_, kind)| *kind).collect(),
            },
            ReadStageTemplate::Unwind {
                at,
                name: element.text.to_owned(),
                value: ReadValueTemplate::Column(local),
            },
            ReadStageTemplate::Project {
                at,
                projection: schema
                    .iter()
                    .enumerate()
                    .map(|(column, (name, _))| ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value: ReadValueTemplate::Column(if column == local {
                            local + 1
                        } else {
                            column
                        }),
                    })
                    .collect(),
                quantifier: GraphSetQuantifier::All,
            },
        ];
        let outer_syntax = self.take_part_syntax()?;
        let boundary = self.boundary_reads.take();
        let projected = self.projected_sources.take();
        let existence = self.existence_reads.take();
        let comprehensions = self.comprehension_reads.take();
        let grouped = self.grouped_comprehensions.take();
        let row_bindings = core::mem::take(&mut self.read_row_bindings);
        let correlations = core::mem::take(&mut self.read_correlations);
        let row_fields = core::mem::take(&mut self.read_row_fields);
        let comparisons = self.read_comparisons.take();
        let element_reads = core::mem::replace(&mut self.call_element_reads, true);
        let optional_body = core::mem::take(&mut self.optional_body);
        let parsed = (|| -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
            let first = UnresolvedGraphText {
                statement,
                syntax: self.take_part_syntax()?,
                projection: None,
                pipeline: initial,
                singleton: true,
                leading: Vec::new(),
                leading_types: Vec::new(),
                correlations: Vec::new(),
                row_fields: Vec::new(),
            };
            let mut body = UnresolvedReadInput::from_first(first);
            let map = if quantifier.is_some() {
                self.word("WHERE")?;
                let predicate_at = self.current.at;
                // Finish each complete predicate before visiting the next member.
                // Eager quantifier member order and first-error precedence are retained.
                let mut predicate_body = UnresolvedReadInput::from_first(UnresolvedGraphText {
                    statement,
                    syntax: self.take_part_syntax()?,
                    projection: None,
                    pipeline: vec![ReadStageTemplate::Arguments {
                        columns: schema
                            .iter()
                            .map(|(name, _)| name.text.to_owned())
                            .collect(),
                        types: schema.iter().map(|(_, kind)| *kind).collect(),
                    }],
                    singleton: true,
                    leading: Vec::new(),
                    leading_types: Vec::new(),
                    correlations: Vec::new(),
                    row_fields: Vec::new(),
                });
                let expanded = self.gather_list_expression(
                    statement,
                    &mut predicate_body,
                    &schema,
                    call_depth + 1,
                )?;
                self.mark_hoisted_boolean_operand();
                let predicate = self.read_scalar_value(None, &expanded, predicate_at, 0, false)?;
                // Keep UNKNOWN. CASE validates Boolean/NULL without coercion, and the
                // let slot evaluates the original predicate exactly once.
                let predicate = ReadValueTemplate::Let {
                    values: vec![predicate],
                    body: Box::new(ReadValueTemplate::Case {
                        arms: vec![(ReadValueTemplate::Local(0), ReadValueTemplate::Local(0))],
                        otherwise: Some(Box::new(ReadValueTemplate::Local(0))),
                    }),
                };
                predicate_body.append_stages(vec![ReadStageTemplate::Project {
                    at: predicate_at,
                    projection: vec![ReadProjectionTemplate {
                        name: output_name.to_owned(),
                        value: predicate,
                    }],
                    quantifier: GraphSetQuantifier::All,
                }]);
                predicate_body.append_element_keys(&mut self.syntax.element_keys);
                let mut syntax = self.take_part_syntax()?;
                syntax.return_at = predicate_at;
                body.continuations.push(UnresolvedContinuation {
                    input: UnresolvedGraphText {
                        statement,
                        syntax,
                        projection: Some(vec![ReadProjectionTemplate {
                            name: output_name.to_owned(),
                            value: ReadValueTemplate::Column(schema.len()),
                        }]),
                        pipeline: Vec::new(),
                        singleton: true,
                        leading: Vec::new(),
                        leading_types: schema
                            .iter()
                            .map(|(_, kind)| *kind)
                            .chain([GraphSetColumnType::Any])
                            .collect(),
                        correlations: Vec::new(),
                        row_fields: Vec::new(),
                    },
                    operation: UnresolvedContinuationOperation::Apply {
                        body: Box::new(predicate_body),
                        imports: (0..schema.len()).collect(),
                        optional: false,
                    },
                });
                ReadValueTemplate::Column(0)
            } else {
                if self.take_word("WHERE")? {
                    let predicate_at = self.current.at;
                    let expanded =
                        self.gather_list_expression(statement, &mut body, &schema, call_depth + 1)?;
                    let mut stages = Vec::new();
                    let mut depth = body.depth();
                    self.row_expression_selection(
                        &expanded,
                        &mut stages,
                        &mut depth,
                        predicate_at,
                    )?;
                    // The map sees only retained elements. Its graph reads are below
                    // this filter, not gathered alongside the predicate's reads.
                    stages.push(ReadStageTemplate::Project {
                        at: predicate_at,
                        projection: schema
                            .iter()
                            .enumerate()
                            .map(|(column, (name, _))| ReadProjectionTemplate {
                                name: name.text.to_owned(),
                                value: ReadValueTemplate::Column(column),
                            })
                            .collect(),
                        quantifier: GraphSetQuantifier::All,
                    });
                    body.append_stages(stages);
                    body.append_element_keys(&mut self.syntax.element_keys);
                    self.boundary_reads = None;
                    self.existence_reads = None;
                    self.comprehension_reads = None;
                }
                if self.take(b'|')? {
                    let expanded =
                        self.gather_list_expression(statement, &mut body, &schema, call_depth + 1)?;
                    self.read_row_value(&expanded, 0)?
                } else {
                    ReadValueTemplate::Column(local)
                }
            };
            if quantifier.is_some() {
                self.punct(b')', ") after a list quantifier")?;
            } else {
                self.punct(b']', "] after a list comprehension")?;
            }
            body.append_stages(vec![ReadStageTemplate::Project {
                at,
                projection: vec![ReadProjectionTemplate {
                    name: output_name.to_owned(),
                    value: map,
                }],
                quantifier: GraphSetQuantifier::All,
            }]);
            body.append_element_keys(&mut self.syntax.element_keys);
            Ok(body)
        })();
        let remainder = self.take_part_syntax()?;
        self.syntax = outer_syntax;
        self.syntax.parameters = remainder.parameters;
        self.syntax.parameter_offsets = remainder.parameter_offsets;
        self.boundary_reads = boundary;
        self.projected_sources = projected;
        self.existence_reads = existence;
        self.comprehension_reads = comprehensions;
        self.grouped_comprehensions = grouped;
        self.read_row_bindings = row_bindings;
        self.read_correlations = correlations;
        self.read_row_fields = row_fields;
        self.read_comparisons = comparisons;
        self.call_element_reads = element_reads;
        self.optional_body = optional_body;
        let body = parsed?;
        Ok((
            UnresolvedContinuationOperation::List {
                body: Box::new(body),
                imports,
                list_column: incoming.len(),
            },
            leading,
        ))
    }

    /// Restore the expression lexer after gathering its native graph sources.
    fn gather_list_expression(
        &mut self,
        statement: &'a str,
        body: &mut UnresolvedReadInput<'a>,
        schema: &RowSchema<'a>,
        call_depth: usize,
    ) -> Result<RowSchema<'a>, GraphSetTextError> {
        let saved = (self.lexer.clone(), self.current);
        let expressions =
            row_pattern_expressions_from(self.current, &mut self.lexer.clone(), |_| false, false)?;
        let (parts, expanded, existence, collected) = self.gather_row_expression_sources(
            statement,
            schema,
            schema.len(),
            call_depth,
            expressions,
        )?;
        body.continuations.extend(parts);
        self.boundary_reads = Some(BoundaryReads {
            visible: schema.len(),
            width: expanded.len(),
            reads: Vec::new(),
            functions: Vec::new(),
            grouped: Vec::new(),
            scoped: Vec::new(),
        });
        self.existence_reads = Some((expanded.len(), existence));
        self.comprehension_reads = Some((expanded.len(), collected));
        (self.lexer, self.current) = saved;
        Ok(expanded)
    }

    fn multipart_existence_operation(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        captures: usize,
        call_depth: usize,
        result: RowPatternResult,
    ) -> Result<UnresolvedContinuationOperation<'a>, GraphSetTextError> {
        let at = self.current.at;
        if !returning_existence_body(self.current.kind, &self.lexer)? {
            let (pattern, join) =
                self.multipart_existence_pattern(statement, incoming, captures, at)?;
            return Ok(UnresolvedContinuationOperation::Pattern {
                pattern: Box::new(pattern),
                join,
                result,
            });
        }
        if call_depth >= 8 {
            return Err(expected(at, "at most eight nested read subquery bodies"));
        }
        self.word("EXISTS")?;
        self.punct(b'{', "{")?;
        // A body has a separate namespace, but shares the original parameter
        // table, catalog slots, lexer and statement-wide admission counters.
        // Hidden witness/property columns are not implicit imports.
        let outer_syntax = self.take_part_syntax()?;
        let boundary = self.boundary_reads.take();
        let projected = self.projected_sources.take();
        let existence = self.existence_reads.take();
        let comprehensions = self.comprehension_reads.take();
        let grouped = self.grouped_comprehensions.take();
        let row_bindings = core::mem::take(&mut self.read_row_bindings);
        let correlations = core::mem::take(&mut self.read_correlations);
        let row_fields = core::mem::take(&mut self.read_row_fields);
        let comparisons = self.read_comparisons.take();
        let element_reads = core::mem::replace(&mut self.call_element_reads, true);
        let call_body = core::mem::replace(&mut self.call_body, true);
        let optional_body = core::mem::take(&mut self.optional_body);
        let parsed = self.multipart_parts_from(
            statement,
            false,
            incoming[..captures].to_vec(),
            call_depth + 1,
        );
        let mut remainder = self.take_part_syntax()?;
        self.syntax = outer_syntax;
        self.syntax.parameters = remainder.parameters;
        self.syntax.parameter_offsets = remainder.parameter_offsets;
        self.boundary_reads = boundary;
        self.projected_sources = projected;
        self.existence_reads = existence;
        self.comprehension_reads = comprehensions;
        self.grouped_comprehensions = grouped;
        self.read_row_bindings = row_bindings;
        self.read_correlations = correlations;
        self.read_row_fields = row_fields;
        self.read_comparisons = comparisons;
        self.call_element_reads = element_reads;
        self.call_body = call_body;
        self.optional_body = optional_body;
        let (mut body, _) = parsed?;
        body.append_element_keys(&mut remainder.element_keys);
        self.punct(b'}', "} after a read-only EXISTS body ending in RETURN")?;
        Ok(UnresolvedContinuationOperation::ExistsBody {
            body: Box::new(body),
            imports: (0..captures).collect(),
            negated: match result {
                RowPatternResult::Filter { negated } => Some(negated),
                RowPatternResult::Exists => None,
                RowPatternResult::Count | RowPatternResult::Collect { .. } => {
                    unreachable!("only EXISTS has a returning body")
                }
            },
        })
    }

    /// Keep the native Collect scope's witness order and NULL placeholders
    /// intact while correlating its root with a completed row. The projection
    /// map runs only after that correlation, through the ordinary value reader.
    #[allow(clippy::type_complexity)]
    fn multipart_comprehension_pattern(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        captures: usize,
        at: usize,
    ) -> Result<
        (
            UnresolvedGraphText<'a>,
            RowJoinSpec,
            usize,
            ReadValueTemplate,
        ),
        GraphSetTextError,
    > {
        self.punct(b'[', "[ before a pattern comprehension")?;
        let scope = self.syntax.scopes.len();
        self.scope(ScopeKind::Collect, ScopeForm::Comprehension)?;
        if let Some(local) = self.syntax.scopes[scope].find_name(&|name| {
            self.elements
                .iter()
                .any(|element| !element.is_empty() && *element == name.text)
        }) {
            return Err(expected(
                local.at,
                "a pattern comprehension that reads no enclosing comprehension local",
            ));
        }
        if !self.read_correlations.is_empty()
            || !self.read_row_fields.is_empty()
            || self
                .read_comparisons
                .as_ref()
                .is_some_and(|comparisons| !comparisons.is_empty())
        {
            return Err(expected(
                at,
                "local graph predicates and parameters inside a carried pattern comprehension",
            ));
        }
        let mut inputs = Vec::new();
        let mut keys = Vec::new();
        for (left, &(name, kind)) in incoming[..captures].iter().enumerate() {
            let body = &self.syntax.scopes[scope].body;
            if body.edges.iter().any(|edge| {
                edge.variable
                    .is_some_and(|binding| binding.text == name.text)
            }) || body
                .filters
                .iter()
                .any(|filter| matches!(filter, Filter::PathCapture(path) if path.text == name.text))
            {
                return Err(expected(
                    name.at,
                    "new relationship and path bindings inside a carried pattern comprehension",
                ));
            }
            let variable = body
                .variables
                .iter()
                .find(|variable| variable.text == name.text)
                .copied();
            if let Some(variable) = variable {
                if !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Any) {
                    return Err(expected(name.at, "a vertex-valued imported binding"));
                }
                self.capacity(
                    self.syntax.variables.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Vertices,
                )?;
                self.syntax.variables.push(variable);
                let right = self.mutation_projection(&mut inputs, variable, None)?;
                keys.push((left, right));
            } else if self.syntax.scopes[scope]
                .find_name(&|binding| binding.text == name.text)
                .is_some()
            {
                // scope() propagates ancestor captures during parsing. A
                // nested-only row reference cannot become a fresh graph local
                // merely because this bridge discovers its roots afterwards.
                return Err(expected(
                    name.at,
                    "imported vertices in the comprehension's positive pattern",
                ));
            }
        }
        if keys.is_empty() {
            return Err(expected(
                at,
                "a pattern comprehension anchored on a carried vertex",
            ));
        }
        self.syntax.root_variables = self.syntax.variables.len();
        let Some(&list_name) = BOUNDARY_READ_NAMES
            .iter()
            .find(|name| incoming.iter().all(|(binding, _)| binding.text != **name))
        else {
            return Err(expected(
                at,
                "bounded private pattern comprehension columns",
            ));
        };
        let column = self.projection_slot(
            &mut inputs,
            Projection {
                variable: Name {
                    text: list_name,
                    at,
                },
                property: None,
                path: None,
                collect: Some(scope),
            },
        )?;
        self.capacity(
            incoming.len() + inputs.len() - 1,
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let map = self.pattern_comprehension_projection(
            scope,
            None,
            &incoming[..captures],
            &mut None,
            0,
        )?;
        // Missing/NULL/nonvertex anchors have no implicit root. Normalize
        // only that absent list, never a projected NULL witness value.
        let list = ReadValueTemplate::Function {
            function: crate::GraphValueFunction::Coalesce,
            argument: Box::new(ReadValueTemplate::List(vec![
                ReadValueTemplate::Column(incoming.len()),
                ReadValueTemplate::List(Vec::new()),
            ])),
        };
        let value = ReadValueTemplate::Comprehension {
            list: Box::new(list),
            filter: None,
            map: (!matches!(map, ReadValueTemplate::Local(0))).then(|| Box::new(map)),
        };
        let types: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
        let join = join_spec(&types, &inputs, &keys, &[], at)?;
        self.syntax.columns = inputs
            .into_iter()
            .map(|input| Column {
                variable: input.variable,
                property: input.property,
                path: input.path,
                alias: input.variable,
                collect: input.collect,
            })
            .collect();
        self.syntax.return_at = at;
        let pattern = UnresolvedGraphText {
            statement,
            syntax: self.take_part_syntax()?,
            projection: None,
            pipeline: Vec::new(),
            singleton: false,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
            row_fields: Vec::new(),
        };
        self.read_row_bindings.clear();
        Ok((pattern, join, column, value))
    }

    /// Parse one positive graph witness with the shared lexer and parameter
    /// table. Only user-visible incoming identities may correlate a compound
    /// predicate; earlier private witness columns are never graph bindings.
    fn multipart_existence_pattern(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        captures: usize,
        at: usize,
    ) -> Result<(UnresolvedGraphText<'a>, RowJoinSpec), GraphSetTextError> {
        self.read_row_bindings = incoming[..captures].iter().map(|(name, _)| *name).collect();
        let count = self.take_word("COUNT")?;
        let explicit = count || self.take_word("EXISTS")?;
        let braces = explicit && self.take(b'{')?;
        if count && !braces {
            return Err(expected(at, "{ before a row-scope pattern count"));
        }
        if braces {
            if !count || !self.is_punct(b'(') {
                self.word("MATCH")?;
            }
            self.syntax.match_mode = self.match_mode()?;
        } else if explicit {
            self.punct(b'(', "( before an EXISTS pattern")?;
        }
        self.positive_pattern()?;
        self.syntax.root_variables = self.syntax.variables.len();
        // The count's WHERE belongs to its positive graph body. Its own
        // graph variables and native parameters resolve before correlation;
        // computed reads of the containing row still refuse below.
        if count && self.take_word("WHERE")? {
            self.scoped_predicates(true)?;
        }
        if explicit {
            if braces {
                self.punct(b'}', "} after an identity-correlated graph pattern")?;
            } else {
                self.punct(b')', ") after an EXISTS pattern")?;
            }
        }
        if !self.read_correlations.is_empty()
            || !self.read_row_fields.is_empty()
            || self
                .read_comparisons
                .as_ref()
                .is_some_and(|comparisons| !comparisons.is_empty())
        {
            return Err(expected(
                at,
                "element identity anchors and literal or parameter properties in a row-scope existence predicate",
            ));
        }
        let mut inputs = Vec::new();
        let mut keys = Vec::new();
        for (left, &(name, kind)) in incoming[..captures].iter().enumerate() {
            if matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Any)
                && !self
                    .syntax
                    .variables
                    .iter()
                    .any(|variable| variable.text == name.text)
                && self.syntax.scopes.iter().any(|scope| {
                    scope
                        .find_name(&|binding| binding.text == name.text)
                        .is_some()
                })
            {
                return Err(expected(name.at, "imported vertices in the root MATCH"));
            }
            if let Some(variable) = self
                .syntax
                .variables
                .iter()
                .find(|variable| variable.text == name.text)
                .copied()
            {
                if !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Any) {
                    return Err(expected(name.at, "a vertex-valued imported binding"));
                }
                let right = self.mutation_projection(&mut inputs, variable, None)?;
                keys.push((left, right));
            }
            if self.syntax.names_path(name.text) {
                return Err(expected(
                    name.at,
                    "new path bindings inside an existence predicate",
                ));
            }
            if let Some((edge, scoped)) = self.syntax.visible_edge(name.text) {
                let Some(variable) = edge.variable else {
                    continue;
                };
                if scoped || edge.walk.is_some() || kind != GraphSetColumnType::Edge {
                    return Err(expected(
                        name.at,
                        "an edge-valued imported root relationship",
                    ));
                }
                let right = self.mutation_projection(&mut inputs, variable, None)?;
                keys.push((left, right));
            }
        }
        if keys.is_empty() {
            return Err(expected(
                at,
                "an existence pattern anchored on a carried element",
            ));
        }
        self.capacity(
            incoming.len() + inputs.len() - 1,
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let types: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
        let join = join_spec(&types, &inputs, &keys, &[], at)?;
        self.syntax.columns = inputs
            .into_iter()
            .map(|input| Column {
                variable: input.variable,
                property: input.property,
                path: input.path,
                alias: input.variable,
                collect: input.collect,
            })
            .collect();
        self.syntax.return_at = at;
        let pattern = UnresolvedGraphText {
            statement,
            syntax: self.take_part_syntax()?,
            projection: None,
            pipeline: Vec::new(),
            singleton: false,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
            row_fields: Vec::new(),
        };
        self.read_row_bindings.clear();
        Ok((pattern, join))
    }

    /// A separable ON operand over the incoming row uses the existing private
    /// input projection. In particular it must not re-match a carried vertex:
    /// that vertex can be NULL, and only the new bindings are null-extended.
    pub(in crate::graph_text) fn computed_imported_operand(
        &mut self,
    ) -> Result<usize, GraphPatternTextError> {
        let at = self.current.at;
        let schema: Vec<_> = self
            .read_row_bindings
            .iter()
            .map(|name| (*name, GraphSetColumnType::Any))
            .collect();
        let (operand, slots) = self
            .row_operand_with_lets(&schema)
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))?;
        let value = self
            .scalar_let_value(operand, slots, at)
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))?;
        if let ReadValueTemplate::Column(column) = value {
            return Ok(column);
        }
        let column = self.read_row_bindings.len() + self.read_row_fields.len();
        self.capacity(
            column,
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        self.read_row_fields.push(value);
        Ok(column)
    }

    fn multipart_read(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        let (mut result, _) = self.multipart_parts(statement, false)?;
        self.end()?;
        result.share_parameters(&self.syntax.parameters, &self.syntax.parameter_offsets);
        Ok(result)
    }

    // Leave RETURN to the exact aggregate compiler, with the same live lexer,
    // argument indices and statement-wide counters. No synthetic query text or
    // second MATCH parser is introduced. The tuple crosses only this private
    // module boundary; the aggregate owner immediately restores a typed head.
    #[allow(clippy::type_complexity)]
    pub(in crate::graph_text) fn multipart_aggregate_prefix(
        &mut self,
        statement: &'a str,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>, usize), GraphSetTextError> {
        let (result, schema) = self.multipart_parts(statement, true)?;
        let depth = result.depth();
        Ok((result, schema, depth))
    }

    /// An insertion or mutation reads the complete native relation before
    /// producing its effects. A later write starts from the previous write's
    /// owned rows and staged graph. Leave the write token and live parameter
    /// table with its owner; no manufactured RETURN is needed.
    pub(in crate::graph_text::scoped::mutation) fn multipart_write_prefix(
        &mut self,
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        incoming: Option<RowSchema<'a>>,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>), GraphSetTextError> {
        let tokens = PreparedGraphText::composition_tokens(statement, declarations)?;
        let zones = self.zones.clone();
        let chain_write = self.chain_write;
        let at = self.current.at;
        let boundary = self.boundary_reads.clone();
        let parameter_types = self.parameter_types.clone();
        let parse = |deferred: &[usize], implicit: &[usize]| -> Result<_, GraphSetTextError> {
            let mut parser =
                Self::new_with_parameter_types(statement, declarations)?.with_zones(zones.clone());
            if incoming.is_some() {
                parser.parameter_types.clone_from(&parameter_types);
                parser = parser.resume_after(boundary.clone(), &[], at)?;
            }
            parser.deferred_wheres = deferred.to_vec();
            parser.implicit_withs = implicit.to_vec();
            parser.chain_write = chain_write;
            parser.read_elements = true;
            parser.call_element_reads = true;
            parser.write_prefix = true;
            let (input, schema) = parser.multipart_parts_with_input(
                statement,
                true,
                incoming.clone().unwrap_or_default(),
                0,
                incoming.is_some(),
            )?;
            if !parser.at_multipart_write() {
                return Err(expected(
                    parser.current.at,
                    "CREATE, INSERT, SET, REMOVE or DELETE after the complete read",
                ));
            }
            parser.write_prefix = false;
            parser.admit_multipart_size(&input)?;
            Ok((parser, input, schema))
        };
        // The same unresolved normalization as an ordinary multipart read:
        // computed/imported WHERE values get their real row boundary, while
        // an OPTIONAL predicate remains inside the optional Apply. Re-reading
        // here resolves no catalog symbol and cannot issue a graph access.
        let (parser, input, schema) = match parse(&[], &[]) {
            Ok(input) => input,
            Err(original) => {
                let (mut deferred, mut implicit) = (Vec::new(), Vec::new());
                let mut offset = original.offset;
                loop {
                    if let Some(at) =
                        crate::set_text::multipart::continuation_where_at(&tokens, offset)
                            .or_else(|| {
                                crate::set_text::multipart::existence_where_at(&tokens, offset)
                            })
                            .filter(|candidate| *candidate >= at && !deferred.contains(candidate))
                    {
                        deferred.push(at);
                    } else if let Some(at) =
                        crate::set_text::multipart::implicit_with_at(&tokens, offset)
                            .filter(|candidate| *candidate >= at && !implicit.contains(candidate))
                    {
                        implicit.push(at);
                    } else if let Some(at) =
                        crate::set_text::multipart::optional_where_at(&tokens, offset)
                            .filter(|candidate| *candidate >= at && !deferred.contains(candidate))
                    {
                        deferred.push(at);
                    } else {
                        return Err(original);
                    }
                    match parse(&deferred, &implicit) {
                        Ok(input) => break input,
                        Err(error) => offset = error.offset,
                    }
                }
            }
        };
        *self = parser;
        Ok((input, schema))
    }

    fn at_multipart_write(&self) -> bool {
        self.write_prefix
            && !self.call_body
            && ["CREATE", "INSERT", "SET", "REMOVE", "DELETE", "DETACH"]
                .iter()
                .any(|write| self.is_word(write))
    }

    fn multipart_parts(
        &mut self,
        statement: &'a str,
        aggregate: bool,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>), GraphSetTextError> {
        self.multipart_parts_from(statement, aggregate, Vec::new(), 0)
    }

    /// A terminal read after a write uses exactly the ordinary multipart
    /// joins, scopes and projections. Its first relation is supplied by the
    /// write host, not a unit row or an Apply argument frame. RETURN shares
    /// the row-domain aggregate owner used by the other write families.
    pub(in crate::graph_text::scoped::mutation) fn write_read_continuation(
        mut self,
        statement: &'a str,
        incoming: RowSchema<'a>,
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        self.read_elements = true;
        self.call_element_reads = true;
        let (mut result, schema) =
            self.multipart_parts_with_input(statement, true, incoming, 0, true)?;
        let stages = self.write_row_pipeline(schema)?;
        result.append_stages(stages);
        result.append_element_keys(&mut self.syntax.element_keys);
        // Like every chain step, the read leaves the unused-declaration check
        // to the chain, since an earlier write may read a declaration.
        self.chain_write = true;
        self.end()?;
        result.share_parameters(&self.syntax.parameters, &self.syntax.parameter_offsets);
        self.admit_multipart_size(&result)?;
        Ok(result)
    }

    fn multipart_parts_from(
        &mut self,
        statement: &'a str,
        aggregate: bool,
        incoming: RowSchema<'a>,
        call_depth: usize,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>), GraphSetTextError> {
        self.multipart_parts_with_input(statement, aggregate, incoming, call_depth, false)
    }

    fn multipart_parts_with_input(
        &mut self,
        statement: &'a str,
        aggregate: bool,
        mut incoming: RowSchema<'a>,
        call_depth: usize,
        owned: bool,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>), GraphSetTextError> {
        self.refuse_call_write(call_depth)?;
        let mut terminal = false;
        let first = if self.is_word("MATCH") && incoming.is_empty() && !owned {
            let (input, next, terminal, _) =
                self.multipart_graph_part(statement, &incoming, None, aggregate, call_depth)?;
            if terminal {
                return Ok((UnresolvedReadInput::from_first(input), next));
            }
            incoming = next;
            input
        } else {
            // A values-only leading pipeline is a unit relation followed by
            // the existing row stages, not a manufactured graph carrier.
            let arguments =
                (!owned && !incoming.is_empty()).then(|| ReadStageTemplate::Arguments {
                    columns: incoming
                        .iter()
                        .map(|(name, _)| name.text.to_owned())
                        .collect(),
                    types: incoming.iter().map(|(_, kind)| *kind).collect(),
                });
            let (mut pipeline, next, done) = self.multipart_row_tail(incoming, aggregate)?;
            self.refuse_call_write(call_depth)?;
            if let Some(arguments) = arguments {
                pipeline.insert(0, arguments);
            }
            incoming = next;
            terminal = done;
            if !terminal
                && !(self.is_word("MATCH")
                    || self.is_word("OPTIONAL")
                    || self.inline_call()?
                    || self.row_existence_filter(incoming.len())?
                    || self.row_existence_projection(incoming.len())?)
            {
                return Err(expected(
                    self.current.at,
                    "MATCH, OPTIONAL MATCH, inline CALL or a whole pattern WHERE after row stages",
                ));
            }
            let syntax = self.take_part_syntax()?;
            UnresolvedGraphText {
                statement,
                syntax,
                projection: None,
                pipeline,
                singleton: true,
                leading: Vec::new(),
                leading_types: Vec::new(),
                correlations: Vec::new(),
                row_fields: Vec::new(),
            }
        };
        let mut result = UnresolvedReadInput {
            first,
            continuations: Vec::new(),
        };
        while !terminal {
            self.refuse_call_write(call_depth)?;
            if self.row_existence_filter(incoming.len())?
                || self.row_existence_projection(incoming.len())?
            {
                let (next, done) = if self.is_word("WHERE")
                    && self.whole_row_existence_filter(incoming.len())?
                {
                    let (part, next, done) =
                        self.multipart_existence_part(statement, &incoming, aggregate, call_depth)?;
                    result.continuations.push(part);
                    (next, done)
                } else {
                    let (parts, next, done) = self.multipart_existence_expression(
                        statement, &incoming, aggregate, call_depth,
                    )?;
                    result.continuations.extend(parts);
                    (next, done)
                };
                incoming = next;
                terminal = done;
            } else if self.inline_call()? {
                let (part, next, done) =
                    self.multipart_call_part(statement, &incoming, aggregate, call_depth)?;
                result.continuations.push(part);
                incoming = next;
                terminal = done;
            } else {
                let kind = if self.take_word("OPTIONAL")? {
                    RowJoinKind::Left
                } else {
                    RowJoinKind::Inner
                };
                if !self.is_word("MATCH") {
                    return Err(expected(self.current.at, "MATCH after WITH or OPTIONAL"));
                }
                if kind == RowJoinKind::Left && self.clause_where_deferred()? {
                    let (part, next, done) =
                        self.multipart_optional_apply(statement, &incoming, aggregate, call_depth)?;
                    result.continuations.push(part);
                    incoming = next;
                    terminal = done;
                    self.admit_multipart_size(&result)?;
                    continue;
                }
                let (input, next, done, join) = self.multipart_graph_part(
                    statement,
                    &incoming,
                    Some(kind),
                    aggregate,
                    call_depth,
                )?;
                result.continuations.push(UnresolvedContinuation {
                    input,
                    operation: UnresolvedContinuationOperation::Match(
                        join.expect("a continuation has an incoming relation"),
                    ),
                });
                incoming = next;
                terminal = done;
            }
            self.admit_multipart_size(&result)?;
        }
        Ok((result, incoming))
    }

    fn admit_multipart_size(
        &self,
        result: &UnresolvedReadInput<'_>,
    ) -> Result<(), GraphSetTextError> {
        if result.depth() > crate::MAX_GRAPH_SET_DEPTH {
            return Err(GraphSetTextError {
                offset: self.current.at,
                kind: GraphSetTextErrorKind::SetBuild(crate::GraphSetBuildError::TooDeep {
                    limit: crate::MAX_GRAPH_SET_DEPTH,
                    observed: result.depth(),
                }),
            });
        }
        if result.operand_count() > crate::MAX_GRAPH_SET_OPERANDS {
            return Err(expected(self.current.at, "bounded graph-source count"));
        }
        Ok(())
    }

    fn inline_call(&self) -> Result<bool, GraphPatternTextError> {
        Ok(self.is_word("CALL")
            && matches!(
                self.lexer.clone().next()?.kind,
                TokenKind::Punct(b'{' | b'(')
            ))
    }

    fn refuse_call_write(&self, call_depth: usize) -> Result<(), GraphSetTextError> {
        if call_depth != 0
            && [
                "CREATE", "INSERT", "MERGE", "SET", "DELETE", "DETACH", "REMOVE",
            ]
            .iter()
            .any(|clause| self.is_word(clause))
        {
            return Err(GraphSetTextError {
                offset: self.current.at,
                kind: GraphSetTextErrorKind::CallBodyWrite,
            });
        }
        Ok(())
    }

    fn multipart_row_tail(
        &mut self,
        schema: RowSchema<'a>,
        aggregate: bool,
    ) -> Result<(Vec<ReadStageTemplate>, RowSchema<'a>, bool), GraphSetTextError> {
        let (mut stages, mut schema, _) = self.row_pipeline_prefix(schema)?;
        if self.at_multipart_write() {
            return Ok((stages, schema, true));
        }
        if !self.is_word("RETURN") || self.row_existence_projection(schema.len())? {
            return Ok((stages, schema, false));
        }
        // An inline CALL body's aggregating RETURN, once per outer row.
        if !aggregate && self.call_body && self.return_has_aggregate()? {
            let (stage, next) = self.return_aggregate_stage(&schema)?;
            stages.push(stage);
            if let Some(page) = self.row_expression_page(&next)? {
                stages.push(page);
            }
            self.boundary_reads = None;
            self.projected_sources = None;
            return Ok((stages, next, true));
        }
        if !aggregate {
            let at = self.current.at;
            self.advance()?;
            let distinct = self.take_word("DISTINCT")?;
            if !distinct {
                self.take_all_quantifier()?;
            }
            let (mut projection, next) = self.row_projection(&schema, true)?;
            let page = self.return_expression_page(&schema, &mut projection, distinct)?;
            self.boundary_reads = None;
            stages.push(ReadStageTemplate::Project {
                at,
                projection,
                quantifier: if distinct {
                    GraphSetQuantifier::Distinct
                } else {
                    GraphSetQuantifier::All
                },
            });
            stages.extend(page);
            schema = next;
        }
        Ok((stages, schema, true))
    }

    fn multipart_call_part(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        aggregate: bool,
        call_depth: usize,
    ) -> Result<(UnresolvedContinuation<'a>, RowSchema<'a>, bool), GraphSetTextError> {
        let at = self.current.at;
        // Admission precedes recursive descent and catalog access. This is a
        // parser stack bound, in addition to the whole relational depth bound.
        if call_depth >= 8 {
            return Err(expected(at, "at most eight nested inline CALL bodies"));
        }
        self.word("CALL")?;
        let mut imports = Vec::new();
        let mut schema = Vec::new();
        let scoped = self.take(b'(')?;
        if scoped && !self.take(b')')? {
            loop {
                let name = self.name()?;
                let Some(column) = incoming
                    .iter()
                    .position(|(binding, _)| binding.text == name.text)
                else {
                    return Err(expected(name.at, "an existing CALL import"));
                };
                if imports.contains(&column) {
                    return Err(expected(name.at, "unique CALL imports"));
                }
                self.capacity(
                    imports.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                imports.push(column);
                schema.push((name, incoming[column].1));
                if !self.take(b',')? {
                    break;
                }
            }
            self.punct(b')', ")")?;
        }
        self.punct(b'{', "{")?;
        if !scoped && let Some((columns, names)) = self.legacy_call_imports(incoming)? {
            imports = columns;
            schema = names;
        }
        self.read_row_bindings.clear();
        self.boundary_reads = None;
        self.projected_sources = None;
        let previous_reads = core::mem::replace(&mut self.call_element_reads, true);
        let previous_body = core::mem::replace(&mut self.call_body, true);
        let body = self.multipart_parts_from(statement, false, schema, call_depth + 1);
        self.call_body = previous_body;
        self.call_element_reads = previous_reads;
        let (body, exports) = body?;
        self.punct(b'}', "} after a read-only CALL body ending in RETURN")?;
        self.read_row_bindings.clear();
        self.boundary_reads = None;
        self.projected_sources = None;
        let mut combined = incoming.clone();
        for &(name, kind) in &exports {
            if combined
                .iter()
                .any(|(binding, _)| binding.text == name.text)
            {
                return Err(expected(
                    name.at,
                    "a CALL export distinct from outer bindings",
                ));
            }
            self.capacity(
                combined.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            combined.push((name, kind));
        }
        let types: Vec<GraphSetColumnType> = combined.iter().map(|(_, kind)| *kind).collect();
        let mut head = GraphProjectionHead {
            with: true,
            plain: false,
            inputs: Vec::new(),
            outputs: (combined.iter().enumerate())
                .map(|(column, &(name, _))| (name, ReadValueTemplate::Column(column)))
                .collect(),
        };
        // A write after the CALL reads its carried identities' preimages
        // (fgdb-cpny9), as a continuation head's write does: row reads only,
        // never a graph input in this unit part.
        if self.write_prefix && !self.call_body {
            self.hoist_boundary_reads_with_imports(&mut head, combined.len(), &combined)?;
            if !head.inputs.is_empty() {
                return Err(expected(at, "carried row reads after an inline CALL"));
            }
            combined = (head.outputs.iter())
                .map(|(name, value)| (*name, value.column_type(&types, &self.syntax.parameters)))
                .collect();
        }
        let projection = (head.outputs.into_iter())
            .map(|(name, value)| ReadProjectionTemplate {
                name: name.text.to_owned(),
                value,
            })
            .collect();
        // A returning CALL exports real typed identities, not graph slots.
        // Its caller's WHERE/RETURN/aggregate reads them through the same
        // governed element reader as the body; dropped names remain absent.
        self.call_element_reads = true;
        let (pipeline, next, terminal) = self.multipart_row_tail(combined, aggregate)?;
        let mut syntax = self.take_part_syntax()?;
        syntax.return_at = at;
        Ok((
            UnresolvedContinuation {
                input: UnresolvedGraphText {
                    statement,
                    syntax,
                    projection: Some(projection),
                    pipeline,
                    singleton: true,
                    leading: Vec::new(),
                    leading_types: types,
                    correlations: Vec::new(),
                    row_fields: Vec::new(),
                },
                operation: UnresolvedContinuationOperation::Apply {
                    body: Box::new(body),
                    imports,
                    optional: false,
                },
            },
            next,
            terminal,
        ))
    }

    /// `OPTIONAL MATCH ... WHERE w`, where `w` reads the incoming row in a way
    /// the graph predicate cannot place (`WHERE friend IN friends`, LDBC SNB
    /// IC5): an optional Apply. Per incoming row, its body matches the clause
    /// as a required continuation correlated on the carried bindings and keeps
    /// the matches `w` holds for, read after an implicit `WITH *`; a row left
    /// with none gets one NULL extension. A filter after the left join would
    /// instead drop the null-extended rows. The body exports the clause's own
    /// bindings, which the clauses after it read as a CALL's exports.
    fn multipart_optional_apply(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        aggregate: bool,
        call_depth: usize,
    ) -> Result<(UnresolvedContinuation<'a>, RowSchema<'a>, bool), GraphSetTextError> {
        let at = self.current.at;
        let imports: Vec<usize> = (0..incoming.len()).collect();
        self.boundary_reads = None;
        self.projected_sources = None;
        self.optional_body = true;
        let body =
            self.multipart_parts_with_input(statement, false, incoming.clone(), call_depth, false);
        self.optional_body = false;
        let (body, exports) = body?;
        self.read_row_bindings.clear();
        self.boundary_reads = None;
        self.projected_sources = None;
        let mut combined = incoming.clone();
        for &(name, kind) in &exports {
            self.capacity(
                combined.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            combined.push((name, kind));
        }
        let projection = combined
            .iter()
            .enumerate()
            .map(|(column, (name, _))| ReadProjectionTemplate {
                name: name.text.to_owned(),
                value: ReadValueTemplate::Column(column),
            })
            .collect();
        let types = combined.iter().map(|(_, kind)| *kind).collect();
        // The clause's bindings are real typed identities after the Apply, as
        // a CALL's exports are; later clauses read their properties through
        // the governed element reader.
        self.call_element_reads = true;
        let (pipeline, next, terminal) = self.multipart_row_tail(combined, aggregate)?;
        let mut syntax = self.take_part_syntax()?;
        syntax.return_at = at;
        Ok((
            UnresolvedContinuation {
                input: UnresolvedGraphText {
                    statement,
                    syntax,
                    projection: Some(projection),
                    pipeline,
                    singleton: true,
                    leading: Vec::new(),
                    leading_types: types,
                    correlations: Vec::new(),
                    row_fields: Vec::new(),
                },
                operation: UnresolvedContinuationOperation::Apply {
                    body: Box::new(body),
                    imports,
                    optional: true,
                },
            },
            next,
            terminal,
        ))
    }

    /// Whether the clause starting at this MATCH has a top-level WHERE the
    /// composition entry re-reads (deferred_wheres). Nothing is consumed.
    fn clause_where_deferred(&self) -> Result<bool, GraphPatternTextError> {
        const CLAUSES: [&str; 10] = [
            "WITH", "RETURN", "MATCH", "OPTIONAL", "UNWIND", "CALL", "UNION", "ORDER", "SKIP",
            "LIMIT",
        ];
        let mut lexer = self.lexer.clone();
        let mut previous = self.current.kind;
        let mut depth = 0_usize;
        loop {
            let token = lexer.next()?;
            match token.kind {
                TokenKind::End => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word)
                    if depth == 0 && !matches!(previous, TokenKind::Punct(b'.')) =>
                {
                    if word.eq_ignore_ascii_case("WHERE") {
                        return Ok(self.deferred_wheres.contains(&token.at));
                    }
                    if CLAUSES
                        .iter()
                        .any(|clause| word.eq_ignore_ascii_case(clause))
                    {
                        return Ok(false);
                    }
                }
                _ => {}
            }
            previous = token.kind;
        }
    }

    /// The legacy importing WITH accepts only a list of outer names. Inspect
    /// the same lexer without consuming it: the ordinary WITH owner still
    /// builds its projection. Expressions remain ordinary local row stages;
    /// they cannot acquire an unlisted outer binding.
    fn legacy_call_imports(
        &self,
        incoming: &RowSchema<'a>,
    ) -> Result<Option<(Vec<usize>, RowSchema<'a>)>, GraphSetTextError> {
        if !self.is_word("WITH") {
            return Ok(None);
        }
        let mut lexer = self.lexer.clone();
        let mut imports = Vec::new();
        let mut schema = Vec::new();
        loop {
            let token = lexer.next()?;
            let TokenKind::Word(text) = token.kind else {
                return Ok(None);
            };
            let Some(column) = incoming.iter().position(|(name, _)| name.text == text) else {
                return Ok(None);
            };
            if imports.contains(&column) {
                return Err(expected(token.at, "unique CALL imports"));
            }
            self.capacity(
                imports.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            imports.push(column);
            schema.push((Name { text, at: token.at }, incoming[column].1));
            let next = lexer.next()?;
            if matches!(next.kind, TokenKind::Punct(b',')) {
                continue;
            }
            return Ok(matches!(next.kind, TokenKind::Word(word)
                if ["RETURN", "MATCH", "OPTIONAL", "WITH", "UNWIND", "CALL"].iter().any(|clause| word.eq_ignore_ascii_case(clause)))
                .then_some((imports, schema)));
        }
    }

    /// Carried vertices whose properties the part starting at this MATCH
    /// reads (`a.p`) although its own pattern does not bind them, in
    /// first-read order. Predicate reads end at the next top-level WITH,
    /// RETURN or set operation. RETURN reads the carried identity directly;
    /// re-matching it would discard a NULL carried from an earlier OPTIONAL.
    /// A part that names a path, a node binding of the same name
    /// and a comprehension or quantifier local are left to the ordinary
    /// compiler. Nothing is consumed.
    fn carried_property_reads(
        &self,
        incoming: &RowSchema<'a>,
    ) -> Result<Vec<Name<'a>>, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut previous = self.current.kind;
        let mut depth = 0_usize;
        let mut reads: Vec<Name<'a>> = Vec::new();
        let mut excluded: Vec<&'a str> = Vec::new();
        loop {
            let token = lexer.next()?;
            let after_dot = matches!(previous, TokenKind::Punct(b'.'));
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else {
                        break;
                    };
                    depth = outer;
                }
                TokenKind::Word(word)
                    if depth == 0
                        && !after_dot
                        && (["RETURN", "UNION", "EXCEPT", "INTERSECT"]
                            .iter()
                            .any(|clause| word.eq_ignore_ascii_case(clause))
                            || (word.eq_ignore_ascii_case("WITH")
                                && !matches!(previous, TokenKind::Word(previous)
                                    if previous.eq_ignore_ascii_case("STARTS")
                                        || previous.eq_ignore_ascii_case("ENDS")))) =>
                {
                    break;
                }
                TokenKind::Word(word) if !after_dot => {
                    let next = lexer.clone().next()?.kind;
                    if depth == 0 && matches!(next, TokenKind::Punct(b'=')) {
                        if matches!(previous, TokenKind::Word(clause)
                            if clause.eq_ignore_ascii_case("MATCH"))
                        {
                            return Ok(Vec::new());
                        }
                    } else if (matches!(previous, TokenKind::Punct(b'('))
                        && matches!(next, TokenKind::Punct(b')' | b':' | b'{')))
                        || (matches!(previous, TokenKind::Punct(b'[' | b'('))
                            && matches!(next, TokenKind::Word(next)
                                if next.eq_ignore_ascii_case("IN")))
                    {
                        // A later part's own binding, `(m)`, `(m:L)` or
                        // `(m {..})`, or a local `[x IN` / `(x IN`.
                        excluded.push(word);
                    } else if matches!(next, TokenKind::Punct(b'.'))
                        && incoming.iter().any(|(name, kind)| {
                            name.text == word && *kind == GraphSetColumnType::Vertex
                        })
                        && reads.iter().all(|read| read.text != word)
                    {
                        reads.push(Name {
                            text: word,
                            at: token.at,
                        });
                    }
                }
                _ => {}
            }
            previous = token.kind;
        }
        reads.retain(|read| !excluded.contains(&read.text));
        Ok(reads)
    }

    pub(in crate::graph_text) fn take_part_syntax(
        &mut self,
    ) -> Result<Syntax<'a>, GraphPatternTextError> {
        // Reset only graph-local bindings. The live lexer, declaration map and
        // definition-wide predicate/edge/identity counters are never reset.
        let mut next: Syntax<'a> = Parser::new("")?.syntax;
        next.element_catalog = self.syntax.element_catalog.clone();
        next.parameters.clone_from(&self.syntax.parameters);
        next.parameter_offsets
            .clone_from(&self.syntax.parameter_offsets);
        Ok(core::mem::replace(&mut self.syntax, next))
    }

    #[allow(clippy::type_complexity)]
    fn multipart_graph_part(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        kind: Option<RowJoinKind>,
        aggregate: bool,
        call_depth: usize,
    ) -> Result<
        (
            UnresolvedGraphText<'a>,
            RowSchema<'a>,
            bool,
            Option<RowJoinSpec>,
        ),
        GraphSetTextError,
    > {
        let at = self.current.at;
        let optional = kind == Some(RowJoinKind::Left);
        let optional_body = core::mem::take(&mut self.optional_body);
        self.read_row_bindings = incoming.iter().map(|(name, _)| *name).collect();
        self.read_comparisons = Some(Vec::new());
        // A carried vertex whose property a required part reads (`WITH a
        // MATCH (b) WHERE a.p < b.p`) re-matches by identity, exactly as the
        // name-reuse form `MATCH (a), (b)` does: the key loop below
        // correlates it with the carried column (fgdb-dhcfo). Not in an
        // OPTIONAL part, where a read through the re-matched vertex would be
        // NULL on a null-extended row although the carried vertex is not.
        if kind == Some(RowJoinKind::Inner) {
            for name in self.carried_property_reads(incoming)? {
                self.capacity(
                    self.syntax.variables.len(),
                    crate::algebra::MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Vertices,
                )?;
                self.syntax.variables.push(name);
            }
        }
        // `OPTIONAL MATCH (a) OPTIONAL MATCH (b)` left-joins clause by
        // clause: this part ends before the next clause, at an implicit
        // `WITH *` (multipart_head), never wrapping it in one left join.
        self.optional_part = optional;
        // The first part's WHERE over values is that part's implicit `WITH *
        // WHERE` (fgdb-1c29d, fgdb-qo3jv). A continuation's WHERE stays its
        // own: an OPTIONAL one belongs to the left join, not after it.
        self.defer_where = kind.is_none();
        let parsed = self.parse_match_prefix();
        self.defer_where = false;
        let deferred = self.deferred_where.take();
        self.optional_part = false;
        let row_fields = core::mem::take(&mut self.read_row_fields);
        let row_comparisons = self.read_comparisons.take().unwrap_or_default();
        parsed?;
        self.refuse_call_write(call_depth)?;
        // An OPTIONAL clause's WHERE over the incoming row filters inside
        // its optional Apply (multipart_optional_apply). After the left join
        // it would drop the null-extended rows.
        if optional && deferred.is_some() {
            return Err(expected(
                at,
                "an OPTIONAL MATCH WHERE inside its optional Apply",
            ));
        }
        if optional_body && deferred.is_none() {
            return Err(expected(at, "the optional Apply's deferred WHERE"));
        }
        // Wrapping several independently scoped clauses in one left join
        // would move their null-extension boundary. Keep this continuation a
        // positive pattern; another WITH may begin the next required/optional
        // part. Existing required-part scoped semantics are unchanged.
        if optional && !self.syntax.scopes.is_empty() {
            return Err(expected(
                at,
                "WITH between an OPTIONAL continuation and scoped clauses",
            ));
        }
        let mut inputs = Vec::new();
        let mut keys = Vec::new();
        for (left, &(name, kind)) in incoming.iter().enumerate() {
            // An imported name cannot silently become an independent local
            // inside EXISTS/OPTIONAL. This subset correlates root vertices;
            // the ordinary scoped compiler can then capture that root value.
            let in_root = self.syntax.variables[..self.syntax.root_variables]
                .iter()
                .any(|variable| variable.text == name.text);
            if !in_root
                && self.syntax.scopes.iter().any(|scope| {
                    scope
                        .find_name(&|variable| variable.text == name.text)
                        .is_some()
                })
            {
                return Err(expected(name.at, "imported vertices in the root MATCH"));
            }
            if let Some(right) = self
                .syntax
                .variables
                .iter()
                .position(|var| var.text == name.text)
            {
                if right >= self.syntax.root_variables {
                    return Err(expected(name.at, "imported vertices in the root MATCH"));
                }
                if !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Any) {
                    return Err(expected(name.at, "a vertex-valued imported binding"));
                }
                let right =
                    self.mutation_projection(&mut inputs, self.syntax.variables[right], None)?;
                keys.push((left, right));
            }
            if self.syntax.names_path(name.text) {
                return Err(expected(name.at, "new path bindings after WITH"));
            }
            // A carried relationship correlates by identity, as a carried
            // vertex does (openCypher: `WITH r MATCH (a)-[r]->(b)` matches
            // that one relationship again). It must be one root edge.
            let edge = self
                .syntax
                .visible_edge(name.text)
                .map(|(edge, scoped)| (edge.variable, edge.walk.is_some() || scoped));
            if let Some((variable, unsupported)) = edge {
                let Some(variable) = variable.filter(|_| !unsupported) else {
                    return Err(expected(
                        name.at,
                        "a carried relationship as one root edge of the next MATCH",
                    ));
                };
                if kind != GraphSetColumnType::Edge {
                    return Err(expected(name.at, "an edge-valued imported binding"));
                }
                let right = self.mutation_projection(&mut inputs, variable, None)?;
                keys.push((left, right));
            }
        }
        // The current property-map correlation record has no scope tag. Do
        // not accidentally move an OPTIONAL/EXISTS predicate onto the outer
        // required join. Root-only maps remain supported without ambiguity.
        if !self.read_correlations.is_empty() && !self.syntax.scopes.is_empty() {
            return Err(expected(at, "root-only imported property-map correlations"));
        }
        if !row_comparisons.is_empty() && !self.syntax.scopes.is_empty() {
            return Err(expected(at, "root-only imported MATCH comparisons"));
        }
        for (variable, key, left) in core::mem::take(&mut self.read_correlations) {
            let right = self.mutation_projection(&mut inputs, variable, key)?;
            if !keys.contains(&(left, right)) {
                keys.push((left, right));
            }
        }
        let mut comparisons = Vec::new();
        for comparison in row_comparisons {
            let right =
                self.mutation_projection(&mut inputs, comparison.variable, comparison.property)?;
            comparisons.push((comparison.row, right, comparison.comparison));
        }
        // An aggregate terminal addresses completed row aliases, not graph
        // slots. A continuation's own RETURN reads every visible binding, so
        // it passes through the `WITH *` boundary openCypher gives it: the
        // incoming columns plus this MATCH's bindings, nothing invented. The
        // first part still needs its explicit boundary before the next MATCH.
        // An UNWIND directly after a continuation reads that same row (TCK
        // Graph8 [4]: `OPTIONAL MATCH (n) UNWIND keys(n) AS x RETURN ...`).
        let before_call = self.inline_call()?;
        let before_existence =
            self.returning_existence_projection()? || self.row_list_projection()?;
        let before_write = self.at_multipart_write();
        // An inline CALL body's aggregating RETURN reads every binding at an
        // implicit `WITH *` and aggregates those rows, once per outer row.
        let body_aggregate = self.call_body && self.return_has_aggregate()?;
        let implicit_star = kind.is_some()
            && deferred.is_none()
            && (((aggregate || body_aggregate) && self.is_word("RETURN"))
                || self.is_word("UNWIND")
                || before_call
                || before_existence);
        if aggregate
            && !implicit_star
            && deferred.is_none()
            && !self.is_word("WITH")
            && !before_call
            && !before_existence
            && !before_write
            && !(kind.is_none() && self.is_word("UNWIND"))
            && !self.implicit_withs.contains(&self.current.at)
        {
            return Err(expected(
                self.current.at,
                "WITH before a multipart aggregate RETURN",
            ));
        }
        let mut head = if let Some((lexer, at_where)) = deferred {
            // The row pipeline reads the WHERE after the implicit `WITH *`,
            // then this part ends at the next MATCH, as after an explicit one.
            // A continuation's `WITH *` also keeps its incoming columns.
            let head = if kind.is_none() {
                self.implicit_with_head()?
            } else {
                let outputs = self.star_outputs(incoming, &mut inputs)?;
                GraphProjectionHead {
                    with: true,
                    plain: false,
                    inputs,
                    outputs,
                }
            };
            self.lexer = lexer;
            self.current = at_where;
            head
        } else if kind.is_none() {
            if before_call || body_aggregate || before_existence || before_write {
                self.implicit_with_head()?
            } else {
                self.graph_projection_head()?
            }
        } else if implicit_star {
            // The same graph-to-row aggregate head owns carried properties
            // in WITH and in an implicit WITH * before aggregate RETURN.
            self.with_graph_head(incoming, inputs, optional)?
        } else {
            self.multipart_head(incoming, inputs, optional)?
        };
        // graph_projection_head numbers its graph inputs from zero; a
        // continuation head places them after the incoming row columns.
        let offset = if kind.is_none() { 0 } else { incoming.len() };
        // The `WITH *` columns, before any hidden read.
        let visible = head.outputs.len();
        if !implicit_star {
            // A write after this part reads its carried row identities'
            // preimages, as the leading-row heads do (fgdb-cpny9); a read
            // keeps its own templates.
            let imports: &[_] = if self.write_prefix && !self.call_body {
                incoming
            } else {
                &[]
            };
            self.hoist_boundary_reads_with_imports(&mut head, offset, imports)?;
        }
        let graph_page = if head.with {
            None
        } else {
            self.graph_expression_page(&mut head, incoming, optional)?
        };
        if head.inputs.is_empty() {
            // A constant projection still emits once per graph occurrence.
            self.mutation_projection(&mut head.inputs, self.syntax.variables[0], None)?;
        }
        let mut types: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
        types.extend(head.inputs.iter().map(projection_type));
        admit_head(&head, &types, &self.syntax.parameters)?;
        let schema: RowSchema<'a> = head
            .outputs
            .iter()
            .map(|(name, value)| (*name, value.column_type(&types, &self.syntax.parameters)))
            .collect();
        // A continuation joins its incoming relation even when its schema is
        // empty; column count is not evidence that there was no earlier part.
        let join = if let Some(kind) = kind {
            let mut left: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
            if !row_fields.is_empty() {
                // Private ON operands have the same pre-catalog domain
                // admission as public outputs, against the real input schema.
                admit_values(
                    row_fields.iter().map(|value| (at, value)),
                    &left,
                    &self.syntax.parameters,
                )?;
            }
            let field_types: Vec<_> = row_fields
                .iter()
                .map(|value| value.column_type(&left, &self.syntax.parameters))
                .collect();
            left.extend(field_types);
            Some(join_spec(&left, &head.inputs, &keys, &comparisons, at)?.with_kind(kind))
        } else {
            None
        };
        let mut terminal = !head.with;
        let (mut pipeline, mut next) = if optional_body {
            // The optional Apply's body ends with its WHERE and exports the
            // clause's own bindings; the clauses after it are the caller's.
            let at = self.current.at;
            let mut stages = Vec::new();
            let mut depth = 2;
            self.word("WHERE")?;
            self.row_selection(&schema, &mut stages, &mut depth, at)?;
            let exports: RowSchema<'a> = schema[incoming.len()..visible].to_vec();
            stages.push(ReadStageTemplate::Project {
                at,
                projection: (incoming.len()..visible)
                    .map(|column| ReadProjectionTemplate {
                        name: schema[column].0.text.to_owned(),
                        value: ReadValueTemplate::Column(column),
                    })
                    .collect(),
                quantifier: GraphSetQuantifier::All,
            });
            self.boundary_reads = None;
            self.projected_sources = None;
            terminal = true;
            (stages, exports)
        } else if head.with {
            let (pipeline, schema, _) = self.row_pipeline_prefix(schema)?;
            (pipeline, schema)
        } else {
            (graph_page.into_iter().collect(), schema)
        };
        // The body's RETURN may follow its own WITH stages, so ask again here.
        if !terminal
            && self.call_body
            && self.return_has_aggregate()?
            && !self.row_existence_projection(next.len())?
        {
            let (stage, schema) = self.return_aggregate_stage(&next)?;
            pipeline.push(stage);
            if let Some(page) = self.row_expression_page(&schema)? {
                pipeline.push(page);
            }
            self.boundary_reads = None;
            self.projected_sources = None;
            next = schema;
            terminal = true;
        } else if !terminal
            && self.is_word("RETURN")
            && !aggregate
            && !self.row_existence_projection(next.len())?
        {
            let at = self.current.at;
            self.advance()?;
            let distinct = self.take_word("DISTINCT")?;
            if !distinct {
                self.take_all_quantifier()?;
            }
            let (mut projection, schema) = self.row_projection(&next, true)?;
            let page = self.return_expression_page(&next, &mut projection, distinct)?;
            // The terminal RETURN is the last reader of any hidden boundary read.
            self.boundary_reads = None;
            pipeline.push(ReadStageTemplate::Project {
                at,
                projection,
                quantifier: if distinct {
                    GraphSetQuantifier::Distinct
                } else {
                    GraphSetQuantifier::All
                },
            });
            if let Some(page) = page {
                pipeline.push(page);
            }
            next = schema;
            terminal = true;
        }
        if aggregate && self.is_word("RETURN") && !self.row_existence_projection(next.len())? {
            terminal = true;
        }
        terminal |= self.at_multipart_write();
        if !row_fields.is_empty() {
            self.capacity(
                incoming.len() + head.inputs.len() + row_fields.len() - 1,
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
        }
        // The surrounding set parser owns terminal ORDER BY/SKIP/LIMIT. Every
        // page before a continuation was already consumed by row_pipeline_prefix.
        self.syntax.columns = head
            .inputs
            .into_iter()
            .map(|input| Column {
                variable: input.variable,
                property: input.property,
                path: input.path,
                alias: input.variable,
                collect: input.collect,
            })
            .collect();
        let projection = Some(
            head.outputs
                .into_iter()
                .map(|(name, value)| ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value,
                })
                .collect(),
        );
        let syntax = self.take_part_syntax()?;
        Ok((
            UnresolvedGraphText {
                statement,
                syntax,
                projection,
                pipeline,
                singleton: false,
                leading: Vec::new(),
                leading_types: incoming.iter().map(|(_, kind)| *kind).collect(),
                correlations: keys,
                row_fields,
            },
            next,
            terminal,
            join,
        ))
    }

    fn multipart_head(
        &mut self,
        incoming: &RowSchema<'a>,
        mut inputs: Vec<Projection<'a>>,
        optional: bool,
    ) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        // An OPTIONAL part that stopped at the next clause, or any part that
        // stopped at an implicit boundary (implicit_withs): `WITH *`.
        let implicit = (optional && (self.is_word("OPTIONAL") || self.is_word("MATCH")))
            || self.implicit_withs.contains(&self.current.at)
            || self.at_multipart_write();
        if !implicit && self.with_has_aggregate()? {
            return self.with_graph_head(incoming, inputs, optional);
        }
        let with = implicit || self.take_word("WITH")?;
        if !with {
            self.word("RETURN")?;
        }
        if !implicit {
            self.syntax.distinct = self.take_word("DISTINCT")?;
            if !self.syntax.distinct {
                self.take_all_quantifier()?;
            }
        }
        let width = incoming.len();
        let mut outputs = Vec::new();
        if implicit || self.take(b'*')? {
            outputs = self.star_outputs(incoming, &mut inputs)?;
            // openCypher orders RETURN *'s columns by name (fgdb-7jja3);
            // WITH *, written or implicit, keeps the row's order.
            if !with {
                outputs.sort_by(|(a, _), (b, _)| a.text.cmp(b.text));
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
                let value = self.read_resolved_item(&mut |parser| {
                    let TokenKind::Word(word) = parser.current.kind else { return Ok(None); };
                    let next = parser.lexer.clone().next()?;
                    if (word.eq_ignore_ascii_case("properties") || word.eq_ignore_ascii_case("keys"))
                        && let Some(argument) = super::property_map_source(word, parser.current.at, &parser.lexer)?
                        && parser.visible_graph_bindings().any(|name| name.text == argument.text) {
                        if optional && incoming.iter().any(|(binding, _)| binding.text == argument.text) {
                            return Err(error(argument.at, GraphPatternTextErrorKind::Expected("project carried graph maps before OPTIONAL MATCH")));
                        }
                        return parser.property_map_function(&mut inputs).map(|column| Some(width + column));
                    }
                    if matches!(next.kind, TokenKind::Punct(b'(')) {
                        if ["labels", "type", "length", "path_length", "nodes", "edges", "relationships"]
                            .iter().any(|name| word.eq_ignore_ascii_case(name))
                        {
                            // A graph-function read of an imported value is
                            // not a read of the nullable right-hand copy. Until
                            // that read has its own source, require a saved
                            // left-hand expression instead of returning NULL.
                            if optional {
                                let mut lookahead = parser.lexer.clone();
                                lookahead.next()?; // opening parenthesis
                                let argument = lookahead.next()?;
                                if matches!(argument.kind, TokenKind::Word(name)
                                    if incoming.iter().any(|(binding, _)| binding.text == name))
                                {
                                    return Err(error(argument.at, GraphPatternTextErrorKind::Expected(
                                        "project carried graph functions before OPTIONAL MATCH",
                                    )));
                                }
                            }
                            let Operand::Column(column) = parser.mutation_operand(&mut inputs)? else {
                                unreachable!("graph function resolves a source column");
                            };
                            return Ok(Some(width + column));
                        }
                        return Ok(None);
                    }
                    if let Some(column) = incoming.iter().position(|(name, _)| name.text == word)
                    {
                        parser.advance()?;
                        return Ok(Some(column));
                    }
                    let graph = parser.visible_graph_bindings().any(|name| name.text == word);
                    if !graph { return Ok(None); }
                    let variable = parser.any_variable()?;
                    let property = if parser.take(b'.')? {
                        parser.require_property_variable(variable)?;
                        Some(parser.schema_name()?)
                    } else { None };
                    let column = parser.mutation_projection(&mut inputs, variable, property)?;
                    Ok(Some(width + column))
                }, 0)?;
                let end = self.current.at;
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if with {
                    implicit
                        .filter(|(_, boundary)| {
                            *boundary == end && matches!(value, ReadValueTemplate::Column(_))
                        })
                        .map(|(name, _)| name)
                        .ok_or_else(|| expected(at, "AS alias for a computed row value"))?
                } else {
                    // An unaliased RETURN item is named by its exact text.
                    self.source_name(at, end)
                };
                if outputs
                    .iter()
                    .any(|(name, _): &(Name<'a>, ReadValueTemplate)| name.text == alias.text)
                {
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
        }
        Ok(GraphProjectionHead {
            with,
            plain: false,
            inputs,
            outputs,
        })
    }

    /// `WITH *`: every incoming column, then each visible graph binding the
    /// incoming row does not already carry.
    fn star_outputs(
        &mut self,
        incoming: &RowSchema<'a>,
        inputs: &mut Vec<Projection<'a>>,
    ) -> Result<Vec<(Name<'a>, ReadValueTemplate)>, GraphSetTextError> {
        let width = incoming.len();
        let mut outputs: Vec<_> = incoming
            .iter()
            .enumerate()
            .map(|(index, &(name, _))| (name, ReadValueTemplate::Column(index)))
            .collect();
        let visible: Vec<_> = self.visible_graph_bindings().collect();
        for variable in visible {
            if incoming.iter().any(|(name, _)| name.text == variable.text) {
                continue;
            }
            self.capacity(
                outputs.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let column = self.mutation_projection(inputs, variable, None)?;
            outputs.push((variable, ReadValueTemplate::Column(width + column)));
        }
        Ok(outputs)
    }
}

impl BoundReadInput {
    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let values = self.checked_arguments(arguments)?;
        self.bind_values(&values)
    }

    pub(crate) fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphPatternTextError> {
        self.first.checked_arguments(arguments)
    }

    /// Only a caller that checked the complete shared argument map may enter.
    /// Both ordinary multipart reads and exact aggregation bind this same tree.
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut input = self.first.bind_values(values)?;
        for part in &self.continuations {
            let source = &part.input;
            if matches!(
                &part.operation,
                BoundContinuationOperation::Pattern { .. }
                    | BoundContinuationOperation::ExistsBody { .. }
            ) {
                let at = source.return_at;
                let names = input.columns().to_vec();
                let width = names.len();
                let witness_name = (0..=width)
                    .map(|candidate| {
                        if candidate == 0 {
                            "__fg_exists_witness".to_owned()
                        } else {
                            format!("__fg_exists_witness_{candidate}")
                        }
                    })
                    .find(|candidate| !names.contains(candidate))
                    .expect("one more private candidate than outer column names");
                let (body, imports, result) = match &part.operation {
                    BoundContinuationOperation::Pattern {
                        pattern,
                        join,
                        result,
                    } => {
                        let arguments = PreparedGraphSet::arguments(
                            names.clone(),
                            input.column_types().to_vec(),
                        )
                        .map_err(|kind| GraphSetTextError {
                            offset: at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        })?;
                        let body = arguments
                            .join(pattern.bind_values(values)?, join.clone())
                            .map_err(|kind| GraphSetTextError {
                                offset: at,
                                kind: GraphSetTextErrorKind::SetBuild(kind),
                            })?;
                        (body, (0..width).collect(), *result)
                    }
                    BoundContinuationOperation::ExistsBody {
                        body,
                        imports,
                        negated,
                    } => (
                        body.bind_values(values)?,
                        imports.clone(),
                        match *negated {
                            Some(negated) => RowPatternResult::Filter { negated },
                            None => RowPatternResult::Exists,
                        },
                    ),
                    _ => unreachable!("graph expression source checked above"),
                };
                let body = if matches!(result, RowPatternResult::Count) {
                    // Native keyless grouping returns one checked Int64 count,
                    // including zero for an empty witness relation. Counting
                    // rows keeps parallel witnesses and never collects a list.
                    body.group_by(&[], &[crate::GraphAggregate::count_rows(&witness_name)])
                        .map_err(|kind| GraphSetTextError {
                            offset: at,
                            kind: GraphSetTextErrorKind::AggregateBuild(kind),
                        })?
                } else if let RowPatternResult::Collect { column } = result {
                    // The graph source already collected this opaque list in
                    // native witness order. Joining/sorting outer rows must
                    // never reorder its entries or drop NULL placeholders.
                    body.project_ordered_output(vec![GraphSetProjection::new(
                        witness_name,
                        GraphSetValue::Column(width + column),
                    )])
                    .map_err(|kind| GraphSetTextError {
                        offset: at,
                        kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                    })?
                } else {
                    let witness = GqlScalarParameter::new(CanonicalScalar::Bool(true))
                        .expect("true is a bounded scalar");
                    // This profile evaluates every governed witness before
                    // paging. Late source/budget failures remain failures even
                    // when an earlier witness already proved existence.
                    body.project_ordered_output(vec![GraphSetProjection::new(
                        witness_name,
                        GraphSetValue::Literal(witness),
                    )])
                    .map_err(|kind| GraphSetTextError {
                        offset: at,
                        kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                    })?
                    .with_page(0, Some(1))
                };
                // Each outer occurrence gains exactly one count or optional
                // Boolean witness. Returning bodies determine absence after
                // their own aggregate/filter/page stages, including a global
                // aggregate's one row over an empty graph witness relation.
                input = if matches!(result, RowPatternResult::Count) {
                    input.apply(body, imports)
                } else {
                    input.optional_apply(body, imports)
                }
                .map_err(|kind| GraphSetTextError {
                    offset: at,
                    kind: GraphSetTextErrorKind::SetBuild(kind),
                })?;
                if let RowPatternResult::Filter { negated } = result {
                    input = input
                        .filter(&[GraphSetPredicateOp::IsNull {
                            operand: GraphSetOperand::Column(width),
                            is_null: negated,
                        }])
                        .map_err(|kind| GraphSetTextError {
                            offset: at,
                            kind: GraphSetTextErrorKind::FilterBuild(kind),
                        })?
                        .project_ordered_output(
                            names
                                .into_iter()
                                .enumerate()
                                .map(|(column, name)| {
                                    GraphSetProjection::new(name, GraphSetValue::Column(column))
                                })
                                .collect(),
                        )
                        .map_err(|kind| GraphSetTextError {
                            offset: at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        })?;
                } else {
                    // The private projection normalizes true/NULL for EXISTS
                    // or carries the native count, retaining every input cell.
                    let columns = source
                        .projection
                        .as_ref()
                        .expect("row graph value projection")
                        .iter()
                        .map(|output| {
                            super::super::bind_read_value(&output.value, values)
                                .map(|value| GraphSetProjection::new(output.name.clone(), value))
                        })
                        .collect::<Result<_, _>>()?;
                    input = input.project_ordered_output(columns).map_err(|kind| {
                        GraphSetTextError {
                            offset: at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        }
                    })?;
                }
                input = super::super::bind_stages(input, &source.pipeline, values)?;
                continue;
            }
            if let BoundContinuationOperation::List {
                body,
                imports,
                list_column,
            } = &part.operation
            {
                input = super::super::bind_stages(input, &source.leading, values)?;
                input = input
                    .collect_apply(body.bind_values(values)?, imports.clone(), *list_column)
                    .map_err(|kind| GraphSetTextError {
                        offset: source.return_at,
                        kind: GraphSetTextErrorKind::SetBuild(kind),
                    })?;
                let columns = source
                    .projection
                    .as_ref()
                    .expect("list expression projection")
                    .iter()
                    .map(|output| {
                        super::super::bind_read_value(&output.value, values)
                            .map(|value| GraphSetProjection::new(output.name.clone(), value))
                    })
                    .collect::<Result<_, _>>()?;
                input =
                    input
                        .project_ordered_output(columns)
                        .map_err(|kind| GraphSetTextError {
                            offset: source.return_at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        })?;
                input = super::super::bind_stages(input, &source.pipeline, values)?;
                continue;
            }
            if let BoundContinuationOperation::Apply {
                body,
                imports,
                optional,
            } = &part.operation
            {
                let body = body.bind_values(values)?;
                input = if *optional {
                    input.optional_apply(body, imports.clone())
                } else {
                    input.apply(body, imports.clone())
                }
                .map_err(|kind| GraphSetTextError {
                    offset: source.return_at,
                    kind: GraphSetTextErrorKind::SetBuild(kind),
                })?;
                if let Some(projection) = &source.projection {
                    input = super::super::bind_projection(
                        input,
                        projection,
                        source.quantifier,
                        values,
                        source.return_at,
                    )?;
                }
                input = super::super::bind_stages(input, &source.pipeline, values)?;
                continue;
            }
            let BoundContinuationOperation::Match(join) = &part.operation else {
                unreachable!("Apply and existence filters handled above")
            };
            let imported_width = input.column_types().len();
            input = source.bind_row_fields(input, values)?;
            let left_width = input.column_types().len();
            let right = source
                .selection
                .as_ref()
                .expect("continuations are graph sources")
                .bind_values(values)?;
            input = input
                .join(right.into(), join.clone())
                .map_err(|kind| GraphSetTextError {
                    offset: source.return_at,
                    kind: GraphSetTextErrorKind::SetBuild(kind),
                })?;
            if !source.row_fields.is_empty() {
                // ON may address private computed LEFT keys. Remove them
                // before public projection/DISTINCT, restoring the original
                // incoming + graph ordinals without changing bag occurrences.
                let projection = (0..imported_width)
                    .chain(left_width..input.column_types().len())
                    .enumerate()
                    .map(|(output, column)| {
                        GraphSetProjection::new(
                            format!("__fg_match_input_{output}"),
                            GraphSetValue::Column(column),
                        )
                    })
                    .collect();
                input =
                    input
                        .project_ordered_output(projection)
                        .map_err(|kind| GraphSetTextError {
                            offset: source.return_at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        })?;
            }
            if let Some(projection) = &source.projection {
                input = super::super::bind_projection(
                    input,
                    projection,
                    source.quantifier,
                    values,
                    source.return_at,
                )?;
            }
            input = super::super::bind_stages(input, &source.pipeline, values)?;
        }
        Ok(input)
    }
}
