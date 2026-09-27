//! Edge history work is part of the public transaction read allowance.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GlaLimitDimension, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnState, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc4; 32],
        DatabaseSecurityNamespaceId([0xc5; 32]),
        [0xc6; 32],
    )
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1, 1, 100_000, 100_000)
}

#[test]
fn point_and_adjacency_budgets_include_historical_version_searches() {
    let ((), report) = run_async_under_lab(0x901b_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let query = contexts.query();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(0))]);
        db.write(&cx, seed).await.unwrap();
        let pinned = db.begin(&txcx).unwrap();
        let first_point = pinned
            .edge_property_governed(&db, &query, EId(10), P, policy())
            .unwrap();
        let first_neighbours = pinned
            .neighbours_governed(&db, &query, VId(1), R, policy())
            .unwrap();
        for value in 1..=64 {
            let mut update = WriteBatch::new(R);
            update.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(value)));
            db.write(&cx, update).await.unwrap();
        }
        let point = pinned
            .edge_property_governed(&db, &query, EId(10), P, policy())
            .unwrap();
        let neighbours = pinned
            .neighbours_governed(&db, &query, VId(1), R, policy())
            .unwrap();
        assert_eq!(point.value, first_point.value);
        assert_eq!(neighbours.value, first_neighbours.value);
        assert_eq!(point.rows, first_point.rows);
        assert_eq!(neighbours.rows, first_neighbours.rows);
        assert_eq!(
            point.evaluator.scratch_entries,
            first_point.evaluator.scratch_entries
        );
        assert_eq!(
            neighbours.evaluator.scratch_entries,
            first_neighbours.evaluator.scratch_entries
        );
        assert!(point.evaluator.work_units > first_point.evaluator.work_units);
        assert!(neighbours.evaluator.work_units > first_neighbours.evaluator.work_units);
        for (edge, measured, original) in [
            (true, point.evaluator, first_point.evaluator),
            (false, neighbours.evaluator, first_neighbours.evaluator),
        ] {
            let exact = GqlQueryPolicy::new(1, 1, measured.work_units, measured.scratch_entries);
            let stale = GqlQueryPolicy::new(1, 1, original.work_units, measured.scratch_entries);
            let error = if edge {
                assert_eq!(
                    pinned
                        .edge_property_governed(&db, &query, EId(10), P, exact)
                        .unwrap(),
                    point
                );
                pinned
                    .edge_property_governed(&db, &query, EId(10), P, stale)
                    .unwrap_err()
            } else {
                assert_eq!(
                    pinned
                        .neighbours_governed(&db, &query, VId(1), R, exact)
                        .unwrap(),
                    neighbours
                );
                pinned
                    .neighbours_governed(&db, &query, VId(1), R, stale)
                    .unwrap_err()
            };
            assert!(matches!(error, GqlQueryError::Evaluator(exceeded)
                if exceeded.dimension == GlaLimitDimension::WorkUnits
                    && exceeded.limit == original.work_units
                    && exceeded.observed == u128::from(original.work_units) + 1));
        }
        pinned.abort();
        // A lookup failure on either accessor must retain the existing
        // refusal witness, even if no logical row was delivered.
        for edge in [true, false] {
            let mut txn = db.begin(&txcx).unwrap();
            let tiny = GqlQueryPolicy::new(1, 1, 2, 100_000);
            let error = if edge {
                txn.edge_property_governed(&db, &query, EId(10), P, tiny)
                    .unwrap_err()
            } else {
                txn.neighbours_governed(&db, &query, VId(1), R, tiny)
                    .unwrap_err()
            };
            assert!(matches!(error, GqlQueryError::Evaluator(_)));
            assert_eq!(txn.state(), EmbeddedTxnState::Active);
            let mut unrelated = WriteBatch::new(R);
            unrelated.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Bool(edge)));
            db.write(&cx, unrelated).await.unwrap();
            assert!(matches!(
                txn.finish(&mut db, &cx).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
