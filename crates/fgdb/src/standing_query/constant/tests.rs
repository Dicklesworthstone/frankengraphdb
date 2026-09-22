use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 1_000_000, 1_000_000)
}
fn value(n: Option<i64>) -> GraphValue {
    GraphValue::Scalar(n.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn relation() -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(
                [Some(3), Some(1), Some(3), None]
                    .into_iter()
                    .map(|n| GraphSetValue::Value(value(n)))
                    .collect(),
            ),
        )
        .unwrap()
}
fn build(query: PreparedGraphSet, p: GqlQueryPolicy) -> Result<State, StandingQueryFailure> {
    State::build(query, CommitSeq(7), p, &mut || Ok(()))
}
fn sequence(state: &State) -> Vec<Vec<GraphValue>> {
    state
        .ordered
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn source_free_pages_and_zero_column_multiplicities_are_exact() {
    let state = build(relation().with_page(1, Some(3)), policy(3)).unwrap();
    assert_eq!(
        sequence(&state),
        vec![
            vec![value(Some(1))],
            vec![value(Some(3))],
            vec![value(None)]
        ]
    );
    assert_eq!(state.rows.len(), 3);
    assert!(state.last_delta.is_none());
    let singleton = build(PreparedGraphSet::singleton(), policy(1)).unwrap();
    assert_eq!(sequence(&singleton), vec![Vec::<GraphValue>::new()]);
    assert_eq!(singleton.rows.iter().next().unwrap().1, &ZWeight::ONE);
    for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        let constant_columns = relation()
            .project(
                vec![GraphSetProjection::new(
                    "c",
                    GraphSetValue::Value(value(Some(1))),
                )],
                q,
            )
            .unwrap();
        let state = build(constant_columns, policy(4)).unwrap();
        let count = if q == GraphSetQuantifier::All { 4 } else { 1 };
        assert_eq!(state.ordered.len(), count);
        assert_eq!(state.rows.len(), 1);
        assert_eq!(
            state.rows.iter().next().unwrap().1.to_i128(),
            Some(count as i128)
        );
    }
}

#[test]
fn constant_execution_and_retention_share_exact_budgets_and_checkpoints() {
    let before = relation().canonical_bytes();
    let mut calls = 0;
    let expected = State::build(relation(), CommitSeq(7), policy(4), &mut || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    let stats = expected.stats;
    let exact = GqlQueryPolicy::new(0, 4, stats.work_units, stats.scratch_entries);
    assert_eq!(build(relation(), exact).unwrap().rows, expected.rows);
    for (p, reason) in [
        (
            GqlQueryPolicy::new(0, 4, stats.work_units - 1, stats.scratch_entries),
            StandingQueryFailure::WorkBudget,
        ),
        (
            GqlQueryPolicy::new(0, 4, stats.work_units, stats.scratch_entries - 1),
            StandingQueryFailure::ScratchBudget,
        ),
        (
            GqlQueryPolicy::new(0, 3, stats.work_units, stats.scratch_entries),
            StandingQueryFailure::ResultBudget,
        ),
    ] {
        assert!(matches!(build(relation(), p), Err(actual) if actual == reason));
    }
    for stop in 1..=calls {
        let mut seen = 0;
        let result = State::build(relation(), CommitSeq(7), exact, &mut || {
            seen += 1;
            if seen == stop {
                Err(StandingQueryFailure::Interrupted)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(StandingQueryFailure::Interrupted)));
        assert_eq!(seen, stop);
        assert_eq!(relation().canonical_bytes(), before);
        assert_eq!(build(relation(), exact).unwrap().rows, expected.rows);
    }
}

#[test]
fn hidden_expression_errors_and_graph_sources_never_become_constants() {
    let divide = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(1)),
        GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ])
    .unwrap();
    let invalid = PreparedGraphSet::singleton()
        .project(
            vec![GraphSetProjection::new(
                "bad",
                GraphSetValue::Integer(divide),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    assert!(matches!(
        build(invalid, policy(0)),
        Err(StandingQueryFailure::InputExpression { column: 0, .. })
    ));
    let graph: PreparedGraphSet =
        PreparedGraphText::prepare("MATCH (n) RETURN n", |_, _: &str| None)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .into();
    for query in [graph.clone(), graph.with_page(0, Some(0))] {
        assert!(matches!(
            build(query, policy(0)),
            Err(StandingQueryFailure::InvalidDelta)
        ));
    }
}

#[test]
fn constant_registry_advances_without_evaluation_and_rebuild_is_atomic() {
    let ((), report) = run_async_under_lab(0x636f_6e01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(
            &commit,
            crate::DatabaseKeys::new([11; 32], DatabaseSecurityNamespaceId([12; 32]), [13; 32]),
        )
        .await
        .unwrap();
        let handle = db
            .register_standing_constant(&cx, relation(), policy(4))
            .unwrap();
        let old_rows = db
            .standing_rows(&cx, &handle)
            .unwrap()
            .rows()
            .checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let len = db.standing_queries.len();
        for id in 1..=3 {
            let mut batch = crate::WriteBatch::new(RelationId(1));
            batch.create_vertex(VId(id), vec![], vec![]);
            let at = db.write(&commit, batch).await.unwrap();
            let view = db.standing_rows(&cx, &handle).unwrap();
            assert_eq!(view.frontier(), at);
            assert_eq!(view.rows(), &old_rows);
            assert_eq!(view.last_maintenance().work_units, 1);
            assert_eq!(view.last_maintenance().scratch_entries, 0);
            let query = &db.standing_queries[handle.index];
            assert!(sets::delta(query).unwrap().is_empty());
        }
        let before = db.standing_rows(&cx, &handle).unwrap().frontier();
        assert!(db.rebuild_standing_query(&cx, &handle, policy(3)).is_err());
        assert_eq!(db.standing_queries.len(), len);
        let view = db.standing_rows(&cx, &handle).unwrap();
        assert_eq!(view.rows(), &old_rows);
        assert_eq!(view.frontier(), before);
        assert_eq!(
            db.rebuild_standing_query(&cx, &handle, policy(4)).unwrap(),
            before
        );
        assert!(sets::delta(&db.standing_queries[handle.index]).is_none());
        assert_eq!(db.standing_rows(&cx, &handle).unwrap().rows(), &old_rows);
        assert!(matches!(
            db.standing_query(&cx, &handle),
            Err(StandingQueryError::Unsupported)
        ));
        let mut other = Database::open_memory(
            &commit,
            crate::DatabaseKeys::new([14; 32], DatabaseSecurityNamespaceId([15; 32]), [16; 32]),
        )
        .await
        .unwrap();
        assert!(matches!(
            other.standing_rows(&cx, &handle),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(matches!(
            other.rebuild_standing_query(&cx, &handle, policy(4)),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
