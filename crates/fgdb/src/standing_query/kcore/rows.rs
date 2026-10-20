//! Checked native rows for an accepted core-number derivative. This optional
//! sink owns no topology and is allocated only by native query registration.

use super::*;
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_gql::algebra::GraphValue;
use fgdb_types::CanonicalScalar;

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

    pub(super) fn from_numbers(
        numbers: &ZSet<(VId, u64)>,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        meter.units(ZSetEvent::ScratchEntry, 8)?;
        let mut state = Self {
            columns: ["vertex".into(), "core".into()],
            rows: ZSet::new(),
            last_delta: None,
        };
        state.prepare(numbers, numbers.len(), meter)?.commit();
        // A baseline is not a successor, including at a nonzero frontier.
        state.last_delta = None;
        Ok(state)
    }

    pub(super) fn prepare(
        &mut self,
        delta: &ZSet<(VId, u64)>,
        vertex_count: usize,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        components::result_bound(vertex_count, meter.policy)?;
        let mut output = ZSet::new();
        let mut inserted = 0_u128;
        let mut removed = 0_u128;
        for ((vertex, number), weight) in delta.iter() {
            meter.charge(ZSetEvent::Work)?;
            // Match ordinary CALL's integer domain, without narrowing a core
            // count or converting a full-width vertex identity into a scalar.
            let number = i64::try_from(*number).map_err(|_| StandingQueryFailure::Arithmetic)?;
            meter.units(ZSetEvent::ScratchEntry, 3)?;
            let row = GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(*vertex),
                GraphValue::Scalar(CanonicalScalar::Int(number)),
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
        // Shell changes replace rows. Bound the completed live-vertex set,
        // never an insertion-before-retraction prefix of the derivative.
        let next_count = (self.rows.len() as u128)
            .checked_sub(removed)
            .and_then(|count| count.checked_add(inserted))
            .ok_or(StandingQueryFailure::InvalidDelta)?;
        if next_count != vertex_count as u128 {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        for _ in output.iter() {
            meter.charge(ZSetEvent::Work)?;
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

#[must_use = "dropping a core-row update preserves rows and the previous derivative"]
pub(super) struct Update<'a> {
    sink: ZSetUpdate<'a, GraphValueRow>,
    output: ZSet<GraphValueRow>,
    last_delta: &'a mut Option<ZSet<GraphValueRow>>,
}

impl Update<'_> {
    pub(super) fn commit(self) {
        self.sink.commit();
        // Unchanged topology still advances a circuit through the same tick.
        *self.last_delta = Some(self.output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000)
    }

    fn numbers(values: &[(u128, u64, i128)]) -> ZSet<(VId, u64)> {
        ZSet::from_updates(
            values
                .iter()
                .map(|&(vertex, core, weight)| ((VId(vertex), core), ZWeight::from_i128(weight))),
            LIMBS,
            &mut |_| Ok::<_, StandingQueryFailure>(()),
        )
        .unwrap()
    }

    fn build(values: &[(u128, u64, i128)]) -> State {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        State::from_numbers(&numbers(values), &mut meter).unwrap()
    }

    #[test]
    fn full_width_ids_and_checked_counts_preserve_atomic_native_rows() {
        let initial = [(0, 3, 1), (u128::MAX, i64::MAX as u64, 1)];
        let before = build(&initial);
        let mut state = build(&initial);
        assert_eq!(state.columns(), ["vertex", "core"]);
        assert!(state.delta().is_none());
        for (delta, error) in [
            (vec![(0, u64::MAX, 1)], StandingQueryFailure::Arithmetic),
            (vec![(0, 3, 1)], StandingQueryFailure::InvalidDelta),
            (vec![(1, 0, -1)], StandingQueryFailure::InvalidDelta),
            (vec![(0, 3, -2)], StandingQueryFailure::InvalidDelta),
        ] {
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state
                    .prepare(&numbers(&delta), 2, &mut meter)
                    .map(Update::commit),
                Err(error),
            );
            assert_eq!(state, before);
        }

        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        // Lower-sorting replacements are admitted under the exact final quota.
        state
            .prepare(
                &numbers(&[
                    (0, 3, -1),
                    (0, 2, 1),
                    (u128::MAX, i64::MAX as u64, -1),
                    (u128::MAX, 0, 1),
                ]),
                2,
                &mut meter,
            )
            .unwrap()
            .commit();
        let expected = build(&[(0, 2, 1), (u128::MAX, 0, 1)]);
        assert_eq!(state.rows(), expected.rows());
        assert_eq!(state.delta().unwrap().len(), 4);
        state.prepare(&ZSet::new(), 2, &mut meter).unwrap().commit();
        assert!(state.delta().unwrap().is_empty());
        assert!(expected.delta().is_none());
    }
}
