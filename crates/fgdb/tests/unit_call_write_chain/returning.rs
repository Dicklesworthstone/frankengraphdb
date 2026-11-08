//! Returning bodies export a correlated bag through the native write owner.

use super::*;
use fgdb_gql::{GraphSymbolResolver, ReverseSymbolCatalog};

struct MetadataCatalog;

impl GraphSymbolResolver for MetadataCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(VISIBLE, "Visible");
        catalog.insert_label(HIDDEN, "Hidden");
        catalog.insert_relation(R, "R");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        catalog.insert_property(SECRET, "secret");
        Some(catalog)
    }
}

fn text_list(values: &[&str]) -> GraphValue {
    GraphValue::List(
        values
            .iter()
            .map(|value| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap()))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}

#[test]
fn returning_call_multiplies_outer_occurrences_and_exports_writable_identities() {
    let ((), report) = run_async_under_lab(0xca11_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let pinned = db.read_session().unwrap();
        let basis = db.frontier().unwrap();
        let mut next = 100_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "UNWIND [1,1,2,3] AS x CALL (x) { \
                 UNWIND [10,20] AS y WITH x,y WHERE x<3 \
                 CREATE (n:Visible {p:x*10+y}) RETURN n AS created,y AS bump } \
                 SET created.q=x+bump \
                 RETURN x,created.p AS p,created.q AS q ORDER BY x,p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(6),
                |_| {
                    next += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(next)))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            values(result, &["x", "p", "q"]),
            [
                [1, 20, 11],
                [1, 20, 11],
                [1, 30, 21],
                [1, 30, 21],
                [2, 30, 12],
                [2, 40, 22],
            ]
            .into_iter()
            .map(|row| row.into_iter().map(int).collect())
            .collect::<Vec<Vec<_>>>()
        );
        assert_eq!(next, 106);
        let expected = db.vertices().unwrap();
        assert_eq!(expected.len(), 6);
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert!(pinned.vertices().unwrap().is_empty());
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), expected);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nested_returning_and_unit_bodies_share_staged_reads_and_export_endpoint_types() {
    let ((), report) = run_async_under_lab(0xca11_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut next_vertex = 10_u128;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "WITH 99 AS marker CALL () { CREATE (n:Visible {p:1}) \
                 CALL (n) { SET n.q=2 RETURN n AS updated } \
                 CALL (updated) { SET updated.p=updated.p+1 } \
                 WITH updated MATCH (seen:Visible) \
                 RETURN updated AS parent,seen AS observed } \
                 CREATE (child:Visible {p:marker}) CREATE (parent)-[:R]->(child) \
                 RETURN parent.p AS p,parent.q AS q,observed=parent AS same,marker",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1),
                |request| {
                    Ok::<_, Infallible>(match request.request {
                        GraphInsertRequest::Vertex { .. } => {
                            next_vertex += 1;
                            ElementId::Vertex(VId(next_vertex))
                        }
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(30)),
                    })
                },
            )
            .await
            .unwrap();
        assert_eq!(
            values(result, &["p", "q", "same", "marker"]),
            vec![vec![
                int(2),
                int(2),
                GraphValue::Scalar(CanonicalScalar::Bool(true)),
                int(99),
            ]]
        );
        assert_eq!(next_vertex, 12);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.edges().unwrap().len(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_exports_do_not_spend_read_authority_or_public_rows() {
    let ((), report) = run_async_under_lab(0xca11_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::Write, 0), NOW).unwrap();
        let (_, rows, _) = db
            .execute_graph_write_chain_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &issuer,
                &token,
                "main",
                &program(
                    "CALL () { CREATE (n:Visible {p:1}) RETURN n AS created,7 AS copied } \
                     CREATE (:Visible {p:copied})",
                ),
                insert_policy(0),
                || NOW,
            )
            .await
            .unwrap();
        assert!(rows.value.is_empty());
        let mut properties: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|row| row.props)
            .collect();
        properties.sort();
        assert_eq!(
            properties,
            [1, 7]
                .into_iter()
                .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                .collect::<Vec<_>>()
        );
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn body_returns_are_private_until_the_final_aggregate_and_all_creations_are_cumulative() {
    let ((), report) = run_async_under_lab(0xca11_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let issuer = authority();
        let call = program(
            "UNWIND [1,2,3,4] AS x CALL (x) { CREATE (:Visible {p:x}) \
             WITH x UNWIND [x,x] AS duplicate RETURN duplicate AS copied } \
             RETURN count(*) AS total",
        );
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
        let (_, rows, _) = db
            .execute_graph_write_chain_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(1),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(rows.value.len(), 1);
        assert_eq!(rows.value[0].values(), &[int(8)]);
        assert_eq!(db.vertices().unwrap().len(), 4);

        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        db.execute_graph_write_chain_authorized(
            &contexts.txn(),
            &contexts.query(),
            &contexts.commit(),
            &issuer,
            &token,
            "main",
            &call,
            GraphInsertPolicy::new(insert_policy(1).query, 3, 0),
            || NOW,
        )
        .await
        .expect_err("each returned body shares the statement's creation allowance");
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());

        let call = program(
            "UNWIND [1,2,3,4] AS x CALL (x) { CREATE (:Visible {p:x}) \
             WITH x UNWIND [x,x] AS duplicate RETURN duplicate AS copied } \
             RETURN copied",
        );
        for (local_rows, granted_rows) in [(7, 8), (8, 7)] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, granted_rows), NOW)
                .unwrap();
            db.execute_graph_write_chain_authorized(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(local_rows),
                || NOW,
            )
            .await
            .expect_err("both the statement and permit cap the final exported bag");
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_outer_inputs_preflight_returned_body_reads_before_any_effect() {
    let ((), report) = run_async_under_lab(0xca11_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let issuer = authority();
        for (text, rights, denial) in [
            (
                "UNWIND [] AS x CALL (x) { CREATE (n:Visible) \
                 RETURN n.secret AS leaked } CREATE (:Visible {p:0})",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "UNWIND [] AS x CALL (x) { CREATE (n:Visible) WITH n \
                 MATCH (m:Visible) RETURN m.secret AS leaked } CREATE (:Visible {p:0})",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "UNWIND [] AS x CALL (x) { CREATE (n:Visible) WITH n \
                 MATCH (m:Visible) RETURN m AS seen } CREATE (:Visible {p:0})",
                Rights::Write,
                Error::PermissionDenied,
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let token = issuer.issue_at(&grant(rights, 0), NOW).unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &token,
                    "main",
                    &program(text),
                    insert_policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(denial), "{text}: {error:?}");
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_return_expression_failure_rolls_back_bodies_and_keeps_the_entry_prefix() {
    let ((), report) = run_async_under_lab(0xca11_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![VISIBLE], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let before = transaction.staged_effect_digest().unwrap();
        let mut allocated = 0;
        transaction
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &program(
                    "UNWIND [1,2] AS x CALL (x) { CREATE (n:Visible {p:x}) \
                     WITH n,x RETURN 10/(2-x) AS exported } RETURN exported",
                ),
                insert_policy(2),
                |_| {
                    allocated += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(100 + allocated)))
                },
            )
            .expect_err("a returned-row error must roll back earlier successful body exports");
        assert_eq!(allocated, 2, "both body writes reached identity allocation");
        assert_eq!(transaction.staged_effect_digest().unwrap(), before);
        assert_eq!(transaction.vertices(&db).unwrap().len(), 1);
        assert_eq!(
            transaction.vertex(&db, VId(7)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(99).into())]
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        transaction
            .finish(&mut db, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_returned_bags_preserve_body_effects_and_suppress_later_outer_writes() {
    let ((), report) = run_async_under_lab(0xca11_1007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut allocated = 0;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "UNWIND [1,2] AS x CALL (x) { CREATE (n:Visible {p:x}) WITH n \
                 MATCH (missing:Hidden) RETURN missing AS absent } \
                 CREATE (:Visible {p:99}) RETURN x",
                &GqlParameters::new(),
                symbols,
                R,
                policy(0),
                |_| {
                    allocated += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(100 + allocated)))
                },
            )
            .await
            .unwrap();
        assert!(values(result, &["x"]).is_empty());
        assert_eq!(
            allocated, 2,
            "both calls ran; the downstream CREATE did not"
        );
        let mut properties: Vec<_> = db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|row| row.props)
            .collect();
        properties.sort();
        assert_eq!(
            properties,
            [1, 2]
                .into_iter()
                .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                .collect::<Vec<_>>()
        );
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn standalone_returning_calls_deliver_their_schema_and_enforce_both_row_limits() {
    let ((), report) = run_async_under_lab(0xca11_1008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let issuer = authority();
        let call =
            program("CALL () { UNWIND [1,2] AS x CREATE (:Visible {p:x}) RETURN x AS copied }");
        assert_eq!(call.columns(), &["copied"]);
        assert!(call.returns_rows());
        for (local_rows, granted_rows, succeeds) in [(2, 2, true), (1, 2, false), (2, 1, false)] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, granted_rows), NOW)
                .unwrap();
            let result = db
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &token,
                    "main",
                    &call,
                    insert_policy(local_rows),
                    || NOW,
                )
                .await;
            if succeeds {
                let (_, rows, _) = result.unwrap();
                assert_eq!(
                    rows.value
                        .iter()
                        .map(|row| row.values().to_vec())
                        .collect::<Vec<_>>(),
                    vec![vec![int(1)], vec![int(2)]]
                );
                assert_eq!(db.vertices().unwrap().len(), 2);
                assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
            } else {
                result.expect_err("a standalone CALL has a public result boundary");
                assert_eq!(db.frontier().unwrap(), basis);
                assert!(db.vertices().unwrap().is_empty());
            }
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_body_write_inputs_still_execute_the_terminal_global_aggregate() {
    let ((), report) = run_async_under_lab(0xca11_1009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut allocated = 0;
        let result = db
            .query_write(
                &contexts.txn(),
                &contexts.query(),
                &contexts.commit(),
                "UNWIND [1,2] AS x CALL (x) { MATCH (n:Visible {p:x}) SET n.q=1 \
                 WITH n MATCH (m:Hidden) RETURN count(*) AS seen } \
                 RETURN x,seen ORDER BY x",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2),
                |_| {
                    allocated += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(100 + allocated)))
                },
            )
            .await
            .unwrap();
        assert_eq!(
            values(result, &["x", "seen"]),
            vec![vec![int(1), int(0)], vec![int(2), int(0)]]
        );
        assert_eq!(allocated, 0);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returned_identity_maps_and_metadata_read_the_completed_native_write() {
    let ((), report) = run_async_under_lab(0xca11_1010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        const SET: &str = "CALL { CREATE (n:Visible {p:1}) RETURN n } SET n.q=2 RETURN ";
        const INSERT: &str = "CALL { CREATE (n:Visible {p:1}) RETURN n } CREATE (:Visible) RETURN ";
        const EDGE: &str = "CALL { CREATE ()-[r:R {p:3}]->() RETURN r } SET r.q=4 RETURN ";
        const CAPTURED: &str = "CALL { CREATE (n:Visible {p:1}) RETURN n } \
            WITH n,n.p AS before SET n.q=2 RETURN ";
        const CAPTURED_INSERT: &str = "CALL { CREATE (n:Visible {p:1}) RETURN n } \
            WITH n,n.p AS before CREATE (:Visible) RETURN ";
        let map12 = GraphValue::map(vec![("p".into(), int(1)), ("q".into(), int(2))]).unwrap();
        let map34 = GraphValue::map(vec![("p".into(), int(3)), ("q".into(), int(4))]).unwrap();
        for (prefix, expression, expected, vertices, edges) in [
            (SET, "n{.p,.q}", map12.clone(), 1, 0),
            (SET, "n{.*}", map12.clone(), 1, 0),
            (SET, "properties(n)", map12.clone(), 1, 0),
            (SET, "keys(n)", text_list(&["p", "q"]), 1, 0),
            (SET, "labels(n)", text_list(&["Visible"]), 1, 0),
            (CAPTURED, "n{.p,.q}", map12.clone(), 1, 0),
            (CAPTURED, "n{.*}", map12.clone(), 1, 0),
            (CAPTURED, "properties(n)", map12, 1, 0),
            (CAPTURED, "keys(n)", text_list(&["p", "q"]), 1, 0),
            (CAPTURED, "labels(n)", text_list(&["Visible"]), 1, 0),
            (CAPTURED_INSERT, "n.p", int(1), 2, 0),
            (
                CAPTURED_INSERT,
                "n{.p}",
                GraphValue::map(vec![("p".into(), int(1))]).unwrap(),
                2,
                0,
            ),
            (
                CAPTURED_INSERT,
                "n{.*}",
                GraphValue::map(vec![("p".into(), int(1))]).unwrap(),
                2,
                0,
            ),
            (
                CAPTURED_INSERT,
                "properties(n)",
                GraphValue::map(vec![("p".into(), int(1))]).unwrap(),
                2,
                0,
            ),
            (CAPTURED_INSERT, "keys(n)", text_list(&["p"]), 2, 0),
            (CAPTURED_INSERT, "labels(n)", text_list(&["Visible"]), 2, 0),
            (
                INSERT,
                "n{.p}",
                GraphValue::map(vec![("p".into(), int(1))]).unwrap(),
                2,
                0,
            ),
            (EDGE, "r{.p,.q}", map34.clone(), 2, 1),
            (EDGE, "properties(r)", map34, 2, 1),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut next = 100_u128;
            let text = format!("{prefix}{expression} AS value");
            let result = db
                .query_write(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &text,
                    &GqlParameters::new(),
                    MetadataCatalog,
                    R,
                    policy(1),
                    |request| {
                        next += 1;
                        Ok::<_, Infallible>(match request.request {
                            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                            GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                        })
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(values(result, &["value"]), vec![vec![expected]], "{text}");
            assert_eq!(db.vertices().unwrap().len(), vertices, "{text}");
            assert_eq!(db.edges().unwrap().len(), edges, "{text}");
            assert_eq!(db.frontier().unwrap().0, 1, "{text}");
            if edges == 1 {
                assert_eq!(
                    db.edges().unwrap()[0].props,
                    vec![
                        (P, CanonicalScalar::Int(3).into()),
                        (Q, CanonicalScalar::Int(4).into()),
                    ]
                );
            } else {
                let mut actual: Vec<_> = db
                    .vertices()
                    .unwrap()
                    .into_iter()
                    .map(|row| row.props)
                    .collect();
                actual.sort();
                let expected = if matches!(prefix, INSERT | CAPTURED_INSERT) {
                    vec![vec![], vec![(P, CanonicalScalar::Int(1).into())]]
                } else {
                    vec![vec![
                        (P, CanonicalScalar::Int(1).into()),
                        (Q, CanonicalScalar::Int(2).into()),
                    ]]
                };
                assert_eq!(actual, expected, "{text}");
            }
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returned_identity_metadata_masks_hidden_fields_and_keeps_authority_before_effects() {
    let ((), report) = run_async_under_lab(0xca11_1011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let issuer = authority();
        let expected_map =
            GraphValue::map(vec![("p".into(), int(2)), ("q".into(), int(2))]).unwrap();
        for (expression, expected) in [
            ("n{.*}", expected_map.clone()),
            ("properties(n)", expected_map),
            ("keys(n)", text_list(&["p", "q"])),
            ("labels(n)", text_list(&["Visible"])),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                vec![VISIBLE],
                vec![
                    (P, CanonicalScalar::Int(1)),
                    (SECRET, CanonicalScalar::Int(99)),
                ],
            );
            seed.create_vertex(
                VId(9),
                vec![HIDDEN],
                vec![(SECRET, CanonicalScalar::Int(123))],
            );
            db.write(&contexts.commit(), seed).await.unwrap();
            let text = format!(
                "CALL () {{ MATCH (n:Visible) SET n.q=2 RETURN n }} SET n.p=n.p+1 RETURN {expression} AS value"
            );
            let call = PreparedGraphWriteChainText::prepare(&text, R, MetadataCatalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let token = issuer.issue_at(&grant(Rights::ReadWrite, 1), NOW).unwrap();
            let (_, rows, _) = db
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &token,
                    "main",
                    &call,
                    insert_policy(1),
                    || NOW,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(rows.value.len(), 1, "{text}");
            assert_eq!(rows.value[0].values(), &[expected], "{text}");
            let before = db.vertices().unwrap();
            assert_eq!(before.len(), 2);
            assert_eq!(
                before[0].props,
                vec![
                    (P, CanonicalScalar::Int(2).into()),
                    (Q, CanonicalScalar::Int(2).into()),
                    (SECRET, CanonicalScalar::Int(99).into())
                ]
            );
            let frontier = db.frontier().unwrap();
            let denied = issuer.issue_at(&grant(Rights::Write, 1), NOW).unwrap();
            let write_only = format!(
                "CALL () {{ CREATE (n:Visible {{p:1}}) RETURN n }} SET n.q=2 RETURN {expression} AS value"
            );
            let write_only = PreparedGraphWriteChainText::prepare(&write_only, R, MetadataCatalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &denied,
                    "main",
                    &write_only,
                    insert_policy(1),
                    || NOW,
                )
                .await
                .expect_err("public metadata needs read authority even when the body only writes");
            assert_eq!(
                authorization(&error),
                Some(Error::PermissionDenied),
                "{text}: {error:?}"
            );
            assert_eq!(db.vertices().unwrap(), before, "{text}");
            assert_eq!(db.frontier().unwrap(), frontier, "{text}");
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
