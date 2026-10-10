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
use fgdb_types::{CanonicalF64, CanonicalScalar};

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

impl ValueRef<'_> {
    fn orderability_rank(self) -> u8 {
        match self {
            Self::Map { .. } => 0,
            Self::Vertex(_) => 1,
            Self::Edge(_) => 2,
            Self::List(_) | Self::Vertices(_) | Self::Edges(_) => 3,
            Self::Path(_) => 4,
            Self::Scalar(CanonicalScalar::Timestamp(_)) => 5,
            Self::Scalar(CanonicalScalar::Bytes(_)) => 6,
            Self::Scalar(CanonicalScalar::Text(_)) => 7,
            Self::Scalar(CanonicalScalar::Bool(_)) => 8,
            Self::Scalar(
                CanonicalScalar::Int(_) | CanonicalScalar::Decimal(_) | CanonicalScalar::Float(_),
            ) => 9,
            Self::Scalar(CanonicalScalar::Null) => 10,
        }
    }

    fn list_len(self) -> Option<usize> {
        match self {
            Self::List(values) => Some(values.len()),
            Self::Vertices(values) => Some(values.len()),
            Self::Edges(values) => Some(values.len()),
            _ => None,
        }
    }

    fn list_cell(self, at: usize) -> Self {
        match self {
            Self::List(values) => Self::from(&values[at]),
            Self::Vertices(values) => Self::Vertex(values[at]),
            Self::Edges(values) => Self::Edge(values[at]),
            _ => unreachable!("only admitted list variants have list cells"),
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
        match (self, other) {
            (
                Self::Map {
                    keys: ak,
                    values: av,
                },
                Self::Map {
                    keys: bk,
                    values: bv,
                },
            ) => {
                // Canonical constructors already sort and uniquify map keys.
                let size = ak.len().cmp(&bk.len());
                if size != Ordering::Equal {
                    return Ok(size);
                }
                for (a, b) in ak.iter().zip(bk) {
                    payload_work(a.len().max(b.len()), control)?;
                    let order = a.cmp(b);
                    if order != Ordering::Equal {
                        return Ok(order);
                    }
                }
                for (a, b) in av.iter().zip(bv) {
                    let order = a.compare_orderability_with_control(b, control)?;
                    if order != Ordering::Equal {
                        return Ok(order);
                    }
                }
                Ok(av.len().cmp(&bv.len()))
            }
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
