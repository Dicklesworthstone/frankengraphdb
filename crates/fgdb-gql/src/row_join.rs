//! Typed incremental equality, theta and outer/presence joins of exact row bags.
//!
//! This is the row/schema adapter for the existing Z-set join derivative, not
//! a second matcher, parser, scheduler or graph store. Equality uses canonical
//! scalar/vertex domains without coercion; any NULL key makes a row unmatchable.
//! Input and output multiplicities stay exact and compressed. Only changed key
//! groups are probed, including the simultaneous-input cross term.

use crate::{GlaExecutionEvent, GraphSetColumnType, GraphSetFilterError, GraphSetPredicateOp};
use crate::algebra::{GraphValue, GraphValueRow, MAX_PATTERN_VERTICES};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};
use std::sync::Arc;

mod input;
use input::{Input, InputUpdate};

// Side-specific NULL domains retain and validate unmatched input counts without
// ever joining two NULLs. An ordinary key always has tag zero.
type Key = (u8, Arc<[GraphValue]>);
type Row = Arc<GraphValueRow>;
type Arranged = ZSet<(Key, Row)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowJoinBuildError {
    EmptyInput,
    TooManyColumns { observed: usize },
    EmptyKeys,
    TooManyKeys,
    KeyColumn { side: usize, column: usize },
    KeyType { key: usize },
    UnsupportedColumn { side: usize, column: usize },
    Predicate(GraphSetFilterError),
}
impl core::fmt::Display for RowJoinBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "row join definition: {self:?}")
    }
}
impl core::error::Error for RowJoinBuildError {}

/// Fixed relational semantics, independent of a tick's signed changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowJoinKind {
    Inner,
    /// All matches, or one null extension per left occurrence when unmatched.
    Left,
    /// All matches, or one null extension per right occurrence when unmatched.
    /// Output columns remain left followed by right, never swapped.
    Right,
    /// All matches plus null extensions for unmatched occurrences on BOTH sides.
    Full,
    /// Preserve left multiplicity when at least one matching right row exists.
    Semi,
    /// Preserve left multiplicity when no matching right row exists.
    Anti,
}
impl RowJoinKind {
    fn includes_right(self) -> bool {
        matches!(self, Self::Inner | Self::Left | Self::Right | Self::Full)
    }
}

/// Immutable positional schema. Inner/outer output concatenates both inputs;
/// semi/anti output contains only the left columns. `new` requires equality
/// keys; `cross` chooses an unrestricted candidate domain. `with_predicate`
/// adds a checked ON predicate to either, for every join kind. Keys are scalar or vertex
/// columns; non-key payloads may use any bounded native value domain.
#[derive(Clone, PartialEq, Eq)]
pub struct RowJoinSpec {
    left: Box<[GraphSetColumnType]>,
    right: Box<[GraphSetColumnType]>,
    keys: Box<[(usize, usize)]>,
    kind: RowJoinKind,
    predicate: Option<Box<[GraphSetPredicateOp]>>,
}
impl RowJoinSpec {
    pub fn new(
        left: &[GraphSetColumnType],
        right: &[GraphSetColumnType],
        keys: &[(usize, usize)],
    ) -> Result<Self, RowJoinBuildError> {
        if left.is_empty() || right.is_empty() {
            return Err(RowJoinBuildError::EmptyInput);
        }
        let width = left.len().saturating_add(right.len());
        if width > MAX_PATTERN_VERTICES {
            return Err(RowJoinBuildError::TooManyColumns { observed: width });
        }
        if keys.is_empty() {
            return Err(RowJoinBuildError::EmptyKeys);
        }
        if keys.len() > MAX_PATTERN_VERTICES {
            return Err(RowJoinBuildError::TooManyKeys);
        }
        for (side, columns) in [left, right].into_iter().enumerate() {
            for (column, kind) in columns.iter().enumerate() {
                // Only key cells participate in equality. Rejecting a list,
                // path or captured edge elsewhere in the row prevents joins
                // from composing with otherwise valid graph projections.
                // arrange() still checks EVERY payload's schema and bounds.
                let is_key = keys
                    .iter()
                    .any(|&(l, r)| column == if side == 0 { l } else { r });
                if is_key
                    && !matches!(
                        kind,
                        GraphSetColumnType::Scalar | GraphSetColumnType::Vertex
                    )
                {
                    return Err(RowJoinBuildError::UnsupportedColumn { side, column });
                }
            }
        }
        for (key, &(l, r)) in keys.iter().enumerate() {
            let l = left
                .get(l)
                .ok_or(RowJoinBuildError::KeyColumn { side: 0, column: l })?;
            let r = right
                .get(r)
                .ok_or(RowJoinBuildError::KeyColumn { side: 1, column: r })?;
            if l != r {
                return Err(RowJoinBuildError::KeyType { key });
            }
        }
        Ok(Self {
            left: left.into(),
            right: right.into(),
            keys: keys.into(),
            kind: RowJoinKind::Inner,
            predicate: None,
        })
    }
    /// An unconditional Cartesian product, using the existing exact join
    /// derivative with one shared empty key. NULL is ordinary payload here:
    /// it never suppresses a pair. All bounded native value domains are valid
    /// because no payload is interpreted as an equality key. A zero-column
    /// relation is valid; a unit tuple multiplies counts, not column widths.
    ///
    /// The default kind is Inner. Every occurrence on the opposite side is a
    /// witness regardless of its values, including for outer and presence
    /// joins. Source counts are still checked
    /// when the opposite bag is empty. Products can be quadratic in support;
    /// this constructor promises neither a selective index nor spill.
    pub fn cross(
        left: &[GraphSetColumnType],
        right: &[GraphSetColumnType],
    ) -> Result<Self, RowJoinBuildError> {
        let width = left.len().saturating_add(right.len());
        if width > MAX_PATTERN_VERTICES {
            return Err(RowJoinBuildError::TooManyColumns { observed: width });
        }
        Ok(Self {
            left: left.into(),
            right: right.into(),
            keys: Box::new([]),
            kind: RowJoinKind::Inner,
            predicate: None,
        })
    }
    /// True only for the explicitly constructed unconditional definition.
    pub fn is_cross(&self) -> bool {
        self.keys.is_empty() && self.predicate.is_none()
    }

    /// Bind a fixed ON predicate over LEFT columns followed by RIGHT columns.
    /// This full input schema also applies to semi/anti joins, whose output is
    /// left-only. Equality keys, when present, remain an additional condition.
    /// `cross(...).with_predicate(...)` therefore expresses a theta join.
    /// Only TRUE matches; FALSE and UNKNOWN do not create a witness. The
    /// ordinary eager row-predicate IR owns comparison, NULL and Boolean rules.
    /// Null extension happens AFTER matching, never before predicate evaluation.
    ///
    /// Definitions are frozen into the operator. This does not add ON text
    /// parsing, a selective range index, authorization or external-memory spill.
    pub fn with_predicate(
        mut self,
        code: &[GraphSetPredicateOp],
    ) -> Result<Self, RowJoinBuildError> {
        let types: Vec<_> = self.left.iter().chain(self.right.iter()).copied().collect();
        GraphSetPredicateOp::validate_schema(&types, code)
            .map_err(RowJoinBuildError::Predicate)?;
        self.predicate = Some(code.to_vec().into_boxed_slice());
        Ok(self)
    }

    pub fn predicate(&self) -> Option<&[GraphSetPredicateOp]> {
        self.predicate.as_deref()
    }

    fn matches<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<bool, ZSetError<E>> {
        let Some(code) = self.predicate() else {
            return Ok(true);
        };
        GraphSetPredicateOp::evaluate_pair_with_control(code, left, right, &mut |event| {
            charge(control, match event {
                GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                _ => ZSetEvent::Work,
            })
        })
    }

    /// Choose semantics before constructing the operator. Input schemas and
    /// key admission are identical for all kinds; a live operator cannot switch.
    pub fn with_kind(mut self, kind: RowJoinKind) -> Self {
        self.kind = kind;
        self
    }
    pub fn kind(&self) -> RowJoinKind {
        self.kind
    }
    pub fn left_types(&self) -> &[GraphSetColumnType] {
        &self.left
    }
    pub fn right_types(&self) -> &[GraphSetColumnType] {
        &self.right
    }
    pub fn keys(&self) -> &[(usize, usize)] {
        &self.keys
    }
    pub fn width(&self) -> usize {
        self.left.len()
            + if self.kind.includes_right() {
                self.right.len()
            } else {
                0
            }
    }
    pub fn column_types(&self) -> impl Iterator<Item = GraphSetColumnType> + '_ {
        let right = if self.kind.includes_right() {
            self.right.len()
        } else {
            0
        };
        self.left
            .iter()
            .chain(self.right.iter().take(right))
            .copied()
    }
}
impl core::fmt::Debug for RowJoinSpec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowJoinSpec")
            .field("kind", &self.kind)
            .field("columns", &self.width())
            .field("keys", &self.keys.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowJoinError<E> {
    Delta(ZSetError<E>),
    InputSchema { side: usize },
    NegativeMultiplicity { side: usize },
    ResultBudget { limit: u64 },
    InvalidResult,
}
impl<E> From<ZSetError<E>> for RowJoinError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for RowJoinError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::InputSchema { side } => {
                write!(f, "row join operand {side} has an incompatible row")
            }
            Self::NegativeMultiplicity { side } => {
                write!(f, "row join operand {side} has a negative final count")
            }
            Self::ResultBudget { limit } => write!(f, "row join final occurrences exceed {limit}"),
            Self::InvalidResult => f.write_str("row join result has a negative final count"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for RowJoinError<E> {}

fn charge<E>(
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    event: ZSetEvent,
) -> Result<(), ZSetError<E>> {
    control(event).map_err(ZSetError::Control)
}
fn reserve_cell<E>(
    value: &GraphValue,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::Work)?;
    // Bounds are checked before this reservation, including recursive payloads.
    for _ in 0..=value.payload_units() {
        charge(control, ZSetEvent::ScratchEntry)?;
    }
    Ok(())
}
fn reserve_row<E>(
    row: &GraphValueRow,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::ScratchEntry)?;
    for value in row.values() {
        reserve_cell(value, control)?;
    }
    Ok(())
}
fn arrange<E>(
    spec: &RowJoinSpec,
    side: usize,
    rows: &ZSet<GraphValueRow>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Arranged, RowJoinError<E>> {
    let schema = if side == 0 { &spec.left } else { &spec.right };
    let mut updates = Vec::new();
    for (row, weight) in rows.iter() {
        charge(control, ZSetEvent::Work)?;
        if row.len() != schema.len() {
            return Err(RowJoinError::InputSchema { side });
        }
        for (value, kind) in row.values().iter().zip(schema.iter()) {
            charge(control, ZSetEvent::Work)?;
            if !kind.accepts(value) || !value.validate_bounds() {
                return Err(RowJoinError::InputSchema { side });
            }
        }
        charge(control, ZSetEvent::ScratchEntry)?;
        let mut values = Vec::with_capacity(spec.keys.len());
        let mut has_null = false;
        for &(l, r) in &spec.keys {
            let value = &row.values()[if side == 0 { l } else { r }];
            has_null |= value.is_null();
            reserve_cell(value, control)?;
            values.push(value.clone());
        }
        let tag = if has_null {
            if side == 0 { 1 } else { 2 }
        } else {
            0
        };
        reserve_row(row, control)?;
        charge(control, ZSetEvent::ScratchEntry)?;
        let row = Arc::new(row.clone());
        charge(control, ZSetEvent::Work)?;
        let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
        charge(control, ZSetEvent::ScratchEntry)?;
        updates.push((((tag, Arc::from(values)), row), weight));
    }
    Ok(ZSet::from_updates(updates, limbs, control)?)
}

/// One exact in-memory row-join circuit. Work/scratch count logical events and
/// payload units, not allocator bytes or key-comparison costs. Arc-owned keys
/// and input rows avoid repeated payload cloning in the generic join products.
/// Inner/outer output can be quadratic. Semi/anti use counted witnesses without
/// producing that Cartesian bag. All kinds share work/scratch and final-row
/// admission; this is not spill or a worst-case-optimal multiway join.
/// With a residual predicate, matched pairs use the exact selected derivative;
/// outer/presence witnesses are recomputed per row within changed key groups.
/// That fallback can scan a group's Cartesian candidate domain but never
/// materializes matched products for semi/anti or visits unrelated groups.
#[derive(PartialEq, Eq)]
pub struct IncrementalRowJoin {
    spec: RowJoinSpec,
    input: Input,
    rows: ZSet<GraphValueRow>,
    total: ZWeight,
}
impl IncrementalRowJoin {
    pub fn new(spec: RowJoinSpec) -> Self {
        let input = Input::new(&spec);
        Self {
            spec,
            input,
            rows: ZSet::new(),
            total: ZWeight::ZERO,
        }
    }
    pub fn spec(&self) -> &RowJoinSpec {
        &self.spec
    }
    pub fn rows(&self) -> &ZSet<GraphValueRow> {
        &self.rows
    }
    pub fn total(&self) -> &ZWeight {
        &self.total
    }

    /// Prepare both complete derivatives against the OLD input arrangements.
    /// Equijoin NULL keys never match; cross definitions have no key columns.
    /// All rows remain retained for count validation. Negative
    /// final counts refuse even when the opposite side currently has no match.
    /// The output bound tests final occurrences, not support or a transient
    /// insertion-first prefix. Dropping the guard leaves all state unchanged.
    pub fn prepare<E>(
        &mut self,
        left: &ZSet<GraphValueRow>,
        right: &ZSet<GraphValueRow>,
        limbs: LimbLimit,
        max_result_rows: Option<u64>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<RowJoinUpdate<'_>, RowJoinError<E>> {
        charge(control, ZSetEvent::Work)?;
        let left = arrange(&self.spec, 0, left, limbs, control)?;
        let right = arrange(&self.spec, 1, right, limbs, control)?;
        for (side, rows) in [&left, &right].into_iter().enumerate() {
            for ((key, row), weight) in rows.iter() {
                charge(control, ZSetEvent::Work)?;
                let old = if side == 0 {
                    self.input.left_weight(key, row)
                } else {
                    self.input.right_weight(key, row)
                };
                let next = match old {
                    Some(old) => old.checked_add(weight, limbs),
                    None => weight.checked_clone(limbs),
                }
                .map_err(ZSetError::Arithmetic)?;
                if next < ZWeight::ZERO {
                    return Err(RowJoinError::NegativeMultiplicity { side });
                }
            }
        }
        let input = self.input.prepare(&self.spec, &left, &right, limbs, control)?;
        let delta = input.project_delta(&self.spec, limbs, control)?;
        let change = delta.total_weight(limbs, control)?;
        charge(control, ZSetEvent::Work)?;
        let next_total = self
            .total
            .checked_add(&change, limbs)
            .map_err(ZSetError::Arithmetic)?;
        if next_total < ZWeight::ZERO {
            return Err(RowJoinError::InvalidResult);
        }
        if let Some(limit) = max_result_rows {
            if next_total > ZWeight::from_i128(i128::from(limit)) {
                return Err(RowJoinError::ResultBudget { limit });
            }
        }
        for (row, _) in delta.iter() {
            reserve_row(row, control)?;
        }
        let sink = self.rows.prepare_update(&delta, limbs, control)?;
        for (row, _) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if sink
                .weight(row)
                .is_some_and(|weight| weight < &ZWeight::ZERO)
            {
                return Err(RowJoinError::InvalidResult);
            }
        }
        charge(control, ZSetEvent::Work)?;
        Ok(RowJoinUpdate {
            input,
            sink,
            total: &mut self.total,
            next_total,
            delta,
        })
    }
}
impl core::fmt::Debug for IncrementalRowJoin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalRowJoin")
            .field("spec", &self.spec)
            .field("support", &self.rows.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[must_use = "dropping a row join update preserves its input and output arrangements"]
pub struct RowJoinUpdate<'a> {
    input: InputUpdate<'a>,
    sink: ZSetUpdate<'a, GraphValueRow>,
    total: &'a mut ZWeight,
    next_total: ZWeight,
    delta: ZSet<GraphValueRow>,
}
impl RowJoinUpdate<'_> {
    pub fn delta(&self) -> &ZSet<GraphValueRow> {
        &self.delta
    }
    pub fn total(&self) -> &ZWeight {
        &self.next_total
    }
    /// Publish without recoverable callbacks between input, output and total.
    pub fn commit(self) -> ZSet<GraphValueRow> {
        let Self {
            input,
            sink,
            total,
            next_total,
            delta,
        } = self;
        input.commit();
        sink.commit();
        *total = next_total;
        delta
    }
}
impl core::fmt::Debug for RowJoinUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowJoinUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod mode_tests;

#[cfg(test)]
mod cross_tests;

#[cfg(test)]
mod outer_tests;

#[cfg(test)]
mod predicate_tests;

#[cfg(test)]
mod payload_tests {
    use super::*;
    use fgdb_types::{CanonicalScalar, VId};

    const LIMBS: LimbLimit = LimbLimit::new(16);

    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }

    fn bag(row: &GraphValueRow, count: i128) -> ZSet<GraphValueRow> {
        ZSet::from_updates(
            [(row.clone(), ZWeight::from_i128(count))],
            LIMBS,
            &mut allow,
        )
        .unwrap()
    }

    fn inputs() -> (GraphValueRow, GraphValueRow) {
        let left = GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(7)),
            GraphValue::List(
                vec![
                    GraphValue::Vertex(VId(u128::MAX)),
                    GraphValue::List(
                        vec![GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice(),
                    ),
                ]
                .into_boxed_slice(),
            ),
        ]);
        let right = GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(7)),
            GraphValue::Vertex(VId(9)),
        ]);
        (left, right)
    }

    fn operator(kind: RowJoinKind) -> IncrementalRowJoin {
        let spec = RowJoinSpec::new(
            &[GraphSetColumnType::Scalar, GraphSetColumnType::List],
            &[GraphSetColumnType::Scalar, GraphSetColumnType::Any],
            &[(0, 0)],
        )
        .unwrap()
        .with_kind(kind);
        IncrementalRowJoin::new(spec)
    }

    #[test]
    fn all_join_kinds_preserve_nested_payloads_and_exact_witness_transitions() {
        let (left, right) = inputs();
        let joined = GraphValueRow::from_owned_values(
            left.values().iter().chain(right.values()).cloned().collect(),
        );
        let mut null_extended = left.values().to_vec();
        null_extended.extend((0..2).map(|_| GraphValue::Scalar(CanonicalScalar::Null)));
        let null_extended = GraphValueRow::from_owned_values(null_extended);
        for kind in [
            RowJoinKind::Inner,
            RowJoinKind::Left,
            RowJoinKind::Right,
            RowJoinKind::Full,
            RowJoinKind::Semi,
            RowJoinKind::Anti,
        ] {
            let mut join = operator(kind);
            join.prepare(&bag(&left, 2), &bag(&right, 3), LIMBS, Some(6), &mut allow)
                .unwrap()
                .commit();
            let matched = match kind {
                RowJoinKind::Inner | RowJoinKind::Left | RowJoinKind::Right | RowJoinKind::Full => {
                    bag(&joined, 6)
                }
                RowJoinKind::Semi => bag(&left, 2),
                RowJoinKind::Anti => ZSet::new(),
            };
            assert_eq!(join.rows(), &matched);
            let expected = match kind {
                RowJoinKind::Inner | RowJoinKind::Right | RowJoinKind::Semi => ZSet::new(),
                RowJoinKind::Left | RowJoinKind::Full => bag(&null_extended, 2),
                RowJoinKind::Anti => bag(&left, 2),
            };
            let delta = join
                .prepare(&ZSet::new(), &bag(&right, -3), LIMBS, Some(2), &mut allow)
                .unwrap()
                .commit();
            assert_eq!(delta, expected.minus(&matched, LIMBS, &mut allow).unwrap());
            assert_eq!(join.rows(), &expected);
            join.prepare(&ZSet::new(), &bag(&right, 3), LIMBS, Some(6), &mut allow)
                .unwrap()
                .commit();
            assert_eq!(join.rows(), &matched);
        }
    }

    #[test]
    fn native_payload_domains_are_admitted_but_never_promoted_to_equality_keys() {
        use GraphSetColumnType::{Any, Edge, Edges, List, Path, Scalar, Vertices};
        for payload in [Any, Edge, Edges, List, Path, Vertices] {
            let schema = [Scalar, payload];
            assert!(RowJoinSpec::new(&schema, &schema, &[(0, 0)]).is_ok());
            assert_eq!(
                RowJoinSpec::new(&schema, &schema, &[(1, 1)]),
                Err(RowJoinBuildError::UnsupportedColumn { side: 0, column: 1 })
            );
            assert_eq!(
                RowJoinSpec::new(&[Scalar], &schema, &[(0, 1)]),
                Err(RowJoinBuildError::UnsupportedColumn { side: 1, column: 1 })
            );
        }
    }

    #[test]
    fn payload_admission_does_not_skip_schema_checks_on_unmatched_rows() {
        let (_, invalid_left) = inputs();
        for kind in [
            RowJoinKind::Inner,
            RowJoinKind::Left,
            RowJoinKind::Right,
            RowJoinKind::Full,
            RowJoinKind::Semi,
            RowJoinKind::Anti,
        ] {
            let mut join = operator(kind);
            assert_eq!(
                join.prepare(&bag(&invalid_left, 1), &ZSet::new(), LIMBS, None, &mut allow)
                    .unwrap_err(),
                RowJoinError::InputSchema { side: 0 }
            );
            assert_eq!(join, operator(kind));
        }
    }

    #[test]
    fn cancellation_while_copying_nested_payloads_never_publishes_partial_state() {
        let (left, right) = inputs();
        let left = bag(&left, 2);
        let right = bag(&right, 3);
        let mut join = operator(RowJoinKind::Left);
        let mut events = 0;
        drop(
            join.prepare(&left, &right, LIMBS, None, &mut |_| {
                events += 1;
                Ok::<_, usize>(())
            })
            .unwrap(),
        );
        assert!(events > 0);
        assert_eq!(join, operator(RowJoinKind::Left));
        for stop in 0..events {
            let mut at = 0;
            let error = join
                .prepare(&left, &right, LIMBS, None, &mut |_| {
                    let current = at;
                    at += 1;
                    if current == stop { Err(stop) } else { Ok(()) }
                })
                .unwrap_err();
            assert_eq!(error, RowJoinError::Delta(ZSetError::Control(stop)));
            assert_eq!(join, operator(RowJoinKind::Left));
        }
        join.prepare(&left, &right, LIMBS, Some(6), &mut allow)
            .unwrap()
            .commit();
        assert_eq!(join.total(), &ZWeight::from_i128(6));
    }
}
