//! Exact owner counts retain native history, resource and output laws.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EmbeddedReadView, GqlError, MemVfs, PreparedNativeRead, QueryError,
    QueryResult, QueryValue, ReadError, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "M") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Label, "Missing") => Some(GraphSymbol::Label(LabelId(3))),
        _ => None,
    }
}

fn aggregate(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn count_result(total: u64) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["total".to_owned()],
        rows: vec![vec![QueryValue::Count(total)]],
    }
}

async fn history(db: &mut Database<MemVfs>, cx: &CommitCx) -> EmbeddedReadView {
    let payload = CanonicalScalar::ucs_basic_text(&"property payload ".repeat(256)).unwrap();
    let mut first = WriteBatch::new(R);
    for id in [1, 2, 3, u128::MAX - 1] {
        first.create_vertex(VId(id), vec![], vec![(P, payload.clone())]);
    }
    for (eid, source, destination) in [(10, 1, 2), (11, 1, 2), (12, 2, 2), (13, 3, 1)] {
        first.add_edge(
            EId(u128::MAX - eid),
            VId(source),
            VId(destination),
            vec![(P, payload.clone())],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let first_view = db.read_session().unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(20), VId(1), VId(2), vec![(P, payload)]);
    assert_eq!(db.write(cx, second).await.unwrap(), CommitSeq(2));
    let mut third = WriteBatch::new(R);
    third.delete_edge(EId(u128::MAX - 10));
    third.set_edge_property(EId(u128::MAX - 11), P, Some(CanonicalScalar::Bool(true)));
    third.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
    third.delete_vertex(VId(u128::MAX - 1));
    third.create_vertex(VId(1 << 127), vec![], vec![]);
    assert_eq!(db.write(cx, third).await.unwrap(), CommitSeq(3));
    first_view
}

#[test]
fn owner_and_pinned_native_counts_keep_history_and_live_multiplicity() {
    let ((), report) = run_async_under_lab(0xc017_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let first_view = history(&mut db, &contexts.commit()).await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();

        // Each entry is an independently counted live fixture: isolated
        // vertices count once; parallel edges and self-loops remain occurrences.
        for (cut, vertices, r_edges, s_edges) in
            [(0, 0, 0, 0), (1, 4, 4, 0), (2, 4, 4, 1), (3, 4, 3, 1)]
        {
            for (pattern, expected, records) in [
                ("(n)", vertices, vertices),
                ("(a)-[:R]->(b)", r_edges, r_edges + s_edges),
                ("(a)<-[:R]-(b)", r_edges, r_edges + s_edges),
                ("(a)-[r:S]->(b)", s_edges, r_edges + s_edges),
                ("(a)-[:T]->(b)", 0, r_edges + s_edges),
            ] {
                let text = format!(
                    "MATCH {pattern} FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS total"
                );
                assert_eq!(
                    db.query(&contexts.query(), &text, &params, symbols, policy())
                        .unwrap(),
                    count_result(expected),
                    "{text}"
                );
                let (result, selected) = view
                    .query_with_snapshot(&contexts.query(), &text, &params, symbols, policy())
                    .unwrap();
                assert_eq!(selected, CommitSeq(cut));
                assert_eq!(result, count_result(expected));
                let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
                assert_eq!(
                    prepared
                        .execute_in_view_with_snapshot(&view, &contexts.query(), &params, policy())
                        .unwrap(),
                    (count_result(expected), CommitSeq(cut))
                );

                let query = aggregate(&format!("MATCH {pattern} RETURN COUNT(*) AS total"));
                assert!(query.count_summary_source().is_some());
                let owned = db
                    .execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(cut),
                        policy(),
                    )
                    .unwrap();
                let pinned = view
                    .execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(cut),
                        policy(),
                    )
                    .unwrap();
                assert_eq!(owned, pinned);
                // Exact current-cut metadata admits one cardinality record;
                // historical cuts retain their full live-source admission.
                assert_eq!(
                    owned.rows.snapshot_records,
                    if cut == 3 { 1 } else { records }
                );
                assert_eq!(owned.value[0].get(0).unwrap().as_count(), Some(expected));
            }
        }

        db.compact(&contexts.commit()).await.unwrap();
        for cut in 0..=3 {
            let text = format!(
                "MATCH (a)-[:R]->(b) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS total"
            );
            assert_eq!(
                db.query(&contexts.query(), &text, &params, symbols, policy())
                    .unwrap(),
                count_result([0, 4, 4, 3][cut as usize])
            );
        }
        drop(db);
        let text = "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total";
        assert_eq!(
            first_view
                .query(&contexts.query(), text, &params, symbols, policy())
                .unwrap(),
            count_result(4)
        );
        assert_eq!(
            view.query(&contexts.query(), text, &params, symbols, policy())
                .unwrap(),
            count_result(3)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_count_limits_and_output_expressions_keep_the_native_finalizer() {
    let ((), report) = run_async_under_lab(0xc017_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let _ = history(&mut db, &contexts.commit()).await;
        let view = db.read_session().unwrap();
        for text in [
            "MATCH (n) RETURN COUNT(*) AS total",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total,COUNT(*) AS again",
            "MATCH (a)-[:T]->(b) RETURN COUNT(*) AS total",
            "MATCH (n) RETURN COUNT(*) + 1 AS total ORDER BY total DESC LIMIT 1",
            "MATCH (n) RETURN COUNT(*) AS total HAVING total < 0",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total SKIP 1 LIMIT 0",
        ] {
            let query = aggregate(text);
            assert!(query.count_summary_source().is_some(), "{text}");
            let full = db
                .execute_graph_aggregate_governed(&contexts.query(), &query, policy())
                .unwrap();
            let exact = GqlQueryPolicy::new(
                full.rows.snapshot_records,
                full.rows.result_rows,
                full.evaluator.work_units,
                full.evaluator.scratch_entries,
            );
            assert_eq!(
                db.execute_graph_aggregate_governed(&contexts.query(), &query, exact)
                    .unwrap(),
                full
            );
            assert_eq!(
                view.execute_graph_aggregate_governed(&contexts.query(), &query, exact)
                    .unwrap(),
                full
            );
            for cap in [
                GqlQueryPolicy::new(u64::MAX, u64::MAX, full.evaluator.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                    full.evaluator.scratch_entries - 1,
                ),
            ] {
                assert!(matches!(
                    db.execute_graph_aggregate_governed(&contexts.query(), &query, cap),
                    Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1
                ));
                assert!(matches!(
                    view.execute_graph_aggregate_governed(&contexts.query(), &query, cap),
                    Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1
                ));
            }
            let records = full.rows.snapshot_records;
            assert!(matches!(
                db.execute_graph_aggregate_governed(&contexts.query(), &query, GqlQueryPolicy::new(records - 1, u64::MAX, u64::MAX, u64::MAX)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.observed == records
            ));
            if full.rows.result_rows > 0 {
                assert!(matches!(
                    db.execute_graph_aggregate_governed(&contexts.query(), &query, GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX)),
                    Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 1
                ));
            }
        }

        let bad = "MATCH (n) RETURN 1 / (COUNT(*) - COUNT(*)) AS total LIMIT 0";
        let query = aggregate(bad);
        assert!(query.count_summary_source().is_some());
        assert!(matches!(
            db.execute_graph_aggregate_governed(&contexts.query(), &query, policy()),
            Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            ))
        ));
        assert!(matches!(
            db.query(
                &contexts.query(),
                bad,
                &GqlParameters::new(),
                symbols,
                policy()
            ),
            Err(QueryError::Aggregate(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            )))
        ));
        let filtered = "MATCH (n) WHERE n.p = 9 RETURN COUNT(*) AS total";
        assert!(aggregate(filtered).count_summary_source().is_none());
        assert_eq!(
            db.query(
                &contexts.query(),
                filtered,
                &GqlParameters::new(),
                symbols,
                policy()
            )
            .unwrap(),
            count_result(1)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_count_history_fences_precede_cancellation_and_empty_page_limits() {
    let ((), report) = run_async_under_lab(0xc017_0013, |root| async move {
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let mut db = Database::open_memory(&contexts.commit(), keys())
                    .await
                    .unwrap();
                let _ = history(&mut db, &contexts.commit()).await;
                let view = db.read_session().unwrap();
                let query = aggregate("MATCH (n) RETURN COUNT(*) AS total LIMIT 0");
                let refused = GqlQueryPolicy::new(0, 0, 0, 0);
                root.cancel_with(CancelKind::User, Some("owner count cancellation law"));
                for result in [
                    db.execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(4),
                        refused,
                    ),
                    view.execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(4),
                        refused,
                    ),
                ] {
                    assert!(matches!(
                        result,
                        Err(GqlQueryError::Source(GraphAggregateError::Source(
                            GqlError::Read(ReadError::BeyondFrontier { .. })
                        )))
                    ));
                }
                for result in [
                    db.execute_graph_aggregate_governed(&contexts.query(), &query, refused),
                    view.execute_graph_aggregate_governed(&contexts.query(), &query, refused),
                ] {
                    assert!(matches!(result, Err(GqlQueryError::Interrupted(_))));
                }
            })
            .unwrap();
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_current_counts_do_not_grow_with_graph_and_reopen_rebuilds_before_reuse() {
    let ((), report) = run_async_under_lab(0xc017_0022, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &dir, keys())
            .await
            .unwrap();
        let vertices = aggregate("MATCH (n) RETURN COUNT(*) AS total");
        let edges = aggregate("MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total");
        let mut baseline = None;
        let mut pinned = None;
        for total in [1_u128, 32, 512] {
            let mut batch = WriteBatch::new(R);
            let begin = if total == 1 {
                1
            } else if total == 32 {
                2
            } else {
                33
            };
            for id in begin..=total {
                batch.create_vertex(VId(id), vec![], vec![]);
                batch.add_edge(EId(id), VId(id), VId(id), vec![]);
            }
            db.write(&contexts.commit(), batch).await.unwrap();
            for query in [&vertices, &edges] {
                let result = db
                    .execute_graph_aggregate_governed(&contexts.query(), query, policy())
                    .unwrap();
                assert_eq!(result.rows.snapshot_records, 1);
                assert_eq!(result.value[0].values()[0].as_count(), Some(total as u64));
            }
            let result = db
                .execute_graph_aggregate_governed(&contexts.query(), &edges, policy())
                .unwrap();
            let observed = (
                result.evaluator.work_units,
                result.evaluator.scratch_entries,
            );
            assert_eq!(
                *baseline.get_or_insert(observed),
                observed,
                "counts touch only relation metadata"
            );
            if total == 32 {
                pinned = Some(db.read_session().unwrap());
            }
        }
        let pinned = pinned.unwrap();
        let mut retired = WriteBatch::new(R);
        retired.delete_vertex(VId(1)); // self-loop cascade counts one retirement
        retired.delete_edge(EId(2));
        db.write(&contexts.commit(), retired).await.unwrap();
        assert_eq!(
            db.execute_graph_aggregate_governed(&contexts.query(), &vertices, policy())
                .unwrap()
                .value[0]
                .values()[0]
                .as_count(),
            Some(511)
        );
        let maintained = db
            .execute_graph_aggregate_governed(&contexts.query(), &edges, policy())
            .unwrap();
        assert_eq!(maintained.value[0].values()[0].as_count(), Some(510));
        assert_eq!(
            pinned
                .execute_graph_aggregate_governed(&contexts.query(), &edges, policy())
                .unwrap()
                .value[0]
                .values()[0]
                .as_count(),
            Some(32)
        );
        db.compact(&contexts.commit()).await.unwrap();
        assert_eq!(
            db.execute_graph_aggregate_governed(&contexts.query(), &edges, policy())
                .unwrap(),
            maintained
        );
        drop(db);
        let mut reopened = Database::open_with_vfs(&contexts.commit(), vfs, &dir, keys())
            .await
            .unwrap();
        let cold = reopened
            .execute_graph_aggregate_governed(&contexts.query(), &edges, policy())
            .unwrap();
        assert_eq!(cold.value, maintained.value);
        assert_eq!(
            cold.rows.snapshot_records, 510,
            "reopen discards process-local metadata"
        );
        let warm = reopened
            .execute_graph_aggregate_governed(&contexts.query(), &edges, policy())
            .unwrap();
        assert_eq!(warm, maintained);
        let mut next = WriteBatch::new(R);
        next.add_edge(EId(513), VId(2), VId(2), vec![]);
        reopened.write(&contexts.commit(), next).await.unwrap();
        let next = reopened
            .execute_graph_aggregate_governed(&contexts.query(), &edges, policy())
            .unwrap();
        assert_eq!(next.value[0].values()[0].as_count(), Some(511));
        assert_eq!(next.rows.snapshot_records, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn identity_counts_use_exact_metadata_and_preserve_historical_multiplicity() {
    let ((), report) = run_async_under_lab(0xc017_0024, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let first = history(&mut db, &contexts.commit()).await;
        let params = GqlParameters::new();
        let small = GqlQueryPolicy::new(1, 100, 1_000_000, 100_000);
        for (text, current, old) in [
            ("MATCH (n) RETURN COUNT(n) AS total", 4, 4),
            ("MATCH (n) RETURN COUNT(DISTINCT n) AS total", 4, 4),
            ("MATCH (a)-[r:R]->(b) RETURN COUNT(r) AS total", 3, 4),
            (
                "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS total",
                3,
                4,
            ),
            ("MATCH (a)<-[r:R]-(b) RETURN COUNT(r) AS total", 3, 4),
            (
                "MATCH (a)<-[r:R]-(b) RETURN COUNT(DISTINCT r) AS total",
                3,
                4,
            ),
            ("MATCH (a)-[r]->(b) RETURN COUNT(r) AS total", 4, 4),
            ("MATCH (a)-[r]->(b) RETURN COUNT(DISTINCT r) AS total", 4, 4),
            ("MATCH (a)-[:R]->(b) RETURN COUNT(b) AS total", 3, 4),
            ("MATCH (a)-[r:T]->(b) RETURN COUNT(r) AS total", 0, 0),
            (
                "MATCH (a)-[r:T]->(b) RETURN COUNT(DISTINCT r) AS total",
                0,
                0,
            ),
        ] {
            let query = aggregate(text);
            assert!(query.count_metadata_source().is_some(), "{text}");
            let result = db
                .execute_graph_aggregate_governed(&contexts.query(), &query, small)
                .unwrap();
            assert_eq!(result.rows.snapshot_records, 1, "{text}");
            assert_eq!(result.value[0].values()[0].as_count(), Some(current));
            assert_eq!(
                db.query(&contexts.query(), text, &params, symbols, small)
                    .unwrap(),
                count_result(current)
            );
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let view = db.read_session().unwrap();
            assert_eq!(
                prepared
                    .execute_in_view_with_snapshot(&view, &contexts.query(), &params, small)
                    .unwrap(),
                (count_result(current), CommitSeq(3))
            );
            // This old generation remains current for its own immutable pin.
            assert_eq!(
                first
                    .execute_graph_aggregate_governed(&contexts.query(), &query, small)
                    .unwrap()
                    .value[0]
                    .values()[0]
                    .as_count(),
                Some(old)
            );
            let historical = db
                .execute_graph_aggregate_governed_at(
                    &contexts.query(),
                    &query,
                    CommitSeq(1),
                    policy(),
                )
                .unwrap();
            assert_eq!(historical.value[0].values()[0].as_count(), Some(old));
            assert!(historical.rows.snapshot_records > 1);
        }

        // A missing property, repeated endpoint, and null-extended edge have
        // different counts from their source population; all keep real input.
        for (text, expected) in [
            ("MATCH (n) RETURN COUNT(n.p) AS total", 3),
            ("MATCH (a)-[:R]->(b) RETURN COUNT(DISTINCT b) AS total", 2),
            (
                "MATCH (a) OPTIONAL MATCH (a)-[r:T]->(b) RETURN COUNT(r) AS total",
                0,
            ),
        ] {
            assert!(aggregate(text).count_metadata_source().is_none());
            assert_eq!(
                db.query(&contexts.query(), text, &params, symbols, policy())
                    .unwrap(),
                count_result(expected)
            );
        }
        let fallible = "MATCH (a)-[r:R]->(b) RETURN 1 / (COUNT(r) - COUNT(*)) AS total LIMIT 0";
        assert!(
            db.query(&contexts.query(), fallible, &params, symbols, small)
                .is_err(),
            "empty delivery cannot suppress a finalizer error"
        );
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_label_counts_track_membership_without_graph_sized_query_admission() {
    let ((), report) = run_async_under_lab(0xc017_0031, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &dir, keys())
            .await
            .unwrap();
        let labeled = aggregate("MATCH (n:L) RETURN COUNT(*) AS total");
        let other = aggregate("MATCH (n:M) RETURN COUNT(*) AS total");
        let absent = aggregate("MATCH (n:Missing) RETURN COUNT(*) AS total");
        let small = GqlQueryPolicy::new(1, 10, 1000, 1000);
        let mut baseline = None;
        let mut pinned = None;
        let mut begin = 1;
        for total in [1_u128, 32, 512] {
            let mut batch = WriteBatch::new(R);
            for id in begin..=total {
                let mut labels = vec![LabelId(1)];
                if id % 3 == 0 {
                    labels.push(LabelId(2));
                }
                batch.create_vertex(VId(id), labels, vec![(P, CanonicalScalar::Int(id as i64))]);
            }
            db.write(&contexts.commit(), batch).await.unwrap();
            for (query, expected) in [
                (&labeled, total as u64),
                (&other, total as u64 / 3),
                (&absent, 0),
            ] {
                let result = db
                    .execute_graph_aggregate_governed(&contexts.query(), query, small)
                    .unwrap();
                assert_eq!(result.rows.snapshot_records, 1);
                assert_eq!(result.value[0].values()[0].as_count(), Some(expected));
            }
            // After both labels exist, lookup and finalization depend only on
            // the fixed label population, not the growing vertex population.
            if total >= 32 {
                let result = db
                    .execute_graph_aggregate_governed(&contexts.query(), &labeled, small)
                    .unwrap();
                let observed = (
                    result.evaluator.work_units,
                    result.evaluator.scratch_entries,
                );
                assert_eq!(*baseline.get_or_insert(observed), observed);
            }
            if total == 32 {
                pinned = Some(db.read_session().unwrap());
            }
            begin = total + 1;
        }
        let pinned = pinned.unwrap();
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_label(VId(1), LabelId(1), false);
        changes.set_vertex_label(VId(1), LabelId(1), false);
        changes.set_vertex_label(VId(2), LabelId(2), true);
        changes.set_vertex_label(VId(2), LabelId(2), true);
        changes.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(-1)));
        changes.delete_vertex(VId(3));
        changes.create_vertex(VId(513), vec![LabelId(1), LabelId(2)], vec![]);
        changes.delete_vertex(VId(513));
        db.write(&contexts.commit(), changes).await.unwrap();
        for (query, expected) in [(&labeled, 510), (&other, 170), (&absent, 0)] {
            let result = db
                .execute_graph_aggregate_governed(&contexts.query(), query, small)
                .unwrap();
            assert_eq!(result.rows.snapshot_records, 1);
            assert_eq!(result.value[0].values()[0].as_count(), Some(expected));
        }
        assert_eq!(
            pinned
                .execute_graph_aggregate_governed(&contexts.query(), &labeled, small)
                .unwrap()
                .value[0]
                .values()[0]
                .as_count(),
            Some(32)
        );
        assert_eq!(
            db.execute_graph_aggregate_governed_at(
                &contexts.query(),
                &labeled,
                CommitSeq(1),
                policy()
            )
            .unwrap()
            .value[0]
                .values()[0]
                .as_count(),
            Some(1)
        );
        let intersection = aggregate("MATCH (n:L:M) RETURN COUNT(*) AS total");
        assert!(intersection.count_metadata_source().is_none());
        assert_eq!(
            db.execute_graph_aggregate_governed(&contexts.query(), &intersection, policy())
                .unwrap()
                .value[0]
                .values()[0]
                .as_count(),
            Some(170)
        );
        db.compact(&contexts.commit()).await.unwrap();
        assert_eq!(
            db.execute_graph_aggregate_governed(&contexts.query(), &labeled, small)
                .unwrap()
                .value[0]
                .values()[0]
                .as_count(),
            Some(510)
        );
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &dir, keys())
            .await
            .unwrap();
        assert!(
            matches!(reopened.execute_graph_aggregate_governed(&contexts.query(), &labeled, small), Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords)
        );
        let cold = reopened
            .execute_graph_aggregate_governed(&contexts.query(), &labeled, policy())
            .unwrap();
        assert_eq!(cold.rows.snapshot_records, 511);
        assert_eq!(cold.value[0].values()[0].as_count(), Some(510));
        for (query, expected) in [(&labeled, 510), (&other, 170), (&absent, 0)] {
            let warm = reopened
                .execute_graph_aggregate_governed(&contexts.query(), query, small)
                .unwrap();
            assert_eq!(warm.rows.snapshot_records, 1);
            assert_eq!(warm.value[0].values()[0].as_count(), Some(expected));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
