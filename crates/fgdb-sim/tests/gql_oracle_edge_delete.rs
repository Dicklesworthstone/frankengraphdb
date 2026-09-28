//! Storage edge retirement matches an independently constructed reference delta.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{DeltaRow, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

#[test]
fn native_relationship_delete_prepares() {
    fgdb_gql::PreparedGraphWriteScript::prepare(
        "MATCH (a)-[e:R]->(b) DELETE e",
        RelationId(1),
        |kind, name| match (kind, name) {
            (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                Some(fgdb_gql::GraphSymbol::Relation(RelationId(1)))
            }
            _ => None,
        },
    )
    .expect("native relationship DELETE required by ftzu");
}

#[test]
fn native_edge_delete_retires_parallel_matches_without_deleting_endpoints() {
    let ((), report) = run_async_under_lab(0x4f7a_ed02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let keys = || {
            DatabaseKeys::new(
                [0xb1; 32],
                DatabaseSecurityNamespaceId([0xb2; 32]),
                [0xb3; 32],
            )
        };
        let vfs = fgdb::MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 1..=3 {
            batch.create_vertex(VId(id), vec![], vec![]);
        }
        for id in 10..=11 {
            batch.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
        db.write(&commit, batch).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(12), VId(2), VId(3), vec![]);
        db.write(&commit, other).await.unwrap();
        let before = db.frontier().unwrap();
        let script = fgdb_gql::PreparedGraphWriteScript::prepare(
            "MATCH (a)-[e:R]->(b) DELETE e",
            RelationId(1),
            |kind, name| match (kind, name) {
                (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                    Some(fgdb_gql::GraphSymbol::Relation(RelationId(1)))
                }
                _ => None,
            },
        )
        .unwrap();
        db.execute_graph_write_script_autocommit_governed(
            &txcx,
            &query,
            &commit,
            &script,
            &fgdb_gql::GqlParameters::new(),
            fgdb_gql::GraphWriteProgramPolicy::new(
                fgdb_gql::GqlQueryPolicy::new(1000, 1000, 100_000, 100_000),
                100,
                10,
                10,
            ),
            |_| -> Result<fgdb_delta_types::ElementId, ()> { panic!("DELETE must not allocate") },
        )
        .await
        .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(
            db.edges()
                .unwrap()
                .iter()
                .map(|e| e.entry.eid)
                .collect::<Vec<_>>(),
            vec![EId(12)]
        );
        assert_eq!(
            db.vertices()
                .unwrap()
                .iter()
                .map(|v| v.vid)
                .collect::<Vec<_>>(),
            vec![VId(1), VId(2), VId(3)]
        );
        assert!(db.edge_at(EId(10), before).unwrap().is_some());
        assert!(db.edge_at(EId(11), before).unwrap().is_some());
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            db.edges()
                .unwrap()
                .iter()
                .map(|e| e.entry.eid)
                .collect::<Vec<_>>(),
            vec![EId(12)]
        );
        assert_eq!(
            db.vertices()
                .unwrap()
                .iter()
                .map(|v| v.vid)
                .collect::<Vec<_>>(),
            vec![VId(1), VId(2), VId(3)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_retirement_preserves_parallel_edge_and_endpoints() {
    let ((), report) = run_async_under_lab(0x4f7a_ed01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut model = ReferenceGraph::new();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            batch.create_vertex(VId(id), vec![], vec![]);
            model
                .apply_row(&DeltaRow::CreateVertex {
                    vid: VId(id),
                    birth_ordinal: id as u64,
                    labels: vec![],
                    props: vec![],
                    valid_time: None,
                })
                .unwrap();
        }
        for id in 10..=11 {
            batch.add_edge(EId(id), VId(1), VId(2), vec![]);
            model
                .apply_row(&DeltaRow::CreateEdge {
                    eid: EId(id),
                    birth_ordinal: id as u64,
                    src: VId(1),
                    relation: RelationId(1),
                    dst: VId(2),
                    canonical_key: None,
                    props: vec![],
                    valid_time: None,
                })
                .unwrap();
        }
        db.write(&commit, batch).await.unwrap();
        let before = db.frontier().unwrap();
        let mut deletion = WriteBatch::new(RelationId(1));
        deletion.delete_edge(EId(10));
        db.write(&commit, deletion).await.unwrap();
        model
            .apply_row(&DeltaRow::DeleteEdge {
                eid: EId(10),
                before_version: model.edge(EId(10)).unwrap().version,
            })
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        let edges: Vec<_> = db
            .edges()
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.entry.eid,
                    row.entry.src,
                    row.entry.relation,
                    row.entry.dst,
                    row.props,
                )
            })
            .collect();
        let expected: Vec<_> = model
            .iter_edges()
            .map(|(id, edge)| {
                (
                    id,
                    edge.src,
                    edge.relation,
                    edge.dst,
                    edge.props.iter().map(|(k, v)| (*k, v.clone())).collect(),
                )
            })
            .collect();
        assert_eq!(edges, expected);
        assert_eq!(
            edges.iter().map(|edge| edge.0).collect::<Vec<_>>(),
            vec![EId(11)]
        );
        assert_eq!(
            db.vertices()
                .unwrap()
                .into_iter()
                .map(|v| v.vid)
                .collect::<Vec<_>>(),
            model.iter_vertices().map(|(id, _)| id).collect::<Vec<_>>()
        );
        assert!(db.edge_at(EId(10), before).unwrap().is_some());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
