//! **Incoming two-hop `WHERE a.k != 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.54`).
//!
//! The C-style spelling on the incoming near end: `!=` must alias `<>`
//! exactly, so the engine's answer equals the SAME reversed derivation
//! as the `<>` oracle — in `MATCH (a)<-[:R]-(b)<-[:S]-(c)` the stored
//! edges run `b -R-> a` and `c -S-> b`, so `a` is the `:R` edge's DEST
//! in storage while the projected `c` is the `:S` edge's SOURCE. The
//! derivation keeps `:R` edges whose dest carries `k` as an Int NOT
//! equal to the literal, joins every `:S` edge arriving at that edge's
//! source, and projects its source, missing-`k` excluded inside the
//! derivation (never "trivially unequal"). Every far-end `c` is keyless
//! so `WHERE c.k != 1` answers the empty set, and the direction control
//! runs on the outgoing EQUALITY (already grammar) — the outgoing
//! near-end `!=` is a separate grammar slice and is not executed here.

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
const IN_TWO_SRC_BANG_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN c";
const IN_TWO_DST_BANG_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN c";
// The direction control runs on the OUTGOING equality, which is already
// grammar — the outgoing near-end != is a separate grammar slice.
const OUT_TWO_SRC_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";

/// Far ends (`:S` sources) of incoming two-hop paths whose NEAR end — the
/// `:R` edge's stored DEST — carries `k` as an Int different from 1: the
/// `<>` law verbatim, because `!=` is an alias and not a new comparator.
fn reference_far_ends_of_unequal_near_ends(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, first) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        let near_end_kept = graph.vertex(first.dst).is_some_and(
            |vertex| matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1),
        );
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
fn incoming_two_hop_near_end_bang_ne_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x71_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-bang-ne-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let rows;
        let far_end_rows;
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
            // The kept chain: k=9 near end, stored reversed:
            // 3 -S-> 2 -R-> 1{k:9}.
            r_batch.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(9))]);
            r_batch.create_vertex(VId(2), vec![], vec![]);
            r_batch.create_vertex(VId(3), vec![], vec![]);
            // A k=1 near end: fails the inequality. 6 -S-> 5 -R-> 4{k:1}.
            r_batch.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(1))]);
            r_batch.create_vertex(VId(5), vec![], vec![]);
            r_batch.create_vertex(VId(6), vec![], vec![]);
            // A keyless near end: what missing-as-unequal wrongly admits.
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
                .execute_gql(IN_TWO_SRC_BANG_NE, &bind)
                .expect("incoming two-hop WHERE a.k != 1 executes");
            far_end_rows = db
                .execute_gql(IN_TWO_DST_BANG_NE, &bind)
                .expect("the far-end sibling still executes");
            outgoing_rows = db
                .execute_gql(OUT_TWO_SRC_EQ, &bind)
                .expect("the outgoing equality spelling still executes");
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
            reference_far_ends_of_unequal_near_ends(graph),
            "the engine's != answer equals the <> derivation — an alias, \
             not a new comparator"
        );
        assert_eq!(
            rows,
            vec![VId(3)],
            "only the k=9 near end's far end answers, exactly as <> does"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the k=1 near end's far end fails the inequality"
        );
        assert!(
            !rows.contains(&VId(9)),
            "the keyless near end's far end is OUT — missing k is not \
             k != 1"
        );
        assert!(
            far_end_rows.is_empty(),
            "every far-end c is keyless, so WHERE c.k != 1 answers [] — a \
             kernel filtering c instead of a disagrees on this fixture"
        );
        assert!(
            outgoing_rows.is_empty(),
            "an outgoing walk (via the already-grammar equality) composes \
             nothing on this reversed fixture"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
