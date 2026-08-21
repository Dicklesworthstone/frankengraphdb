//! **Hop-1 `WHERE a.k != 1` aliases the source `<>`**
//! (`fgdb-w5-parsers-nje-58-vdke`).
//!
//! The C-style spelling on the one-hop SOURCE: `!=` must alias `<>`
//! exactly, so beside the literal `[4]` pin the `!=` rows are asserted
//! EQUAL to the `<>` rows themselves — one comparator, two spellings.
//! On the nje.15 fixture the `k = 1` source fails the inequality and
//! the propertyless source is OUT, not "trivially unequal" — which is
//! what the `5→6` edge separates. The equality sibling and the
//! unfiltered statement are re-pinned, and the direction control
//! executes the incoming one-hop `!=` (nje.47-family grammar), which
//! reads the dest-side cell and answers by its own law rather than
//! parroting the source filter.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const BANG_NE: &str = "MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b";
const NOT_EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";
const EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const DST_BANG_NE: &str = "MATCH (a)-[:R]->(b) WHERE b.k != 1 RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn source_bang_ne_aliases_the_diamond_spelling() {
    let ((), report) = run_async_under_lab(0x58_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(4), vec![], vec![]);
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
            .expect("source != MATCH executes");
        assert_eq!(
            bang,
            vec![VId(4)],
            "only the k=9 source passes: k=1 fails != and the \
             propertyless source is out, not trivially unequal"
        );
        assert!(
            !bang.contains(&VId(6)),
            "the missing-k source's dest must not leak in under !="
        );

        // The alias law itself: != rows equal <> rows, not merely the
        // same literal list.
        assert_eq!(
            bang,
            db.execute_gql(NOT_EQUAL, &bind)
                .expect("the diamond sibling still executes"),
            "!= and <> are one comparator in two spellings"
        );

        assert_eq!(
            db.execute_gql(EQUAL, &bind)
                .expect("the equality sibling still executes"),
            vec![VId(2)],
            "equality beside it still answers its own dest"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(2), VId(4), VId(6)]
        );

        // The cell control: the dest-side != (nje.47 grammar) reads b and
        // projects sources — no dest carries k on this fixture, so it
        // answers [] where a variable-blind kernel would parrot [4].
        assert!(
            db.execute_gql(DST_BANG_NE, &bind)
                .expect("the dest-side != spelling still executes")
                .is_empty(),
            "no dest carries k on this fixture — the source and dest \
             cells stay separated under !="
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
