//! **Two-hop `WHERE c.k <= 1` keeps inclusive lesser far ends**
//! (`fgdb-w5-parsers-nje.39`).
//!
//! The last far-end comparator cell: non-strict less on the composed
//! path's far end, on the four-chain fixture where `<=` cannot cheat as
//! `<` or `<>`: the boundary far end (`k = 1`) is IN — the exact row a
//! renamed `<` drops — while the `k = 9` far end is OUT — the exact row a
//! `<>` cheat admits — and the keyless far end satisfies no comparator at
//! all. Each of those three is asserted by name on top of the exact
//! `[3, 12]`. The full comparator family is re-pinned beside it — `<`
//! `[12]`, `>=` `[3, 6]`, `>` `[6]`, `=` `[3]` — so `<=` is visibly the
//! union of `=` and `<` and disjoint from none of its neighbours by
//! accident. The C-style `!=` on `c` stays Parse, and a WHERE on the
//! incoming two-hop chain stays off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const DST_LE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <= 1 RETURN c";
const DST_LT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k < 1 RETURN c";
const DST_GE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k >= 1 RETURN c";
const DST_GT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c";
const DST_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn two_hop_far_end_less_or_equal_includes_the_boundary_chain() {
    let ((), report) = run_async_under_lab(0x39_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-dst-le-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut r_seed = WriteBatch::new(R);
        r_seed.create_vertex(VId(1), vec![], vec![]);
        r_seed.create_vertex(VId(2), vec![], vec![]);
        r_seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
        r_seed.create_vertex(VId(4), vec![], vec![]);
        r_seed.create_vertex(VId(5), vec![], vec![]);
        r_seed.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(9))]);
        r_seed.create_vertex(VId(7), vec![], vec![]);
        r_seed.create_vertex(VId(8), vec![], vec![]);
        r_seed.create_vertex(VId(9), vec![], vec![]);
        r_seed.create_vertex(VId(10), vec![], vec![]);
        r_seed.create_vertex(VId(11), vec![], vec![]);
        r_seed.create_vertex(VId(12), vec![], vec![(K, CanonicalScalar::Int(0))]);
        r_seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        r_seed.add_edge(EId(11), VId(4), VId(5), vec![]);
        r_seed.add_edge(EId(12), VId(7), VId(8), vec![]);
        r_seed.add_edge(EId(13), VId(10), VId(11), vec![]);
        db.write(&commit, r_seed).await.expect("R chains commit");
        let mut s_seed = WriteBatch::new(S);
        s_seed.add_edge(EId(20), VId(2), VId(3), vec![]);
        s_seed.add_edge(EId(21), VId(5), VId(6), vec![]);
        s_seed.add_edge(EId(22), VId(8), VId(9), vec![]);
        s_seed.add_edge(EId(23), VId(11), VId(12), vec![]);
        db.write(&commit, s_seed).await.expect("S chains commit");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S)
            .with_property("k", K);

        let inclusive = db
            .execute_gql(DST_LE, &bind)
            .expect("far-end less-or-equal MATCH executes");
        assert_eq!(
            inclusive,
            vec![VId(3), VId(12)],
            "boundary AND below-boundary far ends answer"
        );
        assert!(
            inclusive.contains(&VId(3)),
            "the k=1 far end is IN: 1 <= 1 — a renamed < drops exactly this row"
        );
        assert!(
            !inclusive.contains(&VId(6)),
            "the k=9 far end is OUT: 9 <= 1 is false — a <> cheat admits it"
        );
        assert!(
            !inclusive.contains(&VId(9)),
            "the no-k far end satisfies no comparator at all"
        );

        assert_eq!(
            db.execute_gql(DST_LT, &bind)
                .expect("the strict-less sibling still executes"),
            vec![VId(12)],
            "nje.37 unmoved: <= is visibly the union of = and <"
        );
        assert_eq!(
            db.execute_gql(DST_GE, &bind)
                .expect("the greater-or-equal sibling still executes"),
            vec![VId(3), VId(6)],
            "nje.38 unmoved — and <= is not its mirror by accident: they \
             share exactly the boundary row"
        );
        assert_eq!(
            db.execute_gql(DST_GT, &bind)
                .expect("the strict-greater sibling still executes"),
            vec![VId(6)],
            "nje.36 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(DST_EQ, &bind)
                .expect("the equality sibling still executes"),
            vec![VId(3)],
            "nje.33 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered two-hop executes"),
            vec![VId(3), VId(6), VId(9), VId(12)],
            "without WHERE all four chains answer"
        );

        // Off-grammar edges: the C-style alias on c, and a WHERE on the
        // incoming chain.
        for off_grammar in [
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN a",
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
