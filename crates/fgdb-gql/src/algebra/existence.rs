//! Correlated graph clauses over positive connected graph patterns.

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
        Self { pattern, anti: false }
    }
    #[must_use]
    pub const fn not_exists(pattern: &'a GraphPatternBuilder) -> Self {
        Self { pattern, anti: true }
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
    Optional,
    Exists,
    NotExists,
}

/// An ordered correlated clause. OPTIONAL exports its newly introduced vertex
/// variables to later clauses and output, using real null bindings on absence.
/// EXISTS and NOT EXISTS keep new names clause-local and never multiply the
/// incoming bag. Each positive child must connect to at least one variable
/// already visible at this point; same-spelled visible names are correlations.
/// Predicates inside a child participate in matching, before null extension.
#[derive(Clone, Copy)]
pub struct GraphMatchClause<'a> {
    pub(crate) pattern: &'a GraphPatternBuilder,
    pub(crate) kind: GraphMatchKind,
}

impl<'a> GraphMatchClause<'a> {
    #[must_use]
    pub const fn optional(pattern: &'a GraphPatternBuilder) -> Self {
        Self { pattern, kind: GraphMatchKind::Optional }
    }
    #[must_use]
    pub const fn exists(pattern: &'a GraphPatternBuilder) -> Self {
        Self { pattern, kind: GraphMatchKind::Exists }
    }
    #[must_use]
    pub const fn not_exists(pattern: &'a GraphPatternBuilder) -> Self {
        Self { pattern, kind: GraphMatchKind::NotExists }
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
