//! Native relational stages, binary sets, Cartesian products and intermediate
//! numeric aggregation and correlated Apply for host-owned external execution.
//!
//! A checked plan retains every admitted graph or native singleton source and
//! every relational ordering, DISTINCT and pagination barrier. Binary plans
//! own both complete children and share the native resident merge transition.
//! Cartesian products preserve child order and share the native pair-copy kernel.
//! Aggregate plans share the exact native reducer and checked row conversion.
//! Inner/optional/collected Apply retains per-occurrence lexical argument
//! frames and complete local body barriers. Collected outputs keep mapped
//! NULLs and duplicates in body order under the native value bounds.
//! The host owns scratch files, memory reservations and one cumulative query
//! allowance. Row stages use ordinary native expression and predicate evaluators.

pub use crate::set_ops::spill::{
    AsyncSpillElementSource, AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetAggregatePlan,
    SpillSetAggregateRowError, SpillSetApplyKind, SpillSetApplyPlan, SpillSetArgumentsPlan,
    SpillSetBinaryPlan, SpillSetBuildError, SpillSetCollection, SpillSetCrossPlan,
    SpillSetJoinPlan, SpillSetMergeStep, SpillSetRowError, SpillSetRows, SpillSetStage,
    SpillSetValuesPlan,
};
