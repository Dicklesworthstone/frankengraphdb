//! Native vertex MERGE branch-action frontend laws.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphVertexUpsertBranch, GraphVertexUpsertTextErrorKind, PreparedGraphVertexMergeText,
    PreparedGraphVertexUpsertText,
};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const SEEN: LabelId = LabelId(2);
const CREATED: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Seen") => Some(GraphSymbol::Label(SEEN)),
        (GraphSymbolKind::Label, "Created") => Some(GraphSymbol::Label(CREATED)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

#[test]
fn map_replacement_binds_current_keys_only_for_the_selected_clause() {
    use fgdb_gql::GraphVertexUpsertAction;

    let prepared = PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:7}) ON MATCH SET n = {p:n.p,q:$value} SET n = {}",
        R,
        symbols,
    )
    .unwrap();
    let arguments = GqlParameters::new().with_int64("value", 9).unwrap();
    let upsert = prepared.bind_parameters(&arguments).unwrap();
    assert!(matches!(
        upsert.on_match().last(),
        Some(GraphVertexUpsertAction::ClearProperties { keep }) if keep == &[P, Q]
    ));
    assert_eq!(
        upsert.after(),
        &[GraphVertexUpsertAction::ClearProperties { keep: Vec::new() }]
    );
    assert!(upsert.on_create().is_empty());
    assert_eq!(
        prepared
            .bind_parameters(&arguments)
            .unwrap()
            .canonical_bytes(),
        upsert.canonical_bytes()
    );

    // A sibling explicit assignment survives the same simultaneous map
    // replacement, while repeated keys and a later replacement still refuse.
    let sibling = PreparedGraphVertexUpsertText::prepare(
        "MERGE (n {p:7}) ON MATCH SET n = {q:1}, n.p=8",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert!(sibling.on_match().iter().any(|action| {
        matches!(action, GraphVertexUpsertAction::ClearProperties { keep } if keep == &[P, Q])
    }));
    for text in [
        "MERGE (n {p:7}) ON MATCH SET n.q=1, n = {p:7}",
        "MERGE (n {p:7}) SET n = {}, n = {p:7}",
        "MERGE (n {p:7}) SET n = {p:8}, n.p=9",
    ] {
        assert!(
            PreparedGraphVertexUpsertText::prepare(text, R, symbols)
                .and_then(|template| template.bind_parameters(&GqlParameters::new()))
                .is_err(),
            "{text}"
        );
    }
}

#[test]
fn typed_replacement_never_competes_with_a_sibling_assignment() {
    use fgdb_gql::{
        GqlScalarParameter, GraphVertexUpsertAction as Action, GraphVertexUpsertBuildError,
        PreparedGraphVertexUpsert,
    };
    use fgdb_types::CanonicalScalar;

    let merge = PreparedGraphVertexMergeText::prepare("MERGE (n {p:7})", R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let assignment = Action::SetProperty {
        key: P,
        value: GqlScalarParameter::new(CanonicalScalar::Int(8)).unwrap(),
    };
    let clear = Action::ClearProperties { keep: Vec::new() };
    for actions in [
        vec![assignment.clone(), clear.clone()],
        vec![clear, assignment.clone()],
    ] {
        assert!(matches!(
            PreparedGraphVertexUpsert::prepare(merge.clone(), actions, Vec::new()),
            Err(GraphVertexUpsertBuildError::PropertyReplacement { .. })
        ));
    }
    for keep in [vec![Q, P], vec![P, P]] {
        assert!(matches!(
            PreparedGraphVertexUpsert::prepare(
                merge.clone(),
                vec![Action::ClearProperties { keep }],
                Vec::new(),
            ),
            Err(GraphVertexUpsertBuildError::PropertyReplacement { .. })
        ));
    }
    for actions in [
        vec![
            assignment.clone(),
            Action::ClearProperties { keep: vec![P] },
        ],
        vec![Action::ClearProperties { keep: vec![P] }, assignment],
    ] {
        PreparedGraphVertexUpsert::prepare(merge.clone(), actions, Vec::new()).unwrap();
    }
}

#[test]
fn one_parameter_table_spans_merge_key_and_both_action_branches() {
    let text = "MERGE (n:Person {p:$p}) ON MATCH SET n.q=$matched,n:Seen ON CREATE SET n.q=$created,n:Created";
    let calls = Cell::new(0);
    let template = PreparedGraphVertexUpsertText::prepare(text, R, |kind, name: &str| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let resolved = calls.get();
    assert_eq!(template.statement(), text);
    assert_eq!(template.parameter_schema().len(), 3);
    let args = GqlParameters::new()
        .with_int64("p", 7)
        .unwrap()
        .with_int64("matched", 100)
        .unwrap()
        .with_int64("created", 200)
        .unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(calls.get(), resolved, "binding must not re-enter catalog");
    assert_eq!(bound.on_match().len(), 2);
    assert_eq!(bound.on_create().len(), 2);
    let frozen = bound.canonical_bytes();
    assert_eq!(
        template.bind_parameters(&args).unwrap().canonical_bytes(),
        frozen
    );
    assert!(!format!("{template:?} {bound:?}").contains("matched"));

    let reversed = PreparedGraphVertexUpsertText::prepare(
        "MERGE (x:Person {p:$p}) ON CREATE SET x.q=$created,x:Created ON MATCH SET x.q=$matched,x:Seen",
        R, symbols,
    ).unwrap().bind_parameters(&args).unwrap();
    // Branch order in source does not change branch semantics.
    assert_eq!(bound.canonical_bytes(), reversed.canonical_bytes());
}

#[test]
fn duplicate_branch_wrong_target_and_unknown_branch_refuse() {
    for (text, expected) in [
        (
            "MERGE (n:Person {p:1}) ON MATCH SET n.q=1 ON MATCH SET n:Seen",
            Some(GraphVertexUpsertTextErrorKind::DuplicateBranch),
        ),
        ("MERGE (n:Person {p:1}) ON CREATE SET other.q=1", None),
        ("MERGE (n:Person {p:1}) ON DELETE SET n.q=1", None),
    ] {
        let failed = PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap_err();
        if let Some(expected) = expected {
            assert_eq!(failed.kind, expected);
        }
    }
    // Computed assignments over the chosen vertex compile since ec3a710d;
    // their semantics are fgdb's tests/computed_merge_text.rs.
    assert!(
        PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {p:1}) ON CREATE SET n.q=n.q+1",
            R,
            symbols,
        )
        .is_ok()
    );
    assert!(
        PreparedGraphVertexMergeText::prepare(
            "MERGE (n:Person {p:1}) ON CREATE SET n.q=1",
            R,
            symbols,
        )
        .is_err(),
        "plain MERGE entrypoint must not silently discard branch clauses"
    );
}

#[test]
fn branch_duplicate_fields_are_rejected_by_the_typed_upsert_definition() {
    let failed = PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:1}) ON MATCH SET n.q=1,n.q=2",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap_err();
    assert!(matches!(
        failed.kind,
        GraphVertexUpsertTextErrorKind::UpsertBuild(
            fgdb_gql::GraphVertexUpsertBuildError::DuplicateProperty {
                branch: GraphVertexUpsertBranch::Match
            }
        )
    ));
}

#[test]
fn missing_and_wrong_action_arguments_retain_original_offsets() {
    let text = "MERGE (n:Person {p:$p}) ON MATCH SET n.q=$value";
    let template = PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap();
    let missing = template
        .bind_parameters(&GqlParameters::new().with_int64("p", 1).unwrap())
        .unwrap_err();
    assert_eq!(missing.offset, text.find("$value").unwrap());
    assert!(matches!(
        missing.kind,
        GraphVertexUpsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let wrong = GqlParameters::new()
        .with_int64("p", 1)
        .unwrap()
        .with_uint64("value", 2)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphVertexUpsertTextErrorKind::Query(
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
        )
    ));
}

/// openCypher `MERGE ... SET ...`: the assignments apply whether the vertex
/// was matched or created, after any ON clause. Since ec3a710d the trailing
/// SET is its own clause, not folded into both branches: it sees the ON
/// clause's staged effects, and it cannot erase an earlier failure or an
/// unauthorized write by overwriting the same target. A spelling with a
/// trailing SET is therefore a different statement from the explicit
/// two-branch one, and it keeps its trailing actions. Which write wins is
/// fgdb's tests/computed_merge_text.rs.
#[test]
fn a_trailing_set_is_its_own_clause_not_folded_into_both_branches() {
    // Bind exactly the parameters each statement declares.
    let bytes = |text: &str| {
        let template = PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap();
        let mut args = GqlParameters::new();
        for spec in template.parameter_schema() {
            let value = match spec.name.as_str() {
                "p" => 7,
                "v" => 1,
                _ => 2,
            };
            args = args.with_int64(spec.name.as_str(), value).unwrap();
        }
        template.bind_parameters(&args).unwrap().canonical_bytes()
    };
    for (spelled, explicit) in [
        (
            "MERGE (n:Person {p:$p}) SET n.q=$v,n:Seen",
            "MERGE (n:Person {p:$p}) ON MATCH SET n.q=$v,n:Seen ON CREATE SET n.q=$v,n:Seen",
        ),
        (
            "MERGE (n:Person {p:$p}) ON CREATE SET n.q=$c,n:Created SET n.q=$v",
            "MERGE (n:Person {p:$p}) ON MATCH SET n.q=$v ON CREATE SET n:Created,n.q=$v",
        ),
        (
            "MERGE (n:Person {p:$p}) ON MATCH SET n:Seen ON CREATE SET n.q=$c SET n:Seen",
            "MERGE (n:Person {p:$p}) ON MATCH SET n:Seen ON CREATE SET n.q=$c,n:Seen",
        ),
    ] {
        assert_ne!(bytes(spelled), bytes(explicit), "{spelled}");
        let shape = |text: &str| {
            format!(
                "{:?}",
                PreparedGraphVertexUpsertText::prepare(text, R, symbols).unwrap()
            )
        };
        assert!(!shape(spelled).contains("after: 0"), "{spelled}");
        assert!(shape(explicit).contains("after: 0"), "{explicit}");
    }
    // A repeat inside the one SET list still refuses when the typed
    // definition is built, as it does inside an ON clause.
    let repeated = PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:$p}) SET n.q=$v,n.q=$c",
        R,
        symbols,
    )
    .unwrap();
    let args = GqlParameters::new()
        .with_int64("p", 7)
        .unwrap()
        .with_int64("v", 1)
        .unwrap()
        .with_int64("c", 2)
        .unwrap();
    assert!(repeated.bind_parameters(&args).is_err());
}

/// Plan identity for constant-valued MERGE (fgdb-cal4z acceptance). Computed
/// SET values (ec3a710d) compile through the scalar expression compiler, but
/// a literal or parameter value in any clause stays the constant action it
/// was before (SetProperty, transcript tag 0), never an expression (tag 2).
/// So a constant-valued MERGE keeps its pre-ec3a710d encoding. The one
/// deliberate change, a trailing SET kept as its own clause, is pinned by
/// a_trailing_set_is_its_own_clause_not_folded_into_both_branches. Control:
/// a computed value in the same position becomes the expression action.
#[test]
fn constant_values_stay_constant_actions_and_only_computed_values_compile_to_expressions() {
    use fgdb_gql::GraphVertexUpsertAction as Action;
    let args = GqlParameters::new()
        .with_int64("p", 7)
        .unwrap()
        .with_int64("v", 1)
        .unwrap();
    let bound = |text: &str| {
        PreparedGraphVertexUpsertText::prepare(text, R, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap()
    };
    let constant =
        bound("MERGE (n:Person {p:$p}) ON MATCH SET n.q=2,n:Seen ON CREATE SET n.q=$v SET n.q='x'");
    let clauses = [constant.on_match(), constant.on_create(), constant.after()];
    assert_eq!(clauses.map(<[Action]>::len), [2, 1, 1]);
    for action in clauses.iter().flat_map(|clause| clause.iter()) {
        assert!(
            matches!(action, Action::SetProperty { .. } | Action::SetLabel { .. }),
            "{action:?}"
        );
    }
    let computed = bound("MERGE (n:Person {p:$p}) ON MATCH SET n.q=n.q+$v SET n.q=n.q*2");
    for clause in [computed.on_match(), computed.after()] {
        assert!(
            matches!(clause, [Action::SetExpression { .. }]),
            "{clause:?}"
        );
    }
    assert_ne!(constant.canonical_bytes(), computed.canonical_bytes());
}

#[test]
fn map_assignments_are_their_per_key_assignments() {
    // fgdb-0s2pq: `n += {key: value, ...}` in any vertex MERGE SET clause is
    // exactly the assignments `n.key = value`, in entry order.
    let args = GqlParameters::new()
        .with_int64("p", 7)
        .unwrap()
        .with_int64("m", 100)
        .unwrap();
    for (map, keys) in [
        (
            "MERGE (n:Person {p:$p}) ON MATCH SET n += {q: $m} ON CREATE SET n += {q: 2}, n:Created",
            "MERGE (n:Person {p:$p}) ON MATCH SET n.q = $m ON CREATE SET n.q = 2, n:Created",
        ),
        (
            "MERGE (n:Person {p:$p}) SET n += {q: $m, p: 1}",
            "MERGE (n:Person {p:$p}) SET n.q = $m, n.p = 1",
        ),
        (
            "MERGE (n:Person {p:$p}) ON MATCH SET n:Seen, n += {q: n.q + $m}",
            "MERGE (n:Person {p:$p}) ON MATCH SET n:Seen, n.q = n.q + $m",
        ),
        // `{}` assigns nothing (Set5 [5]): an empty branch is no branch.
        (
            "MERGE (n:Person {p:$p}) ON CREATE SET n += {} ON MATCH SET n += {q: $m}",
            "MERGE (n:Person {p:$p}) ON MATCH SET n.q = $m",
        ),
        (
            "MERGE (n:Person {p:$p}) SET n += {}, n.q = $m",
            "MERGE (n:Person {p:$p}) SET n.q = $m",
        ),
    ] {
        let bind = |text: &str| {
            PreparedGraphVertexUpsertText::prepare(text, R, symbols)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"))
                .bind_parameters(&args)
                .unwrap()
                .canonical_bytes()
        };
        assert_eq!(bind(map), bind(keys), "{map}");
    }
    for text in [
        "MERGE (n:Person {p:1}) SET n += {,}",
        "MERGE (n:Person {p:1}) SET n += {q: 1, q: 2}",
        // A map parameter has no keys at preparation.
        "MERGE (n:Person {p:1}) SET n += $m",
    ] {
        assert!(
            PreparedGraphVertexUpsertText::prepare(text, R, symbols).is_err(),
            "{text}"
        );
    }
    // A map entry and a plain assignment of one field are a duplicate field,
    // refused by the typed definition as `n.q=1, n.q=2` is.
    let failed = PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:1}) SET n += {q: 1}, n.q = 2",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap_err();
    assert!(matches!(
        failed.kind,
        GraphVertexUpsertTextErrorKind::UpsertBuild(
            fgdb_gql::GraphVertexUpsertBuildError::DuplicateProperty { .. }
        )
    ));
}
