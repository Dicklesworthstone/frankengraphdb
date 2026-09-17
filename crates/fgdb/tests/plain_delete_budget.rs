//! Plain DELETE incidence validation is part of the statement's query budget.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphDeletePolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphDeleteText,
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
        _ => None,
    }
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
            .execute_graph_pattern_governed(&mut db, &query, definition.selection(), policy().query)
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
        assert_eq!(
            stats.selection.snapshot_records,
            proposal.selection.snapshot_records + 4
        );
        assert_eq!(stats.selection.result_rows, proposal.selection.result_rows);
        assert_eq!(
            stats.evaluator.work_units,
            proposal.evaluator.work_units + 4 + 4 + 1
        );
        assert_eq!(
            stats.evaluator.scratch_entries,
            proposal.evaluator.scratch_entries + 4
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
            .execute_graph_pattern_governed(&mut db, &query, definition.selection(), policy().query)
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
