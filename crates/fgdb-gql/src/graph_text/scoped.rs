//! Scoped MATCH parsing and lowering through the existing positive-pattern compiler.
//! The lexer and numeric argument table are shared with ordinary and aggregate
//! text. OPTIONAL exports names; existential names remain local to their body.

use super::*;
use crate::algebra::GraphMatchClause;

#[derive(Clone, Copy)]
enum ScopeKind {
    Optional,
    Exists,
    NotExists,
}

struct PatternSyntax<'a> {
    variables: Vec<Name<'a>>,
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
        let (builder, filters) = resolve_pattern(
            &self.body.variables,
            &self.body.labels,
            &self.body.edges,
            self.body.filters,
            symbol,
        )?;
        Ok(BoundScope { kind: self.kind, builder, filters })
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
    let mut builder = GraphPatternBuilder::new();
    for name in variables {
        built(name.at, builder.vertex(name.text))?;
    }
    for &(variable, label) in labels {
        let GraphSymbol::Label(label_id) = symbol(GraphSymbolKind::Label, label)? else {
            unreachable!("symbol domain is checked by the shared resolver")
        };
        built(variable.at, builder.filter(variable.text, VertexPredicate::HasLabel(label_id)))?;
    }
    for edge in edges {
        let GraphSymbol::Relation(relation) = symbol(GraphSymbolKind::Relation, edge.relation)? else {
            unreachable!("symbol domain is checked by the shared resolver")
        };
        built(edge.relation.at, builder.edge(
            edge.source.text, relation, edge.direction, edge.destination.text,
        ))?;
    }
    let mut numeric = Vec::new();
    for filter in filters {
        match filter {
            Filter::Identity { left, right, equal } => {
                built(left.at, builder.identity(left.text, right.text, equal))?;
            }
            Filter::Property { variable, key, comparison, value } => {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                numeric.push(BoundFilter {
                    variable: variable.text.to_owned(), key, comparison, value,
                });
            }
            Filter::Scalar { variable, key, predicate } => {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                built(variable.at, builder.filter(variable.text,
                    VertexPredicate::ScalarProperty { key, predicate }))?;
            }
            Filter::Null { variable, key, is_null } => {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                    unreachable!("symbol domain is checked by the shared resolver")
                };
                built(variable.at, builder.filter(variable.text,
                    VertexPredicate::PropertyNull { key, is_null }))?;
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
        let predicate = match filter.value.value(values) {
            GqlParameterValue::Int64(value) => VertexPredicate::IntegerProperty {
                key: filter.key, comparison: filter.comparison, value,
            },
            GqlParameterValue::Scalar(value) => VertexPredicate::ScalarProperty {
                key: filter.key, predicate: value.predicate(filter.comparison),
            },
            GqlParameterValue::UInt64(_) => unreachable!("property arguments were type-checked at preparation"),
        };
        built(at, builder.filter(&filter.variable, predicate))?;
    }
    Ok(builder)
}

impl<'a> Parser<'a> {
    pub(super) fn parse_scoped_head(&mut self) -> Result<(), GraphPatternTextError> {
        self.word("MATCH")?;
        self.positive_pattern()?;
        self.syntax.root_variables = self.syntax.variables.len();
        if self.take_word("WHERE")? {
            self.scoped_predicates(true)?;
        }
        while self.take_word("OPTIONAL")? {
            self.match_scope(ScopeKind::Optional)?;
        }
        self.syntax.return_at = self.current.at;
        self.word("RETURN")
    }

    /// One fixed-length positive-pattern parser, used at the root and in each
    /// scope. Counters remain definition-wide even while local fields move.
    fn positive_pattern(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        loop {
            let mut left = self.node()?;
            while self.is_punct(b'-') || self.is_punct(b'<') {
                self.capacity(self.edge_count, MAX_PATTERN_EDGES, PatternLimitDimension::Edges)?;
                let incoming = self.take(b'<')?;
                self.punct(b'-', "-")?;
                self.punct(b'[', "[")?;
                self.punct(b':', ":")?;
                let relation = self.name()?;
                self.punct(b']', "]")?;
                self.punct(b'-', "-")?;
                let outgoing = self.take(b'>')?;
                if incoming && outgoing {
                    return Err(error(relation.at, GraphPatternTextErrorKind::Expected("one edge direction")));
                }
                let right = self.node()?;
                self.syntax.edges.push(Edge {
                    source: left,
                    relation,
                    destination: right,
                    direction: if incoming { GlaDirection::Reverse }
                        else if outgoing { GlaDirection::Forward }
                        else { GlaDirection::Undirected },
                });
                self.edge_count += 1;
                left = right;
            }
            if !self.take(b',')? { break; }
        }
        Ok(())
    }

    // Look ahead through the SAME lexer, without consuming its token budget.
    // Existing identifiers named `exists` or `not` remain identifiers unless
    // the complete EXISTS { / NOT EXISTS { introducer is present.
    fn starts_existence(&self) -> Result<bool, GraphPatternTextError> {
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
        loop {
            if self.starts_existence()? {
                if !allow_existence {
                    return Err(error(self.current.at,
                        GraphPatternTextErrorKind::Expected("positive predicates inside a scoped MATCH")));
                }
                let anti = self.take_word("NOT")?;
                self.word("EXISTS")?;
                self.punct(b'{', "{")?;
                self.match_scope(if anti { ScopeKind::NotExists } else { ScopeKind::Exists })?;
            } else {
                self.positive_predicate()?;
            }
            if !self.take_word("AND")? { break; }
        }
        Ok(())
    }

    fn positive_predicate(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        let left = self.variable()?;
        if self.take(b'.')? {
            self.capacity(self.predicates, MAX_PATTERN_PREDICATES, PatternLimitDimension::Predicates)?;
            let key = self.name()?;
            let filter = self.property_filter(left, key)?;
            self.syntax.filters.push(filter);
            self.predicates += 1;
        } else {
            self.capacity(self.identities, MAX_PATTERN_IDENTITIES, PatternLimitDimension::Identities)?;
            let comparison = self.comparison()?;
            if !matches!(comparison, IntegerComparison::Equal | IntegerComparison::NotEqual) {
                return Err(error(left.at, GraphPatternTextErrorKind::Expected("vertex equality or inequality")));
            }
            let right = self.variable()?;
            self.syntax.filters.push(Filter::Identity {
                left, right, equal: comparison == IntegerComparison::Equal,
            });
            self.identities += 1;
        }
        Ok(())
    }

    fn take_pattern(&mut self) -> PatternSyntax<'a> {
        PatternSyntax {
            variables: core::mem::take(&mut self.syntax.variables),
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
        let at = self.current.at;
        self.capacity(self.syntax.scopes.len(), MAX_PATTERN_IDENTITIES, PatternLimitDimension::Identities)?;
        let outer = self.take_pattern();
        // Only positive fields change scope. The lexer, global parameters,
        // original offsets, caps and previously completed clauses never reset.
        let parsed = (|| {
            self.word("MATCH")?;
            self.positive_pattern()?;
            if self.take_word("WHERE")? { self.scoped_predicates(false)?; }
            if !matches!(kind, ScopeKind::Optional) { self.punct(b'}', "}")?; }
            Ok::<_, GraphPatternTextError>(self.take_pattern())
        })();
        let body = match parsed {
            Ok(body) => body,
            Err(error) => { self.restore_pattern(outer); return Err(error); }
        };
        let correlated = body.variables.iter().any(|local|
            outer.variables.iter().any(|visible| visible.text == local.text));
        self.restore_pattern(outer);
        if !correlated {
            return Err(error(at, GraphPatternTextErrorKind::Build(PatternBuildError::Disconnected)));
        }
        if matches!(kind, ScopeKind::Optional) {
            for variable in &body.variables {
                if !self.syntax.variables.iter().any(|visible| visible.text == variable.text) {
                    self.capacity(self.syntax.variables.len(), MAX_PATTERN_VERTICES, PatternLimitDimension::Vertices)?;
                    self.syntax.variables.push(*variable);
                }
            }
        }
        self.syntax.scopes.push(ScopeSyntax { kind, body });
        Ok(())
    }
}
