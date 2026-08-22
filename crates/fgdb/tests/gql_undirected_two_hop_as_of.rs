//! **The undirected two-hop, pinned to a sequence**
//! (`fgdb-gql-undir-2hop-7mrc`, as-of slice).
//!
//! Composition through time: a later `:S` continuation widens the live
//! undirected `RETURN c` and must be invisible at the captured frontier.
//! The dual-incidence counterfeit (`9-[:S]->8`, no `:R`-incident endpoint)
//! is planted in the S1 prefix itself, so BOTH passes must exclude 8 —
//! pinning cannot be the thing that filters it. The directed composed
//! statement at S1 rides along exact, so the undirected as-of scan shares
//! the discipline rather than a loosened copy.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const UN_TWO_HOP_C: &str = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
const DIR_TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-undir-2hop-at-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
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

/// Pinned composition: 4 at S1, 5 only live, 8 never.
#[test]
fn the_pinned_undirected_two_hop_answers_the_s1_composition() {
    under_lab(0x7a_51, |cx| async move {
        let cx = &cx;
        let dir = scratch("pinned-compose");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 4, 8, 9] {
            r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
        }
        r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, r_batch).await.expect("R edge commits");
        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
        s_batch.add_edge(EId(21), VId(9), VId(8), vec![]);
        db.write(cx, s_batch).await.expect("S edges commit");
        let s1 = db.frontier().expect("healthy frontier");

        let mut widen = WriteBatch::new(S);
        widen.create_vertex(VId(5), vec![], vec![]);
        widen.add_edge(EId(22), VId(2), VId(5), vec![]);
        db.write(cx, widen)
            .await
            .expect("the widening continuation lands");

        let pinned = db
            .execute_gql_at(UN_TWO_HOP_C, &bind_rs(), s1)
            .expect("the pinned undirected two-hop executes");
        assert!(
            pinned.contains(&VId(4)),
            "the S1 composition through via 2 answers at S1: {pinned:?}"
        );
        assert!(
            !pinned.contains(&VId(5)),
            "the continuation committed after S1 must be invisible to the \
             pinned pass: {pinned:?}"
        );
        assert!(
            !pinned.contains(&VId(8)),
            "8's :S edge is IN the S1 prefix and still excluded — dual \
             incidence filters it, not the pin: {pinned:?}"
        );

        let live = db
            .execute_gql(UN_TWO_HOP_C, &bind_rs())
            .expect("the live undirected two-hop executes");
        assert!(
            live.contains(&VId(4)) && live.contains(&VId(5)),
            "the live composition holds both continuations: {live:?}"
        );
        assert!(
            !live.contains(&VId(8)),
            "the counterfeit stays out live too: {live:?}"
        );

        assert_eq!(
            db.execute_gql_at(DIR_TWO_HOP_C, &bind_rs(), s1)
                .expect("the pinned directed two-hop executes"),
            vec![VId(4)],
            "the directed composed statement at S1 is exact and unmoved"
        );
    });
}
