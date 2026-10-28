//! Ranked row pipelines feed the existing simultaneous assignment reducer.
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphMutationAction as Action, GraphMutationBatch, GraphMutationBuildError, GraphMutationError,
    GraphMutationIntent, GraphMutationPolicy, GraphMutationValue as Value, GraphSetExecutionError,
    GraphSetOperation, GraphSetQuantifier, GraphSetValue, GraphSymbol, GraphSymbolKind,
    MAX_GRAPH_SET_DEPTH, PreparedGraphMutation, PreparedGraphSet, PreparedGraphSetText,
    PreparedGraphText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
use std::cell::Cell;

/// A written scalar as the stored property value an intent carries.
fn stored(value: CanonicalScalar) -> Option<CanonicalPropertyValue> {
    Some(CanonicalPropertyValue::Scalar(value))
}

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type Edge = (VId, RelationId, VId);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn relation(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn wide() -> GraphMutationPolicy {
    GraphMutationPolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000),
        1_000,
    )
}
fn set(target: usize, value: usize) -> Action {
    Action::SetProperty {
        target,
        key: Q,
        value: Value::Column(value),
    }
}
fn execute<C>(
    mutation: &PreparedGraphMutation,
    values: &[CanonicalScalar],
    edges: &[Edge],
    policy: GraphMutationPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<()>, C>> {
    let checkpoint = std::cell::RefCell::new(&mut checkpoint);
    let stored: Vec<_> = values
        .iter()
        .cloned()
        .map(CanonicalPropertyValue::from)
        .collect();
    mutation.execute_governed_over_reading_with_source(
        None,
        policy,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_properties(
                (values.len() + edges.len()) as u64,
                (0..values.len()).map(|i| VId(i as u128)),
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                |vid, key| {
                    Ok(if key == P {
                        stored.get(vid.0 as usize)
                    } else {
                        None
                    })
                },
                remaining,
                || (*checkpoint.borrow_mut())(),
            )
        },
        |element, key| {
            Ok(match element {
                ElementId::Vertex(vid) if key == P => stored.get(vid.0 as usize).cloned(),
                _ => None,
            })
        },
        || (*checkpoint.borrow_mut())(),
    )
}
fn run(
    mutation: &PreparedGraphMutation,
    values: &[CanonicalScalar],
    edges: &[Edge],
    policy: GraphMutationPolicy,
) -> GraphMutationBatch {
    execute(mutation, values, edges, policy, || Ok::<_, ()>(())).unwrap()
}

#[test]
fn ranked_computed_columns_are_the_actual_assignment_schema() {
    let input = relation(
        "MATCH (n) WITH n AS target,n.p AS x WHERE x>0 RETURN x+1 AS value,target ORDER BY value DESC LIMIT 2",
    );
    let leaf = input.clone();
    let mutation = PreparedGraphMutation::prepare_relation(input, R, vec![set(1, 0)]).unwrap();
    assert_eq!(mutation.input_relation(), Some(&leaf));
    let values = [-1, 2, 7, 4].map(CanonicalScalar::Int);
    let result = run(
        &mutation,
        &values,
        &[],
        GraphMutationPolicy::new(GqlQueryPolicy::new(4, 2, 100_000, 100_000), 2),
    );
    assert_eq!(
        result.intents(),
        &[
            GraphMutationIntent::Property {
                vertex: VId(2),
                key: Q,
                value: stored(CanonicalScalar::Int(8))
            },
            GraphMutationIntent::Property {
                vertex: VId(3),
                key: Q,
                value: stored(CanonicalScalar::Int(5))
            },
        ]
    );
    assert_eq!(result.stats().selection.result_rows, 2);
    assert_eq!(result.stats().effects, 2);
}

#[test]
fn generated_filter_rank_pages_match_independent_target_value_oracle() {
    for code in 0..81 {
        let mut digits = code;
        let mut values = Vec::new();
        for _ in 0..4 {
            values.push([-2, 0, 5][digits % 3]);
            digits /= 3;
        }
        for skip in 0..3 {
            for count in 0..3 {
                let input = relation(&format!(
                    "MATCH (n) WITH n AS target,n.p AS x WHERE x>=0 RETURN target,x ORDER BY x DESC SKIP {skip} LIMIT {count}"
                ));
                let mutation =
                    PreparedGraphMutation::prepare_relation(input, R, vec![set(0, 1)]).unwrap();
                let mut expected = values
                    .iter()
                    .copied()
                    .enumerate()
                    .filter(|(_, x)| *x >= 0)
                    .collect::<Vec<_>>();
                expected.sort_by_key(|(target, value)| (std::cmp::Reverse(*value), *target));
                let mut expected = expected
                    .into_iter()
                    .skip(skip)
                    .take(count)
                    .collect::<Vec<_>>();
                expected.sort_by_key(|(target, _)| *target);
                let expected = expected
                    .into_iter()
                    .map(|(target, value)| GraphMutationIntent::Property {
                        vertex: VId(target as u128),
                        key: Q,
                        value: stored(CanonicalScalar::Int(value)),
                    })
                    .collect::<Vec<_>>();
                let scalars = values
                    .iter()
                    .copied()
                    .map(CanonicalScalar::Int)
                    .collect::<Vec<_>>();
                assert_eq!(run(&mutation, &scalars, &[], wide()).intents(), expected);
            }
        }
    }
}

#[test]
fn duplicate_rows_collapse_equal_assignments_but_refuse_conflicting_values() {
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(0), R, VId(2)),
    ];
    let same = [0, 7, 7].map(CanonicalScalar::Int);
    let different = [0, 7, 9].map(CanonicalScalar::Int);
    for text in [
        "MATCH (a)-[:R]->(b) WITH a AS target,b.p AS x RETURN target,x",
        "MATCH (a) WITH a MATCH (a)-[:R]->(b) WITH a AS target,b.p AS x RETURN target,x",
    ] {
        let mutation =
            PreparedGraphMutation::prepare_relation(relation(text), R, vec![set(0, 1)]).unwrap();
        let result = run(&mutation, &same, &edges, wide());
        assert_eq!(result.stats().selection.result_rows, 3, "{text}");
        assert_eq!(result.stats().effects, 1, "{text}");
        assert_eq!(
            result.intents(),
            &[GraphMutationIntent::Property {
                vertex: VId(0),
                key: Q,
                value: stored(CanonicalScalar::Int(7)),
            }],
            "{text}",
        );
        assert!(matches!(
            execute(&mutation, &different, &edges, wide(), || Ok::<_, ()>(())),
            Err(GqlQueryError::Source(
                GraphMutationError::ConflictingAssignment { .. }
            ))
        ));
    }
}

#[test]
fn null_targets_do_not_execute_rhs_and_detach_uses_the_selected_vertex() {
    let division = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ])
    .unwrap();
    for text in [
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WITH b AS target RETURN target",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) WITH b AS target RETURN target",
    ] {
        let mutation = PreparedGraphMutation::prepare_relation(
            relation(text),
            R,
            vec![Action::SetProperty {
                target: 0,
                key: Q,
                value: Value::Expression(division.clone()),
            }],
        )
        .unwrap();
        let batch = run(&mutation, &[CanonicalScalar::Int(4)], &[], wide());
        assert!(batch.intents().is_empty(), "{text}");
        assert_eq!(batch.stats().selection.result_rows, 1, "{text}");
    }
    let input =
        relation("MATCH (a) WITH a AS target,a.p AS x ORDER BY x DESC LIMIT 1 RETURN target");
    let deletion =
        PreparedGraphMutation::prepare_relation(input, R, vec![Action::DetachDelete { target: 0 }])
            .unwrap();
    assert_eq!(
        run(
            &deletion,
            &[CanonicalScalar::Int(2), CanonicalScalar::Int(9)],
            &[],
            wide()
        )
        .intents(),
        &[GraphMutationIntent::DetachDelete { vertex: VId(1) }]
    );
}

#[test]
fn multipart_nullable_targets_remove_and_delete_once_per_element() {
    let values = [0, 1, 2].map(CanonicalScalar::Int);
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    for (text, selected, empty_selected, dynamic) in [
        (
            "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN b AS target",
            4,
            3,
            false,
        ),
        (
            "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) \
             UNWIND [b,b,null] AS target RETURN target",
            12,
            9,
            true,
        ),
    ] {
        for action in [
            Action::RemoveProperty { target: 0, key: P },
            Action::Delete { target: 0 },
            Action::DetachDelete { target: 0 },
        ] {
            let expected: Vec<_> = [VId(1), VId(2)]
                .into_iter()
                .map(|vertex| match &action {
                    Action::RemoveProperty { .. } => GraphMutationIntent::Property {
                        vertex,
                        key: P,
                        value: None,
                    },
                    Action::Delete { .. } => GraphMutationIntent::Delete { vertex },
                    Action::DetachDelete { .. } => GraphMutationIntent::DetachDelete { vertex },
                    _ => unreachable!(),
                })
                .collect();
            let action = match (dynamic, action) {
                (true, Action::Delete { .. }) => Action::DeleteValue {
                    value: GraphSetValue::Column(0),
                    detach: false,
                },
                (true, Action::DetachDelete { .. }) => Action::DeleteValue {
                    value: GraphSetValue::Column(0),
                    detach: true,
                },
                (_, action) => action,
            };
            let mutation =
                PreparedGraphMutation::prepare_relation(relation(text), R, vec![action]).unwrap();
            let batch = run(&mutation, &values, &edges, wide());
            assert_eq!(batch.intents(), expected, "{text}");
            assert_eq!(batch.stats().selection.result_rows, selected, "{text}");
            assert_eq!(batch.stats().effects, 2, "{text}");
            let empty = run(&mutation, &values, &[], wide());
            assert!(empty.intents().is_empty(), "{text}");
            assert_eq!(
                empty.stats().selection.result_rows,
                empty_selected,
                "{text}"
            );
        }
    }
}

#[test]
fn correlated_read_call_supplies_frozen_mutation_values() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation(
            "MATCH (n) WITH n CALL (n) { MATCH (n)-[:R]->(m) RETURN count(*) AS degree } \
             RETURN n,degree",
        ),
        R,
        vec![set(0, 1)],
    )
    .unwrap();
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    let batch = run(
        &mutation,
        &[0, 0, 0].map(CanonicalScalar::Int),
        &edges,
        wide(),
    );
    assert_eq!(
        batch.intents(),
        &[
            GraphMutationIntent::Property {
                vertex: VId(0),
                key: Q,
                value: stored(CanonicalScalar::Int(2)),
            },
            GraphMutationIntent::Property {
                vertex: VId(1),
                key: Q,
                value: stored(CanonicalScalar::Int(1)),
            },
            GraphMutationIntent::Property {
                vertex: VId(2),
                key: Q,
                value: stored(CanonicalScalar::Int(0)),
            },
        ]
    );
}

#[test]
fn multipart_sources_and_reduction_share_limits_and_cancellation() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation(
            "MATCH (n) WITH n AS target,n.p+1 AS x MATCH (target) \
             WITH target,x WHERE x>0 RETURN target,x",
        ),
        R,
        vec![set(0, 1)],
    )
    .unwrap();
    let values = [1, 3, 5].map(CanonicalScalar::Int);
    let checkpoints = Cell::new(0);
    let measured = execute(&mutation, &values, &[], wide(), || {
        checkpoints.set(checkpoints.get() + 1);
        Ok::<_, usize>(())
    })
    .unwrap();
    let stats = measured.stats();
    assert_eq!(stats.selection.snapshot_records, 6);
    assert_eq!(stats.selection.result_rows, 3);
    assert_eq!(stats.effects, 3);
    let caps = [
        stats.selection.snapshot_records,
        stats.selection.result_rows,
        stats.evaluator.work_units,
        stats.evaluator.scratch_entries,
        stats.effects,
    ];
    let policy = |caps: [u64; 5]| {
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            caps[4],
        )
    };
    assert_eq!(run(&mutation, &values, &[], policy(caps)).stats(), stats);
    for dimension in 0..5 {
        let mut less = caps;
        less[dimension] -= 1;
        assert!(execute(&mutation, &values, &[], policy(less), || Ok::<_, ()>(())).is_err());
    }
    for stop in 1..=checkpoints.get() {
        let mut at = 0;
        let result = execute(&mutation, &values, &[], wide(), || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(observed)) if observed == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn a_later_graph_source_failure_returns_no_mutation_even_with_limit_zero() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation("MATCH (a) WITH a MATCH (b) RETURN a AS target,b.p AS x LIMIT 0"),
        R,
        vec![set(0, 1)],
    )
    .unwrap();
    let mut calls = 0;
    let result = mutation.execute_governed_over_reading_with_source(
        None,
        wide(),
        |pattern, remaining| {
            calls += 1;
            if calls == 2 {
                return Err(GqlQueryError::Source("second graph source unavailable"));
            }
            pattern.plan().execute_governed_with_properties(
                1,
                [VId(0)],
                [],
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(None),
                remaining,
                || Ok::<_, ()>(()),
            )
        },
        |_, _| Ok(None),
        || Ok::<_, ()>(()),
    );
    assert_eq!(calls, 2);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphMutationError::InputRelation(
            GraphSetExecutionError::Source("second graph source unavailable")
        )))
    ));
}

#[test]
fn the_legacy_one_shot_source_refuses_reuse_without_panicking() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation("MATCH (a) WITH a MATCH (b) RETURN a AS target,b.p AS x"),
        R,
        vec![set(0, 1)],
    )
    .unwrap();
    let calls = Cell::new(0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mutation.execute_governed(
            wide(),
            |pattern, remaining| {
                calls.set(calls.get() + 1);
                pattern.plan().execute_governed_with_properties(
                    1,
                    [VId(0)],
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        )
    }));
    assert!(
        result.is_ok(),
        "a one-shot host must receive a typed refusal"
    );
    assert!(matches!(
        result.unwrap(),
        Err(GqlQueryError::Source(
            GraphMutationError::ReusableSourceRequired
        ))
    ));
    assert_eq!(calls.get(), 1);
}

#[test]
fn all_phases_share_limits_and_every_interruption_returns_no_batch() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation("MATCH (n) WITH n AS target,n.p+1 AS x WHERE x>0 RETURN target,x"),
        R,
        vec![
            set(0, 1),
            Action::SetLabel {
                target: 0,
                label: LabelId(1),
                present: true,
            },
        ],
    )
    .unwrap();
    let values = [1, 3, 5].map(CanonicalScalar::Int);
    let checkpoints = Cell::new(0);
    let measured = execute(&mutation, &values, &[], wide(), || {
        checkpoints.set(checkpoints.get() + 1);
        Ok::<_, usize>(())
    })
    .unwrap();
    let stats = measured.stats();
    let caps = [
        stats.selection.snapshot_records,
        stats.selection.result_rows,
        stats.evaluator.work_units,
        stats.evaluator.scratch_entries,
        stats.effects,
    ];
    let policy = |caps: [u64; 5]| {
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            caps[4],
        )
    };
    assert_eq!(run(&mutation, &values, &[], policy(caps)).stats(), stats);
    for dimension in 0..5 {
        let mut less = caps;
        less[dimension] -= 1;
        assert!(execute(&mutation, &values, &[], policy(less), || Ok::<_, ()>(())).is_err());
    }
    for stop in 1..=checkpoints.get() {
        let mut at = 0;
        let result = execute(&mutation, &values, &[], wide(), || {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(observed)) if observed == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn input_failures_remain_typed_and_zero_final_rows_cannot_hide_them() {
    let mutation = PreparedGraphMutation::prepare_relation(
        relation("MATCH (n) WITH n AS target,n.p AS x RETURN target,10/x AS value LIMIT 0"),
        R,
        vec![set(0, 1)],
    )
    .unwrap();
    assert!(matches!(
        execute(&mutation, &[CanonicalScalar::Int(0)], &[], wide(), || Ok::<
            _,
            (),
        >(
            ()
        )),
        Err(GqlQueryError::Source(GraphMutationError::InputRelation(
            GraphSetExecutionError::Projection { .. }
        )))
    ));
    let failed = mutation.execute_governed(
        wide(),
        |_, _| {
            Err::<GqlQueryExecution<GraphValueRow>, _>(GqlQueryError::Source("source unavailable"))
        },
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        failed,
        Err(GqlQueryError::Source(GraphMutationError::InputRelation(
            GraphSetExecutionError::Source("source unavailable")
        )))
    ));
    let invalid = mutation.execute_governed(
        wide(),
        |_, _| {
            Ok::<_, GqlQueryError<(), ()>>(GqlQueryExecution {
                value: vec![],
                rows: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 1,
                },
                evaluator: GlaExecutionStats::default(),
            })
        },
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        invalid,
        Err(GqlQueryError::Source(GraphMutationError::InputRelation(
            GraphSetExecutionError::InvalidSourceStatistics { .. }
        )))
    ));
    let safe = PreparedGraphMutation::prepare_relation(
        relation("MATCH (n) WITH n AS target,n.p AS x WHERE x<>0 RETURN target,10/x AS value"),
        R,
        vec![set(0, 1)],
    )
    .unwrap();
    assert!(
        run(&safe, &[CanonicalScalar::Int(0)], &[], wide())
            .intents()
            .is_empty()
    );
}

#[test]
fn final_schema_and_parent_depth_are_checked_before_a_source_is_available() {
    let input = relation("MATCH (n) RETURN n AS target,n.p AS x");
    for action in [
        set(1, 0),
        set(0, 0),
        set(usize::MAX, 1),
        Action::DetachDelete { target: 1 },
    ] {
        assert!(PreparedGraphMutation::prepare_relation(input.clone(), R, vec![action]).is_err());
    }
    assert!(matches!(
        PreparedGraphMutation::prepare_relation(
            input.clone(),
            R,
            vec![set(0, 1), Action::DetachDelete { target: 0 }]
        ),
        Err(GraphMutationBuildError::MixedDeletionAndUpdates)
    ));
    let binary = input
        .clone()
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            input.clone(),
        )
        .unwrap();
    let union = PreparedGraphMutation::prepare_relation(binary, R, vec![set(0, 1)]).unwrap();
    let union_batch = run(&union, &[CanonicalScalar::Int(7)], &[], wide());
    assert_eq!(union_batch.stats().selection.result_rows, 2);
    assert_eq!(union_batch.stats().effects, 1);
    let pattern: PreparedGraphPattern<GraphValueRow> =
        PreparedGraphText::prepare("MATCH (n) RETURN n,n.p AS x", symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
    let mut deep: PreparedGraphSet = pattern.into();
    for _ in 1..MAX_GRAPH_SET_DEPTH {
        deep = deep.nested().unwrap();
    }
    assert!(matches!(
        PreparedGraphMutation::prepare_relation(deep, R, vec![set(0, 1)]),
        Err(GraphMutationBuildError::RelationalInput(_))
    ));
}

#[test]
fn ordinary_definition_is_unchanged_and_pipeline_executes_one_real_leaf() {
    let pattern = PreparedGraphText::prepare("MATCH (n) RETURN n AS target,n.p AS x", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let ordinary = PreparedGraphMutation::prepare(pattern.clone(), R, vec![set(0, 1)]).unwrap();
    let pipeline =
        PreparedGraphMutation::prepare_relation(pattern.clone().into(), R, vec![set(0, 1)])
            .unwrap();
    assert!(ordinary.input_relation().is_none());
    assert_eq!(pipeline.selection(), ordinary.selection());
    assert_ne!(pipeline.canonical_bytes(), ordinary.canonical_bytes());
    let values = [CanonicalScalar::Int(7)];
    let stored = values.clone().map(CanonicalPropertyValue::from);
    assert_eq!(
        run(&ordinary, &values, &[], wide()).intents(),
        run(&pipeline, &values, &[], wide()).intents()
    );
    let calls = Cell::new(0);
    pipeline
        .execute_governed(
            wide(),
            |source, remaining| {
                calls.set(calls.get() + 1);
                assert_eq!(source, &pattern);
                source.plan().execute_governed_with_properties(
                    1,
                    [VId(0)],
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(Some(&stored[0])),
                    remaining,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(calls.get(), 1);
    assert!(!format!("{pipeline:?}").contains("target"));
}

mod source_catalogs {
    use super::*;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{
        GraphMutationBinding, GraphMutationQueryError, GraphSetProjection, GraphSymbolResolver,
        PreparedGraphMutationQuery, ReverseSymbolCatalog,
    };

    struct Catalog(ReverseSymbolCatalog);

    impl GraphSymbolResolver for Catalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            match kind {
                GraphSymbolKind::Label => self.0.labels.iter().find_map(|(&id, candidate)| {
                    (candidate == name).then_some(GraphSymbol::Label(id))
                }),
                GraphSymbolKind::Relation => {
                    self.0.relations.iter().find_map(|(&id, candidate)| {
                        (candidate == name).then_some(GraphSymbol::Relation(id))
                    })
                }
                GraphSymbolKind::Property => {
                    self.0.properties.iter().find_map(|(&id, candidate)| {
                        (candidate == name).then_some(GraphSymbol::Property(id))
                    })
                }
            }
        }

        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            Some(self.0.clone())
        }
    }

    fn base_catalog() -> ReverseSymbolCatalog {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(P, "p");
        catalog.insert_label(LabelId(1), "Before");
        catalog.insert_relation(R, "R");
        catalog
    }

    // A computed output (`7 AS value`) needs the set entrypoint, which also
    // keeps the leaf's reverse catalog for the write's input_catalog.
    fn leaf(catalog: ReverseSymbolCatalog) -> PreparedGraphSet {
        PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            "MATCH (n) RETURN n AS target,properties(n) AS current,labels(n) AS labels,7 AS value",
            &[],
            &mut Catalog(catalog),
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }

    #[test]
    fn compatible_leaf_catalogs_supply_final_maps_and_labels_for_every_occurrence() {
        let mut complete = base_catalog();
        complete.insert_property(Q, "q");
        complete.insert_label(LabelId(2), "After");
        let input = leaf(base_catalog())
            .combine(
                GraphSetOperation::Union,
                GraphSetQuantifier::All,
                leaf(complete),
            )
            .unwrap();
        let mutation = PreparedGraphMutation::prepare_relation(
            input,
            R,
            vec![
                Action::ClearProperties {
                    target: 0,
                    current: 1,
                    keep: vec![Q],
                },
                set(0, 3),
                Action::SetLabel {
                    target: 0,
                    label: LabelId(2),
                    present: true,
                },
            ],
        )
        .unwrap();
        let query = PreparedGraphMutationQuery::prepare(
            mutation,
            vec![
                GraphMutationBinding::Properties {
                    target: 0,
                    current: 1,
                },
                GraphMutationBinding::Labels {
                    target: 0,
                    current: 2,
                },
            ],
            vec![
                GraphSetProjection::new("properties", GraphSetValue::Column(0)),
                GraphSetProjection::new("labels", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
        let scalar = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        let text =
            |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        let current = GraphValue::map(vec![("p".into(), scalar(10))]).unwrap();
        let labels = [text("Before")];
        let property = CanonicalPropertyValue::from(CanonicalScalar::Int(10));
        let mut calls = 0;
        let batch = query
            .execute_governed_over_reading_with_source(
                None,
                wide(),
                |pattern, remaining| {
                    calls += 1;
                    pattern.plan().execute_governed_with_element_maps(
                        1,
                        [VId(0)],
                        [],
                        |_, _| Ok::<_, ()>(true),
                        |_, key| Ok((key == P).then_some(&property)),
                        |_, _| Ok(None),
                        |_| Ok(Some(labels.as_slice())),
                        |_| Ok(None),
                        |_| Ok(Some(&current)),
                        |_| Ok(None),
                        remaining,
                        || Ok::<_, ()>(()),
                    )
                },
                |_, _| Ok(None),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(batch.mutation().stats().selection.result_rows, 2);
        assert_eq!(batch.mutation().stats().effects, 3);
        let expected = vec![
            GraphValue::map(vec![("q".into(), scalar(7))]).unwrap(),
            GraphValue::List(vec![text("Before"), text("After")].into_boxed_slice()),
        ];
        assert_eq!(
            batch
                .returning()
                .value
                .iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            vec![expected.clone(), expected],
        );
    }

    #[test]
    fn incompatible_leaf_catalogs_refuse_before_any_source_is_available() {
        for domain in ["property", "label", "relation"] {
            for same_identity in [false, true] {
                let mut other = ReverseSymbolCatalog::new();
                match (domain, same_identity) {
                    ("property", true) => other.insert_property(P, "renamed"),
                    ("property", false) => other.insert_property(Q, "p"),
                    ("label", true) => other.insert_label(LabelId(1), "Renamed"),
                    ("label", false) => other.insert_label(LabelId(2), "Before"),
                    ("relation", true) => other.insert_relation(R, "Renamed"),
                    ("relation", false) => other.insert_relation(RelationId(2), "R"),
                    _ => unreachable!(),
                }
                let input = leaf(base_catalog())
                    .combine(
                        GraphSetOperation::Union,
                        GraphSetQuantifier::All,
                        leaf(other),
                    )
                    .unwrap();
                assert!(
                    matches!(
                        PreparedGraphMutation::prepare_relation(input, R, vec![set(0, 3)]),
                        Err(GraphMutationBuildError::IncompatibleSourceCatalog)
                    ),
                    "{domain}, same identity: {same_identity}",
                );
            }
        }
    }

    #[test]
    fn multipart_computed_maps_admit_actual_duplicate_fields_and_action_only_metadata_names() {
        let scalar = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        let current = GraphValue::map(vec![("p".into(), scalar(10))]).unwrap();
        let labels = [GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text("Before").unwrap(),
        )];
        let property = CanonicalPropertyValue::from(CanonicalScalar::Int(10));
        for replacement in [false, true] {
            let mut second_catalog = base_catalog();
            second_catalog.insert_label(LabelId(2), "After");
            // Neither source names q. Its final RETURN name must come from
            // the computed action's explicitly pinned property catalog.
            let input = leaf(base_catalog())
                .combine(
                    GraphSetOperation::Union,
                    GraphSetQuantifier::All,
                    leaf(second_catalog),
                )
                .unwrap();
            let mut action_catalog = ReverseSymbolCatalog::new();
            action_catalog.insert_property(P, "p");
            action_catalog.insert_property(Q, "q");
            let patch = if replacement {
                GraphValue::map(vec![("q".into(), scalar(7))]).unwrap()
            } else {
                current.clone()
            };
            let mutation = PreparedGraphMutation::prepare_relation(
                input,
                R,
                vec![Action::SetProperties {
                    target: 0,
                    value: GraphSetValue::Value(patch.clone()),
                    current: replacement.then_some(GraphSetValue::Column(1)),
                    catalog: std::sync::Arc::new(action_catalog),
                }],
            )
            .unwrap();
            let query = PreparedGraphMutationQuery::prepare(
                mutation,
                vec![GraphMutationBinding::Properties {
                    target: 0,
                    current: 1,
                }],
                vec![GraphSetProjection::new(
                    "properties",
                    GraphSetValue::Column(0),
                )],
                GraphSetQuantifier::All,
            )
            .unwrap();
            let expected_admission = if replacement {
                vec![Q, P, Q, P]
            } else {
                vec![P, P]
            };
            for deny_last in [false, true] {
                let sources = Cell::new(0);
                let mut admitted = Vec::new();
                let result = query.execute_governed_over_reading_with_source_and_field_admission(
                    None,
                    wide(),
                    |pattern, remaining| {
                        sources.set(sources.get() + 1);
                        pattern.plan().execute_governed_with_element_maps(
                            1,
                            [VId(0)],
                            [],
                            |_, _| Ok::<_, &'static str>(true),
                            |_, key| Ok((key == P).then_some(&property)),
                            |_, _| Ok(None),
                            |_| Ok(Some(labels.as_slice())),
                            |_| Ok(None),
                            |_| Ok(Some(&current)),
                            |_| Ok(None),
                            remaining,
                            || Ok::<_, ()>(()),
                        )
                    },
                    |_, _| Ok(None),
                    |key| {
                        assert_eq!(
                            sources.get(),
                            2,
                            "every source finishes before field admission"
                        );
                        admitted.push(key);
                        if deny_last && admitted.len() == expected_admission.len() {
                            Err(GqlQueryError::Source("actual computed key denied"))
                        } else {
                            Ok(())
                        }
                    },
                    || Ok::<_, ()>(()),
                );
                assert_eq!(sources.get(), 2);
                assert_eq!(admitted, expected_admission);
                if deny_last {
                    assert!(matches!(
                        result,
                        Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
                            GraphMutationError::Source("actual computed key denied")
                        )))
                    ));
                } else {
                    let batch = result.unwrap();
                    assert_eq!(batch.mutation().stats().selection.result_rows, 2);
                    assert_eq!(
                        batch.mutation().stats().effects,
                        if replacement { 2 } else { 1 }
                    );
                    assert_eq!(
                        batch
                            .returning()
                            .value
                            .iter()
                            .map(|row| row.values().to_vec())
                            .collect::<Vec<_>>(),
                        vec![vec![patch.clone()]; 2],
                    );
                }
            }
        }
    }
}
