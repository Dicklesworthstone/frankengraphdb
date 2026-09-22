use super::*;

fn decode(
    input: &[u8],
    width: usize,
    limits: CsvRecordLimits,
) -> Result<Vec<CsvRecord>, CsvRecordError> {
    let mut decoder = CsvRecordDecoder::new(limits);
    let mut rows = Vec::new();
    for part in input.chunks(width) {
        let mut remaining = part;
        while !remaining.is_empty() {
            let (used, row) = decoder.push(remaining)?;
            assert!(used > 0 && used <= remaining.len());
            remaining = &remaining[used..];
            if let Some(row) = row {
                rows.push(row);
            }
        }
    }
    if let Some(row) = decoder.finish()? {
        rows.push(row);
    }
    assert_eq!(decoder.finish()?, None);
    Ok(rows)
}
fn texts(rows: &[CsvRecord]) -> Vec<Vec<&str>> {
    rows.iter()
        .map(|row| row.fields().iter().map(CsvField::text).collect())
        .collect()
}

#[test]
fn every_chunk_width_preserves_multiline_utf8_quotes_crlf_and_quote_provenance() {
    let source =
        b"a,b,c\r\n\"one, two\",\"a\"\"b\",\"\"\n,\\N,\"\\N\"\r\n\"line\r\nline\nend\",\"\r\",last";
    let expected = vec![
        vec!["a", "b", "c"],
        vec!["one, two", "a\"b", ""],
        vec!["", "\\N", "\\N"],
        vec!["line\r\nline\nend", "\r", "last"],
    ];
    for width in 1..=source.len() {
        let rows = decode(source, width, CsvRecordLimits::default()).unwrap();
        assert_eq!(texts(&rows), expected);
        assert!(!rows[2].fields()[1].is_quoted());
        assert!(rows[2].fields()[2].is_quoted());
        assert!(rows[1].fields()[2].is_quoted());
    }
    let utf8 = "\"雪🙂λ\",🙂\n".as_bytes();
    for width in 1..=utf8.len() {
        assert_eq!(
            texts(&decode(utf8, width, CsvRecordLimits::default()).unwrap()),
            vec![vec!["雪🙂λ", "🙂"]]
        );
    }
}

#[test]
fn blank_records_trailing_empty_fields_and_eof_are_distinct() {
    for (source, expected) in [
        ("", vec![]),
        ("\n", vec![vec![""]]),
        ("\r\n", vec![vec![""]]),
        (",", vec![vec!["", ""]]),
        ("\"\"", vec![vec![""]]),
        ("x,\n\n", vec![vec!["x", ""], vec![""]]),
    ] {
        assert_eq!(
            texts(&decode(source.as_bytes(), 1, CsvRecordLimits::default()).unwrap()),
            expected
        );
    }
    // A decoder does not normalize or silently discard an embedded BOM.
    let rows = decode("\u{feff}x\n".as_bytes(), 1, CsvRecordLimits::default()).unwrap();
    assert_eq!(rows[0].fields()[0].text(), "\u{feff}x");
}

#[test]
fn malformed_source_never_exposes_the_unfinished_record_and_remains_terminal() {
    for (source, kind) in [
        (
            b"a,\"open".as_slice(),
            CsvRecordErrorKind::UnterminatedQuote,
        ),
        (b"a,b\"c\n", CsvRecordErrorKind::UnexpectedQuote),
        (b"a,\"b\"tail\n", CsvRecordErrorKind::TrailingCharacters),
        (b"a,b\rX", CsvRecordErrorKind::InvalidLineEnding),
        (b"a,b\r", CsvRecordErrorKind::InvalidLineEnding),
        (b"a,\xff\n", CsvRecordErrorKind::InvalidUtf8),
        (b"a,\"\xe9\"\n", CsvRecordErrorKind::InvalidUtf8),
    ] {
        for width in 1..=source.len() {
            assert_eq!(
                decode(source, width, CsvRecordLimits::default())
                    .unwrap_err()
                    .kind,
                kind
            );
        }
        let mut decoder = CsvRecordDecoder::new(CsvRecordLimits::default());
        let error = match decoder.push(source) {
            Err(error) => error,
            Ok((_, None)) => decoder.finish().unwrap_err(),
            Ok((_, Some(_))) => panic!("unfinished record escaped"),
        };
        assert_eq!(error.kind, kind);
        assert!(decoder.field.is_empty() && decoder.fields.is_empty());
        assert_eq!(
            decoder.push(b"safe\n").unwrap_err().kind,
            CsvRecordErrorKind::Terminated
        );
        assert_eq!(
            decoder.finish().unwrap_err().kind,
            CsvRecordErrorKind::Terminated
        );
    }
}

#[test]
fn exact_inclusive_limits_count_decoded_bytes_and_complete_raw_records() {
    let source = "\"λ\"\"\",x\r\n";
    let exact = CsvRecordLimits {
        max_record_bytes: source.len(),
        max_field_bytes: 3,
        max_columns: 2,
    };
    assert_eq!(
        texts(&decode(source.as_bytes(), 1, exact).unwrap()),
        vec![vec!["λ\"", "x"]]
    );
    for (limits, kind) in [
        (
            CsvRecordLimits {
                max_record_bytes: exact.max_record_bytes - 1,
                ..exact
            },
            CsvRecordErrorKind::RecordBytes {
                limit: exact.max_record_bytes - 1,
            },
        ),
        (
            CsvRecordLimits {
                max_field_bytes: 2,
                ..exact
            },
            CsvRecordErrorKind::FieldBytes { limit: 2 },
        ),
        (
            CsvRecordLimits {
                max_columns: 1,
                ..exact
            },
            CsvRecordErrorKind::Columns { limit: 1 },
        ),
        (
            CsvRecordLimits {
                max_columns: 0,
                ..exact
            },
            CsvRecordErrorKind::Columns { limit: 0 },
        ),
    ] {
        assert_eq!(
            decode(source.as_bytes(), source.len(), limits)
                .unwrap_err()
                .kind,
            kind
        );
    }
    let empty = CsvRecordLimits {
        max_record_bytes: 1,
        max_field_bytes: 0,
        max_columns: 1,
    };
    assert!(
        decode(b"\n\n", 2, empty).is_ok(),
        "quota resets at a complete record only"
    );
    assert!(decode(b"x", 1, empty).is_err());
    assert!(
        decode(
            b"",
            1,
            CsvRecordLimits {
                max_record_bytes: 0,
                ..empty
            }
        )
        .is_ok()
    );
    assert!(
        decode(
            b"\n",
            1,
            CsvRecordLimits {
                max_record_bytes: 0,
                ..empty
            }
        )
        .is_err()
    );
}

#[test]
fn prefix_delivery_stops_at_the_exact_boundary_and_error_coordinates_are_absolute() {
    let mut decoder = CsvRecordDecoder::new(CsvRecordLimits::default());
    let (used, row) = decoder.push(b"a,b\r\nx,\"bad").unwrap();
    assert_eq!(used, 5);
    assert_eq!(texts(&[row.unwrap()]), vec![vec!["a", "b"]]);
    assert_eq!(
        decoder.position(),
        CsvPosition {
            record: 1,
            column: 0,
            offset: 5
        }
    );
    assert_eq!(decoder.push(b"x,\"bad").unwrap(), (6, None));
    let error = decoder.finish().unwrap_err();
    assert_eq!(
        error.position,
        CsvPosition {
            record: 1,
            column: 1,
            offset: 11
        }
    );
    assert_eq!(error.kind, CsvRecordErrorKind::UnterminatedQuote);
}

#[test]
fn framer_matches_decoder_for_exhaustive_small_ascii_inputs() {
    let alphabet = *b"a,\"\r\n";
    for length in 0..=6u32 {
        for mut code in 0..alphabet.len().pow(length) {
            let mut source = Vec::new();
            for _ in 0..length {
                source.push(alphabet[code % alphabet.len()]);
                code /= alphabet.len();
            }
            let mut framer = CsvRecordFramer::new(CsvRecordLimits::default());
            let framed = (|| {
                let mut records = 0;
                for &byte in &source {
                    records += usize::from(framer.push(byte)?);
                }
                records += usize::from(framer.finish()?);
                Ok::<_, CsvRecordError>(records)
            })();
            let decoded = decode(&source, 1, CsvRecordLimits::default());
            match (framed, decoded) {
                (Ok(count), Ok(rows)) => assert_eq!(count, rows.len(), "{source:?}"),
                (Err(a), Err(b)) => assert_eq!(a, b, "{source:?}"),
                mismatch => panic!("framing/decoding disagree for {source:?}: {mismatch:?}"),
            }
        }
    }
}

#[test]
fn decoding_matches_existing_typed_csv_profile_without_interpolating_values() {
    use crate::csv_parameters::{CsvParameterLimits, decode_csv_parameters};
    use crate::{GqlParameterSpec, GqlParameterType, GqlParameters};
    use fgdb_types::CanonicalScalarKind;
    let schema = [GqlParameterSpec {
        name: "value".into(),
        parameter_type: GqlParameterType::Scalar(CanonicalScalarKind::Text),
        requires_positive: false,
        occurrences: 1,
    }];
    for value in [
        "",
        "simple",
        "雪🙂",
        "a,b",
        "a\"b",
        "a\r\nb",
        "\\N",
        "';DELETE n;",
    ] {
        let source = format!("value\n\"{}\"\n", value.replace('"', "\"\""));
        let actual = decode(source.as_bytes(), 1, CsvRecordLimits::default()).unwrap();
        let parameters =
            decode_csv_parameters(&source, &schema, CsvParameterLimits::default()).unwrap();
        assert_eq!(actual.len(), 2);
        assert_eq!(
            parameters,
            vec![
                GqlParameters::new()
                    .with_text("value", actual[1].fields()[0].text())
                    .unwrap()
            ]
        );
    }
}

#[test]
fn debug_and_errors_do_not_reveal_partial_or_completed_payloads() {
    let mut decoder = CsvRecordDecoder::new(CsvRecordLimits::default());
    let (_, record) = decoder.push(b"secret_payload\n").unwrap();
    assert!(!format!("{record:?}").contains("secret_payload"));
    decoder.push(b"secret_payload,\"another_secret").unwrap();
    assert!(!format!("{decoder:?}").contains("secret_payload"));
    assert!(!format!("{decoder:?}").contains("115, 101, 99, 114, 101, 116"));
    let error = decoder.finish().unwrap_err();
    assert!(!format!("{error} {error:?}").contains("secret"));
}
