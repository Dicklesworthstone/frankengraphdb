use super::*;
use crate::algebra::{
    GlaIdentityOutput, GlaPlan, GraphPatternBuilder, IntegerComparison, VertexPredicate,
};
use core::convert::Infallible;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::CanonicalScalar;

fn allow(_: GlaExecutionEvent) -> Result<(), Infallible> {
    Ok(())
}

#[test]
fn smallest_membership_drives_seeks_without_reweighting_the_primary_bag() {
    let primary = [VId(0), VId(1), VId(1), VId(3)];
    let broad = [VId(0), VId(1), VId(1), VId(2), VId(3), VId(3)];
    let sparse = [VId(1), VId(1)];
    let other = [VId(1), VId(2), VId(3)];
    let mut cursor = Candidates::all(&primary);
    cursor.membership = Some(&broad);
    cursor.add_membership(&sparse, &mut allow).unwrap();
    cursor.add_membership(&other, &mut allow).unwrap();
    assert_eq!(cursor.membership, Some(sparse.as_slice()));
    let mut actual = Vec::new();
    while let Some(value) = cursor.next(&mut allow).unwrap() {
        actual.push(value);
    }
    // Membership duplicates do not multiply output. Logical expansions still
    // provide their real edge occurrences through the unchanged continuation.
    assert_eq!(actual, vec![VId(1), VId(1)]);
}

#[test]
fn refused_descriptor_does_not_change_the_chosen_membership() {
    let primary = [VId(1)];
    let broad = [VId(1), VId(2)];
    let sparse = [VId(1)];
    for refusal in 0..2 {
        let mut cursor = Candidates::all(&primary);
        cursor.membership = Some(&broad);
        let mut visited = 0;
        let result = cursor.add_membership(&sparse, &mut |_| {
            let at = visited;
            visited += 1;
            if at == refusal { Err("stop") } else { Ok(()) }
        });
        assert_eq!(result, Err("stop"));
        assert_eq!(visited, refusal + 1);
        assert_eq!(cursor.membership, Some(broad.as_slice()));
        assert!(cursor.additional.is_empty());
    }
}

#[test]
fn an_empty_smallest_domain_exhausts_without_visiting_the_primary() {
    let primary = [VId(0), VId(u128::MAX)];
    let broad = [VId(0), VId(u128::MAX)];
    let mut cursor = Candidates::all(&primary);
    cursor.membership = Some(&broad);
    cursor.add_membership(&[], &mut allow).unwrap();
    let mut visits = 0;
    assert_eq!(
        cursor
            .next(&mut |_| {
                visits += 1;
                Ok::<_, Infallible>(())
            })
            .unwrap(),
        None
    );
    assert_eq!(visits, 0);
}

#[test]
fn constructor_reordering_matches_every_small_bag_and_membership_permutation() {
    let choices = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
    let mut arrays = vec![Vec::new()];
    for (at, &left) in choices.iter().enumerate() {
        arrays.push(vec![left]);
        for &right in &choices[at..] {
            arrays.push(vec![left, right]);
        }
    }
    for primary in &arrays {
        for first in &arrays {
            for second in &arrays {
                for third in &arrays {
                    let expected: Vec<_> = primary
                        .iter()
                        .copied()
                        .filter(|value| {
                            first.contains(value) && second.contains(value) && third.contains(value)
                        })
                        .collect();
                    let mut cursor = Candidates::all(primary);
                    cursor.membership = Some(first);
                    cursor.add_membership(second, &mut allow).unwrap();
                    cursor.add_membership(third, &mut allow).unwrap();
                    assert_eq!(
                        cursor.membership.unwrap().len(),
                        first.len().min(second.len()).min(third.len())
                    );
                    let mut actual = Vec::new();
                    while let Some(value) = cursor.next(&mut allow).unwrap() {
                        actual.push(value);
                    }
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}

fn filtered_cycle(directions: [GlaDirection; 4]) -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::new();
    for name in ["a", "b", "c"] {
        builder.vertex(name).unwrap();
    }
    for (name, label) in [("a", 1), ("b", 2), ("c", 3)] {
        builder
            .filter(name, VertexPredicate::HasLabel(LabelId(label)))
            .unwrap();
    }
    builder
        .filter(
            "c",
            VertexPredicate::IntegerProperty {
                key: PropertyKeyId(1),
                comparison: IntegerComparison::NotEqual,
                value: 4,
            },
        )
        .unwrap();
    for (source, relation, destination, direction) in [
        ("a", 1, "b", directions[0]),
        ("b", 2, "c", directions[1]),
        ("c", 3, "a", directions[2]),
        ("a", 4, "c", directions[3]),
    ] {
        builder
            .edge(source, RelationId(relation), direction, destination)
            .unwrap();
    }
    builder
}

fn filtered_edges(
    directions: [GlaDirection; 4],
    first_copies: usize,
    second_copies: usize,
) -> Vec<(VId, RelationId, VId)> {
    let mut edges = Vec::new();
    let mut push = |source, relation: u64, destination| {
        let (source, destination) = if directions[(relation - 1) as usize] == GlaDirection::Reverse
        {
            (destination, source)
        } else {
            (source, destination)
        };
        edges.push((VId(source), RelationId(relation), VId(destination)));
    };
    push(1, 1, 2);
    // Repeated prefixes must still visit their predicates through the same
    // cache. A filtered-out complete cycle and an accepted dead end are mixed
    // with the match to expose predicate and missing-edge ordering mistakes.
    for candidate in [3, 3, 4, 5] {
        push(2, 2, candidate);
    }
    for candidate in [3, 4, 5] {
        for _ in 0..first_copies {
            push(candidate, 3, 1);
        }
        if candidate != 5 {
            for _ in 0..second_copies {
                push(1, 4, candidate);
            }
        }
    }
    edges
}

fn visible(vid: VId, predicates: &[VertexPredicate]) -> bool {
    let label = match vid.0 {
        1 => LabelId(1),
        2 => LabelId(2),
        _ => LabelId(3),
    };
    predicates.iter().all(|predicate| {
        predicate.matches(
            &[label],
            &[(PropertyKeyId(1), CanonicalScalar::Int(vid.0 as i64))],
        )
    })
}

struct Observed<Row> {
    result: Result<Vec<Row>, &'static str>,
    work: u64,
    scratch: u64,
    reads: Vec<(VId, Vec<VertexPredicate>)>,
    events: usize,
}

/// Both paths run the live evaluator and collector in this invocation. The
/// incumbent explicitly retains per-occurrence visitation; no copied engine,
/// stored baseline, event subtraction or independently timed setup is used.
fn observe<Row: GlaIdentityOutput>(
    plan: &GlaPlan<Row>,
    edges: &[(VId, RelationId, VId)],
    terminal: bool,
    fail_vertex: Option<VId>,
    cancel_at: Option<usize>,
) -> Observed<Row> {
    let mut work = 0;
    let mut scratch = 0;
    let mut events = 0;
    let mut reads = Vec::new();
    let project = |operator: &GlaOperator,
                   bindings: &[Option<VId>],
                   _paths: &[Option<crate::algebra::GraphPath>],
                   projected: &mut crate::algebra_exec::ProjectedRows<Row>,
                   control: &mut _| {
        Row::collect(operator, bindings, projected, control)?;
        Ok(false)
    };
    let result = plan.execute_projected(
        [],
        edges.iter().copied(),
        |vid, predicates| {
            reads.push((vid, predicates.to_vec()));
            if fail_vertex == Some(vid) {
                Err("source failure")
            } else {
                Ok(visible(vid, predicates))
            }
        },
        |event| {
            events += 1;
            if cancel_at == Some(events) {
                return Err("cancelled");
            }
            match event {
                GlaExecutionEvent::Work => work += 1,
                GlaExecutionEvent::ScratchEntry => scratch += 1,
                GlaExecutionEvent::ResultRow => {}
            }
            Ok(())
        },
        if terminal {
            crate::algebra_exec::Projection::Terminal(project)
        } else {
            crate::algebra_exec::Projection::Visitor(project)
        },
    );
    Observed {
        result,
        work,
        scratch,
        reads,
        events,
    }
}

#[test]
fn filtered_terminal_closures_preserve_oriented_bags_distinct_and_pages() {
    let choices = [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ];
    for encoded in 0..81 {
        let mut encoded = encoded;
        let directions = core::array::from_fn(|_| {
            let direction = choices[encoded % 3];
            encoded /= 3;
            direction
        });
        for (first, second) in [(0, 3), (2, 0), (1, 1), (2, 3)] {
            let builder = filtered_cycle(directions);
            let edges = filtered_edges(directions, first, second);
            for distinct in [false, true] {
                for (offset, count) in [(0, None), (1, Some(4)), (0, Some(0)), (u64::MAX, Some(1))]
                {
                    let mut prepared = builder
                        .prepare_bindings(&["c", "a", "b"], offset, count)
                        .unwrap();
                    if !distinct {
                        prepared = prepared.with_duplicates();
                    }
                    let plan = prepared.plan();
                    let candidate = observe(plan, &edges, true, None, None);
                    let incumbent = observe(plan, &edges, false, None, None);
                    assert_eq!(candidate.result, incumbent.result);
                    assert_eq!(candidate.reads, incumbent.reads);
                    // Independent multigraph arithmetic: two prefix edges,
                    // one accepted complete assignment, then both closing bags.
                    let occurrences = 2 * first * second;
                    let expected = if distinct {
                        usize::from(occurrences > 0)
                    } else {
                        occurrences
                    };
                    let expected = expected
                        .saturating_sub(usize::try_from(offset).unwrap_or(usize::MAX))
                        .min(
                            count
                                .and_then(|n| usize::try_from(n).ok())
                                .unwrap_or(usize::MAX),
                        );
                    let rows = candidate.result.unwrap();
                    assert_eq!(rows.len(), expected);
                    assert!(
                        rows.iter()
                            .all(|row| row.values() == [VId(3), VId(1), VId(2)])
                    );
                }
            }
        }
    }
}

#[test]
fn filtered_closing_join_work_is_compared_with_the_live_incumbent() {
    let directions = [GlaDirection::Forward; 4];
    let builder = filtered_cycle(directions);
    let edges = filtered_edges(directions, 128, 192);
    for distinct in [false, true] {
        let mut prepared = builder
            .prepare_bindings(&["a", "b", "c"], 3, Some(5))
            .unwrap();
        if !distinct {
            prepared = prepared.with_duplicates();
        }
        let incumbent = observe(prepared.plan(), &edges, false, None, None);
        let candidate = observe(prepared.plan(), &edges, true, None, None);
        assert_eq!(candidate.result, incumbent.result);
        assert_eq!(candidate.reads, incumbent.reads);
        eprintln!(
            "filtered closing joins: distinct={distinct}, incumbent_work={}, candidate_work={}, incumbent_scratch={}, candidate_scratch={}",
            incumbent.work, candidate.work, incumbent.scratch, candidate.scratch
        );
        assert!(
            candidate.work * 4 < incumbent.work,
            "work: candidate={}, incumbent={}",
            candidate.work,
            incumbent.work
        );
        // The incumbent does not charge fixed interpreter slots. Factoring
        // explicitly reserves each temporary alias: at most closure width per
        // accepted prefix, independent of the parallel-edge product. This is
        // accounting overhead, not a claim of reduced memory consumption.
        let closure = TerminalClosure::compile(prepared.plan().operators()).unwrap();
        let width = ((closure.projection - closure.start) / 2) as u64;
        let prefixes = edges
            .iter()
            .filter(|(_, relation, target)| *relation == RelationId(2) && *target != VId(4))
            .count() as u64;
        assert!(candidate.scratch <= incumbent.scratch + width * prefixes);
    }
}

#[test]
fn terminal_factoring_preserves_source_errors_on_filtered_and_dead_end_prefixes() {
    let directions = [GlaDirection::Forward; 4];
    let builder = filtered_cycle(directions);
    let edges = filtered_edges(directions, 2, 3);
    for count in [Some(0), Some(2), None] {
        let prepared = builder.prepare_bindings(&["a", "c"], 0, count).unwrap();
        for fail in [VId(1), VId(2), VId(3), VId(4), VId(5)] {
            let incumbent = observe(prepared.plan(), &edges, false, Some(fail), None);
            let candidate = observe(prepared.plan(), &edges, true, Some(fail), None);
            assert_eq!(candidate.result, Err("source failure"));
            assert_eq!(candidate.result, incumbent.result);
            assert_eq!(candidate.reads, incumbent.reads);
            assert_eq!(candidate.reads.last().unwrap().0, fail);
        }
    }
}

#[test]
fn every_factored_execution_checkpoint_refuses_without_partial_rows() {
    let directions = [GlaDirection::Forward; 4];
    let builder = filtered_cycle(directions);
    let prepared = builder
        .prepare_bindings(&["c"], 1, Some(2))
        .unwrap()
        .with_duplicates();
    let edges = filtered_edges(directions, 2, 2);
    let baseline = observe(prepared.plan(), &edges, true, None, None);
    assert_eq!(baseline.result.as_ref().unwrap().len(), 2);
    for stop in 1..=baseline.events {
        let cancelled = observe(prepared.plan(), &edges, true, None, Some(stop));
        assert_eq!(cancelled.events, stop);
        assert_eq!(cancelled.result, Err("cancelled"));
    }
    let replay = observe(prepared.plan(), &edges, true, None, None);
    assert_eq!(replay.result, baseline.result);
    assert_eq!(replay.reads, baseline.reads);
    assert_eq!(replay.work, baseline.work);
}

#[test]
fn closures_do_not_elide_observable_projections_or_aggregate_visitors() {
    let directions = [GlaDirection::Forward; 4];
    let builder = filtered_cycle(directions);
    let properties = builder
        .prepare_values(
            &[crate::algebra::GraphColumn::Property {
                name: "value",
                variable: "c",
                key: PropertyKeyId(1),
            }],
            0,
            Some(0),
        )
        .unwrap();
    assert!(TerminalClosure::compile(properties.plan().operators()).is_none());
    let result = properties.plan().execute_with_properties_control(
        [],
        filtered_edges(directions, 2, 2),
        |vid, predicates| Ok::<_, &str>(visible(vid, predicates)),
        |_, _| Err("projection failed"),
        |_| Ok(()),
    );
    assert_eq!(result, Err("projection failed"));

    let unbounded = builder
        .prepare_bindings(&["c"], 0, None)
        .unwrap()
        .with_duplicates();
    assert!(TerminalClosure::compile(unbounded.plan().operators()).is_none());
    let edges = filtered_edges(directions, 2, 3);
    let incumbent = observe(unbounded.plan(), &edges, false, None, None);
    let candidate = observe(unbounded.plan(), &edges, true, None, None);
    assert_eq!(candidate.result.as_ref().unwrap().len(), 12);
    assert_eq!(candidate.result, incumbent.result);
    assert_eq!(candidate.work, incumbent.work);
    assert_eq!(candidate.scratch, incumbent.scratch);

    // The aggregate seam consumes bindings, not this terminal collector's
    // output contract. Deliberately pass an otherwise optimizable DISTINCT
    // finite-page shape: even that cannot opt a visitor into truncation.
    let values = builder
        .prepare_values(
            &[crate::algebra::GraphColumn::Vertex {
                name: "c",
                variable: "c",
            }],
            0,
            Some(1),
        )
        .unwrap();
    assert!(TerminalClosure::compile(values.plan().operators()).is_some());
    let mut count = 0;
    values
        .plan()
        .visit_value_bindings(
            [],
            edges,
            |vid, predicates| Ok::<_, Infallible>(visible(vid, predicates)),
            |_, _| Ok(None),
            allow,
            |_, _, _, _| {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(count, 12);
}

#[test]
fn native_labeled_cyclic_queries_use_the_same_governed_budget_boundaries() {
    use crate::algebra::GraphValue;
    use crate::{
        GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
        PreparedGraphText,
    };

    let prepared = PreparedGraphText::prepare(
        "MATCH (a:A)-[:R1]->(b:B)-[:R2]->(c:C)-[:R3]->(a)-[:R4]->(c) WHERE c.k <> 4 RETURN c, a, b SKIP 1 LIMIT 2",
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(LabelId(2))),
            (GraphSymbolKind::Label, "C") => Some(GraphSymbol::Label(LabelId(3))),
            (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Relation, name) => name.strip_prefix('R')?.parse().ok().map(|id| GraphSymbol::Relation(RelationId(id))),
            _ => None,
        },
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let plan = prepared.plan();
    assert!(TerminalClosure::compile(plan.operators()).is_some());
    let edges = filtered_edges([GlaDirection::Forward; 4], 3, 4);
    let records = edges.len() as u64;
    let execute = |policy| {
        plan.execute_governed_with_properties(
            records,
            [],
            edges.iter().copied(),
            |vid, predicates| Ok::<_, Infallible>(visible(vid, predicates)),
            |_, _| -> Result<Option<&CanonicalScalar>, Infallible> {
                panic!("identity projection performs no property lookup")
            },
            policy,
            || Ok::<_, Infallible>(()),
        )
    };
    let baseline = execute(GqlQueryPolicy::new(records, 2, u64::MAX, u64::MAX)).unwrap();
    assert_eq!(baseline.value.len(), 2);
    assert!(baseline.value.iter().all(|row| row.values()
        == [
            GraphValue::Vertex(VId(3)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(2))
        ]));
    let work = baseline.evaluator.work_units;
    let scratch = baseline.evaluator.scratch_entries;
    let exact = execute(GqlQueryPolicy::new(records, 2, work, scratch)).unwrap();
    assert_eq!(exact.value, baseline.value);
    assert_eq!(exact.evaluator, baseline.evaluator);
    for policy in [
        GqlQueryPolicy::new(records, 2, work - 1, scratch),
        GqlQueryPolicy::new(records, 2, work, scratch - 1),
    ] {
        assert!(matches!(execute(policy), Err(GqlQueryError::Evaluator(_))));
    }
    assert!(matches!(
        execute(GqlQueryPolicy::new(records, 1, work, scratch)),
        Err(GqlQueryError::Rows(_))
    ));
}
