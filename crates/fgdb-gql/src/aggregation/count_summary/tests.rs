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
        GraphSymbolKind::Label => Some(GraphSymbol::Label(LabelId(1))),
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
    let source = query.count_summary_source().expect("checked count shape");
    let examined = match source {
        GraphCountSource::Vertices => vertices.len() as u64,
        GraphCountSource::Relation(_) => edges.len() as u64,
    };
    let matching = match source {
        GraphCountSource::Vertices => examined,
        GraphCountSource::Relation(relation) => edges
            .iter()
            .filter(|&&(_, actual, _)| actual == relation)
            .count() as u64,
    };
    let records = vertices.len() as u64
        + if source == GraphCountSource::Vertices {
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
            |_, _| -> Result<bool, ()> { panic!("filterless input tested a vertex") },
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
        "MATCH (n) RETURN COUNT(n) AS n",
        "MATCH (n) RETURN COUNT(DISTINCT n) AS n",
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

fn run_metadata<const RUNS: bool>(
    query: &PreparedGraphAggregate,
    matching: u64,
    policy: GqlQueryPolicy,
    stop: Option<usize>,
) -> Outcome {
    let mut calls = Vec::new();
    let result = query
        .prepare_count_metadata(query.count_summary_source().unwrap(), matching)
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
            "MATCH (n) RETURN COUNT(*) AS n, COUNT(*) AS m",
            "MATCH (n) RETURN COUNT(*) + 1 AS n ORDER BY n DESC LIMIT 1",
            "MATCH (n) RETURN COUNT(*) AS n HAVING n > 2",
            "MATCH (n) RETURN COUNT(*) AS n SKIP 1 LIMIT 0",
            "MATCH (n) RETURN 1 / (COUNT(*) - COUNT(*)) AS n LIMIT 0",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)<-[:R]-(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[:T]->(b) RETURN COUNT(*) AS n",
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
                let matching = match query.count_summary_source().unwrap() {
                    GraphCountSource::Vertices => size as u64,
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
        let query = query("MATCH (n) RETURN COUNT(*) AS n");
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
    compare_metadata::<false>();
    compare_metadata::<true>();
}

#[test]
fn exact_metadata_checks_every_budget_and_cancellation_boundary() {
    for text in [
        "MATCH (n) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n, COUNT(*) AS m",
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
