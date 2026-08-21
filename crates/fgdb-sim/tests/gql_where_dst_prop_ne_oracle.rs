//! **`WHERE b.k <> 1 RETURN a`, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.16`).
//!
//! The destination-property inequality, projecting SOURCES: the derivation
//! filters on the DEST's props and returns the edge's source, in plain
//! code over reference rows, with the missing-key exclusion inside the
//! derivation itself (`matches!(props.get(&K), Some(Int(v)) if *v != 1)`).
//! The concrete `[3]` convicts three kernels at once: the
//! complement-of-equality reading answers 5 (its dest carries no `k`), a
//! source-side filter answers nothing here (no SOURCE carries `k` at all),
//! and a dest projection answers 4 instead of 3.

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
const FILTERED: &str = "MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a";

/// Sources of `:R` edges whose DESTINATION carries `k` as an `Int` other
/// than 1 — the missing-key exclusion and the dest-side filtering both
/// live here, in the derivation, so neither a complement-of-equality nor a
/// source-side reading can agree with it on this fixture.
fn reference_sources(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph.vertex(edge.dst).is_some_and(|vertex| {
                matches!(vertex.props.get(&K), Some(CanonicalScalar::Int(v)) if *v != 1)
            })
        })
        .map(|(_, edge)| edge.src)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn dest_property_inequality_equals_reference_filter() {
    let ((), report) = run_async_under_lab(0x42_16, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-dst-prop-ne-oracle-{}",
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
                .execute_gql(FILTERED, &bind)
                .expect("dest-property-inequality MATCH executes");
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
            reference_sources(graph),
            "the engine's dest-inequality answer equals the oracle's derivation"
        );
        assert_eq!(
            rows,
            vec![VId(3)],
            "the k=9 dest's SOURCE answers, and only it"
        );
        assert!(
            !rows.contains(&VId(1)),
            "the k=1 dest's source fails the inequality"
        );
        assert!(
            !rows.contains(&VId(5)),
            "the keyless dest's source satisfies NEITHER predicate — the \
             complement-of-equality reading answers it and is wrong"
        );
        assert!(
            !rows.contains(&VId(4)),
            "sources are projected, not the filtered dests themselves"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
