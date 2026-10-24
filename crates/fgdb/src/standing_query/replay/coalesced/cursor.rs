//! Borrowed net replay delivery without a collected output bag.

use super::*;

/// One borrowed consolidated replay change and its weight.
type ReplayChange<'a> = Result<(&'a [QueryValue], ZWeight), StandingQueryError>;

impl<V: Vfs + Clone> Database<V> {
    /// Open a pull iterator over the net changes in a complete retained prefix.
    /// The tuple's sequence names the SELECTED end, not successful delivery.
    /// Stage each change privately; apply the transition and advance from
    /// `after` only after exhausting the iterator without an error. Dropping
    /// it early is not completion. This API never acknowledges a subscription.
    ///
    /// None means already caught up. Some may yield no rows when changes
    /// cancel, but its end still advances across all selected empty ticks.
    /// Gaps, foreign handles and unavailable replay sinks refuse at opening.
    /// The database borrow pins the selected history and prevents concurrent
    /// writes/rebuild/eviction through this Database until borrows are released.
    /// Dropping the caller's replay handle does not invalidate the iterator.
    ///
    /// Each item contains borrowed native cells and an owned exact signed
    /// weight. Columns match the replay source. Full-width identities and
    /// numeric representations are not coerced. No payload clone, occurrence
    /// expansion, graph scan, query execution or output bag is performed.
    /// Merge state is one iterator/head per nonempty selected tick plus one
    /// exact accumulator. Retained history and caller-owned results remain
    /// outside this transient bound; it is not a live byte cap or spill.
    ///
    /// Opening and all pulls share one cumulative work/scratch policy.
    /// ResultRows counts successfully delivered nonzero net keys. It is
    /// checked after a key's complete sum, so cancellations cost no rows.
    /// Arithmetic, budget or interruption can fail AFTER earlier items; one
    /// error permanently fuses delivery and releases the merge without drain.
    /// Even the final EOF checkpoint must succeed. Retrying by reopening does
    /// not consume history or change an ACK. Use standing_replay_coalesced for
    /// a materialized, all-or-nothing result instead. Neither is an event feed,
    /// durable cursor, receipt or capability-authorized transport operation.
    pub fn standing_replay_coalesced_cursor<'a>(
        &'a self,
        cx: &'a QueryCx,
        replay: &StandingQueryHandle,
        after: CommitSeq,
        max_ticks: NonZeroUsize,
        policy: GqlQueryPolicy,
    ) -> Result<
        Option<(
            CommitSeq,
            impl core::iter::FusedIterator<Item = ReplayChange<'a>> + 'a + use<'a, V>,
        )>,
        StandingQueryError,
    > {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        if state.history.frontier() != state.frontier {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ));
        }
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let Some((frontier, range)) =
                state.history.select_range(after, max_ticks, &mut meter)?
            else {
                return Ok(None);
            };
            let sources = state
                .history
                .frames
                .range(range)
                .map(|frame| frame.rows.iter());
            let merge =
                SortedMerge::new(sources, &mut meter).map_err(StandingQueryError::Delivery)?;
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            let mut pull = Pull {
                merge: Some(merge),
                delivered: 0,
                policy,
                stats: meter.stats,
            };
            let cursor = std::iter::from_fn(move || {
                cx.with_restriction(|| {
                    pull.advance(&mut || {
                        cx.checkpoint()
                            .map_err(|_| StandingQueryFailure::Interrupted)
                    })
                })
            })
            .fuse();
            Ok(Some((frontier, cursor)))
        })
    }

    /// Stream `result(after) - result(before)` for two exact retained cuts.
    /// This is the nonmaterializing form of standing_replay_difference, not a
    /// prefix catch-up request. Both cuts and every selected successor must be
    /// present before opening succeeds. Equal cuts still require owner, health
    /// and retention admission, then return an iterator whose EOF is checked.
    ///
    /// Forward and reverse requests merge the same chronological tick range.
    /// A reverse request negates each complete exact weight after cancellation;
    /// it does not reverse key order, tick arithmetic or selected source scope.
    /// Native row payloads are borrowed, including nested values and full-width
    /// identities. No output bag, payload copy, occurrence expansion, graph
    /// read or query reevaluation is performed. The immutable database borrow
    /// pins the range against writes, eviction and rebuild for the cursor's life.
    ///
    /// Opening, weight arithmetic and all pulls use one cumulative allowance.
    /// ResultRows charges only delivered nonzero net keys. One error or panic
    /// fuses the pull and releases its merge; dropping it never drains or ACKs.
    /// Earlier items may precede an error, including an EOF interruption: stage
    /// them privately and apply the change only after successful exhaustion.
    /// Do not advance a consumer baseline merely because opening succeeded.
    /// Reopening is an independent retry over unchanged retained history.
    ///
    /// Only this sink's session-local derived history is available. There is
    /// no durable cursor, cross-branch history, raw Chronicle fallback or
    /// capability-authorized delivery. Caller-owned results and retained frames
    /// are outside the merge's one-head-per-nonempty-tick transient bound.
    pub fn standing_replay_difference_cursor<'a>(
        &'a self,
        cx: &'a QueryCx,
        replay: &StandingQueryHandle,
        before: CommitSeq,
        after: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        impl core::iter::FusedIterator<Item = ReplayChange<'a>> + 'a + use<'a, V>,
        StandingQueryError,
    > {
        let StandingQuery::Replay(state) = self.admitted_standing_query(cx, replay)? else {
            return Err(StandingQueryError::Unsupported);
        };
        if state.history.frontier() != state.frontier {
            return Err(StandingQueryError::Delivery(
                StandingQueryFailure::InvalidDelta,
            ));
        }
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let range = state.history.select_difference(before, after, &mut meter)?;
            let sources = state
                .history
                .frames
                .range(range)
                .map(|frame| frame.rows.iter());
            let merge =
                SortedMerge::new(sources, &mut meter).map_err(StandingQueryError::Delivery)?;
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            let mut pull = Pull {
                merge: Some(merge),
                delivered: 0,
                policy,
                stats: meter.stats,
            };
            let reverse = before > after;
            Ok(std::iter::from_fn(move || {
                cx.with_restriction(|| {
                    pull.advance_oriented(reverse, &mut || {
                        cx.checkpoint()
                            .map_err(|_| StandingQueryFailure::Interrupted)
                    })
                })
            })
            .fuse())
        })
    }
}

struct Pull<'a, I> {
    merge: Option<SortedMerge<'a, I>>,
    delivered: u64,
    policy: GqlQueryPolicy,
    stats: StandingQueryStats,
}

impl<'a, I: Iterator<Item = Entry<'a>>> Pull<'a, I> {
    fn advance(
        &mut self,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Option<Result<(&'a [QueryValue], ZWeight), StandingQueryError>> {
        self.advance_oriented(false, checkpoint)
    }

    fn advance_oriented(
        &mut self,
        reverse: bool,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Option<Result<(&'a [QueryValue], ZWeight), StandingQueryError>> {
        // Take ownership before any fallible/user code. A refusal or unwind
        // drops the merge and cannot resume a partly summed or undelivered key.
        let mut merge = self.merge.take()?;
        let mut meter = Meter {
            policy: self.policy,
            stats: self.stats,
            checkpoint,
        };
        let result = (|| {
            let Some((row, weight)) = merge.next(&mut meter)? else {
                (meter.checkpoint)()?;
                return Ok(None);
            };
            let delivered = self
                .delivered
                .checked_add(1)
                .ok_or(StandingQueryFailure::ResultBudget)?;
            if self
                .policy
                .rows
                .max_result_rows()
                .is_some_and(|limit| delivered > limit)
            {
                return Err(StandingQueryFailure::ResultBudget);
            }
            let weight = orient_weight(weight, reverse, &mut meter)?;
            // The caller gets a reference to immutable retained cells; no
            // payload scratch is charged for a clone that never happens.
            (meter.checkpoint)()?;
            Ok(Some((row.as_slice(), weight, delivered)))
        })();
        self.stats = meter.stats;
        match result {
            Ok(Some((row, weight, delivered))) => {
                self.delivered = delivered;
                self.merge = Some(merge);
                Some(Ok((row, weight)))
            }
            Ok(None) => None,
            Err(error) => Some(Err(StandingQueryError::Delivery(error))),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod difference_tests {
    use super::*;
    use crate::{DatabaseKeys, QueryResult, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
    }
    fn delivery(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(0, rows, 10_000_000, 10_000_000)
    }
    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([11; 32], DatabaseSecurityNamespaceId([12; 32]), [13; 32])
    }
    fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }
    fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
        let QueryResult::Rows { rows, .. } = result else {
            panic!("native rows required");
        };
        ZSet::from_updates(
            rows.into_iter().map(|row| (row, ZWeight::ONE)),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap()
    }
    fn endpoint_difference(
        before: &ZSet<Vec<QueryValue>>,
        after: &ZSet<Vec<QueryValue>>,
    ) -> ZSet<Vec<QueryValue>> {
        // Independent ordinary query snapshots, not replay output or its merge.
        let mut result = ZSet::new();
        for (row, weight) in before.iter() {
            result
                .accumulate(
                    row.clone(),
                    weight.checked_neg(LIMBS).unwrap(),
                    LIMBS,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
        }
        for (row, weight) in after.iter() {
            result
                .accumulate(
                    row.clone(),
                    weight.checked_clone(LIMBS).unwrap(),
                    LIMBS,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
        }
        result
    }

    #[test]
    fn committed_native_histories_match_every_exact_endpoint_pair_without_source_reads() {
        let ((), report) = run_async_under_lab(0x007d_de61, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for (id, value) in [(1, 3), (2, 3), (3, 7)] {
                seed.create_vertex(
                    VId(id),
                    vec![],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
                );
            }
            let start = db.write(&commit, seed).await.unwrap();
            let params = GqlParameters::new();
            let texts = [
                "MATCH (n) RETURN n.p AS p",
                "MATCH (n) RETURN DISTINCT n.p AS p",
                "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
                "MATCH (n) RETURN COUNT(*) AS c,SUM(n.p) AS s,AVG(n.p) AS a",
                "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            ];
            let mut sources = Vec::new();
            let mut logs = Vec::new();
            for text in texts {
                let source = db
                    .register_standing_native(&cx, text, &params, resolve, policy())
                    .unwrap();
                logs.push(
                    db.register_standing_replay(&cx, &source, 16, 1000, 100_000, delivery(100_000))
                        .unwrap(),
                );
                sources.push(source);
            }
            let read_all = |db: &Database<crate::MemVfs>| {
                texts
                    .iter()
                    .map(|text| {
                        result_bag(db.query(&cx, text, &params, resolve, policy()).unwrap())
                    })
                    .collect::<Vec<_>>()
            };
            let mut snapshots = vec![read_all(&db)];
            let mut cuts = vec![start];
            for tick in 0..3 {
                let mut batch = WriteBatch::new(RelationId(1));
                if tick == 0 {
                    batch.delete_vertex(VId(1));
                } else if tick == 1 {
                    batch.set_vertex_property(
                        VId(3),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(2)),
                    );
                } else {
                    // A real commit with no change to any selected output.
                    batch.set_vertex_property(
                        VId(2),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Int(1)),
                    );
                }
                cuts.push(db.write(&commit, batch).await.unwrap());
                snapshots.push(read_all(&db));
            }
            for (index, log) in logs.iter().enumerate() {
                let window = db.standing_replay_window(&cx, log).unwrap();
                for before in 0..cuts.len() {
                    for after in 0..cuts.len() {
                        let expected = endpoint_difference(
                            &snapshots[before][index],
                            &snapshots[after][index],
                        );
                        let allowance = delivery(expected.len() as u64);
                        let materialized = db
                            .standing_replay_difference(
                                &cx,
                                log,
                                cuts[before],
                                cuts[after],
                                allowance,
                            )
                            .unwrap();
                        assert_eq!(materialized, expected, "{index}: {before}->{after}");
                        let mut streamed = ZSet::new();
                        let mut cursor = db
                            .standing_replay_difference_cursor(
                                &cx,
                                log,
                                cuts[before],
                                cuts[after],
                                allowance,
                            )
                            .unwrap();
                        for change in cursor.by_ref() {
                            let (row, weight) = change.unwrap();
                            streamed
                                .accumulate(row.to_vec(), weight, LIMBS, &mut |_| Ok::<_, ()>(()))
                                .unwrap();
                        }
                        assert!(cursor.next().is_none());
                        assert_eq!(streamed, expected, "{index}: {before}->{after}");
                    }
                }
                assert_eq!(db.standing_replay_window(&cx, log).unwrap(), window);
                // Dropping a partial cursor does not consume the interval.
                let mut partial = db
                    .standing_replay_difference_cursor(
                        &cx,
                        log,
                        cuts[3],
                        cuts[0],
                        delivery(100_000),
                    )
                    .unwrap();
                assert!(partial.next().unwrap().is_ok());
                drop(partial);
                assert_eq!(db.standing_replay_window(&cx, log).unwrap(), window);
            }
            let future = cuts[3].checked_successor().unwrap();
            assert!(matches!(
                db.standing_replay_difference_cursor(&cx, &logs[0], future, future, delivery(0),),
                Err(StandingQueryError::ReplayGap { .. })
            ));
            assert!(matches!(
                db.standing_replay_difference_cursor(
                    &cx,
                    &sources[0],
                    cuts[0],
                    cuts[3],
                    delivery(100_000),
                ),
                Err(StandingQueryError::Unsupported)
            ));
            let foreign = Database::open_memory(&commit, keys()).await.unwrap();
            assert!(matches!(
                foreign.standing_replay_difference_cursor(
                    &cx,
                    &logs[0],
                    cuts[0],
                    cuts[3],
                    delivery(100_000),
                ),
                Err(StandingQueryError::ForeignHandle)
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn failed_or_rebuilt_sinks_cannot_fabricate_equal_or_reverse_history() {
        let ((), report) = run_async_under_lab(0x007d_de62, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(1), vec![], vec![]);
            let start = db.write(&commit, seed).await.unwrap();
            let source = db
                .register_standing_native(
                    &cx,
                    "MATCH (n) RETURN n",
                    &GqlParameters::new(),
                    resolve,
                    policy(),
                )
                .unwrap();
            let log = db
                .register_standing_replay(&cx, &source, 2, 0, 10, delivery(100))
                .unwrap();
            let mut update = WriteBatch::new(RelationId(1));
            update.delete_vertex(VId(1));
            let at = db.write(&commit, update).await.unwrap();
            assert!(matches!(
                db.standing_replay_difference_cursor(&cx, &log, start, start, delivery(0)),
                Err(StandingQueryError::Unavailable { .. })
            ));
            db.rebuild_standing_query(&cx, &log, policy()).unwrap();
            for (before, after) in [(start, at), (at, start), (start, start)] {
                assert!(matches!(
                    db.standing_replay_difference_cursor(&cx, &log, before, after, delivery(0),),
                    Err(StandingQueryError::ReplayGap { .. })
                ));
            }
            let mut empty = db
                .standing_replay_difference_cursor(&cx, &log, at, at, delivery(0))
                .unwrap();
            assert!(empty.next().is_none());
            assert!(empty.next().is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    fn open<'a>(
        sources: &'a [ZSet<Vec<QueryValue>>],
        policy: GqlQueryPolicy,
    ) -> Pull<'a, impl Iterator<Item = Entry<'a>> + use<'a>> {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let merge = SortedMerge::new(sources.iter().map(ZSet::iter), &mut meter).unwrap();
        Pull {
            merge: Some(merge),
            delivered: 0,
            policy,
            stats: meter.stats,
        }
    }
    fn small(entries: &[(i128, i128)]) -> ZSet<Vec<QueryValue>> {
        ZSet::from_updates(
            entries.iter().map(|(key, weight)| {
                (vec![QueryValue::Integer(*key)], ZWeight::from_i128(*weight))
            }),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap()
    }

    #[test]
    fn reverse_pull_refusals_and_unwinds_fuse_without_consuming_source_history() {
        let sources = [small(&[(1, 7), (2, -1)]), small(&[(1, -2), (2, 5)])];
        let mut good = open(&sources, delivery(2));
        let mut calls = 0;
        let mut expected = Vec::new();
        while let Some(change) = good.advance_oriented(true, &mut || {
            calls += 1;
            Ok(())
        }) {
            let (row, weight) = change.unwrap();
            expected.push((row.to_vec(), weight.to_i128().unwrap()));
        }
        assert_eq!(
            expected,
            vec![
                (vec![QueryValue::Integer(1)], -5),
                (vec![QueryValue::Integer(2)], -4),
            ]
        );
        for stop in 1..=calls {
            let mut pull = open(&sources, delivery(2));
            let mut seen = 0;
            let mut prefix = Vec::new();
            loop {
                match pull.advance_oriented(true, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                }) {
                    Some(Ok((row, weight))) => {
                        prefix.push((row.to_vec(), weight.to_i128().unwrap()));
                    }
                    Some(Err(StandingQueryError::Delivery(StandingQueryFailure::Interrupted))) => {
                        break;
                    }
                    _ => panic!("missed reverse interruption"),
                }
            }
            assert_eq!(seen, stop);
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(pull.delivered, prefix.len() as u64);
            assert!(pull.merge.is_none());
            assert!(
                pull.advance_oriented(true, &mut || panic!("fused"))
                    .is_none()
            );
        }
        for stop in [1, calls / 2, calls] {
            let mut pull = open(&sources, delivery(2));
            let mut seen = 0;
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while pull
                        .advance_oriented(true, &mut || {
                            seen += 1;
                            assert_ne!(seen, stop, "injected reverse unwind");
                            Ok(())
                        })
                        .is_some()
                    {}
                }))
                .is_err()
            );
            assert!(pull.merge.is_none());
            assert!(
                pull.advance_oriented(true, &mut || panic!("fused"))
                    .is_none()
            );
        }
        for (work, scratch, rows, passes) in [
            (good.stats.work_units, good.stats.scratch_entries, 2, true),
            (
                good.stats.work_units - 1,
                good.stats.scratch_entries,
                2,
                false,
            ),
            (
                good.stats.work_units,
                good.stats.scratch_entries - 1,
                2,
                false,
            ),
            (good.stats.work_units, good.stats.scratch_entries, 1, false),
        ] {
            let mut pull = open(&sources, GqlQueryPolicy::new(0, rows, work, scratch));
            let mut succeeded = true;
            while let Some(change) = pull.advance_oriented(true, &mut || Ok(())) {
                if change.is_err() {
                    succeeded = false;
                    break;
                }
            }
            assert_eq!(succeeded, passes);
            assert!(pull.merge.is_none());
        }
    }

    #[test]
    fn reverse_promoted_weight_keeps_borrowed_payload_and_cancellation_costs_no_row() {
        let sources = [small(&[(1, i128::MIN)])];
        let mut pull = open(&sources, delivery(1));
        let (row, weight) = pull
            .advance_oriented(true, &mut || Ok(()))
            .unwrap()
            .unwrap();
        assert!(std::ptr::eq(
            row,
            sources[0].iter().next().unwrap().0.as_slice()
        ));
        let expected = ZWeight::from_i128(i128::MAX)
            .checked_add(&ZWeight::ONE, LIMBS)
            .unwrap();
        assert_eq!(weight, expected);
        assert!(weight.is_promoted());
        assert!(pull.advance_oriented(true, &mut || Ok(())).is_none());
        let cancelled = [small(&[(1, 8)]), small(&[(1, -8)])];
        let mut pull = open(&cancelled, delivery(0));
        assert!(pull.advance_oriented(true, &mut || Ok(())).is_none());
        assert_eq!(pull.delivered, 0);
    }
}
