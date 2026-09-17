//! Non-authoritative result values for successfully staged mixed write programs.
//!
//! Receipts expose identities needed by an embedded caller without claiming a
//! commit, durable operation ID, proof, replay token, or allocator rollback.
//! The fgdb transaction adapter constructs them only after the complete program
//! crosses its final acceptance boundary. Debug never exposes graph identities.

use crate::{GraphEdgeMergeOutcome, GraphVertexMergeOutcome, GraphWriteProgramStats};
use fgdb_types::{EId, VId};

#[derive(Clone, PartialEq, Eq)]
pub enum GraphWriteStepReceipt {
    /// Distinct canonical mutation targets, sorted by vertex identity.
    Mutation {
        targets: Vec<VId>,
    },
    /// Created IDs in occurrence/declaration order for this insertion step.
    Insert {
        vertices: Vec<VId>,
        edges: Vec<EId>,
    },
    VertexMerge {
        outcome: GraphVertexMergeOutcome,
    },
    VertexUpsert {
        outcome: GraphVertexMergeOutcome,
    },
    EdgeMerge {
        outcome: GraphEdgeMergeOutcome,
    },
    EdgeUpsert {
        outcome: GraphEdgeMergeOutcome,
    },
    /// Distinct non-detaching DELETE targets, sorted by vertex identity.
    Delete {
        targets: Vec<VId>,
    },
}

impl GraphWriteStepReceipt {
    #[must_use]
    pub fn mutation_targets(&self) -> Option<&[VId]> {
        match self {
            Self::Mutation { targets } | Self::Delete { targets } => Some(targets),
            Self::Insert { .. }
            | Self::VertexMerge { .. }
            | Self::VertexUpsert { .. }
            | Self::EdgeMerge { .. }
            | Self::EdgeUpsert { .. } => None,
        }
    }

    /// Exact targets for a plain DELETE step, including Some(empty) when no
    /// vertex matched. This does not describe arbitrary DETACH mutation steps.
    #[must_use]
    pub fn deleted_vertices(&self) -> Option<&[VId]> {
        match self {
            Self::Delete { targets } => Some(targets),
            _ => None,
        }
    }

    /// Creation-bearing step kinds return Some, including an empty slice for a
    /// matched MERGE. Matched identities are available through merged_vertex.
    #[must_use]
    pub fn created_vertices(&self) -> Option<&[VId]> {
        match self {
            Self::Insert { vertices, .. } => Some(vertices),
            Self::VertexMerge {
                outcome: GraphVertexMergeOutcome::Created(vertex),
            }
            | Self::VertexUpsert {
                outcome: GraphVertexMergeOutcome::Created(vertex),
            } => Some(core::slice::from_ref(vertex)),
            Self::VertexMerge {
                outcome: GraphVertexMergeOutcome::Matched(_),
            }
            | Self::VertexUpsert {
                outcome: GraphVertexMergeOutcome::Matched(_),
            } => Some(&[]),
            Self::Mutation { .. }
            | Self::EdgeMerge { .. }
            | Self::EdgeUpsert { .. }
            | Self::Delete { .. } => None,
        }
    }

    #[must_use]
    pub fn created_edges(&self) -> Option<&[EId]> {
        match self {
            Self::Insert { edges, .. } => Some(edges),
            Self::EdgeMerge {
                outcome: GraphEdgeMergeOutcome::Created(edge),
            }
            | Self::EdgeUpsert {
                outcome: GraphEdgeMergeOutcome::Created(edge),
            } => Some(core::slice::from_ref(edge)),
            Self::EdgeMerge { .. } | Self::EdgeUpsert { .. } => Some(&[]),
            Self::Mutation { .. }
            | Self::VertexMerge { .. }
            | Self::VertexUpsert { .. }
            | Self::Delete { .. } => None,
        }
    }

    #[must_use]
    pub const fn merged_vertex(&self) -> Option<GraphVertexMergeOutcome> {
        match self {
            Self::VertexMerge { outcome } | Self::VertexUpsert { outcome } => Some(*outcome),
            Self::Mutation { .. }
            | Self::Insert { .. }
            | Self::EdgeMerge { .. }
            | Self::EdgeUpsert { .. }
            | Self::Delete { .. } => None,
        }
    }

    /// Some(NoInput) distinguishes a relationship step with no endpoints from
    /// a different statement kind; Some(Matched(_)) never claims a creation.
    #[must_use]
    pub const fn merged_edge(&self) -> Option<GraphEdgeMergeOutcome> {
        match self {
            Self::EdgeMerge { outcome } | Self::EdgeUpsert { outcome } => Some(*outcome),
            Self::Mutation { .. }
            | Self::Insert { .. }
            | Self::VertexMerge { .. }
            | Self::VertexUpsert { .. }
            | Self::Delete { .. } => None,
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
            Self::VertexMerge { outcome } => f
                .debug_struct("VertexMerge")
                .field("outcome", outcome)
                .finish(),
            Self::VertexUpsert { outcome } => f
                .debug_struct("VertexUpsert")
                .field("outcome", outcome)
                .finish(),
            Self::EdgeMerge { outcome } => f
                .debug_struct("EdgeMerge")
                .field("outcome", outcome)
                .finish(),
            Self::EdgeUpsert { outcome } => f
                .debug_struct("EdgeUpsert")
                .field("outcome", outcome)
                .finish(),
            Self::Delete { targets } => f
                .debug_struct("Delete")
                .field("targets", &targets.len())
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
        let mutation = GraphWriteStepReceipt::Mutation {
            targets: vec![VId(u128::MAX)],
        };
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
            selection: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
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

    #[test]
    fn merge_receipts_distinguish_matches_from_creations_without_leaking_ids() {
        let vertex = VId(u128::MAX);
        for step in [
            GraphWriteStepReceipt::VertexMerge {
                outcome: GraphVertexMergeOutcome::Matched(vertex),
            },
            GraphWriteStepReceipt::VertexUpsert {
                outcome: GraphVertexMergeOutcome::Matched(vertex),
            },
        ] {
            assert_eq!(
                step.merged_vertex(),
                Some(GraphVertexMergeOutcome::Matched(vertex))
            );
            assert_eq!(step.created_vertices(), Some(&[][..]));
            assert_eq!(step.created_edges(), None);
            assert!(!format!("{step:?}").contains(&u128::MAX.to_string()));
        }
        for step in [
            GraphWriteStepReceipt::VertexMerge {
                outcome: GraphVertexMergeOutcome::Created(vertex),
            },
            GraphWriteStepReceipt::VertexUpsert {
                outcome: GraphVertexMergeOutcome::Created(vertex),
            },
        ] {
            assert_eq!(
                step.merged_vertex(),
                Some(GraphVertexMergeOutcome::Created(vertex))
            );
            assert_eq!(step.created_vertices(), Some(&[vertex][..]));
            assert!(!format!("{step:?}").contains(&u128::MAX.to_string()));
        }
    }

    #[test]
    fn relationship_receipts_preserve_no_input_matches_and_creations() {
        let edge = EId(u128::MAX);
        for outcome in [
            GraphEdgeMergeOutcome::NoInput,
            GraphEdgeMergeOutcome::Matched(edge),
            GraphEdgeMergeOutcome::Created(edge),
        ] {
            let step = GraphWriteStepReceipt::EdgeMerge { outcome };
            assert_eq!(step.merged_edge(), Some(outcome));
            assert_eq!(
                step.created_edges(),
                Some(if outcome.created() {
                    core::slice::from_ref(&edge)
                } else {
                    &[]
                })
            );
            assert_eq!(step.created_vertices(), None);
            assert_eq!(step.merged_vertex(), None);
            assert!(!format!("{step:?}").contains(&u128::MAX.to_string()));
        }
    }

    #[test]
    fn relationship_upsert_receipts_keep_kind_and_hide_identifiers() {
        let edge = EId(u128::MAX);
        for outcome in [
            GraphEdgeMergeOutcome::NoInput,
            GraphEdgeMergeOutcome::Matched(edge),
            GraphEdgeMergeOutcome::Created(edge),
        ] {
            let step = GraphWriteStepReceipt::EdgeUpsert { outcome };
            assert_eq!(step.merged_edge(), Some(outcome));
            assert_eq!(
                step.created_edges(),
                Some(if outcome.created() {
                    core::slice::from_ref(&edge)
                } else {
                    &[]
                })
            );
            assert_eq!(step.mutation_targets(), None);
            assert_eq!(step.created_vertices(), None);
            assert_eq!(step.merged_vertex(), None);
            assert_ne!(step, GraphWriteStepReceipt::EdgeMerge { outcome });
            assert!(!format!("{step:?}").contains(&u128::MAX.to_string()));
        }
    }

    #[test]
    fn plain_delete_receipts_are_distinct_and_never_expose_identities_in_debug() {
        for targets in [vec![], vec![VId(u128::MAX)]] {
            let step = GraphWriteStepReceipt::Delete {
                targets: targets.clone(),
            };
            assert_eq!(step.deleted_vertices(), Some(targets.as_slice()));
            assert_eq!(step.mutation_targets(), Some(targets.as_slice()));
            assert_eq!(step.created_vertices(), None);
            assert_eq!(step.created_edges(), None);
            assert_eq!(step.merged_vertex(), None);
            assert_eq!(step.merged_edge(), None);
            assert!(!format!("{step:?}").contains(&u128::MAX.to_string()));
            let mutation = GraphWriteStepReceipt::Mutation { targets };
            assert_ne!(step, mutation);
            assert_eq!(mutation.deleted_vertices(), None);
        }
    }
}
