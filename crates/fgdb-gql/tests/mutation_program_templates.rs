//! Reusable program definitions compose the existing statement binders.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphMutationProgramBuildError,
    GraphMutationProgramTemplateError as Error, GraphMutationTextErrorKind,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, MAX_GRAPH_MUTATION_STATEMENTS,
    PreparedGraphMutationProgram, PreparedGraphMutationProgramTemplate, PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::cell::Cell;

const FIRST: &str = "MATCH (n) WHERE n.p>=$threshold SET n.q=COALESCE(n.q,0)+$step";
const SECOND: &str = "MATCH (n) WHERE n.q>=$step SET n.p=n.p+$step";
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn input(text: &str) -> PreparedGraphMutationText {
    PreparedGraphMutationText::prepare(text, RelationId(1), symbols).unwrap()
}

#[test]
fn one_exact_map_binds_shared_and_statement_local_parameters_without_catalog_reentry() {
    let calls = Cell::new(0);
    let prepare = |text| PreparedGraphMutationText::prepare(text, RelationId(1), |kind, name| {
        calls.set(calls.get() + 1); symbols(kind, name)
    }).unwrap();
    let template = PreparedGraphMutationProgramTemplate::prepare(vec![prepare(FIRST), prepare(SECOND)]).unwrap();
    let prepared_calls = calls.get();
    assert_eq!(template.parameter_schema().iter().map(|spec| (spec.name.as_str(), spec.occurrences)).collect::<Vec<_>>(),
        vec![("threshold", 1), ("step", 3)]);
    let arguments = GqlParameters::new().with_int64("threshold", 10).unwrap().with_int64("step", 2).unwrap();
    let actual = template.bind_parameters(&arguments).unwrap();
    let expected = PreparedGraphMutationProgram::prepare(vec![
        input(FIRST).bind_parameters(&arguments).unwrap(),
        input(SECOND).bind_parameters(&GqlParameters::new().with_int64("step", 2).unwrap()).unwrap(),
    ]).unwrap();
    assert_eq!(actual, expected);
    let frozen = actual.canonical_bytes();
    assert_eq!(template.bind_parameters(&arguments).unwrap().canonical_bytes(), frozen);
    let other = GqlParameters::new().with_int64("threshold", 10).unwrap().with_int64("step", 3).unwrap();
    assert_ne!(template.bind_parameters(&other).unwrap().canonical_bytes(), frozen);
    assert_eq!(actual.canonical_bytes(), frozen);
    assert_eq!(calls.get(), prepared_calls);
    assert_eq!(template.statements()[0].statement(), FIRST);
    assert!(!format!("{template:?} {actual:?}").contains("threshold"));
    assert!(matches!(template.bind_parameters(&arguments.with_int64("extra", 1).unwrap()), Err(Error::UnexpectedArguments)));
}

#[test]
fn conflicting_declarations_and_coordinate_or_definition_limits_fail_before_binding() {
    let integer = input("MATCH (n) SET n.p=$value");
    let boolean = PreparedGraphMutationText::prepare_with_parameter_types(
        "MATCH (n) SET n.q=$value", RelationId(1),
        &[("value", GqlParameterType::Scalar(CanonicalScalarKind::of(&CanonicalScalar::Bool(true))))], symbols,
    ).unwrap();
    let conflict = PreparedGraphMutationProgramTemplate::prepare(vec![integer, boolean]);
    assert!(matches!(conflict, Err(Error::ConflictingParameterTypes {
        parameter: 0, first_statement: 0, statement: 1,
    })));
    assert!(matches!(PreparedGraphMutationProgramTemplate::prepare(vec![]),
        Err(Error::Definition(GraphMutationProgramBuildError::Empty))));
    let maximum = vec![input("MATCH (n) SET n.p=1"); MAX_GRAPH_MUTATION_STATEMENTS];
    assert_eq!(PreparedGraphMutationProgramTemplate::prepare(maximum.clone()).unwrap().statements().len(), MAX_GRAPH_MUTATION_STATEMENTS);
    let mut excessive = maximum;
    excessive.push(input("MATCH (n) SET n.p=1"));
    assert!(matches!(PreparedGraphMutationProgramTemplate::prepare(excessive),
        Err(Error::Definition(GraphMutationProgramBuildError::TooManyStatements { .. }))));
    let foreign = PreparedGraphMutationText::prepare("MATCH (n) SET n.p=1", RelationId(2), symbols).unwrap();
    assert!(matches!(PreparedGraphMutationProgramTemplate::prepare(vec![input("MATCH (n) SET n.p=1"), foreign]),
        Err(Error::Definition(GraphMutationProgramBuildError::MixedRelation { statement: 1 }))));
}

#[test]
fn missing_late_arguments_keep_statement_and_utf8_offsets_and_scalars_are_not_interpolated() {
    let first = PreparedGraphMutationText::prepare_with_parameter_types(
        "MATCH (n) SET n.q=$payload", RelationId(1),
        &[("payload", GqlParameterType::Scalar(CanonicalScalarKind::Text))], symbols,
    ).unwrap();
    let later = "\u{2003}MATCH (private_name) SET private_name.p=$missing";
    let template = PreparedGraphMutationProgramTemplate::prepare(vec![first, input(later)]).unwrap();
    let payload = "x' DETACH DELETE private_name -- $missing";
    let partial = GqlParameters::new().with_text("payload", payload).unwrap();
    let error = template.bind_parameters(&partial).unwrap_err();
    assert!(matches!(&error, Error::Bind { statement: 1, source }
        if source.offset == later.find('$').unwrap()
            && source.kind == GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)));
    assert!(!format!("{error:?} {error}").contains("private_name"));
    let wrong = partial.clone().with_bool("missing", true).unwrap();
    assert!(matches!(template.bind_parameters(&wrong), Err(Error::Bind { statement: 1, source })
        if matches!(source.kind, GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. }))));
    let complete = partial.with_int64("missing", 9).unwrap();
    let bound = template.bind_parameters(&complete).unwrap();
    assert!(!format!("{bound:?}").contains(payload));
    let null = GqlParameters::new().with_null("payload").unwrap().with_int64("missing", 9).unwrap();
    assert!(template.bind_parameters(&null).is_ok());
}
