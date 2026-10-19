//! Real snapshot-backed pull execution: no artifact or complete result is built.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphPatternBuilder, IntegerComparison, PreparedGraphPattern, ScalarPredicate,
    VertexPredicate,
};
use fgdb_gql::stream::{VertexScanError, VertexScanState};
use fgdb_gql::{
    GqlBudgetDimension, GqlExecutionStats, GqlQueryError, GqlQueryPolicy, PreparedGqlQuery,
    RelationBind,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    PurposeContexts, VId,
};

const LABEL: LabelId = LabelId(3);
const KEY: PropertyKeyId = PropertyKeyId(7);
const R: RelationId = RelationId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn pattern(offset: u64, count: Option<u64>, filter: bool) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    if filter {
        builder
            .filter("n", VertexPredicate::HasLabel(LABEL))
            .unwrap();
        builder
            .filter(
                "n",
                VertexPredicate::IntegerProperty {
                    key: KEY,
                    comparison: IntegerComparison::GreaterOrEqual,
                    value: 0,
                },
            )
            .unwrap();
    }
    builder.prepare("n", offset, count).unwrap()
}

fn equality(value: CanonicalScalar, offset: u64, count: Option<u64>) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .filter(
            "n",
            VertexPredicate::ScalarProperty {
                key: KEY,
                predicate: ScalarPredicate::new(value, IntegerComparison::Equal).unwrap(),
            },
        )
        .unwrap();
    builder.prepare("n", offset, count).unwrap()
}

#[test]
fn equality_streams_seek_exact_numeric_domains_and_match_the_storage_oracle() {
    let ((), report) = run_async_under_lab(0x51ca_0101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let float = |value| CanonicalScalar::Float(CanonicalF64::new(value));
        let values = vec![
            CanonicalScalar::Int(0),
            float(-0.0),
            CanonicalScalar::Int(7),
            float(7.0),
            float(7.5),
            CanonicalScalar::Int(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0),
            CanonicalScalar::Int(i64::MIN),
            float(-9_223_372_036_854_775_808.0),
            CanonicalScalar::Int(i64::MAX),
            float(9_223_372_036_854_775_808.0),
            float(f64::NEG_INFINITY),
            float(f64::INFINITY),
            float(f64::NAN),
            CanonicalScalar::Bool(false),
            CanonicalScalar::Bool(true),
            CanonicalScalar::ucs_basic_text("").unwrap(),
            CanonicalScalar::ucs_basic_text("key").unwrap(),
        ];
        let mut batch = WriteBatch::new(R);
        for (id, value) in values.iter().enumerate() {
            batch.create_vertex(VId(id as u128), vec![], vec![(KEY, value.clone())]);
        }
        batch.create_vertex(VId(100), vec![], vec![(KEY, CanonicalScalar::Null)]);
        batch.create_vertex(VId(u128::MAX), vec![], vec![]);
        let at = db.write(&commit, batch).await.unwrap();
        let rows = db.vertices_at(at).unwrap();
        for literal in values.into_iter().chain([CanonicalScalar::Int(999)]) {
            let expected: Vec<_> = rows
                .iter()
                .filter(|row| {
                    IntegerComparison::Equal.accepts_scalar_pair(
                        row.props
                            .iter()
                            .find(|(key, _)| *key == KEY)
                            .map(|(_, value)| value),
                        Some(&literal),
                    )
                })
                .map(|row| row.vid)
                .collect();
            let query = equality(literal.clone(), 0, None);
            let eager = db
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap();
            assert_eq!(eager.value, expected, "literal={literal:?}");
            // A full scan exceeds this allowance. Only historical index
            // candidates may reach MVCC resolution and row admission.
            let mut cursor = db
                .stream_graph_vertices_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(expected.len() as u64, u64::MAX, u64::MAX, 0),
                )
                .unwrap();
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert_eq!(cursor.evaluator_stats().work_units, 0);
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected,
                "literal={literal:?}"
            );
            assert_eq!(cursor.row_stats().snapshot_records, expected.len() as u64);
            assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn equality_streams_keep_historical_candidates_lazy_and_recheck_the_pinned_winner() {
    let ((), report) = run_async_under_lab(0x51ca_0102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 100..356_u128 {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(KEY, CanonicalScalar::Int(id as i64))],
            );
        }
        for (vid, value) in [
            (VId(0), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Float(CanonicalF64::new(7.0))),
            (VId(u128::MAX), CanonicalScalar::Int(7)),
        ] {
            batch.create_vertex(vid, vec![], vec![(KEY, value)]);
        }
        let first = db.write(&commit, batch).await.unwrap();
        let query = equality(CanonicalScalar::Int(7), 0, None);
        let pinned = db
            .stream_graph_vertices_governed(&cx, &query, wide())
            .unwrap();
        let mut change = WriteBatch::new(R);
        // VId(0) becomes a candidate in BOTH numeric roots, but remains one
        // canonical output and one candidate admission.
        change.set_vertex_property(
            VId(0),
            KEY,
            Some(CanonicalScalar::Float(CanonicalF64::new(7.0))),
        );
        change.set_vertex_property(VId(3), KEY, Some(CanonicalScalar::Int(9)));
        change.delete_vertex(VId(u128::MAX));
        change.create_vertex(VId(2), vec![], vec![(KEY, CanonicalScalar::Int(7))]);
        let latest = db.write(&commit, change).await.unwrap();
        for compact in [false, true] {
            if compact {
                db.compact(&commit).await.unwrap();
            }
            for (at, expected) in [
                (CommitSeq::ORIGIN, vec![]),
                (first, vec![VId(0), VId(3), VId(u128::MAX)]),
                (latest, vec![VId(0), VId(2)]),
            ] {
                let mut cursor = db
                    .stream_graph_vertices_governed_at(
                        &cx,
                        &query,
                        at,
                        GqlQueryPolicy::new(4, 4, 10_000, 0),
                    )
                    .unwrap();
                assert_eq!(
                    cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected
                );
                assert_eq!(cursor.row_stats().snapshot_records, 4);
                assert_eq!(
                    db.execute_graph_pattern_governed_at(&cx, &query, at, wide())
                        .unwrap()
                        .value,
                    expected
                );
            }
        }
        // A zero page never resolves even the equality root or candidate history.
        let zero = equality(CanonicalScalar::Int(7), 0, Some(0));
        let mut empty = db
            .stream_graph_vertices_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(empty.next().is_none());
        assert_eq!(empty.row_stats().snapshot_records, 0);
        assert_eq!(empty.evaluator_stats().work_units, 1);
        // Each pull consumes only its selected candidate; close never drains.
        let one = equality(CanonicalScalar::Int(7), 0, Some(1));
        let mut limited = db
            .stream_graph_vertices_governed(&cx, &one, GqlQueryPolicy::new(1, 1, 10_000, 0))
            .unwrap();
        assert_eq!(limited.next().unwrap().unwrap(), VId(0));
        assert!(limited.next().is_none());
        assert_eq!(limited.row_stats().snapshot_records, 1);
        let mut cursor = db
            .stream_graph_vertices_governed(&cx, &query, wide())
            .unwrap();
        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let work = cursor.evaluator_stats().work_units;
        let mut refused = db
            .stream_graph_vertices_governed(&cx, &query, GqlQueryPolicy::new(4, 4, work - 1, 0))
            .unwrap();
        assert!(matches!(
            refused.by_ref().collect::<Result<Vec<_>, _>>(),
            Err(GqlQueryError::Evaluator(_))
        ));
        assert_eq!(refused.state(), VertexScanState::Failed);
        assert!(refused.next().is_none());
        drop(db);
        assert_eq!(
            pinned.collect::<Result<Vec<_>, _>>().unwrap(),
            vec![VId(0), VId(3), VId(u128::MAX)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn equality_inside_a_fallible_boolean_does_not_hide_the_prior_data_error() {
    let ((), report) = run_async_under_lab(0x51ca_0103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(0), vec![], vec![(KEY, CanonicalScalar::Int(0))]);
        db.write(&commit, batch).await.unwrap();
        let prepared = fgdb_gql::PreparedGraphText::prepare(
            "MATCH (n) WHERE 10 / n.p > 0 AND n.p = 7 RETURN n",
            |kind, name: &str| match (kind, name) {
                (fgdb_gql::GraphSymbolKind::Property, "p") => {
                    Some(fgdb_gql::GraphSymbol::Property(KEY))
                }
                _ => None,
            },
        )
        .unwrap()
        .bind_parameters(&fgdb_gql::GqlParameters::new())
        .unwrap();
        let mut cursor = db
            .stream_graph_values_governed(&cx, &prepared, wide())
            .unwrap();
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Data(error)))
                if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
        ));
        assert_eq!(cursor.row_stats().snapshot_records, 1);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (vid, value) in [(0, 0), (1, -1), (2, 2), (3, 3), (4, 4), (u128::MAX, 5)] {
        batch.create_vertex(
            VId(vid),
            if vid == 3 { vec![] } else { vec![LABEL] },
            vec![(KEY, CanonicalScalar::Int(value))],
        );
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn streams_pin_history_without_borrowing_the_database_view_or_prepared_definition() {
    let ((), report) = run_async_under_lab(0x51ca_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let prepared = pattern(0, None, true);
        let expected = db
            .execute_graph_pattern_governed(&cx, &prepared, wide())
            .unwrap()
            .value;
        assert_eq!(expected, vec![VId(0), VId(2), VId(4), VId(u128::MAX)]);
        let mut live_cursor = db
            .stream_graph_vertices_governed(&cx, &prepared, wide())
            .unwrap();
        let view = db.read_session().unwrap();
        let pinned_cursor = view
            .stream_graph_vertices_governed(&cx, &prepared, wide())
            .unwrap();
        drop(view);
        drop(prepared); // The public return types must not capture these borrows.
        assert_eq!(live_cursor.snapshot_seq(), basis);
        assert_eq!(
            live_cursor.row_stats(),
            GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0
            }
        );
        assert_eq!(live_cursor.next().unwrap().unwrap(), VId(0));

        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(2), KEY, Some(CanonicalScalar::Int(-9)));
        changes.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(10)));
        changes.set_vertex_label(VId(3), LABEL, true);
        changes.delete_vertex(VId(4));
        changes.create_vertex(VId(5), vec![LABEL], vec![(KEY, CanonicalScalar::Int(5))]);
        db.write(&commit, changes).await.unwrap(); // Must compile with the cursors still alive.
        let prepared = pattern(0, None, true);
        let current = db
            .execute_graph_pattern_governed(&cx, &prepared, wide())
            .unwrap()
            .value;
        assert_eq!(
            current,
            vec![VId(0), VId(1), VId(3), VId(5), VId(u128::MAX)]
        );
        let historical = db
            .stream_graph_vertices_governed_at(&cx, &prepared, basis, wide())
            .unwrap();
        let current_cursor = db
            .stream_graph_vertices_governed(&cx, &prepared, wide())
            .unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        assert_eq!(
            live_cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected[1..]
        );
        assert_eq!(live_cursor.state(), VertexScanState::Exhausted);
        assert_eq!(
            pinned_cursor.collect::<Result<Vec<_>, _>>().unwrap(),
            expected
        );
        assert_eq!(historical.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(
            current_cursor.collect::<Result<Vec<_>, _>>().unwrap(),
            current
        );
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .stream_graph_vertices_governed(&cx, &prepared, wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            current
        );
        assert_eq!(
            reopened
                .stream_graph_vertices_governed_at(&cx, &prepared, basis, wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_cuts_match_snapshot_execution_through_updates_restatements_and_compaction() {
    let ((), report) = run_async_under_lab(0x51ca_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        for seq in 2..=5 {
            let mut changes = WriteBatch::new(R);
            changes.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(seq - 3)));
            changes.set_vertex_label(VId(3), LABEL, seq % 2 == 0);
            changes.create_vertex(
                VId(10 + seq as u128),
                vec![LABEL],
                vec![(KEY, CanonicalScalar::Int(seq))],
            );
            if seq == 4 {
                changes.delete_vertex(VId(2));
            }
            db.write(&commit, changes).await.unwrap();
        }
        for compact in [false, true] {
            if compact {
                db.compact(&commit).await.unwrap();
            }
            let view = db.read_session().unwrap();
            for sequence in 0..=5 {
                for skip in [0, 1, 3, u64::MAX] {
                    for limit in [None, Some(0), Some(1), Some(3)] {
                        for all in [false, true] {
                            let prepared = pattern(skip, limit, true);
                            let prepared = if all {
                                prepared.with_duplicates()
                            } else {
                                prepared
                            };
                            let expected = db
                                .execute_graph_pattern_governed_at(
                                    &cx,
                                    &prepared,
                                    CommitSeq(sequence),
                                    wide(),
                                )
                                .unwrap()
                                .value;
                            let mut actual = view
                                .stream_graph_vertices_governed_at(
                                    &cx,
                                    &prepared,
                                    CommitSeq(sequence),
                                    wide(),
                                )
                                .unwrap();
                            assert_eq!(
                                actual.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                                expected
                            );
                            assert_eq!(actual.snapshot_seq(), CommitSeq(sequence));
                            assert_eq!(actual.row_stats().result_rows, expected.len() as u64);
                            assert!(actual.next().is_none());
                        }
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn first_rows_do_not_admit_the_remaining_table_and_limits_survive_consumer_pauses() {
    let ((), report) = run_async_under_lab(0x51ca_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for start in (0..2048).step_by(128) {
            let mut batch = WriteBatch::new(R);
            for id in start..start + 128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
            db.write(&commit, batch).await.unwrap();
        }
        let prepared = pattern(0, Some(2), false);
        let mut cursor = db
            .stream_graph_vertices_governed(&cx, &prepared, GqlQueryPolicy::new(2, 2, 512, 0))
            .unwrap();
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.evaluator_stats().work_units, 0);
        assert_eq!(cursor.next().unwrap().unwrap(), VId(0));
        assert_eq!(cursor.row_stats().snapshot_records, 1);
        assert_eq!(cursor.next().unwrap().unwrap(), VId(1));
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert_eq!(cursor.row_stats().snapshot_records, 2);
        assert!(cursor.evaluator_stats().work_units < 512);
        assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
        assert!(cursor.next().is_none());

        let unbounded = pattern(0, None, false);
        for records in [true, false] {
            let policy = if records {
                GqlQueryPolicy::new(2, u64::MAX, u64::MAX, u64::MAX)
            } else {
                GqlQueryPolicy::new(u64::MAX, 2, u64::MAX, u64::MAX)
            };
            let mut cursor = db
                .stream_graph_vertices_governed(&cx, &unbounded, policy)
                .unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), VId(0));
            assert_eq!(cursor.next().unwrap().unwrap(), VId(1));
            assert!(
                matches!(cursor.next(), Some(Err(GqlQueryError::Rows(error)))
                if error.dimension == (if records { GqlBudgetDimension::SnapshotRecords } else { GqlBudgetDimension::ResultRows })
                    && error.limit == 2 && error.observed == 3)
            );
            assert_eq!(cursor.row_stats().result_rows, 2);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let mut cursor = db
            .stream_graph_vertices_governed(&cx, &unbounded, wide())
            .unwrap();
        cursor.next().unwrap().unwrap();
        let stats = (cursor.row_stats(), cursor.evaluator_stats());
        cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Closed);
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owned_preparation_future_fences_unsupported_shapes_and_zero_limit_use_the_same_stream() {
    let ((), report) = run_async_under_lab(0x51ca_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let bind = RelationBind::new()
            .with_label("Thing", LABEL)
            .with_property("score", KEY);
        let prepared = PreparedGqlQuery::prepare(
            "MATCH (n:Thing) WHERE n.score >= 0 RETURN n SKIP 1 LIMIT 2",
            &bind,
        )
        .unwrap();
        let view = db.read_session().unwrap();
        let a = db
            .stream_prepared_query_governed(&cx, &prepared, wide())
            .unwrap();
        let b = db
            .stream_prepared_query_governed_at(&cx, &prepared, basis, wide())
            .unwrap();
        let c = view
            .stream_prepared_query_governed(&cx, &prepared, wide())
            .unwrap();
        let d = view
            .stream_prepared_query_governed_at(&cx, &prepared, basis, wide())
            .unwrap();
        drop(prepared);
        drop(bind);
        drop(view);
        for result in [
            a.collect::<Result<Vec<_>, _>>().unwrap(),
            b.collect::<Result<Vec<_>, _>>().unwrap(),
            c.collect::<Result<Vec<_>, _>>().unwrap(),
            d.collect::<Result<Vec<_>, _>>().unwrap(),
        ] {
            assert_eq!(result, vec![VId(2), VId(4)]);
        }
        let zero = pattern(u64::MAX, Some(0), false);
        let mut cursor = db
            .stream_graph_vertices_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(cursor.next().is_none());
        assert_eq!(
            cursor.row_stats(),
            GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0
            }
        );
        let future = CommitSeq(basis.0 + 1);
        assert!(matches!(
            db.stream_graph_vertices_governed_at(
                &cx,
                &zero,
                future,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(VertexScanError::Source(
                ReadError::BeyondFrontier { .. }
            )))
        ));
        let mut edge = GraphPatternBuilder::new();
        edge.vertex("a").unwrap();
        edge.vertex("b").unwrap();
        edge.edge("a", R, GlaDirection::Forward, "b").unwrap();
        let edge = edge.prepare("a", 0, None).unwrap();
        assert!(matches!(
            db.stream_graph_vertices_governed(&cx, &edge, wide()),
            Err(GqlQueryError::Source(VertexScanError::Plan(_)))
        ));
        assert!(matches!(
            db.stream_graph_vertices_governed_at(&cx, &edge, future, wide()),
            Err(GqlQueryError::Source(VertexScanError::Source(
                ReadError::BeyondFrontier { .. }
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_work_limits_and_consumer_chunk_sizes_preserve_one_execution_budget() {
    let ((), report) = run_async_under_lab(0x51ca_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let prepared = pattern(1, None, true);
        let mut baseline = db
            .stream_graph_vertices_governed(&cx, &prepared, wide())
            .unwrap();
        let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = baseline.row_stats();
        let evaluator = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            evaluator.work_units,
            evaluator.scratch_entries,
        );
        for width in 1..=5 {
            let mut cursor = db
                .stream_graph_vertices_governed(&cx, &prepared, exact)
                .unwrap();
            let mut actual = Vec::new();
            while cursor.state() == VertexScanState::Open {
                actual.extend(
                    cursor
                        .by_ref()
                        .take(width)
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                );
            }
            assert_eq!(actual, expected);
            assert_eq!(cursor.row_stats(), rows);
            assert_eq!(cursor.evaluator_stats(), evaluator);
        }
        let too_small = GqlQueryPolicy::new(
            rows.snapshot_records,
            rows.result_rows,
            evaluator.work_units - 1,
            u64::MAX,
        );
        let mut refused = db
            .stream_graph_vertices_governed(&cx, &prepared, too_small)
            .unwrap();
        assert!(matches!(
            refused.by_ref().collect::<Result<Vec<_>, _>>(),
            Err(GqlQueryError::Evaluator(_))
        ));
        assert_eq!(refused.state(), VertexScanState::Failed);
        assert!(refused.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn value_pattern(
    offset: u64,
    count: Option<u64>,
) -> PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow> {
    use fgdb_gql::algebra::GraphColumn;
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("score", "n", KEY),
                GraphColumn::property("name", "n", PropertyKeyId(8)),
                GraphColumn::vertex("again", "n"),
            ],
            offset,
            count,
        )
        .unwrap()
}

#[test]
fn correlated_property_rows_stream_from_pinned_live_and_historical_generations() {
    let ((), report) = run_async_under_lab(0x51ca_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit).await;
        let mut names = WriteBatch::new(R);
        names.set_vertex_property(
            VId(0),
            PropertyKeyId(8),
            Some(CanonicalScalar::ucs_basic_text(&"long name".repeat(40)).unwrap()),
        );
        names.set_vertex_property(VId(1), KEY, None);
        names.set_vertex_property(VId(2), KEY, Some(CanonicalScalar::Null));
        let basis = db.write(&commit, names).await.unwrap();
        let pattern = value_pattern(0, None);
        let expected = db
            .execute_graph_pattern_governed(&cx, &pattern, wide())
            .unwrap()
            .value;
        let mut stream = db
            .stream_graph_values_governed(&cx, &pattern, wide())
            .unwrap();
        let view = db.read_session().unwrap();
        let pinned = view
            .stream_graph_values_governed(&cx, &pattern, wide())
            .unwrap();
        let pinned_at = view
            .stream_graph_values_governed_at(&cx, &pattern, basis, wide())
            .unwrap();
        drop(view);
        drop(pattern);
        let first = stream.next().unwrap().unwrap();
        assert_eq!(first, expected[0]);
        assert_eq!(first.get(0), first.get(3));
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(0), PropertyKeyId(8), None);
        edit.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(999)));
        edit.delete_vertex(VId(2));
        edit.create_vertex(VId(5), vec![], vec![(KEY, CanonicalScalar::Int(-8))]);
        db.write(&commit, edit).await.unwrap();
        let pattern = value_pattern(0, None).with_duplicates();
        let current = db
            .execute_graph_pattern_governed(&cx, &pattern, wide())
            .unwrap()
            .value;
        let historical = db
            .stream_graph_values_governed_at(&cx, &pattern, basis, wide())
            .unwrap();
        let live = db
            .stream_graph_values_governed(&cx, &pattern, wide())
            .unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        assert_eq!(
            stream.collect::<Result<Vec<_>, _>>().unwrap(),
            expected[1..]
        );
        assert_eq!(pinned.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(pinned_at.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(historical.collect::<Result<Vec<_>, _>>().unwrap(), expected);
        assert_eq!(live.collect::<Result<Vec<_>, _>>().unwrap(), current);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .stream_graph_values_governed_at(&cx, &pattern, basis, wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
        assert_eq!(
            reopened
                .stream_graph_values_governed(&cx, &pattern, wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            current
        );
        assert_eq!(
            first, expected[0],
            "delivered values own their scalar payload after source release"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn value_streams_keep_exact_shared_budgets_and_refuse_unstreamable_order_before_scanning() {
    let ((), report) = run_async_under_lab(0x51ca_0007, |root| async move {
        use fgdb_gql::algebra::GraphColumn;
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let pattern = value_pattern(1, Some(3));
        let mut baseline = db
            .stream_graph_values_governed(&cx, &pattern, wide())
            .unwrap();
        let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let rows = baseline.row_stats();
        let stats = baseline.evaluator_stats();
        assert_eq!(
            rows,
            GqlExecutionStats {
                snapshot_records: 4,
                result_rows: 3
            }
        );
        let exact = GqlQueryPolicy::new(4, 3, stats.work_units, stats.scratch_entries);
        let mut cursor = db
            .stream_graph_values_governed(&cx, &pattern, exact)
            .unwrap();
        let mut actual = Vec::new();
        actual.extend(
            cursor
                .by_ref()
                .take(1)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
        actual.extend(
            cursor
                .by_ref()
                .take(1)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
        actual.extend(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap());
        assert_eq!(actual, expected);
        assert_eq!(cursor.evaluator_stats(), stats);
        for policy in [
            GqlQueryPolicy::new(3, 3, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(4, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(4, 3, stats.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(4, 3, u64::MAX, stats.scratch_entries - 1),
        ] {
            let mut failed = db
                .stream_graph_values_governed(&cx, &pattern, policy)
                .unwrap();
            assert!(failed.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(failed.state(), VertexScanState::Failed);
            assert!(failed.next().is_none());
        }
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let property_only = builder
            .prepare_values(&[GraphColumn::property("score", "n", KEY)], 0, None)
            .unwrap();
        assert!(matches!(
            db.stream_graph_values_governed(&cx, &property_only, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(VertexScanError::Plan(_)))
        ));
        let empty = value_pattern(u64::MAX, Some(0));
        let mut empty = db
            .stream_graph_values_governed(&cx, &empty, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(empty.next().is_none());
        assert_eq!(empty.evaluator_stats().scratch_entries, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[path = "vertex_stream/ranges.rs"]
mod ranges;

fn prefix_pattern(
    prefix: &str,
    count: Option<u64>,
) -> PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow> {
    let value = CanonicalScalar::ucs_basic_text(prefix).unwrap();
    let kind = fgdb_types::CanonicalScalarKind::of(&value);
    let args = fgdb_gql::GqlParameters::new()
        .with_scalar("prefix", value)
        .unwrap();
    let text = format!(
        "MATCH (n) WHERE n.p STARTS WITH $prefix RETURN n{}",
        count.map_or(String::new(), |count| format!(" LIMIT {count}")),
    );
    fgdb_gql::PreparedGraphText::prepare_with_parameter_types(
        &text,
        &[("prefix", fgdb_gql::GqlParameterType::Scalar(kind))],
        |kind, name: &str| match (kind, name) {
            (fgdb_gql::GraphSymbolKind::Property, "p") => {
                Some(fgdb_gql::GraphSymbol::Property(KEY))
            }
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&args)
    .unwrap()
}

#[test]
fn prefix_streams_keep_unicode_boundaries_and_the_pinned_historical_winner() {
    let ((), report) = run_async_under_lab(0x51ca_0203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let spellings = [
            "",
            "Ad",
            "Ada",
            "B",
            "éclair",
            "\u{d7ff}",
            "\u{d7ff}tail",
            "\u{e000}",
            "\u{10ffff}",
            "\u{10ffff}tail",
        ];
        let mut batch = WriteBatch::new(R);
        for (id, text) in spellings.iter().enumerate() {
            batch.create_vertex(
                VId(id as u128),
                vec![],
                vec![(KEY, CanonicalScalar::ucs_basic_text(text).unwrap())],
            );
        }
        batch.create_vertex(
            VId(u128::MAX),
            vec![],
            vec![(KEY, CanonicalScalar::ucs_basic_text("Adam").unwrap())],
        );
        batch.create_vertex(VId(100), vec![], vec![(KEY, CanonicalScalar::Null)]);
        batch.create_vertex(VId(101), vec![], vec![]);
        let first = db.write(&commit, batch).await.unwrap();
        for prefix in ["", "Ad", "é", "\u{d7ff}", "\u{10ffff}", "absent"] {
            let query = prefix_pattern(prefix, None);
            let expected = db
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap()
                .value;
            let expected_ids: Vec<_> = spellings
                .iter()
                .enumerate()
                .filter(|(_, value)| value.starts_with(prefix))
                .map(|(id, _)| VId(id as u128))
                .chain("Adam".starts_with(prefix).then_some(VId(u128::MAX)))
                .collect();
            assert_eq!(
                expected
                    .iter()
                    .map(|row| row.get(0).unwrap().as_vertex().unwrap())
                    .collect::<Vec<_>>(),
                expected_ids
            );
            let mut stream = db
                .stream_graph_values_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(expected.len() as u64, 100, 100_000, 1000),
                )
                .unwrap();
            assert_eq!(stream.row_stats().snapshot_records, 0);
            assert_eq!(
                stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(stream.row_stats().snapshot_records, expected.len() as u64);
        }
        let query = prefix_pattern("Ad", None);
        let expected_first = db
            .execute_graph_pattern_governed(&cx, &query, wide())
            .unwrap()
            .value;
        let pinned = db
            .stream_graph_values_governed(&cx, &query, wide())
            .unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(
            VId(1),
            KEY,
            Some(CanonicalScalar::ucs_basic_text("Adele").unwrap()),
        );
        change.set_vertex_property(
            VId(2),
            KEY,
            Some(CanonicalScalar::ucs_basic_text("Bea").unwrap()),
        );
        change.delete_vertex(VId(u128::MAX));
        change.create_vertex(
            VId(99),
            vec![],
            vec![(KEY, CanonicalScalar::ucs_basic_text("Adr").unwrap())],
        );
        let latest = db.write(&commit, change).await.unwrap();
        db.compact(&commit).await.unwrap();
        for at in [CommitSeq::ORIGIN, first, latest] {
            let expected = db
                .execute_graph_pattern_governed_at(&cx, &query, at, wide())
                .unwrap()
                .value;
            let mut stream = db
                .stream_graph_values_governed_at(
                    &cx,
                    &query,
                    at,
                    GqlQueryPolicy::new(4, 4, 100_000, 1000),
                )
                .unwrap();
            assert_eq!(
                stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(stream.row_stats().snapshot_records, 4);
        }
        let zero = prefix_pattern("Ad", Some(0));
        let mut stream = db
            .stream_graph_values_governed(&cx, &zero, GqlQueryPolicy::new(0, 0, 1, 0))
            .unwrap();
        assert!(stream.next().is_none());
        assert_eq!(stream.row_stats().snapshot_records, 0);
        assert_eq!(stream.evaluator_stats().work_units, 1);
        drop(db);
        assert_eq!(
            pinned.collect::<Result<Vec<_>, _>>().unwrap(),
            expected_first
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prefix_selection_keeps_nontext_unknowns_and_boolean_boundaries() {
    let ((), report) = run_async_under_lab(0x51ca_0204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, value) in [
            (0, CanonicalScalar::Int(0)),
            (1, CanonicalScalar::ucs_basic_text("Ab").unwrap()),
            (2, CanonicalScalar::ucs_basic_text("B").unwrap()),
            (3, CanonicalScalar::Null),
        ] {
            batch.create_vertex(VId(id), vec![], vec![(KEY, value)]);
        }
        batch.create_vertex(VId(4), vec![], vec![]);
        db.write(&commit, batch).await.unwrap();
        for (predicate, expected_ids) in [
            ("n.p STARTS WITH 'A'", vec![VId(1)]),
            ("n.p STARTS WITH NULL", vec![]),
            ("n.p STARTS WITH 'A' OR n.p = 'B'", vec![VId(1), VId(2)]),
            ("NOT (n.p STARTS WITH 'A')", vec![VId(2)]),
            (
                "(n.p STARTS WITH 'A' OR n.p = 'B') AND n.p <> 'Ab'",
                vec![VId(2)],
            ),
        ] {
            let text = format!("MATCH (n) WHERE {predicate} RETURN n");
            let query = fgdb_gql::PreparedGraphText::prepare(&text, |kind, name: &str| {
                match (kind, name) {
                    (fgdb_gql::GraphSymbolKind::Property, "p") => {
                        Some(fgdb_gql::GraphSymbol::Property(KEY))
                    }
                    _ => None,
                }
            })
            .unwrap()
            .bind_parameters(&fgdb_gql::GqlParameters::new())
            .unwrap();
            let expected = db
                .execute_graph_pattern_governed(&cx, &query, wide())
                .unwrap()
                .value;
            let mut stream = db
                .stream_graph_values_governed(&cx, &query, GqlQueryPolicy::new(5, 5, 100_000, 1000))
                .unwrap();
            let rows = stream.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected, "{predicate}");
            assert_eq!(
                rows.iter()
                    .map(|row| row.get(0).unwrap().as_vertex().unwrap())
                    .collect::<Vec<_>>(),
                expected_ids,
                "{predicate}"
            );
            // A mixed-type history prevents prefix pruning; OR, NOT and NULL
            // operands are independently ineligible for a direct prefix seek.
            assert_eq!(stream.row_stats().snapshot_records, 5, "{predicate}");
            assert_eq!(stream.state(), VertexScanState::Exhausted);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prefix_and_range_selection_cannot_hide_earlier_arithmetic_errors() {
    let ((), report) = run_async_under_lab(0x51ca_0205, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(0), vec![], vec![(KEY, CanonicalScalar::Int(0))]);
        batch.create_vertex(
            VId(1),
            vec![],
            vec![(KEY, CanonicalScalar::ucs_basic_text("Ab").unwrap())],
        );
        db.write(&commit, batch).await.unwrap();
        for text in [
            "MATCH (n) WHERE 10 / n.p > 0 AND n.p > 7 RETURN n",
            "MATCH (n) WHERE 10 / n.p > 0 AND n.p STARTS WITH 'A' RETURN n",
        ] {
            let query =
                fgdb_gql::PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
                    (fgdb_gql::GraphSymbolKind::Property, "p") => {
                        Some(fgdb_gql::GraphSymbol::Property(KEY))
                    }
                    _ => None,
                })
                .unwrap()
                .bind_parameters(&fgdb_gql::GqlParameters::new())
                .unwrap();
            assert!(
                matches!(db.execute_graph_pattern_governed(&cx, &query, wide()),
                Err(GqlQueryError::Data(error)) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)
            );
            let mut stream = db
                .stream_graph_values_governed(&cx, &query, wide())
                .unwrap();
            assert!(
                matches!(stream.next(),
                Some(Err(GqlQueryError::Data(error))) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero),
                "{text}"
            );
            assert_eq!(stream.row_stats().snapshot_records, 1);
            assert_eq!(stream.state(), VertexScanState::Failed);
            assert!(stream.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
