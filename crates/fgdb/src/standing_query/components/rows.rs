//! Injective native-row projection of accepted component membership changes.
//!
//! This sink owns no topology. It consumes the kernel's exact signed pairs,
//! prepares its row replacements before any source publication, and retains
//! one complete derivative for the existing dependency-ordered row circuit.

use super::*;
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_gql::algebra::GraphValue;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct State {
    columns: [String; 2],
    rows: ZSet<GraphValueRow>,
    last_delta: Option<ZSet<GraphValueRow>>,
}

impl State {
    pub(super) fn columns(&self) -> &[String] {
        &self.columns
    }

    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> {
        &self.rows
    }

    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> {
        self.last_delta.as_ref()
    }

    pub(super) fn from_membership(
        membership: &ZSet<Pair>,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        // Fixed metadata plus the native handle's presentation copy. Payload
        // units count entries, not allocator bytes, just as other row sinks do.
        meter.units(ZSetEvent::ScratchEntry, 8)?;
        let mut state = Self {
            columns: ["vertex".into(), "component".into()],
            rows: ZSet::new(),
            last_delta: None,
        };
        state.prepare(membership, membership.len(), meter)?.commit();
        // A baseline is NOT a successor, even at a nonzero database frontier.
        state.last_delta = None;
        Ok(state)
    }

    pub(super) fn prepare(
        &mut self,
        delta: &ZSet<Pair>,
        vertex_count: usize,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        result_bound(vertex_count, meter.policy)?;
        let mut output = ZSet::new();
        let mut inserted = 0_u128;
        let mut removed = 0_u128;
        for ((vertex, representative), weight) in delta.iter() {
            meter.charge(ZSetEvent::Work)?;
            // Reserve the row and both fixed-width native identity cells
            // before constructing them. The projection preserves exact IDs.
            meter.units(ZSetEvent::ScratchEntry, 3)?;
            let row = GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(*vertex),
                GraphValue::Vertex(*representative),
            ]);
            let change = match weight.to_i128() {
                Some(1) if self.rows.weight(&row).is_none() => {
                    inserted = inserted
                        .checked_add(1)
                        .ok_or(StandingQueryFailure::Arithmetic)?;
                    ZWeight::ONE
                }
                Some(-1) if self.rows.weight(&row) == Some(&ZWeight::ONE) => {
                    removed = removed
                        .checked_add(1)
                        .ok_or(StandingQueryFailure::Arithmetic)?;
                    ZWeight::from_i128(-1)
                }
                _ => return Err(StandingQueryFailure::InvalidDelta),
            };
            output
                .accumulate(row, change, LIMBS, &mut |event| meter.charge(event))
                .map_err(zset_error)?;
        }
        // A representative change can insert a lower-sorting pair before
        // retracting its predecessor. Only the complete successor is bounded.
        let next_count = (self.rows.len() as u128)
            .checked_sub(removed)
            .and_then(|count| count.checked_add(inserted))
            .ok_or(StandingQueryFailure::InvalidDelta)?;
        if next_count != vertex_count as u128 {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        for _ in output.iter() {
            meter.charge(ZSetEvent::Work)?;
            // prepare_update clones each changed native key into its private
            // replacements; unchanged membership is never copied or scanned.
            meter.units(ZSetEvent::ScratchEntry, 3)?;
        }
        let sink = self
            .rows
            .prepare_update(&output, LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        (meter.checkpoint)()?;
        Ok(Update {
            sink,
            output,
            last_delta: &mut self.last_delta,
        })
    }
}

#[must_use = "dropping a component-row update preserves rows and its previous derivative"]
pub(super) struct Update<'a> {
    sink: ZSetUpdate<'a, GraphValueRow>,
    output: ZSet<GraphValueRow>,
    last_delta: &'a mut Option<ZSet<GraphValueRow>>,
}

impl Update<'_> {
    pub(super) fn commit(self) {
        self.sink.commit();
        // Some(empty) is essential: downstream nodes still advance on a tick
        // that changed properties or another relation but not these components.
        *self.last_delta = Some(self.output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
    }

    fn pairs(values: &[(u128, u128, i128)]) -> ZSet<Pair> {
        ZSet::from_updates(
            values
                .iter()
                .map(|&(v, r, weight)| ((VId(v), VId(r)), ZWeight::from_i128(weight))),
            LIMBS,
            &mut |_| Ok::<_, StandingQueryFailure>(()),
        )
        .unwrap()
    }

    fn build(values: &[(u128, u128, i128)]) -> State {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        State::from_membership(&pairs(values), &mut meter).unwrap()
    }

    fn decoded(rows: &ZSet<GraphValueRow>) -> BTreeMap<(u128, u128), i128> {
        rows.iter()
            .map(|(row, weight)| match row.values() {
                [GraphValue::Vertex(v), GraphValue::Vertex(r)] => {
                    ((v.0, r.0), weight.to_i128().unwrap())
                }
                _ => panic!("component projection changed its declared native schema"),
            })
            .collect()
    }

    #[test]
    fn exhaustive_partial_memberships_preserve_exact_signed_derivatives() {
        // Independent assignment enumeration, not the SCC or projection helper.
        // Each of three vertices is absent or names one of three representatives.
        let membership = |mut code: usize| {
            let mut rows = BTreeMap::new();
            for vertex in 0..3_u128 {
                let digit = code % 4;
                code /= 4;
                if digit != 0 {
                    rows.insert((vertex, digit as u128 - 1), 1_i128);
                }
            }
            rows
        };
        for before in 0..64 {
            for after in 0..64 {
                let before = membership(before);
                let after = membership(after);
                let values: Vec<_> = before.iter().map(|(&(v, r), &w)| (v, r, w)).collect();
                let mut state = build(&values);
                let mut expected_delta = BTreeMap::new();
                for &key in before.keys() {
                    if !after.contains_key(&key) {
                        expected_delta.insert(key, -1_i128);
                    }
                }
                for &key in after.keys() {
                    if !before.contains_key(&key) {
                        expected_delta.insert(key, 1_i128);
                    }
                }
                let delta = pairs(
                    &expected_delta
                        .iter()
                        .map(|(&(v, r), &w)| (v, r, w))
                        .collect::<Vec<_>>(),
                );
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                state
                    .prepare(&delta, after.len(), &mut meter)
                    .unwrap()
                    .commit();
                assert_eq!(decoded(state.rows()), after);
                assert_eq!(decoded(state.delta().unwrap()), expected_delta);
            }
        }
    }

    #[test]
    fn wide_ids_final_row_limit_and_baseline_empty_distinction() {
        let wide = 1_u128 << 100;
        let mut state = build(&[(wide, wide, 1), (u128::MAX, wide, 1)]);
        assert_eq!(state.columns(), ["vertex", "component"]);
        assert!(state.delta().is_none());
        let delta = pairs(&[
            (wide, wide, -1),
            (u128::MAX, wide, -1),
            (wide, 0, 1),
            (u128::MAX, 0, 1),
        ]);
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: GqlQueryPolicy::new(100_000, 2, 100_000, 100_000),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        state.prepare(&delta, 2, &mut meter).unwrap().commit();
        assert_eq!(
            decoded(state.rows()),
            [((wide, 0), 1), ((u128::MAX, 0), 1)].into()
        );
        assert_eq!(state.delta().unwrap().len(), 4);
        state.prepare(&ZSet::new(), 2, &mut meter).unwrap().commit();
        assert!(state.delta().unwrap().is_empty());
        let rebuilt = build(&[(wide, 0, 1), (u128::MAX, 0, 1)]);
        assert_eq!(state.rows(), rebuilt.rows());
        assert!(rebuilt.delta().is_none());
    }

    #[test]
    fn malformed_projection_ticks_preserve_the_previous_rows_and_delta() {
        for (bad, count) in [
            (vec![(1, 1, 1)], 2),
            (vec![(9, 9, -1)], 0),
            (vec![(9, 9, 2)], 3),
            (vec![(1, 1, -2)], 0),
            (vec![(9, 9, 1)], 1),
            (vec![], 0),
        ] {
            let mut state = build(&[(1, 1, 1)]);
            let before = build(&[(1, 1, 1)]);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert!(matches!(
                state.prepare(&pairs(&bad), count, &mut meter),
                Err(StandingQueryFailure::InvalidDelta)
            ));
            assert_eq!(state, before);
        }
    }

    #[test]
    fn every_checkpoint_exact_allowance_drop_and_unwind_keep_publication_atomic() {
        let baseline = [(1, 1, 1), (2, 2, 1), (3, 3, 1)];
        let change = pairs(&[(2, 2, -1), (3, 3, -1), (2, 1, 1), (3, 1, 1)]);
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
            success.prepare(&change, 3, &mut meter).unwrap().commit();
            meter.stats
        };
        assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
        for stop in 1..=calls {
            let mut state = build(&baseline);
            let before = build(&baseline);
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop {
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
                assert!(matches!(
                    state.prepare(&change, 3, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                ));
            }
            assert_eq!(seen, stop);
            assert_eq!(state, before);
        }
        for (work, scratch, count, error) in [
            (stats.work_units, stats.scratch_entries, 3, None),
            (
                stats.work_units - 1,
                stats.scratch_entries,
                3,
                Some(StandingQueryFailure::WorkBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries - 1,
                3,
                Some(StandingQueryFailure::ScratchBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries,
                2,
                Some(StandingQueryFailure::ResultBudget),
            ),
        ] {
            let mut state = build(&baseline);
            let before = build(&baseline);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(100_000, count, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let result = state.prepare(&change, 3, &mut meter).map(Update::commit);
            assert_eq!(result, error.map_or(Ok(()), Err));
            if error.is_some() {
                assert_eq!(state, before);
            } else {
                assert_eq!(state, success);
            }
        }
        // Preserve a previously accepted nonempty derivative too, not only None.
        let before_rows = decoded(success.rows());
        let before_delta = decoded(success.delta().unwrap());
        let reverse = pairs(&[(2, 1, -1), (3, 1, -1), (2, 2, 1), (3, 3, 1)]);
        {
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            drop(success.prepare(&reverse, 3, &mut meter).unwrap());
        }
        assert_eq!(decoded(success.rows()), before_rows);
        assert_eq!(decoded(success.delta().unwrap()), before_delta);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut checkpoint = || panic!("injected projection unwind");
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let _ = success.prepare(&reverse, 3, &mut meter);
        }));
        assert!(unwound.is_err());
        assert_eq!(decoded(success.rows()), before_rows);
        assert_eq!(decoded(success.delta().unwrap()), before_delta);
    }
}
