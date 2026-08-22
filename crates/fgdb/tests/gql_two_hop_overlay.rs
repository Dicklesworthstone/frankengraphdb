//! **The two-hop MATCH through the transaction overlay**
//! (`fgdb-gql-two-hop-8pfw`, overlay slice).
//!
//! The composed pattern must compose across the durable/staged boundary: a
//! staged `:S` continuation (`2-[:S]->5`) joins the txn's `RETURN c`
//! beside the durable composed destination, while a staged `:S` edge from
//! a non-`:R`-destination (`9-[:S]->8`) stays excluded — the overlay must
//! not relax hop one's reach just because hop two is staged. Paired at the
//! same instant, the shared handle answers without the staged row (no
//! dirty read); abort erases it; and the one-hop projection stays put, so
//! composing a staged hop moved nothing it should not have.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const TWO_HOP_A: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a";
const TWO_HOP_B: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN b";
const ONE_HOP_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-two-hop-overlay-{}-{name}",
        std::process::id()
    ))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn bind_rs() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

/// The whole slice as one flow: durable compose, stage a continuation and
/// a decoy, pair overlay against base, abort, re-pin.
#[test]
fn the_overlay_composes_staged_continuations_and_abort_erases_them() {
    under_lab(0x27_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("staged-compose");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 4, 7, 8, 9] {
            r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
        }
        r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        r_batch.add_edge(EId(11), VId(1), VId(7), vec![]);
        db.write(&commit, r_batch).await.expect("R edges commit");
        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
        db.write(&commit, s_batch)
            .await
            .expect("durable S edge commits");

        // Stage one :S batch: the real continuation 2->5 and the decoy 9->8
        // whose source is no :R destination.
        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(S);
        staged.create_vertex(VId(5), vec![], vec![]);
        staged.add_edge(EId(21), VId(2), VId(5), vec![]);
        staged.add_edge(EId(22), VId(9), VId(8), vec![]);
        txn.write(&mut db, staged)
            .expect("stages the continuations");

        // THE PAIRING: the staged continuation joins the txn's composed
        // answer — the staged decoy does not — while the shared handle at
        // the same instant answers without either.
        assert_eq!(
            txn.execute_gql(&db, TWO_HOP_C, &bind_rs())
                .expect("the txn's two-hop RETURN c executes"),
            vec![VId(4), VId(5)],
            "the staged :S continuation composes; the staged :S edge from a \
             non-:R-destination does not — staging hop two must not relax \
             hop one's reach"
        );
        assert_eq!(
            db.execute_gql(TWO_HOP_C, &bind_rs())
                .expect("the base two-hop RETURN c executes"),
            vec![VId(4)],
            "DIRTY READ: the staged continuation leaked into the shared handle"
        );
        assert_eq!(
            txn.execute_gql(&db, TWO_HOP_A, &bind_rs())
                .expect("the txn's two-hop RETURN a executes"),
            vec![VId(1)],
            "one source completes both hops — 9's staged edge composes nothing"
        );
        assert_eq!(
            txn.execute_gql(&db, TWO_HOP_B, &bind_rs())
                .expect("the txn's two-hop RETURN b executes"),
            vec![VId(2)],
            "the middle vertex is still only 2 — 7 dangles, 9 was never reached"
        );

        txn.abort();
        assert_eq!(
            db.execute_gql(TWO_HOP_C, &bind_rs())
                .expect("the live two-hop RETURN c executes after abort"),
            vec![VId(4)],
            "the aborted continuation is gone from the composed answer"
        );
        assert_eq!(
            db.execute_gql(ONE_HOP_B, &bind_rs())
                .expect("the live one-hop RETURN b executes"),
            vec![VId(2), VId(7)],
            "the one-hop projection never moved — composing a staged hop \
             touched nothing else"
        );
    });
}
