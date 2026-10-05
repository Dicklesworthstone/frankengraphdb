//! Controlled admission uses the same native binder and preserves its diagnostics.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{
    GraphUnwindBindError, GraphUnwindBindEvent, GraphUnwindRowError, GraphUnwindWriteError,
    GraphUnwindWriteText,
};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphWriteProgram,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const QUERY: &str = "UNWIND $rows AS row MERGE (n:Entity {id:row.id}) SET n.name=row.name";
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn row(id: CanonicalScalar, name: &str) -> GraphValue {
    GraphValue::map(vec![
        ("id".into(), GraphValue::Scalar(id)),
        (
            "name".into(),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap()),
        ),
    ])
    .unwrap()
}
fn arguments(name: &str) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            vec![
                row(CanonicalScalar::Int(1), name),
                row(CanonicalScalar::Int(2), name),
            ],
        )
        .unwrap()
}

#[test]
fn controlled_and_plain_binding_have_identical_programs_and_locations() {
    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    let args = arguments("not syntax '; $value");
    let frozen = args.canonical_bytes();
    let plain = parsed.bind(&args, R, symbols).unwrap();
    let mut definitions = 0;
    let mut binding_units = Vec::new();
    let bound = parsed
        .bind_with_limit_controlled(&args, R, 64, symbols, |event| {
            match event {
                GraphUnwindBindEvent::Work(units) => {
                    assert!(units > 0);
                    if definitions != 0 {
                        binding_units.push(units);
                    }
                }
                GraphUnwindBindEvent::Definition(script) => {
                    definitions += 1;
                    assert!(script.requires_read());
                    assert_eq!(script.statements().len(), 1);
                }
            }
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(definitions, 1);
    // Entire statement reservation, both records, then terminal acceptance.
    assert_eq!(binding_units, [2, 1, 1, 1]);
    assert_eq!(plain.program(), bound.program());
    // Independently bind native statements, without the UNWIND lowering or
    // either batch wrapper, so equality is not just one wrapper calling itself.
    let native = PreparedGraphWriteScript::prepare_with_parameter_types(
        "MERGE (n:Entity {id:$id}) SET n.name=$name",
        R,
        &[
            ("id", GqlParameterType::Scalar(CanonicalScalarKind::Int)),
            ("name", GqlParameterType::Scalar(CanonicalScalarKind::Text)),
        ],
        symbols,
    )
    .unwrap();
    let expected = PreparedGraphWriteProgram::prepare(
        (1..=2)
            .flat_map(|id| {
                let input = GqlParameters::new()
                    .with_scalar("id", CanonicalScalar::Int(id))
                    .unwrap()
                    .with_text("name", "not syntax '; $value")
                    .unwrap();
                native
                    .bind_parameters(&input)
                    .unwrap()
                    .into_statements()
                    .into_vec()
            })
            .collect(),
    )
    .unwrap();
    assert_eq!(
        bound.program().canonical_bytes(),
        expected.canonical_bytes()
    );
    for i in 0..2 {
        assert_eq!(plain.location(i), bound.location(i));
        assert_eq!(bound.location(i).unwrap().argument_set, i);
        assert_eq!(bound.location(i).unwrap().span, 0..QUERY.len());
    }
    assert_eq!(args.canonical_bytes(), frozen);
}

#[test]
fn every_control_boundary_can_refuse_without_a_partial_program_or_later_callback() {
    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    let args = arguments("payload");
    let frozen = args.canonical_bytes();
    let calls = Cell::new(0);
    let mut trace = Vec::new();
    parsed
        .bind_with_limit_controlled(
            &args,
            R,
            64,
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
            |_| {
                trace.push(calls.get());
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    assert!(trace.len() > 2 * args.len());
    for (stop, expected_catalog_calls) in trace.into_iter().enumerate() {
        calls.set(0);
        let mut events = 0;
        let result = parsed.bind_with_limit_controlled(
            &args,
            R,
            64,
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
            |_| {
                let at = events;
                events += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GraphUnwindBindError::Interrupted(at)) if at == stop));
        assert_eq!(events, stop + 1, "control ran after refusal at {stop}");
        assert_eq!(calls.get(), expected_catalog_calls);
        assert_eq!(args.canonical_bytes(), frozen);
    }
}

#[test]
fn a_post_catalog_refusal_is_not_misreported_as_unknown_symbol() {
    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    let calls = Cell::new(0);
    let result = parsed.bind_with_limit_controlled(
        &arguments("x"),
        R,
        64,
        |_, _| {
            calls.set(calls.get() + 1);
            None
        },
        |_| {
            if calls.get() == 0 {
                Ok(())
            } else {
                Err("expired after catalog")
            }
        },
    );
    assert!(matches!(
        result,
        Err(GraphUnwindBindError::Interrupted("expired after catalog"))
    ));
    assert_eq!(calls.get(), 1);
}

#[test]
fn malformed_final_row_retains_its_typed_error_before_catalog_access() {
    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    let args = GqlParameters::new()
        .with_list(
            "rows",
            vec![
                row(CanonicalScalar::Int(1), "x"),
                row(CanonicalScalar::Bool(true), "y"),
            ],
        )
        .unwrap();
    let result = parsed.bind_with_limit_controlled(
        &args,
        R,
        64,
        |_, _| panic!("shape admission must finish first"),
        |_| Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GraphUnwindBindError::Binding(GraphUnwindWriteError::Row {
            row: 1,
            kind: GraphUnwindRowError::IncompatibleFieldTypes,
            ..
        }))
    ));
}

#[test]
fn payload_copies_and_repeated_global_transcripts_are_not_constant_price() {
    let total = |query: &str, args: &GqlParameters| {
        let parsed = GraphUnwindWriteText::parse(query).unwrap();
        let mut work = 0;
        parsed
            .bind_with_limit_controlled(args, R, 64, symbols, |event| {
                if let GraphUnwindBindEvent::Work(units) = event {
                    work += units;
                }
                Ok::<_, ()>(())
            })
            .unwrap();
        work
    };
    let small = arguments("x");
    let large = arguments(&"x".repeat(1025));
    assert_eq!(total(QUERY, &large) - total(QUERY, &small), 2 * 1024);

    let query = "UNWIND $rows AS row MERGE (n:Entity {id:row.id}) SET n.name=$shared";
    let small = arguments("unused").with_text("shared", "x").unwrap();
    let large = arguments("unused")
        .with_text("shared", &"x".repeat(1025))
        .unwrap();
    let bytes = (large.canonical_byte_len() - small.canonical_byte_len()) as u64;
    assert_eq!(total(query, &large) - total(query, &small), 2 * bytes);
}

#[test]
fn definition_gate_runs_before_binding_the_first_invalid_merge_key() {
    let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
    let args = GqlParameters::new()
        .with_list("rows", vec![row(CanonicalScalar::Null, "x")])
        .unwrap();
    let result = parsed.bind_with_limit_controlled(&args, R, 64, symbols, |event| match event {
        GraphUnwindBindEvent::Definition(_) => Err("operation class denied"),
        GraphUnwindBindEvent::Work(_) => Ok(()),
    });
    assert!(matches!(
        result,
        Err(GraphUnwindBindError::Interrupted("operation class denied"))
    ));
    assert!(matches!(
        parsed.bind(&args, R, symbols),
        Err(GraphUnwindWriteError::Binding(_))
    ));
}
