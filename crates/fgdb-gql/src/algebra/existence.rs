//! Ordered graph clauses over positive graph patterns.

use super::GraphPatternBuilder;

/// One scoped EXISTS or NOT EXISTS constraint. Matching variable names in the
/// outer and inner builders are correlations; other inner variables are local.
/// Local variables never become outer projections or correlations for a later
/// constraint. This is not an optional/null-producing join.
#[derive(Clone, Copy)]
pub struct GraphExistence<'a> {
    pub(crate) pattern: &'a GraphPatternBuilder,
    pub(crate) anti: bool,
}

impl<'a> GraphExistence<'a> {
    #[must_use]
    pub const fn exists(pattern: &'a GraphPatternBuilder) -> Self {
        Self {
            pattern,
            anti: false,
        }
    }
    #[must_use]
    pub const fn not_exists(pattern: &'a GraphPatternBuilder) -> Self {
        Self {
            pattern,
            anti: true,
        }
    }
}

impl core::fmt::Debug for GraphExistence<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphExistence")
            .field("negated", &self.anti)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GraphMatchKind {
    Required,
    Optional,
    Exists,
    NotExists,
    /// A pattern comprehension's scope (fgdb-7nwox).
    Collect,
}

/// An ordered graph clause. Required MATCH and OPTIONAL export their new
/// vertex variables to later clauses and output. Required MATCH retains every
/// complete witness and eliminates an incoming row on absence; OPTIONAL instead
/// null-extends that row once. EXISTS and NOT EXISTS keep new names clause-local
/// and never multiply the incoming bag. Shared names are bound correlations;
/// a child without shared names is independent. A null correlation cannot be
/// rebound, including by a zero-hop walk. Predicates belong to their own clause.
#[derive(Clone)]
pub struct GraphMatchClause<'a> {
    pub(crate) pattern: &'a GraphPatternBuilder,
    pub(crate) kind: GraphMatchKind,
    /// A Collect clause's element: the local vertex, relationship or path
    /// each witness contributes, or none (each contributes NULL).
    pub(crate) element: Option<&'a str>,
    /// Predicates evaluated inside this clause, before its witness is accepted.
    /// Their locals and transient bindings never enter the containing scope.
    pub(crate) nested: Vec<GraphMatchClause<'a>>,
}

impl<'a> GraphMatchClause<'a> {
    /// Join this positive pattern to each incoming occurrence at this point in
    /// the clause sequence. This does not flatten or move a preceding OPTIONAL,
    /// and failure of this clause never turns an earlier witness into absence.
    #[must_use]
    pub const fn required(pattern: &'a GraphPatternBuilder) -> Self {
        Self {
            pattern,
            kind: GraphMatchKind::Required,
            element: None,
            nested: Vec::new(),
        }
    }

    #[must_use]
    pub const fn optional(pattern: &'a GraphPatternBuilder) -> Self {
        Self {
            pattern,
            kind: GraphMatchKind::Optional,
            element: None,
            nested: Vec::new(),
        }
    }
    #[must_use]
    pub const fn exists(pattern: &'a GraphPatternBuilder) -> Self {
        Self {
            pattern,
            kind: GraphMatchKind::Exists,
            element: None,
            nested: Vec::new(),
        }
    }
    #[must_use]
    pub const fn not_exists(pattern: &'a GraphPatternBuilder) -> Self {
        Self {
            pattern,
            kind: GraphMatchKind::NotExists,
            element: None,
            nested: Vec::new(),
        }
    }
    /// A pattern comprehension's scope (fgdb-7nwox): once per incoming
    /// occurrence, the list of `element` over every witness, in enumeration
    /// order. Its names stay local, like an existential's, and it never
    /// multiplies or eliminates the incoming row.
    #[must_use]
    pub const fn collect(pattern: &'a GraphPatternBuilder, element: Option<&'a str>) -> Self {
        Self {
            pattern,
            kind: GraphMatchKind::Collect,
            element,
            nested: Vec::new(),
        }
    }
}

impl core::fmt::Debug for GraphMatchClause<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphMatchClause")
            .field("kind", &self.kind)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
