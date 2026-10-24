//! Exact conversions between engine values and FGP wire values.
//!
//! Result cells convert totally: every engine value has exactly one wire
//! spelling. Arguments convert partially: a wire value that has no statement
//! parameter type (an element identity, a wide/exact aggregate) is refused, never
//! coerced, and a zoned timestamp is admitted only through the served
//! database's pinned tz database, exactly as the CLI admits one.

use fgdb::QueryValue;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlListParameter, GqlMapParameter, GqlParameterValue, GqlParameters, GqlScalarParameter,
};
use fgdb_protocol::body::{WireTimestamp, WireValue, WireZone};
use fgdb_types::{
    CanonicalDate, CanonicalDuration, CanonicalF64, CanonicalLocalDateTime, CanonicalLocalTime,
    CanonicalScalar, CanonicalTime, CanonicalTimestamp, ObjectId, TzdbResolver,
};

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

pub(crate) fn graph(value: &GraphValue) -> WireValue {
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
        CanonicalScalar::Date(v) => WireValue::Date(v.to_string()),
        CanonicalScalar::LocalTime(v) => WireValue::LocalTime(v.to_string()),
        CanonicalScalar::Time(v) => WireValue::Time(v.to_string()),
        CanonicalScalar::LocalDateTime(v) => WireValue::LocalDateTime(v.to_string()),
        CanonicalScalar::Duration(v) => WireValue::Duration(v.to_string()),
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
            // Unsigned operands select historical cuts and row windows. They
            // retain their exact domain instead of narrowing through i64.
            WireValue::Count(v) => GqlParameterValue::UInt64(*v),
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
            WireValue::Map(entries) => {
                let entries = entries
                    .iter()
                    .map(|(key, value)| Ok((key.as_str().into(), element(value, tzdb)?)))
                    .collect::<Result<Vec<_>, &'static str>>()
                    .map_err(refuse)?;
                GqlParameterValue::Map(GqlMapParameter::new(entries).map_err(|_| {
                    refuse("map has duplicate keys or exceeds the parameter bounds")
                })?)
            }
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
        // Only the canonical text a server emits: parse, then require that
        // rendering it back gives the same bytes.
        WireValue::Date(v) => match CanonicalDate::parse_iso(v) {
            Ok(date) if date.to_string() == *v => CanonicalScalar::Date(date),
            _ => return Err("a date argument must be canonical ISO 8601 text"),
        },
        WireValue::LocalTime(v) => match CanonicalLocalTime::parse_iso(v) {
            Ok(time) if time.to_string() == *v => CanonicalScalar::LocalTime(time),
            _ => return Err("a local time argument must be canonical ISO 8601 text"),
        },
        WireValue::Time(v) => match CanonicalTime::parse_iso(v) {
            Ok(time) if time.to_string() == *v => CanonicalScalar::Time(time),
            _ => return Err("a time argument must be canonical ISO 8601 text"),
        },
        WireValue::LocalDateTime(v) => match CanonicalLocalDateTime::parse_iso(v) {
            Ok(value) if value.to_string() == *v => CanonicalScalar::LocalDateTime(value),
            _ => return Err("a local date-time argument must be canonical ISO 8601 text"),
        },
        WireValue::Duration(v) => match CanonicalDuration::parse_iso(v) {
            Ok(value) if value.to_string() == *v => CanonicalScalar::Duration(value),
            _ => return Err("a duration argument must be canonical ISO 8601 text"),
        },
        // Decimal output is a canonical fixed-scale string, not a JSON/f64
        // number. Reuse the core inverse codec for scalar and nested operands.
        WireValue::Decimal(v) => CanonicalScalar::Decimal(
            fgdb_types::CanonicalDecimal::parse_canonical(v)
                .map_err(|_| "a decimal argument must be canonical fixed-scale text")?,
        ),
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
        let date = CanonicalDate::from_civil(10_000, 1, 1).unwrap();
        assert_eq!(
            cell(&QueryValue::Value(GraphValue::Scalar(
                CanonicalScalar::Date(date)
            ))),
            WireValue::Date("+10000-01-01".into())
        );
    }

    /// fgdb-txbg9: a date argument is exactly the canonical text a server
    /// emits; another ISO spelling of the same day, or a day that does not
    /// exist, is refused rather than reinterpreted.
    #[test]
    fn date_arguments_are_canonical_text_only() {
        let parameters_of =
            |text: &str| parameters(&[("d".into(), WireValue::Date(text.into()))], None);
        let bound = parameters_of("2015-07-21").unwrap();
        assert!(matches!(
            bound.get("d"),
            Some(GqlParameterValue::Scalar(scalar))
                if *scalar.value()
                    == CanonicalScalar::Date(CanonicalDate::from_civil(2015, 7, 21).unwrap())
        ));
        for text in ["20150721", "2015-W30-2", "+2015-07-21", "2015-02-30", ""] {
            assert_eq!(parameters_of(text).unwrap_err().parameter, "d", "{text}");
        }
        // The times follow the same rule: the canonical spelling binds, and
        // another spelling of the same value is refused.
        let bind = |value: WireValue| {
            parameters(&[("t".into(), value)], None)
                .map(|bound| match bound.get("t") {
                    Some(GqlParameterValue::Scalar(scalar)) => scalar.value().clone(),
                    _ => panic!("a temporal argument binds a scalar"),
                })
                .map_err(|error| error.parameter)
        };
        let local = CanonicalLocalTime::from_hms_nano(21, 40, 0, 0).unwrap();
        assert_eq!(
            bind(WireValue::LocalTime("21:40".into())),
            Ok(CanonicalScalar::LocalTime(local))
        );
        assert_eq!(
            bind(WireValue::Time("21:40-01:30".into())),
            Ok(CanonicalScalar::Time(
                CanonicalTime::new(local, -5_400).unwrap()
            ))
        );
        assert_eq!(
            bind(WireValue::LocalDateTime("2015-07-21T21:40".into())),
            Ok(CanonicalScalar::LocalDateTime(CanonicalLocalDateTime::new(
                CanonicalDate::from_civil(2015, 7, 21).unwrap(),
                local
            )))
        );
        assert_eq!(
            bind(WireValue::Duration("P1Y2M-3DT4.5S".into())),
            Ok(CanonicalScalar::Duration(
                CanonicalDuration::new(14, -3, 4_500_000_000).unwrap()
            ))
        );
        for value in [
            WireValue::LocalTime("2140".into()),
            WireValue::LocalTime("21:40:00".into()),
            WireValue::Time("21:40+00:00".into()),
            WireValue::Time("21:40".into()),
            WireValue::LocalDateTime("20150721T21:40".into()),
            WireValue::LocalDateTime("2015-07-21T24:00".into()),
            WireValue::Duration("P14M".into()),
            WireValue::Duration("PT70S".into()),
        ] {
            assert_eq!(bind(value.clone()), Err("t".into()), "{value:?}");
        }
    }

    #[test]
    fn arguments_without_a_parameter_type_are_refused_by_name() {
        let error = parameters(&[("who".into(), WireValue::Vertex(1))], None).unwrap_err();
        assert_eq!(error.parameter, "who");
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

    #[test]
    fn execute_wire_bulk_lists_reach_native_binding_above_scalar_operand_bounds() {
        use fgdb_protocol::body::{Body, Execute, ExecuteMode};

        let wire = Execute {
            mode: ExecuteMode::Write,
            statement: "UNWIND $rows AS row CREATE (:Node {p:row.p,q:row.q})".into(),
            parameters: vec![(
                "rows".into(),
                WireValue::List(
                    (0..5_000)
                        .map(|id| {
                            WireValue::Map(vec![
                                ("p".into(), WireValue::Int(id)),
                                ("q".into(), WireValue::Int(id * 2)),
                            ])
                        })
                        .collect(),
                ),
            )],
        };
        let bytes = wire.encode().unwrap();
        assert!(bytes.len() > fgdb_gql::algebra::MAX_SCALAR_PREDICATE_BYTES);
        let decoded = Execute::decode(&bytes).unwrap();
        assert_eq!(decoded, wire);
        let bound = parameters(&decoded.parameters, None).unwrap();
        let Some(GqlParameterValue::List(list)) = bound.get("rows") else {
            panic!("wire row list must retain its native parameter type")
        };
        assert_eq!(list.values().len(), 5_000);
        assert_eq!(graph(list.value()), decoded.parameters[0].1);
        assert!(bound.canonical_byte_len() > fgdb_gql::algebra::MAX_SCALAR_PREDICATE_BYTES);
        assert_eq!(bound.canonical_byte_len(), bound.canonical_bytes().len());
    }

    #[test]
    fn unsigned_operands_preserve_their_domain_and_do_not_become_nested_scalars() {
        let bound = parameters(
            &[
                ("cut".into(), WireValue::Count(u64::MAX)),
                ("signed".into(), WireValue::Int(-1)),
            ],
            None,
        )
        .unwrap();
        assert_eq!(bound.get("cut"), Some(GqlParameterValue::UInt64(u64::MAX)));
        assert_eq!(bound.get("signed"), Some(GqlParameterValue::Int64(-1)));
        for value in [
            WireValue::List(vec![WireValue::Count(1)]),
            WireValue::Map(vec![("cut".into(), WireValue::Count(1))]),
        ] {
            let error = parameters(&[("nested".into(), value)], None).unwrap_err();
            assert_eq!(error.parameter, "nested");
        }
    }

    #[test]
    fn top_level_maps_preserve_nested_values_and_canonical_key_order() {
        let input = vec![
            (
                "z".into(),
                WireValue::List(vec![WireValue::Bool(true), WireValue::Null]),
            ),
            (
                "a".into(),
                WireValue::Map(vec![("name".into(), WireValue::Text("Ann".into()))]),
            ),
        ];
        let parameters = parameters(&[("payload".into(), WireValue::Map(input))], None).unwrap();
        let Some(GqlParameterValue::Map(map)) = parameters.get("payload") else {
            panic!("a wire map binds as a map parameter");
        };
        assert_eq!(
            graph(map.value()),
            WireValue::Map(vec![
                (
                    "a".into(),
                    WireValue::Map(vec![("name".into(), WireValue::Text("Ann".into()))])
                ),
                (
                    "z".into(),
                    WireValue::List(vec![WireValue::Bool(true), WireValue::Null])
                ),
            ])
        );
        assert_eq!(
            parameters.canonical_bytes().len(),
            parameters.canonical_byte_len()
        );
    }

    #[test]
    fn map_argument_refusals_never_coerce_identities_or_expose_nested_values() {
        for fields in [
            vec![("private".into(), WireValue::Vertex(9))],
            vec![("private".into(), WireValue::Float(f64::NAN))],
            vec![
                ("private".into(), WireValue::Null),
                ("private".into(), WireValue::Null),
            ],
        ] {
            let error =
                parameters(&[("payload".into(), WireValue::Map(fields))], None).unwrap_err();
            assert_eq!(error.parameter, "payload");
            assert!(!format!("{error}").contains("private"));
        }
    }
}

#[cfg(test)]
mod decimal_tests;
