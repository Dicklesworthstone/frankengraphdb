//! Select existing exact kernels; keep one transactional native-row sink.

use super::*;
use fgdb_delta_types::zset::incremental::{IncrementalJoin, JoinUpdate};
use fgdb_delta_types::zset::incremental::presence::{
    BagInput, BagJoinError, IncrementalLeftJoin, IncrementalPresence, LeftJoinUpdate,
    PresenceMode, PresenceUpdate,
};
use fgdb_types::CanonicalScalar;

#[derive(PartialEq, Eq)]
pub(super) enum Input {
    Inner(IncrementalJoin<Key, Row, Row>),
    Left(IncrementalLeftJoin<Key, Row, Row>),
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
            side: match input { BagInput::Left => 0, BagInput::Right => 1 },
        },
    }
}

impl Input {
    pub(super) fn new(kind: RowJoinKind) -> Self {
        match kind {
            RowJoinKind::Inner => Self::Inner(IncrementalJoin::new()),
            RowJoinKind::Left => Self::Left(IncrementalLeftJoin::new()),
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
            Self::Presence { operator, .. } => operator.left_weight(key, row),
        }
    }

    pub(super) fn right_weight(&self, key: &Key, row: &Row) -> Option<&ZWeight> {
        match self {
            Self::Inner(input) => input.right_weight(key, row),
            Self::Left(input) => input.right_weight(key, row),
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
            Self::Inner(input) => Ok(InputUpdate::Inner(input.prepare(left, right, limbs, control)?)),
            Self::Left(input) => Ok(InputUpdate::Left(
                input.prepare(left, right, limbs, control).map_err(bag_error)?,
            )),
            Self::Presence { operator, right: retained } => {
                // Signed projection retains the simultaneous-update cross term.
                // Do not threshold each delta or enumerate any matched products.
                let witnesses = right.map(|(key, _)| Ok(key.clone()), limbs, control)?;
                let rows = retained.prepare_update(right, limbs, control)?;
                let update = operator.prepare(left, &witnesses, limbs, control).map_err(bag_error)?;
                Ok(InputUpdate::Presence { update, right: rows })
            }
        }
    }
}

pub(super) enum InputUpdate<'a> {
    Inner(JoinUpdate<'a, Key, Row, Row>),
    Left(LeftJoinUpdate<'a, Key, Row, Row>),
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
                    append(&mut updates, spec, left, Some(right), weight, limbs, control)?;
                }
            }
            Self::Left(input) => {
                for ((_, left, right), weight) in input.delta().iter() {
                    append(&mut updates, spec, left, right.as_ref(), weight, limbs, control)?;
                }
            }
            Self::Presence { update, .. } => {
                for ((_, left), weight) in update.delta().iter() {
                    append(&mut updates, spec, left, None, weight, limbs, control)?;
                }
            }
        }
        Ok(ZSet::from_updates(updates, limbs, control)?)
    }

    pub(super) fn commit(self) {
        match self {
            Self::Inner(input) => { let _ = input.commit(); }
            Self::Left(input) => { let _ = input.commit(); }
            Self::Presence { update, right } => {
                let _ = update.commit();
                right.commit();
            }
        }
    }
}

fn append<E>(
    updates: &mut Vec<(GraphValueRow, ZWeight)>,
    spec: &RowJoinSpec,
    left: &Row,
    right: Option<&Row>,
    weight: &ZWeight,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::Work)?;
    charge(control, ZSetEvent::ScratchEntry)?;
    let mut values = Vec::with_capacity(spec.width());
    for value in left.values() {
        reserve_cell(value, control)?;
        values.push(value.clone());
    }
    if spec.kind.includes_right() {
        if let Some(right) = right {
            for value in right.values() {
                reserve_cell(value, control)?;
                values.push(value.clone());
            }
        } else {
            // NULL is a value in both admitted column domains. Never invent a
            // zero vertex ID or confuse an absent match with a null payload.
            for _ in spec.right.iter() {
                let value = GraphValue::Scalar(CanonicalScalar::Null);
                reserve_cell(&value, control)?;
                values.push(value);
            }
        }
    }
    let row = GraphValueRow::from_owned_values(values);
    charge(control, ZSetEvent::Work)?;
    let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
    charge(control, ZSetEvent::ScratchEntry)?;
    updates.push((row, weight));
    Ok(())
}
