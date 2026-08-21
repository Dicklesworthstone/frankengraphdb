//! **The destination label, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.5`, dest-label slice).
//!
//! `(a)-[:R]->(b:Person)` filters the DESTINATION: the answer is the `:R`
//! destinations that carry the label, derived here from the reference edge
//! table plus the reference vertex rows — plain code, so the engine is
//! checked against what the durable stream means. The fixture labels the
//! destination (not the source, unlike the sibling suite), so a kernel
//! that evaluates the label on the wrong end answers `[2, 4]` or `[]`
//! here and cannot agree with the derivation's `[2]`.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(7);
const DST_LABELED: &str = "MATCH (a)-[:R]->(b:Person) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        K_OID,
        NAMESPACE,
        DEK,
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

/// The oracle's dest-labeled answer: `:R` destinations whose reference
/// vertex row carries the label. The filter lives here, on the DEST side,
/// in plain code over reference rows.
fn reference_labeled_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut destinations: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph
                .vertex(edge.dst)
                .is_some_and(|vertex| vertex.labels.contains(&PERSON))
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    destinations.sort_unstable();
    destinations.dedup();
    destinations
}

/// Engine answer before the drop, oracle replay after; nothing but the
/// path and the keys crosses the line.
#[test]
fn the_dest_labeled_match_equals_the_reference_person_dests() {
    let ((), report) = run_async_under_lab(0x38_0d, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-label-dst-oracle-{}",
            std::process::id()
        ));
        let engine_rows;
        {
            let mut db = Database::create(&commit, &dir, engine_keys())
                .await
                .expect("creates");
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(2), vec![PERSON], vec![]);
            for vid in [1u128, 3, 4] {
                batch.create_vertex(VId(vid), vec![], vec![]);
            }
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            batch.add_edge(EId(11), VId(3), VId(4), vec![]);
            db.write(&commit, batch).await.expect("fixture commits");

            let bind = RelationBind::new()
                .with_relation("R", R)
                .with_label("Person", PERSON);
            engine_rows = db
                .execute_gql(DST_LABELED, &bind)
                .expect("the dest-labeled MATCH executes");
        }
        // NOTHING crosses this line except the path and the keys.

        let coordinator = CommitCoordinator::open(&commit, &dir, oracle_keys())
            .await
            .expect("the oracle opens the durable stream");
        let replayed = replay(&commit, &coordinator)
            .await
            .expect("the stream replays");
        let graph = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate");

        assert_eq!(
            engine_rows,
            reference_labeled_destinations(graph),
            "the engine's dest-labeled answer equals the oracle's derivation"
        );
        assert_eq!(
            engine_rows,
            vec![VId(2)],
            "and concretely: the :Person destination answers, the unlabeled \
             4 does not — a source-side (or absent) label filter answers \
             [2, 4] or [] here and cannot agree vacuously"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
