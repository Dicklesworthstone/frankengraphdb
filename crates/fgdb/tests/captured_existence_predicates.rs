//! The public text compiler, eager execution and real pinned sources must
//! agree on captured relationship predicates across durable generations.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

#[test]
fn captured_exists_keeps_one_generation_through_updates_handle_drop_and_reopen() {
    let ((), report) = run_async_under_lab(0xe71e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query = contexts.query();
        let keys = DatabaseKeys::new(
            [0x71; 32],
            DatabaseSecurityNamespaceId([0x72; 32]),
            [0x73; 32],
        );
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys.clone())
            .await
            .unwrap();
        let relation = RelationId(1);
        let property = PropertyKeyId(1);
        let mut seed = WriteBatch::new(relation);
        for id in 1..=4 {
            seed.create_vertex(VId(id), vec![], vec![(property, CanonicalScalar::Int(5))]);
        }
        seed.add_edge(
            EId(10),
            VId(1),
            VId(2),
            vec![(property, CanonicalScalar::Int(0))],
        );
        seed.add_edge(
            EId(11),
            VId(1),
            VId(2),
            vec![(property, CanonicalScalar::Int(10))],
        );
        seed.add_edge(
            EId(12),
            VId(2),
            VId(3),
            vec![(property, CanonicalScalar::Null)],
        );
        let basis = db.write(&cx, seed).await.unwrap();
        let pattern = PreparedGraphText::prepare(
            "MATCH (a) WHERE EXISTS { MATCH (a)-[edge:R]->(b) WHERE edge.p > a.p } RETURN a",
            |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(relation)),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(property)),
                _ => None,
            },
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let policy = GqlQueryPolicy::new(1000, 1000, 100_000, 100_000);
        let plain = |rows: &[GraphValueRow]| {
            rows.iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>()
        };
        let before = db
            .execute_graph_pattern_governed(&query, &pattern, policy)
            .unwrap();
        assert_eq!(plain(&before.value), vec![vec![GraphValue::Vertex(VId(1))]]);
        let pinned = db.read_session().unwrap();
        let mut opened = db
            .stream_graph_values_governed(&query, &pattern, policy)
            .unwrap();
        assert_eq!(opened.row_stats().snapshot_records, 0);

        let mut changed = WriteBatch::new(relation);
        changed.set_edge_property(EId(11), property, Some(CanonicalScalar::Int(-1)));
        changed.delete_vertex(VId(3)); // Its incoming null-valued edge retires too.
        changed.add_edge(
            EId(99),
            VId(2),
            VId(4),
            vec![(property, CanonicalScalar::Int(20))],
        );
        db.write(&cx, changed).await.unwrap();
        let after = db
            .execute_graph_pattern_governed(&query, &pattern, policy)
            .unwrap();
        assert_eq!(plain(&after.value), vec![vec![GraphValue::Vertex(VId(2))]]);
        let historical = db
            .stream_graph_values_governed_at(&query, &pattern, basis, policy)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(historical, before.value);
        drop(db);
        // Both the unopened cursor and the explicit view own the original
        // generation; neither borrows the dropped mutable database handle.
        assert_eq!(
            opened.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            before.value
        );
        assert_eq!(opened.snapshot_seq(), basis);
        let view_rows = pinned
            .stream_graph_values_governed(&query, &pattern, policy)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(view_rows, before.value);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .stream_graph_values_governed(&query, &pattern, policy)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            after.value,
        );
        assert_eq!(
            reopened
                .stream_graph_values_governed_at(&query, &pattern, basis, policy)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            before.value,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_existence_observes_the_pinned_cut_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0xe71e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query = contexts.query();
        let keys = DatabaseKeys::new(
            [0x75; 32],
            DatabaseSecurityNamespaceId([0x76; 32]),
            [0x77; 32],
        );
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys.clone())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=4 {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
        }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        seed.add_edge(EId(2), VId(2), VId(3), vec![]);
        let basis = db.write(&cx, seed).await.unwrap();
        let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) \
            WHERE NOT EXISTS { MATCH (b)-[:R]->(c) WHERE c.p >= $floor } } RETURN a";
        let symbols = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        };
        let args = GqlParameters::new().with_int64("floor", 0).unwrap();
        let pattern = PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap();
        let policy = GqlQueryPolicy::new(1000, 1000, 100_000, 100_000);
        let rows = |id| {
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Vertex(
                VId(id),
            )])]
        };
        let before = db
            .execute_graph_pattern_governed(&query, &pattern, policy)
            .unwrap();
        assert_eq!(before.value, rows(2));
        assert_eq!(
            db.query(&query, text, &args, symbols, policy).unwrap(),
            fgdb::QueryResult::Rows {
                columns: vec!["a".into()],
                rows: vec![vec![fgdb::QueryValue::Value(GraphValue::Vertex(VId(2)))]],
            },
        );
        let pinned = db.read_session().unwrap();
        let mut next = WriteBatch::new(RelationId(1));
        next.add_edge(EId(3), VId(3), VId(4), vec![]);
        let head = db.write(&cx, next).await.unwrap();
        assert_eq!(
            db.execute_graph_pattern_governed(&query, &pattern, policy)
                .unwrap()
                .value,
            rows(3)
        );
        assert_eq!(
            db.execute_graph_pattern_governed_at(&query, &pattern, basis, policy)
                .unwrap()
                .value,
            before.value,
        );
        drop(db);
        assert_eq!(
            pinned
                .execute_graph_pattern_governed(&query, &pattern, policy)
                .unwrap()
                .value,
            rows(2)
        );
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys)
            .await
            .unwrap();
        assert_eq!(reopened.frontier().unwrap(), head);
        assert_eq!(
            reopened
                .execute_graph_pattern_governed(&query, &pattern, policy)
                .unwrap()
                .value,
            rows(3)
        );
        assert_eq!(
            reopened
                .execute_graph_pattern_governed_at(&query, &pattern, basis, policy)
                .unwrap()
                .value,
            rows(2),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
