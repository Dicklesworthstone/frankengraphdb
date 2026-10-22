use super::*;

#[test]
fn numeric_flags_are_decimal_bounded_and_single_use() {
    let mut options = CsvOptions::default();
    options
        .set("--max-changes", "2")
        .map_err(|error| error.message)
        .unwrap();
    assert_eq!(options.max_changes, Some(2));
    assert_eq!(options.set("--max-changes", "3").err().unwrap().code, 2);
    for bad in ["", "0", "-1", "+1", "1.5", " 1", "1000001"] {
        let mut fresh = CsvOptions::default();
        let error = fresh.set("--max-changes", bad).err().unwrap();
        assert_eq!(error.code, 2, "{bad:?} must be a usage refusal");
    }
    let mut bytes = CsvOptions::default();
    let hard = CsvParameterLimits::HARD.max_input_bytes as u64;
    bytes
        .set("--max-input-bytes", &hard.to_string())
        .map_err(|error| error.message)
        .unwrap();
    let mut over = CsvOptions::default();
    assert!(
        over.set("--max-input-bytes", &(hard + 1).to_string())
            .is_err()
    );
}

#[test]
fn file_flags_are_single_use_and_unknown_flags_refuse() {
    let mut options = CsvOptions::default();
    options
        .set("--query-file", "a.gql")
        .map_err(|error| error.message)
        .unwrap();
    assert_eq!(options.query_file(), Some(Path::new("a.gql")));
    assert!(options.set("--query-file", "b.gql").is_err());
    options
        .set("--types-file", "types")
        .map_err(|error| error.message)
        .unwrap();
    assert!(options.set("--types-file", "types").is_err());
    assert!(options.set("--csv-file", "x").is_err());
}

#[test]
fn type_declarations_use_the_closed_vocabulary_and_native_name_rules() {
    let declarations = parse_types("int64\ta\nuint64\tb\nint\tc\ntext\td\nbool\te\nnull\tf\n")
        .map_err(|error| error.message)
        .unwrap();
    assert_eq!(declarations.len(), 6);
    assert_eq!(
        declarations[3],
        ("d", GqlParameterType::Scalar(CanonicalScalarKind::Text))
    );
    assert!(
        parse_types("")
            .map_err(|error| error.message)
            .unwrap()
            .is_empty()
    );
    assert!(
        parse_types("timestamp\tx").is_err(),
        "no inferred or foreign kinds"
    );
    assert!(
        parse_types("text x").is_err(),
        "a TAB separates kind and name"
    );
    assert!(
        parse_types("text\tx\nint\tx").is_err(),
        "duplicate names refuse"
    );
}

fn scalar_script(declarations: &str) -> PreparedGraphWriteScript {
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    let declarations = parse_types(declarations).unwrap();
    PreparedGraphWriteScript::prepare_with_parameter_types(
        "CREATE (n {f:$f,d:$d,b:$b})",
        RelationId(1),
        &declarations,
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Property, "f") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "d") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            (GraphSymbolKind::Property, "b") => Some(GraphSymbol::Property(PropertyKeyId(3))),
            _ => None,
        },
    )
    .unwrap()
}

const SCALAR_TYPES: &str = "float\tf\ndecimal\td\nbytes\tb\n";

#[test]
fn scalar_declarations_select_exact_native_types_without_inference() {
    let declared = parse_types(SCALAR_TYPES).unwrap();
    assert_eq!(
        declared,
        [
            ("f", GqlParameterType::Scalar(CanonicalScalarKind::Float)),
            ("d", GqlParameterType::Scalar(CanonicalScalarKind::Decimal)),
            ("b", GqlParameterType::Scalar(CanonicalScalarKind::Bytes)),
        ]
    );
    for bad in [
        "float32\tx",
        "decimal128\tx",
        "vector\tx",
        "bytes\tx\nfloat\tx",
    ] {
        assert_eq!(parse_types(bad).unwrap_err().code, 2, "{bad}");
    }
    // Undeclared stored parameters remain integers. Decimal-looking input does
    // not silently choose a different schema, even when every row is numeric.
    let input = b"f,d,b\n1.25,0.1,00\n";
    assert!(
        bind_source(
            &scalar_script(""),
            &mut input.as_slice(),
            DEFAULT_INPUT_BYTES,
            || Ok(()),
        )
        .is_err()
    );
}

#[test]
fn cli_scalar_declarations_bind_the_same_program_as_explicit_native_values() {
    use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalScalar};
    let script = scalar_script(SCALAR_TYPES);
    let input = "\u{feff}b,d,f\r\n00AaFf,9007199254740993.000000000000000001,1.25e2\r\n\"\",-0.000,5e-324\r\n\\N,\\N,\\N\r\n";
    let expected = [
        GqlParameters::new()
            .with_scalar("f", CanonicalScalar::Float(CanonicalF64::new(125.0)))
            .unwrap()
            .with_scalar(
                "d",
                CanonicalScalar::Decimal(
                    CanonicalDecimal::from_coefficient(
                        9_007_199_254_740_993_000_000_000_000_000_001,
                    )
                    .unwrap(),
                ),
            )
            .unwrap()
            .with_scalar("b", CanonicalScalar::bytes(vec![0, 170, 255]).unwrap())
            .unwrap(),
        GqlParameters::new()
            .with_scalar(
                "f",
                CanonicalScalar::Float(CanonicalF64::new(f64::from_bits(1))),
            )
            .unwrap()
            .with_scalar(
                "d",
                CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(0).unwrap()),
            )
            .unwrap()
            .with_scalar("b", CanonicalScalar::bytes(Vec::new()).unwrap())
            .unwrap(),
        GqlParameters::new()
            .with_null("f")
            .unwrap()
            .with_null("d")
            .unwrap()
            .with_null("b")
            .unwrap(),
    ];
    let expected = script
        .bind_parameter_sets_with_limit(&expected, 64)
        .unwrap()
        .program()
        .canonical_bytes();
    // Force the actual CLI source adapter through one-byte reads, including
    // within the UTF-8 BOM, CRLF terminators, exponent, and quoted empty bytes.
    struct OneByte<'a>(&'a [u8]);
    impl Read for OneByte<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let count = out.len().min(1);
            self.0.read(&mut out[..count])
        }
    }
    let (actual, records) = bind_source(
        &script,
        &mut OneByte(input.as_bytes()),
        DEFAULT_INPUT_BYTES,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(records, 3);
    assert_eq!(actual.canonical_bytes(), expected);
}

#[test]
fn malformed_late_scalar_fields_refuse_without_returning_a_bound_prefix() {
    let script = scalar_script(SCALAR_TYPES);
    for last in [
        "NaN,0.2,ff",
        "1e309,0.2,ff",
        "+1.5,0.2,ff",
        "2.5,0.0000000000000000001,ff",
        "2.5,10000000000000000,ff",
        "2.5,1e2,ff",
        "2.5,0.2,0",
        "2.5,0.2,0x12",
        "2.5,0.2,private_payload",
        "\"\\N\",0.2,ff",
        "2.5,\"\\N\",ff",
        "2.5,0.2,\"\\N\"",
    ] {
        let input = format!("f,d,b\n1.25,0.1,00\n{last}\n");
        let error = bind_source(&script, &mut input.as_bytes(), DEFAULT_INPUT_BYTES, || {
            Ok(())
        })
        .expect_err("malformed input must not yield an executable prefix");
        assert_eq!(error.code, 3, "{last}: {}", error.message);
        assert!(error.message.contains("InvalidValue"), "{}", error.message);
        assert!(!error.message.contains("private_payload"));
    }
}

#[test]
fn new_scalar_values_keep_the_source_bound_and_cancellation_contract() {
    let script = scalar_script(SCALAR_TYPES);
    let input = b"f,d,b\n1.25,0.1,0000803f\n";
    let (program, records) = bind_source(
        &script,
        &mut input.as_slice(),
        input.len() as u64,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(records, 1);
    assert_eq!(program.statements().len(), 1);
    let error = bind_source(
        &script,
        &mut input.as_slice(),
        input.len() as u64 - 1,
        || Ok(()),
    )
    .err()
    .unwrap();
    assert_eq!(error.code, 3);
    let mut source = std::io::Cursor::new(input);
    let error = bind_source(&script, &mut source, DEFAULT_INPUT_BYTES, || {
        Err(Failure::io("cancelled"))
    })
    .err()
    .unwrap();
    assert_eq!(source.position(), 0);
    assert_eq!(error.code, 5);
    assert_eq!(error.message, "cancelled");
}

#[test]
fn record_chain_text_reads_parameters_from_the_current_record_only_outside_literals() {
    let (text, names) = record_chain_text(
        "MERGE (a:N {id: $s, note: '$s \\'quoted\\' $d'}) // $ignored\n\
         MERGE (b:`$N` {id: $d}) /* $also */ MERGE (a)-[:E {w: $s}]->(b)",
    )
    .unwrap();
    assert_eq!(
        text,
        "UNWIND $__csv_rows AS __csv_row MERGE (a:N {id: __csv_row.s, note: '$s \\'quoted\\' $d'}) \
         // $ignored\nMERGE (b:`$N` {id: __csv_row.d}) /* $also */ \
         MERGE (a)-[:E {w: __csv_row.s}]->(b)"
    );
    // First appearance order, each name once.
    assert_eq!(names, ["s", "d"]);
    // UTF-8 outside the rewritten parameters is copied unchanged.
    let (text, names) =
        record_chain_text("MERGE (a:N {name: '雪', id: $雪id})").unwrap_or_default();
    assert!(
        text.is_empty() && names.is_empty(),
        "a non-ASCII name is not a parameter here"
    );
    let (text, _) = record_chain_text("MERGE (a:N {name: '雪', id: $id})").unwrap();
    assert!(
        text.ends_with("MERGE (a:N {name: '雪', id: __csv_row.id})"),
        "{text}"
    );
}

#[test]
fn record_chain_text_refuses_a_name_clash_or_a_bare_dollar() {
    for statement in [
        "MERGE (__csv_row:N {id: $s}) MERGE (b:N {id: $d})",
        "MERGE (a:N {id: $}) MERGE (b:N {id: $d})",
        "MERGE (a:N {id: $1}) MERGE (b:N {id: $d})",
    ] {
        assert!(record_chain_text(statement).is_none(), "{statement}");
    }
}
