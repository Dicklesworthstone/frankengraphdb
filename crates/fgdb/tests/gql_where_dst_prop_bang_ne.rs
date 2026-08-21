//! **`WHERE b.k != 1` aliases `<>` on the hop-1 dest**
//! (`fgdb-w5-parsers-nje.47`).
//!
//! The C-style spelling of `gql_where_dst_prop_ne.rs`: `!=` binds the DEST
//! variable exactly like `<>`, so `RETURN a` answers the same sources on
//! the same fixture — the two spellings are asserted equal against each
//! other, not just against the literal list. The planted negative is the
//! propertyless dest `6`: an executor that treats a missing `k` as
//! "trivially unequal" answers `[3, 5]` and fails the exact-equality here —
//! missing is OUT. Equality beside it and the unfiltered statement re-pin
//! the frame, and the hop-2 `!=` spelling stays the typed Parse refusal.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const BANG_NE: &str = "MATCH (a)-[:R]->(b) WHERE b.k != 1 RETURN a";
const ANGLE_NE: &str = "MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a";
const EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN a";
const HOP2_BANG_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn destination_property_bang_inequality_aliases_the_angle_spelling() {
    let ((), report) = run_async_under_lab(0x47_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-dst-prop-bang-ne-{}",
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
            .execute_gql(BANG_NE, &bind)
            .expect("destination C-style inequality MATCH executes");
        assert_eq!(
            bang,
            vec![VId(3)],
            "only the k=9 dest's source passes — an executor treating the \
             missing-k dest as trivially unequal answers [3, 5] and fails here"
        );
        assert!(
            !bang.contains(&VId(5)),
            "the propertyless dest's source is OUT, not trivially unequal"
        );
        assert_eq!(
            bang,
            db.execute_gql(ANGLE_NE, &bind)
                .expect("the <> sibling still executes"),
            "!= and <> are the same predicate spelled twice on this fixture"
        );
        assert_eq!(
            db.execute_gql(EQUAL, &bind)
                .expect("destination-equality MATCH executes"),
            vec![VId(1)],
            "equality beside it still answers its own source"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(1), VId(3), VId(5)]
        );

        // The hop-2 C-style spelling is NOT graduated by the hop-1 alias.
        let hop2 = db
            .execute_gql(HOP2_BANG_NE, &bind)
            .expect_err("the hop-2 != spelling is outside the grammar");
        assert!(
            matches!(hop2, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {hop2:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
