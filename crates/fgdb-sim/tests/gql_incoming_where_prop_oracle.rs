//! **Incoming `WHERE b.k`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.19`).
//!
//! Under the flipped arrow `(a)<-[:R]-(b)`, `b` is the edge's ORIGIN and
//! `a` its destination — so `WHERE b.k` must filter on the edge SOURCE's
//! props while `RETURN a` projects edge destinations. The derivation says
//! exactly that in plain code (filter `edge.src`'s `k`, project
//! `edge.dst`), and the two spellings answer disjoint singletons on the
//! shared fixture: equality `[2]`, inequality `[4]` with the keyless
//! origin's row excluded inside the derivation. A kernel that reads the
//! flipped variables as outgoing — filtering the destination's props or
//! projecting origins — answers the wrong side and fails both.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_EQ: &str = "MATCH (a)<-[:R]-(b) WHERE b.k = 1 RETURN a";
const IN_NE: &str = "MATCH (a)<-[:R]-(b) WHERE b.k <> 1 RETURN a";

/// Destinations of `:R` edges whose ORIGIN carries `k` as an `Int` the
/// comparator keeps — missing key out, inside the derivation.
fn reference_destinations(
    graph: &ReferenceGraph,
    keeps: impl Fn(i64) -> bool,
) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.src).is_some_and(|vertex| {
                matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if keeps(*v))
            })
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn incoming_origin_property_filters_equal_their_reference() {
    let ((), report) = run_async_under_lab(0x46_19, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-where-prop-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let eq_rows;
        let ne_rows;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
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
            eq_rows = db
                .execute_gql(IN_EQ, &bind)
                .expect("incoming WHERE b.k = 1 executes");
            ne_rows = db
                .execute_gql(IN_NE, &bind)
                .expect("incoming WHERE b.k <> 1 executes");
        }

        let keys = CapsuleKeys::new(
            [0x5a; 32],
            namespace,
            [0x3c; 32],
            CAPSULE_OBJECT_KIND,
            CapsuleProfile::balanced(),
        );
        let coordinator = CommitCoordinator::open(&commit, &dir, keys)
            .await
            .expect("independent coordinator opens");
        let reference = replay(&commit, &coordinator)
            .await
            .expect("durable stream replays")
            .database;
        let graph = reference
            .graph(GraphId(1), BranchId(1))
            .expect("reference graph exists");

        assert_eq!(
            eq_rows,
            reference_destinations(graph, |k| k == 1),
            "incoming equality equals its derivation"
        );
        assert_eq!(
            eq_rows,
            vec![VId(2)],
            "the k=1 ORIGIN's destination answers — a dest-side filter finds \
             no k anywhere and answers nothing, an origin projection answers \
             1 instead of 2"
        );
        assert_eq!(
            ne_rows,
            reference_destinations(graph, |k| k != 1),
            "incoming inequality equals its derivation"
        );
        assert_eq!(
            ne_rows,
            vec![VId(4)],
            "the k=9 origin's destination alone: the keyless origin's row is \
             excluded — missing k is not k <> 1"
        );
        assert!(
            !ne_rows.contains(&VId(6)),
            "the keyless origin's destination satisfies neither predicate"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
