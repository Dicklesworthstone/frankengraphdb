//! Mutations between two WITH boundaries retain final-state field ownership.
//! Native GLA supplies the frozen source once; expected rows are enumerated
//! independently of the mutation and continuation operators.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError, GraphMutationIntent,
    GraphMutationPolicy, GraphMutationQueryBatch, GraphMutationQueryError, GraphSymbol,
    GraphSymbolKind, GraphWriteChainStep, PreparedGraphMutationQuery,
    PreparedGraphMutationQueryText, PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const BASE: u128 = 1_u128 << 100;
type Execution = Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<()>, usize>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

fn policy(rows: u64) -> GraphMutationPolicy {
    GraphMutationPolicy::new(GqlQueryPolicy::new(1_000, rows, 2_000_000, 1_000_000), 100)
}

fn prepare(text: &str) -> PreparedGraphMutationQuery {
    PreparedGraphMutationQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn execute(
    query: &PreparedGraphMutationQuery,
    allowance: GraphMutationPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> Execution {
    execute_owned(query, allowance, None, checkpoint)
}

fn execute_owned(
    query: &PreparedGraphMutationQuery,
    allowance: GraphMutationPolicy,
    owned: Option<Vec<GraphValueRow>>,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> Execution {
    let vertices = [VId(BASE + 1), VId(BASE + 2), VId(BASE + 3)];
    let properties: BTreeMap<_, CanonicalPropertyValue> = vertices
        .iter()
        .enumerate()
        .map(|(at, &vertex)| ((vertex, P), CanonicalScalar::Int(at as i64 + 1).into()))
        .collect();
    let calls = Cell::new(0);
    let result = query.execute_governed_over(
        owned,
        allowance,
        |selection, budget| {
            calls.set(calls.get() + 1);
            assert_eq!(calls.get(), 1, "a continuation must not reselect the graph");
            selection.plan().execute_governed_with_properties(
                vertices.len() as u64,
                vertices.iter().copied(),
                std::iter::empty::<(VId, RelationId, VId)>(),
                |vertex, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            [],
                            properties.iter().filter_map(|(&(owner, key), value)| {
                                (owner == vertex).then_some((key, value))
                            }),
                        )
                    }))
                },
                |vertex, key| Ok(properties.get(&(vertex, key))),
                budget,
                || Ok::<_, usize>(()),
            )
        },
        checkpoint,
    );
    if result.is_ok() {
        assert_eq!(
            calls.get(),
            1,
            "the positive control must execute native GLA"
        );
    }
    result
}

/// Every leaf sees the same immutable graph. Native GLA evaluates each leaf;
/// the mutation kernel alone owns correlation, row occurrences and proposals.
fn execute_multipart(
    query: &PreparedGraphMutationQuery,
    allowance: GraphMutationPolicy,
    owned: Option<Vec<GraphValueRow>>,
    edges: &[(VId, RelationId, VId)],
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> Execution {
    let vertices = [VId(BASE + 1), VId(BASE + 2), VId(BASE + 3)];
    let properties: BTreeMap<_, CanonicalPropertyValue> = vertices
        .iter()
        .enumerate()
        .map(|(at, &vertex)| ((vertex, P), CanonicalScalar::Int(at as i64 + 1).into()))
        .collect();
    let calls = Cell::new(0);
    let checkpoint = RefCell::new(checkpoint);
    let result = query.execute_governed_over_reading_with_source(
        owned,
        allowance,
        |selection, budget| {
            calls.set(calls.get() + 1);
            selection
                .plan()
                .execute_governed_with_identified_properties(
                    (vertices.len() + edges.len()) as u64,
                    vertices.iter().copied(),
                    edges.iter().enumerate().map(|(at, &(from, relation, to))| {
                        (EId(BASE + at as u128 + 1), from, relation, to)
                    }),
                    |vertex, predicates| {
                        Ok::<_, ()>(predicates.iter().all(|predicate| {
                            predicate.matches_borrowed(
                                [],
                                properties.iter().filter_map(|(&(owner, key), value)| {
                                    (owner == vertex).then_some((key, value))
                                }),
                            )
                        }))
                    },
                    |vertex, key| Ok(properties.get(&(vertex, key))),
                    budget,
                    || (checkpoint.borrow_mut())(),
                )
        },
        |element, key| {
            Ok(match element {
                ElementId::Vertex(vertex) => properties.get(&(vertex, key)).cloned(),
                _ => None,
            })
        },
        || (checkpoint.borrow_mut())(),
    );
    if result.is_ok() {
        assert!(
            calls.get() >= 2,
            "the query must execute all native read parts"
        );
    }
    result
}

fn rows(batch: &GraphMutationQueryBatch) -> Vec<Vec<GraphValue>> {
    batch
        .returning()
        .value
        .iter()
        .map(|row: &GraphValueRow| row.values().to_vec())
        .collect()
}

#[test]
fn multipart_set_rebinds_parameters_and_returns_final_fields_per_occurrence() {
    let catalogs = Cell::new(0);
    let template = PreparedGraphMutationQueryText::prepare(
        "MATCH (a) WITH a AS n,a.p AS frozen UNWIND [0,0] AS occurrence \
         MATCH (n) WHERE n.p >= $minimum WITH n,frozen,occurrence \
         SET n.p=frozen+$delta,n.q=frozen*10 \
         RETURN frozen,n.p AS p,n.q AS q,occurrence ORDER BY frozen,occurrence",
        R,
        |kind, name: &str| {
            catalogs.set(catalogs.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let prepared_catalogs = catalogs.get();
    for (minimum, delta, selected) in [(2, 10, vec![2, 3]), (3, 20, vec![3]), (4, 30, vec![])] {
        let arguments = GqlParameters::new()
            .with_int64("minimum", minimum)
            .unwrap()
            .with_int64("delta", delta)
            .unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        assert_eq!(query, template.bind_parameters(&arguments).unwrap());
        assert_eq!(catalogs.get(), prepared_catalogs);
        let batch = execute_multipart(
            &query,
            policy((selected.len() * 2) as u64),
            None,
            &[],
            || Ok(()),
        )
        .unwrap();
        let expected: Vec<_> = selected
            .iter()
            .flat_map(|&value| {
                [0, 0].into_iter().map(move |occurrence| {
                    vec![
                        int(value),
                        int(value + delta),
                        int(value * 10),
                        int(occurrence),
                    ]
                })
            })
            .collect();
        assert_eq!(rows(&batch), expected);
        assert_eq!(
            batch.mutation().stats().effects,
            (selected.len() * 2) as u64
        );
        assert_eq!(
            batch.mutation().stats().selection.result_rows,
            (selected.len() * 2) as u64
        );
    }
}

#[test]
fn multipart_optional_null_set_target_skips_a_failing_rhs() {
    let query = prepare(
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(missing) \
         SET missing.p=1/0 RETURN a,missing,missing.p AS value ORDER BY a",
    );
    let batch = execute_multipart(&query, policy(3), None, &[], || Ok(())).unwrap();
    assert_eq!(
        rows(&batch),
        (1..=3)
            .map(|id| vec![GraphValue::Vertex(VId(BASE + id)), null(), null()])
            .collect::<Vec<_>>()
    );
    assert!(batch.mutation().intents().is_empty());
    assert!(matches!(
        execute_multipart(
            &query,
            policy(3),
            None,
            &[(VId(BASE + 1), R, VId(BASE + 2))],
            || Ok(()),
        ),
        Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(_)))
    ));
}

#[test]
fn multipart_remove_and_delete_keep_null_and_duplicate_rows() {
    let edges = [
        (VId(BASE + 1), R, VId(BASE + 2)),
        (VId(BASE + 1), R, VId(BASE + 2)),
    ];
    let query = prepare(
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) REMOVE b.p \
         RETURN a,b,b.p AS value ORDER BY a",
    );
    let batch = execute_multipart(&query, policy(4), None, &edges, || Ok(())).unwrap();
    assert_eq!(
        rows(&batch),
        vec![
            vec![
                GraphValue::Vertex(VId(BASE + 1)),
                GraphValue::Vertex(VId(BASE + 2)),
                null()
            ],
            vec![
                GraphValue::Vertex(VId(BASE + 1)),
                GraphValue::Vertex(VId(BASE + 2)),
                null()
            ],
            vec![GraphValue::Vertex(VId(BASE + 2)), null(), null()],
            vec![GraphValue::Vertex(VId(BASE + 3)), null(), null()],
        ],
    );
    assert_eq!(
        batch.mutation().intents(),
        &[GraphMutationIntent::Property {
            vertex: VId(BASE + 2),
            key: P,
            value: None,
        }],
    );
    for action in ["DELETE", "DETACH DELETE"] {
        let query = prepare(&format!(
            "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) \
             UNWIND [b,b,null] AS target {action} target RETURN target ORDER BY target",
        ));
        let batch = execute_multipart(&query, policy(12), None, &edges, || Ok(())).unwrap();
        assert_eq!(batch.mutation().stats().effects, 1);
        assert_eq!(batch.returning().value.len(), 12);
        let expected = if action == "DELETE" {
            GraphMutationIntent::Delete {
                vertex: VId(BASE + 2),
            }
        } else {
            GraphMutationIntent::DetachDelete {
                vertex: VId(BASE + 2),
            }
        };
        assert_eq!(batch.mutation().intents(), &[expected]);
        let empty = execute_multipart(&query, policy(9), None, &[], || Ok(())).unwrap();
        assert!(empty.mutation().intents().is_empty());
        assert_eq!(rows(&empty), vec![vec![null()]; 9]);
    }
}

#[test]
fn inline_read_call_and_leading_optional_read_parts_feed_set() {
    let edges = [
        (VId(BASE + 1), R, VId(BASE + 2)),
        (VId(BASE + 1), R, VId(BASE + 2)),
    ];
    for call in [
        "CALL (n) { MATCH (n)-[:R]->(m) RETURN count(*) AS degree }",
        "CALL { WITH n MATCH (n)-[:R]->(m) RETURN count(*) AS degree }",
    ] {
        let query = prepare(&format!(
            "MATCH (n) WITH n {call} SET n.q=degree RETURN n,n.q AS degree ORDER BY n",
        ));
        let batch = execute_multipart(&query, policy(3), None, &edges, || Ok(())).unwrap();
        assert_eq!(
            rows(&batch),
            vec![
                vec![GraphValue::Vertex(VId(BASE + 1)), int(2)],
                vec![GraphValue::Vertex(VId(BASE + 2)), int(0)],
                vec![GraphValue::Vertex(VId(BASE + 3)), int(0)],
            ],
        );
        assert_eq!(batch.mutation().stats().effects, 3);
    }
    let query = prepare(
        "OPTIONAL MATCH (absent {p:99}) WITH absent MATCH (n) \
         SET n.q=4 RETURN absent,n.q AS value",
    );
    let batch = execute_multipart(&query, policy(3), None, &[], || Ok(())).unwrap();
    assert_eq!(rows(&batch), vec![vec![null(), int(4)]; 3]);
    let query = prepare(
        "CALL { RETURN 8 AS value } MATCH (n) WITH n,value MATCH (n) \
         SET n.q=value RETURN n.q AS value",
    );
    let batch = execute_multipart(&query, policy(3), None, &[], || Ok(())).unwrap();
    assert_eq!(rows(&batch), vec![vec![int(8)]; 3]);
}

#[test]
fn a_carried_binding_named_set_does_not_invent_an_earlier_write() {
    let text = "CALL { MATCH (n) RETURN n AS set } WITH set MATCH (set) \
                SET set.p=1 RETURN set ORDER BY set";
    assert!(!PreparedGraphWriteChainText::is_chain(text).unwrap());
    let query = prepare(text);
    let batch = execute_multipart(&query, policy(3), None, &[], || Ok(())).unwrap();
    assert_eq!(batch.mutation().stats().effects, 3);
    assert_eq!(
        rows(&batch),
        (1..=3)
            .map(|id| vec![GraphValue::Vertex(VId(BASE + id))])
            .collect::<Vec<_>>(),
    );
}

#[test]
fn multipart_conflicts_and_late_return_errors_expose_no_proposal() {
    let query = prepare(
        "MATCH (a) WITH a MATCH (a) UNWIND [0,1] AS change \
         SET a.p=a.p+change RETURN a.p AS value LIMIT 0",
    );
    assert!(matches!(
        execute_multipart(&query, policy(0), None, &[], || Ok(())),
        Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
            GraphMutationError::ConflictingAssignment { .. }
        )))
    ));
    let query = prepare(
        "MATCH (a) WITH a MATCH (a) SET a.p=a.p+10 \
         RETURN 1/(a.p-12) AS bad LIMIT 0",
    );
    assert!(matches!(
        execute_multipart(&query, policy(0), None, &[], || Ok(())),
        Err(GqlQueryError::Source(GraphMutationQueryError::Returning(_)))
    ));
}

#[test]
fn complete_read_parts_between_writes_preserve_owned_and_optional_occurrences() {
    let edges = [
        (VId(BASE + 1), R, VId(BASE + 2)),
        (VId(BASE + 1), R, VId(BASE + 2)),
    ];
    for head in [
        "CREATE (audit) WITH audit",
        "MATCH (audit) SET audit.q=4 WITH audit",
    ] {
        for (action, effects) in [("SET n.p=frozen+5", 3), ("REMOVE n.p", 3), ("DELETE m", 1)] {
            let text = format!(
                "{head} MATCH (n) WITH audit,n,n.p AS frozen UNWIND [0,0] AS occurrence \
                 OPTIONAL MATCH (n)-[:R]->(m) {action} \
                 RETURN audit,n,m,frozen ORDER BY n,m",
            );
            assert!(PreparedGraphWriteChainText::is_chain(&text).unwrap());
            let chain = PreparedGraphWriteChainText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let [_, GraphWriteChainStep::MutationQuery(query)] = chain.steps() else {
                panic!("the complete read pipeline must belong to the next mutation: {text}");
            };
            let audit = GraphValue::Vertex(VId(BASE + 99));
            let batch = execute_multipart(
                query,
                policy(8),
                Some(vec![GraphValueRow::from_owned_values(vec![audit.clone()])]),
                &edges,
                || Ok(()),
            )
            .unwrap();
            assert_eq!(batch.mutation().stats().effects, effects, "{text}");
            assert_eq!(
                rows(&batch),
                (1..=3)
                    .flat_map(|id| {
                        let repeats = if id == 1 { 4 } else { 2 };
                        let audit = audit.clone();
                        (0..repeats).map(move |_| {
                            vec![
                                audit.clone(),
                                GraphValue::Vertex(VId(BASE + id)),
                                if id == 1 {
                                    GraphValue::Vertex(VId(BASE + 2))
                                } else {
                                    null()
                                },
                                int(id as i64),
                            ]
                        })
                    })
                    .collect::<Vec<_>>(),
                "{text}",
            );
        }
    }
}

#[test]
fn multipart_mutation_scope_and_effectful_calls_refuse_before_invalid_symbols_resolve() {
    for text in [
        "MATCH (a) WITH a AS kept MATCH (b) SET a.unavailable=1 RETURN b",
        "MATCH (a) WITH a CALL (a) { CREATE (b) RETURN b } SET a.p=1 RETURN a",
        "CREATE (audit) WITH audit MATCH (n) WITH n MATCH (m) SET audit.unavailable=1 RETURN m",
        "CREATE (audit) WITH audit MATCH (n) CALL (n) { SET n.p=2 RETURN n } SET n.q=3 RETURN n",
    ] {
        let invalid_symbol = Cell::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let resolve = |kind, name: &str| {
                if name == "unavailable" {
                    invalid_symbol.set(true);
                }
                symbols(kind, name)
            };
            if text.starts_with("CREATE") {
                PreparedGraphWriteChainText::prepare(text, R, resolve).is_err()
            } else {
                PreparedGraphMutationQueryText::prepare(text, R, resolve).is_err()
            }
        }));
        assert!(result.is_ok(), "preparation panicked: {text}");
        assert!(result.unwrap(), "unexpectedly admitted: {text}");
        assert!(
            !invalid_symbol.get(),
            "an out-of-scope field reached the catalog: {text}"
        );
    }
}

#[test]
fn post_write_match_deletion_uses_native_relations_and_keeps_occurrences() {
    use fgdb_gql::{GraphMutationIntent, GraphWriteChainStep, PreparedGraphWriteChainText};

    for action in ["DELETE", "DETACH DELETE"] {
        for hidden_rows in [false, true] {
            let text = format!(
                "CREATE (audit) WITH audit MATCH (n) WHERE n.p>=2 \
                 UNWIND [0,0] AS occurrence {action} n RETURN n,occurrence ORDER BY n{}",
                if hidden_rows { " LIMIT 0" } else { "" },
            );
            assert!(PreparedGraphWriteChainText::is_chain(&text).unwrap());
            let chain = PreparedGraphWriteChainText::prepare(&text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let [_, GraphWriteChainStep::MutationQuery(query)] = chain.steps() else {
                panic!("the mandatory MATCH must feed the native mutation step");
            };
            let batch = execute_owned(
                query,
                policy(if hidden_rows { 0 } else { 4 }),
                Some(vec![GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(BASE + 99)),
                ])]),
                || Ok(()),
            )
            .unwrap();
            let expected_intents: Vec<_> = [BASE + 2, BASE + 3]
                .into_iter()
                .map(|id| {
                    if action == "DELETE" {
                        GraphMutationIntent::Delete { vertex: VId(id) }
                    } else {
                        GraphMutationIntent::DetachDelete { vertex: VId(id) }
                    }
                })
                .collect();
            assert_eq!(batch.mutation().intents(), expected_intents);
            assert_eq!(batch.mutation().stats().effects, 2);
            assert_eq!(
                rows(&batch),
                if hidden_rows {
                    vec![]
                } else {
                    [BASE + 2, BASE + 2, BASE + 3, BASE + 3]
                        .into_iter()
                        .map(|id| vec![GraphValue::Vertex(VId(id)), int(0)])
                        .collect()
                },
            );
        }
    }
}

#[test]
fn reordered_row_bindings_carry_simultaneous_final_fields_through_an_alias() {
    let query = prepare(
        "MATCH (n) WITH 7 AS pad,n AS source \
         SET source.p=source.p+10,source.q=source.p*10 \
         WITH source AS kept RETURN kept.p AS p,kept.q AS q ORDER BY p",
    );
    let batch = execute(&query, policy(3), || Ok(())).unwrap();
    assert_eq!(
        rows(&batch),
        vec![
            vec![int(11), int(10)],
            vec![int(12), int(20)],
            vec![int(13), int(30)],
        ]
    );
    assert_eq!(batch.mutation().stats().effects, 6);
    assert_eq!(batch.mutation().stats().selection.result_rows, 3);
}

#[test]
fn grouped_row_mutations_preserve_occurrences_and_compound_summary_slots() {
    let query = prepare(
        "MATCH (a),(b) WITH 7 AS pad,a AS source SET source.p=source.p+10 \
         WITH source AS kept,count(*) AS n,sum(source.p)+count(*) AS score \
         RETURN kept.p AS p,n,score ORDER BY p",
    );
    let batch = execute(&query, policy(3), || Ok(())).unwrap();
    assert_eq!(
        rows(&batch),
        vec![
            vec![int(11), int(3), int(36)],
            vec![int(12), int(3), int(39)],
            vec![int(13), int(3), int(42)],
        ]
    );
    assert_eq!(batch.mutation().stats().effects, 3);
    assert_eq!(batch.mutation().stats().selection.result_rows, 9);
}

#[test]
fn completed_projection_drops_input_scope_without_hiding_real_output_aliases() {
    // The incoming row has one visible item plus a hidden field. The RETURN
    // also has width two, but neither of its outputs is hidden.
    let query = prepare("MATCH (n) WITH n SET n.p=n.p+10 RETURN n.p AS p,n AS node ORDER BY node");
    assert_eq!(
        rows(&execute(&query, policy(3), || Ok(())).unwrap()),
        vec![
            vec![int(11), GraphValue::Vertex(VId(BASE + 1))],
            vec![int(12), GraphValue::Vertex(VId(BASE + 2))],
            vec![int(13), GraphValue::Vertex(VId(BASE + 3))],
        ]
    );
    // A user-chosen reserved-looking alias stays public; generated reads do not.
    let query = prepare(
        "MATCH (n) WITH n AS __fg_boundary_0 SET __fg_boundary_0.p=__fg_boundary_0.p+10 \
         WITH __fg_boundary_0 AS kept UNWIND [kept.p] AS x RETURN *",
    );
    assert_eq!(query.columns(), &["kept", "x"]);
    assert_eq!(
        rows(&execute(&query, policy(3), || Ok(())).unwrap()),
        vec![
            vec![GraphValue::Vertex(VId(BASE + 1)), int(11)],
            vec![GraphValue::Vertex(VId(BASE + 2)), int(12)],
            vec![GraphValue::Vertex(VId(BASE + 3)), int(13)],
        ]
    );
}

#[test]
fn unavailable_or_private_row_fields_refuse_without_panics_or_catalog_access() {
    for text in [
        "MATCH (n) WITH n SET n.p=1 WITH n AS kept RETURN kept.q",
        "MATCH (n) WITH n SET n.p=1 RETURN __fg_boundary_0",
        "MATCH (n) WITH n SET n.p=1 WITH __fg_boundary_0 AS leaked RETURN leaked",
        "MATCH (n) WITH n SET n.p=1 WITH 0 AS kept RETURN kept.p",
    ] {
        let calls = Cell::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            PreparedGraphMutationQueryText::prepare(text, R, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
        }));
        assert!(result.is_ok(), "preparation panicked: {text}");
        assert!(result.unwrap().is_err(), "unexpectedly admitted: {text}");
        assert_eq!(
            calls.get(),
            0,
            "refusal must precede catalog access: {text}"
        );
    }
}

#[test]
fn removed_properties_and_empty_sources_keep_their_distinct_native_meanings() {
    let query = prepare("MATCH (n) WITH n REMOVE n.p WITH n AS kept RETURN kept.p AS p");
    let batch = execute(&query, policy(3), || Ok(())).unwrap();
    assert_eq!(rows(&batch), vec![vec![null()], vec![null()], vec![null()]]);
    assert_eq!(batch.mutation().stats().effects, 3);
    let query = prepare(
        "MATCH (n {p:99}) WITH n SET n.p=n.p+1 WITH n AS kept \
         RETURN count(kept.p) AS n,sum(kept.p) AS total",
    );
    let batch = execute(&query, policy(1), || Ok(())).unwrap();
    assert_eq!(rows(&batch), vec![vec![int(0), null()]]);
    assert_eq!(batch.mutation().stats().effects, 0);
}

#[test]
fn row_mutation_continuations_keep_final_row_limits_late_errors_and_cancellation() {
    let query = prepare(
        "MATCH (n) WITH n SET n.p=n.p+10 WITH n AS kept \
         UNWIND [kept.p,kept.p+1] AS x RETURN sum(x) AS total",
    );
    let mut checkpoints = 0;
    let batch = execute(&query, policy(1), || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(rows(&batch), vec![vec![int(75)]]);
    assert_eq!(batch.mutation().stats().effects, 3);
    assert!(execute(&query, policy(0), || Ok(())).is_err());
    assert!(checkpoints > 1);
    for fail_at in 0..checkpoints {
        let mut at = 0;
        let result = execute(&query, policy(1), || {
            let current = at;
            at += 1;
            if current == fail_at {
                Err(fail_at)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(index)) if index == fail_at));
    }
    let query = prepare(
        "MATCH (n) WITH n SET n.p=n.p+10 WITH n AS kept \
         RETURN 12/(kept.p-12) AS bad LIMIT 0",
    );
    assert!(matches!(
        execute(&query, policy(0), || Ok(())),
        Err(GqlQueryError::Source(_))
    ));
    let query =
        prepare("MATCH (n) WITH n SET n.p=n.p+10 WITH n AS kept RETURN kept.p AS p LIMIT 0");
    let batch = execute(&query, policy(0), || Ok(())).unwrap();
    assert!(batch.returning().value.is_empty());
    assert_eq!(batch.mutation().stats().effects, 3);
}

#[test]
fn rebinding_and_filtering_use_final_fields_without_limiting_mutation_effects() {
    let template = PreparedGraphMutationQueryText::prepare(
        "MATCH (n) WITH n AS source SET source.p=source.p+$delta \
         WITH source AS kept WHERE kept.p>$minimum RETURN kept.p AS p ORDER BY p",
        R,
        symbols,
    )
    .unwrap();
    for (delta, minimum, expected) in [
        (10, 11, vec![vec![int(12)], vec![int(13)]]),
        (20, 30, vec![]),
    ] {
        let arguments = GqlParameters::new()
            .with_int64("delta", delta)
            .unwrap()
            .with_int64("minimum", minimum)
            .unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        let batch = execute(&query, policy(expected.len() as u64), || Ok(())).unwrap();
        assert_eq!(rows(&batch), expected);
        assert_eq!(batch.mutation().stats().effects, 3);
    }
}

/// A SET value over a WITH row reads composite operands through the row, as
/// MATCH ... SET already does (fgdb-c4i0d): a list literal on either side of
/// a `+` chain, and a list function inside arithmetic. SET is simultaneous,
/// so every value reads p before the clause. Over p = 1, 2, 3 the written
/// values are enumerated by hand.
#[test]
fn row_set_values_read_composite_operands_through_the_row() {
    let list = |values: &[i64]| GraphValue::List(values.iter().map(|&v| int(v)).collect());
    let query = prepare(
        "MATCH (n) WITH n AS row \
         SET row.q = [row.p] + [0], row.p = row.p + size([1, 2]) \
         WITH row RETURN row.p AS p, row.q AS q ORDER BY p",
    );
    let batch = execute(&query, policy(3), || Ok(())).unwrap();
    assert_eq!(
        rows(&batch),
        vec![
            vec![int(3), list(&[1, 0])],
            vec![int(4), list(&[2, 0])],
            vec![int(5), list(&[3, 0])],
        ]
    );
    let query = prepare(
        "MATCH (n) WITH n AS row SET row.q = [0] + [row.p] + [row.p * 10] \
         WITH row RETURN row.p AS p, row.q AS q ORDER BY p",
    );
    let batch = execute(&query, policy(3), || Ok(())).unwrap();
    assert_eq!(
        rows(&batch),
        vec![
            vec![int(1), list(&[0, 1, 10])],
            vec![int(2), list(&[0, 2, 20])],
            vec![int(3), list(&[0, 3, 30])],
        ]
    );
}
