//! **`WHERE a.k <> 1`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.15`).
//!
//! The property INEQUALITY is not the equality's complement: a source with
//! NO `k` at all satisfies neither predicate. The reference helper says so
//! in plain code — dests of `:R` sources that CARRY `k` as an `Int(v)`
//! with `v != 1`, missing key excluded — so the concrete `[4]` proves both
//! exclusions at once: 2 is out because its source's `k` IS 1, and 6 is
//! out because its source carries no `k`, the row a
//! complement-of-equality kernel wrongly answers.

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
const FILTERED: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";

/// Dests of `:R` edges whose source CARRIES `k` as an `Int` other than 1 —
/// the missing-key exclusion lives here, in the derivation, so the engine
/// cannot smuggle a complement-of-equality reading past the differential.
fn reference_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.src).is_some_and(|vertex| {
                matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1)
            })
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn source_property_inequality_equals_reference_filter() {
    let ((), report) = run_async_under_lab(0x41_15, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-ne-oracle-{}",
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
            seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(4), vec![], vec![]);
            seed.create_vertex(VId(5), vec![], vec![]);
            seed.create_vertex(VId(6), vec![], vec![]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(4), vec![]);
            seed.add_edge(EId(12), VId(5), VId(6), vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_property("k", K);
            rows = db
                .execute_gql(FILTERED, &bind)
                .expect("property-inequality MATCH executes");
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
            "the engine's inequality answer equals the oracle's derivation"
        );
        assert_eq!(rows, vec![VId(4)], "k=9 passes, and only it");
        assert!(
            !rows.contains(&VId(2)),
            "the k=1 source's dest fails the inequality"
        );
        assert!(
            !rows.contains(&VId(6)),
            "the keyless source's dest satisfies NEITHER predicate — the \
             complement-of-equality reading answers it and is wrong"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
