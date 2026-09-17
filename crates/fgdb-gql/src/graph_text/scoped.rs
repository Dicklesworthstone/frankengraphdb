//! Ordered MATCH parsing and lowering through the existing positive-pattern compiler.
//! The lexer and numeric argument table are shared with ordinary and aggregate
//! text. Required MATCH and OPTIONAL export names; existential names stay local.
//! Bodies without shared variables are independent, not malformed correlations.
//! Child WHERE may read visible outer vertices absent from its positive pattern;
//! those values are captured with null intact, not introduced as node matches.
//! Shortest WALK, TRAIL, ACYCLIC and SIMPLE require one finite quantified atom
//! per selected positive pattern. Restrictions preserve occurrence multiplicity;
//! they never imply a global DISTINCT. An explicit root binding captures the
//! selected path, including its real ordered edge identities.

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
    /// Facade transcript tag for this scope kind, matching the resolved
    /// lowering distinction: optional extends nulls, exists/not-exists probe.
    pub(super) fn kind_tag(&self) -> u8 {
        match self.kind {
            ScopeKind::Required => 0,
            ScopeKind::Optional => 1,
            ScopeKind::Exists => 2,
            ScopeKind::NotExists => 3,
        }
    }
    pub(super) fn builder(&self) -> &GraphPatternBuilder {
        &self.builder
    }
    pub(super) fn filters(&self) -> &[BoundFilter] {
        &self.filters
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
    for (edge_at, edge) in edges.iter().enumerate() {
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
            Some(bounds) if edge.search == GraphWalkSearch::AnyShortest => builder
                .any_shortest_walk(
                    edge.source.text,
                    relation,
                    edge.direction,
                    edge.destination.text,
                    bounds,
                ),
            Some(bounds) if edge.search == GraphWalkSearch::Acyclic => builder.acyclic_walk(
                edge.source.text,
                relation,
                edge.direction,
                edge.destination.text,
                bounds,
            ),
            Some(bounds) if edge.search == GraphWalkSearch::Simple => builder.simple_walk(
                edge.source.text,
                relation,
                edge.direction,
                edge.destination.text,
                bounds,
            ),
            Some(bounds) if edge.search == GraphWalkSearch::Trail => builder.trail_walk(
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
        if let Some(variable) = edge.variable {
            built(variable.at, builder.capture_edge(variable.text, edge_at))?;
        }
    }
    let mut numeric = Vec::new();
    for filter in filters {
        let edge_property = |name: Name<'_>| {
            edges.iter().any(|edge| {
                edge.variable
                    .is_some_and(|variable| variable.text == name.text)
            })
        };
        let uses_edge = match &filter {
            Filter::Property { variable, .. }
            | Filter::Scalar { variable, .. }
            | Filter::Null { variable, .. } => edge_property(*variable),
            Filter::Properties { left, right, .. } => edge_property(*left) || edge_property(*right),
            _ => false,
        };
        if uses_edge {
            numeric.push(BoundFilter::Boolean(
                boolean::BoundBooleanTemplate::resolve(
                    vec![boolean::SyntaxItem::Atom(filter)],
                    0,
                    edges,
                    symbol,
                )?,
            ));
            continue;
        }
        match filter {
            Filter::PathCapture(name) => {
                built(name.at, builder.capture_path(name.text))?;
            }
            Filter::PathLength {
                variable,
                comparison,
                value,
            } => {
                numeric.push(BoundFilter::PathLength {
                    variable: variable.text.to_owned(),
                    comparison,
                    value,
                });
            }
            Filter::PathNull {
                variable,
                function,
                is_null,
            } => {
                built(
                    variable.at,
                    builder.filter_path_null(variable.text, function, is_null),
                )?;
            }
            Filter::Boolean { program, at } => {
                numeric.push(BoundFilter::Boolean(
                    boolean::BoundBooleanTemplate::resolve(program, at, edges, symbol)?,
                ));
            }
            filter @ Filter::VertexNull { .. } => {
                numeric.push(BoundFilter::Boolean(
                    boolean::BoundBooleanTemplate::resolve(
                        vec![boolean::SyntaxItem::Atom(filter)],
                        0,
                        edges,
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
        if let BoundFilter::PathLength {
            variable,
            comparison,
            value,
        } = filter
        {
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
            // List declarations refuse at the typed number parser, matching the
            // scalar property admission invariant; a List value cannot bind.
            GqlParameterValue::List(_) => {
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
    edges: &[Edge<'a>],
) -> Result<Vec<Name<'a>>, GraphPatternTextError> {
    // Expression columns carry variable references just as comparison atoms
    // do. Keep both on the same work stack so mixed expressions and atoms
    // preserve first-reference order, deduplication and the admission bound.
    enum Pending<'query, 'syntax> {
        Predicate(&'syntax Filter<'query>),
        Variable(Name<'query>),
    }

    let mut captures = Vec::new();
    let mut pending: Vec<_> = filters.iter().rev().map(Pending::Predicate).collect();
    while let Some(item) = pending.pop() {
        let names = match item {
            Pending::Variable(name) => [Some(name), None],
            Pending::Predicate(filter) => match filter {
                Filter::PathCapture(_) | Filter::PathLength { .. } | Filter::PathNull { .. } => {
                    return Err(error(
                        0,
                        GraphPatternTextErrorKind::Expected("root path predicate"),
                    ));
                }
                Filter::Boolean { program, .. } => {
                    for item in program.iter().rev() {
                        match item {
                            boolean::SyntaxItem::Atom(atom) => {
                                pending.push(Pending::Predicate(atom));
                            }
                            boolean::SyntaxItem::Expression { columns, .. } => {
                                for &(variable, _) in columns.iter().rev() {
                                    pending.push(Pending::Variable(variable));
                                }
                            }
                            boolean::SyntaxItem::Truth(_)
                            | boolean::SyntaxItem::And
                            | boolean::SyntaxItem::Or
                            | boolean::SyntaxItem::Not => {}
                        }
                    }
                    continue;
                }
                Filter::Properties { left, right, .. } | Filter::Identity { left, right, .. } => {
                    [Some(*left), Some(*right)]
                }
                Filter::VertexNull { variable, .. }
                | Filter::Property { variable, .. }
                | Filter::Scalar { variable, .. }
                | Filter::Null { variable, .. } => [Some(*variable), None],
            },
        };
        for name in names.into_iter().flatten() {
            if edges.iter().any(|edge| {
                edge.variable
                    .is_some_and(|variable| variable.text == name.text)
            }) {
                continue;
            }
            if variables.iter().any(|variable| variable.text == name.text) {
                continue;
            }
            if variables.len() == MAX_PATTERN_VERTICES {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                        dimension: crate::algebra::PatternLimitDimension::Vertices,
                        limit: MAX_PATTERN_VERTICES,
                        observed: variables.len() + 1,
                    }),
                ));
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
    /// One positive-pattern parser, used at the root and in each scope. A
    /// quantifier requires explicit WALK, TRAIL, ACYCLIC or SIMPLE semantics.
    /// Restricted native patterns contain one finite atom; separate MATCH
    /// clauses keep separate restrictions. Definition-wide counters never reset.
    fn positive_pattern(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        let selector_at = self.current.at;
        let search = if self.take_word("ALL")? {
            GraphWalkSearch::AllShortest
        } else if self.take_word("ANY")? {
            GraphWalkSearch::AnyShortest
        } else if self.take_word("ACYCLIC")? {
            GraphWalkSearch::Acyclic
        } else if self.take_word("SIMPLE")? {
            GraphWalkSearch::Simple
        } else if self.take_word("TRAIL")? {
            GraphWalkSearch::Trail
        } else {
            GraphWalkSearch::All
        };
        let shortest = matches!(
            search,
            GraphWalkSearch::AllShortest | GraphWalkSearch::AnyShortest
        );
        let restricted = matches!(
            search,
            GraphWalkSearch::Acyclic | GraphWalkSearch::Simple | GraphWalkSearch::Trail
        );
        let selected = shortest || restricted;
        if shortest {
            self.word("SHORTEST")?;
            self.word("WALK")?;
        } else if !restricted {
            self.take_word("WALK")?;
        }
        let expected_atom = if search == GraphWalkSearch::Trail {
            "one finite quantified TRAIL atom"
        } else if restricted {
            "one finite quantified ACYCLIC or SIMPLE atom"
        } else {
            "one bounded atom in shortest WALK"
        };
        let first_edge = self.syntax.edges.len();
        loop {
            let mut left = self.node()?;
            while self.is_punct(b'-') || self.is_punct(b'<') {
                // No per-atom substitution for a whole-pattern restriction or
                // shortest-total-length selection across a compound pattern.
                if selected && self.syntax.edges.len() != first_edge {
                    return Err(error(
                        self.current.at,
                        GraphPatternTextErrorKind::Expected(expected_atom),
                    ));
                }
                self.capacity(
                    self.edge_count,
                    MAX_PATTERN_EDGES,
                    PatternLimitDimension::Edges,
                )?;
                let incoming = self.take(b'<')?;
                self.punct(b'-', "-")?;
                self.punct(b'[', "[")?;
                let variable = if self.is_punct(b':') {
                    None
                } else {
                    Some(self.name()?)
                };
                self.punct(b':', ":")?;
                let relation = self.name()?;
                let bound_at = self.current.at;
                let walk = self.pattern_walk_bounds()?;
                if selected && walk.is_none() {
                    return Err(error(
                        bound_at,
                        GraphPatternTextErrorKind::Expected(if restricted {
                            expected_atom
                        } else {
                            "finite quantified atom in shortest WALK"
                        }),
                    ));
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
                    variable,
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
            if selected && self.is_punct(b',') {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::Expected(expected_atom),
                ));
            }
            if !self.take(b',')? {
                break;
            }
        }
        if selected && self.syntax.edges.len() == first_edge {
            return Err(error(
                selector_at,
                GraphPatternTextErrorKind::Expected(expected_atom),
            ));
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
    ) -> Result<Option<crate::GraphWalkBounds>, GraphPatternTextError> {
        let at = self.current.at;
        if !self.take(b'*')? {
            return Ok(None);
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
        let path_predicates = allow_existence && self.syntax.path.is_some();
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
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(false);
        };
        if self.syntax.path.is_some_and(|path| path.text == word) {
            return Ok(true);
        }
        Ok((word.eq_ignore_ascii_case("path_length")
            || word.eq_ignore_ascii_case("nodes")
            || word.eq_ignore_ascii_case("edges"))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')))
    }

    fn path_predicate(&mut self) -> Result<(), GraphPatternTextError> {
        self.capacity(
            self.predicates,
            MAX_PATTERN_PREDICATES,
            crate::algebra::PatternLimitDimension::Predicates,
        )?;
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
            Filter::PathNull {
                variable,
                function,
                is_null,
            }
        } else {
            if function != GraphPathFunction::Length {
                return Err(error(
                    expression.at,
                    GraphPatternTextErrorKind::Expected("path IS [NOT] NULL"),
                ));
            }
            let comparison = self.comparison()?;
            let value = self.number(GqlParameterType::Int64)?;
            Filter::PathLength {
                variable,
                comparison,
                value,
            }
        };
        self.syntax.filters.push(filter);
        self.predicates += 1;
        Ok(())
    }

    pub(super) fn positive_predicate(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        let left = if matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.')) {
            self.property_variable()?
        } else {
            self.variable()?
        };
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
        // Leading row aliases are lowered at the root join. They must not
        // escape an OPTIONAL/EXISTS predicate into a post-join filter.
        let row_bindings = core::mem::take(&mut self.read_row_bindings);
        // Only positive fields change scope. The lexer, global parameters,
        // original offsets, caps and previously completed clauses never reset.
        let parsed = (|| {
            self.word("MATCH")?;
            if self.starts_path_binding()? {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::Expected("path binding in root MATCH only"),
                ));
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
                    if !self
                        .syntax
                        .variables
                        .iter()
                        .any(|local| local.text == name.text)
                    {
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
            body.captures = predicate_captures(&mut body.variables, &body.filters, &body.edges)?;
            Ok::<_, GraphPatternTextError>(body)
        })();
        self.read_row_bindings = row_bindings;
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

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::GqlQueryPolicy;
    use fgdb_types::{CanonicalScalar, VId};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        }
    }

    fn run(
        text: &str,
        arguments: &GqlParameters,
        values: &BTreeMap<VId, CanonicalScalar>,
    ) -> Vec<Vec<Option<VId>>> {
        let pattern = PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(arguments)
            .unwrap();
        pattern
            .plan()
            .execute_governed_with_properties(
                5,
                (1..=5).map(VId),
                [(VId(1), RelationId(1), VId(2))],
                |vid, predicate| {
                    let properties: Vec<_> = values
                        .get(&vid)
                        .map(|value| (PropertyKeyId(1), value.clone()))
                        .into_iter()
                        .collect();
                    Ok::<_, ()>(predicate.iter().all(|p| p.matches(&[], &properties)))
                },
                |vid, _| Ok(values.get(&vid)),
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
            .iter()
            .map(|row| row.values().iter().map(|value| value.as_vertex()).collect())
            .collect()
    }

    fn numbers() -> BTreeMap<VId, CanonicalScalar> {
        BTreeMap::from([
            (VId(1), CanonicalScalar::Int(1)),
            (VId(2), CanonicalScalar::Int(2)),
            (VId(3), CanonicalScalar::Int(2)),
            (VId(4), CanonicalScalar::Null),
        ])
    }

    #[test]
    fn expression_captures_keep_first_reference_order_without_matching_outer_nodes() {
        let syntax = Parser::new(
            "MATCH (a),(b),(unused) OPTIONAL MATCH (n) \
             WHERE n.n + b.n > a.n + b.n AND a.n = 1 RETURN n",
        )
        .unwrap()
        .parse()
        .unwrap();
        let body = &syntax.scopes[0].body;
        assert_eq!(
            body.variables
                .iter()
                .map(|name| name.text)
                .collect::<Vec<_>>(),
            vec!["n", "b", "a"]
        );
        assert_eq!(
            body.captures
                .iter()
                .map(|name| name.text)
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn arithmetic_captures_execute_in_required_optional_and_existential_scopes() {
        let values = numbers();
        for clause in ["MATCH", "OPTIONAL MATCH"] {
            let text = format!(
                "MATCH (a {{n:1}}) {clause} (b) \
                 WHERE b.n = a.n + $offset RETURN a,b"
            );
            let arguments = GqlParameters::new().with_int64("offset", 1).unwrap();
            assert_eq!(
                run(&text, &arguments, &values),
                vec![
                    vec![Some(VId(1)), Some(VId(2))],
                    vec![Some(VId(1)), Some(VId(3))]
                ]
            );
            let arguments = GqlParameters::new().with_int64("offset", 10).unwrap();
            let expected = if clause == "MATCH" {
                Vec::new()
            } else {
                vec![vec![Some(VId(1)), None]]
            };
            assert_eq!(run(&text, &arguments, &values), expected);
        }
        let arguments = GqlParameters::new();
        assert_eq!(
            run(
                "MATCH (a) WHERE EXISTS { MATCH (b) WHERE b.n = a.n + 1 } RETURN a",
                &arguments,
                &values,
            ),
            vec![vec![Some(VId(1))]]
        );
        assert_eq!(
            run(
                "MATCH (a) WHERE NOT EXISTS { MATCH (b) WHERE b.n = a.n + 1 } RETURN a",
                &arguments,
                &values,
            ),
            (2..=5).map(|id| vec![Some(VId(id))]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn string_expressions_capture_outer_properties_through_the_same_path() {
        let values = BTreeMap::from([
            (VId(1), CanonicalScalar::ucs_basic_text("Ada").unwrap()),
            (VId(2), CanonicalScalar::ucs_basic_text("ADA").unwrap()),
            (VId(3), CanonicalScalar::ucs_basic_text("ADA").unwrap()),
            (VId(4), CanonicalScalar::Null),
        ]);
        assert_eq!(
            run(
                "MATCH (a {n:'Ada'}) MATCH (b) WHERE b.n = UPPER(a.n) RETURN a,b",
                &GqlParameters::new(),
                &values,
            ),
            vec![
                vec![Some(VId(1)), Some(VId(2))],
                vec![Some(VId(1)), Some(VId(3))]
            ]
        );
    }

    #[test]
    fn nullable_outer_expression_operands_never_become_fresh_positive_matches() {
        for expression in ["b.n + 1", "UPPER(b.n)"] {
            let text = format!(
                "MATCH (a {{n:1}}) OPTIONAL MATCH (a)-[:R]->(b {{n:99}}) \
                 OPTIONAL MATCH (c) WHERE c.n = {expression} RETURN a,b,c"
            );
            assert_eq!(
                run(&text, &GqlParameters::new(), &numbers()),
                vec![vec![Some(VId(1)), None, None]]
            );
        }
    }

    #[test]
    fn expression_capture_admission_counts_distinct_names_at_the_exact_boundary() {
        let storage: Vec<_> = (0..MAX_PATTERN_VERTICES)
            .map(|index| format!("v{index}"))
            .collect();
        let mut variables: Vec<_> = storage[..MAX_PATTERN_VERTICES - 1]
            .iter()
            .map(|text| Name { text, at: 0 })
            .collect();
        let last = Name {
            text: &storage[MAX_PATTERN_VERTICES - 1],
            at: 11,
        };
        let key = Name { text: "n", at: 0 };
        let filter = Filter::Boolean {
            program: vec![boolean::SyntaxItem::Expression {
                columns: vec![(variables[0], key), (last, key), (last, key)],
                program: Vec::new(),
            }],
            at: 0,
        };
        let captures = predicate_captures(&mut variables, &[filter], &[]).unwrap();
        assert_eq!(variables.len(), MAX_PATTERN_VERTICES);
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].text, last.text);
        let overflow = Filter::Boolean {
            program: vec![boolean::SyntaxItem::Expression {
                columns: vec![(
                    Name {
                        text: "overflow",
                        at: 29,
                    },
                    key,
                )],
                program: Vec::new(),
            }],
            at: 0,
        };
        let failure = predicate_captures(&mut variables, &[overflow], &[])
            .err()
            .unwrap();
        assert_eq!(failure.offset, 29);
        assert!(matches!(
            failure.kind,
            GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                dimension: crate::algebra::PatternLimitDimension::Vertices,
                limit: MAX_PATTERN_VERTICES,
                ..
            })
        ));
        assert_eq!(variables.len(), MAX_PATTERN_VERTICES);
    }
}
