use super::*;

fn optional_plan(text: &str) -> AsyncEdgeJoinPlan {
    let definition = prepared(text);
    let identity = definition.plan().canonical_bytes();
    let (plan, _) = AsyncEdgeJoinPlan::compile_sort_input(definition.plan()).unwrap();
    assert_eq!(definition.plan().canonical_bytes(), identity);
    assert!(AsyncEdgeJoinPlan::compile(definition.plan()).is_err());
    plan
}

fn source(mask: usize) -> Source {
    let mut source = Source::new(mask);
    source.expected_output_records = 0;
    source
}

fn eager(text: &str, source: &Source) -> Vec<Vec<GraphValue>> {
    let mut vertices = BTreeMap::new();
    for image in source.images.values() {
        vertices.insert(image.from, &image.left);
        vertices.insert(image.to, &image.right);
    }
    prepared(text)
        .plan()
        .execute_governed_with_element_properties(
            (vertices.len() + source.images.len()) as u64,
            vertices.keys().copied(),
            source
                .images
                .iter()
                .map(|(&id, image)| (id, image.from, image.relation, image.to)),
            |id, predicates| {
                Ok::<_, ()>(predicates.iter().all(|predicate| {
                    let property = predicate.property_key().and_then(|key| {
                        vertices[&id]
                            .iter()
                            .find(|entry| entry.0 == key)
                            .map(|entry| (entry.0, &entry.1))
                    });
                    predicate.matches_borrowed(None, property)
                }))
            },
            |id, key| {
                Ok(vertices[&id]
                    .iter()
                    .find(|entry| entry.0 == key)
                    .map(|entry| &entry.1))
            },
            |id, key| {
                Ok(source.images[&id]
                    .props
                    .iter()
                    .find(|entry| entry.0 == key)
                    .map(|entry| &entry.1))
            },
            policy(),
            ok,
        )
        .unwrap()
        .value
        .into_iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn terminal_optional_streams_exact_witness_bags_in_every_direction() {
    let directions = [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ];
    let mut cases = 0;
    for mask in [0, 2, 6, 10, 63] {
        for first in directions {
            for second in directions {
                for filter in ["", "WHERE s.p >= 0", "WHERE c.p > a.p"] {
                    let text = format!(
                        "MATCH (a){} OPTIONAL MATCH (b){} {filter} \
                         RETURN r,a,b,s,c,r.p AS rp,s.p AS sp,c.p AS cp",
                        atom("r", "b", first, false),
                        atom("s", "c", second, true),
                    );
                    let input = source(mask);
                    let expected = eager(&text, &input);
                    let counts = input.counts.clone();
                    let mut cursor =
                        AsyncEdgeJoinCursor::new(input, optional_plan(&text), policy(), ok);
                    let mut rows = Vec::new();
                    while let Some(row) = run(cursor.next()) {
                        rows.push(row.unwrap().values().to_vec());
                    }
                    rows.sort(); // The external consumer owns canonical order.
                    assert_eq!(rows, expected, "{text}; mask {mask}");
                    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                    assert!(counts.peak.load(Ordering::SeqCst) <= 2);
                    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
                    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 135);
}

#[test]
fn optional_absence_is_one_null_extension_and_parallel_witnesses_keep_outer_duplicates() {
    let text = "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (a)-[s:R]->(c) \
                WHERE s.p >= 0 RETURN r,s,c";
    let mut cursor = AsyncEdgeJoinCursor::new(source(6), optional_plan(text), policy(), ok);
    let mut rows = Vec::new();
    while let Some(row) = run(cursor.next()) {
        rows.push(row.unwrap().values().to_vec());
    }
    assert_eq!(
        rows,
        vec![
            vec![
                GraphValue::Edge(EId(1)),
                GraphValue::Edge(EId(2)),
                GraphValue::Vertex(IDS[1])
            ],
            vec![
                GraphValue::Edge(EId(2)),
                GraphValue::Edge(EId(2)),
                GraphValue::Vertex(IDS[1])
            ],
        ]
    );
    let text = "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) RETURN r,a,b,s,c,s.p AS value";
    let mut cursor = AsyncEdgeJoinCursor::new(source(6), optional_plan(text), policy(), ok);
    for id in [1, 2] {
        let row = run(cursor.next()).unwrap().unwrap();
        assert_eq!(
            row.values(),
            &[
                GraphValue::Edge(EId(id)),
                GraphValue::Vertex(IDS[0]),
                GraphValue::Vertex(IDS[1]),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]
        );
    }
    assert!(run(cursor.next()).is_none());
}

#[test]
fn optional_invisible_candidates_do_not_supply_witnesses_and_empty_outer_skips_intake() {
    let text = "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (a)-[s:R]->(c) \
                WHERE s.p >= 0 RETURN r,s,c";
    let mut input = source(6);
    input.hidden = Some(EId(2));
    let counts = input.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(input, optional_plan(text), policy(), ok);
    let row = run(cursor.next()).unwrap().unwrap();
    assert_eq!(
        row.values(),
        &[
            GraphValue::Edge(EId(1)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Null),
        ]
    );
    drop(row);
    assert!(run(cursor.next()).is_none());
    assert!(cursor.row_stats().snapshot_records >= 4);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    let mut input = source(0);
    input.suspend = true;
    input.unavailable = true;
    let counts = input.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(input, optional_plan(text), policy(), ok);
    assert!(run(cursor.next()).is_none());
    assert_eq!(counts.seeks.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
}

#[test]
fn optional_multihop_paths_preserve_captures_repetition_rules_and_no_match() {
    for text in [
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH p=(b)-[s]->(c)-[t]->(d) RETURN r,p,s,t,d",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c)-[t:R]->(b) RETURN r,s,t,c",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH REPEATABLE ELEMENTS (b)-[s:R]->(c)-[t:R]->(b) RETURN r,s,t,c",
        "MATCH root=(a)-[r:R]->(b) OPTIONAL MATCH p=(b)-[s:R]->(c) RETURN root,p,c",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (a) WHERE a.p > 5 RETURN r,a",
    ] {
        let input = source(63);
        let expected = eager(text, &input);
        let counts = input.counts.clone();
        let mut cursor = AsyncEdgeJoinCursor::new(input, optional_plan(text), policy(), ok);
        let mut rows = Vec::new();
        while let Some(row) = run(cursor.next()) {
            rows.push(row.unwrap().values().to_vec());
        }
        rows.sort();
        assert_eq!(rows, expected, "{text}");
        assert!(counts.peak.load(Ordering::SeqCst) <= 3);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn optional_refusals_never_become_absence_and_limit_zero_keeps_body_failures() {
    let text = "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) RETURN r,s,c LIMIT 0";
    for mode in 0..4 {
        let mut input = source(6);
        match mode {
            0 => input.unavailable = true,
            1 => input.fail_seek = Some(0),
            2 => input.admission = Admission::Missing,
            _ => input.fail_reserve = true,
        }
        // The malformed candidate case needs an actual local witness.
        if mode == 2 {
            input = source(63);
            input.admission = Admission::Missing;
        }
        let counts = input.counts.clone();
        let mut cursor = AsyncEdgeJoinCursor::new(input, optional_plan(text), policy(), ok);
        assert!(run(cursor.next()).unwrap().is_err());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    for limits in [
        GqlQueryPolicy::new(0, 100, 100_000, 100_000),
        GqlQueryPolicy::new(100, 0, 100_000, 100_000),
        GqlQueryPolicy::new(100, 100, 0, 100_000),
        GqlQueryPolicy::new(100, 100, 100_000, 0),
    ] {
        let input = source(6);
        let counts = input.counts.clone();
        let mut cursor = AsyncEdgeJoinCursor::new(input, optional_plan(text), limits, ok);
        assert!(run(cursor.next()).unwrap().is_err());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn pending_optional_pull_drops_its_whole_private_traversal() {
    let text = "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) RETURN r,s";
    let mut input = source(6);
    input.suspend = true;
    let counts = input.counts.clone();
    let mut cursor = AsyncEdgeJoinCursor::new(input, optional_plan(text), policy(), ok);
    let mut pull = Box::pin(cursor.next());
    let mut context = Context::from_waker(Waker::noop());
    assert!(pull.as_mut().poll(&mut context).is_pending());
    assert_eq!(counts.records.load(Ordering::SeqCst), 1);
    assert!(counts.guards.load(Ordering::SeqCst) >= 2);
    drop(pull);
    assert_eq!(counts.records.load(Ordering::SeqCst), 0);
    assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    assert_eq!(cursor.state(), EdgeScanState::Failed);
    assert!(run(cursor.next()).is_none());
}

#[test]
fn optional_cancellation_at_each_checkpoint_keeps_only_completed_rows() {
    let text = "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (a)-[s:R]->(c) RETURN r,s,c";
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let observed = checkpoints.clone();
    let mut cursor =
        AsyncEdgeJoinCursor::new(source(6), optional_plan(text), policy(), move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok::<_, usize>(())
        });
    let mut complete = Vec::new();
    while let Some(row) = run(cursor.next()) {
        complete.push(row.unwrap().values().to_vec());
    }
    let total = checkpoints.load(Ordering::SeqCst);
    assert_eq!(complete.len(), 4);
    for stop in 0..total {
        let input = source(6);
        let counts = input.counts.clone();
        let mut checkpoint = 0;
        let mut cursor =
            AsyncEdgeJoinCursor::new(input, optional_plan(text), policy(), move || {
                let current = checkpoint;
                checkpoint += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            });
        let mut rows = Vec::new();
        let mut refused = false;
        while let Some(row) = run(cursor.next()) {
            match row {
                Ok(row) => rows.push(row.values().to_vec()),
                Err(GqlQueryError::Interrupted(actual)) => {
                    assert_eq!(actual, stop);
                    refused = true;
                    break;
                }
                Err(error) => panic!("unexpected refusal at {stop}: {error:?}"),
            }
        }
        assert!(refused, "checkpoint {stop} of {total}");
        assert_eq!(rows, complete[..rows.len()]);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn unsupported_optional_scopes_refuse_before_any_source_is_opened() {
    for text in [
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (c)-[s:R]->(d) RETURN r,s LIMIT 0",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[:R*1..2]->(c) RETURN r,c LIMIT 0",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) OPTIONAL MATCH (c)-[t:R]->(d) RETURN r,s,t LIMIT 0",
        "MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) MATCH (c)-[t:R]->(d) RETURN r,s,t LIMIT 0",
    ] {
        let definition = prepared(text);
        assert!(
            AsyncEdgeJoinPlan::compile_sort_input(definition.plan()).is_err(),
            "{text}"
        );
    }
}

#[test]
fn optional_compilation_never_erases_a_middle_instruction_or_a_second_projection() {
    let definition =
        prepared("MATCH (a)-[r:R]->(b) OPTIONAL MATCH (b)-[s:R]->(c) RETURN r,s LIMIT 0");
    let original = definition.plan().operators();
    let end = original
        .iter()
        .position(|op| matches!(op, GlaOperator::OptionalEnd { .. }))
        .unwrap();
    for middle in [
        GlaOperator::Select {
            slot: crate::algebra::BindingSlot(0),
            predicates: Vec::new(),
        },
        GlaOperator::Limit {
            offset: 1,
            count: Some(0),
        },
        original[end + 1].clone(),
    ] {
        let mut operators = original.to_vec();
        operators.insert(end + 2, middle);
        operators.insert(end + 3, original[end + 1].clone());
        let malformed = GlaPlan::from_operators(operators);
        assert!(
            crate::scan_stream::ScanSortTail::compile(&malformed).is_ok(),
            "the valid terminal tail alone is insufficient"
        );
        assert!(AsyncEdgeJoinPlan::compile_sort_input(&malformed).is_err());
    }
}
