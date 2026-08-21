//! **Incoming two-hop `WHERE a.k < 9`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.51`).
//!
//! The less-than twin of `gql_incoming_two_hop_where_src_gt_oracle.rs`:
//! in `MATCH (a)<-[:R]-(b)<-[:S]-(c)` the stored edges run `b -R-> a`
//! and `c -S-> b`, so `a` is the `:R` edge's DEST in storage while the
//! projected `c` is the `:S` edge's SOURCE. The derivation composes
//! exactly that in plain code — for every `:R` edge whose dest carries
//! `k` as an Int strictly less than the literal, every `:S` edge
//! arriving at that `:R` edge's source contributes its own source,
//! missing-`k` excluded inside the derivation. The `k = 9` near end is
//! exactly the boundary (`9 < 9` false), the keyless near end pins the
//! exclusion, and reversed storage defeats an outgoing walk.

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
const IN_TWO_SRC_LT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k < 9 RETURN c";
const OUT_TWO_SRC_LT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k < 9 RETURN c";

/// Far ends (`:S` sources) of incoming two-hop paths whose NEAR end — the
/// `:R` edge's stored DEST — carries `k` as an Int strictly less than 9.
/// The reversed composition AND the missing-key exclusion both live here,
/// in the derivation.
fn reference_far_ends_of_lesser_near_ends(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, first) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        let near_end_kept = graph.vertex(first.dst).is_some_and(|vertex| {
            matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v < 9)
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
fn incoming_two_hop_near_end_less_than_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x6e_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-lt-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let rows;
        let outgoing_rows;
        {
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("database creates");
            let mut r_batch = WriteBatch::new(R);
            // A k=9 near end: the exact boundary, 9 < 9 is false.
            // Stored reversed: 3 -S-> 2 -R-> 1{k:9}.
            r_batch.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(9))]);
            r_batch.create_vertex(VId(2), vec![], vec![]);
            r_batch.create_vertex(VId(3), vec![], vec![]);
            // The kept chain: k=1 near end; 6 -S-> 5 -R-> 4{k:1}.
            r_batch.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(1))]);
            r_batch.create_vertex(VId(5), vec![], vec![]);
            r_batch.create_vertex(VId(6), vec![], vec![]);
            // A keyless near end: pins the missing-k exclusion.
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
                .execute_gql(IN_TWO_SRC_LT, &bind)
                .expect("incoming two-hop WHERE a.k < 9 executes");
            outgoing_rows = db
                .execute_gql(OUT_TWO_SRC_LT, &bind)
                .expect("the outgoing spelling still executes");
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
            reference_far_ends_of_lesser_near_ends(graph),
            "the engine equals the reversed near-end derivation"
        );
        assert_eq!(
            rows,
            vec![VId(6)],
            "only the k=1 near end's far end is strictly below 9"
        );
        assert!(
            !rows.contains(&VId(3)),
            "the k=9 near end's far end fails the exact boundary — a <= \
             in disguise answers [3, 6] and fails"
        );
        assert!(
            !rows.contains(&VId(9)),
            "the keyless near end's far end is OUT — missing k is not \
             k < 9"
        );
        assert!(
            outgoing_rows.is_empty(),
            "an outgoing walk composes nothing on this reversed fixture"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
