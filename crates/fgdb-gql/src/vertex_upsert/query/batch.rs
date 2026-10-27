//! One native result pipeline over all vertex-MERGE targets. The host supplies
//! their final staged fields, after every action; no source or evaluator lives
//! here. The singular API keeps its own input and cardinality contract.

use super::*;

impl PreparedGraphVertexUpsertQuery {
    /// Finish the entire final-state input bag once, not one aggregate or page
    /// per target. Source/work/scratch counters include the preceding MERGE and
    /// all actions. Only final output rows consume the result-row allowance.
    /// Hosts must retain their statement rollback guard until this succeeds.
    /// An empty bag is valid here (a global count yields zero), although a
    /// successful standalone vertex MERGE always supplies at least one target.
    pub fn project_rows_governed<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_final_rows(
            rows,
            selection,
            evaluator,
            policy,
            &mut |_, _| None,
            checkpoint,
        )
    }

    /// Finish the whole bag with a reader over all privately staged effects.
    /// Computed identities do not inherit authority from their input column;
    /// the host checks each requested element and property before observation.
    pub fn project_rows_governed_reading<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut elements: impl crate::GraphElementReader<E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_final_rows(
            rows,
            selection,
            evaluator,
            policy,
            &mut crate::set_ops::projection::HostElements(&mut elements),
            checkpoint,
        )
    }

    fn project_final_rows<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        mut evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        elements: &mut crate::set_ops::ElementRead<'_, E>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        let invalid = || GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 0 });
        let mut work = || -> Result<(), GqlQueryError<GraphSetExecutionError<E>, C>> {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let observed = u128::from(evaluator.work_units) + 1;
            if observed > u128::from(policy.evaluator.max_work_units) {
                return Err(GqlQueryError::Evaluator(crate::GlaLimitExceeded {
                    dimension: crate::GlaLimitDimension::WorkUnits,
                    limit: policy.evaluator.max_work_units,
                    observed,
                }));
            }
            evaluator.work_units = observed as u64;
            Ok(())
        };
        work()?;
        // Validate even unprojected cells and rows later discarded by LIMIT.
        // A property is a stored value: a scalar, or a stored List or Map
        // (fgdb-k3nub), never a graph element. Malformed maps and non-text
        // labels may not reach a projected expression.
        for row in &rows {
            work()?;
            if row.len() != self.bindings.len() {
                return Err(invalid());
            }
            for (binding, value) in self.bindings.iter().zip(row) {
                work()?;
                match binding {
                    GraphVertexReturnBinding::Vertex => {
                        if !matches!(value, GraphValue::Vertex(_)) {
                            return Err(invalid());
                        }
                    }
                    GraphVertexReturnBinding::Property(_) => {
                        if !value.is_stored_value() {
                            return Err(invalid());
                        }
                    }
                    GraphVertexReturnBinding::Labels => {
                        let GraphValue::List(labels) = value else {
                            return Err(invalid());
                        };
                        if labels.len() >= GraphValue::MAX_LIST_NODES {
                            return Err(invalid());
                        }
                        for label in labels {
                            work()?;
                            if !matches!(
                                label,
                                GraphValue::Scalar(fgdb_types::CanonicalScalar::Text(_))
                            ) {
                                return Err(invalid());
                            }
                        }
                    }
                    GraphVertexReturnBinding::Properties => {
                        let (keys, values) = value.as_map().ok_or_else(invalid)?;
                        if keys.len() != values.len() || keys.len() >= GraphValue::MAX_LIST_NODES {
                            return Err(invalid());
                        }
                        for (at, (key, value)) in keys.iter().zip(values).enumerate() {
                            work()?;
                            if !value.is_stored_value()
                                || at != 0 && keys[at - 1].as_bytes() >= key.as_bytes()
                            {
                                return Err(invalid());
                            }
                        }
                    }
                }
            }
        }
        drop(work);
        let mut returning = GqlQueryExecution {
            value: rows
                .into_iter()
                .map(GraphValueRow::from_owned_values)
                .collect(),
            rows: selection,
            evaluator,
        };
        if let Some(grouping) = &self.grouping {
            returning = crate::set_ops::finish_owned_aggregate_reading(
                returning,
                policy,
                grouping,
                elements,
                &mut checkpoint,
            )?;
        }
        let head_policy = if self.tail.is_some() {
            crate::set_ops::owned_head_policy(policy)
        } else {
            policy
        };
        let returning = crate::set_ops::finish_owned_projection_reading(
            returning,
            head_policy,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            elements,
            &mut checkpoint,
        )?;
        match &self.tail {
            None => Ok(returning),
            Some(tail) => crate::set_ops::finish_owned_tail_reading(
                returning, tail, policy, elements, checkpoint,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphVertexUpsertQueryText};
    use fgdb_delta_types::{LabelId, RelationId};
    use fgdb_types::{CanonicalScalar, VId};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }
    fn prepare(text: &str) -> PreparedGraphVertexUpsertQuery {
        PreparedGraphVertexUpsertQueryText::prepare(text, RelationId(1), symbols)
            .expect(text)
            .bind_parameters(&GqlParameters::new())
            .expect(text)
    }
    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn policy(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(1_000, rows, 1_000_000, 1_000_000)
    }
    fn output(
        query: &PreparedGraphVertexUpsertQuery,
        rows: Vec<Vec<GraphValue>>,
    ) -> Vec<Vec<GraphValue>> {
        query
            .project_rows_governed::<(), ()>(
                rows,
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(100),
                || Ok(()),
            )
            .unwrap()
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    #[test]
    fn whole_bag_grouping_counts_every_target_before_one_page() {
        let query = prepare("MERGE (n:Item) RETURN n.p AS p, count(*) AS copies ORDER BY p");
        assert_eq!(
            query.bindings(),
            &[GraphVertexReturnBinding::Property(PropertyKeyId(1))]
        );
        assert_eq!(
            output(&query, vec![vec![int(2)], vec![int(1)], vec![int(1)]]),
            vec![vec![int(1), int(2)], vec![int(2), int(1)]],
        );
        let query = prepare("MERGE (n:Item) RETURN count(*) AS n, sum(n.p) AS total");
        let result = query
            .project_rows_governed::<(), ()>(
                vec![vec![int(1)], vec![int(2)], vec![int(3)]],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(1),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(result.value[0].values(), &[int(3), int(6)]);
        assert_eq!(result.rows.result_rows, 1);
    }

    #[test]
    fn carried_alias_reads_and_result_windows_cover_the_complete_bag() {
        let query = prepare(
            "MERGE (n:Item) WITH n AS node WHERE node.p > 0 \
             RETURN node.p AS p ORDER BY p DESC SKIP 1 LIMIT 1",
        );
        assert_eq!(
            query.bindings(),
            &[
                GraphVertexReturnBinding::Vertex,
                GraphVertexReturnBinding::Property(PropertyKeyId(1)),
            ]
        );
        assert_eq!(
            output(
                &query,
                vec![
                    vec![GraphValue::Vertex(VId(1)), int(1)],
                    vec![GraphValue::Vertex(VId((1_u128 << 100) + 1)), int(3)],
                    vec![GraphValue::Vertex(VId(2)), int(2)],
                ]
            ),
            vec![vec![int(2)]]
        );
    }

    #[test]
    fn full_width_identity_is_not_collapsed_or_replaced_by_row_number() {
        let query = prepare("MERGE (n:Item) RETURN DISTINCT n AS node");
        let low = GraphValue::Vertex(VId(7));
        let high = GraphValue::Vertex(VId((1_u128 << 112) + 7));
        // A RETURN without ORDER BY is a bag: both full-width identities come
        // back whole and distinct, in whatever order the batch delivers.
        let mut result = output(&query, vec![vec![high.clone()], vec![low.clone()]]);
        result.sort();
        assert_eq!(result, vec![vec![low], vec![high]]);
    }

    #[test]
    fn empty_global_aggregate_and_filtered_inputs_do_not_fabricate_targets() {
        let query = prepare("MERGE (n:Item) RETURN count(*) AS n");
        assert_eq!(output(&query, Vec::new()), vec![vec![int(0)]]);
        let query = prepare("MERGE (n:Item) WITH n.p AS p WHERE p < 0 RETURN p");
        let result = query
            .project_rows_governed::<(), ()>(
                vec![vec![int(1)], vec![int(2)]],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(0),
                || Ok(()),
            )
            .unwrap();
        assert!(result.value.is_empty());
        assert_eq!(result.rows.result_rows, 0);
    }

    #[test]
    fn limit_zero_does_not_hide_bad_input_or_late_expression_errors() {
        let query = prepare("MERGE (n:Item) RETURN n AS node, n.p AS p LIMIT 0");
        for row in [
            vec![GraphValue::Vertex(VId(1))],
            vec![int(1), int(2)],
            // A stored List is a valid property cell (fgdb-k3nub); a List
            // holding a graph element is not a stored value at any depth.
            vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::List(vec![GraphValue::Vertex(VId(2))].into()),
            ],
        ] {
            assert!(matches!(
                query.project_rows_governed::<(), ()>(
                    vec![row],
                    GqlExecutionStats::default(),
                    GlaExecutionStats::default(),
                    policy(0),
                    || Ok(()),
                ),
                Err(GqlQueryError::Source(
                    GraphSetExecutionError::InputSchema { .. }
                )),
            ));
        }
        let query = prepare("MERGE (n:Item) WITH n.p AS p RETURN 10 / p AS value LIMIT 0");
        assert!(matches!(
            query.project_rows_governed::<(), ()>(
                vec![vec![int(2)], vec![int(0)]],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(0),
                || Ok(()),
            ),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            )),
        ));
    }

    #[test]
    fn all_rows_and_tail_share_prefix_quotas_and_every_interruption_is_terminal() {
        let query =
            prepare("MERGE (n:Item) WITH n.p AS p UNWIND [p, p + 1] AS x RETURN sum(x) AS total");
        let rows = vec![vec![int(1)], vec![int(3)]];
        let prefix = GlaExecutionStats {
            work_units: 37,
            scratch_entries: 11,
        };
        let selection = GqlExecutionStats {
            snapshot_records: 7,
            result_rows: 2,
        };
        let mut checkpoints = 0;
        let result = query
            .project_rows_governed::<(), ()>(rows.clone(), selection, prefix, policy(1), || {
                checkpoints += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(result.value[0].values(), &[int(10)]);
        assert_eq!(result.rows.snapshot_records, 7);
        let exact = GqlQueryPolicy::new(
            7,
            1,
            result.evaluator.work_units,
            result.evaluator.scratch_entries,
        );
        assert!(
            query
                .project_rows_governed::<(), ()>(rows.clone(), selection, prefix, exact, || Ok(()),)
                .is_ok()
        );
        for allowance in [
            GqlQueryPolicy::new(
                7,
                1,
                result.evaluator.work_units - 1,
                result.evaluator.scratch_entries,
            ),
            GqlQueryPolicy::new(
                7,
                1,
                result.evaluator.work_units,
                result.evaluator.scratch_entries - 1,
            ),
        ] {
            assert!(matches!(
                query.project_rows_governed::<(), ()>(
                    rows.clone(),
                    selection,
                    prefix,
                    allowance,
                    || Ok(()),
                ),
                Err(GqlQueryError::Evaluator(_))
            ));
        }
        assert!(checkpoints > 1);
        for stop in 1..=checkpoints {
            let mut seen = 0;
            let result = query.project_rows_governed::<(), usize>(
                rows.clone(),
                selection,
                prefix,
                policy(1),
                || {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(seen, stop);
        }
    }
}
