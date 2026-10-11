//! Native relational stages, binary sets and intermediate numeric aggregation
//! for host-owned external execution.
//!
//! A checked plan retains every admitted graph or native singleton source and
//! every relational ordering, DISTINCT and pagination barrier. Binary plans
//! own both complete children and share the native resident merge transition.
//! Aggregate plans share the exact native reducer and checked row conversion.
//! The host owns scratch files, memory reservations and one cumulative query
//! allowance. Row stages use ordinary native expression and predicate evaluators.

pub use crate::set_ops::spill::{
    AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetAggregatePlan, SpillSetAggregateRowError,
    SpillSetBinaryPlan, SpillSetBuildError, SpillSetMergeStep, SpillSetRowError, SpillSetRows,
    SpillSetStage, SpillSetValuesPlan,
};
