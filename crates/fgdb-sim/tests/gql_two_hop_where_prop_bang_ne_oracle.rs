//! **Two-hop `WHERE a.k != 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje-57-5jmh`).
//!
//! The C-style spelling on the outgoing two-hop ORIGIN: `!=` must alias
//! `<>` exactly, so the engine's answer equals the same plain-code
//! composition as the `<>` oracle — for every `:R` edge whose SOURCE
//! carries `k` as an Int NOT equal to the literal, every `:S` edge
//! leaving its dest contributes that edge's far end, missing-`k`
//! excluded inside the derivation (never "trivially unequal"). The
//! `k = 1` origin's fully composed path and the keyless origin's fully
//! composed path are both excluded, each by the derivation itself. The
//! hop-1 `a.k !=` spelling is deliberately NOT executed — it stays a
//! separate grammar slice.

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
const TWO_SRC_BANG_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN c";

/// Far ends of two-hop paths whose hop-1 ORIGIN carries `k` as an Int
/// different from 1 — the `<>` law verbatim, because `!=` is an alias
/// and not a new comparator: the composition AND the missing-key
/// exclusion both live here, in the derivation.
fn reference_far_ends_of_unequal_origins(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, first) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        let origin_kept = graph.vertex(first.src).is_some_and(|vertex| {
            matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1)
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
fn two_hop_origin_bang_ne_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x74_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-prop-bang-ne-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let rows;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
            let mut r_batch = WriteBatch::new(R);
            // A k=1 origin with a fully composed path: 1 -R-> 2 -S-> 3.
            r_batch.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            r_batch.create_vertex(VId(2), vec![], vec![]);
            r_batch.create_vertex(VId(3), vec![], vec![]);
            // The kept chain: k=9 origin; 4 -R-> 5 -S-> 6.
            r_batch.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
            r_batch.create_vertex(VId(5), vec![], vec![]);
            r_batch.create_vertex(VId(6), vec![], vec![]);
            // A keyless origin with a fully composed path: 7 -R-> 8 -S-> 9.
            r_batch.create_vertex(VId(7), vec![], vec![]);
            r_batch.create_vertex(VId(8), vec![], vec![]);
            r_batch.create_vertex(VId(9), vec![], vec![]);
            r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            r_batch.add_edge(EId(11), VId(4), VId(5), vec![]);
            r_batch.add_edge(EId(12), VId(7), VId(8), vec![]);
            db.write(&commit, r_batch).await.expect("R edges commit");
            let mut s_batch = WriteBatch::new(S);
            s_batch.add_edge(EId(20), VId(2), VId(3), vec![]);
            s_batch.add_edge(EId(21), VId(5), VId(6), vec![]);
            s_batch.add_edge(EId(22), VId(8), VId(9), vec![]);
            db.write(&commit, s_batch).await.expect("S edges commit");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_relation("S", S)
                .with_property("k", K);
            rows = db
                .execute_gql(TWO_SRC_BANG_NE, &bind)
                .expect("two-hop WHERE a.k != 1 executes");
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
            rows,
            reference_far_ends_of_unequal_origins(graph),
            "the engine's != answer equals the <> derivation — an alias, \
             not a new comparator"
        );
        assert_eq!(
            rows,
            vec![VId(6)],
            "only the k=9 origin's far end answers, exactly as <> does"
        );
        assert!(
            !rows.contains(&VId(3)),
            "the k=1 origin's fully composed far end fails the inequality"
        );
        assert!(
            !rows.contains(&VId(9)),
            "the keyless origin's fully composed far end is OUT — missing \
             k is not k != 1, even across two hops"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
