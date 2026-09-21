//! Select existing exact kernels; keep one transactional native-row sink.

use super::*;
use fgdb_delta_types::zset::incremental::presence::{
    BagInput, BagJoinError, IncrementalLeftJoin, IncrementalPresence, LeftJoinUpdate, PresenceMode,
    PresenceUpdate,
};
use fgdb_delta_types::zset::incremental::{IncrementalJoin, JoinUpdate};
use fgdb_types::CanonicalScalar;

#[derive(PartialEq, Eq)]
pub(super) enum Input {
    Inner(IncrementalJoin<Key, Row, Row>),
    Left(IncrementalLeftJoin<Key, Row, Row>),
    // The kernel's left input is the declared RIGHT operand. Projection and
    // diagnostics restore the declared operand order at this boundary.
    Right(IncrementalLeftJoin<Key, Row, Row>),
    Full {
        left_outer: IncrementalLeftJoin<Key, Row, Row>,
        // FULL = LEFT OUTER + (RIGHT ANTI LEFT), not two outer products.
        // The second arm retains only unmatched right rows, so matched pairs
        // appear once and the witness test never expands their multiplicity.
        right_anti: IncrementalPresence<Key, Row>,
    },
    Presence {
        operator: IncrementalPresence<Key, Row>,
        // Count projection alone cannot validate individual right retractions:
        // a negative row could hide behind a positive sibling at the same key.
        // Retain compressed rows, sharing their admitted Arc payloads.
        right: Arranged,
    },
}

fn bag_error<E>(error: BagJoinError<E>) -> RowJoinError<E> {
    match error {
        BagJoinError::ZSet(error) => RowJoinError::Delta(error),
        BagJoinError::NegativeMultiplicity { input } => RowJoinError::NegativeMultiplicity {
            side: match input {
                BagInput::Left => 0,
                BagInput::Right => 1,
            },
        },
    }
}

fn reversed_bag_error<E>(error: BagJoinError<E>) -> RowJoinError<E> {
    match bag_error(error) {
        RowJoinError::NegativeMultiplicity { side } => {
            RowJoinError::NegativeMultiplicity { side: 1 - side }
        }
        error => error,
    }
}

impl Input {
    pub(super) fn new(kind: RowJoinKind) -> Self {
        match kind {
            RowJoinKind::Inner => Self::Inner(IncrementalJoin::new()),
            RowJoinKind::Left => Self::Left(IncrementalLeftJoin::new()),
            RowJoinKind::Right => Self::Right(IncrementalLeftJoin::new()),
            RowJoinKind::Full => Self::Full {
                left_outer: IncrementalLeftJoin::new(),
                right_anti: IncrementalPresence::new(PresenceMode::NotExists),
            },
            RowJoinKind::Semi | RowJoinKind::Anti => Self::Presence {
                operator: IncrementalPresence::new(if kind == RowJoinKind::Semi {
                    PresenceMode::Exists
                } else {
                    PresenceMode::NotExists
                }),
                right: ZSet::new(),
            },
        }
    }

    pub(super) fn left_weight(&self, key: &Key, row: &Row) -> Option<&ZWeight> {
        match self {
            Self::Inner(input) => input.left_weight(key, row),
            Self::Left(input) => input.left_weight(key, row),
            Self::Right(input) => input.right_weight(key, row),
            Self::Full { left_outer, .. } => left_outer.left_weight(key, row),
            Self::Presence { operator, .. } => operator.left_weight(key, row),
        }
    }

    pub(super) fn right_weight(&self, key: &Key, row: &Row) -> Option<&ZWeight> {
        match self {
            Self::Inner(input) => input.right_weight(key, row),
            Self::Left(input) => input.right_weight(key, row),
            Self::Right(input) => input.left_weight(key, row),
            Self::Full { left_outer, .. } => left_outer.right_weight(key, row),
            Self::Presence { right, .. } => right.weight(&(key.clone(), row.clone())),
        }
    }

    pub(super) fn prepare<E>(
        &mut self,
        left: &Arranged,
        right: &Arranged,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<InputUpdate<'_>, RowJoinError<E>> {
        match self {
            Self::Inner(input) => Ok(InputUpdate::Inner(
                input.prepare(left, right, limbs, control)?,
            )),
            Self::Left(input) => Ok(InputUpdate::Left(
                input
                    .prepare(left, right, limbs, control)
                    .map_err(bag_error)?,
            )),
            Self::Right(input) => Ok(InputUpdate::Right(
                input
                    .prepare(right, left, limbs, control)
                    .map_err(reversed_bag_error)?,
            )),
            Self::Full {
                left_outer,
                right_anti,
            } => {
                let witnesses = left.map(|(key, _)| Ok(key.clone()), limbs, control)?;
                let left_outer = left_outer
                    .prepare(left, right, limbs, control)
                    .map_err(bag_error)?;
                // Both guards remain tentative. A refusal in the second arm
                // drops the first; no callback separates their final commits.
                let right_anti = right_anti
                    .prepare(right, &witnesses, limbs, control)
                    .map_err(reversed_bag_error)?;
                Ok(InputUpdate::Full {
                    left_outer,
                    right_anti,
                })
            }
            Self::Presence {
                operator,
                right: retained,
            } => {
                // Signed projection retains the simultaneous-update cross term.
                // Do not threshold each delta or enumerate any matched products.
                let witnesses = right.map(|(key, _)| Ok(key.clone()), limbs, control)?;
                let rows = retained.prepare_update(right, limbs, control)?;
                let update = operator
                    .prepare(left, &witnesses, limbs, control)
                    .map_err(bag_error)?;
                Ok(InputUpdate::Presence {
                    update,
                    right: rows,
                })
            }
        }
    }
}

pub(super) enum InputUpdate<'a> {
    Inner(JoinUpdate<'a, Key, Row, Row>),
    Left(LeftJoinUpdate<'a, Key, Row, Row>),
    Right(LeftJoinUpdate<'a, Key, Row, Row>),
    Full {
        left_outer: LeftJoinUpdate<'a, Key, Row, Row>,
        right_anti: PresenceUpdate<'a, Key, Row>,
    },
    Presence {
        update: PresenceUpdate<'a, Key, Row>,
        right: ZSetUpdate<'a, (Key, Row)>,
    },
}

impl InputUpdate<'_> {
    pub(super) fn project_delta<E>(
        &self,
        spec: &RowJoinSpec,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<GraphValueRow>, RowJoinError<E>> {
        let mut updates = Vec::new();
        match self {
            Self::Inner(input) => {
                for ((_, left, right), weight) in input.delta().iter() {
                    append(
                        &mut updates,
                        spec,
                        Some(left),
                        Some(right),
                        weight,
                        limbs,
                        control,
                    )?;
                }
            }
            Self::Left(input) | Self::Full { left_outer: input, .. } => {
                for ((_, left, right), weight) in input.delta().iter() {
                    append(
                        &mut updates,
                        spec,
                        Some(left),
                        right.as_ref(),
                        weight,
                        limbs,
                        control,
                    )?;
                }
            }
            Self::Right(input) => {
                for ((_, right, left), weight) in input.delta().iter() {
                    append(
                        &mut updates,
                        spec,
                        left.as_ref(),
                        Some(right),
                        weight,
                        limbs,
                        control,
                    )?;
                }
            }
            Self::Presence { update, .. } => {
                for ((_, left), weight) in update.delta().iter() {
                    append(&mut updates, spec, Some(left), None, weight, limbs, control)?;
                }
            }
        }
        if let Self::Full { right_anti, .. } = self {
            for ((_, right), weight) in right_anti.delta().iter() {
                append(&mut updates, spec, None, Some(right), weight, limbs, control)?;
            }
        }
        // Null-extended payloads from different arms can be identical (including
        // zero-column cross joins), so consolidate before output admission.
        Ok(ZSet::from_updates(updates, limbs, control)?)
    }

    pub(super) fn commit(self) {
        match self {
            Self::Inner(input) => {
                let _ = input.commit();
            }
            Self::Left(input) | Self::Right(input) => {
                let _ = input.commit();
            }
            Self::Full {
                left_outer,
                right_anti,
            } => {
                let _ = left_outer.commit();
                let _ = right_anti.commit();
            }
            Self::Presence { update, right } => {
                let _ = update.commit();
                right.commit();
            }
        }
    }
}

fn append_columns<E>(
    values: &mut Vec<GraphValue>,
    row: Option<&Row>,
    width: usize,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    if let Some(row) = row {
        for value in row.values() {
            reserve_cell(value, control)?;
            values.push(value.clone());
        }
    } else {
        // Every native column domain is nullable. Preserve the declared schema
        // width and never invent a zero vertex ID for an absent match.
        for _ in 0..width {
            let value = GraphValue::Scalar(CanonicalScalar::Null);
            reserve_cell(&value, control)?;
            values.push(value);
        }
    }
    Ok(())
}

fn append<E>(
    updates: &mut Vec<(GraphValueRow, ZWeight)>,
    spec: &RowJoinSpec,
    left: Option<&Row>,
    right: Option<&Row>,
    weight: &ZWeight,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::Work)?;
    charge(control, ZSetEvent::ScratchEntry)?;
    let mut values = Vec::with_capacity(spec.width());
    append_columns(&mut values, left, spec.left.len(), control)?;
    if spec.kind.includes_right() {
        append_columns(&mut values, right, spec.right.len(), control)?;
    }
    let row = GraphValueRow::from_owned_values(values);
    charge(control, ZSetEvent::Work)?;
    let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
    charge(control, ZSetEvent::ScratchEntry)?;
    updates.push((row, weight));
    Ok(())
}
