//! **Incoming `WHERE a.k != 1 AND b.m != 9` — the all-C-style conjunction
//! aliases `<>`** (`fgdb-ysm0`).
//!
//! The both-end conjunction crosses to the INCOMING spelling: under
//! `(a)<-[:R]-(b)` the flow is `b -R-> a`, so `a` is the stored dest
//! (carrying `k`) and `b` the stored source (carrying `m`), and
//! `RETURN b` answers the surviving writers. Both conjuncts in the
//! C-style spelling filter exactly like the all-`<>` conjunction, so
//! beside the literal `[7]` pin the all-`!=` rows are asserted EQUAL to
//! the all-`<>` rows themselves — one comparator, two spellings, at both
//! ends of the reversed flow, under the missing-is-OUT law at EACH end:
//! the keyless dest's chain and the keyless source's chain are both
//! barred by name. Swapping the conjunct order changes nothing, and one
//! refusal holds the grammar's edge: a SAME-end conjunction stays typed
//! Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const M: PropertyKeyId = PropertyKeyId(9);
const IN_BOTH_BANG: &str = "MATCH (a)<-[:R]-(b) WHERE a.k != 1 AND b.m != 9 RETURN b";
const IN_BOTH_BANG_REVERSED: &str = "MATCH (a)<-[:R]-(b) WHERE b.m != 9 AND a.k != 1 RETURN b";
const IN_BOTH_ANGLE: &str = "MATCH (a)<-[:R]-(b) WHERE a.k <> 1 AND b.m <> 9 RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_both_end_all_bang_conjunction_aliases_the_angle_spelling() {
    let ((), report) = run_async_under_lab(0x70_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-where-both-prop-both-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        // Writers carry m, dests carry k; each edge is one b -R-> a chain.
        seed.create_vertex(VId(1), vec![], vec![(M, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(2), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(3), vec![], vec![(M, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(5), vec![], vec![(M, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(7), vec![], vec![(M, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(8), vec![], vec![(K, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(9), vec![], vec![(M, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(10), vec![], vec![]);
        seed.create_vertex(VId(11), vec![], vec![]);
        seed.create_vertex(VId(12), vec![], vec![(K, CanonicalScalar::Int(0))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        seed.add_edge(EId(13), VId(7), VId(8), vec![]);
        seed.add_edge(EId(14), VId(9), VId(10), vec![]);
        seed.add_edge(EId(15), VId(11), VId(12), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K)
            .with_property("m", M);

        let bang = db
            .execute_gql(IN_BOTH_BANG, &bind)
            .expect("incoming all-C-style both-end conjunction MATCH executes");
        assert_eq!(
            bang,
            vec![VId(7)],
            "only the m=0 writer onto the k=0 dest survives both conjuncts"
        );
        assert!(
            !bang.contains(&VId(9)),
            "the keyless DEST's writer is OUT: missing k is not k != 1"
        );
        assert!(
            !bang.contains(&VId(11)),
            "the keyless WRITER is OUT: missing m is not m != 9"
        );
        assert_eq!(
            db.execute_gql(IN_BOTH_BANG_REVERSED, &bind)
                .expect("reversed conjunct order executes"),
            vec![VId(7)],
            "conjunct order is not semantics"
        );
        assert_eq!(
            bang,
            db.execute_gql(IN_BOTH_ANGLE, &bind)
                .expect("the all-<> sibling still executes"),
            "!= and <> are one comparator in two spellings on the reversed \
             flow"
        );

        // The refusal: a same-end conjunction stays typed Parse.
        let same_end = db
            .execute_gql(
                "MATCH (a)<-[:R]-(b) WHERE a.k != 1 AND a.m != 9 RETURN b",
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
