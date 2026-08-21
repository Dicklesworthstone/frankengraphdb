//! **`WHERE a.k <> 1 AND b.m != 9` — the C-style dest conjunct aliases `<>`**
//! (`fgdb-w5-parsers-nje.57`, nje.65 slice).
//!
//! The C-style spelling on the DEST conjunct of the both-end conjunction:
//! `!=` on `b.m` filters exactly like `<>`, so beside the literal `[8]`
//! pin the `!=` rows are asserted EQUAL to the all-`<>` rows themselves —
//! one comparator, two spellings, under the same missing-is-OUT law (the
//! `9→10` edge's keyless source excludes rather than trivially satisfying
//! the source conjunct). Swapping the conjunct order changes nothing, the
//! mixed `=`/`!=` conjunction answers the nje.18 sibling lock `[4]`, and
//! one refusal holds the grammar's edge: a SAME-end conjunction stays
//! typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const M: PropertyKeyId = PropertyKeyId(8);
const DST_BANG_BOTH: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m != 9 RETURN b";
const DST_BANG_BOTH_REVERSED: &str = "MATCH (a)-[:R]->(b) WHERE b.m != 9 AND a.k <> 1 RETURN b";
const ANGLE_BOTH: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m <> 9 RETURN b";
const EQ_BANG_MIXED: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m != 9 RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn both_end_dest_bang_inequality_aliases_the_angle_spelling() {
    let ((), report) = run_async_under_lab(0x65_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-both-prop-dst-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(M, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(4), vec![], vec![(M, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(6), vec![], vec![(M, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(7), vec![], vec![(K, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(8), vec![], vec![(M, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(9), vec![], vec![]);
        seed.create_vertex(VId(10), vec![], vec![(M, CanonicalScalar::Int(0))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        seed.add_edge(EId(13), VId(7), VId(8), vec![]);
        seed.add_edge(EId(14), VId(9), VId(10), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K)
            .with_property("m", M);

        let bang = db
            .execute_gql(DST_BANG_BOTH, &bind)
            .expect("both-end dest C-style conjunction MATCH executes");
        assert_eq!(
            bang,
            vec![VId(8)],
            "only the k=0 source with the m=0 dest survives both conjuncts \
             — the keyless source (9) is OUT, not trivially unequal"
        );
        assert_eq!(
            db.execute_gql(DST_BANG_BOTH_REVERSED, &bind)
                .expect("reversed conjunct order executes"),
            vec![VId(8)],
            "conjunct order is not semantics"
        );
        assert_eq!(
            bang,
            db.execute_gql(ANGLE_BOTH, &bind)
                .expect("the all-<> sibling still executes"),
            "!= and <> are one comparator in two spellings inside the AND"
        );

        assert_eq!(
            db.execute_gql(EQ_BANG_MIXED, &bind)
                .expect("mixed =/!= executes, not a Parse"),
            vec![VId(4)],
            "nje.18 sibling lock: k=1 AND m!=9 keeps dest 4 — the m=9 dest \
             (2) fails the dest conjunct"
        );

        // The refusal: a same-end conjunction stays typed Parse.
        let same_end = db
            .execute_gql(
                "MATCH (a)-[:R]->(b) WHERE b.k <> 1 AND b.m != 9 RETURN b",
                &bind,
            )
            .expect_err("a same-end conjunction is outside the grammar");
        assert!(
            matches!(same_end, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {same_end:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
