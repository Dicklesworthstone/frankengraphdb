//! **Undirected `WHERE b.k != 1 RETURN b`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje-63-lpg5`).
//!
//! An undirected match binds each edge twice, once per orientation, and
//! here the predicate and the projection both ride the `b` side: the
//! answer is every endpoint that plays `b` in some binding AND itself
//! carries `k` as an Int NOT equal to the literal — `!=` aliasing `<>`
//! exactly. The derivation walks both orientations in plain code (the
//! dst plays `b` for the src-anchored binding, the src plays `b` for the
//! flipped one), with missing-`k` excluded inside it. The src-side
//! `k = 9` carrier is the flipped-orientation witness a dst-only kernel
//! misses, the `k = 1` carrier fails the inequality, and the keyless
//! edge contributes nothing.

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
const UN_DST_BANG_NE: &str = "MATCH (a)-[:R]-(b) WHERE b.k != 1 RETURN b";

/// Endpoints incident to an `:R` edge that themselves carry `k` as an
/// `Int` different from 1 — both orientations walked (each endpoint
/// plays `b` once per incident edge), missing key out, inside the
/// derivation: the `<>` law verbatim, because `!=` is an alias.
fn reference_unequal_incident_endpoints(graph: &ReferenceGraph) -> Vec<VId> {
    let carrier = |vid: VId| {
        graph.vertex(vid).is_some_and(
            |vertex| matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1),
        )
    };
    let mut rows = Vec::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        if carrier(edge.dst) {
            rows.push(edge.dst);
        }
        if carrier(edge.src) {
            rows.push(edge.src);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn undirected_dest_bang_inequality_equals_its_reference() {
    let ((), report) = run_async_under_lab(0x79_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-where-dst-bang-ne-oracle-{}",
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
            // A k=1 carrier: incident, but fails the inequality.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![]);
            // A k=9 carrier on the edge DEST side: answers as b directly.
            seed.create_vertex(VId(3), vec![], vec![]);
            seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
            // Keyless on both ends: contributes nothing.
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![]);
            // A k=9 carrier on the edge SOURCE side: the flipped-orientation
            // witness a dst-only kernel misses.
            seed.create_vertex(VId(7), vec![], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(8), vec![], vec![]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(4), vec![]);
            seed.add_edge(EId(12), VId(5), VId(6), vec![]);
            seed.add_edge(EId(13), VId(7), VId(8), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K);
            rows = db
                .execute_gql(UN_DST_BANG_NE, &bind)
                .expect("undirected WHERE b.k != 1 executes");
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
            reference_unequal_incident_endpoints(graph),
            "the engine's != answer equals the <> both-orientation \
             derivation — an alias, not a new comparator"
        );
        assert_eq!(
            rows,
            vec![VId(4), VId(7)],
            "the dest-side k=9 carrier answers directly AND the src-side \
             k=9 carrier answers as the flipped binding's b — a dst-only \
             kernel misses 7"
        );
        assert!(rows.contains(&VId(4)), "the unequal incident carrier is IN");
        assert!(
            !rows.contains(&VId(6)),
            "the keyless edge contributes nothing — missing k is not \
             k != 1"
        );
        assert!(
            !rows.contains(&VId(1)),
            "the k=1 carrier is incident but fails the inequality"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
