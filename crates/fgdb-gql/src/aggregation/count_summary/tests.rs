use super::*;
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};
use fgdb_delta_types::LabelId;

fn query(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name| match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(match name {
            "R" => 1,
            "S" => 2,
            _ => 3,
        }))),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
        GraphSymbolKind::Label => Some(GraphSymbol::Label(LabelId(match name {
            "L" => 1,
            "M" => 2,
            _ => 3,
        }))),
    })
    .unwrap_or_else(|error| panic!("{text}: {error:?}"))
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

#[test]
fn count_summary_keeps_external_element_reads_and_the_complete_control_transcript() {
    use crate::{GraphElementKey, GraphSetProjection, GraphSetValue};

    fn compare<const RUNS: bool>(size: usize, hidden: bool) {
        let query = query(if hidden {
            "MATCH (n) RETURN COUNT(*) AS n LIMIT 0"
        } else {
            "MATCH (n) RETURN COUNT(*) AS n"
        })
        .with_output_projection(vec![
            GraphSetProjection::new("n", GraphSetValue::Column(0)),
            GraphSetProjection::new(
                "outside",
                GraphSetValue::MapGet {
                    map: Box::new(GraphSetValue::Value(GraphValue::Vertex(VId(u128::MAX)))),
                    key: "p".into(),
                    property: GraphElementKey::Property(PropertyKeyId(1)),
                },
            ),
            GraphSetProjection::new(
                "missing",
                GraphSetValue::MapGet {
                    map: Box::new(GraphSetValue::Value(GraphValue::Edge(EId(u128::MAX)))),
                    key: "p".into(),
                    property: GraphElementKey::Property(PropertyKeyId(1)),
                },
            ),
        ])
        .unwrap();
        let policy = GqlQueryPolicy::new(100, 1, 100_000, 100_000);
        let run = |summary: bool, stop: Option<usize>, fail: bool| {
            let mut calls = Vec::new();
            let mut reads = Vec::new();
            let host = Host::<RUNS> {
                calls: &mut calls,
                stop,
                refused: false,
            };
            let elements = |element, key| {
                assert_eq!(key, PropertyKeyId(1));
                reads.push(element);
                match element {
                    ElementId::Vertex(VId(u128::MAX)) => Ok(Some(CanonicalScalar::Int(33).into())),
                    ElementId::Edge(EId(u128::MAX)) if fail => Err("late element read"),
                    ElementId::Edge(EId(u128::MAX)) => Ok(None),
                    _ => panic!("only supplied output identities are read"),
                }
            };
            let result = if summary {
                query
                    .prepare_count_summary(GraphCountSource::Vertices, size as u64, size as u64)
                    .unwrap()
                    .execute_governed_with_element_reads(size as u64, elements, policy, host)
            } else {
                query.execute_governed_with_element_reads(
                    size as u64,
                    (0..size).map(|at| VId(at as u128)),
                    [],
                    |_, _| -> Result<bool, &'static str> { panic!("filterless input") },
                    |_, _| -> Result<Option<&fgdb_types::CanonicalPropertyValue>, &'static str> {
                        panic!("output properties must not use the input column reader")
                    },
                    elements,
                    policy,
                    host,
                )
            };
            (result, calls, reads)
        };
        let reference = run(false, None, false);
        assert_eq!(run(true, None, false), reference);
        assert_eq!(
            reference.2,
            [
                ElementId::Vertex(VId(u128::MAX)),
                ElementId::Edge(EId(u128::MAX))
            ]
        );
        let result = reference.0.as_ref().unwrap();
        assert_eq!(result.value.len(), usize::from(!hidden));
        if !hidden {
            assert_eq!(result.value[0].values()[0].as_count(), Some(size as u64));
            assert_eq!(
                result.value[0].values()[1]
                    .as_value()
                    .and_then(GraphValue::as_scalar),
                Some(&CanonicalScalar::Int(33))
            );
            assert!(result.value[0].values()[2].is_null());
        }
        let failure = run(false, None, true);
        assert_eq!(run(true, None, true), failure);
        assert!(matches!(
            failure.0,
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                "late element read"
            )))
        ));
        for stop in 1..=reference.1.len() {
            let ordinary = run(false, Some(stop), false);
            assert_eq!(run(true, Some(stop), false), ordinary);
            assert!(matches!(ordinary.0, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(ordinary.1.len(), stop);
        }
    }

    for size in [0, 3] {
        for hidden in [false, true] {
            compare::<false>(size, hidden);
            compare::<true>(size, hidden);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Unit,
    Units(u64),
}

struct Host<'calls, const RUNS: bool> {
    calls: &'calls mut Vec<Call>,
    stop: Option<usize>,
    refused: bool,
}

impl<const RUNS: bool> Host<'_, RUNS> {
    fn observe(&mut self, call: Call) -> Result<(), usize> {
        assert!(!self.refused, "host called again after refusing");
        self.calls.push(call);
        if self.stop == Some(self.calls.len()) {
            self.refused = true;
            Err(self.calls.len())
        } else {
            Ok(())
        }
    }
}

impl<const RUNS: bool> crate::GovernedCheckpoint<usize> for Host<'_, RUNS> {
    const RUNS: bool = RUNS;

    fn unit(&mut self) -> Result<(), usize> {
        self.observe(Call::Unit)
    }

    fn units(&mut self, n: u64) -> Result<(), usize> {
        self.observe(Call::Units(n))
    }
}

type Outcome = (
    Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<()>, usize>>,
    Vec<Call>,
);

fn run<const RUNS: bool>(
    query: &PreparedGraphAggregate,
    vertices: &[VId],
    edges: &[(VId, RelationId, VId)],
    summary: bool,
    policy: GqlQueryPolicy,
    stop: Option<usize>,
) -> Outcome {
    let source = query.count_metadata_source().expect("checked count shape");
    let examined = match source {
        GraphCountSource::Vertices | GraphCountSource::Label(_) => vertices.len() as u64,
        GraphCountSource::Edges | GraphCountSource::Relation(_) => edges.len() as u64,
    };
    let matching = match source {
        GraphCountSource::Vertices | GraphCountSource::Edges => examined,
        GraphCountSource::Label(_) => vertices.iter().filter(|vid| vid.0 % 3 == 0).count() as u64,
        GraphCountSource::Relation(relation) => edges
            .iter()
            .filter(|&&(_, actual, _)| actual == relation)
            .count() as u64,
    };
    let records = vertices.len() as u64
        + if matches!(
            source,
            GraphCountSource::Vertices | GraphCountSource::Label(_)
        ) {
            0
        } else {
            examined
        };
    let mut calls = Vec::new();
    let host = Host::<RUNS> {
        calls: &mut calls,
        stop,
        refused: false,
    };
    let result = if summary {
        query
            .prepare_count_summary(source, examined, matching)
            .unwrap()
            .execute_governed(records, policy, host)
    } else {
        query.execute_governed(
            records,
            vertices.iter().copied(),
            edges.iter().copied(),
            |vid, predicates| -> Result<bool, ()> {
                assert_eq!(
                    predicates,
                    [crate::algebra::VertexPredicate::HasLabel(LabelId(1))]
                );
                Ok(vid.0 % 3 == 0)
            },
            |_, _| -> Result<Option<&fgdb_types::CanonicalPropertyValue>, ()> {
                panic!("COUNT(*) read an input property")
            },
            policy,
            host,
        )
    };
    (result, calls)
}

fn compare<const RUNS: bool>(text: &str, size: usize) {
    let query = query(text);
    // Full-width, noncontiguous identities; parallel edges, self-loops and a
    // second relation. Cardinality cannot use distinct endpoints or triples.
    let vertices: Vec<_> = (0..size)
        .map(|at| VId(u128::MAX - at as u128 * 1_000_003))
        .collect();
    let edges: Vec<_> = (0..size.saturating_mul(2))
        .map(|at| {
            (
                vertices[at % size],
                RelationId(if at % 3 == 0 { 2 } else { 1 }),
                vertices[(at * 7 + 1) % size],
            )
        })
        .collect();
    let wide = GqlQueryPolicy::new(1_000_000, 100, 100_000_000, 1_000_000);
    let reference = run::<RUNS>(&query, &vertices, &edges, false, wide, None);
    assert_eq!(
        run::<RUNS>(&query, &vertices, &edges, true, wide, None),
        reference,
        "{text}, size {size}, runs {RUNS}"
    );
    // Small graphs stop at EVERY host observation; large graphs cross many
    // evaluator runs and cover their beginning, middle and final partial run.
    let count = reference.1.len();
    let mut stops: Vec<_> = (1..=count.min(80)).collect();
    stops.extend([count / 2, count.saturating_sub(1), count]);
    stops.sort_unstable();
    stops.dedup();
    for stop in stops.into_iter().filter(|&stop| stop > 0) {
        assert_eq!(
            run::<RUNS>(&query, &vertices, &edges, true, wide, Some(stop)),
            run::<RUNS>(&query, &vertices, &edges, false, wide, Some(stop)),
            "{text}, size {size}, stop {stop}, runs {RUNS}"
        );
    }
    let Ok(success) = reference.0 else {
        // Output-expression errors were already compared, including their
        // complete call prefix and stops before the data exception.
        return;
    };
    let mut limits: Vec<_> = (0..=success.evaluator.work_units.min(70)).collect();
    limits.extend([
        success.evaluator.work_units.saturating_sub(1),
        success.evaluator.work_units,
        success.evaluator.work_units + 1,
        2047,
        2048,
        3071,
        3072,
    ]);
    for work in limits {
        let policy = GqlQueryPolicy::new(1_000_000, 100, work, 1_000_000);
        assert_eq!(
            run::<RUNS>(&query, &vertices, &edges, true, policy, None),
            run::<RUNS>(&query, &vertices, &edges, false, policy, None),
            "{text}, size {size}, work {work}, runs {RUNS}"
        );
    }
    for scratch in (0..=success.evaluator.scratch_entries + 1).chain([2047, 2048]) {
        let policy = GqlQueryPolicy::new(1_000_000, 100, 100_000_000, scratch);
        assert_eq!(
            run::<RUNS>(&query, &vertices, &edges, true, policy, None),
            run::<RUNS>(&query, &vertices, &edges, false, policy, None),
            "{text}, size {size}, scratch {scratch}, runs {RUNS}"
        );
    }
    for policy in [
        GqlQueryPolicy::new(0, 100, 100_000_000, 1_000_000),
        GqlQueryPolicy::new(
            success.rows.snapshot_records.saturating_sub(1),
            100,
            100_000_000,
            1_000_000,
        ),
        GqlQueryPolicy::new(success.rows.snapshot_records, 0, 100_000_000, 1_000_000),
        GqlQueryPolicy::new(success.rows.snapshot_records, 1, 100_000_000, 1_000_000),
    ] {
        assert_eq!(
            run::<RUNS>(&query, &vertices, &edges, true, policy, None),
            run::<RUNS>(&query, &vertices, &edges, false, policy, None),
            "{text}, size {size}, row limits, runs {RUNS}"
        );
    }
}

/// Source summaries are the existing visitor's answers, errors, logical
/// counters AND host call transcript, in per-unit and batched host modes.
#[test]
fn counts_keep_the_ordinary_visitor_transcript_and_finalizer() {
    for text in [
        "MATCH (n) RETURN COUNT(*) AS n",
        "MATCH (n) RETURN COUNT(*) AS n, COUNT(*) AS m",
        "MATCH (n) RETURN COUNT(*) + 1 AS n ORDER BY n DESC LIMIT 1",
        "MATCH (n) RETURN COUNT(*) AS n HAVING n > 2",
        "MATCH (n) RETURN COUNT(*) AS n SKIP 1 LIMIT 0",
        "MATCH (n) RETURN 1 / (COUNT(*) - COUNT(*)) AS n LIMIT 0",
        "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n",
        "MATCH (a)<-[:R]-(b) RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n, COUNT(*) AS m",
        "MATCH (a)-[:T]->(b) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n HAVING n > 2 LIMIT 0",
    ] {
        for size in [0, 1, 7] {
            compare::<false>(text, size);
            compare::<true>(text, size);
        }
    }
    for text in [
        "MATCH (n) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n",
    ] {
        compare::<false>(text, 4097);
        compare::<true>(text, 4097);
    }
}

/// Ineligible inputs can fail or change multiplicity before reduction. A
/// source cardinality must never erase their expression/visibility semantics.
#[test]
fn only_plain_filterless_counts_admit_source_summaries() {
    for text in [
        "MATCH (n:L) RETURN COUNT(*) AS n",
        "MATCH (n) WHERE n.p = 1 RETURN COUNT(*) AS n",
        "MATCH (n) WHERE 1 / n.p = 1 RETURN COUNT(*) AS n",
        "MATCH (n) RETURN n, COUNT(*) AS nrows GROUP BY n",
        "MATCH (n) RETURN COUNT(n.p) AS n",
        "MATCH (n) RETURN COUNT(DISTINCT n.p) AS n",
        "MATCH (n) RETURN SUM(n.p) AS n",
        "MATCH (a)-[:R]-(b) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R|S]->(b) RETURN COUNT(*) AS n",
        "MATCH (a)-[r]->(b) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R]->(b:L) RETURN COUNT(*) AS n",
    ] {
        let query = query(text);
        assert_eq!(query.count_summary_source(), None, "{text}");
        assert!(
            query
                .prepare_count_metadata(GraphCountSource::Vertices, 9)
                .is_none()
        );
        assert!(
            query
                .prepare_count_summary(GraphCountSource::Vertices, 9, 9)
                .is_none()
        );
    }
    let vertices = query("MATCH (n) RETURN COUNT(*) AS n");
    assert!(
        vertices
            .prepare_count_metadata(GraphCountSource::Relation(RelationId(1)), 9)
            .is_none()
    );
    assert!(
        vertices
            .prepare_count_summary(GraphCountSource::Vertices, 9, 8)
            .is_none()
    );
    assert!(
        vertices
            .prepare_count_summary(GraphCountSource::Relation(RelationId(1)), 9, 9)
            .is_none()
    );
    let edges = query("MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n");
    assert!(
        edges
            .prepare_count_metadata(GraphCountSource::Relation(RelationId(2)), 9)
            .is_none()
    );
    assert!(
        edges
            .prepare_count_summary(GraphCountSource::Relation(RelationId(1)), 8, 9)
            .is_none()
    );
    assert!(
        edges
            .prepare_count_summary(GraphCountSource::Relation(RelationId(2)), 9, 9)
            .is_none()
    );
}

#[test]
fn only_one_vertex_label_can_supply_exact_metadata() {
    let labeled = query("MATCH (n:L) RETURN COUNT(*) AS n");
    let source = GraphCountSource::Label(LabelId(1));
    assert_eq!(labeled.count_metadata_source(), Some(source));
    assert_eq!(labeled.count_summary_source(), None);
    assert!(labeled.prepare_count_summary(source, 10, 5).is_none());
    assert!(labeled.prepare_count_metadata(source, 5).is_some());
    for other in [
        GraphCountSource::Vertices,
        GraphCountSource::Label(LabelId(2)),
        GraphCountSource::Relation(RelationId(1)),
    ] {
        assert!(labeled.prepare_count_metadata(other, 5).is_none());
    }
    for text in [
        "MATCH (n:L:M) RETURN COUNT(*) AS n",
        "MATCH (n:L) WHERE n.p = 1 RETURN COUNT(*) AS n",
        "MATCH (n:L) RETURN COUNT(n.p) AS n",
        "MATCH (n:L) RETURN n.p, COUNT(*) AS n GROUP BY n.p",
        "MATCH (n:L)-[:R]->(m) RETURN COUNT(*) AS n",
    ] {
        let prepared = query(text);
        assert_eq!(prepared.count_metadata_source(), None, "{text}");
        assert!(
            prepared.prepare_count_metadata(source, 5).is_none(),
            "{text}"
        );
    }
}

#[test]
fn universal_endpoint_labels_use_exact_population_proofs_and_the_native_finalizer() {
    let vertices = [VId(0), VId(17), VId(u128::MAX)];
    let edges = [
        (vertices[0], RelationId(1), vertices[1]),
        (vertices[0], RelationId(1), vertices[1]),
        (vertices[1], RelationId(1), vertices[1]),
        (vertices[2], RelationId(2), vertices[0]),
    ];
    let policy = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
    for text in [
        "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(*) AS n",
        "MATCH (a:L)<-[:R]-(b:M) RETURN COUNT(*) AS n,COUNT(b) AS again",
        "MATCH (a:L:M)-[]->(b:L) RETURN COUNT(a) AS n HAVING COUNT(a) > 0",
        "MATCH (a:L)-[:R]->(b) RETURN COUNT(*) AS n ORDER BY n LIMIT 0",
    ] {
        let query = query(text);
        assert!(query.count_metadata_source().is_none(), "{text}");
        let mut reads = Vec::new();
        let count = query
            .prepare_count_metadata_with_universal_labels(|source| {
                reads.push(source);
                Ok::<_, ()>(Some(match source {
                    GraphCountSource::Vertices | GraphCountSource::Label(_) => 3,
                    GraphCountSource::Edges => 4,
                    GraphCountSource::Relation(RelationId(1)) => 3,
                    GraphCountSource::Relation(_) => 1,
                }))
            })
            .unwrap()
            .unwrap();
        assert!(reads.contains(&GraphCountSource::Vertices));
        assert!(reads.contains(&GraphCountSource::Label(LabelId(1))));
        let expected = query
            .execute_governed(
                7,
                vertices,
                edges,
                |_, predicates| -> Result<bool, ()> {
                    assert!(predicates.iter().all(|predicate| matches!(
                        predicate,
                        crate::algebra::VertexPredicate::HasLabel(LabelId(1) | LabelId(2))
                    )));
                    Ok(true)
                },
                |_, _| -> Result<Option<&fgdb_types::CanonicalPropertyValue>, ()> {
                    panic!("identity counts do not read input properties")
                },
                policy,
                || Ok::<_, ()>(()),
            )
            .unwrap();
        let actual = count
            .execute_governed::<(), ()>(reads.len() as u64, policy, || Ok(()))
            .unwrap();
        assert_eq!(actual.value, expected.value, "{text}");
        assert_eq!(actual.rows.snapshot_records, reads.len() as u64);
    }
}

#[test]
fn endpoint_label_metadata_refuses_unknown_subsets_and_unsupported_intersections() {
    let labeled = query("MATCH (a:L)-[:R]->(b:M) RETURN COUNT(*) AS n");
    for missing in [
        GraphCountSource::Relation(RelationId(1)),
        GraphCountSource::Vertices,
        GraphCountSource::Label(LabelId(1)),
        GraphCountSource::Label(LabelId(2)),
    ] {
        assert!(
            labeled
                .prepare_count_metadata_with_universal_labels(|source| {
                    Ok::<_, ()>((source != missing).then_some(3))
                })
                .unwrap()
                .is_none()
        );
    }
    for label_count in [0, 1, 2, 4] {
        assert!(
            labeled
                .prepare_count_metadata_with_universal_labels(|source| {
                    Ok::<_, ()>(Some(if source == GraphCountSource::Label(LabelId(2)) {
                        label_count
                    } else {
                        3
                    }))
                })
                .unwrap()
                .is_none()
        );
    }
    for text in [
        "MATCH (a:L)-[:R]->(b) WHERE a.p = 1 RETURN COUNT(*) AS n",
        "MATCH (a:L)-[:R]-(b:L) RETURN COUNT(*) AS n",
        "MATCH (a:L)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS n",
        "MATCH (a:L)-[:R]->(a) RETURN COUNT(*) AS n",
        "MATCH (a:L)-[:R]->(b:L) RETURN COUNT(DISTINCT b) AS n",
        "MATCH (a:L)-[r:R]->(b:L) RETURN COUNT(r) AS n",
    ] {
        let prepared = query(text);
        assert!(
            prepared
                .prepare_count_metadata_with_universal_labels(|_| -> Result<Option<u64>, ()> {
                    panic!("unsupported complete shape must refuse before metadata: {text}")
                })
                .unwrap()
                .is_none(),
            "{text}"
        );
    }
}

#[test]
fn endpoint_label_proofs_stop_at_each_lookup_error() {
    let query = query("MATCH (a:L)-[:R]->(b:M) RETURN COUNT(*) AS n");
    for stop in 1..=4 {
        let mut reads = 0;
        let result = query.prepare_count_metadata_with_universal_labels(|_| {
            reads += 1;
            if reads == stop {
                Err(stop)
            } else {
                Ok(Some(3))
            }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(reads, stop);
    }
}

#[test]
fn wildcard_directed_counts_admit_only_exact_metadata() {
    for text in [
        "MATCH ()-[r]->() RETURN COUNT(*) AS n",
        "MATCH (a)-[r]->(b) RETURN COUNT(*) AS n",
        "MATCH (a)<-[r]-(b) RETURN COUNT(*) AS n",
    ] {
        let wildcard = query(text);
        assert_eq!(
            wildcard.count_metadata_source(),
            Some(GraphCountSource::Edges)
        );
        assert_eq!(wildcard.count_summary_source(), None);
        assert!(
            wildcard
                .prepare_count_summary(GraphCountSource::Edges, 9, 9)
                .is_none()
        );
        assert!(
            wildcard
                .prepare_count_metadata(GraphCountSource::Edges, 9)
                .is_some()
        );
        for other in [
            GraphCountSource::Vertices,
            GraphCountSource::Label(LabelId(1)),
            GraphCountSource::Relation(RelationId(1)),
        ] {
            assert!(wildcard.prepare_count_metadata(other, 9).is_none());
        }
    }
    for text in [
        "MATCH (a)-[r]-(b) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R|S]->(b) RETURN COUNT(*) AS n",
        "MATCH (a)-[r]->(b:L) RETURN COUNT(*) AS n",
        "MATCH (a)-[r]->(b) WHERE a.p IS NULL RETURN COUNT(*) AS n",
        "MATCH (a)-[r]->(b) RETURN COUNT(a.p) AS n",
        "MATCH (a)-[r]->(b) RETURN COUNT(DISTINCT a) AS n",
        "MATCH (a)-[r]->(b) RETURN a, COUNT(*) AS n GROUP BY a",
    ] {
        let prepared = query(text);
        assert_eq!(prepared.count_metadata_source(), None, "{text}");
        assert!(
            prepared
                .prepare_count_metadata(GraphCountSource::Edges, 9)
                .is_none(),
            "{text}"
        );
    }
}

#[test]
fn identity_counts_require_a_proven_nonnull_occurrence() {
    for (text, source) in [
        ("MATCH (n) RETURN COUNT(n) AS n", GraphCountSource::Vertices),
        (
            "MATCH (n) RETURN COUNT(DISTINCT n) AS n, COUNT(n) AS m, COUNT(*) AS k",
            GraphCountSource::Vertices,
        ),
        (
            "MATCH (n:L) RETURN COUNT(n) AS n",
            GraphCountSource::Label(LabelId(1)),
        ),
        (
            "MATCH (n:L) RETURN COUNT(DISTINCT n) AS n",
            GraphCountSource::Label(LabelId(1)),
        ),
        (
            "MATCH (a)-[:R]->(b) RETURN COUNT(a) AS n, COUNT(b) AS m",
            GraphCountSource::Relation(RelationId(1)),
        ),
        (
            "MATCH (a)<-[r:R]-(b) RETURN COUNT(r) AS n, COUNT(*) AS m",
            GraphCountSource::Relation(RelationId(1)),
        ),
        (
            "MATCH (a)-[r]->(b) RETURN COUNT(r) AS n, COUNT(a) AS m",
            GraphCountSource::Edges,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS n, COUNT(a) AS m",
            GraphCountSource::Relation(RelationId(1)),
        ),
        (
            "MATCH (a)<-[r:R]-(b) RETURN COUNT(DISTINCT r) AS n",
            GraphCountSource::Relation(RelationId(1)),
        ),
        (
            "MATCH (a)-[r]->(b) RETURN COUNT(DISTINCT r) AS n, COUNT(r) AS m",
            GraphCountSource::Edges,
        ),
    ] {
        let query = query(text);
        assert_eq!(query.count_metadata_source(), Some(source), "{text}");
        // Historical/scoped scan summaries have a different evaluator
        // transcript and must continue through the ordinary counted input.
        assert_eq!(query.count_summary_source(), None, "{text}");
        assert!(query.prepare_count_summary(source, 10, 10).is_none());
        for count in [0, 1, 900_000, u64::MAX] {
            let result = run_metadata::<false>(
                &query,
                count,
                GqlQueryPolicy::new(1, 10, 10_000, 10_000),
                None,
            )
            .0
            .unwrap();
            for value in result.value[0].values() {
                assert_eq!(value.as_count(), Some(count), "{text}");
            }
        }
    }
    for text in [
        "MATCH (n) RETURN COUNT(n.p) AS n",
        "MATCH (n) RETURN COUNT(DISTINCT n.p) AS n",
        "MATCH (n:L:M) RETURN COUNT(n) AS n",
        "MATCH (n) WHERE n.p = 1 RETURN COUNT(n) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(r.p) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT a) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT b) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS n, COUNT(DISTINCT b) AS m",
        "MATCH (a)-[r:R]->(a) RETURN COUNT(r) AS n",
        "MATCH (a)-[r:R]-(b) RETURN COUNT(r) AS n",
        "MATCH (a:L)-[r:R]->(b) RETURN COUNT(r) AS n",
        "MATCH (a)-[r:R]->(b)-[:R]->(c) RETURN COUNT(r) AS n",
        "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b) RETURN COUNT(r) AS n",
        "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS n",
    ] {
        let query = query(text);
        assert_eq!(query.count_metadata_source(), None, "{text}");
    }
}

#[test]
fn edge_identity_metadata_matches_real_parallel_and_loop_occurrences() {
    let vertices = [VId(0), VId(7), VId(u128::MAX)];
    let edges = [
        (EId(0), vertices[0], RelationId(1), vertices[1]),
        (EId(u128::MAX), vertices[0], RelationId(1), vertices[1]),
        (EId(9), vertices[1], RelationId(1), vertices[1]),
        (EId(10), vertices[2], RelationId(2), vertices[0]),
    ];
    let policy = GqlQueryPolicy::new(100, 100, 1_000_000, 100_000);
    for text in [
        "MATCH (a)-[r:R]->(b) RETURN COUNT(r) AS n, COUNT(a) AS m, COUNT(*) AS k",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS n, COUNT(r) AS m, COUNT(*) AS k",
        "MATCH (a)<-[r:R]-(b) RETURN COUNT(r) AS n",
        "MATCH (a)<-[r:R]-(b) RETURN COUNT(DISTINCT r) AS n",
        "MATCH (a)-[r]->(b) RETURN COUNT(r) AS n",
        "MATCH (a)-[r]->(b) RETURN COUNT(DISTINCT r) AS n",
        "MATCH (a)-[r:T]->(b) RETURN COUNT(r) AS n",
        "MATCH (a)-[r:T]->(b) RETURN COUNT(DISTINCT r) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(r) + 1 AS n HAVING n > 2 ORDER BY n DESC LIMIT 1",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) + 1 AS n HAVING n > 2 ORDER BY n DESC LIMIT 1",
        "MATCH (a)-[r:R]->(b) RETURN 1 / (COUNT(r) - COUNT(*)) AS n LIMIT 0",
        "MATCH (a)-[r:R]->(b) RETURN 1 / (COUNT(DISTINCT r) - COUNT(*)) AS n LIMIT 0",
    ] {
        let query = query(text);
        for source_edges in [edges.as_slice(), &[]] {
            let expected = query.execute_governed_with_identified_properties(
                (vertices.len() + source_edges.len()) as u64,
                vertices,
                source_edges.iter().copied(),
                |_, _| -> Result<bool, ()> { panic!("no vertex predicate") },
                |_, _| -> Result<Option<&CanonicalPropertyValue>, ()> {
                    panic!("identity counts cannot read a property")
                },
                policy,
                || Ok::<(), usize>(()),
            );
            let matching = match query.count_metadata_source().unwrap() {
                GraphCountSource::Edges => source_edges.len() as u64,
                GraphCountSource::Relation(relation) => source_edges
                    .iter()
                    .filter(|(_, _, actual, _)| *actual == relation)
                    .count() as u64,
                _ => panic!("single-edge fixture"),
            };
            let actual = run_metadata::<false>(&query, matching, policy, None).0;
            assert_eq!(
                actual.as_ref().map(|result| &result.value),
                expected.as_ref().map(|result| &result.value),
                "{text}"
            );
        }
    }
}

fn run_metadata<const RUNS: bool>(
    query: &PreparedGraphAggregate,
    matching: u64,
    policy: GqlQueryPolicy,
    stop: Option<usize>,
) -> Outcome {
    let mut calls = Vec::new();
    let result = query
        .prepare_count_metadata(query.count_metadata_source().unwrap(), matching)
        .unwrap()
        .execute_governed(
            1,
            policy,
            Host::<RUNS> {
                calls: &mut calls,
                stop,
                refused: false,
            },
        );
    (result, calls)
}

#[test]
fn exact_metadata_keeps_the_native_finalizer_and_has_cardinality_independent_work() {
    fn compare_metadata<const RUNS: bool>() {
        let policy = GqlQueryPolicy::new(100_000, 100, 10_000_000, 1_000_000);
        for text in [
            "MATCH (n) RETURN COUNT(*) AS n",
            "MATCH (n) RETURN COUNT(n) AS n",
            "MATCH (n) RETURN COUNT(n) AS n, COUNT(*) AS m",
            "MATCH (n) RETURN COUNT(DISTINCT n) AS n, COUNT(n) AS m, COUNT(*) AS k",
            "MATCH (n:L) RETURN COUNT(*) AS n",
            "MATCH (n:L) RETURN COUNT(n) AS n",
            "MATCH (n:L) RETURN COUNT(DISTINCT n) AS n",
            "MATCH (n:L) RETURN COUNT(DISTINCT n) + 1 AS n HAVING n > COUNT(*) ORDER BY n DESC LIMIT 1",
            "MATCH (n:L) RETURN 1 / (COUNT(DISTINCT n) - COUNT(*)) AS n LIMIT 0",
            "MATCH (n:L) RETURN COUNT(*) + 1 AS n HAVING n > 2 ORDER BY n DESC LIMIT 1",
            "MATCH (n:L) RETURN 1 / (COUNT(*) - COUNT(*)) AS n LIMIT 0",
            "MATCH (n) RETURN COUNT(*) AS n, COUNT(*) AS m",
            "MATCH (n) RETURN COUNT(*) + 1 AS n ORDER BY n DESC LIMIT 1",
            "MATCH (n) RETURN COUNT(*) AS n HAVING n > 2",
            "MATCH (n) RETURN COUNT(*) AS n SKIP 1 LIMIT 0",
            "MATCH (n) RETURN 1 / (COUNT(*) - COUNT(*)) AS n LIMIT 0",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) RETURN COUNT(a) AS n, COUNT(b) AS m",
            "MATCH (a)<-[:R]-(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[:T]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[r]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)<-[r]-(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[r]->(b) RETURN COUNT(*) + 1 AS n HAVING n > 2 ORDER BY n DESC LIMIT 1",
            "MATCH (a)-[r]->(b) RETURN 1 / (COUNT(*) - COUNT(*)) AS n LIMIT 0",
        ] {
            let query = query(text);
            for size in [0, 1, 7, 4097] {
                let vertices: Vec<_> = (0..size).map(|n| VId(u128::MAX - n as u128)).collect();
                let edges: Vec<_> = (0..size * 2)
                    .map(|n| {
                        (
                            vertices[n % size],
                            RelationId(if n % 3 == 0 { 2 } else { 1 }),
                            vertices[(n * 7) % size],
                        )
                    })
                    .collect();
                let matching = match query.count_metadata_source().unwrap() {
                    GraphCountSource::Vertices => size as u64,
                    GraphCountSource::Edges => edges.len() as u64,
                    GraphCountSource::Label(_) => {
                        vertices.iter().filter(|vid| vid.0 % 3 == 0).count() as u64
                    }
                    GraphCountSource::Relation(relation) => {
                        edges.iter().filter(|&&(_, r, _)| r == relation).count() as u64
                    }
                };
                let expected = run::<RUNS>(&query, &vertices, &edges, false, policy, None);
                let actual = run_metadata::<RUNS>(&query, matching, policy, None);
                assert_eq!(
                    actual.0.as_ref().map(|r| &r.value),
                    expected.0.as_ref().map(|r| &r.value),
                    "{text}, size {size}"
                );
                if let Ok(result) = actual.0 {
                    assert_eq!(result.rows.snapshot_records, 1);
                }
            }
        }
        // Full u64 cardinality is exact; no conversion through a signed row
        // multiplicity or floating-point value is permitted.
        for text in [
            "MATCH (n) RETURN COUNT(*) AS n",
            "MATCH (n) RETURN COUNT(DISTINCT n) AS n",
            "MATCH (a)-[r]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[r]->(b) RETURN COUNT(DISTINCT r) AS n",
        ] {
            let query = query(text);
            let mut work = None;
            for matching in [0, 1, 900_000, u64::MAX] {
                let result = run_metadata::<RUNS>(&query, matching, policy, None)
                    .0
                    .unwrap();
                assert_eq!(result.value[0].values()[0].as_count(), Some(matching));
                let observed = (
                    result.evaluator.work_units,
                    result.evaluator.scratch_entries,
                );
                assert_eq!(*work.get_or_insert(observed), observed);
            }
        }
    }
    compare_metadata::<false>();
    compare_metadata::<true>();
}

#[test]
fn computed_having_aliases_keep_hidden_summaries_and_full_width_counts() {
    let policy = GqlQueryPolicy::new(1, 10, 100_000, 100_000);
    for text in [
        "MATCH (n) RETURN COUNT(*) + 1 AS n HAVING n > COUNT(*)",
        "MATCH (n) RETURN COUNT(*) + 1 AS n, COUNT(*) AS raw HAVING n > raw",
        "MATCH (n) RETURN COUNT(*) + 1 AS n HAVING n - 1 = COUNT(*) AND (COUNT(*) + 1) > COUNT(*)",
        "MATCH (n) RETURN COUNT(*) + 1 AS n HAVING n + 1 = COUNT(*) + 2",
    ] {
        let prepared = query(text);
        for count in [0, 1, u64::MAX] {
            let result = run_metadata::<false>(&prepared, count, policy, None)
                .0
                .unwrap();
            assert_eq!(result.value.len(), 1, "{text}, count {count}");
            assert_eq!(
                result.value[0].values()[0].as_integer(),
                Some(i128::from(count) + 1)
            );
            if result.value[0].values().len() == 2 {
                assert_eq!(result.value[0].values()[1].as_count(), Some(count));
            }
        }
    }
    let prepared = query("MATCH (n) RETURN COUNT(*) + 1 AS n HAVING n > 2");
    for (count, expected) in [(0, None), (1, None), (2, Some(3))] {
        let result = run_metadata::<false>(&prepared, count, policy, None)
            .0
            .unwrap();
        assert_eq!(result.value.len(), usize::from(expected.is_some()));
        assert_eq!(
            result
                .value
                .first()
                .and_then(|row| row.values()[0].as_integer()),
            expected
        );
    }
    let maintained = prepared.incremental_source_definition().unwrap();
    let source = query("MATCH (n) RETURN COUNT(*) AS c");
    for count in [0, 1, 2, u64::MAX] {
        let row = source
            .incremental_row(Vec::new(), vec![GraphAggregateValue::Count(count)])
            .unwrap();
        assert_eq!(
            maintained
                .evaluate_incremental_having(&row, &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            Some(count > 1),
        );
    }
}

#[test]
fn computed_having_errors_survive_decisive_booleans_and_empty_pages() {
    let policy = GqlQueryPolicy::new(1, 10, 100_000, 100_000);
    for condition in [
        "TRUE OR bad > 0",
        "FALSE AND bad > 0",
        "bad IS NULL",
        "TRUE OR bad + 1 > 0",
        "FALSE AND COUNT(*) / (COUNT(*) - COUNT(*)) > 0",
        "COUNT(*) / 0 IN []",
    ] {
        let text = format!(
            "MATCH (n) RETURN COUNT(*) / (COUNT(*) - COUNT(*)) AS bad HAVING {condition} LIMIT 0"
        );
        let prepared = query(&text);
        assert!(
            matches!(
                run_metadata::<false>(&prepared, 7, policy, None).0,
                Err(GqlQueryError::Source(
                    GraphAggregateError::OutputExpression {
                        column: 0,
                        error: crate::GraphIntegerError {
                            kind: crate::GraphIntegerErrorKind::DivisionByZero,
                            ..
                        },
                    }
                ))
            ),
            "{text}"
        );
    }
}

#[test]
fn computed_having_checks_input_columns_and_refuses_unowned_element_reads() {
    use crate::{
        GraphElementKey, GraphHavingExpression, GraphHavingOp, GraphHavingOperand, GraphSetValue,
    };
    let expression = |value| {
        GraphHavingExpression::prepare(&[GraphHavingOp::IsNull {
            operand: GraphHavingOperand::Expression {
                value: std::sync::Arc::new(value),
                column: 0,
            },
            is_null: false,
        }])
        .unwrap()
    };
    assert!(matches!(
        query("MATCH (n) RETURN COUNT(*) AS c")
            .with_having_expression(&expression(GraphSetValue::Column(99))),
        Err(crate::GraphHavingError::InvalidExpression {
            column: 0,
            error: crate::GraphSetProjectionError::UnknownInput { input: 99, .. },
        })
    ));
    assert!(
        query("MATCH (n) RETURN COUNT(*) AS c")
            .with_having_expression(&expression(GraphSetValue::Column(0)))
            .is_ok()
    );
    let value = GraphSetValue::MapGet {
        map: Box::new(GraphSetValue::Column(0)),
        key: "p".into(),
        property: GraphElementKey::Property(PropertyKeyId(1)),
    };
    assert!(matches!(
        query("MATCH (n) RETURN n, COUNT(*) AS c GROUP BY n")
            .with_having_expression(&expression(value)),
        Err(crate::GraphHavingError::ExpressionElementRead { column: 0 })
    ));
    let resolved = |inputs: Vec<GraphSetValue>, value| {
        GraphHavingExpression::prepare(&[GraphHavingOp::IsNull {
            operand: GraphHavingOperand::ResolvedExpression {
                inputs: inputs.into(),
                value: std::sync::Arc::new(value),
                column: 0,
            },
            is_null: false,
        }])
        .unwrap()
    };
    let count = query("MATCH (n) RETURN COUNT(*) AS c");
    for (inputs, value) in [
        (vec![GraphSetValue::Column(99)], GraphSetValue::Column(0)),
        (vec![GraphSetValue::Column(0)], GraphSetValue::Column(99)),
    ] {
        assert!(matches!(
            count
                .clone()
                .with_having_expression(&resolved(inputs, value)),
            Err(crate::GraphHavingError::InvalidExpression {
                error: crate::GraphSetProjectionError::UnknownInput { input: 99, .. },
                ..
            })
        ));
    }
    assert!(matches!(
        count.with_having_expression(&resolved(
            vec![GraphSetValue::Column(0); crate::MAX_GRAPH_INTEGER_INSTRUCTIONS + 1],
            GraphSetValue::Column(0),
        )),
        Err(crate::GraphHavingError::InvalidExpression {
            error: crate::GraphSetProjectionError::TooManyColumns { .. },
            ..
        })
    ));
    assert!(matches!(
        query("MATCH (n) RETURN n, COUNT(*) AS c GROUP BY n").with_having_expression(&resolved(
            vec![GraphSetValue::MapGet {
                map: Box::new(GraphSetValue::Column(0)),
                key: "p".into(),
                property: GraphElementKey::Property(PropertyKeyId(1)),
            }],
            GraphSetValue::Column(0),
        )),
        Err(crate::GraphHavingError::ExpressionElementRead { column: 0 })
    ));
}

#[test]
fn exact_metadata_checks_every_budget_and_cancellation_boundary() {
    for text in [
        "MATCH (n) RETURN COUNT(*) AS n",
        "MATCH (n) RETURN COUNT(DISTINCT n) AS n, COUNT(*) AS m",
        "MATCH (n:L) RETURN COUNT(DISTINCT n) + 1 AS n HAVING n > 2 ORDER BY n DESC LIMIT 1",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS n, COUNT(a) AS m",
        "MATCH (a)<-[r]-(b) RETURN COUNT(DISTINCT r) AS n SKIP 1 LIMIT 0",
        "MATCH (n:L) RETURN COUNT(*) AS n",
        "MATCH (n:L) RETURN COUNT(*) + 1 AS n HAVING n > 2 ORDER BY n DESC LIMIT 1",
        "MATCH (n:L) RETURN COUNT(*) + 1 AS n HAVING n + 1 > COUNT(*) ORDER BY n DESC LIMIT 1",
        "MATCH (n:L) RETURN [COUNT(*)][0] AS n HAVING n + 1 > COUNT(*) ORDER BY n DESC LIMIT 1",
        "MATCH (n:L) RETURN COUNT(*) AS n HAVING COUNT(*) + 1 > n ORDER BY n DESC LIMIT 1",
        "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n, COUNT(*) AS m",
        "MATCH (a)-[r]->(b) RETURN COUNT(*) AS n, COUNT(*) AS m",
        "MATCH (a)<-[r]-(b) RETURN COUNT(*) AS n SKIP 1 LIMIT 0",
        "MATCH (n) RETURN COUNT(*) AS n SKIP 1 LIMIT 0",
    ] {
        let query = query(text);
        let wide = GqlQueryPolicy::new(1, 100, 1_000_000, 1_000_000);
        let (result, calls) = run_metadata::<false>(&query, 900_000, wide, None);
        let result = result.unwrap();
        let exact = GqlQueryPolicy::new(
            1,
            result.rows.result_rows,
            result.evaluator.work_units,
            result.evaluator.scratch_entries,
        );
        assert_eq!(
            run_metadata::<false>(&query, 900_000, exact, None)
                .0
                .unwrap(),
            result
        );
        for stop in 1..=calls.len() {
            let interrupted = run_metadata::<false>(&query, 900_000, wide, Some(stop));
            assert!(matches!(interrupted.0, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(interrupted.1.len(), stop);
        }
        for work in 0..result.evaluator.work_units {
            assert!(matches!(
                run_metadata::<false>(&query, 900_000, GqlQueryPolicy::new(1, 100, work, 1_000_000), None).0,
                Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1
            ));
        }
        for scratch in 0..result.evaluator.scratch_entries {
            assert!(matches!(
                run_metadata::<false>(&query, 900_000, GqlQueryPolicy::new(1, 100, 1_000_000, scratch), None).0,
                Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1
            ));
        }
        assert!(matches!(
            run_metadata::<false>(&query, 900_000, GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000), None).0,
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.observed == 1
        ));
        if result.rows.result_rows > 0 {
            assert!(matches!(
                run_metadata::<false>(&query, 900_000, GqlQueryPolicy::new(1, 0, 1_000_000, 1_000_000), None).0,
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 1
            ));
        }
    }
}
