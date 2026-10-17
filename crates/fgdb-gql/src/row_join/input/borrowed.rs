//! Borrowed row access for physical join hosts. Keys use the same canonical
//! equality as arranged keys; ON and output use the existing native kernels.
//! No source, arrangement, weight, witness set or output bag is retained here.

use super::*;

fn event(event: ZSetEvent) -> GlaExecutionEvent {
    match event {
        ZSetEvent::Work => GlaExecutionEvent::Work,
        ZSetEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
    }
}

impl RowJoinSpec {
    /// Validate one complete input row before retaining or matching it. Side
    /// zero is left and one is right. A physical host must validate EVERY input,
    /// even when the opposite relation is empty or its predicate is FALSE.
    /// Control failures retain their original value in Delta(Control(..)).
    pub fn validate_input_with_control<E>(
        &self,
        side: usize,
        row: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), RowJoinError<E>> {
        control(GlaExecutionEvent::Work).map_err(ZSetError::Control)?;
        let schema = match side {
            0 => self.left_types(),
            1 => self.right_types(),
            _ => return Err(RowJoinError::InputSchema { side }),
        };
        if row.len() != schema.len() {
            return Err(RowJoinError::InputSchema { side });
        }
        for (value, kind) in row.values().iter().zip(schema) {
            control(GlaExecutionEvent::Work).map_err(ZSetError::Control)?;
            if !kind.accepts(value) || !value.validate_bounds() {
                return Err(RowJoinError::InputSchema { side });
            }
        }
        Ok(())
    }

    /// Compare the positional equality-key tuples without allocating them.
    /// This is ascending canonical typed order, with NULL first, suitable for
    /// merging inputs sorted by their respective key columns. Payload columns
    /// do not break ties. Both complete schemas and variable-payload work are
    /// admitted before inspecting key values, as in `test_pair_with_control`.
    ///
    /// Equal is an ORDERING result, not a witness: two NULL keys compare equal
    /// but never match. The host must still use `test_pair_with_control` before
    /// accepting a pair. This method never evaluates a residual ON predicate;
    /// a definition without keys has two equal empty tuples.
    pub fn compare_keys_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<core::cmp::Ordering, RowJoinError<E>> {
        self.validate_input_with_control(0, left, control)?;
        self.validate_input_with_control(1, right, control)?;
        for &(l, r) in self.keys() {
            control(GlaExecutionEvent::Work).map_err(ZSetError::Control)?;
            let (left, right) = (&left.values()[l], &right.values()[r]);
            for value in [left, right] {
                for _ in 0..value.payload_units() {
                    control(GlaExecutionEvent::Work).map_err(ZSetError::Control)?;
                }
            }
            let order = left.cmp(right);
            if order != core::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(core::cmp::Ordering::Equal)
    }

    /// Test one admitted candidate pair without constructing a joined row or
    /// key tuple. Both row schemas are checked, then exact typed equality keys
    /// exclude NULL on either side. Equality is canonical key identity, not
    /// numeric coercion or the SQL comparison law used by the residual ON.
    /// The SAME eager three-valued ON kernel as incremental joins runs only
    /// after all keys match; only TRUE is a witness. Cross keys impose nothing.
    /// Variable payload comparison work is admitted before comparing cells.
    /// This method does not establish absence, multiplicity, or result order.
    pub fn test_pair_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, RowJoinError<E>> {
        self.validate_input_with_control(0, left, control)?;
        self.validate_input_with_control(1, right, control)?;
        for &(l, r) in self.keys() {
            control(GlaExecutionEvent::Work).map_err(ZSetError::Control)?;
            let (left, right) = (&left.values()[l], &right.values()[r]);
            for value in [left, right] {
                for _ in 0..value.payload_units() {
                    control(GlaExecutionEvent::Work).map_err(ZSetError::Control)?;
                }
            }
            if left.is_null() || right.is_null() || left != right {
                return Ok(false);
            }
        }
        Ok(self.matches(left, right, &mut |value| control(event(value)))?)
    }

    /// Copy one selected occurrence through the SAME null-extension/projector
    /// used by IncrementalRowJoin. Inner needs both sides; outer kinds may omit
    /// only their nonpreserved side; full may omit either one. Semi/anti take
    /// only a left occurrence. A missing side is NOT a matched all-null row.
    ///
    /// The host proves match/absence before calling, and admits memory through
    /// ScratchEntry before each allocation. Keep that reservation with the
    /// returned row across encoding and awaited writes. No weight expansion,
    /// ResultRow charge, predicate re-evaluation, or ordering occurs here.
    pub fn project_output_with_control<E>(
        &self,
        left: Option<&GraphValueRow>,
        right: Option<&GraphValueRow>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, RowJoinError<E>> {
        control(GlaExecutionEvent::Work).map_err(ZSetError::Control)?;
        let valid = match self.kind() {
            RowJoinKind::Inner => left.is_some() && right.is_some(),
            RowJoinKind::Left => left.is_some(),
            RowJoinKind::Right => right.is_some(),
            RowJoinKind::Full => left.is_some() || right.is_some(),
            RowJoinKind::Semi | RowJoinKind::Anti => left.is_some() && right.is_none(),
        };
        if !valid {
            return Err(RowJoinError::InvalidResult);
        }
        for (side, row) in [(0, left), (1, right)] {
            if let Some(row) = row {
                self.validate_input_with_control(side, row, control)?;
            }
        }
        Ok(project(self, left, right, &mut |value| {
            control(event(value))
        })?)
    }
}

// Extracted from input::append without changing that path's event sequence.
// Native weighted maintenance and external occurrence hosts share one copier.
pub(super) fn project<E>(
    spec: &RowJoinSpec,
    left: Option<&GraphValueRow>,
    right: Option<&GraphValueRow>,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<GraphValueRow, ZSetError<E>> {
    charge(control, ZSetEvent::Work)?;
    charge(control, ZSetEvent::ScratchEntry)?;
    let mut values = Vec::with_capacity(spec.width());
    append_columns(&mut values, left, spec.left.len(), control)?;
    if spec.kind.includes_right() {
        append_columns(&mut values, right, spec.right.len(), control)?;
    }
    Ok(GraphValueRow::from_owned_values(values))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod key_order_tests {
    use super::*;
    use core::cmp::Ordering;

    fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
        Ok(())
    }

    fn row(values: &[Option<i64>]) -> GraphValueRow {
        GraphValueRow::from_owned_values(
            values
                .iter()
                .map(|value| {
                    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
                })
                .collect(),
        )
    }

    #[test]
    fn composite_key_order_uses_each_sides_positions_not_payload_or_row_order() {
        let types = [GraphSetColumnType::Scalar; 3];
        let spec = RowJoinSpec::new(&types, &types, &[(1, 2), (0, 0), (1, 2)]).unwrap();
        let left = row(&[Some(2), Some(5), Some(999)]);
        let equal = row(&[Some(2), Some(-999), Some(5)]);
        let greater = row(&[Some(3), Some(-999), Some(5)]);
        assert_eq!(
            spec.compare_keys_with_control(&left, &equal, &mut allow)
                .unwrap(),
            Ordering::Equal
        );
        assert!(
            spec.test_pair_with_control(&left, &equal, &mut allow)
                .unwrap()
        );
        assert_eq!(
            spec.compare_keys_with_control(&left, &greater, &mut allow)
                .unwrap(),
            Ordering::Less
        );
    }

    #[test]
    fn null_key_order_equality_never_becomes_a_join_witness() {
        let types = [GraphSetColumnType::Scalar; 2];
        let spec = RowJoinSpec::new(&types, &types, &[(0, 0), (1, 1)]).unwrap();
        let null = row(&[Some(1), None]);
        let nonnull = row(&[Some(1), Some(0)]);
        assert_eq!(
            spec.compare_keys_with_control(&null, &null, &mut allow)
                .unwrap(),
            Ordering::Equal
        );
        assert!(
            !spec
                .test_pair_with_control(&null, &null, &mut allow)
                .unwrap()
        );
        assert_eq!(
            spec.compare_keys_with_control(&null, &nonnull, &mut allow)
                .unwrap(),
            Ordering::Less
        );
        assert_eq!(
            spec.compare_keys_with_control(&nonnull, &null, &mut allow)
                .unwrap(),
            Ordering::Greater
        );
    }

    #[test]
    fn key_comparison_validates_even_nonkey_payload_schema() {
        let types = [GraphSetColumnType::Scalar; 2];
        let spec = RowJoinSpec::new(&types, &types, &[(0, 0)]).unwrap();
        let invalid = GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(1)),
            GraphValue::Vertex(fgdb_types::VId(9)),
        ]);
        let valid = row(&[Some(1), Some(2)]);
        assert!(matches!(
            spec.compare_keys_with_control(&invalid, &valid, &mut allow),
            Err(RowJoinError::InputSchema { side: 0 })
        ));
        assert!(matches!(
            spec.compare_keys_with_control(&valid, &invalid, &mut allow),
            Err(RowJoinError::InputSchema { side: 1 })
        ));
    }

    #[test]
    fn key_comparison_propagates_every_control_refusal_before_payload_comparison() {
        let types = [GraphSetColumnType::Scalar];
        let spec = RowJoinSpec::new(&types, &types, &[(0, 0)]).unwrap();
        let bytes = |value| {
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::bytes(vec![value; 4096]).unwrap(),
            )])
        };
        let (left, right) = (bytes(7), bytes(8));
        let mut events = 0;
        assert_eq!(
            spec.compare_keys_with_control(&left, &right, &mut |_| {
                events += 1;
                Ok::<_, usize>(())
            })
            .unwrap(),
            Ordering::Less
        );
        assert!(events > 5, "variable key payload must spend work");
        for stop in 0..events {
            let mut visited = 0;
            let result = spec.compare_keys_with_control(&left, &right, &mut |_| {
                let current = visited;
                visited += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(
                result,
                Err(RowJoinError::Delta(ZSetError::Control(value))) if value == stop
            ));
            assert_eq!(visited, stop + 1);
        }
    }
}
