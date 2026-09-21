//! Whole-tick witnesses, bootstrap, malformed cascades and atomic refusal for
//! one correlated multi-hop child. No test substitutes edge-prefix presence
//! for a complete OPTIONAL/EXISTS/NOT EXISTS witness.

use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::GraphAggregate;
use fgdb_gql::algebra::{
    GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValue, IntegerComparison,
};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

#[derive(Clone, Copy)]
enum Mode {
    Optional,
    Exists,
    Anti,
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}

fn definition(mode: Mode) -> PreparedGraphAggregate {
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap();
    let mut child = GraphPatternBuilder::new();
    child
        .vertex("a")
        .unwrap()
        .vertex("b")
        .unwrap()
        .vertex("c")
        .unwrap();
    child
        .edge("a", RelationId(1), GlaDirection::Forward, "b")
        .unwrap();
    child
        .edge("b", RelationId(2), GlaDirection::Forward, "c")
        .unwrap();
    child
        .compare_properties(
            "a",
            PropertyKeyId(1),
            IntegerComparison::LessOrEqual,
            "c",
            PropertyKeyId(1),
        )
        .unwrap();
    let clause = match mode {
        Mode::Optional => GraphMatchClause::optional(&child),
        Mode::Exists => GraphMatchClause::exists(&child),
        Mode::Anti => GraphMatchClause::not_exists(&child),
    };
    let (middle, end) = if matches!(mode, Mode::Optional) {
        ("b", "c")
    } else {
        ("a", "a")
    };
    let input = root
        .prepare_values_with_clauses(
            &[clause],
            &[
                GraphColumn::vertex("root", "a"),
                GraphColumn::vertex("middle", middle),
                GraphColumn::vertex("endpoint", end),
                GraphColumn::property("amount", end, PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(
        input,
        &[0],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("middles", 1),
            GraphAggregate::count("endpoints", 2),
            GraphAggregate::sum_int("sum", 3),
            GraphAggregate::count_distinct("distinct_endpoints", 2),
            GraphAggregate::min("minimum", 3),
        ],
        0,
        None,
    )
    .unwrap()
}

fn advance(
    query: &mut StandingQuery,
    batch: &LogicalDeltaBatch,
    checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
) -> Result<StandingQueryStats, StandingQueryFailure> {
    let mut meter = Meter {
        policy: query.policy,
        stats: StandingQueryStats::default(),
        checkpoint,
    };
    query.maintain(batch, &mut meter)?;
    query.frontier = batch.commit_seq();
    Ok(meter.stats)
}

fn seeded(batches: &[LogicalDeltaBatch], mode: Mode) -> StandingQuery {
    let definition = definition(mode);
    assert!(eligible(&definition));
    assert!(scoped::Shape::of(&definition).is_none());
    let shape = Shape::of(&definition).unwrap();
    assert_eq!(shape.atoms.len(), 2);
    assert_eq!(shape.width, 4);
    assert!(shape.scope.is_some());
    let edges = super::super::State::for_definition(&definition);
    assert!(edges.as_ref().unwrap().has_scope());
    let mut query = StandingQuery {
        definition,
        policy: policy(),
        edges,
        vertices: BTreeMap::new(),
        aggregate: IncrementalAggregate::new(),
        rows: ZSet::new(),
        last_delta: None,
        frontier: CommitSeq::ORIGIN,
        stats: StandingQueryStats::default(),
        failure: None,
    };
    for batch in batches {
        advance(&mut query, batch, &mut || Ok(())).unwrap();
    }
    query
}

// A fresh bootstrap and an incrementally advanced query have the same accepted
// state but intentionally different delta availability. Rollback checks below
// compare last_delta separately; never equate a new baseline with a successor.
fn same(actual: &StandingQuery, expected: &StandingQuery) {
    assert_eq!(actual.vertices, expected.vertices);
    assert_eq!(actual.edges, expected.edges); // every complete-path witness count
    assert_eq!(actual.aggregate, expected.aggregate);
    assert_eq!(actual.rows, expected.rows);
    assert_eq!(actual.frontier, expected.frontier);
    assert_eq!(actual.failure, expected.failure);
}

fn row(query: &StandingQuery, root: u128) -> Option<&GraphAggregateRow> {
    query.rows.iter().find_map(|(row, _)| {
        (row.keys().first().and_then(GraphValue::as_vertex) == Some(VId(root))).then_some(row)
    })
}

#[test]
fn only_complete_paths_suppress_null_rows_and_bootstrap_matches_committed_witnesses() {
    let ((), report) = run_async_under_lab(0x6ab0, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        for id in 1..=4 {
            first.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
        }
        first.add_edge(EId(1), VId(1), VId(2), vec![]);
        first.add_edge(EId(2), VId(1), VId(2), vec![]);
        let at = db.write(&commit, first).await.unwrap();
        let first = db.delta_index().unwrap().get(at).unwrap().clone();
        let mut suffix = WriteBatch::new(RelationId(2));
        suffix.add_edge(EId(11), VId(2), VId(3), vec![]);
        suffix.add_edge(EId(12), VId(2), VId(3), vec![]);
        let at = db.write(&commit, suffix).await.unwrap();
        let suffix = db.delta_index().unwrap().get(at).unwrap().clone();
        let mut replace = WriteBatch::new(RelationId(2));
        replace.delete_edge(EId(11)).delete_edge(EId(12));
        replace.add_edge(EId(13), VId(2), VId(4), vec![]);
        let at = db.write(&commit, replace).await.unwrap();
        let replace = db.delta_index().unwrap().get(at).unwrap().clone();
        for mode in [Mode::Optional, Mode::Exists, Mode::Anti] {
            let mut state = seeded(core::slice::from_ref(&first), mode);
            if matches!(mode, Mode::Exists) {
                assert!(state.rows.is_empty());
            } else {
                assert_eq!(state.rows.len(), 4);
                let root = row(&state, 1).unwrap();
                assert_eq!(root.get(0).unwrap().as_count(), Some(1));
                if matches!(mode, Mode::Optional) {
                    // Even the reachable middle is NULL: the ENTIRE child
                    // frame is null-extended when its final edge is absent.
                    assert_eq!(root.get(1).unwrap().as_count(), Some(0));
                    assert_eq!(root.get(2).unwrap().as_count(), Some(0));
                    assert!(root.get(3).unwrap().is_null());
                }
            }
            advance(&mut state, &suffix, &mut || Ok(())).unwrap();
            if matches!(mode, Mode::Optional) {
                let root = row(&state, 1).unwrap();
                assert_eq!(root.get(0).unwrap().as_count(), Some(4));
                assert_eq!(root.get(3).unwrap().as_integer(), Some(12));
                assert_eq!(root.get(4).unwrap().as_count(), Some(1));
            } else {
                assert_eq!(row(&state, 1).is_some(), matches!(mode, Mode::Exists));
            }
            advance(&mut state, &replace, &mut || Ok(())).unwrap();
            if matches!(mode, Mode::Optional) {
                let root = row(&state, 1).unwrap();
                assert_eq!(root.get(0).unwrap().as_count(), Some(2));
                assert_eq!(root.get(3).unwrap().as_integer(), Some(8));
            }
            let rebuilt = db
                .prepare_standing_query(&query, definition(mode), policy())
                .unwrap();
            same(&state, &rebuilt);
            assert!(state.last_delta.is_some());
            assert!(rebuilt.last_delta.is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_multi_hop_scope_checkpoint_and_budget_refusal_preserves_all_state() {
    let ((), report) = run_async_under_lab(0x6ab1, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut r = WriteBatch::new(RelationId(1));
        for id in 1..=5 {
            r.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
        }
        r.add_edge(EId(1), VId(1), VId(2), vec![]);
        r.add_edge(EId(2), VId(1), VId(2), vec![]);
        let mut s = WriteBatch::new(RelationId(2));
        s.add_edge(EId(11), VId(2), VId(3), vec![]);
        s.add_edge(EId(12), VId(2), VId(3), vec![]);
        let at = db.write_atomic(&commit, vec![r, s]).await.unwrap();
        let initial = db.delta_index().unwrap().get(at).unwrap().clone();
        let mut r = WriteBatch::new(RelationId(1));
        r.delete_vertex(VId(2));
        r.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
        r.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
        r.add_edge(EId(3), VId(1), VId(4), vec![]);
        let mut s = WriteBatch::new(RelationId(2));
        s.add_edge(EId(13), VId(4), VId(5), vec![]);
        let at = db.write_atomic(&commit, vec![s, r]).await.unwrap();
        let delta = db.delta_index().unwrap().get(at).unwrap().clone();
        for mode in [Mode::Optional, Mode::Exists, Mode::Anti] {
            let make = || seeded(core::slice::from_ref(&initial), mode);
            let before = make();
            let mut successful = make();
            let mut calls = 0;
            let stats = advance(&mut successful, &delta, &mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
            same(
                &successful,
                &db.prepare_standing_query(&query, definition(mode), policy())
                    .unwrap(),
            );
            for stop in 1..=calls {
                let mut candidate = make();
                let mut seen = 0;
                assert_eq!(
                    advance(&mut candidate, &delta, &mut || {
                        seen += 1;
                        if seen == stop {
                            Err(StandingQueryFailure::Interrupted)
                        } else {
                            Ok(())
                        }
                    }),
                    Err(StandingQueryFailure::Interrupted)
                );
                assert_eq!(seen, stop);
                same(&candidate, &before);
                assert_eq!(candidate.last_delta, before.last_delta);
                advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
                same(&candidate, &successful);
                assert_eq!(candidate.last_delta, successful.last_delta);
            }
            for reason in [
                StandingQueryFailure::WorkBudget,
                StandingQueryFailure::ScratchBudget,
                StandingQueryFailure::ResultBudget,
            ] {
                let mut candidate = make();
                match reason {
                    StandingQueryFailure::WorkBudget => {
                        candidate.policy.evaluator.max_work_units = stats.work_units - 1
                    }
                    StandingQueryFailure::ScratchBudget => {
                        candidate.policy.evaluator.max_scratch_entries = stats.scratch_entries - 1
                    }
                    _ => candidate.policy = GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
                }
                assert_eq!(advance(&mut candidate, &delta, &mut || Ok(())), Err(reason));
                same(&candidate, &before);
                assert_eq!(candidate.last_delta, before.last_delta);
                candidate.policy = policy();
                advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
                same(&candidate, &successful);
                assert_eq!(candidate.last_delta, successful.last_delta);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn incomplete_cascade_refuses_even_when_every_child_has_only_a_partial_path() {
    let ((), report) = run_async_under_lab(0x6ab2, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        let at = db.write(&commit, seed).await.unwrap();
        let initial = db.delta_index().unwrap().get(at).unwrap().clone();
        let mut delete = WriteBatch::new(RelationId(1));
        delete.delete_vertex(VId(2));
        let at = db.write(&commit, delete).await.unwrap();
        let delta = db.delta_index().unwrap().get(at).unwrap().clone();
        let mut entries = delta.coordinate_entries().to_vec();
        let mut removed = 0;
        for entry in &mut entries {
            for row in &mut entry.rows {
                if let DeltaRow::DeleteVertex {
                    sorted_retired_incident_edges,
                    ..
                } = row
                {
                    removed += sorted_retired_incident_edges.len();
                    sorted_retired_incident_edges.clear();
                }
            }
        }
        assert_eq!(removed, 1);
        let bad = LogicalDeltaBatch::from_parts_for_test(
            entries,
            *delta.source_template_digest(),
            delta.commit_marker_identity(),
            delta.commit_seq(),
            delta.frontier(),
        );
        for mode in [Mode::Optional, Mode::Exists, Mode::Anti] {
            let before = seeded(core::slice::from_ref(&initial), mode);
            let mut candidate = seeded(core::slice::from_ref(&initial), mode);
            assert_eq!(
                advance(&mut candidate, &bad, &mut || Ok(())),
                Err(StandingQueryFailure::InvalidDelta)
            );
            same(&candidate, &before);
            assert_eq!(candidate.last_delta, before.last_delta);
            advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
            assert_eq!(candidate.vertices.len(), 1);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
