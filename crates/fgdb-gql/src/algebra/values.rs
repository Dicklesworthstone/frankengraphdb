//! Canonical property-value cells and correlated, distinct projection rows.
//!
//! Source scalars are borrowed while a candidate key is tested. Only a new
//! complete row clones payloads, after its logical scratch reservations.

use super::{BindingSlot, MAX_PATTERN_VERTICES};
use crate::GlaExecutionEvent;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, VId};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::BTreeSet;

/// Preparation-only column declarations. Names are checked before being owned
/// by a prepared pattern. The caller already resolved property key identities.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphColumn<'a> {
    Vertex { name: &'a str, variable: &'a str },
    Property { name: &'a str, variable: &'a str, key: PropertyKeyId },
}

impl<'a> GraphColumn<'a> {
    #[must_use]
    pub const fn vertex(name: &'a str, variable: &'a str) -> Self {
        Self::Vertex { name, variable }
    }

    #[must_use]
    pub const fn property(name: &'a str, variable: &'a str, key: PropertyKeyId) -> Self {
        Self::Property { name, variable, key }
    }

    #[must_use]
    pub const fn name(self) -> &'a str {
        match self { Self::Vertex { name, .. } | Self::Property { name, .. } => name }
    }

    pub(super) const fn variable(self) -> &'a str {
        match self { Self::Vertex { variable, .. } | Self::Property { variable, .. } => variable }
    }
}

impl core::fmt::Debug for GraphColumn<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphColumn").field("definition", &"[REDACTED]").finish()
    }
}

/// Compiler-bound value expression. Column order is semantic, unlike aliases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueProjection {
    Vertex { slot: BindingSlot },
    Property { slot: BindingSlot, key: PropertyKeyId },
}

/// Scalar values retain their exact canonical type, collation and time binding.
/// Missing properties project as Scalar(Null). No numeric coercion occurs.
/// Canonical scalar order precedes the disjoint vertex-identity domain.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GraphValue {
    Scalar(CanonicalScalar),
    Vertex(VId),
}

impl GraphValue {
    #[must_use]
    pub fn as_scalar(&self) -> Option<&CanonicalScalar> {
        match self { Self::Scalar(value) => Some(value), Self::Vertex(_) => None }
    }

    #[must_use]
    pub fn as_vertex(&self) -> Option<VId> {
        match self { Self::Vertex(value) => Some(*value), Self::Scalar(_) => None }
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Scalar(CanonicalScalar::Null))
    }
}

impl core::fmt::Debug for GraphValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self { Self::Scalar(_) => "Scalar", Self::Vertex(_) => "Vertex" };
        f.debug_tuple(kind).field(&"[REDACTED]").finish()
    }
}

/// One complete, owned value tuple. A row's positions match the immutable
/// prepared column schema. Results do not retain the source snapshot lifetime.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphValueRow {
    values: Box<[GraphValue]>,
}

impl GraphValueRow {
    #[must_use]
    pub fn values(&self) -> &[GraphValue] { &self.values }
    #[must_use]
    pub fn get(&self, column: usize) -> Option<&GraphValue> { self.values.get(column) }
    #[must_use]
    pub fn len(&self) -> usize { self.values.len() }
    #[must_use]
    pub fn is_empty(&self) -> bool { self.values.is_empty() }
}

impl core::fmt::Debug for GraphValueRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphValueRow").field("columns", &self.len())
            .field("values", &"[REDACTED]").finish()
    }
}

/// Each additional 64 bytes of variable scalar payload reserves a logical
/// scratch entry before copying. This is not an allocator-byte/peak-memory cap.
pub const GRAPH_VALUE_PAYLOAD_UNIT_BYTES: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ValueRef<'a> {
    Scalar(&'a CanonicalScalar),
    Vertex(VId),
}

impl ValueRef<'_> {
    fn payload_units(self) -> usize {
        let bytes = match self {
            Self::Scalar(CanonicalScalar::Bytes(value)) => value.as_slice().len(),
            Self::Scalar(CanonicalScalar::Text(value)) => {
                // Both lengths have construction-time bounds; their sum fits
                // usize on every supported target, including wasm32.
                value.len() + value.canonical_sort_key().map_or(0, <[u8]>::len)
            }
            Self::Scalar(CanonicalScalar::Timestamp(value)) => {
                value.zone().map_or(0, |zone| zone.identifier().len())
            }
            _ => 0,
        };
        bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
    }

    fn into_owned(self) -> GraphValue {
        match self {
            Self::Scalar(value) => GraphValue::Scalar(value.clone()),
            Self::Vertex(value) => GraphValue::Vertex(value),
        }
    }
}

// Heterogeneous lookup compares borrowed source cells with owned result cells
// under exactly the same order as GraphValueRow::Ord. Hashes never substitute
// for identity. No candidate scalar or tuple allocation is needed for lookup.
trait RowKey {
    fn width(&self) -> usize;
    fn cell(&self, at: usize) -> ValueRef<'_>;
}

struct BorrowedRow<'a>(&'a [ValueRef<'a>]);
impl RowKey for BorrowedRow<'_> {
    fn width(&self) -> usize { self.0.len() }
    fn cell(&self, at: usize) -> ValueRef<'_> { self.0[at] }
}
impl RowKey for GraphValueRow {
    fn width(&self) -> usize { self.values.len() }
    fn cell(&self, at: usize) -> ValueRef<'_> {
        match &self.values[at] {
            GraphValue::Scalar(value) => ValueRef::Scalar(value),
            GraphValue::Vertex(value) => ValueRef::Vertex(*value),
        }
    }
}
impl<'a> Borrow<dyn RowKey + 'a> for GraphValueRow {
    fn borrow(&self) -> &(dyn RowKey + 'a) { self }
}
impl PartialEq for dyn RowKey + '_ {
    fn eq(&self, other: &Self) -> bool { self.cmp(other) == Ordering::Equal }
}
impl Eq for dyn RowKey + '_ {}
impl PartialOrd for dyn RowKey + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for dyn RowKey + '_ {
    fn cmp(&self, other: &Self) -> Ordering {
        for at in 0..self.width().min(other.width()) {
            let order = self.cell(at).cmp(&other.cell(at));
            if order != Ordering::Equal { return order; }
        }
        self.width().cmp(&other.width())
    }
}

pub(super) fn collect_values<'a, E>(
    columns: &[ValueProjection],
    bindings: &[VId],
    projected: &mut BTreeSet<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let null = CanonicalScalar::Null;
    let mut key = [ValueRef::Scalar(&null); MAX_PATTERN_VERTICES];
    for (at, column) in columns.iter().enumerate() {
        control(GlaExecutionEvent::Work)?;
        key[at] = match column {
            ValueProjection::Vertex { slot } => ValueRef::Vertex(bindings[slot.ordinal() as usize]),
            ValueProjection::Property { slot, key } => {
                let value = property(bindings[slot.ordinal() as usize], *key)?;
                ValueRef::Scalar(value.unwrap_or(&null))
            }
        };
        for _ in 0..key[at].payload_units() { control(GlaExecutionEvent::Work)?; }
    }
    let borrowed = BorrowedRow(&key[..columns.len()]);
    if projected.contains(&borrowed as &dyn RowKey) { return Ok(()); }
    control(GlaExecutionEvent::ScratchEntry)?;
    let mut values = Vec::new();
    for value in &key[..columns.len()] {
        control(GlaExecutionEvent::ScratchEntry)?;
        for _ in 0..value.payload_units() { control(GlaExecutionEvent::ScratchEntry)?; }
        values.push((*value).into_owned());
    }
    projected.insert(GraphValueRow { values: values.into_boxed_slice() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::CanonicalF64;

    fn columns() -> [ValueProjection; 2] {
        [ValueProjection::Vertex { slot: BindingSlot(0) },
            ValueProjection::Property { slot: BindingSlot(1), key: PropertyKeyId(7) }]
    }

    #[test]
    fn owned_and_borrowed_value_keys_have_identical_total_order() {
        let scalars = [CanonicalScalar::Null, CanonicalScalar::Bool(false), CanonicalScalar::Int(-1),
            CanonicalScalar::Int(0), CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
            CanonicalScalar::ucs_basic_text("private payload").unwrap(),
            CanonicalScalar::bytes(vec![0, 255]).unwrap()];
        let mut rows = Vec::new();
        for scalar in &scalars {
            rows.push(GraphValueRow { values: vec![GraphValue::Scalar(scalar.clone()), GraphValue::Vertex(VId(9))].into() });
            rows.push(GraphValueRow { values: vec![GraphValue::Vertex(VId(9)), GraphValue::Scalar(scalar.clone())].into() });
        }
        for left in &rows {
            for right in &rows {
                let borrowed: Vec<_> = (0..right.len()).map(|at| right.cell(at)).collect();
                let key = BorrowedRow(&borrowed);
                assert_eq!(left.cmp(right), (left as &dyn RowKey).cmp(&key));
                let set = BTreeSet::from([left.clone()]);
                assert_eq!(set.contains(&key as &dyn RowKey), left == right);
            }
        }
        assert!(!format!("{rows:?}").contains("private payload"));
        assert!(!format!("{rows:?}").contains("VId(9)"));
    }

    #[test]
    fn missing_and_stored_null_collapse_but_vertex_correlations_survive() {
        let null = CanonicalScalar::Null;
        let mut rows = BTreeSet::new();
        for (owner, present) in [(1, false), (1, true), (2, false)] {
            collect_values(&columns(), &[VId(owner), VId(4)], &mut rows,
                &mut |_, _| Ok::<_, ()>(present.then_some(&null)), &mut |_| Ok(())).unwrap();
        }
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert!(row.get(1).unwrap().is_null());
            assert!(row.get(0).unwrap().as_vertex().is_some());
            assert_eq!(row.get(2), None);
            assert!(!row.is_empty());
        }
    }

    #[test]
    fn duplicate_payload_rows_allocate_nothing_and_payload_growth_is_charged() {
        let payload = CanonicalScalar::bytes(vec![7; 129]).unwrap();
        let mut rows = BTreeSet::new();
        let mut scratch = 0;
        for _ in 0..2 {
            collect_values(&columns(), &[VId(1), VId(2)], &mut rows,
                &mut |_, _| Ok::<_, ()>(Some(&payload)), &mut |event| {
                    scratch += usize::from(event == GlaExecutionEvent::ScratchEntry); Ok(())
                }).unwrap();
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(scratch, 1 + 2 + 3);
        assert_eq!(rows.first().unwrap().get(1).unwrap().as_scalar(), Some(&payload));
    }

    #[test]
    fn every_value_projection_checkpoint_refuses_before_row_publication() {
        let payload = CanonicalScalar::ucs_basic_text(&"x".repeat(129)).unwrap();
        let mut total = 0;
        collect_values(&columns(), &[VId(1), VId(2)], &mut BTreeSet::new(),
            &mut |_, _| Ok::<_, usize>(Some(&payload)), &mut |_| { total += 1; Ok(()) }).unwrap();
        for stop in 1..=total {
            let mut rows = BTreeSet::new();
            let mut calls = 0;
            let result = collect_values(&columns(), &[VId(1), VId(2)], &mut rows,
                &mut |_, _| Ok::<_, usize>(Some(&payload)), &mut |_| {
                    calls += 1; if calls == stop { Err(stop) } else { Ok(()) }
                });
            assert_eq!(result, Err(stop));
            assert_eq!(calls, stop);
            assert!(rows.is_empty());
        }
        let mut rows = BTreeSet::new();
        assert_eq!(collect_values(&columns(), &[VId(1), VId(2)], &mut rows,
            &mut |_, _| Err::<Option<&CanonicalScalar>, _>("source"), &mut |_| Ok(())), Err("source"));
        assert!(rows.is_empty());
    }
}
