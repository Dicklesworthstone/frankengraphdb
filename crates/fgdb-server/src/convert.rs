//! Exact conversions between engine values and FGP wire values.
//!
//! Result cells convert totally: every engine value has exactly one wire
//! spelling. Arguments convert partially: a wire value that has no statement
//! parameter type (an element identity, an aggregate) is refused, never
//! coerced, and a zoned timestamp is admitted only through the served
//! database's pinned tz database, exactly as the CLI admits one.

use fgdb::QueryValue;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlListParameter, GqlParameterValue, GqlParameters, GqlScalarParameter};
use fgdb_protocol::body::{WireTimestamp, WireValue, WireZone};
use fgdb_types::{CanonicalF64, CanonicalScalar, CanonicalTimestamp, ObjectId, TzdbResolver};

/// One result cell.
pub(crate) fn cell(value: &QueryValue) -> WireValue {
    match value {
        QueryValue::Value(value) => graph(value),
        QueryValue::Count(count) => WireValue::Count(*count),
        QueryValue::Integer(integer) => WireValue::WideInt(*integer),
        QueryValue::Average(average) => WireValue::Average {
            numerator: average.numerator(),
            denominator: average.denominator(),
        },
    }
}

fn graph(value: &GraphValue) -> WireValue {
    match value {
        GraphValue::Scalar(scalar) => scalar_value(scalar),
        GraphValue::Vertex(vid) => WireValue::Vertex(vid.0),
        GraphValue::Edge(eid) => WireValue::Edge(eid.0),
        GraphValue::Path(path) => WireValue::Path {
            start: path.start().0,
            steps: path
                .steps()
                .iter()
                .map(|(edge, vertex)| (edge.0, vertex.0))
                .collect(),
        },
        GraphValue::Vertices(ids) => WireValue::Vertices(ids.iter().map(|id| id.0).collect()),
        GraphValue::Edges(ids) => WireValue::Edges(ids.iter().map(|id| id.0).collect()),
        GraphValue::List(items) => WireValue::List(items.iter().map(graph).collect()),
        // The engine keeps keys unique and ascending by UTF-8 bytes, which is
        // exactly the wire map's canonical order.
        GraphValue::Map { keys, values } => WireValue::Map(
            keys.iter()
                .zip(values.iter())
                .map(|(key, value)| (key.to_string(), graph(value)))
                .collect(),
        ),
    }
}

fn scalar_value(scalar: &CanonicalScalar) -> WireValue {
    match scalar {
        CanonicalScalar::Null => WireValue::Null,
        CanonicalScalar::Bool(v) => WireValue::Bool(*v),
        CanonicalScalar::Int(v) => WireValue::Int(*v),
        CanonicalScalar::Decimal(v) => WireValue::Decimal(v.to_string()),
        CanonicalScalar::Float(v) => WireValue::Float(v.get()),
        CanonicalScalar::Text(v) => WireValue::Text(v.as_str().to_owned()),
        CanonicalScalar::Timestamp(v) => WireValue::Timestamp(WireTimestamp {
            instant_utc_nanos: v.instant_utc_nanos(),
            utc_offset_seconds: v.utc_offset_seconds(),
            zone: v.zone().map(|zone| WireZone {
                identifier: zone.identifier().to_owned(),
                tzdb_oid: zone.tzdb_oid().0,
            }),
        }),
        CanonicalScalar::Bytes(v) => WireValue::Bytes(v.as_slice().to_vec()),
    }
}

/// Why an argument was refused. Names a parameter, never its value.
#[derive(Debug)]
pub(crate) struct ArgumentError {
    pub(crate) parameter: String,
    pub(crate) reason: &'static str,
}

impl core::fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "parameter ${}: {}", self.parameter, self.reason)
    }
}

/// Bind every named wire argument as a statement parameter.
pub(crate) fn parameters(
    arguments: &[(String, WireValue)],
    tzdb: Option<&(dyn TzdbResolver + Send + Sync)>,
) -> Result<GqlParameters, ArgumentError> {
    let mut out = GqlParameters::new();
    for (name, value) in arguments {
        let refuse = |reason| ArgumentError {
            parameter: name.clone(),
            reason,
        };
        let parameter = match value {
            WireValue::Int(v) => GqlParameterValue::Int64(*v),
            WireValue::List(items) => {
                let items = items
                    .iter()
                    .map(|item| element(item, tzdb))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(refuse)?;
                GqlParameterValue::List(
                    GqlListParameter::new(items)
                        .map_err(|_| refuse("list exceeds the parameter bounds"))?,
                )
            }
            WireValue::Map(_) => return Err(refuse("a map binds only inside a list")),
            scalar => GqlParameterValue::Scalar(
                GqlScalarParameter::new(argument_scalar(scalar, tzdb).map_err(refuse)?)
                    .map_err(|_| refuse("scalar exceeds the parameter bounds"))?,
            ),
        };
        out.insert(name, parameter)
            .map_err(|_| refuse("not a valid parameter name"))?;
    }
    Ok(out)
}

/// A list element or map entry: a scalar, a nested list, or a map.
fn element(
    value: &WireValue,
    tzdb: Option<&(dyn TzdbResolver + Send + Sync)>,
) -> Result<GraphValue, &'static str> {
    Ok(match value {
        WireValue::List(items) => GraphValue::List(
            items
                .iter()
                .map(|item| element(item, tzdb))
                .collect::<Result<_, _>>()?,
        ),
        WireValue::Map(entries) => GraphValue::map(
            entries
                .iter()
                .map(|(key, value)| Ok((key.as_str().into(), element(value, tzdb)?)))
                .collect::<Result<Vec<_>, &'static str>>()?,
        )
        .ok_or("duplicate map key")?,
        scalar => GraphValue::Scalar(argument_scalar(scalar, tzdb)?),
    })
}

fn argument_scalar(
    value: &WireValue,
    tzdb: Option<&(dyn TzdbResolver + Send + Sync)>,
) -> Result<CanonicalScalar, &'static str> {
    Ok(match value {
        WireValue::Null => CanonicalScalar::Null,
        WireValue::Bool(v) => CanonicalScalar::Bool(*v),
        WireValue::Int(v) => CanonicalScalar::Int(*v),
        WireValue::Float(v) => {
            if !v.is_finite() {
                return Err("a float argument must be finite");
            }
            CanonicalScalar::Float(CanonicalF64::new(*v))
        }
        WireValue::Text(v) => CanonicalScalar::ucs_basic_text(v)
            .map_err(|_| "text is not admissible canonical text")?,
        WireValue::Bytes(v) => {
            CanonicalScalar::bytes(v.clone()).map_err(|_| "bytes exceed the scalar bound")?
        }
        WireValue::Timestamp(t) => CanonicalScalar::Timestamp(match &t.zone {
            None => CanonicalTimestamp::offset_only(t.instant_utc_nanos, t.utc_offset_seconds)
                .map_err(|_| "timestamp out of range")?,
            Some(zone) => {
                let resolver =
                    tzdb.ok_or("a zoned timestamp needs a tz database configured on the server")?;
                CanonicalTimestamp::zoned(
                    t.instant_utc_nanos,
                    t.utc_offset_seconds,
                    &zone.identifier,
                    ObjectId(zone.tzdb_oid),
                    resolver,
                )
                .map_err(|_| "zoned timestamp does not resolve under the served tz database")?
            }
        }),
        WireValue::Decimal(_) => return Err("decimal arguments are not supported"),
        WireValue::Vertex(_)
        | WireValue::Edge(_)
        | WireValue::Path { .. }
        | WireValue::Vertices(_)
        | WireValue::Edges(_) => return Err("element identities are not statement arguments"),
        WireValue::Count(_) | WireValue::WideInt(_) | WireValue::Average { .. } => {
            return Err("aggregate values are not statement arguments");
        }
        WireValue::List(_) | WireValue::Map(_) => return Err("unexpected collection"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::{EId, VId};

    #[test]
    fn result_cells_convert_exactly() {
        let value = GraphValue::List(
            vec![
                GraphValue::Scalar(CanonicalScalar::Int(-3)),
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(0.5))),
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Edges(vec![EId(9), EId(2)].into()),
            ]
            .into(),
        );
        assert_eq!(
            cell(&QueryValue::Value(value)),
            WireValue::List(vec![
                WireValue::Int(-3),
                WireValue::Float(0.5),
                WireValue::Vertex(u128::MAX),
                WireValue::Edges(vec![9, 2]),
            ])
        );
        assert_eq!(cell(&QueryValue::Count(7)), WireValue::Count(7));
    }

    #[test]
    fn arguments_without_a_parameter_type_are_refused_by_name() {
        let error = parameters(&[("who".into(), WireValue::Vertex(1))], None).unwrap_err();
        assert_eq!(error.parameter, "who");
        let error = parameters(&[("m".into(), WireValue::Map(vec![]))], None).unwrap_err();
        assert_eq!(error.parameter, "m");
        let error = parameters(&[("f".into(), WireValue::Float(f64::NAN))], None).unwrap_err();
        assert_eq!(error.parameter, "f");
        let rows = WireValue::List(vec![WireValue::Map(vec![(
            "name".into(),
            WireValue::Text("Ann".into()),
        )])]);
        assert!(
            parameters(
                &[("n".into(), WireValue::Int(1)), ("rows".into(), rows)],
                None
            )
            .is_ok()
        );
    }
}
