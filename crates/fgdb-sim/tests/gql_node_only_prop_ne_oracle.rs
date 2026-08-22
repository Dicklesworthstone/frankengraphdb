//! **Node-only `WHERE a.k <> 1` vs the reference labeled isolates**
//! (`fgdb-w5-parsers-nje.15`).
//!
//! The inequality twin of `gql_node_only_prop_oracle.rs`: the filtered scan
//! must equal the reference's labeled vertices that carry `k` as an Int NOT
//! equal to the literal — a vertex with no `k` at all is excluded, not
//! "trivially unequal", which is what the propertyless Person in the
//! fixture separates. The reference derivation applies exactly that rule to
//! its own vertex table, so an executor that read missing-as-unequal fails
//! the equality against the oracle, not just a pinned vector.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, GraphId, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(7);
const K: PropertyKeyId = PropertyKeyId(9);
const FILTERED: &str = "MATCH (a:Person) WHERE a.k <> 1 RETURN a";

/// Labeled vertices whose `k` is an Int different from 1 — a missing `k`
/// excludes the vertex, mirroring the executor's law.
fn reference_people_with_k_not_one(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_vertices()
        .filter(|(_, vertex)| {
            vertex.labels.contains(&PERSON)
                && matches!(
                    vertex.props.get(&K),
                    Some(CanonicalScalar::Int(value)) if *value != 1
                )
        })
        .map(|(vid, _)| vid)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn node_only_property_inequality_equals_reference_vertices() {
    let ((), report) = run_async_under_lab(0x4b_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-node-only-prop-ne-oracle-{}",
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
            seed.create_vertex(VId(1), vec![PERSON], vec![(K, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(3), vec![PERSON], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(5), vec![PERSON], vec![]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_label("Person", PERSON)
                .with_property("k", K);
            rows = db
                .execute_gql(FILTERED, &bind)
                .expect("node-only property-inequality MATCH executes");
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

        assert_eq!(rows, reference_people_with_k_not_one(graph));
        assert_eq!(rows, vec![VId(3)], "only the k=9 Person passes");
        assert!(!rows.contains(&VId(1)), "k=1 fails the inequality");
        assert!(
            !rows.contains(&VId(5)),
            "a missing k excludes the vertex — it is not trivially unequal"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
