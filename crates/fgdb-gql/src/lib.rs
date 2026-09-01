//! Bounded GQL parsing, binding, and owned prepared-query definitions.
//!
//! [`PreparedGqlQuery`] is the coherent reusable form: exact statement bytes,
//! the canonical caller-supplied [`RelationBind`], and the executor-ready
//! [`BoundPlan`] are created together and cannot be mutated independently.

#![forbid(unsafe_code)]

mod parser;
mod prepared;

pub use parser::{
    BindError, BoundPlan, EdgeDirection, ParseError, ParseErrorKind, RelationBind,
    ReturnProjection,
};
pub use prepared::{
    BudgetedGqlError, BudgetedGqlExecution, GqlBudgetDimension, GqlBudgetExceeded,
    GqlExecutionBudget, GqlExecutionStats, PreparedGqlQuery,
};
