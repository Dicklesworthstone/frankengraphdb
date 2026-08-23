//! **`WHERE a.k <> 1 AND b.m <> 9` — both-end inequality**
//! (`fgdb-w5-parsers-nje.17`).
//!
//! The conjunction filters both endpoints independently, each under the
//! missing-is-OUT law: the four `{k∈{1,0}} × {m∈{9,0}}` edges leave exactly
//! one survivor, the `9→10` edge with no `k` proves a missing source
//! property excludes rather than trivially satisfying `<>`, and swapping
//! the conjunct order changes nothing. Mixed `<>`/`=` conjunctions are
//! nje.18 grammar (sibling lock `[6]`). The C-style `!=` stays off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const M: PropertyKeyId = PropertyKeyId(8);
const BOTH_NE: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m <> 9 RETURN b";
const BOTH_NE_REVERSED: &str = "MATCH (a)-[:R]->(b) WHERE b.m <> 9 AND a.k <> 1 RETURN b";
const SOURCE_ONLY: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn both_end_inequality_composes_and_missing_k_is_out() {
    let ((), report) = run_async_under_lab(0x4e_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-both-prop-ne-{}",
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
        assert_eq!(
            db.execute_gql(BOTH_NE, &bind)
                .expect("both-end inequality MATCH executes"),
            vec![VId(8)],
            "only the k=0 source with the m=0 dest survives both conjuncts"
        );
        assert_eq!(
            db.execute_gql(BOTH_NE_REVERSED, &bind)
                .expect("reversed conjunct order executes"),
            vec![VId(8)],
            "conjunct order is not semantics"
        );
        assert_eq!(
            db.execute_gql(SOURCE_ONLY, &bind)
                .expect("source-only inequality executes"),
            vec![VId(6), VId(8)],
            "the no-k source (9) is OUT, not trivially unequal — 10 absent"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(2), VId(4), VId(6), VId(8), VId(10)]
        );

        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m = 9 RETURN b",
                &bind,
            )
            .expect("mixed <>/= is nje.18 grammar, not a Parse"),
            vec![VId(6)],
            "nje.18 sibling lock: k<>1 AND m=9 keeps dest 6"
        );
        // The nje.57+ tranche landed the C-style != alias in AND
        // conjunctions, so the spelling now answers like its <> sibling:
        // chains 1 and 2 fail a.k != 1, chain 3 fails b.m <> 9, and only
        // chain 4 (k=0 -> m=0) survives.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b) WHERE a.k != 1 AND b.m <> 9 RETURN b",
                &bind,
            )
            .expect("the landed C-style != conjunction executes"),
            vec![VId(8)],
            "the k=0 to m=0 chain is the sole survivor"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
