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
        parse_types("float\tx").is_err(),
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
