// Compound summaries reuse the canonical pattern readers. This module owns
// only the one-coordinate lifetime, not matching, set or aggregate semantics.
mod set_aggregate_queries {
    use super::*;
    use fgdb_gql::{
        GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
        PreparedGraphSetAggregate,
    };
    use fgdb_types::QueryCx;

    type ResultRows<E> = Result<
        GqlQueryExecution<GraphAggregateRow>,
        GqlQueryError<GraphAggregateError<E>, Box<asupersync::error::Error>>,
    >;

    impl<V: Vfs + Clone> Database<V> {
        /// Summarize a completed compound relation at one durable frontier.
        /// Native set/WITH inputs may be bound separately and reused. Counts,
        /// i128 sums and rational averages retain their exact result domains.
        pub fn execute_graph_set_aggregate_governed(
            &self,
            cx: &QueryCx,
            query: &PreparedGraphSetAggregate,
            policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            let as_of = self.frontier().map_err(|error| {
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
            })?;
            self.execute_graph_set_aggregate_governed_at(cx, query, as_of, policy)
        }

        /// Every operand reads the SAME retained sequence under this immutable
        /// database borrow. Health/history preflight precedes outer work limits,
        /// even for empty differences, zero output pages or a zero allowance.
        /// All operand visits, relational stages and grouping share one budget.
        pub fn execute_graph_set_aggregate_governed_at(
            &self,
            cx: &QueryCx,
            query: &PreparedGraphSetAggregate,
            as_of: CommitSeq,
            policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            self.execute_graph_set_aggregate_evidenced_at(cx, query, as_of, policy, &mut Vec::new())
        }

        /// [`Self::execute_graph_set_aggregate_governed_at`], appending each
        /// executed procedure's Prism certificate to `evidence` in execution
        /// order, for a certified read (fgdb-3b1v7).
        pub(crate) fn execute_graph_set_aggregate_evidenced_at(
            &self,
            cx: &QueryCx,
            query: &PreparedGraphSetAggregate,
            as_of: CommitSeq,
            policy: GqlQueryPolicy,
            evidence: &mut Vec<fgdb_prism::FnxCertificate>,
        ) -> ResultRows<GqlError> {
            self.ensure_readable()
                .and_then(|()| self.snapshot.check_frontier(as_of))
                .map_err(|error| {
                    GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
                })?;
            // A CALL input runs in Prism at this same sequence.
            cx.with_restriction(|| {
                let evidence = std::cell::RefCell::new(evidence);
                query.execute_governed_from_source(
                    policy,
                    fgdb_gql::with_element_metadata(
                        fgdb_gql::procedure_row_source(
                            |pattern, remaining| {
                                self.execute_graph_pattern_governed_at(
                                    cx, pattern, as_of, remaining,
                                )
                            },
                            |pattern, seeds, remaining| {
                                Some(crate::gql_exec::execute_seeded_pattern_at(
                                    self,
                                    pattern,
                                    Some(seeds),
                                    as_of,
                                    remaining,
                                    || cx.checkpoint(),
                                ))
                            },
                            |call, arguments, remaining| {
                                if crate::query::is_hybrid(call) {
                                    return crate::query::hybrid_procedure_with_registry(
                                        &self.snapshot,
                                        Some(self.maintained_beacon.query_indexes()),
                                        as_of,
                                        call,
                                        arguments,
                                        remaining,
                                        cx,
                                    );
                                }
                                crate::query::fnx_procedure(
                                    call,
                                    arguments,
                                    as_of,
                                    remaining,
                                    &mut evidence.borrow_mut(),
                                    |spec, options, sink| {
                                        self.execute_fnx_into(cx, spec, options, sink)
                                    },
                                )
                            },
                            |call, arguments, remaining| {
                                Some(crate::query::fnx_procedure_rows(
                                    call,
                                    arguments,
                                    as_of,
                                    remaining,
                                    &mut evidence.borrow_mut(),
                                    |spec, options| self.execute_fnx_rows(cx, spec, options),
                                ))
                            },
                            |element, key| {
                                crate::gql_exec::element_property_at(self, element, key, as_of)
                            },
                        ),
                        |element, kind, catalog, control| {
                            crate::gql_exec::element_metadata_at(
                                self, element, kind, catalog, as_of, control,
                            )
                        },
                    ),
                    || cx.checkpoint(),
                )
            })
        }
    }

    impl crate::EmbeddedReadView {
        /// All operands remain within this pinned immutable generation even
        /// while the live database advances or compacts. No live writer lookup
        /// supplies a later operand, nor can a filtered row release the pin.
        pub fn execute_graph_set_aggregate_governed(
            &self,
            cx: &QueryCx,
            query: &PreparedGraphSetAggregate,
            policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            self.execute_graph_set_aggregate_governed_at(cx, query, self.frontier(), policy)
        }

        /// Historical execution is confined to this view's retained frontier.
        /// Only fully accepted final groups escape; input rows stay private.
        pub fn execute_graph_set_aggregate_governed_at(
            &self,
            cx: &QueryCx,
            query: &PreparedGraphSetAggregate,
            as_of: CommitSeq,
            policy: GqlQueryPolicy,
        ) -> ResultRows<GqlError> {
            self.snapshot.check_frontier(as_of).map_err(|error| {
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
            })?;
            cx.with_restriction(|| {
                query.execute_governed_from_source(
                    policy,
                    fgdb_gql::with_element_metadata(
                        fgdb_gql::procedure_row_source(
                            |pattern, remaining| {
                                self.execute_graph_pattern_governed_at(
                                    cx, pattern, as_of, remaining,
                                )
                            },
                            |pattern, seeds, remaining| {
                                Some(crate::gql_exec::execute_seeded_pattern_at(
                                    self,
                                    pattern,
                                    Some(seeds),
                                    as_of,
                                    remaining,
                                    || cx.checkpoint(),
                                ))
                            },
                            |call, arguments, remaining| {
                                if crate::query::is_hybrid(call) {
                                    return crate::query::hybrid_procedure(
                                        &self.snapshot,
                                        Some(&self.beacon_indexes),
                                        as_of,
                                        call,
                                        arguments,
                                        remaining,
                                        cx,
                                    );
                                }
                                crate::query::fnx_procedure(
                                    call,
                                    arguments,
                                    as_of,
                                    remaining,
                                    &mut Vec::new(),
                                    |spec, options, sink| {
                                        self.execute_fnx_into(cx, spec, options, sink)
                                    },
                                )
                            },
                            |call, arguments, remaining| {
                                Some(crate::query::fnx_procedure_rows(
                                    call,
                                    arguments,
                                    as_of,
                                    remaining,
                                    &mut Vec::new(),
                                    |spec, options| self.execute_fnx_rows(cx, spec, options),
                                ))
                            },
                            |element, key| {
                                crate::gql_exec::element_property_at(self, element, key, as_of)
                            },
                        ),
                        |element, kind, catalog, control| {
                            crate::gql_exec::element_metadata_at(
                                self, element, kind, catalog, as_of, control,
                            )
                        },
                    ),
                    || cx.checkpoint(),
                )
            })
        }
    }

    impl WriteTxn {
        /// Summarize the completed set over one canonical staged overlay.
        /// Immutable transaction/database borrows exclude mutation between
        /// operands. Each operand uses the existing positive/negative read and
        /// scan witnesses, including rows canceled by INTERSECT/EXCEPT or pages.
        /// Those observations survive late arithmetic, budget or source errors.
        /// Carried vertex/edge property and metadata reads retain the same
        /// canonical overlay and witnesses in folded and materialized inputs.
        ///
        /// This is read-only with respect to staged effects and durability:
        /// it does not finish, commit, retry or replace the caller's workspace.
        /// Ordinary finish/commit still validates the retained observations.
        pub fn execute_graph_set_aggregate_governed<V: Vfs + Clone>(
            &self,
            database: &Database<V>,
            cx: &QueryCx,
            query: &PreparedGraphSetAggregate,
            policy: GqlQueryPolicy,
        ) -> ResultRows<WriteTxnError> {
            let _ = self
                .query_snapshot(database)
                .map_err(|error| GqlQueryError::Source(GraphAggregateError::Source(error)))?;
            cx.with_restriction(|| {
                let element_failure = std::cell::RefCell::new(None);
                let result = query.execute_governed_from_source(
                    policy,
                    fgdb_gql::with_element_metadata(
                        fgdb_gql::WithSeededPatterns {
                            seeded: |pattern: &fgdb_gql::algebra::PreparedGraphPattern<
                                fgdb_gql::algebra::GraphValueRow,
                            >,
                                     seeds: fgdb_gql::GraphSeeds<'_>,
                                     remaining| {
                                self.execute_seeded_graph_pattern_governed(
                                    database, cx, pattern, seeds, remaining,
                                )
                            },
                            source: fgdb_gql::WithElementReads {
                                source: fgdb_gql::WithProcedures {
                                    patterns:
                                        |pattern: &fgdb_gql::algebra::PreparedGraphPattern<
                                            fgdb_gql::algebra::GraphValueRow,
                                        >,
                                         remaining| {
                                            self.execute_graph_pattern_governed(
                                                database, cx, pattern, remaining,
                                            )
                                        },
                                    procedures:
                                        |call: &fgdb_gql::PreparedProcedureCall,
                                         arguments: &[fgdb_gql::algebra::GraphValue],
                                         remaining| {
                                            self.execute_procedure_governed(
                                                database, cx, call, arguments, remaining,
                                            )
                                        },
                                },
                                elements: |element, key| {
                                    self.element_property_with_checkpoint(
                                        database,
                                        cx,
                                        &element_failure,
                                        element,
                                        key,
                                    )
                                },
                            },
                        },
                        |element, kind, catalog, control| {
                            self.element_metadata_with_checkpoint(
                                database,
                                cx,
                                &element_failure,
                                element,
                                kind,
                                catalog,
                                control,
                            )
                        },
                    ),
                    || {
                        let stopped = element_failure.borrow_mut().take();
                        match stopped {
                            Some(error) => Err(error),
                            None => cx.checkpoint(),
                        }
                    },
                );
                match element_failure.into_inner() {
                    Some(error) => Err(GqlQueryError::Interrupted(error)),
                    None => result,
                }
            })
        }
    }
}
