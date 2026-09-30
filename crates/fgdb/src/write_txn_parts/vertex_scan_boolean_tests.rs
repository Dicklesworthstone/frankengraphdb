// Included beside the existing scan/aggregate fixtures. Every integration case
// compiles actual query text and executes the ordinary transaction adapter.

fn boolean_plan(text: &str) -> PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow> {
    use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
    PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LABEL)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn boolean_ids(rows: &[fgdb_gql::algebra::GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| match row.values() {
            [fgdb_gql::algebra::GraphValue::Vertex(vid)] => *vid,
            other => panic!("expected one vertex column, found {other:?}"),
        })
        .collect()
}

#[test]
fn text_or_allows_nonmembers_but_detects_membership_through_either_property() {
    let ((), report) = run_async_under_lab(0x7653_0201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let query = boolean_plan("MATCH (n:L) WHERE n.p = 7 OR n.q = 9 RETURN n");
        assert!(VertexScanRead::predicates(query.plan()).is_some());
        for case in 0..3 {
            for write in [false, true] {
                let mut database = fixture(&cx).await;
                let mut transaction = database.begin(&txcx).unwrap();
                let result = transaction
                    .execute_graph_pattern_governed(&database, &qcx, &query, policy())
                    .unwrap();
                assert_eq!(boolean_ids(&result.value), vec![VId(1)]);
                assert_eq!(transaction.read_set.borrow().len(), 1);
                assert!(!transaction.scanned_vertices.get());
                assert!(transaction.scanned_vertex_labels.borrow().is_empty());
                assert!(transaction.point_reads.borrow().2[0].complete);
                if write {
                    let mut local = WriteBatch::new(RelationId(1));
                    local.create_vertex(VId(90), vec![OTHER], vec![]);
                    transaction.write(&mut database, local).unwrap();
                }
                let mut winner = WriteBatch::new(RelationId(1));
                match case {
                    0 => {
                        winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(3)));
                    }
                    1 => {
                        winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(7)));
                    }
                    _ => {
                        winner.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(9)));
                    }
                }
                winner.create_vertex(VId(5), vec![LABEL], vec![(P, CanonicalScalar::Int(8))]);
                database.write(&cx, winner).await.unwrap();
                let frontier = database.frontier().unwrap();
                let result = transaction.finish(&mut database, &cx).await;
                if case == 0 {
                    assert_eq!(result.unwrap().commit_seq().is_some(), write);
                } else {
                    assert!(is_read_conflict(result));
                    assert_eq!(database.frontier().unwrap(), frontier);
                }
                assert_eq!(
                    database.vertex(VId(90)).unwrap().is_some(),
                    write && case == 0
                );
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn text_not_keeps_missing_null_and_incompatible_values_unknown() {
    let ((), report) = run_async_under_lab(0x7653_0202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let query = boolean_plan("MATCH (n:L) WHERE NOT (n.p = 7) RETURN n");
        for value in [
            None,
            Some(CanonicalScalar::Null),
            Some(CanonicalScalar::Bool(true)),
            Some(CanonicalScalar::Int(7)),
            Some(CanonicalScalar::Int(8)),
        ] {
            let enters = value == Some(CanonicalScalar::Int(8));
            let mut database = fixture(&cx).await;
            let mut transaction = database.begin(&txcx).unwrap();
            let result = transaction
                .execute_graph_pattern_governed(&database, &qcx, &query, policy())
                .unwrap();
            assert_eq!(boolean_ids(&result.value), vec![VId(2)]);
            assert_eq!(transaction.read_set.borrow().len(), 1);
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(4), P, value);
            database.write(&cx, winner).await.unwrap();
            let result = transaction.finish(&mut database, &cx).await;
            if enters {
                assert!(is_read_conflict(result));
            } else {
                assert!(matches!(
                    result,
                    Ok(EmbeddedTxnCompletion::ReadClosed { .. })
                ));
            }
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_property_pair_entry_then_restoration_validates_every_commit() {
    let ((), report) = run_async_under_lab(0x7653_0203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut seed = WriteBatch::new(RelationId(1));
        seed.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(7)));
        seed.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(3)));
        database.write(&cx, seed).await.unwrap();
        let query = boolean_plan("MATCH (n:L) WHERE n.p = n.q OR n.q = 99 RETURN n");
        let mut transaction = database.begin(&txcx).unwrap();
        let result = transaction
            .execute_graph_pattern_governed(&database, &qcx, &query, policy())
            .unwrap();
        assert_eq!(boolean_ids(&result.value), vec![VId(1)]);
        assert!(
            !transaction
                .read_set
                .borrow()
                .contains(&ElementId::Vertex(VId(2)))
        );
        for value in [2, 3] {
            let mut winner = WriteBatch::new(RelationId(1));
            winner.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(value)));
            database.write(&cx, winner).await.unwrap();
        }
        // Both validation modes used by completion and refresh/rebase must see
        // the earlier membership entry, despite equality with the current head.
        for scope in [ConflictScope::Reads, ConflictScope::ReadsAndWrites] {
            assert!(
                transaction
                    .transaction_conflict_in(&database, scope, &mut || Ok(()))
                    .unwrap()
                    .is_some()
            );
        }
        assert!(is_read_conflict(
            transaction.finish(&mut database, &cx).await
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn text_boolean_zero_count_distinguishes_disjoint_and_same_domain_inserts() {
    let ((), report) = run_async_under_lab(0x7653_0204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for same in [false, true] {
            let mut database = fixture(&cx).await;
            let mut first = database.begin(&txcx).unwrap();
            let mut second = database.begin(&txcx).unwrap();
            let other_key = if same { 42 } else { 43 };
            for (transaction, key) in [(&first, 42), (&second, other_key)] {
                let input = boolean_plan(&format!(
                    "MATCH (n:L) WHERE n.p = {key} OR n.q = {key} RETURN n.p"
                ))
                .with_duplicates();
                let aggregate = PreparedGraphAggregate::prepare(
                    input,
                    &[],
                    &[GraphAggregate::count_rows("count")],
                    0,
                    None,
                )
                .unwrap();
                let result = transaction
                    .execute_graph_aggregate_governed(&database, &qcx, &aggregate, policy())
                    .unwrap();
                assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(0));
                assert!(transaction.read_set.borrow().is_empty());
                assert!(transaction.point_reads.borrow().2[0].complete);
            }
            let mut a = WriteBatch::new(RelationId(1));
            a.create_vertex(VId(50), vec![LABEL], vec![(Q, CanonicalScalar::Int(42))]);
            first.write(&mut database, a).unwrap();
            let mut b = WriteBatch::new(RelationId(1));
            b.create_vertex(
                VId(51),
                vec![LABEL],
                vec![(P, CanonicalScalar::Int(other_key))],
            );
            second.write(&mut database, b).unwrap();
            first.commit(&mut database, &cx).await.unwrap();
            let result = second.finish(&mut database, &cx).await;
            if same {
                assert!(is_read_conflict(result));
            } else {
                assert!(matches!(
                    result,
                    Ok(EmbeddedTxnCompletion::WriteCommitted { .. })
                ));
            }
            assert_eq!(database.vertex(VId(51)).unwrap().is_some(), !same);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn eager_boolean_data_refusal_survives_retry_and_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0x7653_0205, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut seed = WriteBatch::new(RelationId(1));
        seed.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(0)));
        database.write(&cx, seed).await.unwrap();
        let mut transaction = database.begin(&txcx).unwrap();
        transaction.savepoint(&database, "before-boolean").unwrap();
        let failing = boolean_plan("MATCH (n:L) WHERE n.p = 7 OR 1 / n.q > 0 RETURN n");
        assert!(VertexScanRead::predicates(failing.plan()).is_some());
        // OR is eager: a TRUE first leaf must not suppress division by zero.
        assert!(
            transaction
                .execute_graph_pattern_governed(&database, &qcx, &failing, policy())
                .is_err()
        );
        assert!(!transaction.point_reads.borrow().2[0].complete);
        let succeeding = boolean_plan("MATCH (n:L) WHERE n.p = 7 OR n.q = 9 RETURN n");
        transaction
            .execute_graph_pattern_governed(&database, &qcx, &succeeding, policy())
            .unwrap();
        assert_eq!(
            transaction
                .point_reads
                .borrow()
                .2
                .iter()
                .map(|scan| scan.complete)
                .collect::<Vec<_>>(),
            vec![false, true]
        );
        transaction
            .rollback_to_savepoint(&database, "before-boolean")
            .unwrap();
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(3)));
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(
            transaction.finish(&mut database, &cx).await
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_new_arithmetic_refusal_is_a_conflict_not_a_false_predicate() {
    let ((), report) = run_async_under_lab(0x7653_0206, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let query = boolean_plan("MATCH (n:L) WHERE 12 / n.p > 2 RETURN n");
        let result = transaction
            .execute_graph_pattern_governed(&database, &qcx, &query, policy())
            .unwrap();
        assert_eq!(boolean_ids(&result.value), vec![VId(2)]);
        assert!(
            !transaction
                .read_set
                .borrow()
                .contains(&ElementId::Vertex(VId(1)))
        );
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(
            transaction.finish(&mut database, &cx).await
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_witness_propagates_every_control_failure_even_after_true_or() {
    let ((), report) = run_async_under_lab(0x7653_0207, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let database = fixture(&cx).await;
        let query = boolean_plan("MATCH (n:L) WHERE TRUE OR n.q = 9 RETURN n");
        let selections = VertexScanRead::predicates(query.plan()).unwrap();
        let row = database.vertex(VId(1)).unwrap().unwrap();
        let mut count = 0usize;
        assert!(
            VertexScanRead::may_match(selections, &row, &mut |_| {
                count += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
        );
        assert!(
            count > 5,
            "exercise native Boolean admission and eager property reads"
        );
        for stop in 1..=count {
            let mut seen = 0;
            let result = VertexScanRead::may_match(selections, &row, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(seen, stop);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_output_refusal_never_completes_its_witness_on_later_success() {
    let ((), report) = run_async_under_lab(0x7653_0208, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let query = boolean_plan("MATCH (n:L) WHERE n.p = 7 OR n.q = 9 RETURN n");
        let refused = transaction.execute_graph_pattern_governed(
            &database,
            &qcx,
            &query,
            GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000),
        );
        assert!(matches!(refused, Err(GqlQueryError::Rows(_))));
        transaction
            .execute_graph_pattern_governed(&database, &qcx, &query, policy())
            .unwrap();
        assert_eq!(
            transaction
                .point_reads
                .borrow()
                .2
                .iter()
                .map(|scan| scan.complete)
                .collect::<Vec<_>>(),
            vec![false, true]
        );
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(3)));
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(
            transaction.finish(&mut database, &cx).await
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_overlay_read_keeps_full_staged_identity_after_rollback() {
    let ((), report) = run_async_under_lab(0x7653_0209, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        transaction.savepoint(&database, "before-overlay").unwrap();
        let mut local = WriteBatch::new(RelationId(1));
        local.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(9)));
        transaction.write(&mut database, local).unwrap();
        let query = boolean_plan("MATCH (n:L) WHERE n.p = 7 OR n.q = 9 RETURN n");
        let result = transaction
            .execute_graph_pattern_governed(&database, &qcx, &query, policy())
            .unwrap();
        assert_eq!(boolean_ids(&result.value), vec![VId(1), VId(2)]);
        transaction
            .rollback_to_savepoint(&database, "before-overlay")
            .unwrap();
        assert!(transaction.prepared.is_none());
        assert!(
            transaction
                .read_set
                .borrow()
                .contains(&ElementId::Vertex(VId(2)))
        );
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(3)));
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(
            transaction.finish(&mut database, &cx).await
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_queries_with_other_binding_slots_still_use_broad_witnesses() {
    let ((), report) = run_async_under_lab(0x7653_0210, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let query = boolean_plan("MATCH (n:L), (m) WHERE n.p = 7 OR m.q = 9 RETURN n");
        assert!(VertexScanRead::predicates(query.plan()).is_none());
        transaction
            .execute_graph_pattern_governed(&database, &qcx, &query, policy())
            .unwrap();
        assert!(transaction.point_reads.borrow().2.is_empty());
        assert!(transaction.scanned_vertices.get());
        let mut winner = WriteBatch::new(RelationId(1));
        winner.create_vertex(VId(9), vec![OTHER], vec![]);
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(
            transaction.finish(&mut database, &cx).await
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refused_boolean_scans_keep_label_domains_and_detect_domain_entry() {
    let ((), report) = run_async_under_lab(0x7653_0211, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let query = boolean_plan("MATCH (n:L) WHERE n.p = 7 OR n.q = 9 RETURN n");
        for source_failure in [false, true] {
            for enters in [false, true] {
                let mut database = fixture(&cx).await;
                let mut transaction = database.begin(&txcx).unwrap();
                let limits = if source_failure {
                    GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000)
                } else {
                    GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000)
                };
                assert!(matches!(
                    transaction.execute_graph_pattern_governed(&database, &qcx, &query, limits,),
                    Err(GqlQueryError::Rows(_))
                ));
                assert!(!transaction.point_reads.borrow().2[0].complete);
                let mut winner = WriteBatch::new(RelationId(1));
                winner.set_vertex_property(VId(3), Q, Some(CanonicalScalar::Int(3)));
                winner.create_vertex(VId(9), vec![OTHER], vec![]);
                if enters {
                    winner.set_vertex_label(VId(3), LABEL, true);
                }
                database.write(&cx, winner).await.unwrap();
                let result = transaction.finish(&mut database, &cx).await;
                if enters {
                    assert!(is_read_conflict(result));
                } else {
                    assert!(matches!(
                        result,
                        Ok(EmbeddedTxnCompletion::ReadClosed { .. })
                    ));
                }
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_later_label_never_hides_an_earlier_boolean_data_error() {
    let ((), report) = run_async_under_lab(0x7653_0212, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let database = fixture(&cx).await;
        let query = boolean_plan("MATCH (n:L) WHERE 1 / n.p > 0 RETURN n");
        let mut row = database.vertex(VId(3)).unwrap().unwrap();
        row.props = vec![(P, CanonicalScalar::Int(0))];
        let mut scan = VertexScanRead {
            selections: VertexScanRead::predicates(query.plan()).unwrap().to_vec(),
            complete: false,
        };
        assert_eq!(scan.selections.len(), 2);
        assert!(!scan.matches_labels(Some(&row)));
        assert!(!scan.matches(Some(&row), &mut || Ok(())).unwrap());
        // Exercise both valid operator orders: only a preceding label may
        // exclude a row before native Boolean evaluation raises an exception.
        scan.selections.swap(0, 1);
        assert!(scan.matches_labels(Some(&row)));
        assert!(scan.matches(Some(&row), &mut || Ok(())).unwrap());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
