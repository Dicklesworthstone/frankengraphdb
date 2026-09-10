//! Correlated existential constraints over positive connected graph patterns.

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
