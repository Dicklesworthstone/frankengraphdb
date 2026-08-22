//! **Incoming two-hop `WHERE a.k <= 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.53`).
//!
//! The inclusive-lower twin of `gql_incoming_two_hop_where_src_ge_oracle.rs`:
//! in `MATCH (a)<-[:R]-(b)<-[:S]-(c)` the stored edges run `b -R-> a`
//! and `c -S-> b`, so `a` is the `:R` edge's DEST in storage while the
//! projected `c` is the `:S` edge's SOURCE. The derivation composes
//! exactly that in plain code — for every `:R` edge whose dest carries
//! `k` as an Int less than or equal to the literal, every `:S` edge
//! arriving at that `:R` edge's source contributes its own source,
//! missing-`k` excluded inside the derivation. `<= 1` keeps exactly the
//! `k = 1` boundary near end (a strict `<` in disguise answers `[]`),
//! `<= 9` answers `[3, 6]` against its own derivation, every far-end `c`
//! is keyless so `WHERE c.k <= 1` answers the empty set, and the
//! direction control runs on the outgoing EQUALITY (already grammar) —
//! the outgoing hop-2 source `<=` is a separate grammar slice and is
//! not executed here.

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
const IN_TWO_SRC_LE_ONE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <= 1 RETURN c";
const IN_TWO_SRC_LE_NINE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <= 9 RETURN c";
const IN_TWO_DST_LE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN c";
// The direction control runs on the OUTGOING equality, which is already
// grammar — the outgoing hop-2 source <= is a separate grammar slice.
const OUT_TWO_SRC_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";

/// Far ends (`:S` sources) of incoming two-hop paths whose NEAR end — the
/// `:R` edge's stored DEST — carries `k` as an Int the comparator keeps.
/// The reversed composition AND the missing-key exclusion both live here,
/// in the derivation.
fn reference_far_ends_of_kept_near_ends(
    graph: &ReferenceGraph,
    keeps: impl Fn(i64) -> bool,
) -> Vec<VId> {
    let mut rows = Vec::new();
    for (_, first) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        let near_end_kept = graph.vertex(first.dst).is_some_and(
            |vertex| matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if keeps(*v)),
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
fn incoming_two_hop_near_end_le_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x70_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-le-oracle-{}",
            std::process::id()
        ));
        let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
        let rows;
        let below_nine_rows;
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
            // A k=9 near end: above the bound. Stored reversed:
            // 3 -S-> 2 -R-> 1{k:9}.
            r_batch.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(9))]);
            r_batch.create_vertex(VId(2), vec![], vec![]);
            r_batch.create_vertex(VId(3), vec![], vec![]);
            // The kept chain: the k=1 BOUNDARY near end; 6 -S-> 5 -R->
            // 4{k:1}. A strict < in disguise drops it.
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
                .execute_gql(IN_TWO_SRC_LE_ONE, &bind)
                .expect("incoming two-hop WHERE a.k <= 1 executes");
            below_nine_rows = db
                .execute_gql(IN_TWO_SRC_LE_NINE, &bind)
                .expect("the <= 9 spelling executes");
            far_end_rows = db
                .execute_gql(IN_TWO_DST_LE, &bind)
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
            reference_far_ends_of_kept_near_ends(graph, |k| k <= 1),
            "the engine equals the reversed near-end derivation"
        );
        assert_eq!(
            rows,
            vec![VId(6)],
            "the k=1 boundary near end is IN — a strict < in disguise \
             answers [] and fails"
        );
        assert!(
            !rows.contains(&VId(3)),
            "the k=9 near end's far end sits above the bound"
        );
        assert!(
            !rows.contains(&VId(9)),
            "the keyless near end's far end is OUT — missing k is not \
             k <= 1"
        );
        assert_eq!(
            below_nine_rows,
            reference_far_ends_of_kept_near_ends(graph, |k| k <= 9),
            "the <= 9 spelling equals its own derivation"
        );
        assert_eq!(
            below_nine_rows,
            vec![VId(3), VId(6)],
            "<= 9 keeps both keyed near ends"
        );
        assert!(
            far_end_rows.is_empty(),
            "every far-end c is keyless, so WHERE c.k <= 1 answers [] — a \
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
