use super::*;
use crate::algebra::{
    GraphColumn, GraphMatchClause, GraphMatchMode, GraphPathFunction, GraphPatternBuilder,
};

fn collection(
    direction: GlaDirection,
    two: bool,
    count: Option<u64>,
) -> PreparedGraphPattern<GraphValueRow> {
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap().vertex("b").unwrap();
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    outer.capture_edge("r", 0).unwrap();
    let mut inner = GraphPatternBuilder::new();
    inner.match_mode(GraphMatchMode::DifferentEdges);
    inner.vertex("b").unwrap().vertex("c").unwrap();
    inner.edge("b", S, direction, "c").unwrap();
    inner.capture_edge("s", 0).unwrap();
    let (last, edge, tuple) = if two {
        inner.vertex("d").unwrap();
        inner.edge("c", S, direction, "d").unwrap();
        inner.capture_edge("t", 1).unwrap();
        ("d", "t", vec!["b", "s", "c", "t", "d", "p"])
    } else {
        ("c", "s", vec!["b", "s", "c", "p"])
    };
    inner.capture_path("p").unwrap();
    outer
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::collect(&inner, Some(last)),
                GraphMatchClause::collect(&inner, Some(edge)),
                GraphMatchClause::collect(&inner, Some("p")),
                GraphMatchClause::collect_tuple(&inner, tuple),
                GraphMatchClause::collect(&inner, None),
            ],
            &[
                GraphColumn::path("r", "r", GraphPathFunction::Edge),
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::collection("vertices", 0),
                GraphColumn::collection("edges", 1),
                GraphColumn::collection("paths", 2),
                GraphColumn::collection("tuples", 3),
                GraphColumn::collection("nulls", 4),
            ],
            0,
            count,
        )
        .unwrap()
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

// Literal finite relation products, independent of the async/synchronous GLA
// drivers. Lists keep EId order, route identity and parallel multiplicity.
fn expected(source: &Source, direction: GlaDirection, two: bool) -> Vec<GraphValueRow> {
    let orient = |from, to| match direction {
        GlaDirection::Forward => vec![(from, to)],
        GlaDirection::Reverse => vec![(to, from)],
        GlaDirection::Undirected if from == to => vec![(from, to)],
        GlaDirection::Undirected => vec![(from, to), (to, from)],
    };
    let visible = |edge: &&Image| source.hidden != Some(edge.id);
    let roots: Vec<_> = source
        .data
        .edges
        .iter()
        .filter(visible)
        .filter(|edge| edge.relation == R)
        .collect();
    let children: Vec<_> = source
        .data
        .edges
        .iter()
        .filter(visible)
        .filter(|edge| edge.relation == S)
        .collect();
    let mut rows = Vec::new();
    for root in roots {
        let mut routes = Vec::new();
        for first in &children {
            for (from, to) in orient(first.from, first.to) {
                if from != root.to {
                    continue;
                }
                if two {
                    for second in &children {
                        if first.id == second.id {
                            continue;
                        }
                        for (next, end) in orient(second.from, second.to) {
                            if next == to {
                                routes.push(vec![(first.id, to), (second.id, end)]);
                            }
                        }
                    }
                } else {
                    routes.push(vec![(first.id, to)]);
                }
            }
        }
        let mut vertices = Vec::new();
        let mut edges = Vec::new();
        let mut paths = Vec::new();
        let mut tuples = Vec::new();
        let mut nulls = Vec::new();
        for route in routes {
            let &(eid, end) = route.last().unwrap();
            let path = GraphValue::Path(GraphPath::new(root.to, route.clone().into_boxed_slice()));
            vertices.push(GraphValue::Vertex(end));
            edges.push(GraphValue::Edge(eid));
            paths.push(path.clone());
            let mut fields = vec![GraphValue::Vertex(root.to)];
            for (edge, target) in route {
                fields.push(GraphValue::Edge(edge));
                fields.push(GraphValue::Vertex(target));
            }
            fields.push(path);
            tuples.push(list(fields));
            nulls.push(GraphValue::Scalar(CanonicalScalar::Null));
        }
        rows.push(GraphValueRow::from_owned_values(vec![
            GraphValue::Edge(root.id),
            GraphValue::Vertex(root.from),
            GraphValue::Vertex(root.to),
            list(vertices),
            list(edges),
            list(paths),
            list(tuples),
            list(nulls),
        ]));
    }
    rows
}

fn drain_values<F: FnMut() -> Result<(), usize> + Send>(
    cursor: &mut AsyncEdgeJoinCursor<Source, F>,
) -> ScanResult<Vec<GraphValueRow>, &'static str, usize> {
    run(async {
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().await {
            let row = row?;
            rows.push(GraphValueRow::from_owned_values(row.values().to_vec()));
        }
        Ok(rows)
    })
}

fn released(counts: &Counts) {
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(counts.retained_scratch.load(Ordering::SeqCst), 0);
    assert_eq!(counts.output_scratch.load(Ordering::SeqCst), 0);
}

#[test]
fn async_fixed_collections_match_finite_products_with_masked_and_parallel_witnesses() {
    let mut cases = 0;
    for mask in 0..64 {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for two in [false, true] {
                for hidden in [None, Some(EId(3))] {
                    let mut source = Source::new(mask);
                    source.hidden = hidden;
                    let want = expected(&source, direction, two);
                    let counts = source.counts.clone();
                    let definition = collection(direction, two, None);
                    let frozen = definition.canonical_bytes();
                    let plan = AsyncEdgeJoinPlan::compile(definition.plan()).unwrap();
                    assert!(AsyncEdgeJoinPlan::compile_sort_input(definition.plan()).is_ok());
                    assert_eq!(definition.canonical_bytes(), frozen);
                    let mut cursor = AsyncEdgeJoinCursor::new(source, plan, wide(), ok);
                    assert_eq!(
                        drain_values(&mut cursor).unwrap(),
                        want,
                        "mask={mask} two={two}"
                    );
                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                    assert!(counts.peak.load(Ordering::SeqCst) <= if two { 3 } else { 2 });
                    released(&counts);
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 768);
}

#[test]
fn pending_collection_owns_accumulated_values_and_drop_releases_every_reservation() {
    let definition = collection(GlaDirection::Forward, false, Some(1));
    let plan = AsyncEdgeJoinPlan::compile(definition.plan()).unwrap();
    let mut source = Source::new(63);
    source.fault = Fault::PendingAfterWitness;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan, wide(), ok);
    let mut future = Box::pin(cursor.next());
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(counts.seeks.load(Ordering::SeqCst) >= 3);
    assert!(counts.retained_scratch.load(Ordering::SeqCst) > 0);
    assert_eq!(counts.output_scratch.load(Ordering::SeqCst), 0);
    assert_eq!(counts.records.load(Ordering::SeqCst), 1);
    drop(future);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    released(&counts);
}

#[test]
fn delivered_collection_guard_survives_source_and_partial_clone_refuses_atomically() {
    let definition = collection(GlaDirection::Forward, true, Some(1));
    let plan = AsyncEdgeJoinPlan::compile(definition.plan()).unwrap();
    let source = Source::new(63);
    let want = expected(&source, GlaDirection::Forward, true).remove(0);
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan.clone(), wide(), ok);
    let row = run(cursor.next()).unwrap().unwrap();
    assert_eq!(row.row(), &want);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.retained_scratch.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 1);
    assert!(counts.output_scratch.load(Ordering::SeqCst) > 12);
    drop(cursor);
    assert_eq!(row.row(), &want);
    drop(row);
    released(&counts);

    let mut source = Source::new(63);
    source.fault = Fault::Output;
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(source, plan, wide(), ok);
    assert!(matches!(
        run(cursor.next()),
        Some(Err(GqlQueryError::Source(EdgeScanError::Source(
            "collection output admission"
        ))))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
    released(&counts);
}

#[test]
fn collection_source_errors_never_publish_an_already_accumulated_prefix() {
    let definition = collection(GlaDirection::Forward, false, Some(1));
    let plan = AsyncEdgeJoinPlan::compile(definition.plan()).unwrap();
    for fault in [
        Fault::IoAfterWitness,
        Fault::Unavailable,
        Fault::Missing,
        Fault::Twice,
        Fault::Mismatch,
        Fault::FalseEof,
        Fault::Repeat,
    ] {
        let mut source = Source::new(63);
        source.fault = fault;
        let counts = source.counts.clone();
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan.clone(), wide(), ok);
        assert!(matches!(
            run(cursor.next()),
            Some(Err(GqlQueryError::Source(_)))
        ));
        if fault == Fault::IoAfterWitness {
            assert!(counts.seeks.load(Ordering::SeqCst) >= 3);
        }
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        released(&counts);
    }
}

#[test]
fn every_collection_checkpoint_and_budget_refusal_preserves_complete_rows() {
    let definition = collection(GlaDirection::Forward, true, Some(1));
    let plan = AsyncEdgeJoinPlan::compile(definition.plan()).unwrap();
    let mut calls = 0;
    let mut baseline = AsyncEdgeJoinCursor::new(Source::new(63), plan.clone(), wide(), || {
        calls += 1;
        Ok::<_, usize>(())
    });
    let want = drain_values(&mut baseline).unwrap();
    assert_eq!(want.len(), 1);
    let rows = baseline.row_stats();
    let evaluator = baseline.evaluator_stats();
    drop(baseline);
    for boundary in 0..calls {
        let source = Source::new(63);
        let counts = source.counts.clone();
        let mut seen = 0;
        let mut cursor = AsyncEdgeJoinCursor::new(source, plan.clone(), wide(), || {
            let at = seen;
            seen += 1;
            if at == boundary {
                Err(boundary)
            } else {
                Ok(())
            }
        });
        assert!(
            matches!(run(cursor.next()), Some(Err(GqlQueryError::Interrupted(at))) if at == boundary)
        );
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        released(&counts);
    }
    let exact = [
        rows.snapshot_records,
        rows.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    ];
    for dimension in 0..4 {
        for fits in [false, true] {
            let mut limits = exact;
            if !fits {
                limits[dimension] -= 1;
            }
            let source = Source::new(63);
            let counts = source.counts.clone();
            let mut cursor = AsyncEdgeJoinCursor::new(
                source,
                plan.clone(),
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                ok,
            );
            let result = drain_values(&mut cursor);
            if fits {
                assert_eq!(result.unwrap(), want);
            } else {
                assert!(result.is_err());
            }
            released(&counts);
        }
    }
}

#[test]
fn collection_profiles_are_sealed_before_zero_limit_source_access() {
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("a").unwrap().vertex("b").unwrap();
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    outer.capture_edge("r", 0).unwrap();
    for mode in 0..3 {
        let mut inner = GraphPatternBuilder::new();
        inner.match_mode(GraphMatchMode::DifferentEdges);
        let start = if mode == 0 { "x" } else { "b" };
        inner.vertex(start).unwrap().vertex("c").unwrap();
        if mode == 1 {
            inner
                .walk(
                    start,
                    S,
                    GlaDirection::Forward,
                    "c",
                    crate::GraphWalkBounds::new(0, 2).unwrap(),
                )
                .unwrap();
        } else {
            inner.edge(start, S, GlaDirection::Forward, "c").unwrap();
        }
        inner.capture_path("p").unwrap();
        let mut child = GraphPatternBuilder::new();
        let mut clause = GraphMatchClause::collect(&inner, Some("p"));
        if mode == 2 {
            child.vertex("c").unwrap().vertex("d").unwrap();
            child.edge("c", S, GlaDirection::Forward, "d").unwrap();
            clause.nested.push(GraphMatchClause::exists(&child));
        }
        let definition = outer
            .prepare_values_with_clauses(
                &[clause],
                &[
                    GraphColumn::path("r", "r", GraphPathFunction::Edge),
                    GraphColumn::vertex("a", "a"),
                    GraphColumn::vertex("b", "b"),
                    GraphColumn::collection("paths", 0),
                ],
                0,
                Some(0),
            )
            .unwrap();
        assert!(AsyncEdgeJoinPlan::compile(definition.plan()).is_err());
        assert!(AsyncEdgeJoinPlan::compile_sort_input(definition.plan()).is_err());
    }
    let definition = collection(GlaDirection::Forward, false, Some(0));
    let source = Source::new(63);
    let counts = source.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(
        source,
        AsyncEdgeJoinPlan::compile(definition.plan()).unwrap(),
        wide(),
        ok,
    );
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.roots.load(Ordering::SeqCst), 0);
    released(&counts);
}
