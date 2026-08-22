//! **Mixed `=`/`<>` conjunctions keep the matching dest**
//! (`fgdb-w5-parsers-nje.18`).
//!
//! The k/m quadrant fixture gives every conjunction shape exactly one
//! survivor, so the four spellings — `=∧=`, `=∧<>`, `<>∧=`, `<>∧<>` — must
//! answer four DIFFERENT single dests: a kernel that ignored one operator
//! collapses two of them onto the same row and fails loudly. Two predicates
//! on the SAME variable stay off-grammar: the bounded conjunction is one
//! source predicate and one dest predicate.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const M: PropertyKeyId = PropertyKeyId(8);
const EQ_NE: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m <> 9 RETURN b";
const NE_EQ: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m = 9 RETURN b";
const EQ_EQ: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m = 9 RETURN b";
const NE_NE: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m <> 9 RETURN b";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn mixed_operator_conjunctions_each_keep_their_own_dest() {
    let ((), report) = run_async_under_lab(0x4f_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-where-mixed-prop-{}", std::process::id()));
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
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        seed.add_edge(EId(13), VId(7), VId(8), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K)
            .with_property("m", M);
        assert_eq!(
            db.execute_gql(EQ_NE, &bind)
                .expect("= AND <> MATCH executes"),
            vec![VId(4)],
            "k=1 source, m!=9 dest: the =/<> quadrant"
        );
        assert_eq!(
            db.execute_gql(NE_EQ, &bind)
                .expect("<> AND = MATCH executes"),
            vec![VId(6)],
            "k!=1 source, m=9 dest: the <>/= quadrant"
        );
        assert_eq!(
            db.execute_gql(EQ_EQ, &bind)
                .expect("= AND = MATCH executes"),
            vec![VId(2)],
            "the both-equality quadrant still answers its own dest"
        );
        assert_eq!(
            db.execute_gql(NE_NE, &bind)
                .expect("<> AND <> MATCH executes"),
            vec![VId(8)],
            "the both-inequality quadrant still answers its own dest"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(2), VId(4), VId(6), VId(8)]
        );

        let same_side = db
            .execute_gql(
                "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND a.m <> 9 RETURN b",
                &bind,
            )
            .expect_err("two predicates on one variable are outside the grammar");
        assert!(
            matches!(same_side, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {same_side:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
