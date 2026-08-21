//! **`WHERE a.k <> 1` as-of does not see the later non-equal source**
//! (`fgdb-w5-parsers-nje.15`).
//!
//! The epoch face of the property inequality: at the first sequence only a
//! `k = 1` source exists, so the predicate answers EMPTY — the later `k = 9`
//! source and its destination belong to the second epoch, and the pinned
//! call must not see them while the live call must. The pairing catches an
//! as-of face that filters properties at the live frontier instead of the
//! asked sequence.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const NOT_EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn where_prop_ne_as_of_excludes_the_later_source() {
    let ((), report) = run_async_under_lab(0x4c_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-ne-as-of-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, first).await.expect("first epoch commits");
        let s1 = db.frontier().expect("reads S1");

        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
        second.create_vertex(VId(4), vec![], vec![]);
        second.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&commit, second)
            .await
            .expect("later non-equal source commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K);
        let pinned = db
            .execute_gql_at(NOT_EQUAL, &bind, s1)
            .expect("S1 inequality MATCH executes");
        let live = db
            .execute_gql(NOT_EQUAL, &bind)
            .expect("live inequality MATCH executes");

        assert!(
            pinned.is_empty(),
            "at S1 the only source has k = 1, so the predicate answers \
             nothing: {pinned:?}"
        );
        assert!(
            !pinned.contains(&VId(4)),
            "the later epoch's destination must be invisible at S1"
        );
        assert_eq!(
            live,
            vec![VId(4)],
            "live, the k = 9 source's destination answers and the k = 1 \
             source's stays filtered"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
