//! **Node-only `WHERE a.k >= 1` vs the reference labeled isolates**
//! (`fgdb-w5-parsers-nje.31`).
//!
//! The inclusive-comparison twin of `gql_node_only_prop_ne_oracle.rs`: the
//! filtered scan must equal the reference's labeled vertices that carry `k`
//! as an Int greater than or equal to the literal. The `k = 1` Person is
//! the boundary that separates `>=` from `>` (it must be IN), the `k = 0`
//! Person separates it from `<>`, the propertyless Person pins the
//! missing-`k` exclusion, and the unlabeled `k = 1` vertex pins the label
//! constraint — each cheat breaks the equality against the oracle's own
//! vertex table, not just a pinned vector.

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
const FILTERED: &str = "MATCH (a:Person) WHERE a.k >= 1 RETURN a";

/// Labeled vertices whose `k` is an Int greater than or equal to 1 — a
/// missing `k` excludes the vertex and a missing label excludes it too,
/// mirroring the executor's law.
fn reference_people_with_k_ge_one(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_vertices()
        .filter(|(_, vertex)| {
            vertex.labels.contains(&PERSON)
                && matches!(
                    vertex.props.get(&K),
                    Some(CanonicalScalar::Int(value)) if *value >= 1
                )
        })
        .map(|(vid, _)| vid)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn node_only_property_ge_equals_reference_vertices() {
    let ((), report) = run_async_under_lab(0x5c_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-node-only-prop-ge-oracle-{}",
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
            seed.create_vertex(VId(2), vec![PERSON], vec![(K, CanonicalScalar::Int(9))]);
            seed.create_vertex(VId(3), vec![PERSON], vec![(K, CanonicalScalar::Int(0))]);
            seed.create_vertex(VId(4), vec![PERSON], vec![]);
            seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(1))]);
            db.write(&commit, seed).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_label("Person", PERSON)
                .with_property("k", K);
            rows = db
                .execute_gql(FILTERED, &bind)
                .expect("node-only greater-or-equal MATCH executes");
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
            reference_people_with_k_ge_one(graph),
            "the engine equals the reference derivation"
        );
        assert_eq!(
            rows,
            vec![VId(1), VId(2)],
            "the k=1 boundary Person is IN — a strict > answers [2] and fails"
        );
        assert!(
            !rows.contains(&VId(3)),
            "the k=0 Person fails >= — a <> in disguise answers [1, 2, 3]"
        );
        assert!(
            !rows.contains(&VId(4)),
            "a missing k excludes the Person — not vacuously greater-or-equal"
        );
        assert!(
            !rows.contains(&VId(5)),
            "the unlabeled k=1 vertex is OUT — the label constrains the scan"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
