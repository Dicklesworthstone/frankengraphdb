use super::*;
use crate::{GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalarKind, VId};

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(entries.into_iter().map(|(key, value)| (key.into(), value)).collect())
        .unwrap()
}

fn source(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, |_, _| None)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn script(text: &str, declarations: &[(&str, GqlParameterType)]) -> PreparedGraphWriteScript {
    let mut resolve = |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    };
    PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
        text,
        fgdb_delta_types::RelationId(1),
        declarations,
        &mut resolve,
    )
    .unwrap()
}

fn rows(source: &PreparedGraphSet) -> Vec<GraphValueRow> {
    source.execute_governed(
        GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000),
        |_, _| Err(crate::GqlQueryError::Source(())),
        || Ok::<(), ()>(()),
    ).unwrap().value
}

#[test]
fn lists_and_maps_bind_by_name_to_the_same_native_program_as_explicit_arguments() {
    let read = source(
        "UNWIND [2,1,2] AS x RETURN [x,x+1] AS items, \
         {key:x,nested:{copy:'x'}} AS doc,x AS order_key ORDER BY order_key",
    );
    let write = script(
        "CREATE (n {key:$doc.key}); CREATE (m {key:size($items)})",
        &[("doc", GqlParameterType::Map), ("items", GqlParameterType::List)],
    );
    let prepared = PreparedGraphQueryWrite::prepare(read, write.clone()).unwrap();
    let input = rows(prepared.source());
    let actual = prepared.bind_rows(&input, 6).unwrap().unwrap();
    let arguments = [1, 2, 2].into_iter().map(|key| {
        GqlParameters::new()
            .with_list("items", vec![int(key), int(key + 1)]).unwrap()
            .with_map("doc", vec![
                ("key".into(), int(key)),
                ("nested".into(), map(vec![("copy", GraphValue::Scalar(
                    CanonicalScalar::ucs_basic_text("x").unwrap(),
                ))])),
            ]).unwrap()
    }).collect::<Vec<_>>();
    let expected = write.bind_parameter_sets_with_limit(&arguments, 6).unwrap();
    assert_eq!(actual.program(), expected.program());
    assert_eq!(actual.argument_sets(), 3);
    for row in 0..3 {
        assert_eq!(actual.statement_range(row), expected.statement_range(row));
    }
    assert_eq!(input, rows(prepared.source()));
}

#[test]
fn nullable_collection_arguments_stay_null_instead_of_becoming_empty() {
    let write = script(
        "CREATE (n {key:size($items)}); CREATE (m {key:$doc.key})",
        &[("items", GqlParameterType::List), ("doc", GqlParameterType::Map)],
    );
    let prepared = PreparedGraphQueryWrite::prepare(
        source("RETURN null AS items,null AS doc"), write.clone(),
    ).unwrap();
    let actual = prepared.bind_rows(&rows(prepared.source()), 2).unwrap().unwrap();
    let expected = write.bind_parameters(
        &GqlParameters::new().with_null("items").unwrap().with_null("doc").unwrap(),
    ).unwrap();
    assert_eq!(actual.program(), &expected);
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    for kind in [GqlParameterType::List, GqlParameterType::Map] {
        let value = bind_cell::<()>(&null, kind, "v", 0, 0, 0, &mut |_| Ok(())).unwrap();
        assert_eq!(value.parameter_type(), GqlParameterType::Scalar(CanonicalScalarKind::Null));
    }
}

#[test]
fn static_element_domains_and_late_wrong_collection_shapes_refuse() {
    for (read, kind, write) in [
        ("MATCH (n) RETURN n AS arg", GqlParameterType::List, "CREATE (n {key:size($arg)})"),
        ("MATCH (n) RETURN n AS arg", GqlParameterType::Map, "CREATE (n {key:$arg.key})"),
        ("RETURN [] AS arg", GqlParameterType::Map, "CREATE (n {key:$arg.key})"),
    ] {
        assert!(matches!(
            PreparedGraphQueryWrite::prepare(source(read), script(write, &[("arg", kind)])),
            Err(GraphQueryWriteBuildError::CollectionColumn {
                parameter: 0, column: 0, expected,
            }) if expected == kind
        ));
    }
    let prepared = PreparedGraphQueryWrite::prepare(
        source("UNWIND [{v:[1],i:0},{v:7,i:1}] AS r RETURN r.v AS items,r.i AS idx ORDER BY idx"),
        script("CREATE (n {key:size($items)})", &[("items", GqlParameterType::List)]),
    ).unwrap();
    assert!(matches!(
        prepared.bind_rows(&rows(prepared.source()), 2),
        Err(GraphQueryWriteBindError::CollectionValue {
            row: 1, column: 0, expected: GqlParameterType::List,
        })
    ));
    // A failed binding leaves the reusable definition intact.
    let good = rows(&source("RETURN [9] AS items,0 AS idx"));
    assert_eq!(prepared.bind_rows(&good, 1).unwrap().unwrap().argument_sets(), 1);
}

#[test]
fn native_transcript_forecast_covers_all_domains_and_exact_prefix_boundary() {
    let nested = map(vec![
        ("a", GraphValue::List(vec![int(1), GraphValue::Vertex(VId(u128::MAX))].into())),
        ("z", GraphValue::Scalar(CanonicalScalar::ucs_basic_text("quotes ' $x").unwrap())),
    ]);
    for (cell, expected) in [
        (int(7), GqlParameterType::Int64),
        (int(7), GqlParameterType::UInt64),
        (int(-1), GqlParameterType::UInt64),
        (int(7), GqlParameterType::Scalar(CanonicalScalarKind::Int)),
        (GraphValue::Scalar(CanonicalScalar::Bool(true)), GqlParameterType::Scalar(CanonicalScalarKind::Bool)),
        (GraphValue::Scalar(CanonicalScalar::Null), GqlParameterType::Map),
        (GraphValue::Scalar(CanonicalScalar::bytes(vec![0, 1, 255]).unwrap()), GqlParameterType::Scalar(CanonicalScalarKind::Bytes)),
        (nested.clone(), GqlParameterType::Map),
        (GraphValue::List(vec![nested, int(2)].into()), GqlParameterType::List),
    ] {
        for name in ["v".to_owned(), "v".repeat(64)] {
            let size = entry_bytes(&cell, expected, name.len(), cell.canonical_byte_len().unwrap()).unwrap();
            let mut args = GqlParameters::new();
            let header = args.canonical_byte_len();
            let value = bind_cell::<()>(&cell, expected, &name, 0, 0, header as u128, &mut |_| Ok(())).unwrap();
            args.insert(name.clone(), value).unwrap();
            assert_eq!(args.canonical_byte_len() - header, size);
            assert_eq!(args.canonical_bytes().len(), header + size);
            let limit = crate::parameters::MAX_GQL_PARAMETER_TRANSCRIPT_BYTES as u128;
            bind_cell::<()>(&cell, expected, &name, 0, 0, limit - size as u128, &mut |_| Ok(())).unwrap();
            let mut calls = 0;
            let failure = bind_cell::<()>(&cell, expected, &name, 4, 3, limit - size as u128 + 1, &mut |_| {
                calls += 1;
                Ok(())
            });
            assert!(matches!(failure,
                Err(GraphQueryWriteBindError::ExpandedParametersTooLarge { observed, .. })
                    if observed == limit + 1));
            assert_eq!(calls, 0, "oversize prefixes must refuse before a copy reservation");
        }
    }
}

#[test]
fn malformed_and_overlarge_collections_refuse_before_any_copy() {
    let mut deep = int(1);
    for _ in 0..=GraphValue::MAX_LIST_DEPTH {
        deep = GraphValue::List(vec![deep].into());
    }
    let malformed = [
        GraphValue::Map { keys: vec!["b".into(), "a".into()].into(), values: vec![int(1), int(2)].into() },
        GraphValue::Map { keys: vec!["a".into()].into(), values: Vec::new().into() },
        GraphValue::Map { keys: vec!["a".into(), "a".into()].into(), values: vec![int(1), int(2)].into() },
    ];
    for cell in malformed.into_iter().chain([
        deep, GraphValue::List(vec![int(0); GraphValue::MAX_LIST_NODES].into()),
    ]) {
        let expected = if matches!(cell, GraphValue::Map { .. }) { GqlParameterType::Map } else { GqlParameterType::List };
        let mut calls = 0;
        let result = bind_cell::<()>(&cell, expected, "arg", 5, 2, 0, &mut |_| {
            calls += 1;
            Ok(())
        });
        assert!(matches!(result, Err(GraphQueryWriteBindError::InvalidValue { row: 5, column: 2 })));
        assert_eq!(calls, 0);
    }
    let large_map = GraphValue::map((0..5_000).map(|i| (format!("k{i:04}").into_boxed_str(), int(i))).collect()).unwrap();
    assert!(large_map.validate_bounds());
    let mut calls = 0;
    assert!(matches!(bind_cell::<()>(&large_map, GqlParameterType::Map, "arg", 0, 0, 0, &mut |_| {
        calls += 1;
        Ok(())
    }), Err(GraphQueryWriteBindError::Parameter { source: GqlParameterError::MapLiteral, .. })));
    assert_eq!(calls, 0);
}

#[test]
fn repeated_collection_payloads_cannot_escape_the_whole_batch_transcript_ceiling() {
    let read = source("RETURN range(1,500) AS items");
    let input = rows(&read);
    let prepared = PreparedGraphQueryWrite::prepare(
        read, script("CREATE (n {key:size($items)})", &[("items", GqlParameterType::List)]),
    ).unwrap();
    let cell = &input[0].values()[0];
    let per_row = GqlParameters::new().canonical_byte_len()
        + entry_bytes(cell, GqlParameterType::List, "items".len(), cell.canonical_byte_len().unwrap()).unwrap();
    let count = crate::parameters::MAX_GQL_PARAMETER_TRANSCRIPT_BYTES / per_row;
    let all = vec![input[0].clone(); count + 1];
    assert_eq!(prepared.bind_rows(&all[..count], count).unwrap().unwrap().argument_sets(), count);
    assert!(matches!(prepared.bind_rows(&all, count + 1),
        Err(GraphQueryWriteBindError::ExpandedParametersTooLarge { .. })));
}

#[test]
fn every_collection_copy_and_binding_checkpoint_is_interruptible_without_a_prefix() {
    let prepared = PreparedGraphQueryWrite::prepare(
        source("UNWIND [1,2] AS x RETURN {key:x,nested:[x,{v:x+1}]} AS doc"),
        script("CREATE (n {key:$doc.key})", &[("doc", GqlParameterType::Map)]),
    ).unwrap();
    let input = rows(prepared.source());
    let original = input.clone();
    let mut events = Vec::new();
    let expected = prepared.bind_rows_controlled(&input, 2, |event| {
        events.push(event);
        Ok::<(), usize>(())
    }).unwrap().unwrap();
    assert!(events.len() > 20, "nested payload traversal must be controlled");
    for stop in 0..events.len() {
        let mut at = 0;
        let failure = prepared.bind_rows_controlled(&input, 2, |event| {
            assert_eq!(event, events[at]);
            let current = at;
            at += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(failure, Err(GraphQueryWriteBindError::Interrupted { row, source })
            if source == stop && row == events[stop].row));
        assert_eq!(at, stop + 1);
        assert_eq!(input, original);
    }
    assert_eq!(prepared.bind_rows(&input, 2).unwrap().unwrap().program(), expected.program());
}
