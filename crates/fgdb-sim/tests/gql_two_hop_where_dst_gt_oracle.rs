//! **Two-hop `WHERE c.k > 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.36`).
//!
//! The ordered twin of `gql_two_hop_where_dst_ne_oracle.rs`: `RETURN c`
//! answers the far ends of `:R`-then-`:S` paths whose hop-2 destination
//! carries `k` as an Int strictly greater than the literal, and the
//! derivation composes exactly that in plain code — missing-`k` excluded
//! inside it. Four chains: the `k = 1` far end separates `>` from `>=`,
//! the `k = 0` far end separates it from `<>` (an inequality in disguise
//! would admit 12), the keyless far end pins the exclusion, and every
//! hop-1 origin is keyless so a source-side filter answers the empty set
//! — each cheat breaks the oracle equality, not merely a pinned vector.

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
const TWO_DST_GT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c";

/// Far ends of two-hop paths that themselves carry `k` as an Int strictly
/// greater than 1 — the composition AND the missing-key exclusion both
/// live here, in the derivation.
fn reference_far_ends_with_k_greater_than_one(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, first) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        for (_, second) in graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == S && edge.src == first.dst)
        {
            let far_end_kept = graph.vertex(second.dst).is_some_and(
                |vertex| matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v > 1),
            );
            if far_end_kept {
                rows.push(second.dst);
            }
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn two_hop_far_end_greater_than_filter_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x60_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-dst-gt-oracle-{}",
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
            // A k=1 far end: the boundary that separates > from >=.
            r_batch.create_vertex(VId(1), vec![], vec![]);
            r_batch.create_vertex(VId(2), vec![], vec![]);
            r_batch.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
            // The kept path: keyless origin 4, via 5, k=9 far end 6.
            r_batch.create_vertex(VId(4), vec![], vec![]);
            r_batch.create_vertex(VId(5), vec![], vec![]);
            r_batch.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(9))]);
            // A keyless far end: pins the missing-k exclusion.
            r_batch.create_vertex(VId(7), vec![], vec![]);
            r_batch.create_vertex(VId(8), vec![], vec![]);
            r_batch.create_vertex(VId(9), vec![], vec![]);
            // A k=0 far end: what a <> in disguise would admit.
            r_batch.create_vertex(VId(10), vec![], vec![]);
            r_batch.create_vertex(VId(11), vec![], vec![]);
            r_batch.create_vertex(VId(12), vec![], vec![(K, CanonicalScalar::Int(0))]);
            r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            r_batch.add_edge(EId(11), VId(4), VId(5), vec![]);
            r_batch.add_edge(EId(12), VId(7), VId(8), vec![]);
            r_batch.add_edge(EId(13), VId(10), VId(11), vec![]);
            db.write(&commit, r_batch).await.expect("R edges commit");
            let mut s_batch = WriteBatch::new(S);
            s_batch.add_edge(EId(20), VId(2), VId(3), vec![]);
            s_batch.add_edge(EId(21), VId(5), VId(6), vec![]);
            s_batch.add_edge(EId(22), VId(8), VId(9), vec![]);
            s_batch.add_edge(EId(23), VId(11), VId(12), vec![]);
            db.write(&commit, s_batch).await.expect("S edges commit");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_relation("S", S)
                .with_property("k", K);
            rows = db
                .execute_gql(TWO_DST_GT, &bind)
                .expect("two-hop WHERE c.k > 1 executes");
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
            reference_far_ends_with_k_greater_than_one(graph),
            "the engine equals the composed far-end derivation"
        );
        assert_eq!(
            rows,
            vec![VId(6)],
            "only the k=9 far end — every origin is keyless, so a kernel \
             filtering a.k instead of c.k answers [] and fails"
        );
        assert!(
            !rows.contains(&VId(3)),
            "the k=1 far end fails strict greater-than — a >= in disguise \
             answers [3, 6] and fails"
        );
        assert!(
            !rows.contains(&VId(12)),
            "the k=0 far end fails > — a <> in disguise answers [6, 12]"
        );
        assert!(
            !rows.contains(&VId(9)),
            "the keyless far end is OUT — missing k is not k > 1, even \
             across two hops"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
