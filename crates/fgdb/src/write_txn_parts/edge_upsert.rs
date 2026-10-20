// Relationship MERGE has one branch decision, evaluator allowance and native
// rollback boundary. Each SET clause freezes inputs before its writes; the
// following clause reads canonical staged effects, never a second graph model.

type EdgeUpsertActionResult<T, E, A, C> =
    Result<T, fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeUpsertError<E, A>, C>>;

#[allow(clippy::too_many_arguments)]
fn edge_upsert_actions<E, A, C>(
    upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
    policy: fgdb_gql::GraphEdgeUpsertPolicy,
    merge_stats: fgdb_gql::GraphEdgeMergeStats,
    outcome: fgdb_gql::GraphEdgeMergeOutcome,
    mut checkpoint: impl FnMut() -> Result<(), C>,
    mut property: impl FnMut(
        EId,
        fgdb_delta_types::PropertyKeyId,
        &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> EdgeUpsertActionResult<(), E, A, C>,
    ) -> EdgeUpsertActionResult<CanonicalScalar, E, A, C>,
    mut stage: impl FnMut(WriteBatch) -> EdgeUpsertActionResult<(), E, A, C>,
) -> EdgeUpsertActionResult<fgdb_gql::GraphEdgeUpsertStats, E, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryError,
        GraphEdgeMergeOutcome, GraphEdgeUpsertBranch, GraphEdgeUpsertError, GraphEdgeUpsertStats,
        GraphEdgeUpsertValue, GraphIntegerEvaluationError,
    };
    let (branch, selected) = match outcome {
        GraphEdgeMergeOutcome::NoInput => (GraphEdgeUpsertBranch::NoInput, &[][..]),
        GraphEdgeMergeOutcome::Matched(_) => (GraphEdgeUpsertBranch::Match, upsert.on_match()),
        GraphEdgeMergeOutcome::Created(_) => (GraphEdgeUpsertBranch::Create, upsert.on_create()),
    };
    let observed = upsert.action_count(branch) as u128;
    if observed > u128::from(policy.max_actions) {
        return Err(GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit {
            limit: policy.max_actions,
            observed,
        }));
    }
    let mut evaluator = merge_stats.evaluator;
    let mut control = |event| -> EdgeUpsertActionResult<(), E, A, C> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(evaluator.work_units) + 1;
        let scratch = u128::from(evaluator.scratch_entries)
            + u128::from(event == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (
                work,
                policy.merge.query.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                scratch,
                policy.merge.query.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        evaluator.work_units = work as u64;
        evaluator.scratch_entries = scratch as u64;
        Ok(())
    };
    // NoInput must skip the trailing clause as well as both ON branches.
    if let Some(edge) = outcome.edge() {
        for (clause, actions) in [selected, upsert.after()].into_iter().enumerate() {
            if actions.is_empty() {
                continue;
            }
            let mut batch = WriteBatch::new(upsert.merge().relation());
            for (action, definition) in actions.iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                let value = match &definition.value {
                    GraphEdgeUpsertValue::Literal(value) => {
                        point_scalar_payload_admission(value.value(), &mut |event| {
                            control(match event {
                                SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                                _ => GlaExecutionEvent::Work,
                            })
                        })?;
                        value.value().clone()
                    }
                    GraphEdgeUpsertValue::Expression { properties, .. }
                    | GraphEdgeUpsertValue::Composite { properties, .. } => {
                        for _ in properties {
                            control(GlaExecutionEvent::ScratchEntry)?;
                        }
                        let mut inputs = Vec::with_capacity(properties.len());
                        for key in properties {
                            inputs.push(fgdb_gql::algebra::GraphValue::Scalar(property(
                                edge,
                                *key,
                                &mut control,
                            )?));
                        }
                        let value = definition.value
                            .evaluate_with_control(inputs, &mut control)
                            .map_err(|error| match error {
                                GraphIntegerEvaluationError::Control(error) => error,
                                GraphIntegerEvaluationError::Value(source) => {
                                    GqlQueryError::Source(GraphEdgeUpsertError::Expression {
                                        clause,
                                        action,
                                        source,
                                    })
                                }
                            })?;
                        let fgdb_gql::algebra::GraphValue::Scalar(value) = value else {
                            return Err(GqlQueryError::Source(
                                GraphEdgeUpsertError::NonScalarValue { clause, action },
                            ));
                        };
                        value
                    }
                };
                batch.set_edge_property(edge, definition.key, Some(value));
            }
            // All right-hand sides have succeeded before the first write of
            // this clause. The next clause must observe this native staging.
            stage(batch)?;
        }
    }
    // Refusal here still belongs to the outer all-or-nothing workspace.
    control(GlaExecutionEvent::Work)?;
    drop(control);
    Ok(GraphEdgeUpsertStats {
        merge: merge_stats,
        branch,
        action_effects: observed as u64,
        evaluator,
    })
}

// The same history, prepared-net projection, payload reservation and retained
// point witness as ordinary governed reads. Only the already selected EId is
// addressed. A missing field is NULL, not an instruction to remove a property.
fn edge_upsert_property<V: Vfs + Clone, A, C>(
    transaction: &WriteTxn,
    database: &Database<V>,
    edge: EId,
    property: fgdb_delta_types::PropertyKeyId,
    control: &mut dyn FnMut(
        fgdb_gql::GlaExecutionEvent,
    ) -> EdgeUpsertActionResult<(), WriteTxnError, A, C>,
) -> EdgeUpsertActionResult<CanonicalScalar, WriteTxnError, A, C> {
    use crate::gql_exec::source::SourceEvent;
    use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GraphEdgeUpsertError};
    control(GlaExecutionEvent::Work)?;
    let element = ElementId::Edge(edge);
    let field = PointReadField::Property(property);
    let mut attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element,
        accepted: false,
    };
    let mut source_control = |event| {
        control(match event {
            SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
            SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
        })
    };
    let projected = transaction.point_projection_with_control(
        database,
        element,
        field,
        &mut source_control,
        &|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)),
    )?;
    if let Some(value) = projected.property {
        point_payload_admission(value, &mut source_control)?;
    }
    source_control(SourceEvent::ScratchEntry)?;
    source_control(SourceEvent::ScratchEntry)?;
    transaction.point_reads.borrow_mut().record(element, field);
    source_control(SourceEvent::ScratchEntry)?;
    // A MERGE action reads its operands as scalars; a stored List or Map
    // refuses rather than read as NULL (fgdb-k3nub).
    let value = match projected.property {
        None => CanonicalScalar::Null,
        Some(value) => value.as_scalar().cloned().ok_or_else(|| {
            GqlQueryError::from(fgdb_gql::GraphIntegerError {
                instruction: 0,
                kind: fgdb_gql::GraphIntegerErrorKind::StoredCollection,
            })
        })?,
    };
    attempt.accepted = true;
    Ok(value)
}

impl WriteTxn {
    /// Execute directed MERGE, its selected ON clause and subsequent SET as one
    /// staged operation. Each clause has simultaneous assignment semantics;
    /// clauses share the MERGE's original evaluator and action allowances.
    /// NoInput runs no expression, property callback or branch.
    ///
    /// Failure or unwinding restores the original staged prefix but retains
    /// read/absence witnesses and does not reclaim issued external identities.
    /// This stages only; the caller still owns transaction completion.
    pub fn execute_graph_edge_upsert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeUpsertStats,
            fgdb_gql::GraphEdgeMergeOutcome,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError};
        self.ensure_database(database)
            .map_err(|error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error)))?;
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let (merge_stats, outcome) = workspace
                .txn
                .execute_graph_edge_merge_governed(
                    database,
                    cx,
                    upsert.merge(),
                    policy.merge,
                    allocate,
                )
                .map_err(|error| error.map_source(GraphEdgeUpsertError::Merge))?;
            let state = std::cell::RefCell::new((&mut *workspace.txn, &mut *database));
            let stats = edge_upsert_actions::<WriteTxnError, A, _>(
                upsert,
                policy,
                merge_stats,
                outcome,
                || cx.checkpoint(),
                |edge, key, control| {
                    let state = state.borrow();
                    edge_upsert_property(state.0, state.1, edge, key, control)
                },
                |batch| {
                    let mut state = state.borrow_mut();
                    let (transaction, database) = &mut *state;
                    transaction
                        .write(database, batch)
                        .map(|_| ())
                        .map_err(|error| {
                            GqlQueryError::Source(GraphEdgeUpsertError::Staging(error))
                        })
                },
            )?;
            workspace.accept();
            Ok((stats, outcome))
        })
    }
}

#[cfg(test)]
mod native_edge_upsert_return_tests {
    use super::*;
    use crate::DatabaseKeys;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{
        GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergeOutcome,
        GraphEdgeMergePolicy, GraphEdgeUpsertPolicy, GraphEdgeUpsertQueryError,
        GraphSymbol, GraphSymbolKind, PreparedGraphEdgeUpsertQueryText,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    use std::sync::atomic::{AtomicU64, Ordering};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const Q: PropertyKeyId = PropertyKeyId(2);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        }
    }

    #[test]
    fn keyed_upsert_return_reads_staged_branches_and_restores_failed_creation() {
        let ((), report) = run_async_under_lab(0xe6e0_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let keys = DatabaseKeys::new(
                [0x41; 32],
                DatabaseSecurityNamespaceId([0x42; 32]),
                [0x43; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let before = db.frontier().unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
            prefix.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
            transaction.write(&mut db, prefix).unwrap();
            let policy = GraphEdgeUpsertPolicy::new(
                GraphEdgeMergePolicy::new(GqlQueryPolicy::new(
                    1_000, 100, 5_000_000, 2_000_000,
                )),
                100,
            );
            let next = AtomicU64::new(100);
            let mut allocate = |_| -> Result<ElementId, ()> {
                Ok(ElementId::Edge(EId(u128::from(
                    next.fetch_add(1, Ordering::Relaxed),
                ))))
            };
            for (pattern, key, expected_edge, expected_count, created) in [
                ("(a)-[r:R {p:$key}]-(b)", 7, 100, 9, true),
                ("(a)-[r:R {p:$key}]-(b)", 7, 100, 20, false),
                ("(b)-[r:R {p:$key}]-(a)", 7, 100, 31, false),
                ("(a)-[r:R {p:$key}]-(b)", 8, 101, 10, true),
            ] {
                let text = format!(
                    "MATCH (a),(b) WHERE a.p=1 AND b.p=2 MERGE {pattern} \
                     ON CREATE SET r.q=r.p+1 ON MATCH SET r.q=r.q+10 \
                     SET r.q=r.q+1 RETURN r,r.p AS key,r.q AS count"
                );
                let arguments = GqlParameters::new().with_int64("key", key).unwrap();
                let query = PreparedGraphEdgeUpsertQueryText::prepare(&text, R, symbols)
                    .unwrap()
                    .bind_parameters(&arguments)
                    .unwrap();
                let (stats, outcome, rows) = transaction
                    .execute_graph_edge_upsert_query_governed(
                        &mut db,
                        &cx,
                        &query,
                        policy,
                        &mut allocate,
                    )
                    .unwrap();
                assert_eq!(outcome.created(), created);
                assert_eq!(outcome.edge(), Some(EId(expected_edge)));
                assert_eq!(rows.value.len(), 1);
                assert_eq!(
                    rows.value[0].values(),
                    &[
                        GraphValue::Edge(EId(expected_edge)),
                        GraphValue::Scalar(CanonicalScalar::Int(key)),
                        GraphValue::Scalar(CanonicalScalar::Int(expected_count)),
                    ]
                );
                assert!(rows.evaluator.work_units > stats.evaluator.work_units);
                assert!(rows.evaluator.scratch_entries > stats.evaluator.scratch_entries);
            }
            assert_eq!(next.load(Ordering::Relaxed), 102);
            let digest = transaction.staged_effect_digest().unwrap();
            let failing = PreparedGraphEdgeUpsertQueryText::prepare(
                "MATCH (a),(b) WHERE a.p=1 AND b.p=2 \
                 MERGE (a)-[r:R {p:9}]-(b) ON CREATE SET r.q=0 \
                 RETURN 10/r.q AS bad LIMIT 0",
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let result = transaction.execute_graph_edge_upsert_query_governed(
                &mut db,
                &cx,
                &failing,
                policy,
                &mut allocate,
            );
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Returning(_)))
            ));
            assert_eq!(next.load(Ordering::Relaxed), 103);
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.edges(&db).unwrap().len(), 2);
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            transaction.finish(&mut db, &commit).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            let first = db.edge(EId(100)).unwrap().unwrap();
            assert_eq!((first.entry.src, first.entry.dst), (VId(1), VId(2)));
            assert_eq!(first.props, vec![(P, CanonicalScalar::Int(7).into()), (Q, CanonicalScalar::Int(31).into())]);
            assert!(db.edge(EId(102)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
            // A missing MATCH input does not allocate an edge or evaluate the
            // failing row-local RETURN expression.
            let query = PreparedGraphEdgeUpsertQueryText::prepare(
                "MATCH (a),(b) WHERE a.p=99 AND b.p=2 \
                 MERGE (a)-[r:R {p:9}]-(b) ON CREATE SET r.q=0 RETURN 10/r.q AS bad",
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let mut empty = db.begin(&txcx).unwrap();
            let (_, outcome, rows) = empty
                .execute_graph_edge_upsert_query_governed(
                    &mut db, &cx, &query, policy, &mut allocate,
                )
                .unwrap();
            assert_eq!(outcome, GraphEdgeMergeOutcome::NoInput);
            assert!(rows.value.is_empty());
            assert_eq!(next.load(Ordering::Relaxed), 103);
            empty.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[cfg(test)]
mod native_edge_upsert_composite_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{
        GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergePolicy,
        GraphEdgeUpsertAction, GraphEdgeUpsertError, GraphEdgeUpsertPolicy,
        GraphEdgeUpsertValue, GraphSetValue, GraphSymbol, GraphSymbolKind,
        PreparedGraphEdgeUpsert, PreparedGraphEdgeUpsertText,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const Q: PropertyKeyId = PropertyKeyId(2);
    const AFTER: PropertyKeyId = PropertyKeyId(3);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        }
    }

    fn action(target: PropertyKeyId, input: PropertyKeyId, nested: bool)
    -> GraphEdgeUpsertAction<GraphEdgeUpsertValue> {
        let value = if nested {
            GraphSetValue::List(vec![GraphSetValue::Column(0)])
        } else {
            GraphSetValue::Column(0)
        };
        GraphEdgeUpsertAction {
            key: target,
            value: GraphEdgeUpsertValue::Composite {
                properties: vec![input],
                value: GraphSetValue::Index {
                    list: Box::new(GraphSetValue::List(vec![value])),
                    index: Box::new(GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(0)))),
                },
            },
        }
    }

    #[test]
    fn composite_parallel_upserts_are_simultaneous_and_late_refusal_restores_prefix() {
        let ((), report) = run_async_under_lab(0xe6e0_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let keys = crate::DatabaseKeys::new(
                [0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut tx = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
            prefix.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
            for (edge, p, q) in [(10, 7, 4), (11, 9, 6)] {
                prefix.add_edge(EId(edge), VId(1), VId(2), vec![
                    (P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q)),
                ]);
            }
            tx.write(&mut db, prefix).unwrap();
            let parsed = PreparedGraphEdgeUpsertText::prepare(
                "MATCH (a),(b) WHERE a.p=1 AND b.p=2 MERGE (a)-[r:R]->(b) ON MATCH SET r.q=0",
                R, symbols,
            ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let make = |bad| PreparedGraphEdgeUpsert::prepare_with_clauses(
                parsed.merge().clone(),
                vec![action(P, Q, false), action(Q, P, false)],
                vec![],
                vec![action(AFTER, P, bad)],
            ).unwrap();
            let policy = GraphEdgeUpsertPolicy::new(
                GraphEdgeMergePolicy::new(GqlQueryPolicy::new(1_000, 100, 5_000_000, 2_000_000)),
                100,
            );
            let before = tx.staged_effect_digest().unwrap();
            let mut allocate = |_| -> Result<ElementId, ()> { panic!("parallel matches must not create") };
            let error = tx.execute_graph_edge_upserts_governed(
                &mut db, &cx, &make(true), policy, &mut allocate,
            );
            assert!(matches!(error, Err(GqlQueryError::Source(
                GraphEdgeUpsertError::NonScalarValue { clause: 1, action: 0 }
            ))));
            assert_eq!(tx.staged_effect_digest().unwrap(), before);
            let (stats, outcomes) = tx.execute_graph_edge_upserts_governed(
                &mut db, &cx, &make(false), policy, &mut allocate,
            ).unwrap();
            assert_eq!(stats.created_edges, 0);
            assert_eq!(stats.matched_occurrences, 2);
            assert_eq!(stats.action_effects, 6);
            assert_eq!(outcomes.iter().map(|outcome| outcome.edge()).collect::<Vec<_>>(),
                vec![Some(EId(10)), Some(EId(11))]);
            tx.finish(&mut db, &commit).await.unwrap();
            for (edge, p, q) in [(10, 4, 7), (11, 6, 9)] {
                assert_eq!(db.edge(EId(edge)).unwrap().unwrap().props, vec![
                    (P, CanonicalScalar::Int(p).into()), (Q, CanonicalScalar::Int(q).into()),
                    (AFTER, CanonicalScalar::Int(p).into()),
                ]);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
