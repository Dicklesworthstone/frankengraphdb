//! FOREACH runs native writes in one workspace and preserves its input bag.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const COUNTER: LabelId = LabelId(1);
const ITEM: LabelId = LabelId(2);
const OUTER: LabelId = LabelId(3);
const HIGH: u64 = 10_000_000;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Counter") => Some(GraphSymbol::Label(COUNTER)),
        (GraphSymbolKind::Label, "F") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Label, "Outer") => Some(GraphSymbol::Label(OUTER)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn prepare(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    PreparedGraphWriteChainText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn policy(rows: u64, work: u64, scratch: u64, vertices: u64, effects: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(HIGH, rows, work, scratch),
        vertices,
        1000,
    )
    .with_mutation_limit(effects)
}

fn generous() -> GraphInsertPolicy {
    policy(HIGH, HIGH, HIGH, 1000, 1000)
}

fn allocate(next: &AtomicU64, request: GraphInsertRequest) -> Result<ElementId, ()> {
    let id = u128::from(next.fetch_add(1, Ordering::Relaxed));
    Ok(match request {
        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(id)),
        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(id)),
    })
}

#[test]
fn local_create_set_remove_body_commits_once_and_reopens() {
    let ((), report) = run_async_under_lab(0xf0ec_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let pinned = db.read_session().unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "FOREACH (x IN [1,2,3] | CREATE (local:F {p:x}) SET local.q=x+10 REMOVE local.q)",
        );
        let mut txn = db.begin(&txcx).unwrap();
        let next = AtomicU64::new(100);
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(&mut db, &query, &chain, generous(), |request| {
                allocate(&next, request)
            })
            .unwrap();
        assert_eq!(stats.created_vertices, 3);
        assert_eq!(receipt.stats().mutation_effects, 6);
        assert_eq!(receipt.stats().completed_statements, 1);
        assert!(rows.value.is_empty());
        assert_eq!(rows.rows.result_rows, 0);
        let vertices = txn.vertices(&db).unwrap();
        assert_eq!(vertices.len(), 3);
        for (vertex, value) in vertices.iter().zip(1..=3) {
            assert_eq!(vertex.labels, vec![ITEM]);
            assert_eq!(vertex.props, vec![(P, CanonicalScalar::Int(value).into())]);
        }
        assert!(db.vertices().unwrap().is_empty());
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert!(pinned.vertices().unwrap().is_empty());
        let expected = db.vertices().unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), expected);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn each_iteration_reads_previous_staged_property_values() {
    let ((), report) = run_async_under_lab(0xf0ec_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let chain =
            prepare("CREATE (n:Counter {p:0}) FOREACH (x IN [1,2,3] | SET n.p=n.p+x) RETURN n.p");
        let next = AtomicU64::new(100);
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(receipt.stats().mutation_effects, 3);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(6))]
        );
        assert_eq!(
            txn.vertices(&db).unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(6).into())]
        );
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(
            db.vertices().unwrap()[0].props,
            vec![(P, CanonicalScalar::Int(6).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collection_is_pinned_per_outer_row_and_refreshed_for_the_next_occurrence() {
    let ((), report) = run_async_under_lab(0xf0ec_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let chain = prepare(
            "CREATE (n:Counter {p:[1,2]}) WITH n UNWIND [0,1] AS repeat \
             FOREACH (x IN n.p | CREATE (:F {q:x}) SET n.p=[9]) RETURN n",
        );
        let next = AtomicU64::new(100);
        let (stats, rows, _) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 4);
        assert_eq!(rows.value.len(), 2);
        assert_eq!(rows.value[0], rows.value[1]);
        let created: Vec<_> = txn
            .vertices(&db)
            .unwrap()
            .into_iter()
            .filter(|row| row.labels.contains(&ITEM))
            .collect();
        assert_eq!(created.len(), 3);
        for (vertex, value) in created.iter().zip([1, 2, 9]) {
            assert_eq!(vertex.props, vec![(Q, CanonicalScalar::Int(value).into())]);
        }
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 4);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_null_and_multiple_entries_keep_exactly_the_outer_occurrences() {
    let ((), report) = run_async_under_lab(0xf0ec_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (list, body_creations) in [("[]", 0), ("null", 0), ("[null]", 2), ("[1,2,3]", 6)] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let chain = prepare(&format!(
                "UNWIND [7,8] AS seed_value CREATE (n:Outer {{p:seed_value}}) \
                 FOREACH (x IN {list} | CREATE (:F {{p:x}})) RETURN n.p",
            ));
            let next = AtomicU64::new(100);
            let (stats, rows, _) = txn
                .execute_graph_write_chain_governed(
                    &mut db,
                    &contexts.query(),
                    &chain,
                    generous(),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(stats.created_vertices, 2 + body_creations);
            assert_eq!(
                rows.value.len(),
                2,
                "FOREACH {list} must not filter or multiply outer rows"
            );
            assert_eq!(
                rows.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(7))]
            );
            assert_eq!(
                rows.value[1].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(8))]
            );
            let vertices = txn.vertices(&db).unwrap();
            assert_eq!(
                vertices
                    .iter()
                    .filter(|row| row.labels.contains(&OUTER))
                    .count(),
                2
            );
            assert_eq!(
                vertices
                    .iter()
                    .filter(|row| row.labels.contains(&ITEM))
                    .count(),
                body_creations as usize
            );
        }
        assert!(
            PreparedGraphWriteChainText::prepare(
                "FOREACH (x IN [1] | CREATE (local:F)) RETURN local",
                R,
                symbols,
            )
            .is_err(),
            "a body-local declaration must not escape FOREACH"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collected_elements_are_native_mutation_targets_without_extra_outer_rows() {
    let ((), report) = run_async_under_lab(0xf0ec_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![COUNTER], vec![(P, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(2), vec![COUNTER], vec![(P, CanonicalScalar::Int(10))]);
        db.write(&contexts.commit(), seed).await.unwrap();
        let chain = prepare(
            "MATCH (n:Counter) WITH collect(n) AS nodes FOREACH (n IN nodes | SET n.p=n.p+1) RETURN size(nodes)",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let next = AtomicU64::new(100);
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(receipt.stats().mutation_effects, 2);
        assert_eq!(next.load(Ordering::Relaxed), 100);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(2))]
        );
        for (vid, value) in [(VId(1), 1), (VId(2), 11)] {
            assert_eq!(
                txn.vertex(&db, vid).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(value).into())]
            );
        }
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_collection_foreach_completes_as_a_read_without_a_marker() {
    let ((), report) = run_async_under_lab(0xf0ec_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        for text in [
            "FOREACH (x IN [] | CREATE (:F {p:x})) RETURN 0",
            "FOREACH (x IN null | CREATE (:F {p:x})) RETURN 0",
            "MATCH (n:Counter) WITH collect(n) AS nodes FOREACH (n IN nodes | SET n.p=1) RETURN size(nodes)",
        ] {
            let chain = prepare(text);
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let next = AtomicU64::new(100);
            let (stats, rows, receipt) = txn
                .execute_graph_write_chain_governed(
                    &mut db,
                    &contexts.query(),
                    &chain,
                    generous(),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(stats.created_vertices, 0);
            assert_eq!(receipt.stats().mutation_effects, 0);
            assert_eq!(next.load(Ordering::Relaxed), 100);
            assert_eq!(rows.value.len(), 1);
            assert_eq!(
                rows.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(0))]
            );
            assert!(matches!(
                txn.finish(&mut db, &contexts.commit()).await.unwrap(),
                fgdb_types::EmbeddedTxnCompletion::ReadClosed { .. }
            ));
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(db.delta_since(basis).unwrap().count(), 0);
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_iteration_failure_restores_the_prior_transaction() {
    let ((), report) = run_async_under_lab(0xf0ec_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let chain = prepare("FOREACH (x IN [1,2,3] | CREATE (n:F {p:x}) SET n.q=10/(3-x))");
        let next = AtomicU64::new(100);
        txn.execute_graph_write_chain_governed(
            &mut db,
            &contexts.query(),
            &chain,
            generous(),
            |request| allocate(&next, request),
        )
        .expect_err("the third iteration's arithmetic error must discard earlier iterations");
        assert!(
            next.load(Ordering::Relaxed) > 101,
            "earlier iterations must have allocated"
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertices(&db).unwrap().len(), 1);
        assert_eq!(db.frontier().unwrap(), basis);
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nonreturning_body_and_following_create_spend_no_public_result_rows() {
    let ((), report) = run_async_under_lab(0xf0ec_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for continuation in [false, true] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let tail = if continuation {
                " CREATE (after:Outer {p:n.p})"
            } else {
                ""
            };
            let chain = prepare(&format!(
                "CREATE (n:Counter {{p:0}}) FOREACH (x IN [1,2,3] | \
                 CREATE (item:F {{p:x}}) SET n.p=n.p+x REMOVE item.p){tail}",
            ));
            let next = AtomicU64::new(100);
            let (stats, rows, receipt) = txn
                .execute_graph_write_chain_governed(
                    &mut db,
                    &contexts.query(),
                    &chain,
                    policy(0, HIGH, HIGH, 5, 6),
                    |request| allocate(&next, request),
                )
                .expect("private body and continuation rows must not spend public output quota");
            assert!(rows.value.is_empty());
            assert_eq!(stats.selection.result_rows, 0);
            assert_eq!(stats.selection, rows.rows);
            assert_eq!(stats.evaluator, rows.evaluator);
            assert_eq!(receipt.stats().selection, stats.selection);
            assert_eq!(receipt.stats().evaluator, stats.evaluator);
            assert_eq!(receipt.stats().mutation_effects, 6);
            assert_eq!(stats.created_vertices, if continuation { 5 } else { 4 });
            let vertices = txn.vertices(&db).unwrap();
            assert_eq!(
                vertices
                    .iter()
                    .filter(|row| row.labels.contains(&ITEM))
                    .count(),
                3
            );
            for vertex in vertices {
                if vertex.labels.contains(&ITEM) {
                    assert!(vertex.props.is_empty());
                } else {
                    assert_eq!(vertex.props, vec![(P, CanonicalScalar::Int(6).into())]);
                }
            }
            txn.finish(&mut db, &contexts.commit()).await.unwrap();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_iterations_share_work_scratch_creation_effect_and_final_row_allowances() {
    let ((), report) = run_async_under_lab(0xf0ec_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let chain = prepare(
            "CREATE (n:Counter {p:0}) FOREACH (x IN [1,2,3] | CREATE (:F {p:x}) SET n.p=n.p+x) RETURN n.p",
        );
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let next = AtomicU64::new(100);
        let (stats, _, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 4);
        assert_eq!(receipt.stats().mutation_effects, 3);
        assert!(stats.evaluator.work_units > 1 && stats.evaluator.scratch_entries > 1);
        drop(txn);
        for limit in [
            policy(1, stats.evaluator.work_units - 1, HIGH, 1000, 1000),
            policy(1, HIGH, stats.evaluator.scratch_entries - 1, 1000, 1000),
            policy(1, HIGH, HIGH, 3, 1000),
            policy(1, HIGH, HIGH, 1000, 2),
            policy(0, HIGH, HIGH, 1000, 1000),
        ] {
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
            txn.write(&mut db, prefix).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            txn.execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                limit,
                |request| allocate(&next, request),
            )
            .expect_err("a FOREACH boundary must not reset any statement allowance");
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(txn.vertices(&db).unwrap().len(), 1);
            assert!(db.vertices().unwrap().is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
