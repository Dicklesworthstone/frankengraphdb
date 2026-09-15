//! Non-authoritative result values for successfully staged mixed write programs.
//!
//! Receipts expose identities needed by an embedded caller without claiming a
//! commit, durable operation ID, proof, replay token, or allocator rollback.
//! The fgdb transaction adapter constructs them only after the complete program
//! crosses its final acceptance boundary. Debug never exposes graph identities.

use crate::GraphWriteProgramStats;
use fgdb_types::{EId, VId};

#[derive(Clone, PartialEq, Eq)]
pub enum GraphWriteStepReceipt {
    /// Distinct canonical mutation targets, sorted by vertex identity.
    Mutation { targets: Vec<VId> },
    /// Created IDs in occurrence/declaration order for this insertion step.
    Insert { vertices: Vec<VId>, edges: Vec<EId> },
}

impl GraphWriteStepReceipt {
    #[must_use]
    pub fn mutation_targets(&self) -> Option<&[VId]> {
        match self {
            Self::Mutation { targets } => Some(targets),
            Self::Insert { .. } => None,
        }
    }

    #[must_use]
    pub fn created_vertices(&self) -> Option<&[VId]> {
        match self {
            Self::Insert { vertices, .. } => Some(vertices),
            Self::Mutation { .. } => None,
        }
    }

    #[must_use]
    pub fn created_edges(&self) -> Option<&[EId]> {
        match self {
            Self::Insert { edges, .. } => Some(edges),
            Self::Mutation { .. } => None,
        }
    }
}

impl core::fmt::Debug for GraphWriteStepReceipt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mutation { targets } => f
                .debug_struct("Mutation")
                .field("targets", &targets.len())
                .field("identities", &"[REDACTED]")
                .finish(),
            Self::Insert { vertices, edges } => f
                .debug_struct("Insert")
                .field("vertices", &vertices.len())
                .field("edges", &edges.len())
                .field("identities", &"[REDACTED]")
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct GraphWriteProgramReceipt {
    stats: GraphWriteProgramStats,
    steps: Vec<GraphWriteStepReceipt>,
}

impl GraphWriteProgramReceipt {
    /// Construct an application result value. This does not validate or grant
    /// storage authority; execution adapters own the correspondence to stats.
    #[must_use]
    pub fn new(stats: GraphWriteProgramStats, steps: Vec<GraphWriteStepReceipt>) -> Self {
        Self { stats, steps }
    }

    #[must_use]
    pub const fn stats(&self) -> GraphWriteProgramStats {
        self.stats
    }

    #[must_use]
    pub fn steps(&self) -> &[GraphWriteStepReceipt] {
        &self.steps
    }
}

impl core::fmt::Debug for GraphWriteProgramReceipt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphWriteProgramReceipt")
            .field("completed_statements", &self.stats.completed_statements)
            .field("mutation_effects", &self.stats.mutation_effects)
            .field("created_vertices", &self.stats.created_vertices)
            .field("created_edges", &self.stats.created_edges)
            .field("steps", &self.steps)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GlaExecutionStats, GqlExecutionStats};

    #[test]
    fn accessors_preserve_step_kind_and_debug_redacts_identities() {
        let mutation = GraphWriteStepReceipt::Mutation { targets: vec![VId(u128::MAX)] };
        assert_eq!(mutation.mutation_targets(), Some(&[VId(u128::MAX)][..]));
        assert_eq!(mutation.created_vertices(), None);
        let insert = GraphWriteStepReceipt::Insert {
            vertices: vec![VId(u128::MAX - 1)],
            edges: vec![EId(u128::MAX - 2)],
        };
        assert_eq!(insert.created_vertices(), Some(&[VId(u128::MAX - 1)][..]));
        assert_eq!(insert.created_edges(), Some(&[EId(u128::MAX - 2)][..]));
        let stats = GraphWriteProgramStats {
            completed_statements: 2,
            selection: GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
            evaluator: GlaExecutionStats::default(),
            target_vertex_visits: 1,
            mutation_effects: 1,
            created_vertices: 1,
            created_edges: 1,
        };
        let receipt = GraphWriteProgramReceipt::new(stats, vec![mutation, insert]);
        assert_eq!(receipt.stats(), stats);
        assert_eq!(receipt.steps().len(), 2);
        let debug = format!("{receipt:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(&u128::MAX.to_string()));
    }
}
