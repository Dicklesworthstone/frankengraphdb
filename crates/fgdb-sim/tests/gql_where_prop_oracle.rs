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
const FILTERED: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";

fn reference_destinations(graph: &ReferenceGraph) -> Vec<VId> {
    let mut rows: Vec<_> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .filter(|(_, edge)| {
            graph
                .vertex(edge.src)
                .is_some_and(|vertex| vertex.props.get(&K) == Some(&CanonicalScalar::Int(1)))
        })
        .map(|(_, edge)| edge.dst)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[test]
fn source_property_match_equals_reference_filter() {
    let ((), report) = run_async_under_lab(0x41_02, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-where-prop-oracle-{}", std::process::id()));
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
                .expect("property-filtered MATCH executes");
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

        assert_eq!(rows, reference_destinations(graph));
        assert_eq!(rows, vec![VId(2)]);
        assert!(!rows.contains(&VId(4)) && !rows.contains(&VId(6)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
