//! Independent multiset semantics for fixed, connected typed graph patterns.
//! Expected multiplicities count physical edge occurrences for full assignments;
//! no GLA slot order, adjacency index, or terminal collector is shared.

use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphPatternBuilder, VertexPredicate};
use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GqlQueryPolicy};
use fgdb_types::VId;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
type Edge = (VId, RelationId, VId);

fn builder() -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::new();
    for name in ["a", "b", "c"] {
        builder.vertex(name).unwrap();
    }
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    builder.edge("b", S, GlaDirection::Forward, "c").unwrap();
    builder
}

fn parallel_edges() -> [Edge; 5] {
    [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), S, VId(3)),
        (VId(2), S, VId(3)),
        (VId(2), S, VId(3)),
    ]
}

#[test]
fn parallel_edge_products_are_bags_and_mode_is_part_of_logical_identity() {
    let builder = builder();
    let distinct = builder.prepare_bindings(&["a", "c"], 0, None).unwrap();
    let before = distinct.canonical_bytes();
    let all = distinct.clone().with_duplicates();
    assert!(!distinct.preserves_duplicates());
    assert!(all.preserves_duplicates());
    assert_eq!(all.clone().with_duplicates(), all);
    assert_eq!(all.columns(), distinct.columns());
    assert_eq!(distinct.canonical_bytes(), before);
    assert_ne!(all.canonical_bytes(), before);
    let run = |pattern: &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphBindingRow>| {
        pattern.plan().execute([], parallel_edges(), |_, _| Ok::<_, ()>(true)).unwrap()
            .into_iter().map(|row| row.values().to_vec()).collect::<Vec<_>>()
    };
    assert_eq!(run(&distinct), vec![vec![VId(1), VId(3)]]);
    assert_eq!(run(&all), vec![vec![VId(1), VId(3)]; 6]);
    let scalar = builder.prepare("c", 0, None).unwrap().with_duplicates();
    assert_eq!(scalar.plan().execute([], parallel_edges(), |_, _| Ok::<_, ()>(true)).unwrap(),
        vec![VId(3); 6]);
    assert!(!format!("{all:?}").contains("VId"));
}

fn multiplicity(
    edges: &[Edge],
    assignment: &[VId; 3],
    left: usize,
    relation: RelationId,
    direction: GlaDirection,
    right: usize,
) -> usize {
    edges.iter().filter(|&&(src, rel, dst)| {
        if rel != relation {
            return false;
        }
        let forward = src == assignment[left] && dst == assignment[right];
        let reverse = dst == assignment[left] && src == assignment[right];
        match direction {
            GlaDirection::Forward => forward,
            GlaDirection::Reverse => reverse,
            // One undirected self-loop is one occurrence, not two orientations.
            GlaDirection::Undirected => forward || reverse,
        }
    }).count()
}

fn oracle(edges: &[Edge], direction: GlaDirection, shape: usize, projection: &[usize]) -> Vec<Vec<VId>> {
    let mut rows = Vec::new();
    for bits in 0..8 {
        let assignment = std::array::from_fn(|at| VId(1 + ((bits >> at) & 1) as u128));
        if shape == 2 && assignment[0] == assignment[1] {
            continue;
        }
        let mut count = multiplicity(edges, &assignment, 0, R, direction, 1)
            * multiplicity(edges, &assignment, 1, S, GlaDirection::Reverse, 2);
        if shape >= 1 {
            count *= multiplicity(edges, &assignment, 2, R, direction, 0);
        }
        let row: Vec<_> = projection.iter().map(|&at| assignment[at]).collect();
        rows.extend(std::iter::repeat_n(row, count));
    }
    rows.sort();
    rows
}

#[test]
fn exhaustive_multigraphs_match_full_assignment_multiplicities() {
    let universe = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(1)),
        (VId(1), S, VId(2)),
        (VId(2), S, VId(1)),
        (VId(2), S, VId(2)),
    ];
    let names = ["a", "b", "c"];
    for mut encoding in 0..3_usize.pow(universe.len() as u32) {
        let mut edges = Vec::new();
        for edge in universe {
            edges.extend(std::iter::repeat_n(edge, encoding % 3));
            encoding /= 3;
        }
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            for shape in 0..3 {
                let mut b = GraphPatternBuilder::new();
                for name in names {
                    b.vertex(name).unwrap();
                }
                b.edge("a", R, direction, "b").unwrap();
                b.edge("b", S, GlaDirection::Reverse, "c").unwrap();
                if shape >= 1 {
                    b.edge("c", R, direction, "a").unwrap();
                }
                if shape == 2 {
                    b.identity("a", "b", false).unwrap();
                }
                for projection in [&[0, 2][..], &[2, 0][..], &[0][..], &[0, 1, 2][..]] {
                    let columns: Vec<_> = projection.iter().map(|&at| names[at]).collect();
                    let distinct = b.prepare_bindings(&columns, 0, None).unwrap();
                    let all = distinct.clone().with_duplicates();
                    let actual = all.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                        .unwrap().into_iter().map(|row| row.values().to_vec()).collect::<Vec<_>>();
                    let mut expected = oracle(&edges, direction, shape, projection);
                    assert_eq!(actual, expected, "shape={shape}, direction={direction:?}, columns={columns:?}");
                    expected.dedup();
                    let actual = distinct.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                        .unwrap().into_iter().map(|row| row.values().to_vec()).collect::<Vec<_>>();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}

#[test]
fn occurrence_pagination_and_all_four_policy_dimensions_are_exact() {
    let b = builder();
    let all = b.prepare_bindings(&["a", "c"], 0, None).unwrap().with_duplicates();
    let run = |policy| all.plan().execute_governed(5, [], parallel_edges(), |_, _| Ok::<_, ()>(true),
        policy, || Ok::<_, ()>(()));
    let complete = run(GqlQueryPolicy::new(5, 6, u64::MAX, u64::MAX)).unwrap();
    assert_eq!(complete.value.len(), 6);
    assert_eq!(complete.rows.result_rows, 6);
    let exact = GqlQueryPolicy::new(5, 6, complete.evaluator.work_units, complete.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), complete);
    for (policy, dimension, observed) in [
        (GqlQueryPolicy::new(4, 6, u64::MAX, u64::MAX), GqlBudgetDimension::SnapshotRecords, 5),
        (GqlQueryPolicy::new(5, 2, u64::MAX, u64::MAX), GqlBudgetDimension::ResultRows, 3),
    ] {
        assert!(matches!(run(policy), Err(GqlQueryError::Rows(error))
            if error.dimension == dimension && error.observed == observed));
    }
    for (policy, dimension) in [
        (GqlQueryPolicy::new(5, 6, complete.evaluator.work_units - 1, u64::MAX), GlaLimitDimension::WorkUnits),
        (GqlQueryPolicy::new(5, 6, u64::MAX, complete.evaluator.scratch_entries - 1), GlaLimitDimension::ScratchEntries),
    ] {
        assert!(matches!(run(policy), Err(GqlQueryError::Evaluator(error))
            if error.dimension == dimension && error.observed == u128::from(error.limit) + 1));
    }
    for (offset, count, expected) in [(4, Some(2), 2), (5, Some(9), 1), (6, None, 0), (0, Some(0), 0), (u64::MAX, None, 0)] {
        let paged = b.prepare_bindings(&["a", "c"], offset, count).unwrap().with_duplicates();
        let result = paged.plan().execute_governed(5, [], parallel_edges(), |_, _| Ok::<_, ()>(true),
            GqlQueryPolicy::new(5, expected, u64::MAX, u64::MAX), || Ok::<_, ()>(())).unwrap();
        assert_eq!(result.value.len() as u64, expected);
        assert_eq!(result.rows.result_rows, expected);
    }
}

#[test]
fn every_interruption_checkpoint_and_late_source_error_return_no_bag() {
    let all = builder().prepare_bindings(&["a", "c"], 0, None).unwrap().with_duplicates();
    let policy = GqlQueryPolicy::new(5, 6, u64::MAX, u64::MAX);
    let mut total = 0;
    all.plan().execute_governed(5, [], parallel_edges(), |_, _| Ok::<_, ()>(true), policy,
        || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut calls = 0;
        let result = all.plan().execute_governed(5, [], parallel_edges(), |_, _| Ok::<_, ()>(true), policy,
            || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(calls, stop);
    }
    let mut b = GraphPatternBuilder::new();
    b.vertex("a").unwrap().vertex("b").unwrap();
    b.edge("a", R, GlaDirection::Forward, "b").unwrap();
    b.filter("b", VertexPredicate::HasLabel(LabelId(1))).unwrap();
    let pattern = b.prepare_bindings(&["a", "b"], 0, None).unwrap().with_duplicates();
    let error = pattern.plan().execute([], [
        (VId(1), R, VId(2)), (VId(1), R, VId(2)), (VId(1), R, VId(3)),
    ], |vid, _| if vid == VId(3) { Err("source failed") } else { Ok(true) }).unwrap_err();
    assert_eq!(error, "source failed");
}
