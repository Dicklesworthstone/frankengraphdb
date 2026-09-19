//! Failed derived maintenance can be rebuilt without rolling back durable data.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, DerivedPublicationStage, MemVfs, StandingQueryError,
    StandingQueryFailure, StandingQueryHandle, WriteBatch, WriteError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
use fgdb_gql::{GqlQueryPolicy, GraphAggregate, GraphAggregateRow, PreparedGraphAggregate};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId,
};

const GROUP: PropertyKeyId = PropertyKeyId(1);
const SCORE: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn definition() -> PreparedGraphAggregate {
    let mut input = GraphPatternBuilder::new();
    input.vertex("n").unwrap();
    let input = input
        .prepare_values(
            &[
                GraphColumn::property("group", "n", GROUP),
                GraphColumn::property("score", "n", SCORE),
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
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("total", 1),
            GraphAggregate::average_int("mean", 1),
        ],
        0,
        None,
    )
    .unwrap()
}
async fn insert(db: &mut Database<MemVfs>, cx: &CommitCx, id: u128, group: i64, score: i64) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(
        VId(id),
        vec![],
        vec![
            (GROUP, CanonicalScalar::Int(group)),
            (SCORE, CanonicalScalar::Int(score)),
        ],
    );
    db.write(cx, batch).await.unwrap();
}
fn rows(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
) -> Vec<GraphAggregateRow> {
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    view.rows()
        .iter()
        .map(|(row, weight)| {
            assert_eq!(weight, &ZWeight::ONE);
            row.clone()
        })
        .collect()
}
fn verify(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle) {
    let mut expected = db
        .execute_graph_aggregate_governed(cx, &definition(), wide())
        .unwrap()
        .value;
    expected.sort();
    assert_eq!(rows(db, cx, handle), expected);
}

#[test]
fn failed_group_limit_rebuilds_in_place_at_current_frontier_and_resumes_commits() {
    let ((), report) = run_async_under_lab(0x6b01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        insert(&mut db, &commit, 1, 1, 7).await;
        let narrow = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let handle = db
            .register_standing_query(&cx, definition(), narrow)
            .unwrap();
        let alias = handle.clone();
        let basis = db.frontier().unwrap();
        insert(&mut db, &commit, 2, 2, 9).await;
        assert!(
            matches!(db.standing_query(&cx, &handle), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::ResultBudget,
        }) if frontier == basis)
        );
        let mut later = WriteBatch::new(R);
        later.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(17)));
        db.write(&commit, later).await.unwrap();
        let current = db.frontier().unwrap();
        assert!(matches!(
            db.rebuild_standing_query(&cx, &handle, narrow),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(
            matches!(db.standing_query(&cx, &alias), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::ResultBudget,
        }) if frontier == basis)
        );
        assert_eq!(
            db.rebuild_standing_query(&cx, &handle, wide()).unwrap(),
            current
        );
        assert_eq!(
            db.frontier().unwrap(),
            current,
            "rebuild is not a database write"
        );
        verify(&db, &cx, &alias);
        let snapshot = rows(&db, &cx, &alias);
        assert_eq!(snapshot[0].get(1).unwrap().as_integer(), Some(17));
        insert(&mut db, &commit, 3, 3, 11).await;
        verify(&db, &cx, &handle);
        assert_eq!(
            rows(&db, &cx, &alias).len(),
            3,
            "successful rebuild installs its wider policy"
        );
        db.compact(&commit).await.unwrap();
        verify(&db, &cx, &handle);
        assert_eq!(snapshot.len(), 2, "owned prior result remains unchanged");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_rebuild_preserves_healthy_policy_and_corrected_data_can_repair_numeric_failure() {
    let ((), report) = run_async_under_lab(0x6b02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        insert(&mut db, &commit, 1, 1, 7).await;
        let handle = db
            .register_standing_query(&cx, definition(), wide())
            .unwrap();
        let before = rows(&db, &cx, &handle);
        let stats = *db.standing_query(&cx, &handle).unwrap().last_maintenance();
        for policy in [
            GqlQueryPolicy::new(100_000, 100_000, 0, 10_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 0),
            GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000),
        ] {
            assert!(matches!(
                db.rebuild_standing_query(&cx, &handle, policy),
                Err(StandingQueryError::Maintenance(_))
            ));
            assert_eq!(rows(&db, &cx, &handle), before);
            assert_eq!(
                *db.standing_query(&cx, &handle).unwrap().last_maintenance(),
                stats
            );
        }
        insert(&mut db, &commit, 2, 2, 3).await;
        verify(&db, &cx, &handle);
        let healthy = db.frontier().unwrap();
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(
            VId(1),
            SCORE,
            Some(CanonicalScalar::ucs_basic_text("not-a-number").unwrap()),
        );
        db.write(&commit, invalid).await.unwrap();
        assert!(matches!(
            db.rebuild_standing_query(&cx, &handle, wide()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::NonIntegerSum
            ))
        ));
        assert!(
            matches!(db.standing_query(&cx, &handle), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::NonIntegerSum,
        }) if frontier == healthy)
        );
        let mut correction = WriteBatch::new(R);
        correction.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(-9)));
        db.write(&commit, correction).await.unwrap();
        assert!(matches!(
            db.standing_query(&cx, &handle),
            Err(StandingQueryError::Unavailable { .. })
        ));
        db.rebuild_standing_query(&cx, &handle, wide()).unwrap();
        verify(&db, &cx, &handle);
        let mut deletion = WriteBatch::new(R);
        deletion.delete_vertex(VId(1));
        db.write(&commit, deletion).await.unwrap();
        verify(&db, &cx, &handle);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rebuild_exact_work_and_scratch_limits_succeed_and_one_below_preserves_the_view() {
    let ((), report) = run_async_under_lab(0x6b03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        insert(&mut db, &commit, 1, 1, 5).await;
        insert(&mut db, &commit, 2, 1, 8).await;
        let handle = db
            .register_standing_query(&cx, definition(), wide())
            .unwrap();
        let frontier = db.frontier().unwrap();
        db.rebuild_standing_query(&cx, &handle, wide()).unwrap();
        let stats = *db.standing_query(&cx, &handle).unwrap().last_maintenance();
        assert!(stats.work_units > 0 && stats.scratch_entries > 0);
        let before = rows(&db, &cx, &handle);
        for (work, scratch, passes) in [
            (stats.work_units, stats.scratch_entries, true),
            (stats.work_units - 1, stats.scratch_entries, false),
            (stats.work_units, stats.scratch_entries - 1, false),
        ] {
            let result = db.rebuild_standing_query(
                &cx,
                &handle,
                GqlQueryPolicy::new(100_000, 100_000, work, scratch),
            );
            assert_eq!(result.is_ok(), passes);
            if passes {
                assert_eq!(result.unwrap(), frontier);
            }
            assert_eq!(rows(&db, &cx, &handle), before);
            assert_eq!(
                *db.standing_query(&cx, &handle).unwrap().last_maintenance(),
                stats
            );
            assert_eq!(db.frontier().unwrap(), frontier);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rebuild_never_adopts_foreign_or_reopened_handles() {
    let ((), report) = run_async_under_lab(0x6b04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        insert(&mut db, &commit, 1, 1, 5).await;
        let handle = db
            .register_standing_query(&cx, definition(), wide())
            .unwrap();
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.rebuild_standing_query(&cx, &handle, wide()),
            Err(StandingQueryError::ForeignHandle)
        ));
        verify(&db, &cx, &handle);
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            reopened.rebuild_standing_query(&cx, &handle, wide()),
            Err(StandingQueryError::ForeignHandle)
        ));
        let fresh = reopened
            .register_standing_query(&cx, definition(), wide())
            .unwrap();
        reopened
            .rebuild_standing_query(&cx, &fresh, wide())
            .unwrap();
        verify(&reopened, &cx, &fresh);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fenced_publication_requires_database_recovery_before_any_view_rebuild() {
    let ((), report) = run_async_under_lab(0x6b05, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        insert(&mut db, &commit, 1, 1, 5).await;
        let handle = db
            .register_standing_query(&cx, definition(), wide())
            .unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(13)));
        assert!(matches!(
            db.write_with_publication_failure(
                &commit,
                change,
                DerivedPublicationStage::PublishPartitionRoot
            )
            .await,
            Err(WriteError::CommittedNeedsRecovery { .. })
        ));
        assert!(matches!(
            db.rebuild_standing_query(&cx, &handle, wide()),
            Err(StandingQueryError::Read(_))
        ));
        assert!(matches!(
            db.standing_query(&cx, &handle),
            Err(StandingQueryError::Read(_))
        ));
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.rebuild_standing_query(&cx, &handle, wide()),
            Err(StandingQueryError::ForeignHandle)
        ));
        let handle = db
            .register_standing_query(&cx, definition(), wide())
            .unwrap();
        verify(&db, &cx, &handle);
        assert_eq!(
            rows(&db, &cx, &handle)[0].get(1).unwrap().as_integer(),
            Some(13)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
