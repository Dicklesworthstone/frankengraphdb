//! **Undirected `WHERE a.k`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.20`).
//!
//! An undirected match binds each edge twice, once per orientation, and
//! the predicate rides the binding: `WHERE a.k = 1 RETURN b` answers the
//! OTHER endpoint of every binding whose `a`-side carries `k = 1`. The
//! derivation walks both orientations of every edge in plain code — src
//! carrier pushes dst, dst carrier pushes src — so the flipped-orientation
//! row (a `k = 1` carrier sitting on the edge's DEST side answering its
//! SOURCE) is derived, not assumed: an outgoing-only kernel wearing the
//! undirected spelling misses exactly that row. Missing `k` is excluded
//! inside the derivation, and the `<>` twin is pinned beside the equality
//! against the same comparator-parameterized helper.

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
const UN_EQ: &str = "MATCH (a)-[:R]-(b) WHERE a.k = 1 RETURN b";
const UN_NE: &str = "MATCH (a)-[:R]-(b) WHERE a.k <> 1 RETURN b";

/// Other endpoints of every `:R` edge binding whose `a`-side carries `k`
/// as an `Int` the comparator keeps — BOTH orientations walked, missing
/// key out, inside the derivation.
fn reference_other_endpoints(
    graph: &ReferenceGraph,
    keeps: impl Fn(i64) -> bool,
) -> Vec<VId> {
    let carrier = |vid: VId| {
        graph.vertex(vid).is_some_and(|vertex| {
            matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if keeps(*v))
        })
    };
    let mut rows = Vec::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        if carrier(edge.src) {
            rows.push(edge.dst);
        }
        if carrier(edge.dst) {
            rows.push(edge.src);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn undirected_origin_property_filters_equal_their_reference() {
    let ((), report) = run_async_under_lab(0x47_20, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-where-prop-oracle-{}",
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
            // Carrier on the edge SOURCE side.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![]);
            // Carrier on the edge DEST side: the flipped-orientation witness.
            seed.create_vertex(VId(3), vec![], vec![]);
            seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(1))]);
            // The inequality's carrier.
            seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(6), vec![], vec![]);
            // Keyless on both ends: out of both statements.
            seed.create_vertex(VId(7), vec![], vec![]);
            seed.create_vertex(VId(8), vec![], vec![]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(4), vec![]);
            seed.add_edge(EId(12), VId(5), VId(6), vec![]);
            seed.add_edge(EId(13), VId(7), VId(8), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K);
            eq_rows = db
                .execute_gql(UN_EQ, &bind)
                .expect("undirected WHERE a.k = 1 executes");
            ne_rows = db
                .execute_gql(UN_NE, &bind)
                .expect("undirected WHERE a.k <> 1 executes");
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
            reference_other_endpoints(graph, |k| k == 1),
            "undirected equality equals its both-orientation derivation"
        );
        assert_eq!(
            eq_rows,
            vec![VId(2), VId(3)],
            "the source-side carrier answers 2 AND the dest-side carrier \
             answers 3 — an outgoing-only kernel misses 3"
        );
        assert_eq!(
            ne_rows,
            reference_other_endpoints(graph, |k| k != 1),
            "undirected inequality equals its derivation"
        );
        assert_eq!(
            ne_rows,
            vec![VId(6)],
            "only the k=9 carrier's other endpoint: the k=1 carriers fail \
             the inequality and the keyless pair satisfies neither predicate"
        );
        assert!(
            !eq_rows.contains(&VId(8)) && !ne_rows.contains(&VId(8)),
            "the keyless edge contributes to no answer"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
