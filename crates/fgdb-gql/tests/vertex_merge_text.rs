//! Native bounded vertex MERGE frontend laws.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertBuildError;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphVertexMergeTextErrorKind, PreparedGraphVertexMergeText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const ACTIVE: PropertyKeyId = PropertyKeyId(3);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        (GraphSymbolKind::Property, "active") => Some(GraphSymbol::Property(ACTIVE)),
        _ => None,
    }
}

#[test]
fn statement_time_list_keys_remain_closed_programs_until_governed_materialization() {
    use fgdb_gql::{GqlQueryError, GraphVertexMergeError};
    use fgdb_types::CanonicalTimestamp;

    for (statement, literals) in [
        ("MERGE (n:Person {p:[1/timestamp()]})", ["[1]", "[0]"]),
        (
            "MERGE (n:Person {p:[{p:7}.p,1/timestamp()]})",
            ["[7,1]", "[7,0]"],
        ),
    ] {
        let template = PreparedGraphVertexMergeText::prepare(statement, R, symbols).unwrap();
        let arguments = |ms: i128| {
            GqlParameters::new()
                .with_statement_time(CanonicalTimestamp::offset_only(ms * 1_000_000, 0).unwrap())
                .unwrap()
        };
        let zero = template.bind_parameters(&arguments(0)).unwrap();
        assert!(zero.selection().is_none() && zero.creation().is_none());
        let error = zero.materialize::<(), (), ()>(&mut |_| Ok(())).unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(GraphVertexMergeError::RowExpression {
                row: 0,
                property: 0,
                ..
            })
        ));
        let mut checkpoints = 0;
        let error = zero
            .materialize::<(), (), u8>(&mut |_| {
                checkpoints += 1;
                Err(GqlQueryError::Interrupted(17))
            })
            .unwrap_err();
        assert!(matches!(error, GqlQueryError::Interrupted(17)));
        assert_eq!(
            checkpoints, 1,
            "slot admission precedes even a lazy arithmetic fault"
        );
        let mut identities = Vec::new();
        for (ms, literal) in [1, 2].into_iter().zip(literals) {
            let bound = template.bind_parameters(&arguments(ms)).unwrap();
            let before = bound.canonical_bytes();
            let materialized = bound.materialize::<(), (), ()>(&mut |_| Ok(())).unwrap();
            let expected = PreparedGraphVertexMergeText::prepare(
                &format!("MERGE (n:Person {{p:{literal}}})"),
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            assert_eq!(
                materialized.creation().unwrap().canonical_bytes(),
                expected.creation().unwrap().canonical_bytes()
            );
            assert!(materialized.selection().is_some());
            assert_eq!(
                bound.canonical_bytes(),
                before,
                "materialization cannot mutate the bound IR"
            );
            identities.push(before);
        }
        assert_ne!(
            identities[0], identities[1],
            "the actual captured instant binds the program"
        );
        assert!(template.bind_parameters(&GqlParameters::new()).is_err());
        assert!(
            template
                .bind_parameters(&arguments(1).with_int64("unused", 1).unwrap())
                .is_err()
        );
    }
}

#[test]
fn deferred_time_keys_keep_original_static_creation_schema_refusals() {
    use fgdb_gql::algebra::{PatternBuildError, PatternLimitDimension};
    use fgdb_types::CanonicalTimestamp;
    let arguments = GqlParameters::new()
        .with_statement_time(CanonicalTimestamp::offset_only(0, 0).unwrap())
        .unwrap();
    // Preserve the actual original boundary (MAX_PATTERN_EDGES + 1),
    // comparing ordinary concrete keys and deferred time keys at both sides.
    for count in [65, 66] {
        for expression in ["[0]", "[timestamp()]"] {
            let fields = (0..count)
                .map(|n| format!("p{n}:{expression}"))
                .collect::<Vec<_>>()
                .join(",");
            let template = PreparedGraphVertexMergeText::prepare(
                &format!("MERGE (n {{{fields}}})"),
                R,
                |kind, name: &str| match kind {
                    GraphSymbolKind::Property => name
                        .strip_prefix('p')?
                        .parse::<u64>()
                        .ok()
                        .map(|key| GraphSymbol::Property(PropertyKeyId(key + 1))),
                    _ => None,
                },
            )
            .unwrap();
            let bound = template.bind_parameters(&arguments);
            if count == 65 {
                let bound = bound.unwrap();
                let materialized = bound.materialize::<(), (), ()>(&mut |_| Ok(())).unwrap();
                assert_eq!(materialized.creation().unwrap().vertices_per_row(), 1);
            } else {
                assert_eq!(
                    bound.unwrap_err().kind,
                    GraphVertexMergeTextErrorKind::InsertBuild(
                        GraphInsertBuildError::TooManyFields {
                            limit: 65,
                            observed: 66,
                        }
                    )
                );
            }
        }
    }
    for (statement, expected) in [
        (
            "MERGE (n:A:B {p:[timestamp()]})",
            GraphInsertBuildError::DuplicateLabel { vertex: 0 },
        ),
        (
            "MERGE (n {p:[timestamp()],same:[timestamp()]})",
            GraphInsertBuildError::DuplicateProperty { declaration: 0 },
        ),
    ] {
        let template =
            PreparedGraphVertexMergeText::prepare(statement, R, |kind, name: &str| {
                match (kind, name) {
                    (GraphSymbolKind::Label, "A" | "B") => Some(GraphSymbol::Label(PERSON)),
                    (GraphSymbolKind::Property, "p" | "same") => Some(GraphSymbol::Property(P)),
                    _ => None,
                }
            })
            .unwrap();
        assert_eq!(
            template.bind_parameters(&arguments).unwrap_err().kind,
            GraphVertexMergeTextErrorKind::InsertBuild(expected)
        );
    }
    let labels = (0..193).map(|n| format!(":L{n}")).collect::<String>();
    let fields = (0..64)
        .map(|n| format!("p{n}:[timestamp()]"))
        .collect::<Vec<_>>()
        .join(",");
    let template = PreparedGraphVertexMergeText::prepare(
        &format!("MERGE (n{labels} {{{fields}}})"),
        R,
        |kind, name: &str| match kind {
            GraphSymbolKind::Label => name
                .strip_prefix('L')?
                .parse::<u64>()
                .ok()
                .map(|id| GraphSymbol::Label(LabelId(id + 1))),
            GraphSymbolKind::Property => name
                .strip_prefix('p')?
                .parse::<u64>()
                .ok()
                .map(|id| GraphSymbol::Property(PropertyKeyId(id + 1))),
            _ => None,
        },
    )
    .unwrap();
    assert_eq!(
        template.bind_parameters(&arguments).unwrap_err().kind,
        GraphVertexMergeTextErrorKind::Query(GraphPatternTextErrorKind::Build(
            PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Predicates,
                limit: 256,
                observed: 257,
            },
        ))
    );
    let text = "MERGE (n {p:$s+$s,active:[timestamp()]})";
    let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        text,
        R,
        &[("s", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
        symbols,
    )
    .unwrap();
    let large = arguments
        .clone()
        .with_text("s", &"x".repeat(32_768))
        .unwrap();
    assert_eq!(
        template.bind_parameters(&large).unwrap_err().kind,
        GraphVertexMergeTextErrorKind::Query(GraphPatternTextErrorKind::ScalarLiteral)
    );
    let small = arguments.with_text("s", "ok").unwrap();
    assert!(
        template
            .bind_parameters(&small)
            .unwrap()
            .materialize::<(), (), ()>(&mut |_| Ok(()))
            .is_ok()
    );
}

#[test]
fn integer_scalar_and_literal_values_share_match_and_creation_binding() {
    let text = "MERGE (n:Person {p:$p,name:$name,active:TRUE})";
    let text_kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("x").unwrap());
    let calls = Cell::new(0);
    let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        text,
        R,
        &[("name", GqlParameterType::Scalar(text_kind))],
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolved = calls.get();
    assert_eq!(template.statement(), text);
    assert_eq!(template.parameter_schema().len(), 2);
    let args = GqlParameters::new()
        .with_int64("p", 7)
        .unwrap()
        .with_text("name", "Ada")
        .unwrap();
    let first = template.bind_parameters(&args).unwrap();
    let frozen = first.canonical_bytes();
    assert_eq!(
        calls.get(),
        resolved,
        "binding must not re-enter the catalog"
    );
    assert_eq!(first.creation().unwrap().vertices_per_row(), 1);
    assert_eq!(first.creation().unwrap().edges_per_row(), 0);
    assert_eq!(first.target_column(), 0);
    assert_eq!(
        template.bind_parameters(&args).unwrap().canonical_bytes(),
        frozen
    );
    assert!(!format!("{template:?} {first:?}").contains("Ada"));

    let renamed = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        "MERGE (renamed:Person {p:$p,name:$name,active:TRUE})",
        R,
        &[("name", GqlParameterType::Scalar(text_kind))],
        symbols,
    )
    .unwrap()
    .bind_parameters(&args)
    .unwrap();
    assert_eq!(
        first.canonical_bytes(),
        renamed.canonical_bytes(),
        "variable spelling is not semantic"
    );
}

#[test]
fn null_keys_wrong_types_and_extra_arguments_refuse_at_original_parameter() {
    let text = "MERGE (n:Person {name:$name})";
    let text_kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("x").unwrap());
    let template = PreparedGraphVertexMergeText::prepare_with_parameter_types(
        text,
        R,
        &[("name", GqlParameterType::Scalar(text_kind))],
        symbols,
    )
    .unwrap();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find("$name").unwrap());
    assert!(matches!(
        missing.kind,
        GraphVertexMergeTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let wrong = GqlParameters::new().with_int64("name", 1).unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphVertexMergeTextErrorKind::Query(
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
        )
    ));
    let null = GqlParameters::new()
        .with_scalar("name", CanonicalScalar::Null)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&null).unwrap_err().kind,
        GraphVertexMergeTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
            "non-null MERGE property value"
        ))
    ));
    let extra = GqlParameters::new()
        .with_text("name", "Ada")
        .unwrap()
        .with_int64("extra", 1)
        .unwrap();
    assert!(template.bind_parameters(&extra).is_err());
}

#[test]
fn malformed_duplicate_and_relationship_merge_syntax_refuse_before_catalog_access() {
    for text in [
        "MERGE",
        "MERGE ()",
        "MERGE (n:Person:Person)",
        "MERGE (n:Person {p:1,p:2})",
        "MERGE (n:Person {})",
        "MERGE (n:Person {p:NULL})",
        "MERGE (a)-[:R]->(b)",
        "MERGE (n:Person {p:1}) ON CREATE SET n.p=2",
        "MATCH (n) MERGE (n)",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphVertexMergeText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "malformed MERGE reached catalog: {text}");
    }
    let duplicate = PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:1,p:2})", R, symbols)
        .unwrap_err();
    assert!(matches!(
        duplicate.kind,
        GraphVertexMergeTextErrorKind::InsertBuild(GraphInsertBuildError::DuplicateProperty {
            declaration: 0
        })
    ));
}

#[test]
fn row_merge_target_reuse_requires_every_branch_to_preserve_keys_and_visibility() {
    use fgdb_gql::{
        GraphSymbolResolver, GraphVertexMergeRowAction, GraphWriteChainStep,
        PreparedGraphWriteChainText, ReverseSymbolCatalog,
    };

    struct Catalog;
    impl GraphSymbolResolver for Catalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            symbols(kind, name)
        }

        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::new();
            catalog.insert_property(P, "p");
            catalog.insert_property(NAME, "name");
            catalog.insert_property(ACTIVE, "active");
            Some(catalog)
        }
    }

    for (pattern, actions, expected) in [
        ("p:k", "", true),
        (
            "p:k",
            "ON CREATE SET n.active=0 ON MATCH SET n.active=n.active+1",
            true,
        ),
        ("p:k", "ON CREATE SET n.name=[1,2] SET n.active=k+1", true),
        ("p:k,active:7", "SET n.name='payload'", true),
        ("p:k", "ON CREATE SET n.p=2", false),
        ("p:k", "ON MATCH SET n.p=n.p+1", false),
        ("p:k", "ON CREATE SET n.active=0 SET n.p=k", false),
        ("p:k,active:7", "ON MATCH SET n.active=8", false),
        ("p:k", "ON CREATE SET n:Person", false),
        ("p:k", "ON MATCH SET n:Person", false),
        ("p:k", "SET n={active:k}", false),
        ("p:k", "ON MATCH SET n += {active:k}", true),
        ("p:k", "ON MATCH SET n += {p:k}", false),
        (
            "p:k",
            "ON MATCH SET n += CASE WHEN k=1 THEN {active:k} ELSE {} END",
            false,
        ),
    ] {
        let text = format!("UNWIND [1,1] AS k MERGE (n:Person {{{pattern}}}) {actions} RETURN n");
        let chain = PreparedGraphWriteChainText::prepare(&text, R, Catalog)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        let [GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
            panic!("expected one row MERGE query: {text}");
        };
        assert_eq!(query.merge().preserves_match_targets(), expected, "{text}");
        if actions == "ON MATCH SET n += {active:k}" {
            // A literal += map becomes one named assignment. A runtime map
            // above remains a whole-map action and cannot earn target reuse.
            let actions = query.merge().actions().unwrap();
            assert!(actions.on_create.is_empty());
            assert!(actions.after.is_empty());
            assert!(matches!(
                &actions.on_match[..],
                [GraphVertexMergeRowAction::Value {
                    key,
                    inputs: None,
                    ..
                }] if *key == ACTIVE
            ));
        }
    }
}


#[test]
fn target_list_merge_dispatch_preserves_scalar_constants_and_lexical_scopes() {
    use fgdb_gql::PreparedGraphWriteChainText;

    // Existing parameter-batch copied-map dispatch also covers a WITH
    // continuation; restricting the new list proof must preserve that owner.
    assert!(PreparedGraphWriteChainText::is_chain(
        "UNWIND $rows AS row MERGE (n:Person {p:row.p}) WITH n,row SET n += row"
    ).unwrap());

    for head in [
        "MERGE (n:Person {p:1})",
        "MATCH (a),(b) MERGE (a)-[n:R]->(b)",
    ] {
        for tail in [
            "SET n.active=n.active+1,n.name=[1,2] RETURN n.name",
            "SET n.name=[1,2],n.active=n.active+1 RETURN n.name",
            "SET n.active=size([1,2])+n.active RETURN [n.active]",
            "SET n.name=['n.active + [1]', {n:7}.n] RETURN n.active",
            "SET n.name=[n IN [{p:1}] | n.p] RETURN n.name",
            "SET n.name=[n IN [{p:1}] WHERE n.p=1 | n.p] RETURN n.name",
            "SET n.name=[reduce(n={p:0}, x IN [1] | {p:n.p+x}).p] RETURN n.name",
            "ON CREATE SET n.name=[1] ON MATCH SET n.active=n.active+1 RETURN n.name",
            "SET n.active=n.active+1 RETURN [n.active]",
            "SET n.active=n.active+1; MERGE (m) SET m.name=[m.p]",
        ] {
            let text = format!("{head} {tail}");
            assert!(!PreparedGraphWriteChainText::is_chain(&text).unwrap(), "{text}");
        }
        for rhs in [
            "[n.p]",
            "n.name+[$value]",
            "n.name+[{p:n.p}.p]",
            "tail(n.name)",
            "reverse(n.name)",
            "split(n.name, ',')",
            "[x IN n.name | x]",
            "[n IN n.name | n]",
            "[reduce(n=n.name, x IN [1] | n+x)]",
            "[[n IN [{p:1}] | n.p], n.p]",
            "CASE WHEN n.active=1 THEN [n.p] ELSE [] END",
        ] {
            let text = format!("{head} SET n.name={rhs} RETURN n.name");
            assert!(PreparedGraphWriteChainText::is_chain(&text).unwrap(), "{text}");
        }
    }
}

#[test]
fn target_list_merge_binds_stored_frames_without_reentering_the_catalog() {
    use fgdb_gql::{
        GraphEdgeMergeRowAction, GraphVertexMergeRowAction, GraphVertexMergeRowInput,
        GraphWriteChainStep, PreparedGraphWriteChainText,
    };

    for head in [
        "MERGE (n:Person {p:1})",
        "MATCH (a),(b) MERGE (a)-[n:R]->(b)",
    ] {
        for rhs in [
            "[n.p,n.p]",
            "n.name+[$value]",
            "n.name+[{p:n.p}.p]",
            "tail(n.name)",
            "reverse(n.name)",
            "split(n.name, ',')",
            "[x IN n.name | x]",
            "[n IN n.name | n]",
            "CASE WHEN n.active=1 THEN [n.p] ELSE [] END",
        ] {
            let text = format!("{head} ON MATCH SET n.name={rhs} RETURN n.name");
            let calls = Cell::new(0);
            let template = PreparedGraphWriteChainText::prepare(&text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                if kind == GraphSymbolKind::Relation && name == "R" {
                    Some(GraphSymbol::Relation(R))
                } else {
                    symbols(kind, name)
                }
            })
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            let prepared_calls = calls.get();
            for value in [2, 7] {
                let arguments = if rhs.contains("$value") {
                    GqlParameters::new().with_int64("value", value).unwrap()
                } else {
                    GqlParameters::new()
                };
                let frozen = arguments.canonical_bytes();
                let bound = template
                    .bind_parameters(&arguments)
                    .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                let inputs = match bound.steps() {
                    [GraphWriteChainStep::VertexMergeQuery(query)] => {
                        let actions = query.merge().actions().unwrap();
                        assert!(actions.on_create.is_empty() && actions.after.is_empty());
                        let [GraphVertexMergeRowAction::Value {
                            key, inputs: Some(inputs), ..
                        }] = &actions.on_match[..] else {
                            panic!("expected one stored vertex frame: {text}");
                        };
                        assert_eq!(*key, NAME);
                        inputs
                    }
                    [GraphWriteChainStep::EdgeUpsertQuery { upsert, .. }] => {
                        let actions = upsert.actions();
                        assert!(actions.on_create.is_empty() && actions.after.is_empty());
                        let [GraphEdgeMergeRowAction::Value {
                            key, inputs: Some(inputs), ..
                        }] = &actions.on_match[..] else {
                            panic!("expected one stored relationship frame: {text}");
                        };
                        assert_eq!(*key, NAME);
                        inputs
                    }
                    _ => panic!("expected one native MERGE step: {text}"),
                };
                assert!(!inputs.is_empty(), "{text}");
                assert!(inputs.iter().all(|input| matches!(
                    input, GraphVertexMergeRowInput::Property(_)
                )), "{text}");
                if rhs == "[n.p,n.p]" {
                    assert_eq!(inputs, &[GraphVertexMergeRowInput::Property(P)]);
                }
                assert_eq!(calls.get(), prepared_calls, "{text}");
                assert_eq!(arguments.canonical_bytes(), frozen);
            }
        }
    }
    for rhs in ["[n.p,other.p]", "n.name+[{p:other.p}.p]"] {
        let text = format!("MERGE (n:Person {{p:1}}) SET n.name={rhs}");
        let calls = Cell::new(0);
        assert!(PreparedGraphWriteChainText::prepare(&text, R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls.get(), 0, "{text}");
    }
}
