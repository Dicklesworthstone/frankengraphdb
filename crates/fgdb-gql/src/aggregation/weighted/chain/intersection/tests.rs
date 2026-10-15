use super::*;
use crate::{
    GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphText,
};

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match kind {
        GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(match name {
            "R" => 1,
            "S" => 2,
            "T" => 3,
            "U" => 4,
            _ => 5,
        }))),
        GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
        GraphSymbolKind::Label => Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1))),
    }
}
/// These kernels read an identity-free topology (a multiplicity per
/// endpoint triple), so every pattern their laws prepare counts
/// homomorphisms: REPEATABLE ELEMENTS, the semantics they were written
/// for. Under the DIFFERENT EDGES default, atoms of one type would need edge
/// identities this input does not have (fgdb-vexfr).
fn repeatable(text: &str) -> String {
    text.replacen("MATCH", "MATCH REPEATABLE ELEMENTS", 1)
}
pub(super) fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(&repeatable(text), resolve)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
pub(super) fn aggregate(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(&repeatable(text), resolve)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
pub(super) fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

fn projected(columns: &[ValueProjection], bindings: &[Option<VId>]) -> Vec<VId> {
    columns
        .iter()
        .map(|column| {
            let ValueProjection::Vertex { slot } = column else {
                panic!("vertex fixture");
            };
            bindings[slot.ordinal() as usize].unwrap()
        })
        .collect()
}
fn collect(plan: &GlaPlan<GraphValueRow>, topology: &Topology, optimized: bool) -> Vec<Vec<VId>> {
    let mut result = Vec::new();
    if optimized {
        visit_bindings(
            plan,
            [],
            topology,
            |_, _| Ok::<_, VisitError<(), ()>>(true),
            |_, _| Ok(None),
            |_| Ok(()),
            |columns, bindings, _, _| {
                result.push(projected(columns, bindings));
                Ok(())
            },
        )
        .unwrap();
    } else {
        plan.visit_value_bindings(
            [],
            topology.keys().copied(),
            |_, _| Ok::<_, VisitError<(), ()>>(true),
            |_, _| Ok(None),
            |_| Ok(()),
            |columns, bindings, _, _| {
                result.push(projected(columns, bindings));
                Ok(())
            },
        )
        .unwrap();
    }
    result.sort();
    result
}

#[test]
fn triangle_intersections_match_independent_assignments_and_the_original_visitor() {
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        let arrow = match direction {
            GlaDirection::Forward => "-[:R]->",
            GlaDirection::Reverse => "<-[:R]-",
            _ => "-[:R]-",
        };
        for unequal in [false, true] {
            let q = pattern(&format!(
                "MATCH (a){arrow}(b){arrow}(c){arrow}(a) {} RETURN a,b,c",
                if unequal {
                    "WHERE a<>b AND b<>c AND a<>c"
                } else {
                    ""
                }
            ));
            assert!(Shape::compile(q.plan()).is_some());
            for mask in 0..512 {
                let mut topology = BTreeMap::new();
                for left in 0..3 {
                    for right in 0..3 {
                        if mask & (1 << (3 * left + right)) != 0 {
                            topology.insert(
                                normalized(
                                    VId(left),
                                    RelationId(1),
                                    VId(right),
                                    direction == GlaDirection::Undirected,
                                ),
                                Multiplicity::ONE,
                            );
                        }
                    }
                }
                let topology = Topology::from(topology);
                let present = |from, to| {
                    topology.contains_key(&match direction {
                        GlaDirection::Forward => (VId(from), RelationId(1), VId(to)),
                        GlaDirection::Reverse => (VId(to), RelationId(1), VId(from)),
                        _ => normalized(VId(from), RelationId(1), VId(to), true),
                    })
                };
                // Enumerate vertex assignments, not trie candidates or GLA operators.
                let mut expected = Vec::new();
                for a in 0..3 {
                    for b in 0..3 {
                        for c in 0..3 {
                            if (!unequal || (a != b && b != c && a != c))
                                && present(a, b)
                                && present(b, c)
                                && present(c, a)
                            {
                                expected.push(vec![VId(a), VId(b), VId(c)]);
                            }
                        }
                    }
                }
                assert_eq!(collect(q.plan(), &topology, true), expected);
                assert_eq!(collect(q.plan(), &topology, false), expected);
            }
        }
    }
}

#[test]
fn cliques_parallel_constraints_loops_aliases_and_reversed_atoms_share_one_binding_assignment() {
    let topology: Topology = [0, 1, 1_u128 << 100, u128::MAX]
        .into_iter()
        .flat_map(|a| {
            [0, 1, 1_u128 << 100, u128::MAX]
                .into_iter()
                .flat_map(move |b| {
                    (1..=5)
                        .filter(move |relation| (a ^ b ^ u128::from(*relation)) & 3 != 0)
                        .map(move |relation| {
                            ((VId(a), RelationId(relation), VId(b)), Multiplicity::ONE)
                        })
                })
        })
        .collect();
    for input in [
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(a)-[:U]->(d)-[:S]->(c),(b)-[:T]->(d) RETURN a,b,c,d",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(a)-[:R]->(b),(c)-[:U]->(c) RETURN c,a,b",
        "MATCH (a)<-[:R]-(b)-[:S]->(c)<-[:T]-(a) WHERE a<>c RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE a=a AND a<>a RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a),(b)-[:U]->(d) RETURN a,b,c,d",
    ] {
        let q = pattern(input);
        assert!(Shape::compile(q.plan()).is_some(), "{input}");
        assert_eq!(
            collect(q.plan(), &topology, true),
            collect(q.plan(), &topology, false),
            "{input}"
        );
    }
}

#[test]
fn only_pure_cyclic_support_uses_intersection_and_fallback_keeps_its_event_trace() {
    for input in [
        "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(a) RETURN a,b",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN DISTINCT a,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.p=1 RETURN a,b,c",
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN a,b,c LIMIT 0",
    ] {
        assert!(Shape::compile(pattern(input).plan()).is_none(), "{input}");
    }
    let q = pattern("MATCH (a)-[:R]->(b)-[:R]->(c) RETURN a,b,c");
    let topology = Topology::from(BTreeMap::from([(
        (VId(0), RelationId(1), VId(1)),
        Multiplicity::ONE,
    )]));
    let mut direct_events = Vec::new();
    q.plan()
        .visit_value_bindings(
            [],
            topology.keys().copied(),
            |_, _| Ok::<_, VisitError<(), ()>>(true),
            |_, _| Ok(None),
            |event| {
                direct_events.push(event);
                Ok(())
            },
            |_, _, _, _| Ok(()),
        )
        .unwrap();
    let mut events = Vec::new();
    visit_bindings(
        q.plan(),
        [],
        &topology,
        |_, _| Ok::<_, VisitError<(), ()>>(true),
        |_, _| Ok(None),
        |event| {
            events.push(event);
            Ok(())
        },
        |_, _, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!(events, direct_events);
}

#[test]
fn weighted_counts_groups_and_support_summaries_equal_unfactored_bags() {
    let mut edges = Vec::new();
    for a in 0..3 {
        for b in 0..3 {
            for relation in 1..=3 {
                for _ in 0..((a + 2 * b + u128::from(relation)) % 3) {
                    edges.push((VId(a), RelationId(relation), VId(b)));
                }
            }
        }
    }
    for body in [
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a)",
        "(a)<-[:R]-(b)-[:S]-(c)-[:T]->(a)",
        "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a) WHERE a<>c",
    ] {
        let prefix = format!(
            "MATCH {body} RETURN a,b,c,COUNT(*) AS n,COUNT(c) AS m,COUNT(DISTINCT c) AS d,MIN(c) AS lo,MAX(c) AS hi"
        );
        let q = aggregate(&format!("{prefix} GROUP BY a,b,c"));
        let ordinary = aggregate(&format!("{prefix},COLLECT(c) AS ignored GROUP BY a,b,c"))
            .with_aggregate_output_prefix(5)
            .unwrap();
        let run = |q: &PreparedGraphAggregate| {
            q.execute_governed(
                edges.len() as u64,
                (0..3).map(VId),
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
        };
        assert_eq!(run(&q).value, run(&ordinary).value);
    }
}

/// A 30-vertex, three-relation graph whose triangle count takes thousands
/// of work units, so evaluator runs open and close many times.
fn run_graph() -> Vec<(VId, RelationId, VId)> {
    (0..30_u128)
        .flat_map(|a| {
            (0..30_u128).flat_map(move |b| {
                (1..=3_u64)
                    .filter(move |r| (a * 7 + b * 3 + u128::from(*r)) % 4 == 0)
                    .map(move |r| (VId(a), RelationId(r), VId(b)))
            })
        })
        .collect()
}

/// **AN EVALUATOR RUN CHARGES THE PER-EVENT TOTAL IN FEWER HOST CALLS.** A
/// triangle count over a 30-vertex, three-relation graph returns the same
/// value and statistics with a checkpoint that takes a run in one call as
/// with one that is called per unit; the host sees exactly the per-unit
/// total, in far fewer calls.
#[test]
fn evaluator_runs_charge_the_per_event_total_in_fewer_host_calls() {
    use crate::GovernedCheckpoint;
    use std::cell::Cell;
    struct Bulk<'c> {
        units: &'c Cell<u64>,
        calls: &'c Cell<u64>,
    }
    impl GovernedCheckpoint<()> for Bulk<'_> {
        const RUNS: bool = true;
        fn unit(&mut self) -> Result<(), ()> {
            self.units(1)
        }
        fn units(&mut self, n: u64) -> Result<(), ()> {
            self.units.set(self.units.get() + n);
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }
    }
    let q = aggregate("MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN COUNT(*) AS n");
    let edges = run_graph();
    let mut per_unit = 0_u64;
    let expected = q
        .execute_governed(
            edges.len() as u64,
            (0..30).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                per_unit += 1;
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    let (units, calls) = (Cell::new(0), Cell::new(0));
    let bulk = q
        .execute_governed(
            edges.len() as u64,
            (0..30).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            Bulk {
                units: &units,
                calls: &calls,
            },
        )
        .unwrap();
    assert_eq!(bulk, expected);
    assert_eq!(units.get(), per_unit, "the per-unit total");
    assert!(
        calls.get() * 100 < units.get(),
        "{} calls for {} units",
        calls.get(),
        units.get()
    );
}

/// **AN EVALUATOR RUN NEVER MOVES A REFUSAL.** Over the same graph, a
/// grouped and an ungrouped triangle count refuse at every sampled work,
/// scratch and host limit exactly as per-event charging does: the evaluator
/// limit's own unit observes limit + 1, and a host that charges a run in one
/// call (refusing inside it at the exceeding unit, as fgdb's signed meter
/// does) refuses at the same host ordinal as one called per unit.
#[test]
fn evaluator_runs_refuse_at_the_per_event_ordinal() {
    use crate::{GlaLimitDimension, GlaLimitExceeded, GovernedCheckpoint};
    /// Refuses at unit `budget + 1`, inside a bulk call as fgdb's signed
    /// meter does, and is never to be called again after refusing.
    struct Budget<'u> {
        used: &'u mut u64,
        budget: u64,
        refused: bool,
    }
    impl GovernedCheckpoint<u64> for Budget<'_> {
        const RUNS: bool = true;
        fn unit(&mut self) -> Result<(), u64> {
            self.units(1)
        }
        fn units(&mut self, n: u64) -> Result<(), u64> {
            assert!(!self.refused, "called after refusing");
            if self.budget - *self.used < n {
                *self.used = self.budget;
                self.refused = true;
                return Err(self.budget + 1);
            }
            *self.used += n;
            Ok(())
        }
    }
    let edges = run_graph();
    for text in [
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN COUNT(*) AS n",
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN a, COUNT(*) AS n GROUP BY a",
    ] {
        let q = aggregate(text);
        let run = |policy, checkpoint: &mut dyn FnMut() -> Result<(), u64>| {
            q.execute_governed(
                edges.len() as u64,
                (0..30).map(VId),
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                policy,
                checkpoint,
            )
        };
        let mut host = 0_u64;
        let baseline = run(wide(), &mut || {
            host += 1;
            Ok(())
        })
        .unwrap();
        let (work, scratch) = (
            baseline.evaluator.work_units,
            baseline.evaluator.scratch_entries,
        );
        assert!(work > 4 * 1024, "{work} units must span several runs");
        let sampled = |total: u64| (0..total).filter(move |at| at % 13 == 0 || total - at <= 40);
        for limit in sampled(work) {
            let refused = run(
                GqlQueryPolicy::new(u64::MAX, u64::MAX, limit, u64::MAX),
                &mut || Ok(()),
            );
            assert!(
                matches!(refused, Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension: GlaLimitDimension::WorkUnits,
                    limit: found,
                    observed,
                })) if found == limit && observed == u128::from(limit) + 1),
                "work limit {limit}"
            );
        }
        for limit in sampled(scratch) {
            let refused = run(
                GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, limit),
                &mut || Ok(()),
            );
            assert!(
                matches!(refused, Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension: GlaLimitDimension::ScratchEntries,
                    limit: found,
                    observed,
                })) if found == limit && observed == u128::from(limit) + 1),
                "scratch limit {limit}"
            );
        }
        for budget in sampled(host) {
            let mut used = 0;
            let refused = q.execute_governed(
                edges.len() as u64,
                (0..30).map(VId),
                edges.iter().copied(),
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                Budget {
                    used: &mut used,
                    budget,
                    refused: false,
                },
            );
            assert!(
                matches!(refused, Err(GqlQueryError::Interrupted(at)) if at == budget + 1),
                "host budget {budget}"
            );
            assert_eq!(used, budget);
        }
        let mut used = 0;
        let accepted = q.execute_governed(
            edges.len() as u64,
            (0..30).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            Budget {
                used: &mut used,
                budget: host,
                refused: false,
            },
        );
        assert_eq!(accepted.unwrap(), baseline);
        assert_eq!(used, host);
    }
}

#[test]
fn every_trie_build_seek_and_complete_binding_refusal_retries_without_partial_aggregate() {
    let q = aggregate(
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c",
    );
    let edges: Vec<_> = (0..3)
        .flat_map(|a| {
            (0..3).flat_map(move |b| (1..=3).map(move |r| (VId(a), RelationId(r), VId(b))))
        })
        .collect();
    let mut total = 0;
    let baseline = q
        .execute_governed(
            edges.len() as u64,
            (0..3).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = q.execute_governed(
            edges.len() as u64,
            (0..3).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop));
        assert_eq!(at, stop);
    }
    let exact = GqlQueryPolicy::new(
        baseline.rows.snapshot_records,
        baseline.rows.result_rows,
        baseline.evaluator.work_units,
        baseline.evaluator.scratch_entries,
    );
    let run = |policy| {
        q.execute_governed(
            edges.len() as u64,
            (0..3).map(VId),
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy,
            || Ok::<_, usize>(()),
        )
    };
    assert_eq!(run(exact).unwrap(), baseline);
    assert!(matches!(
        run(GqlQueryPolicy::new(
            27,
            27,
            exact.evaluator.max_work_units - 1,
            u64::MAX
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
    assert!(matches!(
        run(GqlQueryPolicy::new(
            27,
            27,
            u64::MAX,
            exact.evaluator.max_scratch_entries - 1
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
}

#[test]
fn sparse_cycle_closure_intersects_before_a_quadratic_wedge_is_generated() {
    let n = 1024_u128;
    let mut edges = Vec::new();
    for i in 0..n {
        edges.push((VId(i), RelationId(1), VId(n)));
        edges.push((VId(n), RelationId(2), VId(n + 1 + i)));
        edges.push((VId(n + 1 + i), RelationId(3), VId(i)));
    }
    let q = aggregate(
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:T]->(a) RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c",
    );
    assert!(Shape::compile(q.input_pattern().plan()).is_some());
    let result = q
        .execute_governed(
            edges.len() as u64,
            (0..=2 * n).map(VId),
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            GqlQueryPolicy::new(3 * n as u64, n as u64, 1_000_000, 300_000),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), n as usize);
    assert!(result.evaluator.work_units < n as u64 * n as u64);
}

#[test]
fn support_summaries_do_not_expand_or_reject_an_above_count_parallel_product() {
    let body = "(a)-[:R]->(b)-[:S]->(c)-[:T]->(a)".to_owned() + &",(a)-[:R]->(b)".repeat(5);
    // Eight independent factors of 256 give 2^64 edge-identified occurrences,
    // but only one complete vertex assignment. Its support remains meaningful.
    let edges: Vec<_> = [
        (VId(0), RelationId(1), VId(1)),
        (VId(1), RelationId(2), VId(2)),
        (VId(2), RelationId(3), VId(0)),
    ]
    .into_iter()
    .flat_map(|edge| std::iter::repeat_n(edge, 256))
    .collect();
    let support = aggregate(&format!(
        "MATCH {body} RETURN a,b,c,COUNT(DISTINCT c) AS n GROUP BY a,b,c"
    ));
    let count = aggregate(&format!(
        "MATCH {body} RETURN a,b,c,COUNT(*) AS n GROUP BY a,b,c"
    ));
    let run = |q: &PreparedGraphAggregate| {
        q.execute_governed(
            768,
            [VId(0), VId(1), VId(2)],
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            GqlQueryPolicy::new(768, 1, 100_000, 100_000),
            || Ok::<_, ()>(()),
        )
    };
    let rows = run(&support).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(1));
    assert!(matches!(run(&count), Err(GqlQueryError::Source(_))));
}

/// The tree-of-trees build the flat trie replaced: the reference for every
/// charge it makes and every row it holds.
fn tree_build(
    atom: Atom,
    topology: &Topology,
    events: &mut Vec<GlaExecutionEvent>,
) -> BTreeMap<VId, Vec<VId>> {
    let mut rows: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
    for &(left, relation, right) in topology.keys() {
        events.push(GlaExecutionEvent::Work);
        if relation != atom.relation {
            continue;
        }
        let orientations = match atom.direction {
            GlaDirection::Forward => [(left, right), (left, right)],
            GlaDirection::Reverse => [(right, left), (right, left)],
            GlaDirection::Undirected => [(left, right), (right, left)],
        };
        let count = if atom.direction == GlaDirection::Undirected && left != right {
            2
        } else {
            1
        };
        for &(from, to) in orientations.iter().take(count) {
            events.push(GlaExecutionEvent::Work);
            if atom.left == atom.right && from != to {
                continue;
            }
            let (low, high) = if atom.left <= atom.right {
                (from, to)
            } else {
                (to, from)
            };
            if !rows.contains_key(&low) {
                events.push(GlaExecutionEvent::ScratchEntry);
            }
            if !rows.get(&low).is_some_and(|row| row.contains(&high)) {
                events.push(GlaExecutionEvent::ScratchEntry);
                rows.entry(low).or_default().insert(high);
            }
        }
    }
    let mut children = BTreeMap::new();
    for (low, values) in rows {
        events.extend([
            GlaExecutionEvent::Work,
            GlaExecutionEvent::ScratchEntry,
            GlaExecutionEvent::ScratchEntry,
        ]);
        let mut child = Vec::new();
        for value in values {
            events.extend([GlaExecutionEvent::Work, GlaExecutionEvent::ScratchEntry]);
            child.push(value);
        }
        children.insert(low, child);
    }
    children
}

#[test]
fn flat_tries_charge_and_hold_what_the_tree_build_did() {
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    for round in 0..12 {
        // Two relations over six vertices, loops and both orientations of a
        // pair included; keys are unique as in any topology table.
        let topology: Topology = (0..8 + 4 * round)
            .map(|_| {
                let key = (
                    VId(u128::from(next(6))),
                    RelationId(1 + next(2)),
                    VId(u128::from(next(6))),
                );
                (key, Multiplicity::ONE)
            })
            .collect();
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for (left, right) in [(0, 1), (1, 0), (1, 1)] {
                let atom = Atom {
                    left,
                    right,
                    relation: RelationId(1),
                    direction,
                };
                let mut expected = Vec::new();
                let rows = tree_build(atom, &topology, &mut expected);
                let mut events = Vec::new();
                let trie = Trie::build(atom, &topology, &mut |event| {
                    events.push(event);
                    Ok::<_, ()>(())
                })
                .unwrap();
                assert_eq!(events, expected, "{round} {direction:?} {left} {right}");
                assert!(trie.roots.iter().eq(rows.keys()));
                // A loop atom's one variable ranges over its roots alone.
                if trie.low != trie.high {
                    let mut assigned = [None; MAX_PATTERN_VERTICES];
                    for (low, children) in &rows {
                        assigned[trie.low] = Some(*low);
                        assert_eq!(trie.domain(trie.high, &assigned), Some(&children[..]));
                    }
                    // An absent row is an empty domain, as the tree's was.
                    assigned[trie.low] = Some(VId(99));
                    assert_eq!(trie.domain(trie.high, &assigned), Some(&[][..]));
                }
                // A refusal at any charge stops the build there.
                for stop in 1..=expected.len() {
                    let mut at = 0;
                    let result = Trie::build(atom, &topology, &mut |_| {
                        at += 1;
                        if at == stop { Err(at) } else { Ok(()) }
                    });
                    assert!(matches!(result, Err(found) if found == stop));
                    assert_eq!(at, stop);
                }
            }
        }
    }
}
