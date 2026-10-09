//! Native unary relational stages for host-owned external execution.
//!
//! A checked plan retains one admitted async graph source and every complete
//! relational ordering, DISTINCT and pagination barrier. The host owns scratch
//! files, memory reservations and the single cumulative query allowance. The
//! row stages use the ordinary native expression and predicate evaluators.

pub use crate::set_ops::spill::{
    AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetBuildError, SpillSetRowError, SpillSetStage,
};
