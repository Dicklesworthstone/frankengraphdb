//! Checked expression preparation through the public MATCH/mutation pipeline.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphIntegerBuildError, GraphIntegerErrorKind, GraphMutationBatch, GraphMutationError,
    GraphMutationIntent, GraphMutationPolicy, GraphMutationTextErrorKind, GraphPatternTextErrorKind,
    GraphSymbol, GraphSymbolKind, PreparedGraphMutation, PreparedGraphMutationText};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Triple = (VId, RelationId, VId);
type MutationResult<C = ()> = Result<GraphMutationBatch, GqlQueryError<GraphMutationError<()>, C>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphMutation {
    PreparedGraphMutationText::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000), 1_000)
}
fn run(plan: &PreparedGraphMutation, vertices: &[VId], edges: &[Triple], props: &Props,
    policy: GraphMutationPolicy) -> MutationResult {
    plan.execute_governed(policy, |selection, budget| {
        selection.plan().execute_governed_with_properties(
            (vertices.len() + edges.len()) as u64, vertices.iter().copied(), edges.iter().copied(),
            |vid, predicates| Ok::<_, ()>(predicates.iter().all(|predicate| {
                predicate.matches_borrowed([], props.iter().filter_map(|(&(owner, key), value)|
                    (owner == vid).then_some((key, value))))
            })),
            |vid, key| Ok(props.get(&(vid, key))), budget, || Ok::<_, ()>(()),
        )
    }, || Ok::<_, ()>(()))
}
fn property(vertex: u128, key: PropertyKeyId, value: Option<i64>) -> GraphMutationIntent {
    GraphMutationIntent::Property { vertex: VId(vertex), key,
        value: Some(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)) }
}

#[test]
fn precedence_associativity_signs_and_nullable_functions_have_explicit_results() {
    for (expression, expected) in [
        ("2+3*4", Some(14)), ("(2+3)*4", Some(20)), ("20-6-3", Some(11)),
        ("20/(6-1)", Some(4)), ("20/3*2", Some(12)), ("20%6*2", Some(4)),
        ("-7/3", Some(-2)), ("-7%3", Some(-1)), ("7%-3", Some(1)),
        ("-(-7)", Some(7)), ("ABS(-7)", Some(7)), ("--7", Some(7)),
        ("1--2", Some(3)), ("1+-2", Some(-1)),
        ("-9223372036854775808%(-1)", Some(0)),
        ("COALESCE(NULL,NULL,7,1/0)", Some(7)),
        ("COALESCE(NULLIF(5,5),ABS(-4))+1", Some(5)),
        ("8/NULLIF(0,0)", None), ("NULL/0", None),
        ("COALESCE(NULLIF(5,NULL),9)", Some(5)),
        ("COALESCE(NULLIF(NULL,5),9)", Some(9)),
    ] {
        let plan = prepare(&format!("MATCH (n) SET n.q={expression}"));
        let result = run(&plan, &[VId(1)], &[], &Props::new(), policy()).unwrap();
        assert_eq!(result.intents(), &[property(1, Q, expected)], "{expression}");
    }
    for variable in ["abs", "coalesce", "nullif", "true", "null"] {
        let plan = prepare(&format!("MATCH ({variable}) SET {variable}.q=ABS({variable}.p)+1"));
        let props = Props::from([((VId(1), P), CanonicalScalar::Int(-7))]);
        assert_eq!(run(&plan, &[VId(1)], &[], &props, policy()).unwrap().intents(), &[property(1, Q, Some(8))]);
    }
    for literal in ["'a+1 / NULLIF(x,0)'", "TRUE", "NULL", "-9223372036854775808"] {
        assert_eq!(prepare(&format!("MATCH (n) SET n.q={literal}")).canonical_bytes(),
            prepare(&format!("MATCH (n) SET n.q=({literal})")).canonical_bytes());
    }
}

#[test]
fn reciprocal_arithmetic_and_duplicate_counters_use_the_frozen_pre_statement_values() {
    let props = Props::from([((VId(1), P), CanonicalScalar::Int(10)), ((VId(2), P), CanonicalScalar::Int(20))]);
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(2), R, VId(1))];
    let plan = prepare("MATCH (a)-[:R]->(b) SET a.p=b.p+1,b.p=a.p+1");
    let result = run(&plan, &[VId(1), VId(2)], &edges, &props, policy()).unwrap();
    assert_eq!(result.intents(), &[property(1, P, Some(21)), property(2, P, Some(11))]);
    assert_eq!((result.stats().selection.result_rows, result.stats().effects), (3, 2));
    let counter = prepare("MATCH (a)-[:R]->(b) SET a.p=COALESCE(a.p,0)+1");
    let result = run(&counter, &[VId(1), VId(2)], &edges, &props, policy()).unwrap();
    assert_eq!(result.intents(), &[property(1, P, Some(11)), property(2, P, Some(21))]);
    let changed = Props::from([((VId(1), P), CanonicalScalar::Int(10)),
        ((VId(2), P), CanonicalScalar::Int(20)), ((VId(3), P), CanonicalScalar::Int(30))]);
    assert!(matches!(run(&plan, &[VId(1), VId(2), VId(3)],
        &[(VId(1), R, VId(2)), (VId(1), R, VId(3))], &changed, policy()),
        Err(GqlQueryError::Source(GraphMutationError::ConflictingAssignment { .. }))));
}

#[test]
fn arithmetic_errors_are_typed_and_no_successful_prefix_is_returned() {
    for (expression, first, second, kind) in [
        ("n.p+1", CanonicalScalar::Int(10), CanonicalScalar::Int(i64::MAX), GraphIntegerErrorKind::Overflow),
        ("10/n.p", CanonicalScalar::Int(2), CanonicalScalar::Int(0), GraphIntegerErrorKind::DivisionByZero),
        ("n.p*2", CanonicalScalar::Int(2), CanonicalScalar::Bool(true), GraphIntegerErrorKind::NonInteger),
    ] {
        let props = Props::from([((VId(1), P), first), ((VId(2), P), second)]);
        let plan = prepare(&format!("MATCH (n) SET n.q={expression}"));
        let failed = run(&plan, &[VId(1), VId(2)], &[], &props, policy()).unwrap_err();
        assert!(matches!(failed, GqlQueryError::Source(GraphMutationError::Arithmetic {
            row: 1, action: 0, error,
        }) if error.kind == kind), "{expression}: {failed:?}");
    }
    for expression in ["ABS(-9223372036854775808)", "-(-9223372036854775808)",
        "-9223372036854775808/-1", "9223372036854775807*2"] {
        assert!(matches!(run(&prepare(&format!("MATCH (n) SET n.q={expression}")),
            &[VId(1)], &[], &Props::new(), policy()),
            Err(GqlQueryError::Source(GraphMutationError::Arithmetic { error, .. }))
                if error.kind == GraphIntegerErrorKind::Overflow));
    }
    // Binding does not eagerly fold a dead constant expression into an error.
    let plan = prepare("MATCH (n) SET n.q=1/0");
    assert!(run(&plan, &[], &[], &Props::new(), policy()).unwrap().intents().is_empty());
}

#[test]
fn nullable_targets_and_lazy_arithmetic_do_not_hide_property_source_failures() {
    let absent = prepare("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) SET b.q=1/0");
    assert!(run(&absent, &[VId(1)], &[], &Props::new(), policy()).unwrap().intents().is_empty());
    let fallback = prepare("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) SET a.q=COALESCE(b.p,0)+1");
    assert_eq!(run(&fallback, &[VId(1)], &[], &Props::new(), policy()).unwrap().intents(), &[property(1, Q, Some(1))]);
    let plan = prepare("MATCH (n) SET n.q=COALESCE(5,n.p/0)");
    let reads = Cell::new(0);
    let result = plan.execute_governed(policy(), |selection, budget| {
        selection.plan().execute_governed_with_properties(1, [VId(1)], [],
            |_, _| Ok::<_, &str>(true),
            |_, _| { reads.set(reads.get() + 1); Err::<Option<&CanonicalScalar>, _>("property unavailable") },
            budget, || Ok::<_, ()>(()))
    }, || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::Source("property unavailable")))));
    assert_eq!(reads.get(), 1);
    let missing = run(&plan, &[VId(1)], &[], &Props::new(), policy()).unwrap();
    assert_eq!(missing.intents(), &[property(1, Q, Some(5))]);
}

#[test]
fn computed_and_literal_assignments_share_identity_but_null_is_not_removal() {
    for (text, expected) in [
        ("MATCH (n) SET n.q=1+2,n.q=3", Some(3)),
        ("MATCH (n) SET n.q=NULL+1,n.q=NULL", None),
    ] {
        let result = run(&prepare(text), &[VId(1)], &[], &Props::new(), policy()).unwrap();
        assert_eq!(result.intents(), &[property(1, Q, expected)]);
    }
    for text in ["MATCH (n) SET n.q=NULL+1 REMOVE n.q", "MATCH (n) SET n.q=1+2,n.q=4",
        "MATCH (n) SET n.q=1+0,n.q=TRUE"] {
        assert!(matches!(run(&prepare(text), &[VId(1)], &[], &Props::new(), policy()),
            Err(GqlQueryError::Source(GraphMutationError::ConflictingAssignment { .. }))));
    }
}

#[test]
fn typed_rhs_parameters_reuse_the_statement_schema_catalog_and_original_offsets() {
    let text = "\u{2003}MATCH (n) WHERE n.p >= $step SET n.q=COALESCE($base,n.p)+$step*$step";
    let integer = CanonicalScalarKind::of(&CanonicalScalar::Int(0));
    let calls = Cell::new(0);
    let template = PreparedGraphMutationText::prepare_with_parameter_types(text, R,
        &[("base", GqlParameterType::Scalar(integer))], |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(template.parameter_schema()[0].occurrences, 3);
    assert_eq!(template.parameter_schema()[1].occurrences, 1);
    let args = GqlParameters::new().with_int64("step", 2).unwrap().with_null("base").unwrap();
    let plan = template.bind_parameters(&args).unwrap();
    let frozen = plan.canonical_bytes();
    assert_eq!(template.bind_parameters(&args).unwrap().canonical_bytes(), frozen);
    assert_eq!(calls.get(), 2);
    let props = Props::from([((VId(1), P), CanonicalScalar::Int(10))]);
    assert_eq!(run(&plan, &[VId(1)], &[], &props, policy()).unwrap().intents(), &[property(1, Q, Some(14))]);
    let failed = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(failed.offset, text.find("$step").unwrap());
    assert!(matches!(failed.kind, GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)));
    let wrong = GqlParameters::new().with_int64("step", 2).unwrap().with_bool("base", true).unwrap();
    assert!(matches!(template.bind_parameters(&wrong).unwrap_err().kind,
        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })));
    assert!(template.bind_parameters(&args.with_int64("unused", 1).unwrap()).is_err());
    let textual = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("x").unwrap());
    let calls = Cell::new(0);
    let failed = PreparedGraphMutationText::prepare_with_parameter_types(text, R,
        &[("base", GqlParameterType::Scalar(textual))], |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).unwrap_err();
    assert_eq!(failed.kind, GraphMutationTextErrorKind::IntegerOperand);
    assert_eq!(failed.offset, text.find("$base").unwrap());
    assert_eq!(calls.get(), 0);
    assert!(!format!("{template:?} {plan:?} {failed:?}").contains("$base"));
}

#[test]
fn malformed_and_excessive_expression_definitions_fail_before_catalog_access() {
    for expression in ["n.p+", "n.p+*1", "ABS()", "ABS(1,2)", "NULLIF(1)",
        "NULLIF(1,2,3)", "COALESCE(1)", "COALESCE(1,)", "n.p+TRUE", "'7'+1",
        "n.p^2", "unknown.p+1", "((1+2)", "9223372036854775808+1"] {
        let calls = Cell::new(0);
        assert!(PreparedGraphMutationText::prepare(&format!("MATCH (n) SET n.q={expression}"), R,
            |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) }).is_err(), "{expression}");
        assert_eq!(calls.get(), 0, "{expression}");
    }
    let valid = format!("MATCH (n) SET n.q={}", vec!["1"; 512].join("+"));
    assert_eq!(run(&prepare(&valid), &[VId(1)], &[], &Props::new(), policy()).unwrap().intents(), &[property(1, Q, Some(512))]);
    let calls = Cell::new(0);
    let failed = PreparedGraphMutationText::prepare(
        &format!("MATCH (n) SET n.q={}", vec!["1"; 513].join("+")), R,
        |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) }).unwrap_err();
    assert!(matches!(failed.kind, GraphMutationTextErrorKind::IntegerExpression(
        GraphIntegerBuildError::TooManyInstructions { .. })));
    assert_eq!(calls.get(), 0);
    let nested = |depth| format!("MATCH (n) SET n.q={}1{}", "(".repeat(depth), ")".repeat(depth));
    prepare(&nested(64));
    assert!(matches!(PreparedGraphMutationText::prepare(&nested(65), R, symbols).unwrap_err().kind,
        GraphMutationTextErrorKind::IntegerNesting { limit: 64 }));
}

#[test]
fn exact_limits_and_every_interruption_cover_selection_bytecode_and_proposals() {
    let plan = prepare("MATCH (a)-[:R]->(b) SET a.q=COALESCE(a.p,0)+ABS(-2)*3,b.q=COALESCE(b.p,0)+6");
    let props = Props::from([((VId(2), P), CanonicalScalar::Int(20))]);
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(2), R, VId(1))];
    let measured = run(&plan, &[VId(1), VId(2)], &edges, &props, policy()).unwrap().stats();
    let exact = GraphMutationPolicy::new(GqlQueryPolicy::new(measured.selection.snapshot_records,
        measured.selection.result_rows, measured.evaluator.work_units, measured.evaluator.scratch_entries), measured.effects);
    assert_eq!(run(&plan, &[VId(1), VId(2)], &edges, &props, exact).unwrap().stats(), measured);
    for budget in [
        GraphMutationPolicy::new(GqlQueryPolicy::new(measured.selection.snapshot_records - 1, 100, u64::MAX, u64::MAX), 100),
        GraphMutationPolicy::new(GqlQueryPolicy::new(100, measured.selection.result_rows - 1, u64::MAX, u64::MAX), 100),
        GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, measured.evaluator.work_units - 1, u64::MAX), 100),
        GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, u64::MAX, measured.evaluator.scratch_entries - 1), 100),
        GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, u64::MAX, u64::MAX), measured.effects - 1),
    ] { assert!(run(&plan, &[VId(1), VId(2)], &edges, &props, budget).is_err()); }
    let execute = |stop: usize| {
        let calls = Cell::new(0);
        let checkpoint = || { let at = calls.get() + 1; calls.set(at); if at == stop { Err(stop) } else { Ok(()) } };
        let result: MutationResult<usize> = plan.execute_governed(policy(), |selection, budget| {
            selection.plan().execute_governed_with_properties(5, [VId(1), VId(2)], edges,
                |_, _| Ok::<_, ()>(true), |vid, key| Ok(props.get(&(vid, key))), budget, checkpoint)
        }, checkpoint);
        (result, calls.get())
    };
    let (complete, total) = execute(0);
    assert_eq!(complete.unwrap().intents(), &[property(1, Q, Some(6)), property(2, Q, Some(26))]);
    for stop in 1..=total {
        let (result, calls) = execute(stop);
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(calls, stop);
    }
}
