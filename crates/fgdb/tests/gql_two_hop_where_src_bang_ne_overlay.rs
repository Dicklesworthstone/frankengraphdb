//! The C-style twin of `gql_two_hop_where_prop_overlay.rs`
//! (`fgdb-w5-parsers-nje.56`): the WriteTxn overlay's `!=` source filter
//! must alias `<>` — the durable origin carries `k = 1` and fails the
//! inequality, so only the path from the `k = 9` origin, completed by the
//! staged `:S` continuation, answers; the base sees nothing.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

#[test]
fn two_hop_source_bang_ne_sees_the_staged_continuation() {
    let ((), report) = run_async_under_lab(0x73_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-two-hop-where-src-bang-ne-overlay-{}",
            std::process::id()
        ));
        let r = RelationId(1);
        let s = RelationId(2);
        let key = PropertyKeyId(7);
        let mut db = Database::create(
            &commit,
            &dir,
            DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            ),
        )
        .await
        .expect("database creates");

        // WriteTxn accepts one relation per txn. Durable R owns both hop-1
        // origins; durable S owns the k=1 origin's continuation; the k=9
        // origin's continuation is the staged :S batch.
        let mut seed_r = WriteBatch::new(r);
        seed_r.create_vertex(VId(1), vec![], vec![(key, CanonicalScalar::Int(1))]);
        seed_r.create_vertex(VId(2), vec![], vec![]);
        seed_r.create_vertex(VId(3), vec![], vec![]);
        seed_r.create_vertex(VId(4), vec![], vec![(key, CanonicalScalar::Int(9))]);
        seed_r.create_vertex(VId(5), vec![], vec![]);
        seed_r.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed_r.add_edge(EId(12), VId(4), VId(5), vec![]);
        db.write(&commit, seed_r).await.expect("R fixture commits");
        let mut seed_s = WriteBatch::new(s);
        seed_s.add_edge(EId(11), VId(2), VId(3), vec![]);
        db.write(&commit, seed_s).await.expect("S fixture commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged_s = WriteBatch::new(s);
        staged_s.create_vertex(VId(6), vec![], vec![]);
        staged_s.add_edge(EId(13), VId(5), VId(6), vec![]);
        txn.write(&mut db, staged_s).expect("S overlay stages");

        let bind = RelationBind::new()
            .with_relation("R", r)
            .with_relation("S", s)
            .with_property("k", key);
        let filtered = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN c";
        assert_eq!(
            txn.execute_gql(&db, filtered, &bind)
                .expect("overlay source != MATCH executes"),
            vec![VId(6)],
            "only the k=9 origin's staged continuation answers — the \
             durable k=1 origin fails != exactly as it fails <>"
        );
        assert_eq!(
            db.execute_gql(filtered, &bind)
                .expect("base source != MATCH executes"),
            Vec::<VId>::new(),
            "the base's only composed path starts at k=1"
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
