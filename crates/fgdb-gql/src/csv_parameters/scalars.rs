//! Explicit CSV representations for the non-integral scalar lanes.
//!
//! Floats use finite decimal binary64 parsing. Decimals are exact scale-18
//! values: excess nonzero fractional digits refuse, never round. Bytes use
//! an even number of hexadecimal digits, with no prefix. Empty bytes are not
//! NULL. The parent decoder owns quote-sensitive NULL and all CSV byte caps.

use super::CsvParameterErrorKind;
use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalScalar, CanonicalScalarKind};

pub(super) fn decode(
    kind: CanonicalScalarKind,
    text: &str,
) -> Result<CanonicalScalar, CsvParameterErrorKind> {
    let invalid = || CsvParameterErrorKind::InvalidValue;
    match kind {
        CanonicalScalarKind::Float => {
            // Rust's parser admits leading '+' and nonfinite spellings; CSV
            // deliberately admits neither. No whitespace is silently trimmed.
            if text.is_empty() || text.starts_with('+') || text.trim() != text {
                return Err(invalid());
            }
            let value: f64 = text.parse().map_err(|_| invalid())?;
            if !value.is_finite() {
                return Err(invalid());
            }
            Ok(CanonicalScalar::Float(CanonicalF64::new(value)))
        }
        CanonicalScalarKind::Decimal => decimal(text).ok_or_else(invalid),
        CanonicalScalarKind::Bytes => bytes(text),
        _ => Err(CsvParameterErrorKind::UnsupportedType),
    }
}

fn decimal(text: &str) -> Option<CanonicalScalar> {
    let negative = text.starts_with('-');
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) if !fraction.is_empty() => (whole, fraction),
        Some(_) => return None,
        None => (unsigned, ""),
    };
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    // Trailing zeros carry no extra precision. An exact spelling with more
    // than 18 fractional places is legal only when those extra places are zero.
    let fraction = fraction.trim_end_matches('0');
    let scale = CanonicalDecimal::scale();
    let digits = u32::try_from(fraction.len()).ok()?;
    let shift = scale.checked_sub(digits)?;
    let integral = whole
        .parse::<i128>()
        .ok()?
        .checked_mul(10_i128.pow(scale))?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .ok()?
            .checked_mul(10_i128.pow(shift))?
    };
    let coefficient = integral.checked_add(fractional)?;
    let coefficient = if negative {
        coefficient.checked_neg()?
    } else {
        coefficient
    };
    CanonicalDecimal::from_coefficient(coefficient)
        .ok()
        .map(CanonicalScalar::Decimal)
}

fn bytes(text: &str) -> Result<CanonicalScalar, CsvParameterErrorKind> {
    // Validate the complete representation and its decoded bound before any
    // payload allocation. A malformed late byte must not allocate a prefix.
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CsvParameterErrorKind::InvalidValue);
    }
    let len = text.len() / 2;
    if len > fgdb_types::MAX_SCALAR_PAYLOAD {
        return Err(CsvParameterErrorKind::ParameterRejected);
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(len)
        .map_err(|_| CsvParameterErrorKind::ParameterRejected)?;
    let nibble = |byte: u8| {
        if byte.is_ascii_digit() {
            byte - b'0'
        } else {
            byte.to_ascii_lowercase() - b'a' + 10
        }
    };
    for pair in text.as_bytes().as_chunks::<2>().0 {
        decoded.push((nibble(pair[0]) << 4) | nibble(pair[1]));
    }
    CanonicalScalar::bytes(decoded).map_err(|_| CsvParameterErrorKind::ParameterRejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csv_parameters::{CsvParameterLimit, CsvParameterLimits, decode_csv_parameters};
    use crate::{GqlParameterSpec, GqlParameterType, GqlParameters, PreparedGraphWriteScript};
    use fgdb_delta_types::{PropertyKeyId, RelationId};

    fn schema(kind: CanonicalScalarKind) -> [GqlParameterSpec; 1] {
        [GqlParameterSpec {
            name: "v".into(),
            parameter_type: GqlParameterType::Scalar(kind),
            requires_positive: false,
            occurrences: 1,
        }]
    }

    fn decoded(kind: CanonicalScalarKind, text: &str) -> GqlParameters {
        decode_csv_parameters(
            &format!("v\n{text}"),
            &schema(kind),
            CsvParameterLimits::default(),
        )
        .unwrap()
        .remove(0)
    }

    #[test]
    fn float_values_use_native_binary64_and_canonical_zero() {
        for (text, expected) in [
            ("0.125", 0.125),
            ("-0", 0.0),
            ("1.25e+2", 125.0),
            ("-2.5E-3", -0.0025),
            ("5e-324", f64::from_bits(1)),
            ("1.7976931348623157e308", f64::MAX),
        ] {
            assert_eq!(
                decoded(CanonicalScalarKind::Float, text),
                GqlParameters::new()
                    .with_scalar("v", CanonicalScalar::Float(CanonicalF64::new(expected)))
                    .unwrap(),
            );
        }
        for text in [
            "NaN", "nan", "inf", "-inf", "Infinity", "1e309", "+1.5", " 1", "1 ", "1_0", "0x10", "",
        ] {
            assert_eq!(
                decode(CanonicalScalarKind::Float, text),
                Err(CsvParameterErrorKind::InvalidValue),
                "{text:?}",
            );
        }
    }

    #[test]
    fn decimals_are_exact_and_never_round_through_binary_float() {
        for (text, coefficient) in [
            ("0.1", 100_000_000_000_000_000),
            ("-12.5000", -12_500_000_000_000_000_000),
            ("0.000000000000000001", 1),
            ("-0.000000000000000001000", -1),
            ("-0.000000000000000000000", 0),
            (
                "9999999999999999.999999999999999999",
                fgdb_types::MAX_DECIMAL_COEFFICIENT,
            ),
            (
                "-9999999999999999.999999999999999999",
                fgdb_types::MIN_DECIMAL_COEFFICIENT,
            ),
        ] {
            let expected =
                CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(coefficient).unwrap());
            assert_eq!(
                decoded(CanonicalScalarKind::Decimal, text),
                GqlParameters::new().with_scalar("v", expected).unwrap(),
            );
        }
        for text in [
            "0.0000000000000000001",
            "1.0000000000000000005",
            "10000000000000000",
            "-10000000000000000",
            "1e2",
            "+1",
            ".5",
            "1.",
            "1.2.3",
            "NaN",
            " 1",
            "",
            "9999999999999999999999999999999999999999999",
        ] {
            assert_eq!(
                decode(CanonicalScalarKind::Decimal, text),
                Err(CsvParameterErrorKind::InvalidValue),
                "{text:?}",
            );
        }
    }

    #[test]
    fn bytes_preserve_embedding_payloads_and_empty_is_not_null() {
        let packed: Vec<u8> = [0.25_f32, -1.5, 3.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let hex = packed
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        assert_eq!(
            decoded(CanonicalScalarKind::Bytes, &hex),
            GqlParameters::new()
                .with_scalar("v", CanonicalScalar::bytes(packed).unwrap())
                .unwrap(),
        );
        assert_eq!(
            decoded(CanonicalScalarKind::Bytes, "\"\""),
            GqlParameters::new()
                .with_scalar("v", CanonicalScalar::bytes(Vec::new()).unwrap())
                .unwrap(),
        );
        assert_eq!(
            decode(CanonicalScalarKind::Bytes, "00aAFf").unwrap(),
            CanonicalScalar::bytes(vec![0, 170, 255]).unwrap(),
        );
        for text in ["0", "abc", "0x12", "00zz", "ff ff", "é", "\n"] {
            assert_eq!(
                decode(CanonicalScalarKind::Bytes, text),
                Err(CsvParameterErrorKind::InvalidValue),
            );
        }
    }

    #[test]
    fn quote_sensitive_null_and_redacted_late_failure_hold_for_every_new_type() {
        for (kind, value) in [
            (CanonicalScalarKind::Float, "1.5"),
            (CanonicalScalarKind::Decimal, "1.5"),
            (CanonicalScalarKind::Bytes, "ab12"),
        ] {
            assert_eq!(
                decoded(kind, "\\N"),
                GqlParameters::new().with_null("v").unwrap(),
            );
            assert!(
                decode_csv_parameters("v\n\"\\N\"", &schema(kind), CsvParameterLimits::default(),)
                    .is_err()
            );
            let error = decode_csv_parameters(
                &format!("v\n{value}\nsecret_payload"),
                &schema(kind),
                CsvParameterLimits::default(),
            )
            .unwrap_err();
            assert_eq!(error.record, 2);
            assert_eq!(error.column, Some(0));
            assert_eq!(error.offset, 3 + value.len());
            assert_eq!(error.kind, CsvParameterErrorKind::InvalidValue);
            assert!(!format!("{error:?} {error}").contains("secret_payload"));
        }
    }

    #[test]
    fn new_scalars_spend_existing_field_and_cumulative_transcript_caps() {
        for (kind, value) in [
            (CanonicalScalarKind::Float, "1.25"),
            (CanonicalScalarKind::Decimal, "1.25"),
            (CanonicalScalarKind::Bytes, "abcd"),
        ] {
            let input = format!("v\n{value}\n{value}");
            let schema = schema(kind);
            let rows =
                decode_csv_parameters(&input, &schema, CsvParameterLimits::default()).unwrap();
            let total = rows.iter().map(GqlParameters::canonical_byte_len).sum();
            let limits = CsvParameterLimits {
                max_field_bytes: value.len(),
                max_parameter_bytes: total,
                ..CsvParameterLimits::default()
            };
            assert_eq!(
                decode_csv_parameters(&input, &schema, limits).unwrap(),
                rows
            );
            let error = decode_csv_parameters(
                &input,
                &schema,
                CsvParameterLimits {
                    max_parameter_bytes: total - 1,
                    ..limits
                },
            )
            .unwrap_err();
            assert_eq!(error.record, 2);
            assert!(matches!(
                error.kind,
                CsvParameterErrorKind::Limit {
                    dimension: CsvParameterLimit::ParameterBytes,
                    ..
                }
            ));
            let error = decode_csv_parameters(
                &input,
                &schema,
                CsvParameterLimits {
                    max_field_bytes: value.len() - 1,
                    ..limits
                },
            )
            .unwrap_err();
            assert!(matches!(
                error.kind,
                CsvParameterErrorKind::Limit {
                    dimension: CsvParameterLimit::FieldBytes,
                    ..
                }
            ));
        }
    }

    fn script() -> PreparedGraphWriteScript {
        use crate::{GraphSymbol, GraphSymbolKind};
        let types = [
            ("f", GqlParameterType::Scalar(CanonicalScalarKind::Float)),
            ("d", GqlParameterType::Scalar(CanonicalScalarKind::Decimal)),
            ("b", GqlParameterType::Scalar(CanonicalScalarKind::Bytes)),
        ];
        PreparedGraphWriteScript::prepare_with_parameter_types(
            "CREATE (n {f:$f,d:$d,b:$b})",
            RelationId(1),
            &types,
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Property, "f") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                (GraphSymbolKind::Property, "d") => Some(GraphSymbol::Property(PropertyKeyId(2))),
                (GraphSymbolKind::Property, "b") => Some(GraphSymbol::Property(PropertyKeyId(3))),
                _ => None,
            },
        )
        .unwrap()
    }

    #[test]
    fn streaming_and_eager_binding_match_explicit_native_arguments_at_every_split() {
        let script = script();
        let input = "\u{feff}b,f,d\r\n\"0000803f\",1.25,-2.500\r\n\\N,\\N,0.000000000000000001\r\n";
        let arguments = [
            GqlParameters::new()
                .with_scalar("f", CanonicalScalar::Float(CanonicalF64::new(1.25)))
                .unwrap()
                .with_scalar(
                    "d",
                    CanonicalScalar::Decimal(
                        CanonicalDecimal::from_coefficient(-2_500_000_000_000_000_000).unwrap(),
                    ),
                )
                .unwrap()
                .with_scalar(
                    "b",
                    CanonicalScalar::bytes(1_f32.to_le_bytes().to_vec()).unwrap(),
                )
                .unwrap(),
            GqlParameters::new()
                .with_null("f")
                .unwrap()
                .with_null("b")
                .unwrap()
                .with_scalar(
                    "d",
                    CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(1).unwrap()),
                )
                .unwrap(),
        ];
        let expected = script
            .bind_parameter_sets_with_limit(&arguments, 64)
            .unwrap()
            .program()
            .canonical_bytes();
        assert_eq!(
            script
                .bind_csv(input, CsvParameterLimits::default())
                .unwrap()
                .program()
                .canonical_bytes(),
            expected
        );
        for split in 1..input.len() {
            let mut at = 0;
            let (actual, records) = script
                .bind_csv_stream_controlled(
                    CsvParameterLimits::default(),
                    64,
                    |buffer| -> Result<usize, ()> {
                        let boundary = if at < split { split } else { input.len() };
                        let n = buffer.len().min(boundary - at);
                        buffer[..n].copy_from_slice(&input.as_bytes()[at..at + n]);
                        at += n;
                        Ok(n)
                    },
                    || Ok(()),
                )
                .unwrap();
            assert_eq!(records, 2);
            assert_eq!(actual.canonical_bytes(), expected, "split {split}");
        }
    }

    #[test]
    fn streaming_interruption_and_bad_late_values_never_return_a_program_prefix() {
        use crate::csv_write_script::GraphWriteScriptCsvStreamError;
        use std::io::Read;
        let script = script();
        let input = b"f,d,b\n1.25,0.1,00\n2.5,0.2,ff\n";
        let mut calls = 0;
        let mut source = input.as_slice();
        let (_, records) = script
            .bind_csv_stream_controlled(
                CsvParameterLimits::default(),
                64,
                |buffer| source.read(buffer).map_err(|_| ()),
                || {
                    calls += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(records, 2);
        for stop in 0..calls {
            let mut at = 0;
            let mut source = input.as_slice();
            let result = script.bind_csv_stream_controlled(
                CsvParameterLimits::default(),
                64,
                |buffer| source.read(buffer).map_err(|_| ()),
                || {
                    let current = at;
                    at += 1;
                    if current == stop { Err(()) } else { Ok(()) }
                },
            );
            assert!(matches!(
                result,
                Err(GraphWriteScriptCsvStreamError::Interrupted(()))
            ));
        }
        for last in ["NaN,0.2,ff", "2.5,0.0000000000000000001,ff", "2.5,0.2,fg"] {
            let input = format!("f,d,b\n1.25,0.1,00\n{last}\n");
            let mut source = input.as_bytes();
            let result = script.bind_csv_stream_controlled(
                CsvParameterLimits::default(),
                64,
                |buffer| source.read(buffer).map_err(|_| ()),
                || Ok(()),
            );
            let error = match result.expect_err("invalid CSV must not bind") {
                GraphWriteScriptCsvStreamError::Csv(error) => Some(error),
                _ => None,
            }
            .expect("the late value must be refused by the CSV decoder");
            assert_eq!(error.record, 2);
            assert_eq!(error.kind, CsvParameterErrorKind::InvalidValue);
        }
    }
}
