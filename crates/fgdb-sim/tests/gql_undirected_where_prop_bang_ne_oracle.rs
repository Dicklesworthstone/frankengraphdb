//! **Undirected `WHERE a.k != 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje-61-56vl`).
//!
//! The C-style spelling of `gql_undirected_where_prop_oracle.rs`'s
//! inequality face: an undirected match binds each edge twice, once per
//! orientation, and `!=` must alias `<>` exactly — so the engine equals
//! the SAME both-orientation derivation: src carrier pushes dst, dst
//! carrier pushes src, kept when the carrier's `k` is an Int NOT equal
//! to the literal, missing-`k` excluded inside the derivation (never
//! "trivially unequal"). The dest-side `k = 9` carrier is the
//! flipped-orientation witness an outgoing-only kernel misses, the
//! `k = 1` carrier fails the inequality, and the keyless edge
//! contributes nothing.

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
const UN_BANG_NE: &str = "MATCH (a)-[:R]-(b) WHERE a.k != 1 RETURN b";

/// Other endpoints of every `:R` edge binding whose `a`-side carries `k`
/// as an `Int` different from 1 — the `<>` law verbatim, because `!=` is
/// an alias and not a new comparator: BOTH orientations walked, missing
/// key out, inside the derivation.
fn reference_other_endpoints_of_unequal_carriers(graph: &ReferenceGraph) -> Vec<VId> {
    let carrier = |vid: VId| {
        graph.vertex(vid).is_some_and(
            |vertex| matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1),
        )
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
fn undirected_origin_bang_inequality_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x77_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-where-prop-bang-ne-oracle-{}",
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
            let mut seed = WriteBatch::new(R);
            // A k=1 carrier on the source side: fails the inequality.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![]);
            // A k=9 carrier on the source side: its neighbour answers.
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(4), vec![], vec![]);
            // Keyless on both ends: contributes nothing.
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![]);
            // A k=9 carrier on the DEST side: the flipped-orientation
            // witness an outgoing-only kernel misses.
            seed.create_vertex(VId(7), vec![], vec![]);
            seed.create_vertex(VId(8), vec![], vec![(K, CanonicalScalar::Int(9))]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(4), vec![]);
            seed.add_edge(EId(12), VId(5), VId(6), vec![]);
            seed.add_edge(EId(13), VId(7), VId(8), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K);
            rows = db
                .execute_gql(UN_BANG_NE, &bind)
                .expect("undirected WHERE a.k != 1 executes");
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
            reference_other_endpoints_of_unequal_carriers(graph),
            "the engine's != answer equals the <> both-orientation \
             derivation — an alias, not a new comparator"
        );
        assert_eq!(
            rows,
            vec![VId(4), VId(7)],
            "the source-side k=9 carrier answers 4 AND the dest-side k=9 \
             carrier answers 7 — an outgoing-only kernel misses 7"
        );
        assert!(
            rows.contains(&VId(4)),
            "the unequal carrier's incident neighbour is IN"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the keyless edge contributes nothing — missing k is not \
             k != 1"
        );
        assert!(
            !rows.contains(&VId(2)),
            "the k=1 carrier's neighbour fails the inequality"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
