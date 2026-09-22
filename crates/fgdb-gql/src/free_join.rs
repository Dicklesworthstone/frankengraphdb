//! Loom's conjunctive inner-join physical family.
//!
//! A checked plan partitions variables into covered groups. Singleton groups
//! select Generic Join; larger groups express binary/FreeJoin access. Every
//! group proposes from its smallest covering projection and probes every other
//! participating relation. There is no binary Cartesian intermediate.
//!
//! Inputs are already admitted relations, not an authorization interface. A
//! source adapter must preserve its pinned generation, canonical key order and
//! exact multiplicities. NULL-sensitive, optional, negative and correlated
//! semantics remain GLA operators outside this inner-join kernel.
//!
//! All data-dependent loops cross the existing GLA work/scratch control seam.
//! These are logical admission units, NOT allocator-byte or I/O accounting.
//! Singleton-group execution has Generic Join's worst-case support bound up to
//! ordered-search logarithms, after the separately accounted trie preparation.
//! A grouped FreeJoin plan does not inherit that bound merely from its name.

mod execute;
mod plan;
mod trie;

pub use execute::{FreeJoin, JoinBinding, MultiplicityOverflow};
pub use plan::{FreeJoinPlan, JoinPlanError, JoinVariable, MAX_JOIN_RELATIONS, MAX_JOIN_VARIABLES};
pub use trie::{ColumnarCursor, ColumnarTrie, TrieBuildError, TrieCursor, TrieRelation};
