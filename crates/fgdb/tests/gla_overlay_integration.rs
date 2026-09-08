//! Production-adapter regression coverage for the transaction-first GLA cutover.
//! Durable execution deliberately remains an independent implementation here.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BoundPlan, Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{EdgeDirection, ReturnProjection};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const LABEL: LabelId = LabelId(3);
const PROPERTY: PropertyKeyId = PropertyKeyId(4);
const HIGH: VId = VId((1_u128 << 96) + 7);

fn binding() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
        .with_label("L", LABEL)
        .with_property("n", PROPERTY)
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    );
    let mut db = Database::open_memory(cx, keys)
        .await
        .expect("memory database");
    let mut first = WriteBatch::new(R);
    for (vid, label, value) in [
        (VId(1), true, Some(-1)),
        (VId(2), true, Some(2)),
        (VId(3), false, Some(7)),
        (VId(4), true, None),
        (VId(5), true, Some(i64::MIN)),
        (HIGH, true, Some(i64::MAX)),
    ] {
        first.create_vertex(
            vid,
            if label { vec![LABEL] } else { vec![] },
            value
                .map(|value| vec![(PROPERTY, CanonicalScalar::Int(value))])
                .unwrap_or_default(),
        );
    }
    for (eid, source, destination) in [
        (100, VId(1), VId(2)),
        (101, VId(1), VId(2)),
        (102, VId(2), VId(3)),
        (103, VId(3), VId(3)),
        (104, VId(4), VId(1)),
        (105, VId(2), HIGH),
        (106, VId(5), VId(1)),
    ] {
        first.add_edge(EId(eid), source, destination, vec![]);
    }
    db.write(cx, first).await.expect("seed R");
    let mut second = WriteBatch::new(S);
    for (eid, source, destination) in [
        (200, VId(2), VId(4)),
        (201, VId(3), VId(1)),
        (202, VId(1), VId(4)),
        (203, HIGH, VId(1)),
    ] {
        second.add_edge(EId(eid), source, destination, vec![]);
    }
    db.write(cx, second).await.expect("seed S");
    db
}

fn plans() -> Vec<BoundPlan> {
    let bind = binding();
    let mut plans = Vec::new();
    for statement in [
        "MATCH (a)-[:R]->(b) RETURN b",
        "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c",
        "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN c",
    ] {
        let base = bind.bind(statement).expect("supported pattern");
        for direction in [
            EdgeDirection::Outgoing,
            EdgeDirection::Incoming,
            EdgeDirection::Undirected,
        ] {
            for projection in [
                ReturnProjection::Source,
                ReturnProjection::Destination,
                ReturnProjection::Hop2Destination,
            ] {
                if projection == ReturnProjection::Hop2Destination && base.hop2_relation.is_none() {
                    continue;
                }
                let mut plan = base.clone();
                plan.direction = direction;
                plan.projection = projection;
                plans.push(plan.clone());
                plan.skip = Some(1);
                plan.limit = Some(2);
                plans.push(plan);
            }
        }
    }
    // Exercise every legacy comparator field at the lowering boundary without
    // extending the parser grammar or sharing the durable executor's filters.
    let setters: [fn(&mut BoundPlan); 18] = [
        |p| p.src_prop = Some((PROPERTY, 2)),
        |p| p.src_prop_ne = Some((PROPERTY, 2)),
        |p| p.src_prop_gt = Some((PROPERTY, 2)),
        |p| p.src_prop_lt = Some((PROPERTY, 2)),
        |p| p.src_prop_ge = Some((PROPERTY, 2)),
        |p| p.src_prop_le = Some((PROPERTY, 2)),
        |p| p.dst_prop = Some((PROPERTY, 2)),
        |p| p.dst_prop_ne = Some((PROPERTY, 2)),
        |p| p.dst_prop_gt = Some((PROPERTY, 2)),
        |p| p.dst_prop_lt = Some((PROPERTY, 2)),
        |p| p.dst_prop_ge = Some((PROPERTY, 2)),
        |p| p.dst_prop_le = Some((PROPERTY, 2)),
        |p| p.hop2_dst_prop = Some((PROPERTY, 2)),
        |p| p.hop2_dst_prop_ne = Some((PROPERTY, 2)),
        |p| p.hop2_dst_prop_gt = Some((PROPERTY, 2)),
        |p| p.hop2_dst_prop_lt = Some((PROPERTY, 2)),
        |p| p.hop2_dst_prop_ge = Some((PROPERTY, 2)),
        |p| p.hop2_dst_prop_le = Some((PROPERTY, 2)),
    ];
    let base = bind
        .bind("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c")
        .expect("two hops");
    for direction in [
        EdgeDirection::Outgoing,
        EdgeDirection::Incoming,
        EdgeDirection::Undirected,
    ] {
        for setter in setters {
            let mut plan = base.clone();
            plan.direction = direction;
            setter(&mut plan);
            plans.push(plan);
        }
        for equal in [false, true] {
            let mut plan = base.clone();
            plan.direction = direction;
            if equal {
                plan.eq = Some(("a".into(), "b".into()));
            } else {
                plan.neq = Some(("a".into(), "b".into()));
            }
            plans.push(plan);
        }
        let mut plan = base.clone();
        plan.direction = direction;
        plan.src_label = Some(LABEL);
        plan.dst_label = Some(LABEL);
        plan.src_prop_ge = Some((PROPERTY, -1));
        plan.dst_prop_ne = Some((PROPERTY, 7));
        plan.hop2_dst_prop_le = Some((PROPERTY, i64::MAX));
        plans.push(plan);
    }
    let node = bind.bind("MATCH (a:L) RETURN a").expect("node scan");
    plans.push(node.clone());
    for setter in setters.into_iter().take(6) {
        let mut plan = node.clone();
        setter(&mut plan);
        plans.push(plan);
    }
    plans
}

#[test]
fn overlay_lowering_matches_durable_history_before_and_after_ordered_staging() {
    let ((), report) = run_async_under_lab(0x61a0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let basis = db.frontier().expect("basis");
        let pinned = db.read_session().expect("pin immutable generation");
        let plans = plans();
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut before = Vec::new();
        for plan in &plans {
            let durable = db.execute_prepared_gql(plan).expect("durable execution");
            assert_eq!(
                txn.execute_prepared_gql(&db, plan).expect("GLA execution"),
                durable,
                "{plan:?}"
            );
            assert_eq!(
                pinned.execute_prepared_gql(plan).expect("pinned execution"),
                durable
            );
            before.push(durable);
        }
        let high_plan = binding()
            .bind("MATCH (a)-[:R]->(b) RETURN b")
            .expect("high-ID query");
        assert!(
            txn.execute_prepared_gql(&db, &high_plan)
                .expect("high-ID result")
                .contains(&HIGH)
        );

        let mut staged = WriteBatch::new(R);
        staged.delete_edge(EId(102));
        staged.delete_vertex(VId(5));
        staged.set_vertex_label(VId(3), LABEL, true);
        staged.set_vertex_property(VId(2), PROPERTY, Some(CanonicalScalar::Int(10)));
        staged.set_vertex_property(VId(1), PROPERTY, None);
        staged.create_vertex(
            VId(9),
            vec![LABEL],
            vec![(PROPERTY, CanonicalScalar::Int(11))],
        );
        staged.add_edge(EId(300), VId(1), VId(9), vec![]);
        staged.add_edge(EId(301), VId(9), VId(3), vec![]);
        txn.write(&mut db, staged).expect("stage changes");
        let mut second = WriteBatch::new(R);
        second.set_vertex_property(VId(9), PROPERTY, Some(CanonicalScalar::Int(12)));
        txn.write(&mut db, second).expect("ordered second stage");
        let overlay: Vec<_> = plans
            .iter()
            .map(|plan| txn.execute_prepared_gql(&db, plan).expect("staged GLA"))
            .collect();
        assert_ne!(overlay, before, "fixture must change query results");
        for (plan, expected) in plans.iter().zip(&before) {
            assert_eq!(
                db.execute_prepared_gql(plan)
                    .expect("unpublished durable result"),
                *expected
            );
        }
        let published = txn
            .commit(&mut db, &commit)
            .await
            .expect("commit staged effects");
        for ((plan, expected), historical) in plans.iter().zip(&overlay).zip(&before) {
            assert_eq!(
                db.execute_prepared_gql_at(plan, published)
                    .expect("committed result"),
                *expected,
                "{plan:?}"
            );
            assert_eq!(
                db.execute_prepared_gql_at(plan, basis)
                    .expect("historical result"),
                *historical
            );
            assert_eq!(
                pinned
                    .execute_prepared_gql(plan)
                    .expect("old immutable generation"),
                *historical
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owned_preparation_overlay_evidence_and_cursor_replay_use_the_same_rows() {
    let ((), report) = run_async_under_lab(0x61a0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let mut txn = db.begin(&txn_cx).expect("begin");
        let query = txn
            .prepare_gql_query("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c", &binding())
            .expect("prepare");
        let rows = txn
            .execute_prepared_query(&db, &query)
            .expect("owned execution");
        assert_eq!(rows, vec![VId(1), VId(4)]);
        let artifact = txn
            .execute_prepared_query_overlay_artifact(&db, &query)
            .expect("issue evidence");
        assert_eq!(artifact.rows(), rows.as_slice());
        let bytes = artifact.to_bytes();
        let mut cursor = txn
            .open_untrusted_prepared_query_overlay_artifact_cursor(&db, &query, &bytes)
            .expect("audit and replay");
        assert_eq!(cursor.next_page(1).expect("first page").rows(), &[VId(1)]);
        let checkpoint = cursor.checkpoint_token().expect("remaining row").to_bytes();
        let mut resumed = txn
            .resume_untrusted_prepared_query_overlay_artifact_cursor(
                &db,
                &query,
                &bytes,
                &checkpoint,
            )
            .expect("resume");
        assert_eq!(resumed.next_page(1).expect("second page").rows(), &[VId(4)]);
        let other = txn
            .prepare_gql_query("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a", &binding())
            .expect("different projection");
        assert!(
            txn.open_untrusted_prepared_query_overlay_artifact_cursor(&db, &other, &bytes)
                .is_err()
        );
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(99)));
        txn.write(&mut db, staged)
            .expect("advance overlay without changing rows");
        assert_eq!(
            txn.execute_prepared_query(&db, &query)
                .expect("same logical result"),
            rows
        );
        assert!(
            txn.open_untrusted_prepared_query_overlay_artifact_cursor(&db, &query, &bytes)
                .is_err(),
            "same rows cannot authorize a stale overlay artifact"
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_out_rows_stay_in_the_conflict_footprint_but_disjoint_writes_can_commit() {
    let ((), report) = run_async_under_lab(0x61a0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        for conflicts in [true, false] {
            let mut db = seeded(&commit).await;
            let mut txn = db.begin(&txn_cx).expect("begin");
            let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, staged).expect("stage disjoint output");
            let mut plan = binding()
                .bind("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c LIMIT 1")
                .expect("query");
            plan.hop2_dst_prop_gt = Some((PROPERTY, 1000));
            assert!(
                txn.execute_prepared_gql(&db, &plan)
                    .expect("filtered read")
                    .is_empty()
            );
            let mut winner = WriteBatch::new(R);
            if conflicts {
                winner.set_vertex_property(VId(3), PROPERTY, Some(CanonicalScalar::Int(8)));
            } else {
                winner.create_vertex(VId(77), vec![], vec![]);
            }
            db.write(&commit, winner).await.expect("advance live");
            assert!(
                txn.execute_prepared_gql(&db, &plan)
                    .expect("still pinned")
                    .is_empty()
            );
            let frontier = db.frontier().expect("winner frontier");
            let result = txn.commit(&mut db, &commit).await;
            if conflicts {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(db.frontier().expect("no publication"), frontier);
                assert!(db.vertex(VId(99)).expect("loser output absent").is_none());
            } else {
                assert!(result.is_ok(), "disjoint commit was refused: {result:?}");
                assert!(
                    db.vertex(VId(99))
                        .expect("disjoint output published")
                        .is_some()
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
