//! Native mutation text and public GLA execution, without a separate matcher.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationBatch,
    GraphMutationBuildError, GraphMutationError, GraphMutationIntent, GraphMutationPolicy,
    GraphMutationTextErrorKind, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    MAX_GRAPH_MUTATION_ACTIONS, PreparedGraphMutation, PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const OLD: PropertyKeyId = PropertyKeyId(3);
const TEXT: PropertyKeyId = PropertyKeyId(4);
const UPDATED: LabelId = LabelId(1);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Triple = (VId, RelationId, VId);
type MutationResult<C = ()> = Result<GraphMutationBatch, GqlQueryError<GraphMutationError<()>, C>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "old") => Some(GraphSymbol::Property(OLD)),
        (GraphSymbolKind::Property, "text") => Some(GraphSymbol::Property(TEXT)),
        (GraphSymbolKind::Label, "Updated") => Some(GraphSymbol::Label(UPDATED)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphMutation {
    PreparedGraphMutationText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(
        GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000),
        1_000,
    )
}
fn run(
    plan: &PreparedGraphMutation,
    vertices: &[VId],
    edges: &[Triple],
    props: &Props,
    policy: GraphMutationPolicy,
) -> MutationResult {
    plan.execute_governed(
        policy,
        |selection, budget| {
            selection.plan().execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |vid, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            [],
                            props.iter().filter_map(|(&(owner, key), value)| {
                                (owner == vid).then_some((key, value))
                            }),
                        )
                    }))
                },
                |vid, key| Ok(props.get(&(vid, key))),
                budget,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    )
}
fn props() -> Props {
    BTreeMap::from([
        ((VId(1), P), CanonicalScalar::Int(10)),
        ((VId(2), P), CanonicalScalar::Int(20)),
        ((VId(3), P), CanonicalScalar::Int(30)),
    ])
}

#[test]
fn reciprocal_updates_freeze_rhs_and_collapse_parallel_matches() {
    let plan = prepare("MATCH (a)-[:R]->(b) SET a.p=b.p,b.p=a.p");
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(1)),
    ];
    let result = run(&plan, &[VId(1), VId(2)], &edges, &props(), policy()).unwrap();
    assert_eq!(
        result.intents(),
        &[
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: P,
                value: Some(CanonicalScalar::Int(20))
            },
            GraphMutationIntent::Property {
                vertex: VId(2),
                key: P,
                value: Some(CanonicalScalar::Int(10))
            },
        ]
    );
    assert_eq!(result.stats().selection.result_rows, 3);
    assert_eq!(
        (result.stats().target_vertices, result.stats().effects),
        (2, 2)
    );
    let renamed = prepare("MATCH (x)-[:R]->(y) SET x.p=y.p,y.p=x.p");
    assert_eq!(plan.canonical_bytes(), renamed.canonical_bytes());
}

#[test]
fn literal_map_updates_share_frozen_values_across_targets_and_other_assignments() {
    let plan = prepare(
        "MATCH (a)-[:R]->(b) SET a += {p: b.p + 1, q: a.p}, \
         b += {p: a.p + 1, q: b.p}, a:Updated",
    );
    let direct = prepare(
        "MATCH (a)-[:R]->(b) SET a.p=b.p+1,a.q=a.p, \
         b.p=a.p+1,b.q=b.p,a:Updated",
    );
    assert_eq!(plan.canonical_bytes(), direct.canonical_bytes());
    let result = run(
        &plan,
        &[VId(1), VId(2)],
        &[(VId(1), R, VId(2)), (VId(1), R, VId(2))],
        &props(),
        policy(),
    )
    .unwrap();
    assert_eq!(result.stats().selection.result_rows, 2);
    assert_eq!(result.stats().effects, 5);
    for (vertex, key, value) in [(1, P, 21), (1, Q, 10), (2, P, 11), (2, Q, 20)] {
        assert!(result.intents().contains(&GraphMutationIntent::Property {
            vertex: VId(vertex),
            key,
            value: Some(CanonicalScalar::Int(value)),
        }));
    }
    assert!(result.intents().contains(&GraphMutationIntent::Label {
        vertex: VId(1),
        label: UPDATED,
        present: true,
    }));

    let edge_map = prepare("MATCH (a)-[e:R]->(b) SET e += {p: a.p, q: NULL}");
    let edge_direct = prepare("MATCH (a)-[e:R]->(b) SET e.p=a.p,e.q=NULL");
    assert_eq!(edge_map.canonical_bytes(), edge_direct.canonical_bytes());
}

#[test]
fn literal_map_parameters_nulls_and_explicit_removals_keep_the_existing_contract() {
    let text = "MATCH (a) WHERE a.p >= $n SET a += {q: $n, text: $t, p: NULL} \
                REMOVE a.old";
    let payload = "value }, a.p=0 REMOVE a.text";
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let mut calls = BTreeMap::new();
    let template = PreparedGraphMutationText::prepare_with_parameter_types(
        text,
        R,
        &[("t", GqlParameterType::Scalar(kind))],
        |kind, name| {
            *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(calls.len(), 4);
    assert!(calls.values().all(|count| *count == 1));
    assert_eq!(template.parameter_schema()[0].occurrences, 2);
    assert_eq!(template.parameter_schema()[1].occurrences, 1);
    let arguments = GqlParameters::new()
        .with_int64("n", 10)
        .unwrap()
        .with_text("t", payload)
        .unwrap();
    let plan = template.bind_parameters(&arguments).unwrap();
    let result = run(&plan, &[VId(1)], &[], &props(), policy()).unwrap();
    assert_eq!(
        result.intents(),
        &[
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: P,
                value: Some(CanonicalScalar::Null),
            },
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: Q,
                value: Some(CanonicalScalar::Int(10)),
            },
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: OLD,
                value: None,
            },
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: TEXT,
                value: Some(CanonicalScalar::ucs_basic_text(payload).unwrap()),
            },
        ]
    );
    assert_eq!(template.statement(), text);
    assert!(!format!("{template:?} {plan:?} {result:?}").contains(payload));
    let missing = template
        .bind_parameters(&GqlParameters::new().with_int64("n", 10).unwrap())
        .unwrap_err();
    assert_eq!(missing.offset, text.find("$t").unwrap());
    assert!(matches!(
        missing.kind,
        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let wrong = GqlParameters::new()
        .with_int64("n", 10)
        .unwrap()
        .with_bool("t", true)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })
    ));
}

#[test]
fn literal_maps_refuse_malformed_and_unsupported_shapes_before_catalog_access() {
    for text in [
        "MATCH (a) SET a += {}",
        "MATCH (a) SET a.p=1,a += {}",
        "MATCH (a) SET a += $map",
        "MATCH (a) SET a += a.p",
        "MATCH (a) SET a = {p: 1}",
        "MATCH (a) REMOVE a += {p: 1}",
        "MATCH (a) SET a + {p: 1}",
        "MATCH (a) SET a += {p 1}",
        "MATCH (a) SET a += {p:}",
        "MATCH (a) SET a += {p: 1,}",
        "MATCH (a) SET a += {p: 1 q: 2}",
        "MATCH (a) SET a += {p: 1",
        "MATCH (a) SET a += {p: 1},",
        "MATCH (a) SET a += {p: 1, p: 1}",
        "MATCH (a) SET a += {p: 1, p: 2}",
        "MATCH (a) SET a += {p: {q: 1}}",
        "MATCH (a) SET a += {p: [1, 2]}",
        "MATCH (a) SET a += {p: 1} RETURN a",
        "MATCH (a) SET a += {p: 1} DETACH DELETE a",
        "MATCH (a) SET missing += {p: 1}",
        "MATCH (a) SET a += {p: missing.p}",
        "MATCH (a) WHERE EXISTS { MATCH (local) } SET local += {p: 1}",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphMutationText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let duplicate = "MATCH (a) SET a += {p: 1, p: 2}";
    let error = PreparedGraphMutationText::prepare(duplicate, R, symbols).unwrap_err();
    assert_eq!(error.offset, duplicate.rfind("p:").unwrap());
    assert!(matches!(
        error.kind,
        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
            "distinct property keys in SET += map"
        ))
    ));
}

#[test]
fn literal_map_fields_share_the_complete_statement_action_bound() {
    let fields = |count| {
        (0..count)
            .map(|index| format!("p{index}: {index}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let exact = format!(
        "MATCH (a) SET a += {{{}}}",
        fields(MAX_GRAPH_MUTATION_ACTIONS)
    );
    let plan = PreparedGraphMutationText::prepare(&exact, R, |kind, name| {
        if kind != GraphSymbolKind::Property {
            return None;
        }
        let key = name.strip_prefix('p')?.parse().ok()?;
        Some(GraphSymbol::Property(PropertyKeyId(key)))
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let result = run(&plan, &[VId(1)], &[], &Props::new(), policy()).unwrap();
    assert_eq!(result.stats().effects, MAX_GRAPH_MUTATION_ACTIONS as u64);
    for (index, intent) in result.intents().iter().enumerate() {
        assert_eq!(
            intent,
            &GraphMutationIntent::Property {
                vertex: VId(1),
                key: PropertyKeyId(index as u64),
                value: Some(CanonicalScalar::Int(index as i64)),
            }
        );
    }
    for text in [
        format!(
            "MATCH (a) SET a += {{{}}}",
            fields(MAX_GRAPH_MUTATION_ACTIONS + 1)
        ),
        format!("{exact},a.p=1"),
        format!(
            "MATCH (a) SET a.p=1,a += {{{}}}",
            fields(MAX_GRAPH_MUTATION_ACTIONS)
        ),
        format!(
            "MATCH (a) SET a += {{{}}},a += {{p: 1,q: 2}}",
            fields(MAX_GRAPH_MUTATION_ACTIONS - 1)
        ),
    ] {
        let calls = Cell::new(0);
        let error = PreparedGraphMutationText::prepare(&text, R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap_err();
        assert_eq!(
            error.kind,
            GraphMutationTextErrorKind::Build(GraphMutationBuildError::TooManyActions {
                limit: MAX_GRAPH_MUTATION_ACTIONS,
                observed: MAX_GRAPH_MUTATION_ACTIONS + 1,
            })
        );
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn literal_map_updates_preserve_conflict_and_atomic_proposal_refusals() {
    for text in [
        "MATCH (a) SET a += {p: 1},a += {p: 2}",
        "MATCH (a) SET a += {p: 1},a.p=2",
        "MATCH (a) SET a += {q: NULL} REMOVE a.q",
    ] {
        assert!(matches!(
            run(&prepare(text), &[VId(1)], &[], &props(), policy()),
            Err(GqlQueryError::Source(
                GraphMutationError::ConflictingAssignment { .. }
            ))
        ));
    }
    let arithmetic = prepare("MATCH (a) SET a += {p: 1, q: 1 / (a.p - 10)}");
    assert!(matches!(
        run(&arithmetic, &[VId(1)], &[], &props(), policy()),
        Err(GqlQueryError::Source(GraphMutationError::Arithmetic {
            action: 1,
            ..
        }))
    ));
    let plan = prepare("MATCH (a) SET a += {p: 1, q: COALESCE(a.q, 2)},a += {p: 1}");
    let complete = run(&plan, &[VId(1)], &[], &props(), policy()).unwrap();
    assert_eq!(complete.stats().effects, 2);
    let usage = complete.stats().evaluator;
    let exact = GraphMutationPolicy::new(
        GqlQueryPolicy::new(1, 1, usage.work_units, usage.scratch_entries),
        2,
    );
    assert_eq!(
        run(&plan, &[VId(1)], &[], &props(), exact)
            .unwrap()
            .intents(),
        complete.intents()
    );
    for cap in [
        GraphMutationPolicy::new(exact.query, 1),
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(1, 1, usage.work_units - 1, usage.scratch_entries),
            2,
        ),
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(1, 1, usage.work_units, usage.scratch_entries - 1),
            2,
        ),
    ] {
        assert!(run(&plan, &[VId(1)], &[], &props(), cap).is_err());
    }
}

#[test]
fn inconsistent_assignments_refuse_across_rows_or_aliases_instead_of_choosing_order() {
    let plan = prepare("MATCH (a)-[:R]->(b) SET a.p=b.p");
    for edges in [
        [(VId(1), R, VId(2)), (VId(1), R, VId(3))],
        [(VId(1), R, VId(3)), (VId(1), R, VId(2))],
    ] {
        assert!(matches!(
            run(&plan, &[VId(1), VId(2), VId(3)], &edges, &props(), policy()),
            Err(GqlQueryError::Source(
                GraphMutationError::ConflictingAssignment { .. }
            ))
        ));
    }
    let aliases = prepare("MATCH (a),(b) WHERE a=b SET a.p=1,b.p=2");
    assert!(matches!(
        run(&aliases, &[VId(1)], &[], &props(), policy()),
        Err(GqlQueryError::Source(
            GraphMutationError::ConflictingAssignment {
                first_row: 0,
                row: 0,
                first_action: 0,
                action: 1,
            }
        ))
    ));
}

#[test]
fn optional_null_targets_are_skipped_and_missing_rhs_is_a_canonical_null() {
    let plan = prepare("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) SET b.q=a.p");
    let result = run(
        &plan,
        &[VId(1), VId(2), VId(3)],
        &[(VId(1), R, VId(2))],
        &props(),
        policy(),
    )
    .unwrap();
    assert_eq!(result.stats().selection.result_rows, 3);
    assert_eq!(
        result.intents(),
        &[GraphMutationIntent::Property {
            vertex: VId(2),
            key: Q,
            value: Some(CanonicalScalar::Int(10)),
        }]
    );
    let missing = prepare("MATCH (a) SET a.p=a.q REMOVE a.old");
    let result = run(&missing, &[VId(1)], &[], &props(), policy()).unwrap();
    assert_eq!(
        result.intents(),
        &[
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: P,
                value: Some(CanonicalScalar::Null)
            },
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: OLD,
                value: None
            },
        ]
    );
    let conflicting_null = prepare("MATCH (a) SET a.q=NULL REMOVE a.q");
    assert!(matches!(
        run(&conflicting_null, &[VId(1)], &[], &props(), policy()),
        Err(GqlQueryError::Source(
            GraphMutationError::ConflictingAssignment { .. }
        ))
    ));
}

#[test]
fn bounded_walk_targets_keep_occurrences_but_each_delete_is_proposed_once() {
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    let plan = prepare("MATCH REPEATABLE ELEMENTS WALK (a)-[:R*0..2]->(b) DETACH DELETE b");
    let result = run(&plan, &[VId(1), VId(2)], &edges, &props(), policy()).unwrap();
    assert_eq!(result.stats().selection.result_rows, 11);
    assert_eq!(
        result.intents(),
        &[
            GraphMutationIntent::DetachDelete { vertex: VId(1) },
            GraphMutationIntent::DetachDelete { vertex: VId(2) },
        ]
    );
    let isolated = run(&plan, &[VId(9)], &[], &Props::new(), policy()).unwrap();
    assert_eq!(
        isolated.intents(),
        &[GraphMutationIntent::DetachDelete { vertex: VId(9) }]
    );
}

#[test]
fn typed_arguments_and_catalog_resolution_are_shared_by_read_and_write_clauses() {
    let text = "MATCH (a) WHERE a.p >= $n SET a.q=$n,a.text=$t,a:Updated REMOVE a.old";
    let payload = "x' REMOVE a.p SET a.q=$n";
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let mut calls = BTreeMap::new();
    let template = PreparedGraphMutationText::prepare_with_parameter_types(
        text,
        R,
        &[("t", GqlParameterType::Scalar(kind))],
        |kind, name| {
            *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(calls.len(), 5);
    assert!(calls.values().all(|count| *count == 1));
    assert_eq!(template.parameter_schema()[0].occurrences, 2);
    assert_eq!(template.parameter_schema()[1].occurrences, 1);
    let args = GqlParameters::new()
        .with_int64("n", 10)
        .unwrap()
        .with_text("t", payload)
        .unwrap();
    let plan = template.bind_parameters(&args).unwrap();
    let frozen = plan.canonical_bytes();
    assert_eq!(
        frozen,
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    let result = run(&plan, &[VId(1)], &[], &props(), policy()).unwrap();
    assert_eq!(result.stats().effects, 4);
    assert!(result.intents().contains(&GraphMutationIntent::Property {
        vertex: VId(1),
        key: TEXT,
        value: Some(CanonicalScalar::ucs_basic_text(payload).unwrap()),
    }));
    assert!(result.intents().contains(&GraphMutationIntent::Label {
        vertex: VId(1),
        label: UPDATED,
        present: true
    }));
    assert!(!format!("{template:?} {plan:?} {result:?}").contains(payload));
    assert_eq!(template.statement(), text);
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find("$n").unwrap());
    assert!(matches!(
        missing.kind,
        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
    let wrong = GqlParameters::new()
        .with_int64("n", 1)
        .unwrap()
        .with_bool("t", true)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })
    ));
    assert!(
        template
            .bind_parameters(&args.clone().with_int64("extra", 1).unwrap())
            .is_err()
    );
    assert_eq!(plan.canonical_bytes(), frozen);
}

#[test]
fn malformed_unsupported_and_oversized_mutations_refuse_before_catalog_callbacks() {
    for text in [
        "MATCH (a) DELETE a",
        "MATCH (a) SET a.p=1 DETACH DELETE a",
        "MATCH (a) DETACH DELETE a SET a.p=1",
        "MATCH (a) SET a.p=1 RETURN a",
        "MATCH (a) SET a.p=a.p+*1",
        "MATCH (a) SET a.p=1,",
        "MATCH (a) REMOVE",
        "MATCH (a) SET missing.p=1",
        "MATCH (a) SET a.p=other.p",
        "MATCH (a) WHERE EXISTS { MATCH (local) } SET local.p=1",
        "MATCH (a) SET a.p=9223372036854775808",
        "MATCH (a) SET a.p=$ value",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphMutationText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let text = format!(
        "MATCH (a) SET {}",
        vec!["a.p=1"; MAX_GRAPH_MUTATION_ACTIONS + 1].join(",")
    );
    let calls = Cell::new(0);
    let failed = PreparedGraphMutationText::prepare(&text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap_err();
    assert!(matches!(
        failed.kind,
        GraphMutationTextErrorKind::Build(GraphMutationBuildError::TooManyActions { .. })
    ));
    assert_eq!(calls.get(), 0);
    let unicode = "\u{2003}MATCH (a) SET a.p=$missing";
    let template = PreparedGraphMutationText::prepare(unicode, R, symbols).unwrap();
    assert_eq!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .offset,
        unicode.find('$').unwrap()
    );
}

#[test]
fn exact_proposal_budgets_include_source_work_and_scalar_payload_copies() {
    let plan = prepare("MATCH (a) SET a.text='a moderately sized canonical payload',a:Updated");
    let wide = run(&plan, &[VId(1), VId(2)], &[], &Props::new(), policy()).unwrap();
    let stats = wide.stats();
    let exact = GraphMutationPolicy::new(
        GqlQueryPolicy::new(
            2,
            2,
            stats.evaluator.work_units,
            stats.evaluator.scratch_entries,
        ),
        4,
    );
    assert_eq!(
        run(&plan, &[VId(1), VId(2)], &[], &Props::new(), exact)
            .unwrap()
            .stats(),
        stats
    );
    for cap in [
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(2, 2, stats.evaluator.work_units - 1, u64::MAX),
            4,
        ),
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(2, 2, u64::MAX, stats.evaluator.scratch_entries - 1),
            4,
        ),
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(2, 2, u64::MAX, stats.evaluator.scratch_entries - 1),
            4,
        ),
        GraphMutationPolicy::new(GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX), 3),
    ] {
        assert!(run(&plan, &[VId(1), VId(2)], &[], &Props::new(), cap).is_err());
    }
}

#[test]
fn every_selection_and_proposal_checkpoint_can_interrupt_without_returning_a_batch() {
    let plan = prepare("MATCH (a)-[:R]->(b) SET a.q=b.p,a:Updated");
    let properties = props();
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2))];
    let execute = |stop: usize| {
        let calls = Cell::new(0);
        let checkpoint = || {
            let at = calls.get() + 1;
            calls.set(at);
            if at == stop { Err(stop) } else { Ok(()) }
        };
        let result: MutationResult<usize> = plan.execute_governed(
            policy(),
            |selection, budget| {
                selection.plan().execute_governed_with_properties(
                    2,
                    [],
                    edges,
                    |_, _| Ok::<_, ()>(true),
                    |vid, key| Ok(properties.get(&(vid, key))),
                    budget,
                    checkpoint,
                )
            },
            checkpoint,
        );
        (result, calls.get())
    };
    let (complete, total) = execute(0);
    assert_eq!(complete.unwrap().stats().effects, 2);
    for stop in 1..=total {
        let (result, calls) = execute(stop);
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(calls, stop);
    }
    let late_failure: MutationResult<&str> = plan.execute_governed(
        policy(),
        |_, _| Err(GqlQueryError::Interrupted("late source failure")),
        || Ok(()),
    );
    assert!(matches!(
        late_failure,
        Err(GqlQueryError::Interrupted("late source failure"))
    ));
}
