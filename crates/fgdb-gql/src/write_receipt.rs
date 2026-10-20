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
    /// Distinct canonical mutation targets, sorted within each identity domain.
    Mutation {
        targets: Vec<VId>,
        edges: Vec<EId>,
    },
    /// Created IDs in occurrence/declaration order for this insertion step.
    Insert {
        vertices: Vec<VId>,
        edges: Vec<EId>,
    },
    VertexMerge {
        outcome: GraphVertexMergeOutcome,
    },
    /// A vertex MERGE over input rows (a chained write, fgdb-cpny9): one
    /// outcome per distinct property tuple and vertex, tuples in first-row
    /// order and each tuple's vertices in identity order. `created` retains
    /// exactly the Created IDs in that order.
    VertexMergeRows {
        outcomes: Vec<GraphVertexMergeOutcome>,
        created: Vec<VId>,
    },
    VertexUpsert {
        outcome: GraphVertexMergeOutcome,
    },
    /// Multiple existing vertex matches in full identity order. Native
    /// execution uses the singular arm for one match or the one creation;
    /// every outcome in this arm is Matched, never an invented creation.
    VertexMergeBatch {
        outcomes: Vec<GraphVertexMergeOutcome>,
    },
    /// The same frozen distinct target order, after ON/SET on every vertex.
    /// Non-authoritative like all receipts; not a fresh snapshot of the IDs.
    VertexUpsertBatch {
        outcomes: Vec<GraphVertexMergeOutcome>,
    },
    EdgeMerge {
        outcome: GraphEdgeMergeOutcome,
    },
    /// One outcome per distinct directed endpoint pair, in canonical pair
    /// order. Only a multi-pair plain MERGE uses this arm; zero/one pair keep
    /// `EdgeMerge`. `created` retains exactly the Created IDs in that order.
    EdgeMergeBatch {
        outcomes: Vec<GraphEdgeMergeOutcome>,
        created: Vec<EId>,
    },
    EdgeUpsert {
        outcome: GraphEdgeMergeOutcome,
    },
    /// ON/SET outcomes in MATCH occurrence order, then matched identity order.
    /// Unlike plain MERGE, repeated endpoint pairs and identities are retained:
    /// each represents another execution of the selected branch and actions.
    /// `created` contains only creation outcomes, in that same order. Native
    /// zero/one-outcome steps retain `EdgeUpsert`, including NoInput.
    EdgeUpsertBatch {
        outcomes: Vec<GraphEdgeMergeOutcome>,
        created: Vec<EId>,
    },
    /// Distinct non-detaching DELETE targets, sorted within each identity domain.
    Delete {
        targets: Vec<VId>,
        edges: Vec<EId>,
    },
}

impl GraphWriteStepReceipt {
    #[must_use]
    pub fn mutation_targets(&self) -> Option<&[VId]> {
        match self {
            Self::Mutation { targets, .. } | Self::Delete { targets, .. } => Some(targets),
            Self::Insert { .. }
            | Self::VertexMerge { .. }
            | Self::VertexMergeRows { .. }
            | Self::VertexUpsert { .. }
            | Self::VertexMergeBatch { .. }
            | Self::VertexUpsertBatch { .. }
            | Self::EdgeMerge { .. }
            | Self::EdgeMergeBatch { .. }
            | Self::EdgeUpsert { .. }
            | Self::EdgeUpsertBatch { .. } => None,
        }
    }

    #[must_use]
    pub fn mutation_edges(&self) -> Option<&[EId]> {
        match self {
            Self::Mutation { edges, .. } | Self::Delete { edges, .. } => Some(edges),
            _ => None,
        }
    }

    /// Exact targets for a plain DELETE step, including Some(empty) when no
    /// vertex matched. This does not describe arbitrary DETACH mutation steps.
    #[must_use]
    pub fn deleted_vertices(&self) -> Option<&[VId]> {
        match self {
            Self::Delete { targets, .. } => Some(targets),
            _ => None,
        }
    }

    #[must_use]
    pub fn deleted_edges(&self) -> Option<&[EId]> {
        match self {
            Self::Delete { edges, .. } => Some(edges),
            _ => None,
        }
    }

    /// Creation-bearing step kinds return Some, including an empty slice for a
    /// matched MERGE. Matched identities are available through merged_vertex.
    #[must_use]
    pub fn created_vertices(&self) -> Option<&[VId]> {
        match self {
            Self::Insert { vertices, .. } => Some(vertices),
            Self::VertexMergeRows { created, .. } => Some(created),
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
            }
            | Self::VertexMergeBatch { .. }
            | Self::VertexUpsertBatch { .. } => Some(&[]),
            Self::Mutation { .. }
            | Self::EdgeMerge { .. }
            | Self::EdgeMergeBatch { .. }
            | Self::EdgeUpsert { .. }
            | Self::EdgeUpsertBatch { .. }
            | Self::Delete { .. } => None,
        }
    }

    #[must_use]
    pub fn created_edges(&self) -> Option<&[EId]> {
        match self {
            Self::Insert { edges, .. } => Some(edges),
            Self::EdgeMergeBatch { created, .. } | Self::EdgeUpsertBatch { created, .. } => {
                Some(created)
            }
            Self::EdgeMerge {
                outcome: GraphEdgeMergeOutcome::Created(edge),
            }
            | Self::EdgeUpsert {
                outcome: GraphEdgeMergeOutcome::Created(edge),
            } => Some(core::slice::from_ref(edge)),
            Self::EdgeMerge { .. } | Self::EdgeUpsert { .. } => Some(&[]),
            Self::Mutation { .. }
            | Self::VertexMerge { .. }
            | Self::VertexMergeRows { .. }
            | Self::VertexUpsert { .. }
            | Self::VertexMergeBatch { .. }
            | Self::VertexUpsertBatch { .. }
            | Self::Delete { .. } => None,
        }
    }

    /// A row-wise vertex MERGE returns None; inspect `merged_vertices`.
    #[must_use]
    pub const fn merged_vertex(&self) -> Option<GraphVertexMergeOutcome> {
        match self {
            Self::VertexMerge { outcome } | Self::VertexUpsert { outcome } => Some(*outcome),
            Self::Mutation { .. }
            | Self::Insert { .. }
            | Self::VertexMergeRows { .. }
            | Self::VertexMergeBatch { .. }
            | Self::VertexUpsertBatch { .. }
            | Self::EdgeMerge { .. }
            | Self::EdgeMergeBatch { .. }
            | Self::EdgeUpsert { .. }
            | Self::EdgeUpsertBatch { .. }
            | Self::Delete { .. } => None,
        }
    }

    /// Complete bulk vertex results; a multiple-match receipt never silently
    /// selects one identity through `merged_vertex`. Singular receipts keep
    /// that accessor, including their one possible creation. A vertex MERGE
    /// over input rows (fgdb-cpny9) reports its tuple and vertex outcomes.
    #[must_use]
    pub fn merged_vertices(&self) -> Option<&[GraphVertexMergeOutcome]> {
        match self {
            Self::VertexMergeBatch { outcomes }
            | Self::VertexUpsertBatch { outcomes }
            | Self::VertexMergeRows { outcomes, .. } => Some(outcomes),
            _ => None,
        }
    }

    /// Some(NoInput) distinguishes a relationship step with no endpoints from
    /// a different statement kind; Some(Matched(_)) never claims a creation.
    /// A bulk receipt returns None; inspect `merged_edges` for its results.
    #[must_use]
    pub const fn merged_edge(&self) -> Option<GraphEdgeMergeOutcome> {
        match self {
            Self::EdgeMerge { outcome } | Self::EdgeUpsert { outcome } => Some(*outcome),
            Self::Mutation { .. }
            | Self::Insert { .. }
            | Self::VertexMerge { .. }
            | Self::VertexMergeRows { .. }
            | Self::VertexUpsert { .. }
            | Self::VertexMergeBatch { .. }
            | Self::VertexUpsertBatch { .. }
            | Self::EdgeMergeBatch { .. }
            | Self::EdgeUpsertBatch { .. }
            | Self::Delete { .. } => None,
        }
    }

    /// Bulk relationship outcomes: canonical pair order for plain MERGE, or
    /// occurrence order (including repeats) for ON/SET upserts. Singular
    /// receipts retain their existing `merged_edge` accessor.
    #[must_use]
    pub fn merged_edges(&self) -> Option<&[GraphEdgeMergeOutcome]> {
        match self {
            Self::EdgeMergeBatch { outcomes, .. } | Self::EdgeUpsertBatch { outcomes, .. } => {
                Some(outcomes)
            }
            _ => None,
        }
    }
}

impl core::fmt::Debug for GraphWriteStepReceipt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mutation { targets, edges } => f
                .debug_struct("Mutation")
                .field("targets", &targets.len())
                .field("edges", &edges.len())
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
            Self::VertexMergeRows { outcomes, created } => f
                .debug_struct("VertexMergeRows")
                .field("outcomes", &outcomes.len())
                .field("created", &created.len())
                .field("identities", &"[REDACTED]")
                .finish(),
            Self::VertexUpsert { outcome } => f
                .debug_struct("VertexUpsert")
                .field("outcome", outcome)
                .finish(),
            Self::VertexMergeBatch { outcomes } => f
                .debug_struct("VertexMergeBatch")
                .field("targets", &outcomes.len())
                .field("identities", &"[REDACTED]")
                .finish(),
            Self::VertexUpsertBatch { outcomes } => f
                .debug_struct("VertexUpsertBatch")
                .field("targets", &outcomes.len())
                .field("identities", &"[REDACTED]")
                .finish(),
            Self::EdgeMerge { outcome } => f
                .debug_struct("EdgeMerge")
                .field("outcome", outcome)
                .finish(),
            Self::EdgeMergeBatch { outcomes, created } => f
                .debug_struct("EdgeMergeBatch")
                .field("pairs", &outcomes.len())
                .field("created", &created.len())
                .field("identities", &"[REDACTED]")
                .finish(),
            Self::EdgeUpsert { outcome } => f
                .debug_struct("EdgeUpsert")
                .field("outcome", outcome)
                .finish(),
            Self::EdgeUpsertBatch { outcomes, created } => f
                .debug_struct("EdgeUpsertBatch")
                .field("occurrences", &outcomes.len())
                .field("created", &created.len())
                .field("identities", &"[REDACTED]")
                .finish(),
            Self::Delete { targets, edges } => f
                .debug_struct("Delete")
                .field("targets", &targets.len())
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
    fn bulk_vertex_receipts_keep_all_matches_without_claiming_creations() {
        let outcomes = vec![
            GraphVertexMergeOutcome::Matched(VId(1_u128 << 96)),
            GraphVertexMergeOutcome::Matched(VId(u128::MAX)),
        ];
        for receipt in [
            GraphWriteStepReceipt::VertexMergeBatch {
                outcomes: outcomes.clone(),
            },
            GraphWriteStepReceipt::VertexUpsertBatch {
                outcomes: outcomes.clone(),
            },
        ] {
            assert_eq!(receipt.merged_vertices(), Some(outcomes.as_slice()));
            assert_eq!(receipt.merged_vertex(), None);
            assert_eq!(receipt.created_vertices(), Some(&[][..]));
            assert_eq!(receipt.created_edges(), None);
            assert_eq!(receipt.mutation_targets(), None);
            assert_eq!(receipt.merged_edge(), None);
            let debug = format!("{receipt:?}");
            assert!(debug.contains("targets: 2"));
            assert!(debug.contains("[REDACTED]"));
            assert!(!debug.contains(&u128::MAX.to_string()));
        }
    }

    #[test]
    fn batch_relationship_receipt_preserves_order_and_separates_created_ids() {
        let created = EId(u128::MAX);
        let matched = EId(u128::MAX - 1);
        let outcomes = vec![
            GraphEdgeMergeOutcome::Created(created),
            GraphEdgeMergeOutcome::Matched(matched),
        ];
        let receipt = GraphWriteStepReceipt::EdgeMergeBatch {
            outcomes: outcomes.clone(),
            created: vec![created],
        };
        assert_eq!(receipt.merged_edges(), Some(outcomes.as_slice()));
        assert_eq!(receipt.created_edges(), Some(&[created][..]));
        assert_eq!(receipt.merged_edge(), None);
        assert_eq!(receipt.merged_vertex(), None);
        assert_eq!(receipt.created_vertices(), None);
        assert_eq!(receipt.mutation_targets(), None);
        assert_eq!(receipt.mutation_edges(), None);
        let debug = format!("{receipt:?}");
        assert!(debug.contains("pairs: 2"));
        assert!(debug.contains("created: 1"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(&created.0.to_string()));
        assert!(!debug.contains(&matched.0.to_string()));
    }

    #[test]
    fn accessors_preserve_step_kind_and_debug_redacts_identities() {
        let mutation = GraphWriteStepReceipt::Mutation {
            targets: vec![VId(u128::MAX)],
            edges: Vec::new(),
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
            target_edge_visits: 0,
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
                edges: Vec::new(),
            };
            assert_eq!(step.deleted_vertices(), Some(targets.as_slice()));
            assert_eq!(step.mutation_targets(), Some(targets.as_slice()));
            assert_eq!(step.created_vertices(), None);
            assert_eq!(step.created_edges(), None);
            assert_eq!(step.merged_vertex(), None);
            assert_eq!(step.merged_edge(), None);
            assert!(!format!("{step:?}").contains(&u128::MAX.to_string()));
            let mutation = GraphWriteStepReceipt::Mutation {
                targets,
                edges: Vec::new(),
            };
            assert_ne!(step, mutation);
            assert_eq!(mutation.deleted_vertices(), None);
        }
    }

    #[test]
    fn bulk_upsert_receipts_retain_repeated_occurrences_without_repeating_creations() {
        let created = EId(u128::MAX);
        let matched = EId(u128::MAX - 1);
        let outcomes = vec![
            GraphEdgeMergeOutcome::Created(created),
            GraphEdgeMergeOutcome::Matched(matched),
            GraphEdgeMergeOutcome::Matched(created),
            GraphEdgeMergeOutcome::Matched(matched),
        ];
        let receipt = GraphWriteStepReceipt::EdgeUpsertBatch {
            outcomes: outcomes.clone(),
            created: vec![created],
        };
        assert_eq!(receipt.merged_edges(), Some(outcomes.as_slice()));
        assert_eq!(receipt.created_edges(), Some(&[created][..]));
        assert_eq!(receipt.merged_edge(), None);
        assert_eq!(receipt.merged_vertex(), None);
        assert_eq!(receipt.created_vertices(), None);
        assert_eq!(receipt.mutation_targets(), None);
        assert_eq!(receipt.mutation_edges(), None);
        assert_ne!(
            receipt,
            GraphWriteStepReceipt::EdgeMergeBatch {
                outcomes,
                created: vec![created],
            }
        );
        let debug = format!("{receipt:?}");
        assert!(debug.contains("EdgeUpsertBatch"));
        assert!(debug.contains("occurrences: 4"));
        assert!(debug.contains("created: 1"));
        assert!(!debug.contains(&created.0.to_string()));
        assert!(!debug.contains(&matched.0.to_string()));
    }
}
