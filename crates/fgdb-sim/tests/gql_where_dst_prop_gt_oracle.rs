//! **`WHERE b.k > 1` vs the reference edge table**
//! (`fgdb-w5-parsers-nje.24`).
//!
//! The engine's ordered dest comparison is judged against a plain-code
//! derivation over the reference oracle's own tables: sources of edges
//! whose DEST carries `k` as an Int strictly greater than the literal —
//! with the missing-`k` exclusion written into the derivation itself, so an
//! executor treating a propertyless dest as "greater than nothing" (or a
//! `>=` masquerading as `>` on the `k = 1` dest) fails the oracle equality,
//! not merely a pinned vector.

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
const DST_GREATER: &str = "MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a";

/// Sources of `:R` edges whose dest carries `k` as an Int strictly greater
/// than 1 — a missing `k` excludes the edge, mirroring the executor's law.
fn reference_sources_of_greater_dests(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.dst).is_some_and(|dest| {
                matches!(
                    dest.props.get(&K),
                    Some(CanonicalScalar::Int(value)) if *value > 1
                )
            })
        })
        .map(|(_, edge)| edge.src)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn dest_property_greater_than_equals_reference_sources() {
    let ((), report) = run_async_under_lab(0x56_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-dst-prop-gt-oracle-{}",
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
            seed.create_vertex(VId(1), vec![], vec![]);
            seed.create_vertex(VId(2), vec![], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(3), vec![], vec![]);
            seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
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
                .execute_gql(DST_GREATER, &bind)
                .expect("dest greater-than MATCH executes");
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
            reference_sources_of_greater_dests(graph),
            "the engine equals the reference derivation"
        );
        assert_eq!(rows, vec![VId(3)], "only the k=9 dest's source passes");
        assert!(
            !rows.contains(&VId(1)),
            "the k=1 dest fails strict greater-than — a >= in disguise \
             answers [1, 3] and fails"
        );
        assert!(
            !rows.contains(&VId(5)),
            "the missing-k dest's source is OUT, not vacuously greater"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
