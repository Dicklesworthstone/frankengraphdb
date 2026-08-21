//! **Incoming two-hop `WHERE a.k = 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.48`).
//!
//! The NEAR-END predicate on the incoming chain: in
//! `MATCH (a)<-[:R]-(b)<-[:S]-(c)` the stored edges run `b -R-> a` and
//! `c -S-> b`, so `a` is the `:R` edge's DEST in storage while the
//! projected `c` is the `:S` edge's SOURCE. The derivation composes
//! exactly that in plain code — for every `:R` edge whose dest carries
//! `k` as an Int equal to the literal, every `:S` edge arriving at that
//! `:R` edge's source contributes its own source, missing-`k` excluded
//! inside the derivation. Every far-end `c` in the fixture is keyless,
//! so `WHERE c.k = 1` answers the empty set — the separation between
//! filtering `a` and filtering `c`; the `k = 9` dest's chain and the
//! keyless dest's chain pin the equality and the exclusion, and reversed
//! storage defeats an outgoing walk.

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
const IN_TWO_SRC_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c";
const IN_TWO_DST_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c";

/// Far ends (`:S` sources) of incoming two-hop paths whose NEAR end — the
/// `:R` edge's stored DEST — carries `k` as an Int equal to 1. The
/// reversed composition AND the missing-key exclusion both live here, in
/// the derivation.
fn reference_far_ends_of_k_one_near_ends(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, first) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        let near_end_kept = graph.vertex(first.dst).is_some_and(|vertex| {
            matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v == 1)
        });
        if !near_end_kept {
            continue;
        }
        for (_, second) in graph
            .iter_edges()
            .filter(|(_, edge)| edge.relation == S && edge.dst == first.src)
        {
            rows.push(second.src);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn incoming_two_hop_near_end_filter_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x6b_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let rows;
        let far_end_rows;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
            let mut r_batch = WriteBatch::new(R);
            // A k=9 near end, stored reversed: 3 -S-> 2 -R-> 1{k:9}.
            r_batch.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(9))]);
            r_batch.create_vertex(VId(2), vec![], vec![]);
            r_batch.create_vertex(VId(3), vec![], vec![]);
            // The kept chain: near end 4{k:1}; 6 -S-> 5 -R-> 4.
            r_batch.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(1))]);
            r_batch.create_vertex(VId(5), vec![], vec![]);
            r_batch.create_vertex(VId(6), vec![], vec![]);
            // A keyless near end: 9 -S-> 8 -R-> 7.
            r_batch.create_vertex(VId(7), vec![], vec![]);
            r_batch.create_vertex(VId(8), vec![], vec![]);
            r_batch.create_vertex(VId(9), vec![], vec![]);
            r_batch.add_edge(EId(10), VId(2), VId(1), vec![]);
            r_batch.add_edge(EId(11), VId(5), VId(4), vec![]);
            r_batch.add_edge(EId(12), VId(8), VId(7), vec![]);
            db.write(&commit, r_batch).await.expect("R edges commit");
            let mut s_batch = WriteBatch::new(S);
            s_batch.add_edge(EId(20), VId(3), VId(2), vec![]);
            s_batch.add_edge(EId(21), VId(6), VId(5), vec![]);
            s_batch.add_edge(EId(22), VId(9), VId(8), vec![]);
            db.write(&commit, s_batch).await.expect("S edges commit");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_relation("S", S)
                .with_property("k", K);
            rows = db
                .execute_gql(IN_TWO_SRC_EQ, &bind)
                .expect("incoming two-hop WHERE a.k = 1 executes");
            far_end_rows = db
                .execute_gql(IN_TWO_DST_EQ, &bind)
                .expect("the far-end sibling still executes");
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
            reference_far_ends_of_k_one_near_ends(graph),
            "the engine equals the reversed near-end derivation"
        );
        assert_eq!(
            rows,
            vec![VId(6)],
            "only the k=1 near end's far end — an outgoing walk on this \
             reversed fixture composes nothing and answers []"
        );
        assert!(
            !rows.contains(&VId(3)),
            "the k=9 near end's far end fails the equality"
        );
        assert!(
            !rows.contains(&VId(9)),
            "the keyless near end's far end is OUT — missing k is not \
             k = 1"
        );
        assert!(
            far_end_rows.is_empty(),
            "every far-end c is keyless, so WHERE c.k = 1 answers [] — a \
             kernel filtering c instead of a disagrees on this fixture"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
