//! Language order for explicit sorting and extrema, separate from canonical
//! storage, grouping and DISTINCT identity. The openCypher 9 orderability
//! contract orders maps, vertices, edges, lists, paths, strings, booleans,
//! numbers and NULL. Timestamp and byte values retain distinct extension
//! positions before strings. Exact numeric ties remain ties until all sort
//! keys are compared; the caller then applies its canonical row tie-break.
//!
//! Source: https://s3.amazonaws.com/artifacts.opencypher.org/openCypher9.pdf
//! (Orderability, pp. 30–32). This does not claim a complete language profile.

use super::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, ValueRef};
use crate::{GlaExecutionEvent, GraphExactAverage};
use core::cmp::Ordering;
use core::convert::Infallible;
use core::num::NonZeroU64;
use fgdb_types::{CanonicalF64, CanonicalPropertyValue, CanonicalScalar};

#[derive(Clone, Copy)]
pub(crate) enum OrderableNumber {
    Rational(i128, NonZeroU64),
    Float(CanonicalF64),
}

impl OrderableNumber {
    pub(crate) fn scalar(value: &CanonicalScalar) -> Option<Self> {
        match value {
            CanonicalScalar::Int(value) => {
                Some(Self::Rational(i128::from(*value), NonZeroU64::MIN))
            }
            CanonicalScalar::Decimal(value) => Some(Self::Rational(
                value.coefficient(),
                NonZeroU64::new(10_u64.pow(fgdb_types::CanonicalDecimal::scale()))
                    .expect("the fixed decimal scale fits a positive u64"),
            )),
            CanonicalScalar::Float(value) => Some(Self::Float(*value)),
            _ => None,
        }
    }

    pub(crate) fn cmp(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::Rational(a, da), Self::Rational(b, db)) => GraphExactAverage::new(a, da.get())
                .expect("nonzero denominator")
                .cmp(&GraphExactAverage::new(b, db.get()).expect("nonzero denominator")),
            (Self::Float(a), Self::Float(b)) => a.cmp(&b),
            (Self::Float(a), Self::Rational(b, db)) => a.compare_rational(b, db),
            (Self::Rational(a, da), Self::Float(b)) => b.compare_rational(a, da).reverse(),
        }
    }
}

impl<'a> From<&'a GraphValue> for ValueRef<'a> {
    fn from(value: &'a GraphValue) -> Self {
        match value {
            GraphValue::Scalar(value) => Self::Scalar(value),
            GraphValue::Vertex(value) => Self::Vertex(*value),
            GraphValue::Edge(value) => Self::Edge(*value),
            GraphValue::Path(value) => Self::Path(value),
            GraphValue::Vertices(value) => Self::Vertices(value),
            GraphValue::Edges(value) => Self::Edges(value),
            GraphValue::List(value) => Self::List(value),
            GraphValue::Map { keys, values } => Self::Map { keys, values },
        }
    }
}

impl GraphValue {
    /// Compare values for explicit ORDER BY and MIN/MAX. This leaves `Ord`,
    /// canonical encoding and equality unchanged. Equal numeric values of
    /// different representations compare equal; callers needing an identity
    /// order must apply a canonical tie-break after all semantic sort keys.
    /// The caller owns admission and collection-comparison cost accounting.
    #[must_use]
    pub fn compare_orderability(&self, other: &Self) -> Ordering {
        ValueRef::from(self).cmp_orderability(ValueRef::from(other))
    }

    /// The same comparison with a work checkpoint per visited value and
    /// logical payload unit, without cloning either input or allocating.
    pub fn compare_orderability_with_control<E>(
        &self,
        other: &Self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        ValueRef::from(self).cmp_orderability_with_control(ValueRef::from(other), control)
    }
}

impl<'a> ValueRef<'a> {
    /// openCypher orderability: map < node < relationship < list < path <
    /// zoned datetime < local datetime < date < zoned time < local time <
    /// duration < string < boolean < number < null. The gaps keep the
    /// temporal kinds not yet in the scalar domain at their places
    /// (fgdb-txbg9).
    fn orderability_rank(self) -> u8 {
        match self {
            Self::Stored(CanonicalPropertyValue::Scalar(scalar)) => {
                Self::Scalar(scalar).orderability_rank()
            }
            Self::Stored(CanonicalPropertyValue::Map(_)) => 0,
            Self::Stored(CanonicalPropertyValue::List(_)) => 3,
            Self::Map { .. } => 0,
            Self::Vertex(_) => 1,
            Self::Edge(_) => 2,
            Self::List(_) | Self::Vertices(_) | Self::Edges(_) => 3,
            Self::Path(_) => 4,
            Self::Scalar(CanonicalScalar::Timestamp(_)) => 5,
            Self::Scalar(CanonicalScalar::LocalDateTime(_)) => 6,
            Self::Scalar(CanonicalScalar::Date(_)) => 7,
            Self::Scalar(CanonicalScalar::Time(_)) => 8,
            Self::Scalar(CanonicalScalar::LocalTime(_)) => 9,
            Self::Scalar(CanonicalScalar::Duration(_)) => 10,
            Self::Scalar(CanonicalScalar::Bytes(_)) => 11,
            Self::Scalar(CanonicalScalar::Text(_)) => 12,
            Self::Scalar(CanonicalScalar::Bool(_)) => 13,
            Self::Scalar(
                CanonicalScalar::Int(_) | CanonicalScalar::Decimal(_) | CanonicalScalar::Float(_),
            ) => 14,
            Self::Scalar(CanonicalScalar::Null) => 15,
        }
    }

    fn list_len(self) -> Option<usize> {
        match self {
            Self::Stored(CanonicalPropertyValue::List(list)) => Some(list.len()),
            Self::List(values) => Some(values.len()),
            Self::Vertices(values) => Some(values.len()),
            Self::Edges(values) => Some(values.len()),
            _ => None,
        }
    }

    fn list_cell(self, at: usize) -> Self {
        match self {
            Self::Stored(CanonicalPropertyValue::List(list)) => Self::stored(&list.values()[at]),
            Self::List(values) => Self::from(&values[at]),
            Self::Vertices(values) => Self::Vertex(values[at]),
            Self::Edges(values) => Self::Edge(values[at]),
            _ => unreachable!("only admitted list variants have list cells"),
        }
    }

    /// The entry count of a map, in memory or stored (fgdb-bqri3).
    fn map_len(self) -> Option<usize> {
        match self {
            Self::Map { keys, .. } => Some(keys.len()),
            Self::Stored(CanonicalPropertyValue::Map(map)) => Some(map.len()),
            _ => None,
        }
    }

    /// A map's `at`-th key and value. Both representations keep their keys
    /// sorted and unique.
    fn map_entry(self, at: usize) -> (&'a str, Self) {
        match self {
            Self::Map { keys, values } => (&keys[at], Self::from(&values[at])),
            Self::Stored(CanonicalPropertyValue::Map(map)) => {
                let entry = &map.entries()[at];
                (entry.key().as_str(), Self::stored(entry.value()))
            }
            _ => unreachable!("only maps have entries"),
        }
    }

    pub(crate) fn cmp_orderability(self, other: Self) -> Ordering {
        match self.cmp_orderability_with_control(other, &mut |_| Ok::<_, Infallible>(())) {
            Ok(order) => order,
            Err(never) => match never {},
        }
    }

    pub(crate) fn cmp_orderability_with_control<E>(
        self,
        other: Self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        control(GlaExecutionEvent::Work)?;
        let rank = self.orderability_rank().cmp(&other.orderability_rank());
        if rank != Ordering::Equal {
            return Ok(rank);
        }
        if let (Some(a), Some(b)) = (self.list_len(), other.list_len()) {
            for at in 0..a.min(b) {
                let order = self
                    .list_cell(at)
                    .cmp_orderability_with_control(other.list_cell(at), control)?;
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            return Ok(a.cmp(&b));
        }
        // A map in memory or stored (fgdb-bqri3). Canonical constructors
        // already sort and uniquify map keys.
        if let (Some(a), Some(b)) = (self.map_len(), other.map_len()) {
            let size = a.cmp(&b);
            if size != Ordering::Equal {
                return Ok(size);
            }
            for at in 0..a {
                let ((a, _), (b, _)) = (self.map_entry(at), other.map_entry(at));
                payload_work(a.len().max(b.len()), control)?;
                let order = a.cmp(b);
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            for at in 0..a {
                let ((_, a), (_, b)) = (self.map_entry(at), other.map_entry(at));
                let order = a.cmp_orderability_with_control(b, control)?;
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            return Ok(Ordering::Equal);
        }
        match (self, other) {
            (Self::Scalar(a), Self::Scalar(b)) => {
                if let (Some(a), Some(b)) = (OrderableNumber::scalar(a), OrderableNumber::scalar(b))
                {
                    return Ok(a.cmp(b));
                }
                for value in [a, b] {
                    let bytes = match value {
                        CanonicalScalar::Text(value) => {
                            value.len() + value.canonical_sort_key().map_or(0, <[u8]>::len)
                        }
                        CanonicalScalar::Bytes(value) => value.as_slice().len(),
                        CanonicalScalar::Timestamp(value) => {
                            value.zone().map_or(0, |zone| zone.identifier().len())
                        }
                        _ => 0,
                    };
                    payload_work(bytes, control)?;
                }
                Ok(a.cmp(b))
            }
            (Self::Path(a), Self::Path(b)) => {
                for _ in 0..a.len().min(b.len()) {
                    control(GlaExecutionEvent::Work)?;
                }
                Ok(a.cmp(b))
            }
            _ => Ok(self.cmp(&other)),
        }
    }
}

fn payload_work<E>(
    bytes: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
        control(GlaExecutionEvent::Work)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
