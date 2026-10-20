//! Mutations between two WITH boundaries retain final-state field ownership.
//! Native GLA supplies the frozen source once; expected rows are enumerated
//! independently of the mutation and continuation operators.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationPolicy,
    GraphMutationQueryBatch, GraphMutationQueryError, GraphSymbol, GraphSymbolKind,
    PreparedGraphMutationQuery, PreparedGraphMutationQueryText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const BASE: u128 = 1_u128 << 100;
type Execution =
    Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<()>, usize>>;

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
    let vertices = [VId(BASE + 1), VId(BASE + 2), VId(BASE + 3)];
    let properties: BTreeMap<_, CanonicalPropertyValue> = vertices
        .iter()
        .enumerate()
        .map(|(at, &vertex)| ((vertex, P), CanonicalScalar::Int(at as i64 + 1).into()))
        .collect();
    let calls = Cell::new(0);
    let result = query.execute_governed(
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
        assert_eq!(calls.get(), 1, "the positive control must execute native GLA");
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
    let query = prepare(
        "MATCH (n) WITH n SET n.p=n.p+10 RETURN n.p AS p,n AS node ORDER BY node",
    );
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
    let query = prepare(
        "MATCH (n) WITH n SET n.p=n.p+10 WITH n AS kept RETURN kept.p AS p LIMIT 0",
    );
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
