//! Lossless native delivery aliases for typed maintained row circuits.
//! The alias owns presentation metadata only, never another registry entry.

use super::*;

impl<V: Vfs + Clone> Database<V> {
    /// Attach native column metadata to a typed row/set/join/filter/projection/
    /// window/closure handle WITHOUT copying rows or registering another circuit.
    /// Use the result with native cursors, bag/delta delivery or acknowledged
    /// subscriptions. Both handles identify the same maintained state, failure
    /// boundary and rebuild target; their lifetimes never change query semantics.
    ///
    /// An already native handle is shared unchanged, including owned-circuit
    /// rebuild metadata and aggregate output slots. This must not flatten a
    /// Circuit layout to Rows and accidentally rebuild only its final node.
    /// New aliases use the registry's complete positional names/types, even for
    /// empty results. Existing selected pages are not recomputed or moved across
    /// a filter/join. Ranked windows keep their order; other typed relational
    /// outputs use canonical bag order, not an implicit execution sequence.
    ///
    /// Only work/scratch allowances apply to the metadata copy; SnapshotRecords
    /// and ResultRows are unused. Existing native metadata is Arc-shared after
    /// owner/health/cancellation admission without another payload-copy charge.
    /// No source graph read, per-consumer maintenance node, durable registration
    /// or user-defined query label is introduced. Unsupported row domains refuse.
    pub fn standing_native_handle(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let source = self.admitted_standing_query(cx, handle)?;
        if handle.native.is_some() {
            return Ok(handle.clone());
        }
        let names = sets::columns(source).ok_or(StandingQueryError::Unsupported)?;
        sets::rows(source).ok_or(StandingQueryError::Unsupported)?;
        cx.with_restriction(|| {
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Delivery)?;
            let mut columns = Vec::new();
            for name in names {
                meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Delivery)?;
                let units = name.len().div_ceil(64).checked_add(1)
                    .ok_or(StandingQueryError::Delivery(StandingQueryFailure::ScratchBudget))?;
                meter.units(ZSetEvent::ScratchEntry, units).map_err(StandingQueryError::Delivery)?;
                columns.push(name.clone());
            }
            meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Delivery)?;
            let layout = Arc::new(Layout::Rows { columns });
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            let mut alias = handle.clone();
            alias.native = Some(layout);
            Ok(alias)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts};
    use fgdb_gql::{GraphSetOperand, GraphSetPredicateOp, GraphSymbol, GraphSymbolKind};

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(10000, 10000, 1_000_000, 1_000_000)
    }
    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        }
    }
    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
    }

    fn native_agreement<V: Vfs + Clone>(
        db: &Database<V>, cx: &QueryCx, handle: &StandingQueryHandle,
    ) {
        let (at, result) = db.standing_native_query(cx, handle, policy()).unwrap();
        let QueryResult::Rows { columns, rows } = result else {
            panic!("a native row handle must return rows");
        };
        let cursor = db.standing_native_cursor(cx, handle, policy()).unwrap();
        assert_eq!(cursor.snapshot_seq(), at);
        assert_eq!(cursor.columns(), columns.as_slice());
        assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap(), rows);
        let bag = ZSet::from_updates(
            rows.into_iter().map(|row| (row, ZWeight::ONE)),
            fgdb_delta_types::LimbLimit::new(4), &mut |_| Ok::<_, ()>(()),
        ).unwrap();
        let (bag_at, expected) = db.standing_native_bag(cx, handle, policy()).unwrap();
        assert_eq!(bag_at, at);
        assert_eq!(bag, expected);
    }

    #[test]
    fn typed_post_recursion_join_can_feed_shared_native_subscriptions() {
        let ((), report) = run_async_under_lab(0x6c05_11, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=3 { seed.create_vertex(VId(id), vec![], vec![]); }
            seed.add_edge(EId(1), VId(1), VId(2), vec![]);
            seed.add_edge(EId(2), VId(2), VId(3), vec![]);
            db.write(&commit, seed).await.unwrap();
            let params = GqlParameters::new();
            let source = db.register_standing_native(&cx, "MATCH (a)-[:R]->(b) RETURN a,b",
                &params, symbols, policy()).unwrap();
            let closure = db.register_standing_closure(&cx, &source, [0, 1], policy()).unwrap();
            let vertices = db.register_standing_native(&cx, "MATCH (n) RETURN n",
                &params, symbols, policy()).unwrap();
            let joined = db.register_standing_join(&cx, &closure, &vertices, &[(1, 0)], policy()).unwrap();
            assert!(joined.native.is_none());
            let before = db.standing_queries.len();
            let native = db.standing_native_handle(&cx, &joined, policy()).unwrap();
            assert_eq!(native.index, joined.index);
            assert_eq!(db.standing_queries.len(), before);
            assert!(joined.native.is_none());
            assert_eq!(db.standing_native_columns(&cx, &native).unwrap().len(), 3);
            native_agreement(&db, &cx, &closure);
            native_agreement(&db, &cx, &native);
            let mut sub = db.open_standing_subscription(&cx, &native).unwrap();
            sub.enable_replay(&mut db, &cx, 4, 100, 10000, policy()).unwrap();
            let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            let mut delivered = baseline.rows().checked_clone(fgdb_delta_types::LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(())).unwrap();
            sub.acknowledge(baseline.receipt()).unwrap();
            let mut edit = WriteBatch::new(RelationId(1));
            edit.delete_edge(EId(2));
            let at = db.write(&commit, edit).await.unwrap();
            let delta = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!(delta.frontier(), at);
            delivered.integrate(delta.rows(), fgdb_delta_types::LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(delivered, db.standing_native_bag(&cx, &native, policy()).unwrap().1);
            assert_eq!(delivered.len(), 1);
            native_agreement(&db, &cx, &closure);
            native_agreement(&db, &cx, &native);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn existing_native_circuit_layout_and_rebuild_ownership_are_preserved() {
        let ((), report) = run_async_under_lab(0x6c05_12, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let original = db.register_standing_native(&cx,
                "MATCH (a) RETURN a AS n UNION ALL MATCH (b) RETURN b AS n",
                &GqlParameters::new(), symbols, policy()).unwrap();
            assert!(matches!(original.native.as_deref(), Some(Layout::Circuit { .. })));
            let count = db.standing_queries.len();
            let alias = db.standing_native_handle(&cx, &original, GqlQueryPolicy::new(0, 0, 0, 0)).unwrap();
            assert!(Arc::ptr_eq(original.native.as_ref().unwrap(), alias.native.as_ref().unwrap()));
            assert_eq!(db.standing_queries.len(), count);
            db.rebuild_standing_query(&cx, &alias, policy()).unwrap();
            assert_eq!(db.standing_queries.len(), count);
            assert!(db.standing_native_query(&cx, &original, policy()).is_ok());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn metadata_refusal_and_foreign_handles_do_not_mutate_registrations() {
        let ((), report) = run_async_under_lab(0x6c05_13, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let source = db.register_standing_native(&cx, "MATCH (n) RETURN n",
                &GqlParameters::new(), symbols, policy()).unwrap();
            let typed = db.register_standing_join(&cx, &source, &source, &[(0, 0)], policy()).unwrap();
            let count = db.standing_queries.len();
            for limited in [GqlQueryPolicy::new(0, 0, 0, 100), GqlQueryPolicy::new(0, 0, 100, 0)] {
                assert!(matches!(db.standing_native_handle(&cx, &typed, limited), Err(StandingQueryError::Delivery(_))));
                assert!(typed.native.is_none());
                assert_eq!(db.standing_queries.len(), count);
            }
            let alias = db.standing_native_handle(&cx, &typed, policy()).unwrap();
            assert!(db.standing_native_query(&cx, &alias, policy()).is_ok());
            let foreign = Database::open_memory(&commit, keys()).await.unwrap();
            assert!(matches!(foreign.standing_native_handle(&cx, &alias, policy()), Err(StandingQueryError::ForeignHandle)));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn typed_filter_has_eager_pull_and_signed_delta_delivery() {
        let ((), report) = run_async_under_lab(0x6c05_14, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(1), vec![], vec![]);
            seed.create_vertex(VId(2), vec![], vec![]);
            let basis = db.write(&commit, seed).await.unwrap();
            let source = db.register_standing_native(&cx, "MATCH (n) RETURN n",
                &GqlParameters::new(), symbols, policy()).unwrap();
            let filtered = db.register_standing_filter(&cx, &source, &[
                GraphSetPredicateOp::IsNull {
                    operand: GraphSetOperand::Column(0), is_null: false,
                },
            ], policy()).unwrap();
            let native = db.standing_native_handle(&cx, &filtered, policy()).unwrap();
            native_agreement(&db, &cx, &native);
            let mut delivered = db.standing_native_bag(&cx, &native, policy()).unwrap().1;
            assert_eq!(delivered.len(), 2);
            let mut edit = WriteBatch::new(RelationId(1));
            edit.create_vertex(VId(3), vec![], vec![]);
            let at = db.write(&commit, edit).await.unwrap();
            let cursor = db.standing_native_delta_cursor(&cx, &native, basis, policy())
                .unwrap().unwrap();
            assert_eq!(cursor.snapshot_seq(), at);
            let frames = cursor.collect::<Result<Vec<_>, _>>().unwrap();
            let delta = ZSet::from_updates(frames, fgdb_delta_types::LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(delta, db.standing_native_delta(&cx, &native, basis, policy()).unwrap().1);
            delivered.integrate(&delta, fgdb_delta_types::LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(delivered.len(), 3);
            assert_eq!(delivered, db.standing_native_bag(&cx, &native, policy()).unwrap().1);
            native_agreement(&db, &cx, &native);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
