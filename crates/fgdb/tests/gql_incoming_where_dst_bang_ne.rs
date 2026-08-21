//! **Incoming `WHERE a.k != 1` keeps the unequal dest**
//! (`fgdb-w5-parsers-nje-62-j5vd`).
//!
//! The C-style inequality moves to the incoming hop-1 DEST: on the
//! spelling `(a)<-[:R]-(b)` the flow is `b -R-> a`, so `a` is the stored
//! destination and the predicate reads it there while `RETURN a` answers
//! the same rows it filters. Only the dests carry `k` (`2{k:1}`,
//! `4{k:9}`, `6{no k}`), so `!= 1` keeps exactly the `k = 9` dest — the
//! keyless dest must not leak in (missing satisfies neither predicate,
//! the complement-of-equality cheat), and the `k = 1` dest fails the
//! inequality. The equality sibling still answers `[2]`, the unfiltered
//! statement answers all three dests, and the variable control runs on
//! the SOURCE spelling (`b.k != 1`, grammar since nje.60), which answers
//! nothing here because no stored source carries `k`.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_A_BANG_NE: &str = "MATCH (a)<-[:R]-(b) WHERE a.k != 1 RETURN a";
const IN_A_EQ: &str = "MATCH (a)<-[:R]-(b) WHERE a.k = 1 RETURN a";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b) RETURN a";
const IN_B_BANG_NE: &str = "MATCH (a)<-[:R]-(b) WHERE b.k != 1 RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_destination_bang_inequality_keeps_the_unequal_dest() {
    let ((), report) = run_async_under_lab(0x62_02, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-where-dst-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(5), vec![], vec![]);
        seed.create_vertex(VId(6), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K);

        let bang = db
            .execute_gql(IN_A_BANG_NE, &bind)
            .expect("incoming destination C-style inequality MATCH executes");
        assert_eq!(
            bang,
            vec![VId(4)],
            "only the k=9 dest passes the inequality"
        );
        assert!(
            !bang.contains(&VId(6)),
            "the keyless dest is OUT, not trivially unequal"
        );
        assert!(!bang.contains(&VId(2)), "the k=1 dest fails the inequality");

        assert_eq!(
            db.execute_gql(IN_A_EQ, &bind)
                .expect("the destination-equality sibling still executes"),
            vec![VId(2)],
            "equality still answers exactly the k=1 dest"
        );
        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming MATCH executes"),
            vec![VId(2), VId(4), VId(6)],
            "without WHERE all three dests answer"
        );

        // The variable control: the SOURCE spelling (grammar since nje.60)
        // still executes — no stored source carries k on this fixture.
        assert!(
            db.execute_gql(IN_B_BANG_NE, &bind)
                .expect("the source != spelling still executes (nje.60)")
                .is_empty(),
            "no writer carries k — the dest and source cells are separated"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
