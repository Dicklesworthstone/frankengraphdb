//! All-target vertex actions use the ordinary program meter. Distinct targets
//! and source occurrences are separate counts; an empty MATCH can create one
//! vertex without inventing a selected row or a refreshed action allowance.

use super::*;
use crate::GraphVertexUpsertBranch;

impl MixedMeter {
    pub(super) fn absorb_vertex_upsert_batch<E, A, C>(
        &mut self,
        statement: usize,
        input: &PreparedGraphVertexUpsert,
        stats: GraphVertexUpsertStats,
        target_vertices: u64,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        let selected = stats.merge.match_selection.result_rows;
        let (actions, valid_targets) = match stats.branch {
            GraphVertexUpsertBranch::Match => (
                input.on_match(),
                stats.merge.created_vertices == 0
                    && target_vertices != 0
                    && target_vertices <= selected,
            ),
            GraphVertexUpsertBranch::Create => (
                input.on_create(),
                stats.merge.created_vertices == 1 && selected == 0 && target_vertices == 1,
            ),
        };
        let per_target = actions.len() as u128 + input.after().len() as u128;
        if !valid_targets || !input.validates_action_effects(&stats, target_vertices) {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        let effects = self.common.add(
            statement,
            GraphMutationProgramDimension::Effects,
            stats.action_effects,
        )?;
        let visits = if per_target == 0 { 0 } else { target_vertices };
        let targets = self
            .common
            .stats
            .target_vertex_visits
            .checked_add(visits)
            .ok_or(GraphMutationProgramError::InvalidStatistics { statement })?;
        // Source, creation and evaluator validation must also succeed before
        // any successor is installed. This keeps failure meter-atomic.
        self.absorb_vertex_merge(statement, stats.merge)?;
        self.common.stats.effects = effects;
        self.common.stats.target_vertex_visits = targets;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphVertexUpsertText};
    use fgdb_delta_types::PropertyKeyId;

    fn input() -> PreparedGraphVertexUpsert {
        PreparedGraphVertexUpsertText::prepare(
            "MERGE (n {p:1}) ON CREATE SET n.p=2 ON MATCH SET n.p=3,n.q=4 SET n.q=5",
            RelationId(1),
            |kind: GraphSymbolKind, name: &str| match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
                _ => None,
            },
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }

    fn policy() -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(GqlQueryPolicy::new(20, 8, 100, 80), 18, 1, 0)
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

    fn matched() -> GraphVertexUpsertStats {
        GraphVertexUpsertStats {
            merge: GraphVertexMergeStats {
                match_selection: GqlExecutionStats {
                    snapshot_records: 10,
                    result_rows: 4,
                },
                evaluator: GlaExecutionStats {
                    work_units: 50,
                    scratch_entries: 40,
                },
                created_vertices: 0,
            },
            branch: GraphVertexUpsertBranch::Match,
            property_removals: 0,
            action_effects: 9,
        }
    }

    #[test]
    fn distinct_targets_do_not_replace_source_occurrences_or_clause_counts() {
        let input = input();
        let mut meter = meter(policy());
        for statement in 0..2 {
            meter
                .absorb_vertex_upsert_batch::<(), (), ()>(statement, &input, matched(), 3)
                .unwrap();
        }
        assert_eq!(meter.common.stats.completed_statements, 2);
        assert_eq!(meter.common.stats.selection.result_rows, 8);
        assert_eq!(meter.common.stats.selection.snapshot_records, 20);
        assert_eq!(meter.common.stats.effects, 18);
        assert_eq!(meter.common.stats.target_vertex_visits, 6);
        assert_eq!(meter.common.stats.target_edge_visits, 0);
        assert_eq!(meter.vertices, 0);
    }

    #[test]
    fn replacement_receipts_count_dynamic_unsets_and_empty_target_visits() {
        let input = input();
        let replacement = PreparedGraphVertexUpsert::prepare(
            input.merge().clone(),
            vec![crate::GraphVertexUpsertAction::ClearProperties { keep: Vec::new() }],
            Vec::new(),
        )
        .unwrap();
        let empty = GraphVertexUpsertStats {
            action_effects: 0,
            ..matched()
        };
        let mut singular = meter(policy());
        singular
            .absorb_vertex_upsert::<(), (), ()>(0, &replacement, empty)
            .unwrap();
        assert_eq!(singular.common.stats.target_vertex_visits, 1);
        assert_eq!(singular.common.stats.effects, 0);
        let mut plural = meter(policy());
        plural
            .absorb_vertex_upsert_batch::<(), (), ()>(0, &replacement, empty, 3)
            .unwrap();
        assert_eq!(plural.common.stats.target_vertex_visits, 3);
        assert_eq!(plural.common.stats.effects, 0);

        let dynamic = GraphVertexUpsertStats {
            property_removals: 6,
            action_effects: 6,
            ..matched()
        };
        let mut plural = meter(policy());
        plural
            .absorb_vertex_upsert_batch::<(), (), ()>(0, &replacement, dynamic, 3)
            .unwrap();
        assert_eq!(plural.common.stats.effects, 6);
        let mut unmetered = dynamic;
        unmetered.merge.evaluator.scratch_entries = 5;
        for stats in [
            GraphVertexUpsertStats {
                action_effects: 5,
                ..dynamic
            },
            GraphVertexUpsertStats {
                property_removals: 0,
                ..dynamic
            },
            unmetered,
        ] {
            let mut meter = meter(policy());
            let before = meter.common.stats;
            assert!(matches!(
                meter.absorb_vertex_upsert_batch::<(), (), ()>(0, &replacement, stats, 3),
                Err(GraphWriteProgramError::Program(
                    GraphMutationProgramError::InvalidStatistics { .. }
                ))
            ));
            assert_eq!(meter.common.stats, before);
        }
        // A fixed-shape clause cannot explain extra writes as replacement
        // removals, even if all generic work and effect limits would allow it.
        let mut meter = meter(policy());
        assert!(matches!(
            meter.absorb_vertex_upsert_batch::<(), (), ()>(
                0,
                &input,
                GraphVertexUpsertStats {
                    action_effects: 15,
                    ..dynamic
                },
                3,
            ),
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::InvalidStatistics { .. }
            ))
        ));
    }

    #[test]
    fn invalid_branch_target_and_action_products_leave_every_counter_unchanged() {
        let input = input();
        let original = matched();
        let mut created = original;
        created.branch = GraphVertexUpsertBranch::Create;
        created.merge.created_vertices = 1;
        created.action_effects = 2;
        let mut overflow = original;
        overflow.merge.match_selection.result_rows = u64::MAX;
        overflow.action_effects = u64::MAX;
        for (stats, targets) in [
            (original, 0),
            (original, 5),
            (
                GraphVertexUpsertStats {
                    action_effects: 3,
                    ..original
                },
                3,
            ),
            (created, 1), // A create branch must not claim existing MATCH rows.
            (overflow, u64::MAX),
        ] {
            let mut meter = meter(policy());
            let before = meter.common.stats;
            assert!(matches!(
                meter.absorb_vertex_upsert_batch::<(), (), ()>(0, &input, stats, targets),
                Err(GraphWriteProgramError::Program(
                    GraphMutationProgramError::InvalidStatistics { .. }
                ))
            ));
            assert_eq!(meter.common.stats, before);
            assert_eq!(meter.vertices, 0);
        }
        created.merge.match_selection.result_rows = 0;
        let mut meter = meter(policy());
        meter
            .absorb_vertex_upsert_batch::<(), (), ()>(0, &input, created, 1)
            .unwrap();
        assert_eq!(meter.vertices, 1);
        assert_eq!(meter.common.stats.selection.result_rows, 0);
        assert_eq!(meter.common.stats.effects, 2);
        assert_eq!(meter.common.stats.target_vertex_visits, 1);
    }

    #[test]
    fn cumulative_caps_and_singular_validation_are_not_weakened() {
        let input = input();
        let mut policies = Vec::new();
        for query in [
            GqlQueryPolicy::new(19, 8, 100, 80),
            GqlQueryPolicy::new(20, 7, 100, 80),
            GqlQueryPolicy::new(20, 8, 99, 80),
            GqlQueryPolicy::new(20, 8, 100, 79),
        ] {
            policies.push(GraphWriteProgramPolicy::new(query, 18, 1, 0));
        }
        policies.push(GraphWriteProgramPolicy::new(
            policy().mutations.query,
            17,
            1,
            0,
        ));
        for policy in policies {
            let mut meter = meter(policy);
            meter
                .absorb_vertex_upsert_batch::<(), (), ()>(0, &input, matched(), 3)
                .unwrap();
            let before = meter.common.stats;
            assert!(
                meter
                    .absorb_vertex_upsert_batch::<(), (), ()>(1, &input, matched(), 3)
                    .is_err()
            );
            assert_eq!(meter.common.stats, before);
            assert_eq!(meter.vertices, 0);
        }
        let mut meter = meter(policy());
        assert!(matches!(
            meter.absorb_vertex_upsert::<(), (), ()>(0, &input, matched()),
            Err(GraphWriteProgramError::Program(
                GraphMutationProgramError::InvalidStatistics { .. }
            ))
        ));
        let no_actions = PreparedGraphVertexUpsert::prepare_with_trailing_actions(
            input.merge().clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        meter
            .absorb_vertex_upsert_batch::<(), (), ()>(
                0,
                &no_actions,
                GraphVertexUpsertStats {
                    action_effects: 0,
                    ..matched()
                },
                3,
            )
            .unwrap();
        assert_eq!(meter.common.stats.target_vertex_visits, 0);
        assert_eq!(meter.common.stats.effects, 0);
    }
}
