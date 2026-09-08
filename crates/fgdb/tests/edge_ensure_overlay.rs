use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::GqlExecutionBudget;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

#[test]
fn ensure_noops_do_not_invent_edge_aliases_or_replace_existing_properties() {
    let ((), report) = run_async_under_lab(0xe050_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let keys = DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32]);
        let mut db = Database::open_memory(&cx, keys).await.expect("database");
        let relation = RelationId(1);
        let property = PropertyKeyId(1);
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![(property, CanonicalScalar::Int(7))]);
        db.write(&cx, seed).await.expect("seed");
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(relation);
        staged.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![(property, CanonicalScalar::Int(99))]);
        staged.ensure_edge_by_triple(EId(10), VId(1), VId(2), vec![(property, CanonicalScalar::Int(88))]);
        txn.write(&mut db, staged).expect("both ensures resolve to the existing edge");
        assert!(txn.edge(&db, EId(999)).expect("unused alias").is_none());
        let expected = db.edge(EId(10)).expect("durable point").expect("edge");
        assert_eq!(txn.edge(&db, EId(10)).expect("overlay point"), Some(expected.clone()));
        assert_eq!(txn.edges(&db).expect("overlay table"), vec![expected]);
        let query = txn.prepare_gql_query("MATCH (a)-[:R]->(b) RETURN b",
            &RelationBind::new().with_relation("R", relation)).expect("query");
        let result = txn.execute_prepared_query_budgeted(&db, &query, GqlExecutionBudget::new(1, 1))
            .expect("no nonexistent alias may inflate the admission count");
        assert_eq!(result.stats.snapshot_records, 1);
        assert_eq!(result.value, vec![VId(2)]);
        txn.commit(&mut db, &cx).await.expect("publish canonical no-op");
        assert!(db.edge(EId(999)).expect("alias after commit").is_none());
        assert_eq!(db.edge(EId(10)).expect("edge after commit").expect("edge").props,
            vec![(property, CanonicalScalar::Int(7))]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deleting_the_old_triple_then_ensuring_materializes_only_the_real_successor() {
    let ((), report) = run_async_under_lab(0xe050_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let keys = DatabaseKeys::new([0xb1; 32], DatabaseSecurityNamespaceId([0xb2; 32]), [0xb3; 32]);
        let mut db = Database::open_memory(&cx, keys).await.expect("database");
        let relation = RelationId(1);
        let property = PropertyKeyId(1);
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&cx, seed).await.expect("seed");
        let mut txn = db.begin(&txn_cx).expect("begin");
        let mut first = WriteBatch::new(relation);
        first.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        first.delete_edge(EId(10));
        txn.write(&mut db, first).expect("ensure then delete");
        assert!(txn.edges(&db).expect("no imaginary old alias").is_empty());
        let mut second = WriteBatch::new(relation);
        second.ensure_edge_by_triple(EId(20), VId(1), VId(2), vec![(property, CanonicalScalar::Int(1))]);
        second.set_edge_property(EId(20), property, Some(CanonicalScalar::Int(2)));
        txn.write(&mut db, second).expect("new real successor");
        assert!(txn.edge(&db, EId(999)).expect("unused alias").is_none());
        assert!(txn.edge(&db, EId(10)).expect("deleted edge").is_none());
        let point = txn.edge(&db, EId(20)).expect("successor").expect("edge");
        assert_eq!(point.props, vec![(property, CanonicalScalar::Int(2))]);
        assert_eq!(txn.edges(&db).expect("same canonical net table"), vec![point]);
        txn.commit(&mut db, &cx).await.expect("publish");
        assert!(db.edge(EId(999)).expect("alias after publication").is_none());
        assert_eq!(db.edge(EId(20)).expect("committed successor").expect("edge").props,
            vec![(property, CanonicalScalar::Int(2))]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
