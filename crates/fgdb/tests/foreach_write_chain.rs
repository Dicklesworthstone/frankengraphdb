//! FOREACH runs native writes in one workspace and preserves its input bag.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    GraphWriteProgramPolicy, PreparedGraphWriteChain, PreparedGraphWriteChainText,
    ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalList, CanonicalPropertyValue, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId,
    EId, PurposeContexts, VId,
};
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

struct Catalog;

impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        // properties() enumerates the complete trusted host catalog, including
        // keys absent from the expression. Token probes cannot supply it.
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(COUNTER, "Counter");
        catalog.insert_label(ITEM, "F");
        catalog.insert_label(OUTER, "Outer");
        catalog.insert_relation(R, "R");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        Some(catalog)
    }
}

fn prepare(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    PreparedGraphWriteChainText::prepare(text, R, Catalog)
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
fn independent_fresh_subgraphs_match_visible_prefix_staging_and_reopen() {
    let ((), report) = run_async_under_lab(0xf0ec_0040, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let chain = prepare(
            "UNWIND [2,1] AS outer FOREACH (x IN [outer,outer+10] | \
             CREATE (a:F {p:x})-[r:R {p:x}]->(b:F {p:x+1})) RETURN outer",
        );
        assert_eq!(chain.independent_create_relation(), Some(R));
        let mut first_graph = None;
        for prefixed in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let pinned = db.read_session().unwrap();
            let basis = db.frontier().unwrap();
            // Ordinary read UNWIND preserves the written input order. The
            // write owner's terminal projection below canonicalizes its bag;
            // neither path may change the input [2,1] used by this control.
            assert_eq!(
                db.query(
                    &query,
                    "UNWIND [2,1] AS outer RETURN outer",
                    &GqlParameters::new(),
                    Catalog,
                    GqlQueryPolicy::new(HIGH, 2, HIGH, HIGH),
                )
                .unwrap(),
                QueryResult::Rows {
                    columns: vec!["outer".into()],
                    rows: [2, 1]
                        .into_iter()
                        .map(|value| {
                            vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                                value,
                            )))]
                        })
                        .collect(),
                },
            );
            let mut txn = db.begin(&txcx).unwrap();
            if prefixed {
                // A nonempty entry workspace must use ordinary visible staging.
                let mut prefix = WriteBatch::new(R);
                prefix.create_vertex(VId(7), vec![OUTER], vec![(P, CanonicalScalar::Int(99))]);
                txn.write(&mut db, prefix).unwrap();
            }
            let next = AtomicU64::new(100);
            let (stats, rows, receipt) = txn
                .execute_graph_write_chain_governed(
                    &mut db,
                    &query,
                    &chain,
                    policy(2, HIGH, HIGH, 8, 0),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (8, 4));
            assert_eq!(receipt.stats().mutation_effects, 0);
            assert_eq!(receipt.stats().completed_statements, 1);
            assert_eq!(next.load(Ordering::Relaxed), 112);
            assert_eq!(rows.value.len(), 2);
            // An unpaged RETURN canonicalizes its projected tuple; the
            // collection's written order does not impose an ORDER BY.
            for (row, outer) in rows.value.iter().zip([1, 2]) {
                assert_eq!(
                    row.values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(outer))]
                );
            }
            let mut vertices: Vec<_> = txn
                .vertices(&db)
                .unwrap()
                .into_iter()
                .filter(|vertex| vertex.vid != VId(7))
                .collect();
            let edges = txn.edges(&db).unwrap();
            assert_eq!(vertices.len(), 8);
            assert_eq!(edges.len(), 4);
            for value in [2, 12, 1, 11] {
                let edge = edges
                    .iter()
                    .find(|edge| edge.props == vec![(P, CanonicalScalar::Int(value).into())])
                    .unwrap();
                assert_eq!(edge.entry.relation, R);
                assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(value).into())]);
                let source = txn.vertex(&db, edge.entry.src).unwrap().unwrap();
                let destination = txn.vertex(&db, edge.entry.dst).unwrap().unwrap();
                assert_eq!(source.labels, vec![ITEM]);
                assert_eq!(destination.labels, vec![ITEM]);
                assert_eq!(source.props, edge.props);
                assert_eq!(
                    destination.props,
                    vec![(P, CanonicalScalar::Int(value + 1).into())]
                );
            }
            // The ordinary case has exactly one extra creation intent before
            // this chain. Remove only that known ordinal offset; every other
            // field, including each relative birth ordinal, must still agree.
            for vertex in &mut vertices {
                vertex.birth_ordinal = vertex
                    .birth_ordinal
                    .checked_sub(u64::from(prefixed))
                    .unwrap();
            }
            let graph = (vertices, edges);
            if let Some(first) = &first_graph {
                assert_eq!(&graph, first);
            } else {
                first_graph = Some(graph);
            }
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
            txn.finish(&mut db, &commit).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
            assert!(pinned.vertices().unwrap().is_empty());
            assert!(pinned.edges().unwrap().is_empty());
            let expected = (db.vertices().unwrap(), db.edges().unwrap());
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(
                (reopened.vertices().unwrap(), reopened.edges().unwrap()),
                expected
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_creation_refuses_collisions_before_later_iteration_errors() {
    use fgdb_gql::insertion::GraphInsertError;
    use fgdb_gql::{GqlQueryError, GraphInsertQueryError, GraphWriteChainError};

    let ((), report) = run_async_under_lab(0xf0ec_0041, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let chain = prepare("FOREACH (x IN [0,1,2] | CREATE (:F {p:10/(2-x)}))");
        assert_eq!(chain.independent_create_relation(), Some(R));
        for spent in [false, true] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            if spent {
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(VId(100), vec![ITEM], vec![]);
                db.write(&contexts.commit(), seed).await.unwrap();
                let mut retire = WriteBatch::new(R);
                retire.delete_vertex(VId(100));
                db.write(&contexts.commit(), retire).await.unwrap();
            }
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let calls = AtomicU64::new(0);
            let error = txn
                .execute_graph_write_chain_governed(
                    &mut db,
                    &contexts.query(),
                    &chain,
                    generous(),
                    |request| {
                        assert!(matches!(request, GraphInsertRequest::Vertex { .. }));
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok::<_, ()>(ElementId::Vertex(VId(100)))
                    },
                )
                .unwrap_err();
            let GqlQueryError::Source(GraphWriteChainError::Insert(
                GraphInsertQueryError::Insertion(GraphInsertError::Source(
                    fgdb::WriteTxnError::Write(write_error),
                )),
            )) = error
            else {
                panic!("identity admission must precede the later division by zero: {error:?}");
            };
            if spent {
                assert!(matches!(
                    write_error,
                    fgdb::WriteError::IdentitySpent {
                        elem: ElementId::Vertex(VId(100)),
                    }
                ));
            } else {
                assert!(matches!(
                    write_error,
                    fgdb::WriteError::AlreadyLive {
                        elem: ElementId::Vertex(VId(100)),
                    }
                ));
            }
            assert_eq!(calls.load(Ordering::Relaxed), if spent { 1 } else { 2 });
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert!(txn.vertices(&db).unwrap().is_empty());
            assert!(txn.edges(&db).unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(db.delta_since(basis).unwrap().count(), 0);
            assert_eq!(contexts.txn().outstanding_obligations(), 1);
            txn.abort();
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dependent_creation_bodies_keep_live_properties_and_imported_endpoints() {
    let ((), report) = run_async_under_lab(0xf0ec_0042, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (text, edge_count) in [
            (
                "FOREACH (x IN [1,2] | CREATE (a:F {p:x}) CREATE (:Outer {p:[a][0].p+10}))",
                0,
            ),
            (
                "FOREACH (x IN [1,2] | CREATE (a:F {p:x}) CREATE (:Outer {p:properties([a][0]).p+10}))",
                0,
            ),
            (
                "FOREACH (x IN [1,2] | CREATE (a:F {p:x}) CREATE (b:Outer {p:x+10}) CREATE (a)-[:R]->(b))",
                2,
            ),
        ] {
            let chain = prepare(text);
            assert_eq!(chain.independent_create_relation(), None, "{text}");
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let next = AtomicU64::new(100);
            let (stats, output, _) = txn
                .execute_graph_write_chain_governed(
                    &mut db,
                    &contexts.query(),
                    &chain,
                    generous(),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(
                (stats.created_vertices, stats.created_edges),
                (4, edge_count)
            );
            assert!(output.value.is_empty());
            for (label, expected) in [(ITEM, [1, 2]), (OUTER, [11, 12])] {
                let props: Vec<_> = txn
                    .vertices(&db)
                    .unwrap()
                    .into_iter()
                    .filter(|vertex| vertex.labels == vec![label])
                    .map(|vertex| vertex.props)
                    .collect();
                assert_eq!(
                    props,
                    expected
                        .into_iter()
                        .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                        .collect::<Vec<_>>()
                );
            }
            for edge in txn.edges(&db).unwrap() {
                assert_eq!(
                    txn.vertex(&db, edge.entry.src).unwrap().unwrap().labels,
                    vec![ITEM]
                );
                assert_eq!(
                    txn.vertex(&db, edge.entry.dst).unwrap().unwrap().labels,
                    vec![OUTER]
                );
            }
            txn.finish(&mut db, &contexts.commit()).await.unwrap();
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
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
fn separate_set_clauses_read_prior_effects_but_one_clause_keeps_conflicts_atomic() {
    let ((), report) = run_async_under_lab(0xf0ec_0026, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![COUNTER],
            vec![(P, CanonicalScalar::Int(2)), (Q, CanonicalScalar::Int(7))],
        );
        db.write(&contexts.commit(), seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "MATCH (n:Counter) FOREACH (x IN [1] | SET n.p=n.p+1 \
             SET n.p=n.p*10 REMOVE n.q SET n.q=n.p+5) RETURN n.p,n.q",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH, 0, 4),
                |_| -> Result<ElementId, ()> { panic!("SET reserves no identity") },
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (0, 0));
        assert_eq!(receipt.stats().mutation_effects, 4);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(30)),
                GraphValue::Scalar(CanonicalScalar::Int(35)),
            ]
        );
        let expected = vec![
            (P, CanonicalScalar::Int(30).into()),
            (Q, CanonicalScalar::Int(35).into()),
        ];
        assert_eq!(txn.vertex(&db, VId(1)).unwrap().unwrap().props, expected);
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props, expected);

        let committed = db.frontier().unwrap();
        let chain = prepare("MATCH (n:Counter) FOREACH (x IN [1] | SET n.p=2,n.p=3) RETURN n.p");
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let error = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |_| -> Result<ElementId, ()> { panic!("a conflicting SET reserves no identity") },
            )
            .expect_err("comma-separated assignments remain one simultaneous proposal");
        assert!(matches!(
            error,
            fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Mutation(
                fgdb_gql::GraphMutationQueryError::Mutation(
                    fgdb_gql::GraphMutationError::ConflictingAssignment { .. }
                )
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertices(&db).unwrap().len(), 2);
        assert_eq!(txn.vertex(&db, VId(1)).unwrap().unwrap().props, expected);
        assert_eq!(db.frontier().unwrap(), committed);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 1);
        txn.abort();
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn plain_repeated_set_uses_native_chain_and_one_clause_conflicts_remain_atomic() {
    let ((), report) = run_async_under_lab(0xf0ec_0028, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![COUNTER], vec![(P, CanonicalScalar::Int(2))]);
        db.write(&contexts.commit(), seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let text = "MATCH (n:Counter) SET n.p=n.p+1 SET n.p=n.p*10 RETURN n.p AS value";
        assert!(PreparedGraphWriteChainText::is_chain(text).unwrap());
        let allowance =
            GraphWriteProgramPolicy::new(GqlQueryPolicy::new(HIGH, 1, HIGH, HIGH), 2, 0, 0);
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                text,
                &GqlParameters::new(),
                Catalog,
                R,
                allowance,
                |_| -> Result<ElementId, ()> { panic!("plain SET reserves no identity") },
            )
            .await
            .unwrap();
        let QueryResult::Rows { columns, rows } = result else {
            panic!("the public mutation query must retain RETURN");
        };
        assert_eq!(columns, ["value"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            [QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                30
            )))]
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        let expected = db.vertices().unwrap();
        assert_eq!(expected.len(), 1);
        assert_eq!(
            expected[0].props,
            vec![(P, CanonicalScalar::Int(30).into())]
        );

        let committed = db.frontier().unwrap();
        let text = "MATCH (n:Counter) SET n.p=2,n.p=3 RETURN n.p";
        assert!(!PreparedGraphWriteChainText::is_chain(text).unwrap());
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let error = txn
            .query_write(
                &mut db,
                &contexts.query(),
                text,
                &GqlParameters::new(),
                Catalog,
                R,
                allowance,
                |_| -> Result<ElementId, ()> { panic!("a conflicting SET reserves no identity") },
            )
            .expect_err("the public route keeps one-clause conflicting assignments strict");
        assert!(matches!(
            error,
            fgdb::QueryWriteError::Mutation(fgdb_gql::GqlQueryError::Source(
                fgdb_gql::GraphMutationQueryError::Mutation(
                    fgdb_gql::GraphMutationError::ConflictingAssignment { .. }
                )
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertices(&db).unwrap().len(), 2);
        assert_eq!(db.vertices().unwrap(), expected);
        assert_eq!(db.frontier().unwrap(), committed);
        drop(txn);
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), expected);
        assert_eq!(reopened.frontier().unwrap(), committed);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
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

#[test]
fn vertex_merges_reuse_staged_keys_and_keep_every_body_local_match() {
    let ((), report) = run_async_under_lab(0xf0ec_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (seed, members, expected_vertices, expected_logs, effects) in [
            (
                Vec::new(),
                "[1,2,1]",
                vec![(1, 11), (2, 10)],
                vec![10, 10, 11],
                3,
            ),
            (
                vec![(1, 5), (1, 7)],
                "[1,1]",
                vec![(1, 7), (1, 9)],
                vec![6, 7, 8, 9],
                4,
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            if !seed.is_empty() {
                let mut batch = WriteBatch::new(R);
                for (index, &(p, q)) in seed.iter().enumerate() {
                    batch.create_vertex(
                        VId(index as u128 + 1),
                        vec![ITEM],
                        vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
                    );
                }
                db.write(&contexts.commit(), batch).await.unwrap();
            }
            let basis = db.frontier().unwrap();
            let chain = prepare(&format!(
                "FOREACH (x IN {members} | MERGE (n:F {{p:x}}) \
                 ON CREATE SET n.q=10 ON MATCH SET n.q=n.q+1 \
                 CREATE (:Outer {{p:n.q}})) RETURN 42 AS done",
            ));
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
            assert_eq!(
                stats.created_vertices as usize,
                expected_logs.len() + expected_vertices.len() - seed.len()
            );
            assert_eq!(stats.created_edges, 0);
            assert_eq!(receipt.stats().mutation_effects, effects);
            assert_eq!(receipt.stats().completed_statements, 1);
            assert_eq!(
                rows.value.len(),
                1,
                "native MERGE matches stay inside the loop"
            );
            assert_eq!(
                rows.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(42))]
            );
            let vertices = txn.vertices(&db).unwrap();
            let mut found: Vec<_> = vertices
                .iter()
                .filter(|row| row.labels.contains(&ITEM))
                .map(|row| row.props.clone())
                .collect();
            found.sort();
            assert_eq!(
                found,
                expected_vertices
                    .into_iter()
                    .map(|(p, q)| vec![
                        (P, CanonicalScalar::Int(p).into()),
                        (Q, CanonicalScalar::Int(q).into()),
                    ])
                    .collect::<Vec<_>>()
            );
            let mut logs: Vec<_> = vertices
                .into_iter()
                .filter(|row| row.labels.contains(&OUTER))
                .map(|row| row.props)
                .collect();
            logs.sort();
            assert_eq!(
                logs,
                expected_logs
                    .into_iter()
                    .map(|value| vec![(P, CanonicalScalar::Int(value).into()),])
                    .collect::<Vec<_>>()
            );
            txn.finish(&mut db, &contexts.commit()).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        }
        assert!(
            PreparedGraphWriteChainText::prepare(
                "FOREACH (x IN [1] | MERGE (local:F {p:x})) RETURN local",
                R,
                symbols,
            )
            .is_err()
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relationship_merges_visit_parallel_matches_and_share_the_effect_allowance() {
    let ((), report) = run_async_under_lab(0xf0ec_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, p) in [(1, 1), (2, 2)] {
            seed.create_vertex(VId(id), vec![COUNTER], vec![(P, CanonicalScalar::Int(p))]);
        }
        for (id, src, dst, q) in [(10, 1, 2, 5), (11, 1, 2, 7), (12, 2, 1, 50), (13, 1, 1, 60)] {
            seed.add_edge(
                EId(id),
                VId(src),
                VId(dst),
                vec![(Q, CanonicalScalar::Int(q))],
            );
        }
        db.write(&contexts.commit(), seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "MATCH (a:Counter {p:1}), (b:Counter {p:2}) \
             FOREACH (delta IN [1,2] | MERGE (a)-[r:R]->(b) \
             ON MATCH SET r.q=r.q+delta CREATE (:Outer {p:r.q})) RETURN a.p",
        );
        let next = AtomicU64::new(100);
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![OUTER], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        txn.execute_graph_write_chain_governed(
            &mut db,
            &contexts.query(),
            &chain,
            policy(1, HIGH, HIGH, 100, 3),
            |request| allocate(&next, request),
        )
        .expect_err("two matches in each of two iterations require four effects");
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        for (eid, value) in [(10, 5), (11, 7), (12, 50), (13, 60)] {
            assert_eq!(
                txn.edge(&db, EId(eid)).unwrap().unwrap().props,
                vec![(Q, CanonicalScalar::Int(value).into())]
            );
        }
        drop(txn);
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH, 4, 4),
                |request| {
                    assert!(
                        matches!(request, GraphInsertRequest::Vertex { .. }),
                        "matched relationships must never allocate edges"
                    );
                    allocate(&next, request)
                },
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (4, 0));
        assert_eq!(receipt.stats().mutation_effects, 4);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(1))]
        );
        for (eid, value) in [(10, 8), (11, 10), (12, 50), (13, 60)] {
            assert_eq!(
                txn.edge(&db, EId(eid)).unwrap().unwrap().props,
                vec![(Q, CanonicalScalar::Int(value).into())]
            );
        }
        let mut logs: Vec<_> = txn
            .vertices(&db)
            .unwrap()
            .into_iter()
            .filter(|row| row.labels.contains(&OUTER))
            .map(|row| row.props)
            .collect();
        logs.sort();
        assert_eq!(
            logs,
            [6, 8, 8, 10]
                .into_iter()
                .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                .collect::<Vec<_>>()
        );
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_creation_limits_and_late_branch_errors_restore_the_prior_transaction() {
    let ((), report) = run_async_under_lab(0xf0ec_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vertex = "FOREACH (x IN [1,2,1] | MERGE (n:F {p:x}) \
                      ON CREATE SET n.q=10 ON MATCH SET n.q=n.q+1) RETURN 0";
        let vertex_error = "FOREACH (x IN [1,2,1] | MERGE (n:F {p:x}) \
                            ON CREATE SET n.q=10 ON MATCH SET n.q=1/(x-1)) RETURN 0";
        let edge = "CREATE (a:Counter {p:1}), (b:Counter {p:2}) \
                    FOREACH (x IN [1,2,1] | MERGE (a)-[r:R {p:x}]->(b) \
                    ON CREATE SET r.q=1 ON MATCH SET r.q=r.q+1) RETURN 0";
        let edge_error = "CREATE (a:Counter {p:1}), (b:Counter {p:2}) \
                          FOREACH (x IN [1,2,1] | MERGE (a)-[r:R {p:x}]->(b) \
                          ON CREATE SET r.q=1 ON MATCH SET r.q=1/(x-1)) RETURN 0";
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let next = AtomicU64::new(100);
        let mut exact = policy(1, HIGH, HIGH, 2, 3);
        exact.max_edges = 2;
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &prepare(edge),
                exact,
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
        assert_eq!(receipt.stats().mutation_effects, 3);
        assert_eq!(rows.value.len(), 1);
        let mut edges: Vec<_> = txn
            .edges(&db)
            .unwrap()
            .into_iter()
            .map(|row| row.props)
            .collect();
        edges.sort();
        assert_eq!(
            edges,
            [(1, 2), (2, 1)]
                .into_iter()
                .map(|(p, q)| vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(q).into()),
                ])
                .collect::<Vec<_>>()
        );
        txn.finish(&mut db, &contexts.commit()).await.unwrap();

        let mut one_edge = generous();
        one_edge.max_edges = 1;
        for (text, allowance) in [
            (vertex, policy(1, HIGH, HIGH, 1, 100)),
            (vertex, policy(1, HIGH, HIGH, 100, 2)),
            (vertex_error, generous()),
            (edge, one_edge),
            (edge, policy(1, HIGH, HIGH, 100, 2)),
            (edge_error, generous()),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(7), vec![OUTER], vec![(P, CanonicalScalar::Int(99))]);
            txn.write(&mut db, prefix).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let next = AtomicU64::new(100);
            txn.execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &prepare(text),
                allowance,
                |request| allocate(&next, request),
            )
            .expect_err("a MERGE body must retain one creation/effect budget and rollback guard");
            assert!(
                next.load(Ordering::Relaxed) > 100,
                "the refusal must occur after prior loop work"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(txn.vertices(&db).unwrap().len(), 1);
            assert!(txn.edges(&db).unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), basis);
            txn.finish(&mut db, &contexts.commit()).await.unwrap();
            assert_eq!(db.vertices().unwrap().len(), 1);
            assert!(db.edges().unwrap().is_empty());
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_pattern_keys_refresh_staged_elements_but_preserve_scalar_aliases() {
    let ((), report) = run_async_under_lab(0xf0ec_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "CREATE (counter:Counter {p:0}) WITH counter, counter.p AS frozen \
             FOREACH (x IN [1,2,3] | MERGE (n:F {p:counter.p}) \
             MERGE (counter)-[r:R {p:counter.p}]->(n) SET counter.p=counter.p+1) \
             RETURN counter.p AS current, frozen",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let next = AtomicU64::new(100);
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH, 4, 3),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (4, 3));
        assert_eq!(receipt.stats().mutation_effects, 3);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(3)),
                GraphValue::Scalar(CanonicalScalar::Int(0)),
            ]
        );
        let mut keys: Vec<_> = txn
            .vertices(&db)
            .unwrap()
            .into_iter()
            .filter(|row| row.labels.contains(&ITEM))
            .map(|row| row.props)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            (0..3)
                .map(|p| vec![(P, CanonicalScalar::Int(p).into())])
                .collect::<Vec<_>>()
        );
        let edges = txn.edges(&db).unwrap();
        assert_eq!(edges.len(), 3);
        for edge in edges {
            assert_eq!(edge.entry.src, VId(100));
            assert_eq!(edge.entry.relation, R);
            let destination = txn.vertex(&db, edge.entry.dst).unwrap().unwrap();
            assert_eq!(destination.labels, vec![ITEM]);
            assert_eq!(
                edge.props, destination.props,
                "both pattern keys must read this iteration's counter"
            );
        }
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_merge_selected_on_values_read_carried_properties_and_metadata() {
    let ((), report) = run_async_under_lab(0xf0ec_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        let list =
            |values: &[i64]| GraphValue::List(values.iter().map(|&value| int(value)).collect());
        for matched in [false, true] {
            for (expression, expected) in [
                ("source.p", int(7)),
                ("source.q", list(&[8, 9])),
                ("[source.p, source.q[1]]", list(&[7, 9])),
                ("properties(source).p", int(7)),
            ] {
                let mut db = Database::open_memory(&contexts.commit(), keys())
                    .await
                    .unwrap();
                let mut seed = WriteBatch::new(R);
                seed.create_vertex_values(
                    VId(1),
                    vec![COUNTER],
                    vec![
                        (P, CanonicalScalar::Int(7).into()),
                        (
                            Q,
                            CanonicalPropertyValue::List(
                                CanonicalList::try_new(vec![
                                    CanonicalScalar::Int(8).into(),
                                    CanonicalScalar::Int(9).into(),
                                ])
                                .unwrap(),
                            ),
                        ),
                    ],
                );
                if matched {
                    seed.create_vertex(
                        VId(2),
                        vec![ITEM],
                        vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(99))],
                    );
                }
                db.write(&contexts.commit(), seed).await.unwrap();
                let (create, update) = if matched {
                    ("source.p/0", expression)
                } else {
                    (expression, "source.p/0")
                };
                let chain = prepare(&format!(
                    "MATCH (source:Counter) MERGE (target:F {{p:1}}) \
                     ON CREATE SET target.q={create} ON MATCH SET target.q={update} \
                     RETURN target.q",
                ));
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
                    .expect("only the selected ON branch may read and evaluate its value");
                assert_eq!(stats.created_vertices, u64::from(!matched));
                assert_eq!(receipt.stats().mutation_effects, 1);
                assert_eq!(rows.value.len(), 1);
                assert_eq!(
                    rows.value[0].values(),
                    std::slice::from_ref(&expected),
                    "{expression}"
                );
                let target = if matched { VId(2) } else { VId(100) };
                let properties = vec![
                    (P, CanonicalScalar::Int(1).into()),
                    (Q, expected.to_property_value().unwrap()),
                ];
                assert_eq!(txn.vertex(&db, target).unwrap().unwrap().props, properties);
                txn.finish(&mut db, &contexts.commit()).await.unwrap();
                assert_eq!(db.vertex(target).unwrap().unwrap().props, properties);
            }
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_metadata_without_a_complete_catalog_refuses_before_reserving_identity() {
    let ((), report) = run_async_under_lab(0xf0ec_0027, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![COUNTER],
            vec![(P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(8))],
        );
        db.write(&contexts.commit(), seed).await.unwrap();
        let before = db.vertices().unwrap();
        let basis = db.frontier().unwrap();
        // An ordinary resolver knows the mentioned tokens. It cannot enumerate
        // all property names for properties(source), even when p is mentioned.
        let chain = PreparedGraphWriteChainText::prepare(
            "MATCH (source:Counter) MERGE (target:F {p:1}) \
             ON CREATE SET target.q=properties(source).p \
             ON MATCH SET target.q=0 RETURN target.q",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let error = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |_| -> Result<ElementId, ()> {
                    panic!("missing catalog metadata must refuse before identity reservation")
                },
            )
            .expect_err("metadata reads require a complete trusted reverse catalog");
        assert!(matches!(
            error,
            fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphWriteChainError::VertexMerge(
                fgdb_gql::GraphVertexMergeRowsQueryError::Merge(
                    fgdb_gql::GraphVertexMergeError::InputRelation(
                        fgdb_gql::GraphSetExecutionError::Source(fgdb::WriteTxnError::Read(
                            fgdb::ReadError::UnmappedProperty(P)
                        ))
                    )
                )
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertices(&db).unwrap(), before);
        assert_eq!(db.vertices().unwrap(), before);
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(contexts.txn().outstanding_obligations(), 1);
        txn.abort();
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_merge_on_values_refresh_between_foreach_iterations_and_keep_scalar_aliases() {
    let ((), report) = run_async_under_lab(0xf0ec_0016, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "CREATE (source:Counter {p:10,q:[4,5]}) WITH source, source.p AS frozen \
             FOREACH (x IN [1,2,1] | MERGE (target:F {p:x}) \
             ON CREATE SET target.q=[source.p,source.q[0],frozen] \
             ON MATCH SET target.q=[source.p,source.q[1],frozen] \
             SET source.p=source.p+1) RETURN source.p, frozen",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let next = AtomicU64::new(100);
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH, 3, 6),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (3, 0));
        assert_eq!(receipt.stats().mutation_effects, 6);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(13)),
                GraphValue::Scalar(CanonicalScalar::Int(10)),
            ]
        );
        let mut copied: Vec<_> = txn
            .vertices(&db)
            .unwrap()
            .into_iter()
            .filter(|vertex| vertex.labels == [ITEM])
            .map(|vertex| vertex.props)
            .collect();
        copied.sort();
        assert_eq!(
            copied,
            [(1, [12, 5, 10]), (2, [11, 4, 10])]
                .into_iter()
                .map(|(p, values)| vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (
                        Q,
                        CanonicalPropertyValue::List(
                            CanonicalList::try_new(
                                values
                                    .into_iter()
                                    .map(|value| CanonicalScalar::Int(value).into())
                                    .collect(),
                            )
                            .unwrap(),
                        ),
                    ),
                ])
                .collect::<Vec<_>>()
        );
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_merge_carried_aliases_keep_the_input_cut_when_the_target_is_the_same_vertex() {
    let ((), report) = run_async_under_lab(0xf0ec_0017, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![ITEM],
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(10))],
        );
        db.write(&contexts.commit(), seed).await.unwrap();
        let chain = prepare(
            "MATCH (source:F) WITH source UNWIND [1,2] AS occurrence \
             MERGE (target:F {p:1}) ON MATCH SET target.q=source.q+1 \
             RETURN target.q",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |_| -> Result<ElementId, ()> { panic!("both rows match the existing vertex") },
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(receipt.stats().mutation_effects, 2);
        assert_eq!(rows.value.len(), 2);
        for row in rows.value {
            assert_eq!(
                row.values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(11))]
            );
        }
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (P, CanonicalScalar::Int(1).into()),
                (Q, CanonicalScalar::Int(11).into()),
            ]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn no_return_merge_values_and_keys_read_earlier_created_vertices() {
    let ((), report) = run_async_under_lab(0xf0ec_0018, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (key, value, expected_key, expected_value) in [
            ("1", "source.p", 1, CanonicalScalar::Int(7).into()),
            (
                "1",
                "properties(source).p",
                1,
                CanonicalScalar::Int(7).into(),
            ),
            (
                "source.p",
                "source.q",
                7,
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(vec![
                        CanonicalScalar::Int(8).into(),
                        CanonicalScalar::Int(9).into(),
                    ])
                    .unwrap(),
                ),
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let chain = prepare(&format!(
                "MERGE (source:Counter {{p:7}}) ON CREATE SET source.q=[8,9] \
                 MERGE (target:F {{p:{key}}}) ON CREATE SET target.q={value}",
            ));
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let next = AtomicU64::new(100);
            let (stats, rows, receipt) = txn
                .execute_graph_write_chain_governed(
                    &mut db,
                    &contexts.query(),
                    &chain,
                    policy(0, HIGH, HIGH, 2, 2),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!((stats.created_vertices, stats.created_edges), (2, 0));
            assert_eq!(receipt.stats().mutation_effects, 2);
            assert!(rows.value.is_empty());
            let expected = vec![
                (P, CanonicalScalar::Int(expected_key).into()),
                (Q, expected_value),
            ];
            assert_eq!(txn.vertex(&db, VId(101)).unwrap().unwrap().props, expected);
            assert!(db.vertices().unwrap().is_empty());
            txn.finish(&mut db, &contexts.commit()).await.unwrap();
            assert_eq!(db.vertex(VId(101)).unwrap().unwrap().props, expected);
            assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relationship_merge_endpoint_padding_preserves_hidden_body_fields() {
    let ((), report) = run_async_under_lab(0xf0ec_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "CREATE (a:Counter {p:1}) FOREACH (x IN [1] | \
             CREATE (c:F {p:a.p}) \
             MERGE (a)-[r:R {p:a.p+x}]->(b:Outer {q:c.p}) \
             ON CREATE SET r.q=b.q SET a.q=r.q) RETURN a.p, a.q",
        );
        let next = AtomicU64::new(100);
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut allowance = policy(1, HIGH, HIGH, 3, 2);
        allowance.max_edges = 1;
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                allowance,
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (3, 1));
        assert_eq!(receipt.stats().mutation_effects, 2);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(1)),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
            ]
        );
        let vertices = txn.vertices(&db).unwrap();
        assert_eq!(vertices.len(), 3);
        let copied = vertices
            .iter()
            .find(|vertex| vertex.labels == [ITEM])
            .unwrap();
        assert_eq!(copied.props, vec![(P, CanonicalScalar::Int(1).into())]);
        let edges = txn.edges(&db).unwrap();
        assert_eq!(edges.len(), 1);
        let edge = &edges[0];
        assert_eq!(edge.entry.relation, R);
        assert_eq!(
            edge.props,
            vec![
                (P, CanonicalScalar::Int(2).into()),
                (Q, CanonicalScalar::Int(1).into()),
            ]
        );
        let source = txn.vertex(&db, edge.entry.src).unwrap().unwrap();
        let destination = txn.vertex(&db, edge.entry.dst).unwrap().unwrap();
        assert_eq!(source.labels, vec![COUNTER]);
        assert_eq!(
            source.props,
            vec![
                (P, CanonicalScalar::Int(1).into()),
                (Q, CanonicalScalar::Int(1).into()),
            ]
        );
        assert_eq!(destination.labels, vec![OUTER]);
        assert_eq!(destination.props, vec![(Q, CanonicalScalar::Int(1).into())]);
        assert!(db.vertices().unwrap().is_empty());
        assert!(db.edges().unwrap().is_empty());
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(db.edge(edge.entry.eid).unwrap().unwrap().props, edge.props);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relationship_merge_on_carried_vertex_cannot_read_its_own_relationship() {
    // The ON action runs inside MERGE. A following standalone SET has the
    // post-MERGE relationship reader, exercised by the positive test above.
    let error = PreparedGraphWriteChainText::prepare(
        "CREATE (a:Counter),(b:Outer) FOREACH (x IN [1] | \
         MERGE (a)-[r:R {p:x}]->(b) ON CREATE SET a.q=r.p)",
        R,
        Catalog,
    )
    .expect_err("an ON action on a carried vertex cannot read the MERGE relationship");
    assert!(
        error
            .to_string()
            .contains("a value that does not read the MERGE relationship"),
        "{error:?}"
    );
}

#[test]
fn nested_shadowing_and_empty_children_restore_parent_continuations() {
    let ((), report) = run_async_under_lab(0xf0ec_0020, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "WITH 99 AS x FOREACH (x IN [1,2] | \
             CREATE (parent:Outer {p:x,q:0}) \
             FOREACH (x IN [10,20] | CREATE (child:F {p:parent.p,q:x}) \
             SET parent.q=parent.q+1) \
             FOREACH (x IN [] | SET parent.q=1000) \
             SET parent.q=parent.q+x CREATE (:Counter {p:x,q:parent.q})) RETURN x",
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
        assert_eq!((stats.created_vertices, stats.created_edges), (8, 0));
        assert_eq!(receipt.stats().mutation_effects, 6);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(99))]
        );
        let vertices = txn.vertices(&db).unwrap();
        for label in [OUTER, COUNTER] {
            let mut found: Vec<_> = vertices
                .iter()
                .filter(|vertex| vertex.labels.contains(&label))
                .map(|vertex| vertex.props.clone())
                .collect();
            found.sort();
            assert_eq!(
                found,
                [(1, 3), (2, 4)]
                    .into_iter()
                    .map(|(p, q)| vec![
                        (P, CanonicalScalar::Int(p).into()),
                        (Q, CanonicalScalar::Int(q).into()),
                    ])
                    .collect::<Vec<_>>()
            );
        }
        let mut children: Vec<_> = vertices
            .into_iter()
            .filter(|vertex| vertex.labels.contains(&ITEM))
            .map(|vertex| vertex.props)
            .collect();
        children.sort();
        assert_eq!(
            children,
            [(1, 10), (1, 20), (2, 10), (2, 20)]
                .into_iter()
                .map(|(p, q)| vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(q).into()),
                ])
                .collect::<Vec<_>>()
        );
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn child_collections_refresh_between_merge_matches_and_remain_fixed_while_running() {
    let ((), report) = run_async_under_lab(0xf0ec_0021, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [1, 2] {
            seed.create_vertex(
                VId(id),
                vec![ITEM],
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(0))],
            );
        }
        db.write(&contexts.commit(), seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "CREATE (counter:Counter {p:[1,2]}) \
             FOREACH (round IN [1,2] | MERGE (n:F {p:1}) \
             FOREACH (value IN counter.p | CREATE (:Outer {p:n.q,q:value}) \
             SET counter.p=[9] SET n.q=n.q+1) SET n.q=n.q+100) \
             RETURN size(counter.p)",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let next = AtomicU64::new(100);
        let (stats, rows, _) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH, 100, 100),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (6, 0));
        assert_eq!(
            rows.value.len(),
            1,
            "child and MERGE rows must stay inside their parents"
        );
        assert_eq!(
            rows.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(1))]
        );
        let vertices = txn.vertices(&db).unwrap();
        let mut matched: Vec<_> = vertices
            .iter()
            .filter(|vertex| vertex.labels.contains(&ITEM))
            .map(|vertex| vertex.props.clone())
            .collect();
        matched.sort();
        assert_eq!(
            matched,
            [202, 203]
                .into_iter()
                .map(|q| vec![
                    (P, CanonicalScalar::Int(1).into()),
                    (Q, CanonicalScalar::Int(q).into()),
                ])
                .collect::<Vec<_>>()
        );
        let mut visits: Vec<_> = vertices
            .into_iter()
            .filter(|vertex| vertex.labels.contains(&OUTER))
            .map(|vertex| vertex.props)
            .collect();
        visits.sort();
        // The first child invocation retains both original members. Every
        // later MERGE occurrence sees the replacement singleton collection.
        assert_eq!(
            visits,
            [(0, 1), (0, 9), (1, 2), (101, 9), (102, 9)]
                .into_iter()
                .map(|(p, q)| vec![
                    (P, CanonicalScalar::Int(p).into()),
                    (Q, CanonicalScalar::Int(q).into()),
                ])
                .collect::<Vec<_>>()
        );
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_inner_failure_restores_the_transaction_prefix_and_earlier_parent_effects() {
    let ((), report) = run_async_under_lab(0xf0ec_0022, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![COUNTER], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&contexts.commit(), seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![OUTER], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let chain = prepare(
            "MATCH (counter:Counter) FOREACH (round IN [1,2] | \
             SET counter.p=counter.p+round CREATE (:F {p:round}) \
             FOREACH (member IN [1,2] | CREATE (:F {p:round*10+member}) \
             SET counter.p=counter.p+1/(4-round-member)) \
             CREATE (:Outer {p:counter.p})) RETURN counter.p",
        );
        let next = AtomicU64::new(100);
        txn.execute_graph_write_chain_governed(
            &mut db,
            &contexts.query(),
            &chain,
            generous(),
            |request| allocate(&next, request),
        )
        .expect_err("an inner failure must unwind every enclosing FOREACH workspace");
        assert_eq!(
            next.load(Ordering::Relaxed),
            107,
            "the first parent and three inner iterations completed"
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(txn.vertices(&db).unwrap().len(), 2);
        assert_eq!(
            txn.vertex(&db, VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(0).into())]
        );
        assert_eq!(
            txn.vertex(&db, VId(7)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(99).into())]
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap().len(), 1);
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(0).into())]
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreach_delete_preserves_identity_and_captured_values_and_reopens() {
    let ((), report) = run_async_under_lab(0xf0ec_0021, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![COUNTER], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let chain = prepare(
            "MATCH (n:Counter) WITH n,n.p AS captured \
             FOREACH (target IN [n,NULL,n] | DELETE target) RETURN n,captured,[n]",
        );
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &query,
                &chain,
                generous(),
                |_| -> Result<ElementId, ()> { panic!("deletion reserves no identity") },
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert_eq!(receipt.stats().mutation_effects, 2);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(7)),
                GraphValue::List(vec![GraphValue::Vertex(VId(1))].into()),
            ]
        );
        assert!(txn.vertices(&db).unwrap().is_empty());
        assert_eq!(db.vertices().unwrap().len(), 1);
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert_eq!(pinned.vertices().unwrap().len(), 1);
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(reopened.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreach_local_create_update_delete_and_nested_continuation_are_one_write() {
    let ((), report) = run_async_under_lab(0xf0ec_0022, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let chain = prepare(
            "FOREACH (x IN [1,2] | CREATE (local:F {p:x}) SET local.q=x \
             DELETE local FOREACH (y IN [x] | CREATE (:Outer {p:y})))",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let next = AtomicU64::new(100);
        let (stats, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                policy(0, HIGH, HIGH, 4, 4),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 4);
        assert_eq!(receipt.stats().mutation_effects, 4);
        assert!(rows.value.is_empty());
        assert_eq!(next.load(Ordering::Relaxed), 104);
        let vertices = txn.vertices(&db).unwrap();
        assert_eq!(
            vertices.iter().map(|vertex| vertex.vid).collect::<Vec<_>>(),
            [VId(101), VId(103)]
        );
        for (vertex, value) in vertices.iter().zip([1, 2]) {
            assert_eq!(vertex.labels, [OUTER]);
            assert_eq!(vertex.props, vec![(P, CanonicalScalar::Int(value).into())]);
        }
        let published_at = CommitSeq(basis.0 + 1);
        let mut expected = vertices;
        for vertex in &mut expected {
            assert_eq!(
                vertex.created_at, basis,
                "staged rows use the transaction basis"
            );
            vertex.created_at = published_at;
        }
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), published_at);
        assert_eq!(db.vertices().unwrap(), expected);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreach_detach_cascades_self_parallel_and_incoming_edges_before_repeated_deletes() {
    let ((), report) = run_async_under_lab(0xf0ec_0023, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![COUNTER], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![OUTER], vec![(P, CanonicalScalar::Int(2))]);
        for (edge, source, destination) in [(1, 1, 1), (2, 1, 2), (3, 1, 2), (4, 2, 1)] {
            seed.add_edge(EId(edge), VId(source), VId(destination), vec![]);
        }
        db.write(&contexts.commit(), seed).await.unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let chain = prepare(
            "MATCH (n:Counter)-[r:R]->(m:Outer) WITH n,collect(r) AS edges \
             FOREACH (x IN [1,2] | DETACH DELETE n FOREACH (r IN edges | DELETE r)) \
             RETURN n,size(edges)",
        );
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let (_, rows, receipt) = txn
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &chain,
                generous(),
                |_| -> Result<ElementId, ()> { panic!("deletion reserves no identity") },
            )
            .unwrap();
        assert_eq!(receipt.stats().mutation_effects, 6);
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(2))
            ]
        );
        assert!(txn.edges(&db).unwrap().is_empty());
        assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
        assert_eq!(txn.vertices(&db).unwrap().len(), 1);
        txn.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert!(db.edges().unwrap().is_empty());
        assert_eq!(pinned.edges().unwrap().len(), 4);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreach_delete_failures_restore_the_entry_workspace_and_reads_remain_lazy() {
    let ((), report) = run_async_under_lab(0xf0ec_0024, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (text, incident, effects, succeeds) in [
            (
                "MATCH (n:Counter) FOREACH (target IN [n,1] | DELETE target)",
                false,
                1000,
                false,
            ),
            (
                "MATCH (n:Counter) FOREACH (x IN [1] | DELETE n)",
                true,
                1000,
                false,
            ),
            (
                "MATCH (n:Counter) FOREACH (x IN [1,2] | DELETE n)",
                false,
                1,
                false,
            ),
            (
                "MATCH (n:Counter) FOREACH (x IN [1] | DELETE n) RETURN n.p",
                false,
                1000,
                false,
            ),
            (
                "MATCH (n:Counter) FOREACH (x IN [1] | DELETE n) RETURN properties(n).p",
                false,
                1000,
                false,
            ),
            (
                "MATCH (n:Counter) WITH n,n.p AS captured FOREACH (x IN [1] | DELETE n) \
              RETURN CASE WHEN false THEN n.p ELSE captured END",
                false,
                1000,
                true,
            ),
            (
                "MATCH (n:Counter) WITH n,n.p AS captured FOREACH (x IN [1] | DELETE n) \
              RETURN CASE WHEN false THEN properties(n).p ELSE captured END",
                false,
                1000,
                true,
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![COUNTER], vec![(P, CanonicalScalar::Int(7))]);
            if incident {
                seed.add_edge(EId(1), VId(1), VId(1), vec![]);
            }
            db.write(&contexts.commit(), seed).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(50), vec![OUTER], vec![(P, CanonicalScalar::Int(99))]);
            txn.write(&mut db, prefix).unwrap();
            let entry = (
                txn.staged_effect_digest().unwrap(),
                txn.vertices(&db).unwrap(),
                txn.edges(&db).unwrap(),
            );
            let result = txn.execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &prepare(text),
                policy(HIGH, HIGH, HIGH, 0, effects),
                |_| -> Result<ElementId, ()> { panic!("deletion reserves no identity") },
            );
            if succeeds {
                let (_, rows, _) = result.unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(rows.value.len(), 1);
                assert_eq!(
                    rows.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(7))]
                );
                assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
            } else {
                result.expect_err(text);
                assert_eq!(
                    (
                        txn.staged_effect_digest().unwrap(),
                        txn.vertices(&db).unwrap(),
                        txn.edges(&db).unwrap()
                    ),
                    entry,
                    "{text}"
                );
            }
            assert_eq!(db.frontier().unwrap(), basis);
            txn.finish(&mut db, &contexts.commit()).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
            assert!(db.vertex(VId(50)).unwrap().is_some());
            assert_eq!(db.vertex(VId(1)).unwrap().is_none(), succeeds);
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
