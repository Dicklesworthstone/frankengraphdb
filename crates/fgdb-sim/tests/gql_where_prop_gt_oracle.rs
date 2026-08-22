//! **`WHERE a.k > 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.22`).
//!
//! The first ORDERED comparator: strictly-greater on the source's `k`.
//! The derivation keeps dests whose source carries `k` as an `Int` with
//! `*v > 1` — missing key excluded inside — and the fixture places a
//! carrier on each side of the boundary plus ON it: `k = 1` convicts a
//! `>=` reading (the boundary is not greater), `k = 0` convicts a `<>`
//! reading (below-boundary is not-equal but not greater), the keyless
//! source convicts the treat-missing-as-passing reading, and only the
//! `k = 9` carrier's dest survives — the concrete `[4]`.

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
const FILTERED: &str = "MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b";

/// Dests of `:R` edges whose source carries `k` as an `Int` strictly
/// greater than 1 — the boundary and missing-key exclusions live here.
fn reference_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.src).is_some_and(
                |vertex| matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v > 1),
            )
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn source_property_greater_than_equals_reference_filter() {
    let ((), report) = run_async_under_lab(0x49_22, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-gt-oracle-{}",
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
            // ON the boundary: 1 > 1 is false — convicts >=.
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![]);
            // Above the boundary: the survivor.
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(4), vec![], vec![]);
            // Missing key: satisfies no comparator.
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![]);
            // Below the boundary: not-equal but not greater — convicts <>.
            seed.create_vertex(VId(7), vec![], vec![(K, CanonicalScalar::Int(0))]);
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
                .execute_gql(FILTERED, &bind)
                .expect("greater-than MATCH executes");
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
            reference_destinations(graph),
            "the engine's greater-than answer equals the oracle's derivation"
        );
        assert_eq!(rows, vec![VId(4)], "only the k=9 carrier's dest survives");
        assert!(
            !rows.contains(&VId(2)),
            "the boundary carrier fails: 1 > 1 is false — a >= reading \
             answers 2"
        );
        assert!(
            !rows.contains(&VId(8)),
            "the below-boundary carrier fails: 0 is not-equal but not \
             greater — a <> reading answers 8"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the keyless source satisfies no ordered comparator"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
