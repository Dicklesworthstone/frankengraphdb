//! Database-owned exact triangle views, using the same committed-input,
//! snapshot source, failure fence and prepared sink as other maintained views.

use super::*;
use fgdb_delta_types::zset::committed::EdgeInputError;
use fgdb_delta_types::zset::triangles::committed::{CommittedTriangles, CommittedTrianglesError};
use fgdb_delta_types::zset::triangles::{TriangleError, TriangleQuantifier};
use fgdb_delta_types::{LimbLimit, ZWeight};

const LIMBS: LimbLimit = LimbLimit::new(4);
type Triple = (VId, VId, VId);

pub(crate) struct State {
    input: CommittedTriangles,
    rows: ZSet<Triple>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}
fn input_error(error: CommittedTrianglesError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        CommittedTrianglesError::Input(EdgeInputError::Delta(error))
        | CommittedTrianglesError::Triangles(TriangleError::Delta(error)) => zset_error(error),
        _ => StandingQueryFailure::InvalidDelta,
    }
}
fn result_bound(total: &ZWeight, policy: GqlQueryPolicy) -> Result<(), StandingQueryFailure> {
    if total < &ZWeight::ZERO {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    if policy
        .rows
        .max_result_rows()
        .is_some_and(|limit| total > &ZWeight::from_i128(i128::from(limit)))
    {
        return Err(StandingQueryFailure::ResultBudget);
    }
    Ok(())
}
impl State {
    pub(super) fn relation(&self) -> RelationId {
        self.input.relation()
    }
    pub(super) fn quantifier(&self) -> TriangleQuantifier {
        self.input.quantifier()
    }

    pub(super) fn maintain(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        if self.frontier != self.input.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        recursive::observe_batch(batch, meter)?;
        let pending = self
            .input
            .prepare_committed_successor(cx, batch, LIMBS, &mut |event| meter.charge(event))
            .map_err(input_error)?;
        // Bound the FINAL occurrence count, not support size or a transient
        // insertion-before-retraction prefix. ALL does not expand duplicates.
        result_bound(pending.total(), meter.policy)?;
        let sink = self
            .rows
            .prepare_update(pending.delta(), LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (triple, _) in pending.delta().iter() {
            meter.charge(ZSetEvent::Work)?;
            if sink
                .weight(triple)
                .is_some_and(|weight| weight < &ZWeight::ZERO)
            {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        (meter.checkpoint)()?;
        // Input identity, exact total and materialized triples publish together.
        // No callbacks or recoverable work occur between these publications.
        let _ = pending.commit();
        sink.commit();
        Ok(())
    }

    fn from_snapshot(
        snapshot: &crate::Snapshot,
        relation: RelationId,
        quantifier: TriangleQuantifier,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let baseline = recursive::topology_snapshot(snapshot, relation, meter)?;
        let input = CommittedTriangles::from_snapshot(
            baseline,
            relation,
            quantifier,
            LIMBS,
            &mut |event| meter.charge(event),
        )
        .map_err(input_error)?;
        result_bound(input.total(), meter.policy)?;
        let rows = input
            .snapshot(LIMBS, &mut |event| meter.charge(event))
            .map_err(input_error)?;
        (meter.checkpoint)()?;
        Ok(Self {
            input,
            rows,
            policy: meter.policy,
            frontier: snapshot.frontier,
            stats: meter.stats,
            failure: None,
        })
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Register exact triangles in the undirected projection of one relation.
    /// Keys are native triples (a,b,c) with a < b < c. DISTINCT assigns each
    /// supported triple weight one; ALL multiplies the three unordered-side
    /// multiplicities, including parallel and opposite-oriented edges.
    /// Self-loops never form triangles. Labels, properties and valid time do not
    /// filter this explicit topology API; it is not directed-cycle counting.
    ///
    /// Ordinary committed writes maintain only changed neighborhoods. A read
    /// returns the exact current view or a typed unavailable error, never stale
    /// rows labeled current. Failure affects this derived view, not the durable
    /// write or healthy siblings. rebuild_standing_query repairs from CURRENT
    /// source state without replaying historical triangles or changing the handle.
    ///
    /// Result budgets count triangle occurrences, not distinct support keys.
    /// Work/scratch include topology input and sink under one allowance. Source
    /// records at initialization include physical versions and tombstones. This
    /// retains in-memory arrangements and triples, not spill or byte-memory bounds.
    /// Registration is session-local, not durable subscription/certificate state.
    pub fn register_standing_triangles(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        quantifier: TriangleQuantifier,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let query = self.prepare_standing_triangles(cx, relation, quantifier, policy)?;
        Ok(self.store_standing_query(StandingQuery::Triangles(Box::new(query))))
    }

    pub(super) fn prepare_standing_triangles(
        &self,
        cx: &QueryCx,
        relation: RelationId,
        quantifier: TriangleQuantifier,
        policy: GqlQueryPolicy,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
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
            State::from_snapshot(&self.snapshot, relation, quantifier, &mut meter)
                .map_err(StandingQueryError::Maintenance)
        })
    }

    /// Borrow the exact current canonical weighted triple set. ordered_rows()
    /// is None: the Z-set's native triple order is canonical and multiplicities
    /// are not expanded into repeated allocations. Shares the registry's owner,
    /// health, cancellation and unavailable checks with aggregate/recursive reads.
    pub fn standing_triangles<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, (VId, VId, VId)>, StandingQueryError> {
        let StandingQuery::Triangles(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView {
            rows: &query.rows,
            ordered: None,
            frontier: query.frontier,
            stats: &query.stats,
        })
    }

    /// Borrow the maintained exact occurrence count without scanning triples or
    /// narrowing wide arithmetic. The database borrow pins it to the same
    /// generation as a simultaneously borrowed standing_triangles() result.
    pub fn standing_triangle_total<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<&'a ZWeight, StandingQueryError> {
        let StandingQuery::Triangles(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.input.total())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts};

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
    }
    fn build(snapshot: &crate::Snapshot) -> State {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        State::from_snapshot(snapshot, RelationId(1), TriangleQuantifier::All, &mut meter).unwrap()
    }
    fn unchanged(actual: &State, before: &State) {
        assert_eq!(actual.input, before.input);
        assert_eq!(actual.rows, before.rows);
        assert_eq!(actual.frontier, before.frontier);
        assert_eq!(actual.stats, before.stats);
        assert_eq!(actual.failure, before.failure);
        assert_eq!(actual.policy, before.policy);
    }

    #[test]
    fn every_input_operator_sink_and_final_refusal_is_atomic_and_retryable() {
        let ((), report) = run_async_under_lab(0x7472_6910, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = DatabaseKeys::new(
                [0xe1; 32],
                DatabaseSecurityNamespaceId([0xe2; 32]),
                [0xe3; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=4 {
                seed.create_vertex(VId(id), vec![], vec![]);
            }
            for (id, a, b) in [(1, 1, 2), (2, 2, 3), (3, 3, 1)] {
                seed.add_edge(EId(id), VId(a), VId(b), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let baseline = Arc::clone(&db.snapshot);
            let before = build(&baseline);
            let mut swap = WriteBatch::new(RelationId(1));
            swap.delete_edge(EId(1));
            swap.add_edge(EId(4), VId(1), VId(4), vec![]);
            swap.add_edge(EId(5), VId(4), VId(3), vec![]);
            db.write(&commit, swap).await.unwrap();
            let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();
            let expected = build(&db.snapshot);
            assert_ne!(before.rows, expected.rows);
            let mut success = build(&baseline);
            let mut calls = 0;
            let stats = {
                let mut checkpoint = || {
                    calls += 1;
                    Ok(())
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                success.maintain(&commit, batch, &mut meter).unwrap();
                meter.stats
            };
            assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
            assert_eq!(success.rows, expected.rows);
            assert_eq!(success.input, expected.input);
            assert_eq!(success.input.frontier(), batch.commit_seq());
            for stop in 1..=calls {
                let mut state = build(&baseline);
                let mut visited = 0;
                {
                    let mut checkpoint = || {
                        visited += 1;
                        if visited == stop {
                            Err(StandingQueryFailure::Interrupted)
                        } else {
                            Ok(())
                        }
                    };
                    let mut meter = Meter {
                        policy: policy(),
                        stats: StandingQueryStats::default(),
                        checkpoint: &mut checkpoint,
                    };
                    assert_eq!(
                        state.maintain(&commit, batch, &mut meter),
                        Err(StandingQueryFailure::Interrupted),
                        "refusal at {stop}"
                    );
                }
                assert_eq!(visited, stop);
                unchanged(&state, &before);
            }
            // Exact admission succeeds, one-less work/scratch refuses without
            // publishing even a successfully prepared upstream participant.
            for (work, scratch, error) in [
                (stats.work_units, stats.scratch_entries, None),
                (
                    stats.work_units - 1,
                    stats.scratch_entries,
                    Some(StandingQueryFailure::WorkBudget),
                ),
                (
                    stats.work_units,
                    stats.scratch_entries - 1,
                    Some(StandingQueryFailure::ScratchBudget),
                ),
            ] {
                let mut state = build(&baseline);
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: GqlQueryPolicy::new(100_000, 1, work, scratch),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                let result = state.maintain(&commit, batch, &mut meter);
                if let Some(error) = error {
                    assert_eq!(result, Err(error));
                    unchanged(&state, &before);
                } else {
                    result.unwrap();
                    assert_eq!(state.rows, expected.rows);
                    assert_eq!(state.input, expected.input);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
