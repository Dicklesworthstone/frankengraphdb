//! Transaction-overlay coverage for strict source-property less-than MATCH.
//!
//! The unfiltered pair proves the durable and staged edge faces are both
//! populated before the comparator is attributed. The filtered pair then
//! proves staged `k = 0` is visible only through the transaction while the
//! durable boundary value `k = 1` fails strict `< 1`.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const FILTERED: &str = "MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn less_than_overlay_sees_staged_lesser_source_without_dirty_read() {
    let ((), report) = run_async_under_lab(0x61_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-lt-overlay-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("durable edge commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(0))]);
        staged.create_vertex(VId(6), vec![], vec![]);
        staged.add_edge(EId(11), VId(5), VId(6), vec![]);
        txn.write(&mut db, staged)
            .expect("stages the lesser source and destination");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K);

        // Attribution first: both unfiltered faces are non-empty, and only
        // the transaction sees staged destination 6.
        assert_eq!(
            txn.execute_gql(&db, UNFILTERED, &bind)
                .expect("unfiltered overlay MATCH executes"),
            vec![VId(2), VId(6)]
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered base MATCH executes"),
            vec![VId(2)],
            "DIRTY READ: staged destination 6 leaked into the shared handle"
        );

        assert_eq!(
            txn.execute_gql(&db, FILTERED, &bind)
                .expect("filtered overlay MATCH executes"),
            vec![VId(6)]
        );
        assert!(
            db.execute_gql(FILTERED, &bind)
                .expect("filtered base MATCH executes")
                .is_empty(),
            "durable k=1 fails strict < 1 and staged destination 6 must stay hidden"
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
