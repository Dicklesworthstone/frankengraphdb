//! Exercise the public text/builder boundary and the incumbent GLA evaluator.
use super::*;
use crate::algebra::GraphValue;
use crate::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, EId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn edges() -> [(EId, VId, RelationId, VId); 6] {
    [
        (EId(11), VId(1), R, VId(2)),
        (EId(12), VId(1), R, VId(2)),
        (EId(13), VId(2), R, VId(3)),
        (EId(14), VId(3), R, VId(4)),
        (EId(u128::MAX), VId(4), R, VId(4)),
        (EId(21), VId(1), S, VId(4)),
    ]
}

fn properties() -> BTreeMap<EId, CanonicalScalar> {
    BTreeMap::from([
        (EId(11), CanonicalScalar::Int(0)),
        (EId(12), CanonicalScalar::Int(10)),
        (EId(13), CanonicalScalar::Null),
        (EId(u128::MAX), CanonicalScalar::Int(9)),
        (EId(21), CanonicalScalar::Int(20)),
    ])
}

fn run(text: &str) -> Vec<Vec<GraphValue>> {
    let pattern = prepare(text);
    let properties = properties();
    let threshold = CanonicalScalar::Int(5);
    pattern
        .plan()
        .execute_governed_with_element_properties(
            11,
            (1..=5).map(VId),
            edges(),
            |_, predicates| Ok::<_, &'static str>(predicates.iter().all(|p| p.matches(&[], &[]))),
            |_, key| Ok((key == P).then_some(&threshold)),
            |eid, key| Ok(properties.get(&eid).filter(|_| key == P)),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
        .into_iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn relationship_predicates_preserve_parallel_occurrences_and_unknown_truth() {
    for (prefix, ids) in [("", vec![1, 4]), ("NOT ", vec![2, 3, 5])] {
        for condition in ["edge.p > 5", "edge.p > a.p", "NOT (edge.p <= 5)"] {
            let text = format!(
                "MATCH (a) WHERE {prefix}EXISTS {{ MATCH (a)-[edge:R]->(b) WHERE {condition} }} RETURN a"
            );
            assert_eq!(
                run(&text),
                ids.iter()
                    .map(|id| vec![GraphValue::Vertex(VId(*id))])
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn reanchoring_a_later_reversed_edge_preserves_both_capture_identities() {
    let text = "MATCH (anchor) WHERE EXISTS { MATCH (x)-[left:R]->(y), (z)-[right:S]->(anchor) WHERE left.p = 10 AND right.p = 20 } RETURN anchor";
    assert_eq!(run(text), vec![vec![GraphValue::Vertex(VId(4))]]);
    let reversed = "MATCH (anchor) WHERE EXISTS { MATCH (x)-[left:R]->(y), (anchor)<-[right:S]-(z) WHERE left.p = 10 AND right.p = 20 } RETURN anchor";
    assert_eq!(run(reversed), run(text));
    assert!(run("MATCH (anchor) WHERE EXISTS { MATCH (x)-[left:R]->(y), (z)-[right:S]->(anchor) WHERE left.p = 20 AND right.p = 10 } RETURN anchor").is_empty());
}

#[test]
fn sibling_probe_captures_restore_outer_payloads_and_never_escape() {
    let text = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (a)-[s:R]->(x) WHERE s.p = 10 } AND NOT EXISTS { MATCH (b)-[s:R]->(y) WHERE s.p = 10 } RETURN r, a, r.p";
    assert_eq!(
        run(text),
        vec![
            vec![
                GraphValue::Edge(EId(11)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(0))
            ],
            vec![
                GraphValue::Edge(EId(12)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(10))
            ],
        ]
    );
    let prepared = prepare(text);
    let captures = prepared
        .plan()
        .operators()
        .iter()
        .filter_map(|op| match op {
            GlaOperator::CapturePath { capture, .. } => Some(*capture),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        captures,
        vec![0, 1, 1],
        "private slots may be reused, never outer slot zero"
    );
}

#[test]
fn existential_failures_are_not_absence_and_capture_limits_precede_execution() {
    for prefix in ["", "NOT "] {
        let pattern = prepare(&format!(
            "MATCH (a) WHERE {prefix}EXISTS {{ MATCH (a)-[r:R]->(b) WHERE r.p = 10 }} RETURN a"
        ));
        let result = pattern.plan().execute_governed_with_element_properties(
            11,
            (1..=5).map(VId),
            edges(),
            |_, _| Ok::<_, &'static str>(true),
            |_, _| Ok(None),
            |_, _| Err("edge payload unavailable"),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source("edge payload unavailable"))
        ));
    }
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap().vertex("b").unwrap();
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    for index in 0..MAX_PATTERN_IDENTITIES {
        outer.capture_edge(&format!("r{index}"), 0).unwrap();
    }
    let mut inner = GraphPatternBuilder::new();
    inner.vertex("a").unwrap().vertex("x").unwrap();
    inner.edge("a", R, GlaDirection::Forward, "x").unwrap();
    inner.capture_edge("local", 0).unwrap();
    assert!(matches!(
        outer.prepare_values_with_existence(
            &[GraphExistence::exists(&inner)],
            &[GraphColumn::vertex("a", "a")],
            0,
            Some(0)
        ),
        Err(PatternBuildError::LimitExceeded {
            dimension: PatternLimitDimension::PathCaptures,
            ..
        })
    ));
}

#[test]
fn local_edges_do_not_silently_rebind_outer_names_or_export_from_probes() {
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap().vertex("b").unwrap();
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    outer.capture_edge("root_edge", 0).unwrap();
    for name in ["a", "root_edge", "private_edge"] {
        let mut inner = GraphPatternBuilder::new();
        inner.vertex("x").unwrap().vertex("y").unwrap();
        inner.edge("x", R, GlaDirection::Forward, "y").unwrap();
        inner.capture_edge(name, 0).unwrap();
        let clauses = [GraphMatchClause::exists(&inner)];
        let result =
            outer.prepare_values_with_clauses(&clauses, &[GraphColumn::vertex("a", "a")], 0, None);
        if name == "private_edge" {
            assert!(result.is_ok());
            assert!(
                outer
                    .prepare_values_with_clauses(
                        &clauses,
                        &[GraphColumn::edge_property("p", name, P)],
                        0,
                        None
                    )
                    .is_err()
            );
        } else {
            assert!(matches!(result, Err(PatternBuildError::DuplicateVariable)));
        }
        // Required and OPTIONAL clauses export their relationships (fgdb-o4uen),
        // so a fresh name projects while a visible name still refuses.
        for clause in [
            GraphMatchClause::optional(&inner),
            GraphMatchClause::required(&inner),
        ] {
            let result = outer.prepare_values_with_clauses(
                &[clause],
                &[GraphColumn::vertex("a", "a")],
                0,
                Some(0),
            );
            if name == "private_edge" {
                assert!(result.is_ok());
                assert!(
                    outer
                        .prepare_values_with_clauses(
                            &[clause],
                            &[GraphColumn::edge_property("p", name, P)],
                            0,
                            None
                        )
                        .is_ok()
                );
            } else {
                assert!(matches!(result, Err(PatternBuildError::DuplicateVariable)));
            }
        }
    }
}

/// Every expected row below is enumerated by hand from `edges()`: parallel
/// 11/12 from 1 to 2, a self-loop at 4 with the largest identity, and vertex
/// 5 with no R edge. An OPTIONAL clause without a witness leaves its
/// relationship NULL, and two exported clauses never share a capture slot.
#[test]
fn optional_and_required_clauses_export_relationships_with_null_absence() {
    let edge = |id| GraphValue::Edge(EId(id));
    let vertex = |id| GraphValue::Vertex(VId(id));
    let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let sorted = |mut rows: Vec<Vec<GraphValue>>| {
        rows.sort();
        rows
    };
    assert_eq!(
        sorted(run(
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) RETURN a, r, s, r.p"
        )),
        sorted(vec![
            vec![vertex(1), edge(11), edge(13), int(0)],
            vec![vertex(1), edge(12), edge(13), int(10)],
            vec![vertex(2), edge(13), edge(14), null.clone()],
            vec![vertex(3), edge(14), edge(u128::MAX), null.clone()],
            vec![vertex(4), edge(u128::MAX), edge(u128::MAX), int(9)],
            vec![vertex(5), null.clone(), null.clone(), null.clone()],
        ])
    );
    // The clause's own predicate selects its witness; absence is per row.
    assert_eq!(
        sorted(run(
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b) WHERE r.p > 5 RETURN a, r"
        )),
        sorted(vec![
            vec![vertex(1), edge(12)],
            vec![vertex(2), null.clone()],
            vec![vertex(3), null.clone()],
            vec![vertex(4), edge(u128::MAX)],
            vec![vertex(5), null.clone()],
        ])
    );
    // A required clause exports its relationship and never null-extends.
    assert_eq!(
        sorted(run(
            "MATCH (a) MATCH (a)-[r:R]->(b) WHERE a <> b RETURN a, r"
        )),
        sorted(vec![
            vec![vertex(1), edge(11)],
            vec![vertex(1), edge(12)],
            vec![vertex(2), edge(13)],
            vec![vertex(3), edge(14)],
        ])
    );
    // Exported captures take distinct slots after the root's.
    let prepared = prepare(
        "MATCH (a)-[q:S]->(d) OPTIONAL MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) RETURN q, r, s",
    );
    let captures = prepared
        .plan()
        .operators()
        .iter()
        .filter_map(|op| match op {
            GlaOperator::CapturePath { capture, .. } => Some(*capture),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(captures, vec![0, 1, 2]);
}
