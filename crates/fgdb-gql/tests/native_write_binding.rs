//! Pure native dispatch/binding tests: no database or substitute evaluator.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::unwind_write::{GraphUnwindRowError, GraphUnwindWriteError};
use fgdb_gql::{
    BoundNativeGraphWrite, GqlParameters, GqlQueryError, GraphMutationProgramError, GraphSymbol,
    GraphSymbolKind, GraphVertexUpsertError, GraphWriteProgramError,
    GraphWriteScriptExecutionError, NativeGraphWriteBindError,
};
use fgdb_types::CanonicalScalar;

const R: RelationId = RelationId(1);
const UPSERT: &str = "UNWIND $rows AS row MERGE (n:Person {p:row.p}) SET n.q=row.q";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn row(p: i64, q: CanonicalScalar) -> GraphValue {
    GraphValue::map(vec![
        ("p".into(), GraphValue::Scalar(CanonicalScalar::Int(p))),
        ("q".into(), GraphValue::Scalar(q)),
    ])
    .unwrap()
}
fn rows(values: Vec<GraphValue>) -> GqlParameters {
    GqlParameters::new().with_list("rows", values).unwrap()
}

#[test]
fn bounded_unwind_expands_beyond_the_script_definition_limit() {
    let arguments = rows(
        (0..65)
            .map(|p| row(p, CanonicalScalar::Int(p + 1)))
            .collect(),
    );
    let frozen = arguments.canonical_bytes();
    let bound = BoundNativeGraphWrite::bind(UPSERT, &arguments, R, symbols).unwrap();
    assert_eq!(bound.input_records(), Some(65));
    assert_eq!(bound.program().statements().len(), 65);
    assert_eq!(arguments.canonical_bytes(), frozen);
    let debug = format!("{bound:?}");
    assert!(!debug.contains(UPSERT));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn late_row_refusal_precedes_all_resolution_and_is_not_retried() {
    let arguments = rows(vec![
        row(1, CanonicalScalar::Int(10)),
        row(2, CanonicalScalar::Bool(true)),
    ]);
    let mut calls = 0;
    let result = BoundNativeGraphWrite::bind(UPSERT, &arguments, R, |kind, name| {
        calls += 1;
        symbols(kind, name)
    });
    assert!(matches!(
        result,
        Err(NativeGraphWriteBindError::Unwind(
            GraphUnwindWriteError::Row {
                row: 1,
                kind: GraphUnwindRowError::IncompatibleFieldTypes,
                ..
            }
        ))
    ));
    assert_eq!(calls, 0);
    for arguments in [rows(vec![]), GqlParameters::new()] {
        assert!(matches!(
            BoundNativeGraphWrite::bind(UPSERT, &arguments, R, |_, _| panic!("catalog")),
            Err(NativeGraphWriteBindError::Unwind(_))
        ));
    }
}

#[test]
fn ordinary_scripts_and_native_unwind_create_keep_their_compiler() {
    for text in [
        "CREATE (n:Person {p:$p}); MATCH (n:Person {p:$p}) SET n.q=4",
        "CREATE (n:Person {p:$p, q:'UNWIND $rows AS row MERGE'});",
    ] {
        let arguments = GqlParameters::new().with_int64("p", 1).unwrap();
        let bound = BoundNativeGraphWrite::bind(text, &arguments, R, symbols).unwrap();
        assert_eq!(bound.input_records(), None);
    }
    let bound = BoundNativeGraphWrite::bind(
        "UNWIND $rows AS row CREATE (n:Person {p:row.p, q:row.q})",
        &rows(vec![row(1, CanonicalScalar::Int(2))]),
        R,
        symbols,
    )
    .unwrap();
    assert_eq!(bound.input_records(), None);
    assert_eq!(bound.program().statements().len(), 1);
}

#[test]
fn preparation_and_binding_errors_remain_distinguishable() {
    assert!(matches!(
        BoundNativeGraphWrite::bind("CREATE (", &GqlParameters::new(), R, symbols),
        Err(NativeGraphWriteBindError::ScriptPreparation(_))
    ));
    assert!(matches!(
        BoundNativeGraphWrite::bind(
            "CREATE (n:Person {p:$p})",
            &GqlParameters::new(),
            R,
            symbols,
        ),
        Err(NativeGraphWriteBindError::ScriptBinding(_))
    ));
}

#[test]
fn row_coordinates_survive_but_infrastructure_gets_no_invented_row() {
    let bound = BoundNativeGraphWrite::bind(
        UPSERT,
        &rows(vec![
            row(1, CanonicalScalar::Int(2)),
            row(2, CanonicalScalar::Int(3)),
        ]),
        R,
        symbols,
    )
    .unwrap();
    let located = bound.execution_error::<(), (), ()>(GraphWriteProgramError::VertexUpsert {
        statement: 1,
        source: GqlQueryError::Source(GraphVertexUpsertError::ActionLimit {
            limit: 0,
            observed: 1,
        }),
    });
    let GraphWriteScriptExecutionError::BatchProgram {
        location: Some(location),
        ..
    } = located
    else {
        panic!("a failing expanded step must retain its input coordinate")
    };
    assert_eq!(location.argument_set, 1);
    assert_eq!(location.statement, 0);
    assert_eq!(location.span, 0..UPSERT.len());
    assert!(matches!(
        bound.execution_error::<(), (), ()>(GraphWriteProgramError::Program(
            GraphMutationProgramError::Preflight(())
        )),
        GraphWriteScriptExecutionError::BatchProgram { location: None, .. }
    ));
}

#[test]
fn multi_statement_unwind_is_refused_as_a_whole() {
    let arguments = rows(vec![row(1, CanonicalScalar::Int(2))]);
    let result = BoundNativeGraphWrite::bind(
        &format!("{UPSERT}; CREATE (n:Person {{p:5}})"),
        &arguments,
        R,
        |_, _| panic!("whole-script classification must refuse before resolution"),
    );
    assert!(result.is_err());
}
