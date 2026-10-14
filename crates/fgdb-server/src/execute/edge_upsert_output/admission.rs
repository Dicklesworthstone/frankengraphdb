//! Borrowed preflight for wire ownership. This is not a second encoder: the
//! existing converter and FGP codec still decide the exact representation.
//! Walk no deeper than the protocol permits, and spend the live permit before
//! copying any part of the row into a second, wire-owned value tree.

use super::{Refusal, body_refusal};
use fgdb_gql::algebra::GraphValue;
use fgdb_protocol::body::{
    BodyError, ErrorCode, MAX_DATE_BYTES, MAX_DURATION_BYTES, MAX_LOCAL_DATETIME_BYTES,
    MAX_LOCAL_TIME_BYTES, MAX_NAME_BYTES, MAX_SCALAR_BYTES, MAX_TIME_BYTES, MAX_VALUE_DEPTH,
    MAX_VALUE_NODES,
};
use fgdb_types::CanonicalScalar;
use std::fmt::{self, Write};

type Charge<'a> = &'a mut dyn FnMut(u64) -> Result<(), Refusal>;

pub(super) fn row(cells: &[GraphValue], charge: Charge<'_>) -> Result<(), Refusal> {
    let mut admission = Admission { charge, nodes: 0 };
    for cell in cells {
        admission.value(cell, 0)?;
    }
    Ok(())
}

fn bounded(len: usize, limit: usize) -> Result<(), Refusal> {
    if len > limit {
        Err(body_refusal(BodyError::TooLarge))
    } else {
        Ok(())
    }
}

fn bytes(len: usize, charge: Charge<'_>) -> Result<(), Refusal> {
    let units = u64::try_from(len.div_ceil(64)).map_err(|_| body_refusal(BodyError::TooLarge))?;
    charge(units)
}

struct Admission<'a> {
    charge: Charge<'a>,
    // This is the protocol's per-body value-node ceiling applied to one row,
    // not a whole result: independent chunks may deliver additional rows.
    nodes: usize,
}

impl Admission<'_> {
    fn payload(&mut self, len: usize, limit: usize) -> Result<(), Refusal> {
        bounded(len, limit)?;
        bytes(len, self.charge)
    }

    fn identities(&mut self, len: usize, width: usize) -> Result<(), Refusal> {
        bounded(len, MAX_VALUE_NODES)?;
        let size = len
            .checked_mul(width)
            .ok_or_else(|| body_refusal(BodyError::TooLarge))?;
        bytes(size, self.charge)
    }

    fn children(&self, len: usize) -> Result<(), Refusal> {
        // Every child needs at least one value node. Refuse impossible shapes
        // before walking a large collection, not after copying its prefix.
        bounded(len, MAX_VALUE_NODES - self.nodes)
    }

    fn value(&mut self, value: &GraphValue, depth: usize) -> Result<(), Refusal> {
        (self.charge)(1)?;
        if depth > MAX_VALUE_DEPTH {
            return Err(body_refusal(BodyError::TooDeep));
        }
        if self.nodes == MAX_VALUE_NODES {
            return Err(body_refusal(BodyError::TooLarge));
        }
        self.nodes += 1;
        match value {
            GraphValue::Scalar(scalar) => match scalar {
                CanonicalScalar::Text(text) => self.payload(text.as_str().len(), MAX_SCALAR_BYTES),
                CanonicalScalar::Bytes(value) => {
                    self.payload(value.as_slice().len(), MAX_SCALAR_BYTES)
                }
                CanonicalScalar::Timestamp(value) => {
                    if let Some(zone) = value.zone() {
                        self.payload(zone.identifier().len(), MAX_NAME_BYTES)?;
                    }
                    Ok(())
                }
                CanonicalScalar::Decimal(value) => formatted(value, MAX_SCALAR_BYTES, self.charge),
                CanonicalScalar::Date(value) => formatted(value, MAX_DATE_BYTES, self.charge),
                CanonicalScalar::LocalTime(value) => {
                    formatted(value, MAX_LOCAL_TIME_BYTES, self.charge)
                }
                CanonicalScalar::Time(value) => formatted(value, MAX_TIME_BYTES, self.charge),
                CanonicalScalar::LocalDateTime(value) => {
                    formatted(value, MAX_LOCAL_DATETIME_BYTES, self.charge)
                }
                CanonicalScalar::Duration(value) => {
                    formatted(value, MAX_DURATION_BYTES, self.charge)
                }
                CanonicalScalar::Null
                | CanonicalScalar::Bool(_)
                | CanonicalScalar::Int(_)
                | CanonicalScalar::Float(_) => Ok(()),
            },
            GraphValue::Vertex(_) | GraphValue::Edge(_) => Ok(()),
            GraphValue::Path(path) => self.identities(path.steps().len(), 32),
            GraphValue::Vertices(ids) => self.identities(ids.len(), 16),
            GraphValue::Edges(ids) => self.identities(ids.len(), 16),
            GraphValue::List(items) => {
                self.children(items.len())?;
                for item in items {
                    self.value(item, depth + 1)?;
                }
                Ok(())
            }
            GraphValue::Map { keys, values } => {
                // The total converter zips keys and values. Validate equality
                // BEFORE that zip so malformed native values cannot lose data.
                if keys.len() != values.len() {
                    return Err(body_refusal(BodyError::Noncanonical));
                }
                self.children(values.len())?;
                let mut previous: Option<&str> = None;
                for (key, value) in keys.iter().zip(values.iter()) {
                    let key: &str = key.as_ref();
                    (self.charge)(1)?;
                    self.payload(key.len(), MAX_NAME_BYTES)?;
                    if let Some(previous) = previous {
                        bytes(previous.len(), self.charge)?;
                        if previous.as_bytes() >= key.as_bytes() {
                            return Err(body_refusal(BodyError::Noncanonical));
                        }
                    }
                    previous = Some(key);
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
        }
    }
}

// Temporal and decimal values render to strings, unlike borrowed Text/Bytes.
// Bound and meter that rendering without allocating the rendered string.
// The sink retains the original meter refusal instead of losing it in fmt::Error.
struct DisplayAdmission<'a> {
    charge: Charge<'a>,
    remaining: usize,
    failure: Option<Refusal>,
}

impl Write for DisplayAdmission<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.failure.is_some() {
            return Err(fmt::Error);
        }
        let result = (|| {
            bounded(value.len(), self.remaining)?;
            (self.charge)(1)?;
            bytes(value.len(), self.charge)?;
            self.remaining -= value.len();
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failure = Some(error);
                Err(fmt::Error)
            }
        }
    }
}

fn formatted(value: &dyn fmt::Display, limit: usize, charge: Charge<'_>) -> Result<(), Refusal> {
    let mut sink = DisplayAdmission {
        charge,
        remaining: limit,
        failure: None,
    };
    let result = write!(&mut sink, "{value}");
    if let Some(error) = sink.failure {
        return Err(error);
    }
    result.map_err(|_| Refusal::new(ErrorCode::Execution, "native scalar formatting failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert;
    use fgdb_protocol::body::{Body, ResultChunk, WireValue};
    use fgdb_types::{CanonicalDate, CanonicalDuration, CanonicalF64, EId, VId};

    fn accepted<T>(result: Result<T, Refusal>) -> T {
        result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
    }

    #[test]
    fn nested_ownership_is_metered_without_changing_wire_values() {
        let cells = vec![GraphValue::List(
            vec![
                GraphValue::Scalar(CanonicalScalar::Int(-3)),
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(0.5))),
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Edges(vec![EId(9), EId(2)].into()),
                GraphValue::map(vec![(
                    "key".into(),
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(65)).unwrap()),
                )])
                .unwrap(),
            ]
            .into(),
        )];
        let mut used = 0;
        accepted(row(&cells, &mut |units| {
            used += units;
            Ok(())
        }));
        // Seven value nodes, one key visit, one key payload unit, one ID
        // payload unit and two text payload units. Encoding has its own bill.
        assert_eq!(used, 12);
        let wire: Vec<_> = cells.iter().map(convert::graph).collect();
        assert_eq!(
            wire,
            vec![WireValue::List(vec![
                WireValue::Int(-3),
                WireValue::Float(0.5),
                WireValue::Vertex(u128::MAX),
                WireValue::Edges(vec![9, 2]),
                WireValue::Map(vec![("key".into(), WireValue::Text("x".repeat(65)))]),
            ])]
        );
        let chunk = ResultChunk {
            columns: Some(vec!["value".into()]),
            rows: vec![wire],
        };
        let decoded = ResultChunk::decode(&chunk.encode().unwrap()).unwrap();
        assert_eq!(decoded.rows, chunk.rows);
        assert_eq!(decoded.columns, chunk.columns);
    }

    #[test]
    fn nested_visits_stop_at_the_first_live_permit_refusal() {
        let cells = vec![GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Int(1)); 1000].into(),
        )];
        let mut calls = 0;
        let error = row(&cells, &mut |_| {
            calls += 1;
            if calls == 4 {
                Err(Refusal::new(ErrorCode::PermissionDenied, "retired"))
            } else {
                Ok(())
            }
        })
        .expect_err("nested elements must not be free");
        assert_eq!(calls, 4);
        assert_eq!(error.code, ErrorCode::PermissionDenied);
        assert_eq!(error.message, "retired");
    }

    #[test]
    fn depth_and_row_node_limits_apply_before_recursive_conversion() {
        let mut value = GraphValue::Scalar(CanonicalScalar::Null);
        for _ in 0..MAX_VALUE_DEPTH {
            value = GraphValue::List(vec![value].into());
        }
        accepted(row(std::slice::from_ref(&value), &mut |_| Ok(())));
        value = GraphValue::List(vec![value].into());
        assert_eq!(
            row(&[value], &mut |_| Ok(())).err().unwrap().code,
            ErrorCode::Budget
        );
        // Exercise the cumulative counter's exact boundary without allocating
        // millions of native values just to reach the same state.
        let mut charge = |_| Ok(());
        let mut admission = Admission {
            charge: &mut charge,
            nodes: MAX_VALUE_NODES - 1,
        };
        accepted(admission.value(&GraphValue::Scalar(CanonicalScalar::Null), 0));
        assert_eq!(
            admission
                .value(&GraphValue::Scalar(CanonicalScalar::Null), 0)
                .err()
                .unwrap()
                .code,
            ErrorCode::Budget
        );
    }

    #[test]
    fn malformed_maps_are_not_silently_truncated_by_zip() {
        for value in [
            GraphValue::Map {
                keys: vec!["a".into()].into(),
                values: Vec::new().into(),
            },
            GraphValue::Map {
                keys: vec!["b".into(), "a".into()].into(),
                values: vec![GraphValue::Scalar(CanonicalScalar::Null); 2].into(),
            },
            GraphValue::Map {
                keys: vec!["a".into(), "a".into()].into(),
                values: vec![GraphValue::Scalar(CanonicalScalar::Null); 2].into(),
            },
        ] {
            assert_eq!(
                row(&[value], &mut |_| Ok(())).err().unwrap().code,
                ErrorCode::Execution
            );
        }
        let name = "private".repeat(MAX_NAME_BYTES);
        let value = GraphValue::map(vec![(
            name.clone().into(),
            GraphValue::Scalar(CanonicalScalar::Null),
        )])
        .unwrap();
        let error = row(&[value], &mut |_| Ok(())).err().unwrap();
        assert_eq!(error.code, ErrorCode::Budget);
        assert!(!error.message.contains(&name));
    }

    #[test]
    fn temporal_rendering_is_bounded_and_preserves_the_original_meter_error() {
        let date = CanonicalDate::from_civil(10_000, 1, 1).unwrap();
        let cells = [
            GraphValue::Scalar(CanonicalScalar::Date(date)),
            GraphValue::Scalar(CanonicalScalar::Duration(
                CanonicalDuration::new(14, -3, 4_500_000_000).unwrap(),
            )),
        ];
        accepted(row(&cells, &mut |_| Ok(())));
        assert_eq!(
            convert::graph(&cells[0]),
            WireValue::Date("+10000-01-01".into())
        );
        assert_eq!(
            formatted(&date, 1, &mut |_| Ok(())).err().unwrap().code,
            ErrorCode::Budget
        );
        let mut calls = 0;
        let error = formatted(&date, MAX_DATE_BYTES, &mut |_| {
            calls += 1;
            Err(Refusal::new(
                ErrorCode::Cancelled,
                "cancelled at formatting",
            ))
        })
        .err()
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(error.message, "cancelled at formatting");
    }
}
