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

pub(super) mod mutation;
mod path;

use super::*;
use crate::algebra::{EdgeRelation, GraphMatchClause, GraphWalkSearch};

#[derive(Clone, Copy)]
enum ScopeKind {
    Required,
    Optional,
    Exists,
    NotExists,
    /// A pattern comprehension's body (fgdb-7nwox).
    Collect,
}

impl ScopeKind {
    fn exports_bindings(self) -> bool {
        matches!(self, Self::Required | Self::Optional)
    }
}

struct PatternSyntax<'a> {
    match_mode: GraphMatchMode,
    variables: Vec<Name<'a>>,
    captures: Vec<Name<'a>>,
    labels: Vec<(Name<'a>, Name<'a>)>,
    edges: Vec<Edge<'a>>,
    filters: Vec<Filter<'a>>,
}

pub(super) struct ScopeSyntax<'a> {
    kind: ScopeKind,
    body: PatternSyntax<'a>,
    nested: Vec<ScopeSyntax<'a>>,
    /// A Collect scope's element: the local each witness contributes.
    element: Option<Name<'a>>,
    /// Multiple locals are captured together, never as independent lists.
    tuple: Option<Vec<Name<'a>>>,
}

#[derive(Clone)]
pub(super) struct BoundScope {
    kind: ScopeKind,
    builder: GraphPatternBuilder,
    filters: Vec<BoundFilter>,
    element: Option<String>,
    tuple: Option<Vec<String>>,
    nested: Vec<BoundScope>,
}

impl BoundScope {
    pub(super) fn clause(&self) -> GraphMatchClause<'_> {
        let mut clause = match self.kind {
            ScopeKind::Required => GraphMatchClause::required(&self.builder),
            ScopeKind::Optional => GraphMatchClause::optional(&self.builder),
            ScopeKind::Exists => GraphMatchClause::exists(&self.builder),
            ScopeKind::NotExists => GraphMatchClause::not_exists(&self.builder),
            ScopeKind::Collect => match &self.tuple {
                Some(tuple) => GraphMatchClause::collect_tuple(
                    &self.builder,
                    tuple.iter().map(String::as_str).collect(),
                ),
                None => GraphMatchClause::collect(&self.builder, self.element.as_deref()),
            },
        };
        clause.nested = self.nested.iter().map(Self::clause).collect();
        clause
    }
    /// Facade transcript tag for this scope kind, matching the resolved
    /// lowering distinction: optional extends nulls, exists/not-exists probe,
    /// collect gathers a list.
    pub(super) fn kind_tag(&self) -> u8 {
        match self.kind {
            ScopeKind::Required => 0,
            ScopeKind::Optional => 1,
            ScopeKind::Exists => 2,
            ScopeKind::NotExists => 3,
            ScopeKind::Collect => 4,
        }
    }
    /// A Collect scope's element name, for its transcript.
    pub(super) fn element(&self) -> Option<&str> {
        self.element.as_deref()
    }
    pub(super) fn tuple(&self) -> Option<&[String]> {
        self.tuple.as_deref()
    }
    pub(super) fn builder(&self) -> &GraphPatternBuilder {
        &self.builder
    }
    pub(super) fn filters(&self) -> &[BoundFilter] {
        &self.filters
    }
    pub(super) fn nested(&self) -> &[Self] {
        &self.nested
    }
    pub(super) fn any_pattern(
        &self,
        predicate: &impl Fn(&GraphPatternBuilder, &[BoundFilter]) -> bool,
    ) -> bool {
        predicate(&self.builder, &self.filters)
            || self.nested.iter().any(|scope| scope.any_pattern(predicate))
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
            element: self.element.clone(),
            tuple: self.tuple.clone(),
            nested: self
                .nested
                .iter()
                .map(|scope| scope.bind_values(values, at))
                .collect::<Result<_, _>>()?,
        })
    }
}

impl<'a> ScopeSyntax<'a> {
    /// Find a binding mentioned anywhere in this bounded scope tree. Audits
    /// at row and comprehension boundaries must also see names that appear
    /// only in a descendant, before they can become fresh graph locals.
    pub(super) fn find_name(&self, predicate: &impl Fn(&Name<'a>) -> bool) -> Option<Name<'a>> {
        self.body
            .variables
            .iter()
            .chain(&self.body.captures)
            .chain(
                self.body
                    .edges
                    .iter()
                    .filter_map(|edge| edge.variable.as_ref()),
            )
            .copied()
            .find(predicate)
            .or_else(|| {
                self.nested
                    .iter()
                    .find_map(|scope| scope.find_name(predicate))
            })
    }

    /// Relationship patterns visible after this clause closes, each with
    /// whether it can be NULL. A required clause exports its edges; an
    /// OPTIONAL clause's edges read NULL where it has no witness. Existential
    /// locals stay private.
    pub(super) fn exported_edges(&self) -> impl Iterator<Item = (&Edge<'a>, bool)> {
        let exported = if self.kind.exports_bindings() {
            &self.body.edges[..]
        } else {
            &[]
        };
        let nullable = matches!(self.kind, ScopeKind::Optional);
        exported.iter().map(move |edge| (edge, nullable))
    }

    pub(super) fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<BoundScope, GraphPatternTextError> {
        let (builder, filters) = resolve_pattern_with_captures(
            self.body.match_mode,
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
            element: self.element.map(|name| name.text.to_owned()),
            tuple: self
                .tuple
                .map(|tuple| tuple.into_iter().map(|name| name.text.to_owned()).collect()),
            nested: self
                .nested
                .into_iter()
                .map(|scope| scope.resolve(symbol))
                .collect::<Result<_, _>>()?,
        })
    }
}

/// Root and scoped bodies use the same schema-bound construction. Numeric
/// predicates retain operands for rebinding instead of entering a second AST
/// interpreter or re-resolving a catalog on every execution.
pub(super) fn resolve_pattern<'a>(
    match_mode: GraphMatchMode,
    variables: &[Name<'a>],
    labels: &[(Name<'a>, Name<'a>)],
    edges: &[Edge<'a>],
    filters: Vec<Filter<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<(GraphPatternBuilder, Vec<BoundFilter>), GraphPatternTextError> {
    resolve_pattern_with_captures(match_mode, variables, &[], labels, edges, filters, symbol)
}

fn resolve_pattern_with_captures<'a>(
    match_mode: GraphMatchMode,
    variables: &[Name<'a>],
    captures: &[Name<'a>],
    labels: &[(Name<'a>, Name<'a>)],
    edges: &[Edge<'a>],
    filters: Vec<Filter<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<(GraphPatternBuilder, Vec<BoundFilter>), GraphPatternTextError> {
    let mut builder = GraphPatternBuilder::new();
    builder.match_mode(match_mode);
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
    let mut numeric = Vec::new();
    for (edge_at, edge) in edges.iter().enumerate() {
        let mut admitted = match edge.relation {
            Some(name) => {
                let mut relations = Vec::with_capacity(1 + edge.alternatives.len());
                for name in core::iter::once(name).chain(edge.alternatives.iter().copied()) {
                    let GraphSymbol::Relation(relation) = symbol(GraphSymbolKind::Relation, name)?
                    else {
                        unreachable!("symbol domain is checked by the shared resolver")
                    };
                    relations.push(relation);
                }
                Some((name, relations))
            }
            None => None,
        };
        // A WHERE's type tests intersect with the written type. A type no
        // catalog names matches nothing, as in the row filter; it is no
        // error (fgdb-fj106).
        for test in &edge.type_tests {
            let mut relations = Vec::with_capacity(test.len());
            for &name in test {
                match symbol(GraphSymbolKind::Relation, name) {
                    Ok(GraphSymbol::Relation(relation)) => relations.push(relation),
                    Ok(_) => unreachable!("symbol domain is checked by the shared resolver"),
                    Err(error)
                        if error.kind
                            == GraphPatternTextErrorKind::UnknownSymbol(
                                GraphSymbolKind::Relation,
                            ) => {}
                    Err(error) => return Err(error),
                }
            }
            let at = test.first().map_or(edge.source, |name| *name);
            admitted = Some(match admitted {
                None => (at, relations),
                Some((at, written)) => (
                    at,
                    written
                        .into_iter()
                        .filter(|relation| relations.contains(relation))
                        .collect(),
                ),
            });
        }
        let relation = match admitted {
            None => EdgeRelation::Any,
            // No type is admitted: the clause matches nothing.
            Some((_, relations)) if relations.is_empty() => {
                numeric.push(BoundFilter::Boolean(
                    boolean::BoundBooleanTemplate::resolve(
                        vec![boolean::SyntaxItem::Truth(Some(false))],
                        0,
                        edges,
                        symbol,
                    )?,
                ));
                EdgeRelation::Any
            }
            // The parser bounded the alternation; duplicates collapse.
            Some((name, relations)) => EdgeRelation::alternation(&relations).ok_or_else(|| {
                error(
                    name.at,
                    GraphPatternTextErrorKind::Expected(
                        "at most 8 relationship types in an alternation",
                    ),
                )
            })?,
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
        built(edge.relation.map_or(edge.source.at, |name| name.at), result)?;
        if let Some(variable) = edge.variable {
            built(variable.at, builder.capture_edge(variable.text, edge_at))?;
        }
    }
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
            GqlParameterValue::List(_) | GqlParameterValue::Map(_) => {
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
                // A clause's named path captures its own pattern; it reads no
                // containing value.
                Filter::PathCapture(_) => continue,
                Filter::PathLength { .. } | Filter::PathNull { .. } => {
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
                            boolean::SyntaxItem::StoredProperty { variable, .. } => {
                                pending.push(Pending::Variable(*variable));
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
        self.syntax.match_mode = self.match_mode()?;
        if self.starts_path_binding()? {
            let path = self.name()?;
            self.punct(b'=', "=")?;
            self.syntax.path = Some(path);
        }
        // An OPTIONAL part's comma parts stay one clause.
        self.split_paths = !self.optional_part;
        let pattern = self.positive_pattern();
        self.split_paths = false;
        pattern?;
        self.syntax.root_variables = self.syntax.variables.len();
        // openCypher: a named variable-length relationship `[r*m..n]` is the
        // list of relationships it matched. When it is the pattern's only
        // edge and no path is named, that list is the edge list of the one
        // path the pattern matches, so the path is captured under `r`.
        if self.syntax.path.is_none()
            && let [edge] = self.syntax.edges.as_mut_slice()
            && edge.walk.is_some()
            && let Some(variable) = edge.variable.take()
        {
            self.syntax.path = Some(variable);
            self.syntax.path_edges = true;
        }
        if let Some(path) = self.syntax.path {
            self.syntax.filters.push(Filter::PathCapture(path));
        }
        if self.is_word("WHERE")
            && ((self.defer_where && self.where_needs_values()?)
                || self.deferred_wheres.contains(&self.current.at))
        {
            // A predicate over list or path values runs as a row filter after
            // the match, at an implicit `WITH *` (fgdb-1c29d). So does a
            // required continuation's WHERE the graph predicate refused. The
            // composition entry re-reads it from here.
            self.deferred_where = Some((self.lexer.clone(), self.current));
            self.skip_where()?;
        } else if self.take_word("WHERE")? {
            self.scoped_predicates(true)?;
        }
        // A deferred WHERE filters this clause's rows before any later MATCH
        // or OPTIONAL MATCH joins them: that clause starts the next part of
        // a multipart read (deferred_where_continues), never a scope here.
        while self.deferred_where.is_none() {
            // An OPTIONAL continuation ends before the next clause; its
            // caller draws the implicit `WITH *` there.
            if self.optional_part && (self.is_word("OPTIONAL") || self.is_word("MATCH")) {
                break;
            }
            // So does a clause that reads a name its child scope cannot see
            // (implicit_withs): it starts the next part at an implicit
            // `WITH *`.
            if self.implicit_withs.contains(&self.current.at) {
                break;
            }
            if self.take_word("OPTIONAL")? {
                self.match_scope(ScopeKind::Optional)?;
            } else if self.is_word("MATCH")
                || self
                    .comma_clause
                    .is_some_and(|(at, _)| at == self.current.at)
            {
                self.match_scope(ScopeKind::Required)?;
            } else {
                break;
            }
        }
        self.syntax.return_at = self.current.at;
        Ok(())
    }

    /// A mutation may begin with OPTIONAL MATCH (openCypher Delete1, Set4,
    /// Remove1): the unit relation left-joined with the pattern, so the
    /// writes see its witnesses or exactly one row whose variables are all
    /// NULL. Only OPTIONAL scopes may follow such a root; a required MATCH
    /// would have to run against that NULL row, outside the one pattern.
    pub(super) fn parse_mutation_match_prefix(&mut self) -> Result<bool, GraphPatternTextError> {
        let optional = self.take_word("OPTIONAL")?;
        self.parse_match_prefix()?;
        if optional
            && self
                .syntax
                .scopes
                .iter()
                .any(|scope| matches!(scope.kind, ScopeKind::Required))
        {
            return Err(error(
                self.syntax.return_at,
                GraphPatternTextErrorKind::Expected(
                    "OPTIONAL MATCH after a leading OPTIONAL MATCH",
                ),
            ));
        }
        Ok(optional)
    }

    /// The MATCH prefix of a write. A WHERE the graph predicate cannot hold
    /// (a label test under OR or NOT, a predicate over list values) is
    /// deferred as in a read, so the write reads its rows exactly as `MATCH
    /// ... WITH * WHERE ...` would. When it is deferred the parser stands at
    /// that WHERE again, for the caller's implicit row prefix, and this
    /// returns true.
    pub(super) fn write_match_prefix(&mut self) -> Result<bool, GraphPatternTextError> {
        self.defer_where = true;
        let parsed = self.parse_match_prefix();
        self.defer_where = false;
        parsed?;
        Ok(self.resume_deferred_where())
    }

    /// Stand at a WHERE that parse_match_prefix deferred, if it did.
    pub(super) fn resume_deferred_where(&mut self) -> bool {
        match self.deferred_where.take() {
            Some((lexer, at_where)) => {
                self.lexer = lexer;
                self.current = at_where;
                true
            }
            None => false,
        }
    }

    /// Whether the WHERE at the current token needs the value language
    /// (fgdb-1c29d): a list quantifier, a list comprehension, a list or path
    /// function, `type(r)`, IN over a computed list, or a list or map value
    /// compared, subscripted or sliced (fgdb-bqri3). Pattern predicates
    /// accept none of these, so a WHERE without one keeps its exact pattern
    /// lowering. The boolean filter cannot read a relationship's type
    /// (fgdb-fj106); the row filter reads it through the hoisted metadata
    /// column `RETURN type(r)` already uses.
    /// `nodes(p)` and `edges(p)` at depth zero are path predicates; inside
    /// parentheses (`size(nodes(p))`) they are list values.
    ///
    /// So does a label test (fgdb-qo3jv) the pattern filter cannot read: a
    /// relationship's `r:T`, and a vertex's `v:L` once the WHERE has OR, NOT
    /// or parentheses, or any other conjunct the Boolean parser reads as a
    /// compound expression. There a label test is a Boolean leaf, which the
    /// row filter reads as `'L' IN labels(v)` (`type(r) = 'T'`), three-valued.
    /// A flat AND of label tests and comparisons keeps its inline labels,
    /// and an EXISTS or pattern-predicate conjunct is its own scope, whose
    /// labels and connectives count for neither.
    fn where_needs_values(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut depth = 0_usize;
        let mut braces = 0_usize;
        let mut previous: Option<TokenKind<'a>> = None;
        // A conjunct begins the WHERE and follows each top-level AND.
        let (mut conjunct, mut existence) = (true, false);
        let (mut vertex_test, mut edge_test, mut compound) = (false, false, false);
        // An existence conjunct beside a compound one (`NOT a = b AND NOT
        // (a)-->(b)`, LDBC SNB IC10) is a conjunction the graph predicate
        // refuses (scoped_predicates); the row filter reads the existence
        // test from a hidden column instead (hoist_existence_reads).
        let mut existential = false;
        loop {
            let token = lexer.next()?;
            let key = matches!(previous, Some(TokenKind::Punct(b'.' | b':')));
            let mut ahead = lexer.clone();
            let next = ahead.next()?.kind;
            let binds = |ahead: &mut Lexer<'a>| -> Result<bool, GraphPatternTextError> {
                Ok(matches!(ahead.next()?.kind, TokenKind::Word(word)
                    if word.eq_ignore_ascii_case("IN")))
            };
            if core::mem::take(&mut conjunct) {
                existence = existence_follows(token.kind, &lexer)?;
                existential |= existence;
            }
            // So is an existence test inside a compound conjunct (`b.p = 3 OR
            // (a)-->(b)`), which the Boolean parser refuses (boolean_unary).
            if !existence && !key && existence_follows(token.kind, &lexer)? {
                return Ok(true);
            }
            if !key && Self::relational_existence_body(token.kind, &lexer)? {
                return Ok(true);
            }
            let needs = edge_test || (vertex_test && compound) || (existential && compound);
            match token.kind {
                TokenKind::End => return Ok(needs),
                TokenKind::Punct(b'}') if depth == 0 && self.call_body => return Ok(needs),
                TokenKind::Word(word) if depth == 0 && !key && Self::ends_where(word, previous) => {
                    return Ok(needs);
                }
                // So is an existence test under a top-level OR or XOR
                // (`(a)-->(b) OR b.p = 3`).
                TokenKind::Word(word)
                    if existence
                        && depth == 0
                        && !key
                        && (word.eq_ignore_ascii_case("OR")
                            || word.eq_ignore_ascii_case("XOR")) =>
                {
                    return Ok(true);
                }
                TokenKind::Word(word) if !key && matches!(next, TokenKind::Punct(b'(')) => {
                    let list_function = [
                        "relationships",
                        "labels",
                        "type",
                        "keys",
                        "reduce",
                        "head",
                        "last",
                        "tail",
                        "reverse",
                        "range",
                    ]
                    .iter()
                    .any(|name| word.eq_ignore_ascii_case(name));
                    let path_list = depth > 0
                        && (word.eq_ignore_ascii_case("nodes")
                            || word.eq_ignore_ascii_case("edges"));
                    let quantifier = ["all", "any", "none", "single"]
                        .iter()
                        .any(|name| word.eq_ignore_ascii_case(name))
                        && {
                            ahead.next()?;
                            binds(&mut ahead)?
                        };
                    if list_function || path_list || quantifier {
                        return Ok(true);
                    }
                }
                TokenKind::Punct(b'[')
                    if matches!(next, TokenKind::Word(_)) && binds(&mut ahead)? =>
                {
                    return Ok(true);
                }
                TokenKind::Word(word)
                    if !key
                        && word.eq_ignore_ascii_case("IN")
                        && !matches!(next, TokenKind::Punct(b'[') | TokenKind::Parameter(_)) =>
                {
                    return Ok(true);
                }
                // A list literal that holds a graph element (`b IN [a, c]`,
                // LDBC SNB IC3's `country IN [countryX, countryY]`): the
                // pattern filter compares scalars only, so the row filter
                // compares the elements.
                TokenKind::Word(word)
                    if !key
                        && word.eq_ignore_ascii_case("IN")
                        && matches!(next, TokenKind::Punct(b'['))
                        && self.list_names_element(ahead.clone())? =>
                {
                    return Ok(true);
                }
                // A list literal, subscript or slice, or a map literal
                // compared, that is no relationship bracket, IN's literal
                // list or subquery body: the pattern filter compares scalars
                // only (fgdb-bqri3), as in `n.tags = ['a']` or `n.p[0] = 1`.
                TokenKind::Punct(b'[')
                    if braces == 0
                        && !existence
                        && !matches!(previous, Some(TokenKind::Punct(b'-')))
                        && !matches!(previous, Some(TokenKind::Word(word))
                            if word.eq_ignore_ascii_case("IN")) =>
                {
                    return Ok(true);
                }
                TokenKind::Punct(b'{')
                    if braces == 0
                        && !existence
                        && matches!(previous, Some(TokenKind::Punct(b'=' | b'<' | b'>'))) =>
                {
                    return Ok(true);
                }
                _ => {}
            }
            if !existence && !key {
                match token.kind {
                    TokenKind::Word(word)
                        if braces == 0 && matches!(next, TokenKind::Punct(b':')) =>
                    {
                        if self.syntax.variables.iter().any(|name| name.text == word) {
                            vertex_test = true;
                        } else if self.syntax.visible_edge(word).is_some() {
                            edge_test = true;
                        }
                    }
                    // `IS NOT NULL` is a flat atom; every other NOT, OR,
                    // list or range membership, text operator and
                    // parenthesis makes the Boolean parser's expression
                    // compound.
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT") => {
                        compound |= !matches!(previous, Some(TokenKind::Word(is))
                            if is.eq_ignore_ascii_case("IS"));
                    }
                    TokenKind::Word(word) => {
                        compound |= ["OR", "XOR", "IN", "BETWEEN", "STARTS", "ENDS", "CONTAINS"]
                            .iter()
                            .any(|operator| word.eq_ignore_ascii_case(operator));
                    }
                    TokenKind::Punct(b'(') => compound = true,
                    _ => {}
                }
            }
            if depth == 0
                && !key
                && matches!(token.kind, TokenKind::Word(word)
                if word.eq_ignore_ascii_case("AND"))
            {
                conjunct = true;
            }
            match token.kind {
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                _ => {}
            }
            match token.kind {
                TokenKind::Punct(b'{') => braces += 1,
                TokenKind::Punct(b'}') => braces = braces.saturating_sub(1),
                _ => {}
            }
            previous = Some(token.kind);
        }
    }

    /// Whether the list literal `lexer` starts in (just past its `[`) holds
    /// a bare vertex, relationship or path binding: not a property read
    /// (`a.p`), a call (`f(a)`), a label or a map key.
    fn list_names_element(&self, mut lexer: Lexer<'a>) -> Result<bool, GraphPatternTextError> {
        let mut depth = 1_usize;
        let mut previous: Option<TokenKind<'a>> = None;
        loop {
            let token = lexer.next()?;
            match token.kind {
                TokenKind::End => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(false);
                    }
                }
                TokenKind::Word(word)
                    if !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                        && !matches!(
                            lexer.clone().next()?.kind,
                            TokenKind::Punct(b'.' | b'(' | b':')
                        )
                        && (self.syntax.variables.iter().any(|name| name.text == word)
                            || self.syntax.visible_edge(word).is_some()
                            || self.syntax.names_path(word)) =>
                {
                    return Ok(true);
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
    }

    /// Whether a root MATCH WHERE the row filter reads (where_needs_values)
    /// is followed by another MATCH or OPTIONAL MATCH clause (fgdb-qo3jv,
    /// TCK Match7 [25]). That WHERE filters the first part at its implicit
    /// `WITH *`, so the statement is a multipart read whose next part starts
    /// at that clause, exactly as `MATCH ... WITH * WHERE ... OPTIONAL MATCH`
    /// is. Only a token-level candidate (where_then_match) parses its root
    /// MATCH a second time, before any catalog access; a parse error is left
    /// to the read's own parser.
    pub(super) fn deferred_where_continues(
        statement: &str,
        declarations: &[(&str, crate::GqlParameterType)],
        tokens: &[crate::set_text::TextToken<'_>],
        zones: Option<&crate::GqlTimeZones>,
    ) -> bool {
        if !crate::set_text::multipart::where_then_match(tokens) {
            return false;
        }
        let Ok(parser) = Parser::new_with_parameter_types(statement, declarations) else {
            return false;
        };
        let mut parser = parser.with_zones(zones.cloned());
        parser.defer_where = true;
        parser.parse_match_prefix().is_ok()
            && parser.deferred_where.is_some()
            && (parser.is_word("MATCH") || parser.is_word("OPTIONAL"))
    }

    /// Step past a deferred WHERE and its predicate, to the next clause.
    fn skip_where(&mut self) -> Result<(), GraphPatternTextError> {
        self.advance()?;
        let mut depth = 0_usize;
        let mut previous: Option<TokenKind<'a>> = None;
        loop {
            let key = matches!(previous, Some(TokenKind::Punct(b'.' | b':')));
            match self.current.kind {
                TokenKind::End => return Ok(()),
                TokenKind::Punct(b'}') if depth == 0 && self.call_body => return Ok(()),
                TokenKind::Word(word) if depth == 0 && !key && Self::ends_where(word, previous) => {
                    return Ok(());
                }
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                _ => {}
            }
            previous = Some(self.current.kind);
            self.advance()?;
        }
    }

    /// A clause keyword that ends a root WHERE predicate at depth zero. The
    /// WITH of `STARTS WITH` or `ENDS WITH` is an operator, not a clause.
    pub(super) fn ends_where(word: &str, previous: Option<TokenKind<'_>>) -> bool {
        if word.eq_ignore_ascii_case("WITH")
            && matches!(previous, Some(TokenKind::Word(previous))
                if previous.eq_ignore_ascii_case("STARTS") || previous.eq_ignore_ascii_case("ENDS"))
        {
            return false;
        }
        [
            "RETURN", "WITH", "MATCH", "OPTIONAL", "UNWIND", "ORDER", "SKIP", "LIMIT", "UNION",
            "CALL", "CREATE", "INSERT", "MERGE", "SET", "DELETE", "DETACH", "REMOVE", "FOREACH",
        ]
        .iter()
        .any(|clause| word.eq_ignore_ascii_case(clause))
    }

    fn starts_path_binding(&self) -> Result<bool, GraphPatternTextError> {
        Ok(matches!(self.current.kind, TokenKind::Word(_))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'=')))
    }
    /// One positive-pattern parser, used at the root and in each scope. Both
    /// relationship quantifier spellings retain the native path semantics.
    /// Restricted native patterns contain one finite atom; separate MATCH
    /// clauses keep separate restrictions. Definition-wide counters never reset.
    fn positive_pattern(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        let selector_at = self.current.at;
        // openCypher shortestPath(...) / allShortestPaths(...) (fgdb-20foe)
        // are the GQL ANY SHORTEST / ALL SHORTEST selectors around the same
        // one-atom pattern, with the same laws.
        let function = if matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
            if self.is_word("shortestPath") {
                Some(GraphWalkSearch::AnyShortest)
            } else if self.is_word("allShortestPaths") {
                Some(GraphWalkSearch::AllShortest)
            } else {
                None
            }
        } else {
            None
        };
        let search = match function {
            Some(search) => {
                self.advance()?;
                self.punct(b'(', "(")?;
                search
            }
            None => self.path_search()?,
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
        let expected_atom = if search == GraphWalkSearch::Trail {
            "one finite quantified TRAIL atom"
        } else if restricted {
            "one finite quantified ACYCLIC or SIMPLE atom"
        } else {
            "one quantified atom in shortest WALK"
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
                let mut alternatives = Vec::new();
                let (variable, relation, walk) = if self.take(b'[')? {
                    let variable = if matches!(self.current.kind, TokenKind::Word(_)) {
                        Some(self.name()?)
                    } else {
                        None
                    };
                    let relation = if self.take(b':')? {
                        Some(self.schema_name()?)
                    } else {
                        None
                    };
                    // openCypher type alternation `:A|B|:C` (Match2 [6]).
                    while relation.is_some() && self.take(b'|')? {
                        if alternatives.len() + 1 == crate::algebra::MAX_EDGE_RELATION_ALTERNATIVES
                        {
                            return Err(error(
                                self.current.at,
                                GraphPatternTextErrorKind::Expected(
                                    "at most 8 relationship types in an alternation",
                                ),
                            ));
                        }
                        self.take(b':')?;
                        alternatives.push(self.schema_name()?);
                    }
                    let walk = self.pattern_walk_bounds()?;
                    // openCypher `-[r:T {k: v}]->` filters that one captured
                    // relationship. A walk's map would range over a list.
                    if self.is_punct(b'{') {
                        match (variable, walk) {
                            (Some(variable), None) => self.property_map(variable, true)?,
                            _ => {
                                return Err(error(
                                    self.current.at,
                                    GraphPatternTextErrorKind::Expected(
                                        "named fixed-length relationship for a property map",
                                    ),
                                ));
                            }
                        }
                    }
                    self.punct(b']', "]")?;
                    (variable, relation, walk)
                } else {
                    (None, None, None)
                };
                let bound_at = self.current.at;
                self.punct(b'-', "-")?;
                // openCypher `<-->` matches either direction, as `--` does.
                let outgoing = self.take(b'>')?;
                let walk = self.relationship_walk_bounds(walk)?;
                // Shape only, so before any catalog call: an omitted upper
                // bound needs a repetition rule that ends (the compiler
                // checks the same rule for every builder).
                if walk.is_some_and(|bounds| {
                    !bounds.is_bounded() && !search.ends_unbounded(self.syntax.match_mode)
                }) {
                    return Err(error(
                        bound_at,
                        GraphPatternTextErrorKind::Expected(
                            "an upper hop bound, or TRAIL, ACYCLIC, SIMPLE, a shortest selector or DIFFERENT EDGES for an unbounded quantifier",
                        ),
                    ));
                }
                if selected && walk.is_none() {
                    return Err(error(
                        bound_at,
                        GraphPatternTextErrorKind::Expected(if restricted {
                            expected_atom
                        } else {
                            "a quantified atom in shortest WALK"
                        }),
                    ));
                }
                let right = self.node()?;
                self.syntax.edges.push(Edge {
                    variable,
                    source: left,
                    relation,
                    alternatives,
                    type_tests: Vec::new(),
                    destination: right,
                    direction: match (incoming, outgoing) {
                        (true, false) => GlaDirection::Reverse,
                        (false, true) => GlaDirection::Forward,
                        _ => GlaDirection::Undirected,
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
            // `MATCH (a), (b), p = shortestPath((a)-[*]-(b))` (LDBC SNB IC13,
            // IC14): a named path after node-only parts starts a new required
            // clause there. A clause scopes relationship uniqueness, and the
            // parts before the comma bind no relationship, so the rows are
            // those of the one clause.
            if self.split_paths
                && self.is_punct(b',')
                && self.syntax.edges.len() == first_edge
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Word(_))
                && {
                    let mut ahead = self.lexer.clone();
                    ahead.next()?;
                    matches!(ahead.next()?.kind, TokenKind::Punct(b'='))
                }
            {
                self.comma_clause = Some((self.current.at, self.syntax.match_mode));
                break;
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
        if function.is_some() {
            self.punct(b')', ")")?;
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
        let hops = crate::graph_text::integer_magnitude(digits)
            .and_then(|hops| u32::try_from(hops).ok())
            .ok_or_else(|| error(at, GraphPatternTextErrorKind::IntegerOutOfRange))?;
        self.advance()?;
        Ok(hops)
    }

    /// Bound metadata is parsed once, before catalog resolution. This profile
    /// accepts exact *k, inclusive *m..n, and *..n with the conventional minimum
    /// one, and the unbounded *, *.., *m.. (openCypher Match5, Match6). A
    /// written upper bound is checked, including LIMIT 0; an omitted one is
    /// unbounded, never a guessed bound or an implicit truncation, and the
    /// pattern compiler admits it only under a repetition rule that ends.
    fn pattern_walk_bounds(
        &mut self,
    ) -> Result<Option<crate::GraphWalkBounds>, GraphPatternTextError> {
        let at = self.current.at;
        if !self.take(b'*')? {
            return Ok(None);
        }
        let digits = |parser: &Self| matches!(parser.current.kind, TokenKind::Digits(_));
        let minimum = if digits(self) {
            Some(self.walk_hop_literal()?)
        } else {
            None
        };
        let maximum = if self.take(b'.')? {
            self.punct(b'.', "..")?;
            if digits(self) {
                Some(self.walk_hop_literal()?)
            } else {
                None
            }
        } else {
            minimum
        };
        let minimum = minimum.unwrap_or(1);
        match maximum {
            // openCypher: a reversed interval matches nothing.
            Some(maximum) if minimum > maximum && minimum <= crate::MAX_GRAPH_WALK_HOPS => {
                Ok(crate::GraphWalkBounds::empty())
            }
            Some(maximum) => crate::GraphWalkBounds::new(minimum, maximum),
            None => crate::GraphWalkBounds::unbounded(minimum),
        }
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
    // the complete EXISTS { / NOT EXISTS { introducer, an openCypher pattern
    // predicate `[NOT] (n)-[...]-(...)`, or its `[NOT] exists(<pattern>)`
    // spelling is present.
    pub(super) fn starts_existence(&self) -> Result<bool, GraphPatternTextError> {
        existence_follows(self.current.kind, &self.lexer)
    }

    fn scoped_predicates(&mut self, root: bool) -> Result<(), GraphPatternTextError> {
        let mut has_existence = false;
        let mut has_boolean = false;
        let path_predicates = root && self.syntax.path.is_some();
        loop {
            if self.starts_existence()? {
                if has_boolean {
                    return Err(error(
                        self.current.at,
                        GraphPatternTextErrorKind::UnsupportedBooleanScope,
                    ));
                }
                let anti = self.take_word("NOT")?;
                let kind = if anti {
                    ScopeKind::NotExists
                } else {
                    ScopeKind::Exists
                };
                if self.take_word("EXISTS")? {
                    if self.take(b'(')? {
                        // openCypher `exists((n)-[:R]->())` is the pattern
                        // predicate itself (fgdb-20foe).
                        self.scope(kind, ScopeForm::Pattern)?;
                        self.punct(b')', ")")?;
                    } else {
                        self.punct(b'{', "{")?;
                        self.match_scope(kind)?;
                    }
                } else {
                    // `(n)-[:R]->()` is `EXISTS { MATCH (n)-[:R]->() }`, and
                    // `NOT (n)-[:R]->()` its anti form (fgdb-44d8n).
                    self.scope(kind, ScopeForm::Pattern)?;
                }
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
            || word.eq_ignore_ascii_case("length")
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
            let key = self.schema_name()?;
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
            match_mode: core::mem::replace(
                &mut self.syntax.match_mode,
                GraphMatchMode::DifferentEdges,
            ),
            variables: core::mem::take(&mut self.syntax.variables),
            captures: Vec::new(),
            labels: core::mem::take(&mut self.syntax.labels),
            edges: core::mem::take(&mut self.syntax.edges),
            filters: core::mem::take(&mut self.syntax.filters),
        }
    }

    fn restore_pattern(&mut self, pattern: PatternSyntax<'a>) {
        self.syntax.match_mode = pattern.match_mode;
        self.syntax.variables = pattern.variables;
        self.syntax.labels = pattern.labels;
        self.syntax.edges = pattern.edges;
        self.syntax.filters = pattern.filters;
    }

    fn match_scope(&mut self, kind: ScopeKind) -> Result<(), GraphPatternTextError> {
        self.scope(kind, ScopeForm::Clause)
    }

    /// One scoped body: a `MATCH ... [WHERE ...]` clause (closed by `}` for an
    /// existential), or a bare pattern-predicate pattern, which has no MATCH,
    /// WHERE or braces and is otherwise the identical body.
    fn scope(&mut self, kind: ScopeKind, form: ScopeForm) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        self.capacity(
            self.scope_count,
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::Identities,
        )?;
        self.scope_count += 1;
        let scopes_at = self.syntax.scopes.len();
        let outer = self.take_pattern();
        let anonymous_at = self.outer_anonymous.len();
        self.outer_anonymous.extend(
            outer
                .variables
                .iter()
                .filter(|name| name.text.starts_with(Self::ANONYMOUS_PREFIX))
                .map(|name| name.text),
        );
        // Leading row aliases are lowered at the root join. They must not
        // escape an OPTIONAL/EXISTS predicate into a post-join filter. A
        // later required MATCH is an inner join, so its equality with a row
        // value is the same post-join filter as the root's (fgdb-0s2pq:
        // `UNWIND $rows AS row MATCH (a {id: row.a}) MATCH (b {id: row.b})`).
        let row_bindings = if matches!(kind, ScopeKind::Required) && form == ScopeForm::Clause {
            self.read_row_bindings.clone()
        } else {
            core::mem::take(&mut self.read_row_bindings)
        };
        // Only positive fields change scope. The lexer, global parameters,
        // original offsets, caps and previously completed clauses never reset.
        let clause = form == ScopeForm::Clause;
        // A pattern comprehension's body (fgdb-7nwox): a pattern that may
        // name its path and introduce locals, and a WHERE, all private.
        let comprehension = form == ScopeForm::Comprehension;
        let parsed = (|| {
            if let Some((at, mode)) = self.comma_clause
                && clause
                && at == self.current.at
            {
                // The named path after a required clause's node-only comma
                // parts (positive_pattern): its own clause, in that clause's
                // match mode.
                self.comma_clause = None;
                self.advance()?;
                self.syntax.match_mode = mode;
            } else if clause {
                // openCypher `EXISTS { (n)-->() }` leaves out the MATCH of
                // `EXISTS { MATCH (n)-->() }`; the body is otherwise the same.
                if kind.exports_bindings() || !self.is_punct(b'(') {
                    self.word("MATCH")?;
                }
                self.syntax.match_mode = self.match_mode()?;
            }
            // A required or OPTIONAL clause may name its path, exported like
            // its vertices (openCypher Match7, OPTIONAL MATCH p = ...). An
            // existential body's locals never escape, so it names none.
            let path = if (comprehension || (clause && kind.exports_bindings()))
                && self.starts_path_binding()?
            {
                let path = self.name()?;
                self.punct(b'=', "=")?;
                Some(path)
            } else {
                if self.starts_path_binding()? {
                    return Err(error(
                        self.current.at,
                        GraphPatternTextErrorKind::Expected(
                            "path binding in a required or OPTIONAL MATCH",
                        ),
                    ));
                }
                None
            };
            self.split_paths = clause && matches!(kind, ScopeKind::Required);
            let pattern = self.positive_pattern();
            self.split_paths = false;
            pattern?;
            // As at the root, a named variable-length relationship that is a
            // required or OPTIONAL clause's only edge, with no path named,
            // captures that edge's path under its name, and a bare name reads
            // the path's edge list (openCypher Match4, Match9).
            let mut edges = false;
            let path = match path {
                None if clause && kind.exports_bindings() => {
                    match self.syntax.edges.as_mut_slice() {
                        [edge] if edge.walk.is_some() => {
                            edges = edge.variable.is_some();
                            edge.variable.take()
                        }
                        _ => None,
                    }
                }
                path => path,
            };
            if let Some(path) = path {
                self.syntax.filters.push(Filter::PathCapture(path));
            }
            if !clause && !comprehension {
                // An openCypher pattern predicate tests the bindings it names
                // and cannot introduce one (UndefinedVariable). Only the
                // EXISTS { MATCH ... } spelling declares existential locals.
                let vertices = (self.syntax.variables.iter().copied()).filter(|name| {
                    !name.text.starts_with(Self::ANONYMOUS_PREFIX)
                        && !outer.variables.iter().any(|seen| seen.text == name.text)
                });
                let edges =
                    (self.syntax.edges.iter().filter_map(|edge| edge.variable)).filter(|name| {
                        !(outer.edges.iter().filter_map(|edge| edge.variable))
                            .any(|seen| seen.text == name.text)
                    });
                if let Some(name) = vertices.chain(edges).min_by_key(|name| name.at) {
                    return Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable));
                }
            }
            let matched_variables = self.syntax.variables.len();
            if (clause || comprehension) && self.take_word("WHERE")? {
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
            if clause && !kind.exports_bindings() {
                // openCypher `EXISTS { MATCH ... RETURN true }`: a RETURN of
                // literals or `*` keeps every row, so the subquery still asks
                // only whether a witness exists (ExistentialSubquery2 [1]).
                if self.take_word("RETURN")? {
                    loop {
                        let literal = match self.current.kind {
                            TokenKind::Word(word) => ["TRUE", "FALSE", "NULL"]
                                .iter()
                                .any(|value| word.eq_ignore_ascii_case(value)),
                            TokenKind::Digits(_) | TokenKind::Quoted(_) => true,
                            TokenKind::Punct(b'*') => true,
                            _ => false,
                        };
                        if !literal {
                            return Err(error(
                                self.current.at,
                                GraphPatternTextErrorKind::Expected(
                                    "a literal or * in an EXISTS subquery's RETURN",
                                ),
                            ));
                        }
                        self.advance()?;
                        if self.take_word("AS")? {
                            self.name()?;
                        }
                        if !self.take(b',')? {
                            break;
                        }
                    }
                }
                self.punct(b'}', "}")?;
            }
            let mut body = self.take_pattern();
            body.variables.truncate(matched_variables);
            body.captures = predicate_captures(&mut body.variables, &body.filters, &body.edges)?;
            let nested = self.syntax.scopes.split_off(scopes_at);
            // A grandchild may name an ancestor that this body's own pattern
            // and predicates never mention. Carry that value through this
            // frame as a nullable capture, never as an independent node scan.
            // Each child has already propagated its own descendants' reads.
            for child in &nested {
                for &name in &child.body.variables {
                    if outer
                        .variables
                        .iter()
                        .any(|visible| visible.text == name.text)
                        && !body.variables.iter().any(|local| local.text == name.text)
                    {
                        self.capacity(
                            body.variables.len(),
                            MAX_PATTERN_VERTICES,
                            PatternLimitDimension::Vertices,
                        )?;
                        body.variables.push(name);
                        body.captures.push(name);
                    }
                }
            }
            Ok::<_, GraphPatternTextError>((body, path, edges, nested))
        })();
        self.outer_anonymous.truncate(anonymous_at);
        self.read_row_bindings = row_bindings;
        let (body, path, edges, nested) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                self.syntax.scopes.truncate(scopes_at);
                self.restore_pattern(outer);
                return Err(error);
            }
        };
        self.restore_pattern(outer);
        if let Some(path) = path {
            if self
                .syntax
                .variables
                .iter()
                .any(|seen| seen.text == path.text)
                || self.syntax.names_path(path.text)
                || self.syntax.visible_edge(path.text).is_some()
            {
                return Err(error(
                    path.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateVariable),
                ));
            }
            // A comprehension's path is its own local, never exported.
            if !comprehension {
                self.syntax.scope_paths.push(path);
                if edges {
                    self.syntax.scope_edge_paths.push(path.text);
                }
            }
        }
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
        self.syntax.scopes.push(ScopeSyntax {
            kind,
            body,
            nested,
            element: None,
            tuple: None,
        });
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeForm {
    Clause,
    Pattern,
    /// A pattern comprehension's body (fgdb-7nwox).
    Comprehension,
}

/// Whether `first`, with `lexer` just after it, starts an existence
/// predicate: EXISTS, NOT EXISTS, or a pattern predicate or its anti form.
fn existence_follows(
    first: TokenKind<'_>,
    lexer: &Lexer<'_>,
) -> Result<bool, GraphPatternTextError> {
    let mut lookahead = lexer.clone();
    match first {
        TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS") => {
            exists_introducer_follows(&mut lookahead)
        }
        TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT") => {
            let next = lookahead.next()?;
            if matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS")) {
                return exists_introducer_follows(&mut lookahead);
            }
            pattern_predicate_follows(next.kind, &mut lookahead)
        }
        first => pattern_predicate_follows(first, &mut lookahead),
    }
}

/// After an `EXISTS` word: `{` (a subquery), or `(` whose content is a
/// pattern predicate (openCypher's `exists((n)-[...]->(...))`). A property
/// test such as `exists(n.p)` is neither.
pub(super) fn exists_introducer_follows(
    lookahead: &mut Lexer<'_>,
) -> Result<bool, GraphPatternTextError> {
    match lookahead.next()?.kind {
        TokenKind::Punct(b'{') => Ok(true),
        TokenKind::Punct(b'(') => {
            let first = lookahead.next()?;
            pattern_predicate_follows(first.kind, lookahead)
        }
        _ => Ok(false),
    }
}

/// Whether `first` and the tokens after it spell an openCypher pattern
/// predicate: a parenthesised node pattern (variable, labels, property map;
/// no `.`, operator or literal outside the map) followed by an edge
/// introducer. `-` then `[`, `-` or `>`, or `<` `-` then `[` or `-`, never
/// arithmetic `(x) - 2` or a comparison `(x) <-3`.
pub(super) fn pattern_predicate_follows(
    first: TokenKind<'_>,
    lookahead: &mut Lexer<'_>,
) -> Result<bool, GraphPatternTextError> {
    if !matches!(first, TokenKind::Punct(b'(')) {
        return Ok(false);
    }
    let mut map_depth = 0_usize;
    loop {
        match lookahead.next()?.kind {
            TokenKind::End => return Ok(false),
            TokenKind::Punct(b')') if map_depth == 0 => break,
            TokenKind::Punct(b'{') => map_depth += 1,
            TokenKind::Punct(b'}') => {
                let Some(outer) = map_depth.checked_sub(1) else {
                    return Ok(false);
                };
                map_depth = outer;
            }
            _ if map_depth > 0 => {}
            TokenKind::Word(_) | TokenKind::Punct(b':' | b'|' | b'&' | b'!') => {}
            _ => return Ok(false),
        }
    }
    Ok(match (lookahead.next()?.kind, lookahead.next()?.kind) {
        (TokenKind::Punct(b'-'), TokenKind::Punct(b'[' | b'-' | b'>')) => true,
        (TokenKind::Punct(b'<'), TokenKind::Punct(b'-')) => {
            matches!(lookahead.next()?.kind, TokenKind::Punct(b'[' | b'-'))
        }
        _ => false,
    })
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::GqlQueryPolicy;
    use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};

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
        let values: BTreeMap<VId, CanonicalPropertyValue> = values
            .iter()
            .map(|(vid, value)| (*vid, value.clone().into()))
            .collect();
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
    fn stored_literal_predicates_capture_each_outer_variable_in_reference_order() {
        fn name(text: &'static str) -> Name<'static> {
            Name { text, at: 0 }
        }
        let key = name("n");
        let predicate = |variable: &'static str| boolean::SyntaxItem::StoredProperty {
            variable: name(variable),
            key,
            value: crate::set_text::ReadValueTemplate::List(Vec::new()),
        };
        let mut variables = vec![name("local")];
        let filters = [Filter::Boolean {
            program: vec![
                predicate("outer_b"),
                predicate("outer_a"),
                boolean::SyntaxItem::And,
                predicate("outer_b"),
                boolean::SyntaxItem::And,
            ],
            at: 0,
        }];
        let captures = predicate_captures(&mut variables, &filters, &[]).unwrap();
        assert_eq!(
            variables.iter().map(|name| name.text).collect::<Vec<_>>(),
            vec!["local", "outer_b", "outer_a"]
        );
        assert_eq!(
            captures.iter().map(|name| name.text).collect::<Vec<_>>(),
            vec!["outer_b", "outer_a"]
        );
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
                columns: vec![
                    (variables[0], Some(key)),
                    (last, Some(key)),
                    (last, Some(key)),
                ],
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
                    Some(key),
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
