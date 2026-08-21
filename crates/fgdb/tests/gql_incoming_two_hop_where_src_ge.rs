//! **Incoming two-hop `WHERE a.k >= 9` keeps inclusive near ends**
//! (`fgdb-w5-parsers-nje.52`).
//!
//! The inclusive-greater comparator reads the near-end `a` destination on
//! the reversed two-hop chain while `RETURN c` answers the far-end origin.
//! Only destinations carry `k`: `1{k:9}`, `4{k:1}`, and `7{no k}`. Thus
//! `>= 9` answers `[3]`, `>= 1` answers `[3, 6]`, and `>= 10` answers
//! nothing. The keyless destination stays OUT. Equality and strict-greater
//! remain pinned, the far-end spelling is empty because origins carry no
//! `k`, and outgoing equality provides the direction control.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_A_GE_9: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 9 RETURN c";
const IN_A_GE_1: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 1 RETURN c";
const IN_A_GE_10: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 10 RETURN c";
const IN_A_GT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 1 RETURN c";
const IN_A_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c";
const IN_C_GE_9: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 9 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_A_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_near_end_greater_or_equal_keeps_inclusive_chains() {
    let ((), report) = run_async_under_lab(0x52_09, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-ge-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut r_seed = WriteBatch::new(R);
        r_seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(9))]);
        r_seed.create_vertex(VId(2), vec![], vec![]);
        r_seed.create_vertex(VId(3), vec![], vec![]);
        r_seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(1))]);
        r_seed.create_vertex(VId(5), vec![], vec![]);
        r_seed.create_vertex(VId(6), vec![], vec![]);
        r_seed.create_vertex(VId(7), vec![], vec![]);
        r_seed.create_vertex(VId(8), vec![], vec![]);
        r_seed.create_vertex(VId(9), vec![], vec![]);
        r_seed.add_edge(EId(10), VId(2), VId(1), vec![]);
        r_seed.add_edge(EId(11), VId(5), VId(4), vec![]);
        r_seed.add_edge(EId(12), VId(8), VId(7), vec![]);
        db.write(&commit, r_seed).await.expect("R chains commit");
        let mut s_seed = WriteBatch::new(S);
        s_seed.add_edge(EId(20), VId(3), VId(2), vec![]);
        s_seed.add_edge(EId(21), VId(6), VId(5), vec![]);
        s_seed.add_edge(EId(22), VId(9), VId(8), vec![]);
        db.write(&commit, s_seed).await.expect("S chains commit");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S)
            .with_property("k", K);

        let inclusive = db
            .execute_gql(IN_A_GE_9, &bind)
            .expect("incoming near-end greater-or-equal MATCH executes");
        assert_eq!(
            inclusive,
            vec![VId(3)],
            "only the k=9 destination meets the inclusive boundary"
        );
        assert!(
            !inclusive.contains(&VId(9)),
            "the keyless destination satisfies no ordered comparator"
        );

        assert_eq!(
            db.execute_gql(IN_A_GE_1, &bind)
                .expect("lower-bound inclusive spelling executes"),
            vec![VId(3), VId(6)],
            "both keyed destinations meet >= 1"
        );
        assert_eq!(
            db.execute_gql(IN_A_GE_10, &bind)
                .expect("upper-bound inclusive spelling executes"),
            Vec::<VId>::new(),
            "neither keyed destination meets >= 10"
        );

        assert_eq!(
            db.execute_gql(IN_A_GT, &bind)
                .expect("strict-greater sibling still executes"),
            vec![VId(3)],
            "only the k=9 destination is strictly greater than 1"
        );
        assert_eq!(
            db.execute_gql(IN_A_EQ, &bind)
                .expect("equality sibling still executes"),
            vec![VId(6)],
            "only the k=1 destination satisfies equality"
        );

        assert_eq!(
            db.execute_gql(IN_C_GE_9, &bind)
                .expect("far-end greater-or-equal spelling executes"),
            Vec::<VId>::new(),
            "origins carry no k, separating near-end and far-end cells"
        );
        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three reversed chains answer"
        );

        assert!(
            db.execute_gql(OUT_A_EQ, &bind)
                .expect("outgoing equality direction control executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
        );

        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <= 1 RETURN c",
                &bind
            )
            .expect("nje.53 near-end <= is grammar, not a Parse"),
            vec![VId(6)],
            "only the k=1 destination meets <= 1; the keyless destination stays OUT"
        );

        for off_grammar in [
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k >= 9 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 9 RETURN a",
        ] {
            let err = db.execute_gql(off_grammar, &bind).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm: {err:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
