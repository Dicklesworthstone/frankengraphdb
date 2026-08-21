//! **Two-hop `WHERE a.k`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.21`).
//!
//! The source predicate composed over two hops: `RETURN c` answers the
//! far ends of paths whose hop-1 ORIGIN carries `k` per the comparator,
//! and the derivation composes exactly that in plain code — for every
//! `:R` edge with a kept origin, every `:S` continuation from its dest
//! contributes its far end. The two spellings answer disjoint singletons
//! on separate vias (`[4]` equality, `[6]` inequality), the keyless
//! origin's fully composed path is excluded inside the derivation, and a
//! kept origin whose hop has no `:S` continuation composes nothing — so a
//! kernel that filters the via or the far end instead of the ORIGIN, or
//! one that treats missing `k` as `<>`, disagrees with the derivation
//! somewhere on this fixture.

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
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const TWO_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";
const TWO_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k <> 1 RETURN c";

/// Far ends of two-hop paths whose hop-1 origin carries `k` as an `Int`
/// the comparator keeps — the composition AND the missing-key exclusion
/// both live here, in the derivation.
fn reference_far_ends(graph: &ReferenceGraph, keeps: impl Fn(i64) -> bool) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, first) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        let origin_kept = graph.vertex(first.src).is_some_and(|vertex| {
            matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if keeps(*v))
        });
        if !origin_kept {
            continue;
        }
        for (_, second) in graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == S && edge.src == first.dst)
        {
            rows.push(second.dst);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn two_hop_origin_property_filters_equal_their_reference() {
    let ((), report) = run_async_under_lab(0x48_21, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-prop-oracle-{}",
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
            let mut r_batch = WriteBatch::new(R);
            // EQ's path: k=1 origin, via 2, far end 4.
            r_batch.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            r_batch.create_vertex(VId(2), vec![], vec![]);
            r_batch.create_vertex(VId(4), vec![], vec![]);
            // NE's path: k=9 origin, its OWN via 5, far end 6.
            r_batch.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
            r_batch.create_vertex(VId(5), vec![], vec![]);
            r_batch.create_vertex(VId(6), vec![], vec![]);
            // Keyless origin with a fully composed path: out of both.
            r_batch.create_vertex(VId(7), vec![], vec![]);
            r_batch.create_vertex(VId(8), vec![], vec![]);
            r_batch.create_vertex(VId(9), vec![], vec![]);
            // Kept origin whose hop never continues: composes nothing.
            r_batch.create_vertex(VId(10), vec![], vec![(K, CanonicalScalar::Int(1))]);
            r_batch.create_vertex(VId(11), vec![], vec![]);
            r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            r_batch.add_edge(EId(11), VId(3), VId(5), vec![]);
            r_batch.add_edge(EId(12), VId(7), VId(8), vec![]);
            r_batch.add_edge(EId(13), VId(10), VId(11), vec![]);
            db.write(&commit, r_batch).await.expect("R edges commit");
            let mut s_batch = WriteBatch::new(S);
            s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
            s_batch.add_edge(EId(21), VId(5), VId(6), vec![]);
            s_batch.add_edge(EId(22), VId(8), VId(9), vec![]);
            db.write(&commit, s_batch).await.expect("S edges commit");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_relation("S", S)
                .with_property("k", K);
            eq_rows = db
                .execute_gql(TWO_EQ, &bind)
                .expect("two-hop WHERE a.k = 1 executes");
            ne_rows = db
                .execute_gql(TWO_NE, &bind)
                .expect("two-hop WHERE a.k <> 1 executes");
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
            reference_far_ends(graph, |k| k == 1),
            "two-hop equality equals its composed derivation"
        );
        assert_eq!(
            eq_rows,
            vec![VId(4)],
            "the k=1 origin's far end alone — the continuation-less k=1 \
             origin at 10 composes nothing"
        );
        assert_eq!(
            ne_rows,
            reference_far_ends(graph, |k| k != 1),
            "two-hop inequality equals its composed derivation"
        );
        assert_eq!(
            ne_rows,
            vec![VId(6)],
            "the k=9 origin's far end alone"
        );
        assert!(
            !eq_rows.contains(&VId(9)) && !ne_rows.contains(&VId(9)),
            "the keyless origin's fully composed far end satisfies NEITHER \
             predicate — missing k is not k <> 1, even across two hops"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
