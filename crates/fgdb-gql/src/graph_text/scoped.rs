//! Ordered MATCH parsing and lowering through the existing positive-pattern compiler.
//! The lexer and numeric argument table are shared with ordinary and aggregate
//! text. Required MATCH and OPTIONAL export names; existential names stay local.
//! Bodies without shared variables are independent, not malformed correlations.
//! Child WHERE may read visible outer vertices absent from its positive pattern;
//! those values are captured with null intact, not introduced as node matches.
//! MATCH ANY SHORTEST WALK selects one occurrence per endpoint pair; MATCH ALL
//! SHORTEST WALK keeps every tie. Both require one finite quantified atom in
//! each selected positive pattern. They share all ordinary MATCH consumers and
//! never imply a global DISTINCT. An explicit root binding captures the path.

mod mutation;

use super::*;
use crate::algebra::{GraphMatchClause, GraphWalkSearch};

#[derive(Clone, Copy)]
enum ScopeKind {
    Required,
    Optional,
    Exists,
    NotExists,
}

impl ScopeKind {
    fn exports_bindings(self) -> bool {
        matches!(self, Self::Required | Self::Optional)
    }
}

struct PatternSyntax<'a> {
    variables: Vec<Name<'a>>,
    captures: Vec<Name<'a>>,
    labels: Vec<(Name<'a>, Name<'a>)>,
    edges: Vec<Edge<'a>>,
    filters: Vec<Filter<'a>>,
}

pub(super) struct ScopeSyntax<'a> {
    kind: ScopeKind,
    body: PatternSyntax<'a>,
}

#[derive(Clone)]
pub(super) struct BoundScope {
    kind: ScopeKind,
    builder: GraphPatternBuilder,
    filters: Vec<BoundFilter>,
}

impl BoundScope {
    pub(super) fn clause(&self) -> GraphMatchClause<'_> {
        match self.kind {
            ScopeKind::Required => GraphMatchClause::required(&self.builder),
            ScopeKind::Optional => GraphMatchClause::optional(&self.builder),
            ScopeKind::Exists => GraphMatchClause::exists(&self.builder),
            ScopeKind::NotExists => GraphMatchClause::not_exists(&self.builder),
        }
    }

    pub(super) fn bind_values(
        &self,
        values: &[GqlParameterValue],
        at: usize,
    ) -> Result<Self, GraphPatternTextError> {
        Ok(Self {
            kind: self.kind,
            builder: bind_builder(&self.builder, &self.filters, values, at)?,
            filters: Vec::new(),
        })
    }
}

impl<'a> ScopeSyntax<'a> {
    pub(super) fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<BoundScope, GraphPatternTextError> {
        let (builder, filters) = resolve_pattern_with_captures(
            &self.body.variables,
            &self.body.captures,
            &self.body.labels,
            &self.body.edges,
            self.body.filters,
            symbol,
        )?;
        Ok(BoundScope {
            kind: self.kind,
            builder,
            filters,
        })
    }
}

/// Root and scoped bodies use the same schema-bound construction. Numeric
/// predicates retain operands for rebinding instead of entering a second AST
/// interpreter or re-resolving a catalog on every execution.
pub(super) fn resolve_pattern<'a>(
    variables: &[Name<'a>],
    labels: &[(Name<'a>, Name<'a>)],
    edges: &[Edge<'a>],
    filters: Vec<Filter<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<(GraphPatternBuilder, Vec<BoundFilter>), GraphPatternTextError> {
    resolve_pattern_with_captures(variables, &[], labels, edges, filters, symbol)
}

fn resolve_pattern_with_captures<'a>(
    variables: &[Name<'a>],
    captures: &[Name<'a>],
    labels: &[(Name<'a>, Name<'a>)],
    edges: &[Edge<'a>],
    filters: Vec<Filter<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<(GraphPatternBuilder, Vec<BoundFilter>), GraphPatternTextError> {
    let mut builder = GraphPatternBuilder::new();
    for name in variables {
        let declared = if captures.iter().any(|capture| capture.text == name.text) {
            builder.outer_vertex(name.text)
        } else {
            builder.vertex(name.text)
        };
        built(name.at, declared)?;
    }
    for &(variable, label) in labels {
        let GraphSymbol::Label(label_id) = symbol(GraphSymbolKind::Label, label)? else {
            unreachable!("symbol domain is checked by the shared resolver")
        };
        built(
            variable.at,
            builder.filter(variable.text, VertexPredicate::HasLabel(label_id)),
        )?;
    }
    for edge in edges {
        let GraphSymbol::Relation(relation) = symbol(GraphSymbolKind::Relation, edge.relation)?
        else {
            unreachable!("symbol domain is checked by the shared resolver")
        };
        let result = match edge.walk {
            Some(bounds) if edge.search == GraphWalkSearch::AllShortest => builder.shortest_walk(
                edge.source.text,
                relation,
                edge.direction,
                edge.destination.text,
                bounds,
            ),
            Some(bounds) if edge.search == GraphWalkSearch::AnyShortest => builder.any_shortest_walk(
                edge.source.text,
                relation,
                edge.direction,
                edge.destination.text,
                bounds,
            ),
            Some(bounds) => builder.walk(
                edge.source.text,
                relation,
                edge.direction,
                edge.destination.text,
                bounds,
            ),
            None => builder.edge(
                edge.source.text,
                relation,
                edge.direction,
                edge.destination.text,
            ),
        };
        built(edge.relation.at, result)?;
    }
    let mut numeric = Vec::new();
    for filter in filters {
        match filter {
            Filter::PathCapture(name) => {
                built(name.at, builder.capture_path(name.text))?;
            }
            Filter::PathLength { variable, comparison, value } => {
                numeric.push(BoundFilter::PathLength {
                    variable: variable.text.to_owned(), comparison, value,
                });
            }
            Filter::PathNull { variable, function, is_null } => {
                built(variable.at, builder.filter_path_null(variable.text, function, is_null))?;
            }
            Filter::Boolean { program, at } => {
                numeric.push(BoundFilter::Boolean(
                    boolean::BoundBooleanTemplate::resolve(program, at, symbol)?,
                ));
            }
            filter @ Filter::VertexNull { .. } => {
                numeric.push(BoundFilter::Boolean(
                    boolean::BoundBooleanTemplate::resolve(
                        vec![boolean::SyntaxItem::Atom(filter)],
                        0,
                        symbol,
                    )?,
                ));
            }
            Filter::Properties {
                left,
                left_key,
                right,
                right_key,
                comparison,
            } => {
                let GraphSymbol::Property(left_key) = symbol(GraphSymbolKind::Property, left_key)?
                else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                let GraphSymbol::Property(right_key) =
                    symbol(GraphSymbolKind::Property, right_key)?
                else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                built(
                    left.at,
                    builder
                        .compare_properties(left.text, left_key, comparison, right.text, right_key),
                )?;
            }
            Filter::Identity { left, right, equal } => {
                built(left.at, builder.identity(left.text, right.text, equal))?;
            }
            Filter::Property {
                variable,
                key,
                comparison,
                value,
            } => {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                numeric.push(BoundFilter::Property {
                    variable: variable.text.to_owned(),
                    key,
                    comparison,
                    value,
                });
            }
            Filter::Scalar {
                variable,
                key,
                predicate,
            } => {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                built(
                    variable.at,
                    builder.filter(
                        variable.text,
                        VertexPredicate::ScalarProperty { key, predicate },
                    ),
                )?;
            }
            Filter::Null {
                variable,
                key,
                is_null,
            } => {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                built(
                    variable.at,
                    builder.filter(
                        variable.text,
                        VertexPredicate::PropertyNull { key, is_null },
                    ),
                )?;
            }
        }
    }
    Ok((builder, numeric))
}

pub(super) fn bind_builder(
    builder: &GraphPatternBuilder,
    filters: &[BoundFilter],
    values: &[GqlParameterValue],
    at: usize,
) -> Result<GraphPatternBuilder, GraphPatternTextError> {
    let mut builder = builder.clone();
    for filter in filters {
        if let BoundFilter::PathLength { variable, comparison, value } = filter {
            let GqlParameterValue::Int64(value) = value.value(values) else {
                unreachable!("path length arguments were type-checked at preparation")
            };
            built(at, builder.filter_path_length(variable, *comparison, value))?;
            continue;
        }
        let BoundFilter::Property {
            variable,
            key,
            comparison,
            value,
        } = filter
        else {
            let BoundFilter::Boolean(template) = filter else {
                unreachable!("closed filter domain")
            };
            let expression = template.bind(values)?;
            built(at, builder.filter_boolean(&expression))?;
            continue;
        };
        let predicate = match value.value(values) {
            GqlParameterValue::Int64(value) => VertexPredicate::IntegerProperty {
                key: *key,
                comparison: *comparison,
                value,
            },
            GqlParameterValue::Scalar(value) => VertexPredicate::ScalarProperty {
                key: *key,
                predicate: value.predicate(*comparison),
            },
            GqlParameterValue::UInt64(_) => {
                unreachable!("property arguments were type-checked at preparation")
            }
        };
        built(at, builder.filter(variable, predicate))?;
    }
    Ok(builder)
}

// The native parser has already validated every operand against the union of
// child-pattern names and visible outer names. Retain only actual captures,
// in first-reference order, not every visible name. This is preparation-only
// symbol collection over the SAME bounded predicate IR, never evaluation.
fn predicate_captures<'a>(
    variables: &mut Vec<Name<'a>>,
    filters: &[Filter<'a>],
) -> Result<Vec<Name<'a>>, GraphPatternTextError> {
    let mut captures = Vec::new();
    let mut pending: Vec<_> = filters.iter().rev().collect();
    while let Some(filter) = pending.pop() {
        let names = match filter {
            Filter::PathCapture(_) | Filter::PathLength { .. } | Filter::PathNull { .. } => {
                return Err(error(0, GraphPatternTextErrorKind::Expected("root path predicate")));
            }
            Filter::Boolean { program, .. } => {
                for item in program.iter().rev() {
                    if let boolean::SyntaxItem::Atom(atom) = item { pending.push(atom); }
                }
                continue;
            }
            Filter::Properties { left, right, .. } | Filter::Identity { left, right, .. } => {
                [Some(*left), Some(*right)]
            }
            Filter::VertexNull { variable, .. } | Filter::Property { variable, .. }
            | Filter::Scalar { variable, .. } | Filter::Null { variable, .. } => {
                [Some(*variable), None]
            }
        };
        for name in names.into_iter().flatten() {
            if variables.iter().any(|variable| variable.text == name.text) { continue; }
            if variables.len() == MAX_PATTERN_VERTICES {
                return Err(error(name.at, GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Vertices,
                    limit: MAX_PATTERN_VERTICES,
                    observed: variables.len() + 1,
                })));
            }
            variables.push(name);
            captures.push(name);
        }
    }
    Ok(captures)
}

impl<'a> Parser<'a> {
    pub(super) fn parse_scoped_head(&mut self) -> Result<(), GraphPatternTextError> {
        self.parse_match_prefix()?;
        self.word("RETURN")
    }

    /// Shared read/write MATCH prefix. Required and OPTIONAL clauses may be
    /// interleaved after the mandatory root; each retains its own predicates,
    /// search selector and position. Child predicates may capture visible outer
    /// values without matching those vertices. Nested existential bodies remain
    /// unsupported. A mutation attaches its own typed terminal clause.
    pub(super) fn parse_match_prefix(&mut self) -> Result<(), GraphPatternTextError> {
        self.word("MATCH")?;
        if self.starts_path_binding()? {
            let path = self.name()?;
            self.punct(b'=', "=")?;
            self.syntax.path = Some(path);
        }
        self.positive_pattern()?;
        self.syntax.root_variables = self.syntax.variables.len();
        if let Some(path) = self.syntax.path {
            self.syntax.filters.push(Filter::PathCapture(path));
        }
        if self.take_word("WHERE")? {
            self.scoped_predicates(true)?;
        }
        loop {
            if self.take_word("OPTIONAL")? {
                self.match_scope(ScopeKind::Optional)?;
            } else if self.is_word("MATCH") {
                self.match_scope(ScopeKind::Required)?;
            } else {
                break;
            }
        }
        self.syntax.return_at = self.current.at;
        Ok(())
    }

    fn starts_path_binding(&self) -> Result<bool, GraphPatternTextError> {
        Ok(matches!(self.current.kind, TokenKind::Word(_))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'=')))
    }
    /// One positive-pattern parser, used at the root and in each scope. WALK
    /// is explicit per MATCH; a bare quantifier never silently adopts repeated-
    /// edge semantics. Counters remain definition-wide while local fields move.
    fn positive_pattern(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        let selector_at = self.current.at;
        let search = if self.take_word("ALL")? {
            GraphWalkSearch::AllShortest
        } else if self.take_word("ANY")? {
            GraphWalkSearch::AnyShortest
        } else {
            GraphWalkSearch::All
        };
        let shortest = search != GraphWalkSearch::All;
        let walk_mode = if shortest {
            self.word("SHORTEST")?;
            self.word("WALK")?;
            true
        } else {
            self.take_word("WALK")?
        };
        let first_edge = self.syntax.edges.len();
        loop {
            let mut left = self.node()?;
            while self.is_punct(b'-') || self.is_punct(b'<') {
                // A native selector applies to its complete positive pattern.
                // Until compound-path minimization exists, accept ONE atom,
                // never silently substitute independent atom-wise shortest.
                if shortest && self.syntax.edges.len() != first_edge {
                    return Err(error(self.current.at, GraphPatternTextErrorKind::Expected(
                        "one bounded atom in shortest WALK",
                    )));
                }
                self.capacity(
                    self.edge_count,
                    MAX_PATTERN_EDGES,
                    PatternLimitDimension::Edges,
                )?;
                let incoming = self.take(b'<')?;
                self.punct(b'-', "-")?;
                self.punct(b'[', "[")?;
                self.punct(b':', ":")?;
                let relation = self.name()?;
                let bound_at = self.current.at;
                let walk = self.pattern_walk_bounds(walk_mode)?;
                if shortest && walk.is_none() {
                    return Err(error(bound_at, GraphPatternTextErrorKind::Expected(
                        "finite quantified atom in shortest WALK",
                    )));
                }
                self.punct(b']', "]")?;
                self.punct(b'-', "-")?;
                let outgoing = self.take(b'>')?;
                if incoming && outgoing {
                    return Err(error(
                        relation.at,
                        GraphPatternTextErrorKind::Expected("one edge direction"),
                    ));
                }
                let right = self.node()?;
                self.syntax.edges.push(Edge {
                    source: left,
                    relation,
                    destination: right,
                    direction: if incoming {
                        GlaDirection::Reverse
                    } else if outgoing {
                        GlaDirection::Forward
                    } else {
                        GlaDirection::Undirected
                    },
                    walk,
                    search,
                });
                self.edge_count += 1;
                left = right;
            }
            if shortest && self.is_punct(b',') {
                return Err(error(self.current.at, GraphPatternTextErrorKind::Expected(
                    "one bounded atom in shortest WALK",
                )));
            }
            if !self.take(b',')? {
                break;
            }
        }
        if shortest && self.syntax.edges.len() == first_edge {
            return Err(error(selector_at, GraphPatternTextErrorKind::Expected(
                "one bounded atom in shortest WALK",
            )));
        }
        Ok(())
    }

    fn walk_hop_literal(&mut self) -> Result<u32, GraphPatternTextError> {
        let at = self.current.at;
        let TokenKind::Digits(digits) = self.current.kind else {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("finite integer WALK hop bound"),
            ));
        };
        let hops = digits
            .parse::<u32>()
            .map_err(|_| error(at, GraphPatternTextErrorKind::IntegerOutOfRange))?;
        self.advance()?;
        Ok(hops)
    }

    /// Bound metadata is parsed once, before catalog resolution. This profile
    /// accepts exact *k, inclusive *m..n, and *..n with the conventional minimum
    /// one. Every upper bound is mandatory and checked, including LIMIT 0.
    /// No source-text rewriting, guessed bound or implicit truncation occurs.
    fn pattern_walk_bounds(
        &mut self,
        enabled: bool,
    ) -> Result<Option<crate::GraphWalkBounds>, GraphPatternTextError> {
        let at = self.current.at;
        if !self.take(b'*')? {
            return Ok(None);
        }
        if !enabled {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("explicit MATCH WALK for quantified atoms"),
            ));
        }
        let (minimum, maximum) = if self.take(b'.')? {
            self.punct(b'.', "..")?;
            (1, self.walk_hop_literal()?)
        } else {
            let minimum = self.walk_hop_literal()?;
            let maximum = if self.take(b'.')? {
                self.punct(b'.', "..")?;
                self.walk_hop_literal()?
            } else {
                minimum
            };
            (minimum, maximum)
        };
        crate::GraphWalkBounds::new(minimum, maximum)
            .map(Some)
            .map_err(|_| {
                error(
                    at,
                    GraphPatternTextErrorKind::Expected(
                        "finite ordered WALK bounds within the hop limit",
                    ),
                )
            })
    }

    // Look ahead through the SAME lexer, without consuming its token budget.
    // Existing identifiers named `exists` or `not` remain identifiers unless
    // the complete EXISTS { / NOT EXISTS { introducer is present.
    pub(super) fn starts_existence(&self) -> Result<bool, GraphPatternTextError> {
        let mut lookahead = self.lexer.clone();
        if self.is_word("EXISTS") {
            return Ok(matches!(lookahead.next()?.kind, TokenKind::Punct(b'{')));
        }
        if self.is_word("NOT") {
            let next = lookahead.next()?;
            if matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS")) {
                return Ok(matches!(lookahead.next()?.kind, TokenKind::Punct(b'{')));
            }
        }
        Ok(false)
    }

    fn scoped_predicates(&mut self, allow_existence: bool) -> Result<(), GraphPatternTextError> {
        let mut has_existence = false;
        let mut has_boolean = false;
        let path_predicates = self.syntax.path.is_some();
        loop {
            if self.starts_existence()? {
                if has_boolean {
                    return Err(error(
                        self.current.at,
                        GraphPatternTextErrorKind::UnsupportedBooleanScope,
                    ));
                }
                if !allow_existence {
                    return Err(error(
                        self.current.at,
                        GraphPatternTextErrorKind::Expected(
                            "positive predicates inside a scoped MATCH",
                        ),
                    ));
                }
                let anti = self.take_word("NOT")?;
                self.word("EXISTS")?;
                self.punct(b'{', "{")?;
                self.match_scope(if anti {
                    ScopeKind::NotExists
                } else {
                    ScopeKind::Exists
                })?;
                has_existence = true;
            } else if path_predicates {
                if self.starts_path_predicate()? {
                    self.path_predicate()?;
                } else {
                    self.positive_predicate()?;
                }
            } else {
                let extended = self.boolean_predicates()?;
                if extended && has_existence {
                    return Err(error(
                        self.current.at,
                        GraphPatternTextErrorKind::UnsupportedBooleanScope,
                    ));
                }
                has_boolean |= extended;
            }
            if !self.take_word("AND")? {
                break;
            }
        }
        Ok(())
    }

    fn starts_path_predicate(&self) -> Result<bool, GraphPatternTextError> {
        let TokenKind::Word(word) = self.current.kind else { return Ok(false); };
        if self.syntax.path.is_some_and(|path| path.text == word) {
            return Ok(true);
        }
        Ok((word.eq_ignore_ascii_case("path_length") || word.eq_ignore_ascii_case("nodes")
            || word.eq_ignore_ascii_case("edges"))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')))
    }

    fn path_predicate(&mut self) -> Result<(), GraphPatternTextError> {
        self.capacity(self.predicates, MAX_PATTERN_PREDICATES,
            crate::algebra::PatternLimitDimension::Predicates)?;
        let expression = self.name()?;
        let (variable, function) = if self.take(b'(')? {
            let function = Self::path_function(expression)?;
            let variable = self.path_variable()?;
            self.punct(b')', ")")?;
            (variable, function)
        } else {
            (expression, GraphPathFunction::Value)
        };
        let filter = if self.take_word("IS")? {
            let is_null = !self.take_word("NOT")?;
            self.word("NULL")?;
            Filter::PathNull { variable, function, is_null }
        } else {
            if function != GraphPathFunction::Length {
                return Err(error(expression.at, GraphPatternTextErrorKind::Expected("path IS [NOT] NULL")));
            }
            let comparison = self.comparison()?;
            let value = self.number(GqlParameterType::Int64)?;
            Filter::PathLength { variable, comparison, value }
        };
        self.syntax.filters.push(filter);
        self.predicates += 1;
        Ok(())
    }

    pub(super) fn positive_predicate(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        let left = self.variable()?;
        if self.take(b'.')? {
            self.capacity(
                self.predicates,
                MAX_PATTERN_PREDICATES,
                PatternLimitDimension::Predicates,
            )?;
            let key = self.name()?;
            let filter = self.property_filter(left, key)?;
            self.syntax.filters.push(filter);
            self.predicates += 1;
        } else if self.take_word("IS")? {
            self.capacity(
                self.predicates,
                MAX_PATTERN_PREDICATES,
                PatternLimitDimension::Predicates,
            )?;
            let negate = self.take_word("NOT")?;
            self.word("NULL")?;
            self.syntax.filters.push(Filter::VertexNull {
                variable: left,
                is_null: !negate,
            });
            self.predicates += 1;
        } else {
            self.capacity(
                self.identities,
                MAX_PATTERN_IDENTITIES,
                PatternLimitDimension::Identities,
            )?;
            let comparison = self.comparison()?;
            if !matches!(
                comparison,
                IntegerComparison::Equal | IntegerComparison::NotEqual
            ) {
                return Err(error(
                    left.at,
                    GraphPatternTextErrorKind::Expected("vertex equality or inequality"),
                ));
            }
            let right = self.variable()?;
            self.syntax.filters.push(Filter::Identity {
                left,
                right,
                equal: comparison == IntegerComparison::Equal,
            });
            self.identities += 1;
        }
        Ok(())
    }

    fn take_pattern(&mut self) -> PatternSyntax<'a> {
        PatternSyntax {
            variables: core::mem::take(&mut self.syntax.variables),
            captures: Vec::new(),
            labels: core::mem::take(&mut self.syntax.labels),
            edges: core::mem::take(&mut self.syntax.edges),
            filters: core::mem::take(&mut self.syntax.filters),
        }
    }

    fn restore_pattern(&mut self, pattern: PatternSyntax<'a>) {
        self.syntax.variables = pattern.variables;
        self.syntax.labels = pattern.labels;
        self.syntax.edges = pattern.edges;
        self.syntax.filters = pattern.filters;
    }

    fn match_scope(&mut self, kind: ScopeKind) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        self.capacity(
            self.syntax.scopes.len(),
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::Identities,
        )?;
        let outer = self.take_pattern();
        // Only positive fields change scope. The lexer, global parameters,
        // original offsets, caps and previously completed clauses never reset.
        let parsed = (|| {
            self.word("MATCH")?;
            if self.starts_path_binding()? {
                return Err(error(self.current.at,
                    GraphPatternTextErrorKind::Expected("path binding in root MATCH only")));
            }
            self.positive_pattern()?;
            let matched_variables = self.syntax.variables.len();
            if self.take_word("WHERE")? {
                // The positive pattern is complete. Temporarily expose the
                // containing symbol table to the SAME predicate parser, without
                // inserting nodes or altering its name/parameter/token grammar.
                // Each table has at most 65 names, so their union is bounded.
                // Unused names are removed before any body is resolved/lowered.
                for &name in &outer.variables {
                    if !self.syntax.variables.iter().any(|local| local.text == name.text) {
                        self.syntax.variables.push(name);
                    }
                }
                self.scoped_predicates(false)?;
            }
            if !kind.exports_bindings() {
                self.punct(b'}', "}")?;
            }
            let mut body = self.take_pattern();
            body.variables.truncate(matched_variables);
            body.captures = predicate_captures(&mut body.variables, &body.filters)?;
            Ok::<_, GraphPatternTextError>(body)
        })();
        let body = match parsed {
            Ok(body) => body,
            Err(error) => {
                self.restore_pattern(outer);
                return Err(error);
            }
        };
        self.restore_pattern(outer);
        // Captures already name visible values. Only actual new matched names
        // are exported; existential locals never enter the containing table.
        if kind.exports_bindings() {
            for variable in &body.variables {
                if !self
                    .syntax
                    .variables
                    .iter()
                    .any(|visible| visible.text == variable.text)
                {
                    self.capacity(
                        self.syntax.variables.len(),
                        MAX_PATTERN_VERTICES,
                        PatternLimitDimension::Vertices,
                    )?;
                    self.syntax.variables.push(*variable);
                }
            }
        }
        self.syntax.scopes.push(ScopeSyntax { kind, body });
        Ok(())
    }
}
