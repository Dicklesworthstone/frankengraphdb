//! ALL SHORTEST endpoint execution uses the real retained graph and governed
//! resource path. Expected bags are pinned independently from the implementation
//! so a fallback to ordinary WALK or to the live frontier fails these laws.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::{GqlQueryPolicy, GraphWalkBounds};
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}
fn bounds(minimum: u32, maximum: u32) -> GraphWalkBounds {
    GraphWalkBounds::new(minimum, maximum).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) -> fgdb_types::CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in [1_u128, 2, 3, 4, 5, 6, 10, 11] {
        batch.create_vertex(VId(id), vec![], vec![]);
    }
    // Source 1 has two equal first edges to 2, one to 3. Endpoint 4 has
    // three shortest depth-two occurrences. Endpoint 5 has two depth-two
    // occurrences; the three alternatives through 4 are longer and must lose.
    for (eid, source, destination) in [
        (11, 1, 2),
        (12, 1, 2),
        (13, 1, 3),
        (14, 2, 4),
        (15, 3, 4),
        (16, 4, 5),
        (17, 2, 5),
        (18, 4, 6),
        (19, 5, 5),
        // Isolated two-cycle for lower-bound semantics.
        (20, 10, 11),
        (21, 11, 10),
    ] {
        batch.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn shortest_bag_tracks_historical_topology_and_survives_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x5a07_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let seq1 = seed(&mut db, &commit).await;

        let expected1 = vec![
            VId(2),
            VId(2),
            VId(3),
            VId(4),
            VId(4),
            VId(4),
            VId(5),
            VId(5),
            VId(6),
            VId(6),
            VId(6),
        ];
        let first = db
            .execute_all_shortest_walk_governed_at(
                &query,
                VId(1),
                R,
                GlaDirection::Forward,
                bounds(1, 4),
                seq1,
                wide(),
            )
            .unwrap();
        assert_eq!(first.value, expected1);

        // A direct shortcut changes only endpoint 6's partition: its three
        // depth-three alternatives remain walks but are no longer shortest.
        let mut shortcut = WriteBatch::new(R);
        shortcut.add_edge(EId(22), VId(1), VId(6), vec![]);
        let seq2 = db.write(&commit, shortcut).await.unwrap();
        let pinned = db.read_session().unwrap();
        let expected2 = vec![
            VId(2),
            VId(2),
            VId(3),
            VId(4),
            VId(4),
            VId(4),
            VId(5),
            VId(5),
            VId(6),
        ];
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(1),
                R,
                GlaDirection::Forward,
                bounds(1, 4),
                seq2,
                wide(),
            )
            .unwrap()
            .value,
            expected2
        );
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(1),
                R,
                GlaDirection::Forward,
                bounds(1, 4),
                seq1,
                wide(),
            )
            .unwrap()
            .value,
            expected1,
            "new shortcut leaked into historical search"
        );

        // Remove the depth-two route to 5. The three depth-three routes through
        // endpoint 4 now become the shortest admissible alternatives to 5.
        let mut remove = WriteBatch::new(R);
        remove.delete_edge(EId(17));
        let seq3 = db.write(&commit, remove).await.unwrap();
        let expected3 = vec![
            VId(2),
            VId(2),
            VId(3),
            VId(4),
            VId(4),
            VId(4),
            VId(5),
            VId(5),
            VId(5),
            VId(6),
        ];
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(1),
                R,
                GlaDirection::Forward,
                bounds(1, 4),
                seq3,
                wide(),
            )
            .unwrap()
            .value,
            expected3
        );

        assert_eq!(
            pinned
                .execute_all_shortest_walk_governed_at(
                    &query,
                    VId(1),
                    R,
                    GlaDirection::Forward,
                    bounds(1, 4),
                    seq1,
                    wide(),
                )
                .unwrap()
                .value,
            expected1
        );
        assert_eq!(
            pinned
                .execute_all_shortest_walk_governed_at(
                    &query,
                    VId(1),
                    R,
                    GlaDirection::Forward,
                    bounds(1, 4),
                    seq2,
                    wide(),
                )
                .unwrap()
                .value,
            expected2
        );
        assert!(
            pinned
                .execute_all_shortest_walk_governed_at(
                    &query,
                    VId(1),
                    R,
                    GlaDirection::Forward,
                    bounds(1, 4),
                    seq3,
                    wide(),
                )
                .is_err(),
            "pinned shortest search cannot read beyond its frontier"
        );

        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (seq, expected) in [(seq1, expected1), (seq2, expected2), (seq3, expected3)] {
            assert_eq!(
                db.execute_all_shortest_walk_governed_at(
                    &query,
                    VId(1),
                    R,
                    GlaDirection::Forward,
                    bounds(1, 4),
                    seq,
                    wide(),
                )
                .unwrap()
                .value,
                expected,
                "reopen changed shortest bag at {seq:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn directions_lower_bounds_missing_sources_and_exact_resource_caps_are_governed() {
    let ((), report) = run_async_under_lab(0x5a07_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let seq = seed(&mut db, &commit).await;

        // Reverse from 6 reaches 4, then 2/3, then source 1 through three
        // equal shortest occurrences (two parallel 1->2 edges plus 1->3).
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(6),
                R,
                GlaDirection::Reverse,
                bounds(1, 4),
                seq,
                wide(),
            )
            .unwrap()
            .value,
            vec![VId(1), VId(1), VId(1), VId(2), VId(3), VId(4)]
        );

        // Undirected search counts a self-loop once, not once per orientation.
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(5),
                R,
                GlaDirection::Undirected,
                bounds(1, 2),
                seq,
                wide(),
            )
            .unwrap()
            .value,
            vec![VId(1), VId(1), VId(2), VId(3), VId(4), VId(5), VId(6)]
        );

        // Before the lower bound, identities remain traversable but un-settled.
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(10),
                R,
                GlaDirection::Forward,
                bounds(2, 3),
                seq,
                wide(),
            )
            .unwrap()
            .value,
            vec![VId(10), VId(11)]
        );
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(10),
                R,
                GlaDirection::Forward,
                bounds(0, 3),
                seq,
                wide(),
            )
            .unwrap()
            .value,
            vec![VId(10), VId(11)]
        );

        let missing = db
            .execute_all_shortest_walk_governed_at(
                &query,
                VId(999),
                R,
                GlaDirection::Forward,
                bounds(0, 4),
                seq,
                wide(),
            )
            .unwrap();
        assert!(
            missing.value.is_empty(),
            "absent source must not fabricate a zero-hop row"
        );
        assert_eq!(missing.rows.result_rows, 0);

        let measured = db
            .execute_all_shortest_walk_governed_at(
                &query,
                VId(1),
                R,
                GlaDirection::Forward,
                bounds(1, 4),
                seq,
                wide(),
            )
            .unwrap();
        let exact = GqlQueryPolicy::new(
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(1),
                R,
                GlaDirection::Forward,
                bounds(1, 4),
                seq,
                exact,
            )
            .unwrap(),
            measured
        );
        for policy in [
            GqlQueryPolicy::new(
                measured.rows.snapshot_records - 1,
                measured.rows.result_rows,
                u64::MAX,
                u64::MAX,
            ),
            GqlQueryPolicy::new(
                measured.rows.snapshot_records,
                measured.rows.result_rows - 1,
                u64::MAX,
                u64::MAX,
            ),
            GqlQueryPolicy::new(
                measured.rows.snapshot_records,
                measured.rows.result_rows,
                measured.evaluator.work_units - 1,
                u64::MAX,
            ),
            GqlQueryPolicy::new(
                measured.rows.snapshot_records,
                measured.rows.result_rows,
                u64::MAX,
                measured.evaluator.scratch_entries - 1,
            ),
        ] {
            assert!(
                db.execute_all_shortest_walk_governed_at(
                    &query,
                    VId(1),
                    R,
                    GlaDirection::Forward,
                    bounds(1, 4),
                    seq,
                    policy,
                )
                .is_err()
            );
        }

        assert!(
            db.execute_all_shortest_walk_governed_at(
                &query,
                VId(1),
                R,
                GlaDirection::Forward,
                bounds(1, 4),
                fgdb_types::CommitSeq(seq.0 + 1),
                wide(),
            )
            .is_err(),
            "future sequence must fail before search"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
