//! Relationship identity is enforced before contributions and scope witnesses.

use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::GraphAggregate;
use fgdb_gql::algebra::{GraphColumn, GraphMatchClause, GraphMatchMode, GraphPatternBuilder};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

#[derive(Clone, Copy, Debug)]
enum Kind {
    Positive,
    Optional,
    Exists,
    Anti,
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn definition(mode: GraphMatchMode, kind: Kind) -> PreparedGraphAggregate {
    let mut pattern = GraphPatternBuilder::new();
    pattern.match_mode(mode);
    pattern.vertex("a").unwrap().vertex("b").unwrap();
    pattern
        .edge("a", RelationId(1), GlaDirection::Undirected, "b")
        .unwrap();
    pattern
        .edge("b", RelationId(1), GlaDirection::Undirected, "a")
        .unwrap();
    let columns = [GraphColumn::vertex("root", "a")];
    let input = if matches!(kind, Kind::Positive) {
        pattern.prepare_values(&columns, 0, None).unwrap()
    } else {
        let mut root = GraphPatternBuilder::new();
        root.vertex("a").unwrap();
        let clause = match kind {
            Kind::Optional => GraphMatchClause::optional(&pattern),
            Kind::Exists => GraphMatchClause::exists(&pattern),
            Kind::Anti => GraphMatchClause::not_exists(&pattern),
            Kind::Positive => unreachable!(),
        };
        root.prepare_values_with_clauses(&[clause], &columns, 0, None)
            .unwrap()
    }
    .with_duplicates();
    PreparedGraphAggregate::prepare(input, &[0], &[GraphAggregate::count_rows("count")], 0, None)
        .unwrap()
}

fn counts(query: &StandingQuery) -> BTreeMap<VId, u64> {
    query
        .rows
        .iter()
        .map(|(row, weight)| {
            assert_eq!(*weight, fgdb_delta_types::ZWeight::ONE);
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
            )
        })
        .collect()
}

fn expected(kind: Kind, positive: [u64; 3]) -> BTreeMap<VId, u64> {
    positive
        .into_iter()
        .enumerate()
        .filter_map(|(index, count)| {
            let count = match kind {
                Kind::Positive => count,
                Kind::Optional => count.max(1),
                Kind::Exists => u64::from(count != 0),
                Kind::Anti => u64::from(count == 0),
            };
            (count != 0).then_some((VId(index as u128 + 1), count))
        })
        .collect()
}

#[test]
fn different_edges_maintains_parallel_paths_and_complete_scope_witnesses_across_insert_retract() {
    let ((), report) = run_async_under_lab(0x6ab4, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        for id in 1..=3 {
            first.create_vertex(VId(id), vec![], vec![]);
        }
        first.add_edge(EId(1), VId(1), VId(2), vec![]);
        db.write(&commit, first).await.unwrap();
        let mut states = Vec::new();
        for mode in [
            GraphMatchMode::DifferentEdges,
            GraphMatchMode::RepeatableElements,
        ] {
            for kind in [Kind::Positive, Kind::Optional, Kind::Exists, Kind::Anti] {
                let prepared = definition(mode, kind);
                assert!(eligible(&prepared));
                assert!(Shape::of(&prepared).is_some());
                let state = db
                    .prepare_standing_query(&query, prepared, policy())
                    .unwrap();
                let positive = match mode {
                    GraphMatchMode::DifferentEdges => [0, 0, 0],
                    GraphMatchMode::RepeatableElements => [1, 1, 0],
                };
                assert_eq!(
                    counts(&state),
                    expected(kind, positive),
                    "bootstrap {mode:?} {kind:?}"
                );
                states.push((mode, kind, state));
            }
        }

        let mut parallel = WriteBatch::new(RelationId(1));
        parallel.add_edge(EId(2), VId(1), VId(2), vec![]);
        let mut replace = WriteBatch::new(RelationId(1));
        replace.delete_edge(EId(1));
        replace.add_edge(EId(3), VId(1), VId(1), vec![]);
        let mut loops = WriteBatch::new(RelationId(1));
        loops.add_edge(EId(4), VId(1), VId(1), vec![]);
        let mut remove_bridge = WriteBatch::new(RelationId(1));
        remove_bridge.delete_edge(EId(2));
        for (batch, different, repeatable) in [
            (parallel, [2, 2, 0], [4, 4, 0]),
            (replace, [0, 0, 0], [2, 1, 0]),
            (loops, [2, 0, 0], [5, 1, 0]),
            (remove_bridge, [2, 0, 0], [4, 0, 0]),
        ] {
            let at = db.write(&commit, batch).await.unwrap();
            let delta = db.delta_index().unwrap().get(at).unwrap().clone();
            for (mode, kind, state) in &mut states {
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: state.policy,
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                state.maintain(&delta, &mut meter).unwrap();
                state.frontier = at;
                let positive = match mode {
                    GraphMatchMode::DifferentEdges => different,
                    GraphMatchMode::RepeatableElements => repeatable,
                };
                assert_eq!(
                    counts(state),
                    expected(*kind, positive),
                    "tick {at:?} {mode:?} {kind:?}"
                );
                let fresh = db
                    .prepare_standing_query(&query, definition(*mode, *kind), policy())
                    .unwrap();
                assert_eq!(state.rows, fresh.rows);
                assert_eq!(state.aggregate, fresh.aggregate);
                assert_eq!(state.edges, fresh.edges);
                assert_eq!(state.vertices, fresh.vertices);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
