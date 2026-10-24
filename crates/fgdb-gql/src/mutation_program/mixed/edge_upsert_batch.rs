//! Bulk relationship actions in the ordinary mixed-program meter.
//!
//! MATCH occurrences, resolved relationships and distinct modified targets are
//! different counts. Never turn an occurrence bag into a singular branch or
//! fabricate MATCH rows to make the ordinary mutation bound accept it.

use super::*;
use crate::{GraphEdgeUpsertBatchStats, GraphEdgeUpsertBranch};

impl MixedMeter {
    pub(super) fn absorb_edge_upsert_batch<E, A, C>(
        &mut self,
        statement: usize,
        input: &PreparedGraphEdgeUpsert,
        stats: GraphEdgeUpsertBatchStats,
        target_edges: u64,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        let created = u128::from(stats.created_edges);
        let matched = u128::from(stats.matched_occurrences);
        let selected = u128::from(stats.match_selection.result_rows);
        let resolved = created + matched;
        let create_actions = input.action_count(GraphEdgeUpsertBranch::Create) as u128;
        let match_actions = input.action_count(GraphEdgeUpsertBranch::Match) as u128;
        let expected = created * create_actions + matched * match_actions;
        let modified =
            created * u128::from(create_actions != 0) + matched * u128::from(match_actions != 0);
        let targets = u128::from(target_edges);
        // Each occurrence resolves at least once. A creation answers one real
        // input, and each match came from an examined overlay edge. Multiple
        // matches and repeated identities are legal, but invented branches,
        // omitted actions and invented distinct targets are not.
        if created > selected
            || resolved < selected
            || matched > u128::from(stats.overlay_edges)
            || (selected == 0 && resolved != 0)
            || u128::from(stats.action_effects) != expected
            || targets > modified
            || (modified != 0 && targets == 0)
            || targets < created * u128::from(create_actions != 0)
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        // Literal clauses have a known payload cost. Dynamic clauses must pay
        // at least for every proposal and their acceptance event. These are
        // lower bounds: the batch also owns selection, existence scans, input
        // and receipt storage. The singular adapter keeps its exact-cost law.
        let usage = |branch| {
            input.fixed_action_usage(branch).unwrap_or_else(|| {
                let actions = input.action_count(branch) as u64;
                GlaExecutionStats {
                    work_units: actions + 1,
                    scratch_entries: actions,
                }
            })
        };
        let create_usage = usage(GraphEdgeUpsertBranch::Create);
        let match_usage = usage(GraphEdgeUpsertBranch::Match);
        let action_work = created * u128::from(create_usage.work_units)
            + matched * u128::from(match_usage.work_units);
        let action_scratch = created * u128::from(create_usage.scratch_entries)
            + matched * u128::from(match_usage.scratch_entries);
        if u128::from(stats.evaluator.work_units) <= action_work
            || u128::from(stats.evaluator.scratch_entries) < action_scratch
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        let effects = self.common.add(
            statement,
            GraphMutationProgramDimension::Effects,
            stats.action_effects,
        )?;
        let targets = self
            .common
            .stats
            .target_edge_visits
            .checked_add(target_edges)
            .ok_or(GraphMutationProgramError::InvalidStatistics { statement })?;
        // This existing absorber checks empty input, source addition, creation
        // counts and all cumulative query counters. Install action/target counts
        // only after it succeeds, so a refused step changes no meter state.
        self.absorb_edge_merge(
            statement,
            GraphEdgeMergeStats {
                match_selection: stats.match_selection,
                overlay_edges: stats.overlay_edges,
                created_edges: stats.created_edges,
                evaluator: stats.evaluator,
            },
        )?;
        self.common.stats.effects = effects;
        self.common.stats.target_edge_visits = targets;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeUpsertText};
    use fgdb_delta_types::PropertyKeyId;

    fn input() -> PreparedGraphEdgeUpsert {
        PreparedGraphEdgeUpsertText::prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.p=1,r.q=2 ON MATCH SET r.p=3 SET r.s=4",
            RelationId(1),
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
                (GraphSymbolKind::Property, "s") => Some(GraphSymbol::Property(PropertyKeyId(3))),
                _ => None,
            },
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }

    fn stats() -> GraphEdgeUpsertBatchStats {
        GraphEdgeUpsertBatchStats {
            match_selection: GqlExecutionStats {
                snapshot_records: 7,
                result_rows: 3,
            },
            overlay_edges: 9,
            created_edges: 1,
            matched_occurrences: 3,
            action_effects: 9,
            evaluator: GlaExecutionStats {
                work_units: 60,
                scratch_entries: 40,
            },
        }
    }

    fn policy() -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(GqlQueryPolicy::new(32, 6, 120, 80), 18, 0, 2)
    }

    fn meter(policy: GraphWriteProgramPolicy) -> MixedMeter {
        MixedMeter {
            common: ProgramMeter {
                policy: policy.mutations,
                stats: GraphMutationProgramStats::default(),
            },
            policy,
            vertices: 0,
            edges: 0,
        }
    }

    #[test]
    fn bulk_actions_preserve_real_selection_and_count_distinct_targets_per_step() {
        let input = input();
        let mut meter = meter(policy());
        for statement in 0..2 {
            meter
                .absorb_edge_upsert_batch::<(), (), ()>(statement, &input, stats(), 3)
                .unwrap();
        }
        assert_eq!(meter.edges, 2);
        assert_eq!(meter.common.stats.completed_statements, 2);
        assert_eq!(meter.common.stats.selection.snapshot_records, 32);
        assert_eq!(meter.common.stats.selection.result_rows, 6);
        assert_eq!(meter.common.stats.effects, 18);
        assert_eq!(meter.common.stats.target_edge_visits, 6);
        assert_eq!(meter.common.stats.target_vertex_visits, 0);
        assert_eq!(meter.common.stats.evaluator.work_units, 120);
        assert_eq!(meter.common.stats.evaluator.scratch_entries, 80);
    }

    #[test]
    fn malformed_bulk_statistics_refuse_without_installing_any_counters() {
        let input = input();
        let baseline = stats();
        let mut cases = vec![
            (
                GraphEdgeUpsertBatchStats {
                    action_effects: 8,
                    ..baseline
                },
                3,
            ),
            (
                GraphEdgeUpsertBatchStats {
                    created_edges: 4,
                    ..baseline
                },
                3,
            ),
            (
                GraphEdgeUpsertBatchStats {
                    matched_occurrences: 0,
                    ..baseline
                },
                3,
            ),
            (
                GraphEdgeUpsertBatchStats {
                    overlay_edges: 2,
                    ..baseline
                },
                3,
            ),
            (baseline, 0),
            (baseline, 5),
        ];
        let mut empty = baseline;
        empty.match_selection.result_rows = 0;
        cases.push((empty, 3));
        let mut low_work = baseline;
        low_work.evaluator.work_units = 13;
        cases.push((low_work, 3));
        let mut low_scratch = baseline;
        low_scratch.evaluator.scratch_entries = 8;
        cases.push((low_scratch, 3));
        for (stats, targets) in cases {
            let mut meter = meter(policy());
            let before = meter.common.stats;
            assert!(matches!(
                meter.absorb_edge_upsert_batch::<(), (), ()>(0, &input, stats, targets),
                Err(GraphWriteProgramError::Program(
                    GraphMutationProgramError::InvalidStatistics { statement: 0 }
                ))
            ));
            assert_eq!(meter.common.stats, before);
            assert_eq!(meter.edges, 0);
        }
    }

    #[test]
    fn every_cumulative_cap_applies_across_bulk_statement_boundaries() {
        let input = input();
        let exact = policy();
        let mut policies = Vec::new();
        for query in [
            GqlQueryPolicy::new(31, 6, 120, 80),
            GqlQueryPolicy::new(32, 5, 120, 80),
            GqlQueryPolicy::new(32, 6, 119, 80),
            GqlQueryPolicy::new(32, 6, 120, 79),
        ] {
            policies.push(GraphWriteProgramPolicy::new(query, 18, 0, 2));
        }
        policies.push(GraphWriteProgramPolicy::new(
            exact.mutations.query,
            17,
            0,
            2,
        ));
        policies.push(GraphWriteProgramPolicy::new(
            exact.mutations.query,
            18,
            0,
            1,
        ));
        for policy in policies {
            let mut meter = meter(policy);
            meter
                .absorb_edge_upsert_batch::<(), (), ()>(0, &input, stats(), 3)
                .unwrap();
            let before = meter.common.stats;
            let result = meter.absorb_edge_upsert_batch::<(), (), ()>(1, &input, stats(), 3);
            assert!(matches!(
                result,
                Err(GraphWriteProgramError::Program(
                    GraphMutationProgramError::Budget { statement: 1, .. }
                )) | Err(GraphWriteProgramError::CreationBudget { statement: 1, .. })
            ));
            assert_eq!(meter.common.stats, before);
            assert_eq!(meter.edges, 1);
        }
    }

    #[test]
    fn empty_input_and_actionless_branches_do_not_invent_mutation_targets() {
        let original = input();
        let actionless = PreparedGraphEdgeUpsert::prepare_with_clauses(
            original.merge().clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let mut meter = meter(policy());
        meter
            .absorb_edge_upsert_batch::<(), (), ()>(
                0,
                &actionless,
                GraphEdgeUpsertBatchStats {
                    action_effects: 0,
                    ..stats()
                },
                0,
            )
            .unwrap();
        assert_eq!(meter.common.stats.effects, 0);
        assert_eq!(meter.common.stats.target_edge_visits, 0);
        let empty = GraphEdgeUpsertBatchStats {
            match_selection: GqlExecutionStats {
                snapshot_records: 3,
                result_rows: 0,
            },
            overlay_edges: 0,
            created_edges: 0,
            matched_occurrences: 0,
            action_effects: 0,
            evaluator: GlaExecutionStats {
                work_units: 2,
                scratch_entries: 0,
            },
        };
        meter
            .absorb_edge_upsert_batch::<(), (), ()>(1, &original, empty, 0)
            .unwrap();
        assert_eq!(meter.edges, 1);
        assert_eq!(meter.common.stats.selection.result_rows, 3);
        assert_eq!(meter.common.stats.effects, 0);
        assert_eq!(meter.common.stats.target_edge_visits, 0);
    }

    #[test]
    fn occurrence_action_products_are_widened_before_validation() {
        let input = input();
        let mut meter = meter(GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            u64::MAX,
            0,
            u64::MAX,
        ));
        let mut impossible = stats();
        impossible.matched_occurrences = u64::MAX;
        impossible.overlay_edges = u64::MAX;
        impossible.action_effects = u64::MAX;
        impossible.evaluator = GlaExecutionStats {
            work_units: u64::MAX,
            scratch_entries: u64::MAX,
        };
        let before = meter.common.stats;
        assert!(matches!(
            meter.absorb_edge_upsert_batch::<(), (), ()>(0, &input, impossible, 3),
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::InvalidStatistics { statement: 0 }
            ))
        ));
        assert_eq!(meter.common.stats, before);
        assert_eq!(meter.edges, 0);
    }
}
