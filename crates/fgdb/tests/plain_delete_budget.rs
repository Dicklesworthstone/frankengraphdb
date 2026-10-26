//! Plain DELETE incidence validation is part of the statement's query budget.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GlaLimitDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeletePolicy,
    GraphMutationPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphDeleteText,
    PreparedGraphMutationQueryText, PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}

#[test]
fn chain_delete_proof_and_return_share_limits_and_preserve_an_existing_prefix() {
    let ((), report) = run_async_under_lab(0xd31e_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        for continue_after_delete in [false, true] {
            let text = if continue_after_delete {
                "MATCH (a {p:1})-[r:R]->(b {p:2}) CREATE (audit {p:99}) \
                 WITH a,r,audit DELETE a,r WITH audit,size(range(1,32)) AS size \
                 CREATE (out {p:size}) RETURN out.p LIMIT 0"
            } else {
                "MATCH (a {p:1})-[r:R]->(b {p:2}) CREATE (audit {p:99}) \
                 WITH a,r,audit DELETE a,r \
                 RETURN size(range(1,32)) AS size,audit.p AS p LIMIT 0"
            };
            let chain = PreparedGraphWriteChainText::prepare(text, R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let mut measured: Option<[u64; 3]> = None;
            let mut expected_graph = None;
            for attempt in 0..5 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                for id in 1..=3_u128 {
                    seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
                }
                // Two MATCH occurrences create two audit rows. The isolated
                // deletion proof must read both explicit parallel edges once;
                // the unrelated edge and its property must survive unchanged.
                for id in [10, 11] {
                    seed.add_edge(EId(id), VId(1), VId(2), vec![]);
                }
                seed.add_edge(EId(12), VId(2), VId(3), vec![(P, CanonicalScalar::Int(42))]);
                db.write(&commit, seed).await.unwrap();
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let mut txn = db.begin(&txcx).unwrap();
                let mut prefix = WriteBatch::new(R);
                prefix.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-3)));
                txn.write(&mut db, prefix).unwrap();
                let prefix_digest = txn.staged_effect_digest().unwrap();
                let prefix_graph = (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap());
                let created = if continue_after_delete { 4 } else { 2 };
                let mut budget = GraphInsertPolicy::new(
                    GqlQueryPolicy::new(10_000, 0, 2_000_000, 2_000_000),
                    created,
                    0,
                )
                .with_mutation_limit(3);
                if let Some([records, work, scratch]) = measured {
                    budget.query = GqlQueryPolicy::new(
                        records - u64::from(attempt == 4),
                        0,
                        work - u64::from(attempt == 2),
                        scratch - u64::from(attempt == 3),
                    );
                }
                let mut next = 1_000_u128;
                let result = txn.execute_graph_write_chain_governed(
                    &mut db,
                    &query,
                    &chain,
                    budget,
                    |request| {
                        let id = next;
                        next += 1;
                        Ok::<_, ()>(match request {
                            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(id)),
                            GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(id)),
                        })
                    },
                );
                if attempt < 2 {
                    let (stats, rows, receipt) = result.unwrap();
                    let usage = [
                        rows.rows.snapshot_records,
                        rows.evaluator.work_units,
                        rows.evaluator.scratch_entries,
                    ];
                    assert_eq!(stats.selection, rows.rows);
                    assert_eq!(stats.evaluator, rows.evaluator);
                    assert_eq!(stats.created_vertices, created);
                    assert_eq!(receipt.stats().mutation_effects, 3);
                    assert!(rows.value.is_empty(), "LIMIT 0 cannot suppress effects");
                    assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
                    let graph = (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap());
                    assert_eq!(graph.0.len(), 2 + created as usize);
                    assert_eq!(graph.1, vec![before.2[2].clone()]);
                    if let Some(expected) = measured {
                        assert_eq!(usage, expected, "exact cumulative limits must succeed");
                        assert_eq!(Some(&graph), expected_graph.as_ref());
                        txn.finish(&mut db, &commit).await.unwrap();
                        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                        assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), graph);
                    } else {
                        assert!(usage.into_iter().all(|value| value > 0));
                        measured = Some(usage);
                        expected_graph = Some(graph);
                        txn.abort();
                    }
                } else {
                    let error = result.unwrap_err();
                    match attempt {
                        2 => assert!(
                            matches!(error, GqlQueryError::Evaluator(limit)
                                if limit.dimension == GlaLimitDimension::WorkUnits),
                            "{error:?}"
                        ),
                        3 => assert!(
                            matches!(error, GqlQueryError::Evaluator(limit)
                                if limit.dimension == GlaLimitDimension::ScratchEntries),
                            "{error:?}"
                        ),
                        4 => assert!(matches!(error, GqlQueryError::Rows(_)), "{error:?}"),
                        _ => unreachable!(),
                    }
                    assert_eq!(txn.staged_effect_digest().unwrap(), prefix_digest);
                    assert_eq!(
                        (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap()),
                        prefix_graph
                    );
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before,
                        "every chain effect stays private after a combined-limit refusal"
                    );
                    txn.abort();
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
fn policy() -> GraphDeletePolicy {
    GraphDeletePolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000),
        100,
    )
}

#[test]
fn exact_limits_include_edge_scan_and_each_proposal_and_refusal_preserves_prefix() {
    let ((), report) = run_async_under_lab(0xd31e_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=6_u128 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        for id in 10..14_u128 {
            seed.add_edge(EId(id), VId(1), VId(2), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let definition =
            PreparedGraphDeleteText::prepare("MATCH (n) WHERE n.p>=3 DELETE n", R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(-1)));
        txn.write(&mut db, prefix.clone()).unwrap();
        let selection = txn
            .execute_graph_pattern_governed(&db, &query, definition.selection(), policy().query)
            .unwrap();
        let proposal = definition
            .execute_governed(
                policy(),
                |_, _| Ok::<_, GqlQueryError<(), ()>>(selection),
                || Ok(()),
            )
            .unwrap()
            .stats();
        let (stats, targets) = txn
            .execute_graph_delete_returning_governed(&mut db, &query, &definition, policy())
            .unwrap();
        assert_eq!(targets, vec![VId(3), VId(4), VId(5), VId(6)]);
        // Since 6c0b23e5 the incidence proof walks each target's adjacency
        // index instead of scanning every edge. None of the four targets has an
        // incident edge, so no edge row is read and no snapshot record is
        // charged beyond the selection.
        assert_eq!(
            stats.selection.snapshot_records,
            proposal.selection.snapshot_records
        );
        assert_eq!(stats.selection.result_rows, proposal.selection.result_rows);
        // Scratch: one witness per target, then one staged deletion per target.
        assert_eq!(
            stats.evaluator.scratch_entries,
            proposal.evaluator.scratch_entries + 4 + 4
        );
        // Work: entry, the staged prefix's coordinate and effect, one unit per
        // target plus that target's index probe, the proof's close, and the
        // staging. Probe cost is the index's tree traversal (at least one node
        // per face), so the exact total is pinned by the cap loop below.
        let entry_prefix_targets_close_staging = 1 + 2 + 4 * (1 + 2) + 1 + 1;
        let floor = proposal.evaluator.work_units + entry_prefix_targets_close_staging;
        assert!(
            stats.evaluator.work_units >= floor,
            "{stats:?} vs {proposal:?}"
        );
        txn.abort();
        let caps = [
            stats.selection.snapshot_records,
            stats.selection.result_rows,
            stats.evaluator.work_units,
            stats.evaluator.scratch_entries,
            stats.target_vertices,
        ];
        for dimension in 0..5 {
            let mut lower = caps;
            lower[dimension] -= 1;
            let limit = GraphDeletePolicy::new(
                GqlQueryPolicy::new(lower[0], lower[1], lower[2], lower[3]),
                lower[4],
            );
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, prefix.clone()).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            assert!(
                txn.execute_graph_delete_returning_governed(&mut db, &query, &definition, limit)
                    .is_err(),
                "dimension {dimension}"
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
            assert!(txn.vertex(&db, VId(3)).unwrap().is_some());
            txn.abort();
        }
        let exact = GraphDeletePolicy::new(
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            caps[4],
        );
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, prefix).unwrap();
        assert_eq!(
            txn.execute_graph_delete_governed(&mut db, &query, &definition, exact)
                .unwrap(),
            stats
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(
            db.edges().unwrap().len(),
            4,
            "unrelated relationships must survive"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_selection_neither_scans_incidence_nor_spends_a_target_allowance() {
    let ((), report) = run_async_under_lab(0xd31e_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        seed.add_edge(EId(10), VId(1), VId(1), vec![]);
        db.write(&commit, seed).await.unwrap();
        let definition =
            PreparedGraphDeleteText::prepare("MATCH (n) WHERE n.p=99 DELETE n", R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let selected = txn
            .execute_graph_pattern_governed(&db, &query, definition.selection(), policy().query)
            .unwrap();
        let expected = definition
            .execute_governed(
                policy(),
                |_, _| Ok::<_, GqlQueryError<(), ()>>(selected),
                || Ok(()),
            )
            .unwrap()
            .stats();
        let exact = GraphDeletePolicy::new(
            GqlQueryPolicy::new(
                expected.selection.snapshot_records,
                0,
                expected.evaluator.work_units,
                expected.evaluator.scratch_entries,
            ),
            0,
        );
        assert_eq!(
            txn.execute_graph_delete_governed(&mut db, &query, &definition, exact)
                .unwrap(),
            expected
        );
        let before = db.frontier().unwrap();
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            fgdb_types::EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn standalone_delete_return_reserves_its_projection_before_incidence_and_staging() {
    let ((), report) = run_async_under_lab(0xd31e_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        for page in ["", " LIMIT 0"] {
            let statement = PreparedGraphMutationQueryText::prepare(
                &format!(
                    "MATCH (a {{p:1}})-[r:R]->(b {{p:2}}) DELETE a,r \
                     RETURN size(range(1,32)) AS size,b.p AS p{page}"
                ),
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let rows = if page.is_empty() { 2 } else { 0 };
            let mut measured: Option<[u64; 3]> = None;
            let mut expected = None;
            for attempt in 0..5 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(R);
                for id in 1..=3_u128 {
                    seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
                }
                for id in [10, 11] {
                    seed.add_edge(EId(id), VId(1), VId(2), vec![]);
                }
                seed.add_edge(EId(12), VId(2), VId(3), vec![(P, CanonicalScalar::Int(42))]);
                db.write(&commit, seed).await.unwrap();
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let mut txn = db.begin(&txcx).unwrap();
                let mut prefix = WriteBatch::new(R);
                prefix.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-3)));
                txn.write(&mut db, prefix).unwrap();
                let prefix_digest = txn.staged_effect_digest().unwrap();
                let prefix_graph = (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap());
                let mut budget = GraphMutationPolicy::new(
                    GqlQueryPolicy::new(10_000, rows, 2_000_000, 2_000_000),
                    3,
                );
                if let Some([records, work, scratch]) = measured {
                    budget.query = GqlQueryPolicy::new(
                        records - u64::from(attempt == 4),
                        rows,
                        work - u64::from(attempt == 2),
                        scratch - u64::from(attempt == 3),
                    );
                }
                let result =
                    txn.execute_graph_mutation_query_governed(&mut db, &query, &statement, budget);
                if attempt < 2 {
                    let (stats, result) = result.unwrap();
                    assert_eq!(stats.effects, 3);
                    assert_eq!(result.value.len() as u64, rows);
                    for row in &result.value {
                        assert_eq!(
                            row.values(),
                            &[
                                fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(32)),
                                fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(2)),
                            ]
                        );
                    }
                    assert!(result.evaluator.work_units > stats.evaluator.work_units);
                    let usage = [
                        result.rows.snapshot_records,
                        result.evaluator.work_units,
                        result.evaluator.scratch_entries,
                    ];
                    assert!(txn.vertex(&db, VId(1)).unwrap().is_none());
                    let graph = (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap());
                    assert_eq!(graph.0.len(), 2);
                    assert_eq!(graph.1, vec![before.2[2].clone()]);
                    if let Some(prior) = measured {
                        assert_eq!(usage, prior);
                        assert_eq!(Some(&(stats, result, graph.clone())), expected.as_ref());
                        txn.finish(&mut db, &commit).await.unwrap();
                        assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                        assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), graph);
                    } else {
                        measured = Some(usage);
                        expected = Some((stats, result, graph));
                        txn.abort();
                    }
                } else {
                    let error = result.unwrap_err();
                    match attempt {
                        2 => assert!(
                            matches!(error, GqlQueryError::Evaluator(limit)
                            if limit.dimension == GlaLimitDimension::WorkUnits),
                            "{error:?}"
                        ),
                        3 => assert!(
                            matches!(error, GqlQueryError::Evaluator(limit)
                            if limit.dimension == GlaLimitDimension::ScratchEntries),
                            "{error:?}"
                        ),
                        4 => assert!(matches!(error, GqlQueryError::Rows(_)), "{error:?}"),
                        _ => unreachable!(),
                    }
                    assert_eq!(txn.staged_effect_digest().unwrap(), prefix_digest);
                    assert_eq!(
                        (txn.vertices(&db).unwrap(), txn.edges(&db).unwrap()),
                        prefix_graph
                    );
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                    txn.abort();
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
