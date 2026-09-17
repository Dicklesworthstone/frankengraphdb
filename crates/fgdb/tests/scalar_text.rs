//! Scalar WHERE operands reach the real snapshot and canonical overlay sources.
//! These expectations are fixture-derived, not results from a second GLA query.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const STATUS: PropertyKeyId = PropertyKeyId(1);
const ACTIVE: PropertyKeyId = PropertyKeyId(2);
const APPROVED: PropertyKeyId = PropertyKeyId(3);
const NOTE: PropertyKeyId = PropertyKeyId(4);
const HIGH: VId = VId((1_u128 << 100) + 7);
const OPTIONAL: &str = "MATCH (n:Person) WHERE n.active = TRUE \
    OPTIONAL MATCH (n)-[:R]->(c) WHERE c.approved = TRUE AND c.note IS NULL";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 100_000)
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "status") => Some(GraphSymbol::Property(STATUS)),
        (GraphSymbolKind::Property, "active") => Some(GraphSymbol::Property(ACTIVE)),
        (GraphSymbolKind::Property, "approved") => Some(GraphSymbol::Property(APPROVED)),
        (GraphSymbolKind::Property, "note") => Some(GraphSymbol::Property(NOTE)),
        _ => None,
    }
}
fn query(statement: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(statement, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
        .collect()
}
fn pairs(rows: &[GraphValueRow]) -> Vec<(VId, Option<VId>)> {
    rows.iter()
        .map(|row| {
            let child = row.get(1).unwrap();
            assert!(child.is_null() || child.as_vertex().is_some());
            (row.get(0).unwrap().as_vertex().unwrap(), child.as_vertex())
        })
        .collect()
}
fn counts(rows: &[GraphAggregateRow]) -> Vec<(VId, u64, u64)> {
    rows.iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_count().unwrap(),
            )
        })
        .collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (vid, status, active, note) in [
        (VId(0), text("ready"), CanonicalScalar::Bool(true), None),
        (
            VId(1),
            text("ready"),
            CanonicalScalar::Bool(false),
            Some(CanonicalScalar::Null),
        ),
        (
            VId(2),
            text("other"),
            CanonicalScalar::Bool(true),
            Some(text("present")),
        ),
        (
            VId(3),
            CanonicalScalar::Int(1),
            CanonicalScalar::Int(1),
            Some(CanonicalScalar::Bool(false)),
        ),
        (HIGH, text("O'Reilly 🦀"), CanonicalScalar::Bool(true), None),
    ] {
        let mut props = vec![(STATUS, status), (ACTIVE, active)];
        if let Some(value) = note {
            props.push((NOTE, value));
        }
        batch.create_vertex(vid, vec![PERSON], props);
    }
    batch.create_vertex(
        VId(10),
        vec![],
        vec![(APPROVED, CanonicalScalar::Bool(true))],
    );
    batch.create_vertex(
        VId(11),
        vec![],
        vec![(APPROVED, CanonicalScalar::Bool(false))],
    );
    for (eid, source, destination) in [
        (100, VId(0), VId(10)),
        (101, VId(0), VId(10)),
        (102, VId(1), VId(11)),
        (103, VId(2), VId(10)),
        (104, HIGH, VId(11)),
    ] {
        batch.add_edge(EId(eid), source, destination, vec![]);
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn scalar_text_preserves_types_and_nulls_on_all_five_read_entrypoints() {
    let ((), report) = run_async_under_lab(0x5ca1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        // Candidate counts derive from this fixture's own vertex properties,
        // never from an engine counter: how many of the seven vertices carry
        // that exact key value in the seed batch below (V0..V4 and HIGH;
        // rows [V0,V1], [V2,HIGH], [HIGH], [V0,V2,HIGH], [V0,V2,HIGH],
        // [V3], and none for the note/NULL shapes). Non-equality and
        // null-comparison shapes never consult the index: 7 full-domain
        // records. The ruling (fgdb-bupm 2026-09-17): an equality lookup
        // charges only the candidates it resolves.
        for (predicate, expected, indexed_records) in [
            ("n.status = 'ready'", vec![VId(0), VId(1)], 2),
            ("n.status <> 'ready'", vec![VId(2), HIGH], 7),
            ("n.status = 'O''Reilly 🦀'", vec![HIGH], 1),
            ("n.active = TRUE", vec![VId(0), VId(2), HIGH], 3),
            ("n.active <> false", vec![VId(0), VId(2), HIGH], 7),
            ("n.active = 1", vec![VId(3)], 1),
            ("n.note IS NULL", vec![VId(0), VId(1), HIGH], 7),
            ("n.note IS NOT NULL", vec![VId(2), VId(3)], 7),
            ("n.note = NULL", vec![], 7),
            ("n.note <> NULL", vec![], 7),
        ] {
            let pattern = query(&format!("MATCH (n:Person) WHERE {predicate} RETURN n"));
            for (entrypoint, result) in [
                db.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap(),
                db.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                    .unwrap(),
                view.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap(),
                view.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                    .unwrap(),
                txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy())
                    .unwrap(),
            ]
            .into_iter()
            .enumerate()
            {
                assert_eq!(ids(&result.value), expected, "{predicate}");
                // Transaction overlays have their own complete canonical source.
                let records = if entrypoint == 4 { 7 } else { indexed_records };
                assert_eq!(result.rows.snapshot_records, records, "{predicate}");
            }
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scalar_optional_and_aggregate_text_use_canonical_staged_values_and_retained_history() {
    let ((), report) = run_async_under_lab(0x5ca1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let pattern = query(&format!("{OPTIONAL} RETURN n,c"));
        let absent = query(
            "MATCH (n:Person) WHERE n.active = TRUE AND NOT EXISTS { \
            MATCH (n)-[:R]->(c) WHERE c.approved = TRUE AND c.note IS NULL } RETURN n",
        );
        let summary = PreparedGraphAggregateText::prepare(
            &format!("{OPTIONAL} RETURN n,COUNT(*) AS occurrences,COUNT(c) AS present GROUP BY n"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let before = vec![
            (VId(0), Some(VId(10))),
            (VId(0), Some(VId(10))),
            (VId(2), Some(VId(10))),
            (HIGH, None),
        ];
        assert_eq!(
            pairs(
                &db.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        assert_eq!(
            ids(&db
                .execute_graph_pattern_governed(&cx, &absent, policy())
                .unwrap()
                .value),
            vec![HIGH]
        );
        assert_eq!(
            counts(
                &db.execute_graph_aggregate_governed(&cx, &summary, policy())
                    .unwrap()
                    .value
            ),
            vec![(VId(0), 2, 2), (VId(2), 1, 1), (HIGH, 1, 0)]
        );

        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.set_vertex_property(VId(11), APPROVED, Some(CanonicalScalar::Bool(true)));
        stage.set_vertex_property(VId(10), APPROVED, None);
        stage.set_vertex_property(VId(0), ACTIVE, Some(CanonicalScalar::Bool(false)));
        stage.set_vertex_property(VId(2), STATUS, None);
        stage.ensure_edge_by_triple(EId(999), HIGH, VId(11), vec![]);
        txn.write(&mut db, stage).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = vec![(VId(2), None), (HIGH, Some(VId(11)))];
        let staged = txn
            .execute_graph_pattern_governed(&db, &cx, &pattern, policy())
            .unwrap();
        assert_eq!(pairs(&staged.value), expected);
        let exact = GqlQueryPolicy::new(
            staged.rows.snapshot_records,
            staged.rows.result_rows,
            staged.evaluator.work_units,
            staged.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_pattern_governed(&db, &cx, &pattern, exact)
                .unwrap(),
            staged
        );
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &absent, policy())
                .unwrap()
                .value),
            vec![VId(2)]
        );
        let removed = query("MATCH (n:Person) WHERE n.status IS NULL RETURN n");
        assert_eq!(
            ids(&txn
                .execute_graph_pattern_governed(&db, &cx, &removed, policy())
                .unwrap()
                .value),
            vec![VId(2)]
        );
        assert_eq!(
            counts(
                &txn.execute_graph_aggregate_governed(&db, &cx, &summary, policy())
                    .unwrap()
                    .value
            ),
            vec![(VId(2), 1, 0), (HIGH, 1, 1)]
        );
        assert_eq!(
            pairs(
                &db.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            pairs(
                &db.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            pairs(
                &db.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        assert_eq!(
            pairs(
                &view
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        assert_eq!(
            counts(
                &db.execute_graph_aggregate_governed_at(&cx, &summary, basis, policy())
                    .unwrap()
                    .value
            ),
            vec![(VId(0), 2, 2), (VId(2), 1, 1), (HIGH, 1, 0)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_scalar_rows_and_matching_insertions_conflict_even_after_result_refusal() {
    let ((), report) = run_async_under_lab(0x5ca1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for refused in [false, true] {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R);
                stage.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let pattern =
                    query("MATCH (n:Person) WHERE n.status = 'ready' AND n.active = TRUE RETURN n");
                let result = txn.execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &pattern,
                    GqlQueryPolicy::new(100, if refused { 0 } else { 100 }, 1_000_000, 100_000),
                );
                if refused {
                    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
                } else {
                    assert_eq!(ids(&result.unwrap().value), vec![VId(0)]);
                }
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => winner.create_vertex(VId(77), vec![], vec![]),
                    1 => winner.set_vertex_property(
                        VId(1),
                        ACTIVE,
                        Some(CanonicalScalar::Bool(true)),
                    ),
                    _ => winner.create_vertex(
                        VId(77),
                        vec![PERSON],
                        vec![
                            (STATUS, text("ready")),
                            (ACTIVE, CanonicalScalar::Bool(true)),
                        ],
                    ),
                };
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if change == 0 {
                    result.unwrap();
                    assert!(db.vertex(VId(99)).unwrap().is_some());
                } else {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scalar_queries_keep_exact_policy_boundaries_and_real_runtime_cancellation() {
    let ((), report) = run_async_under_lab(0x5ca1_0004, |root| async move {
        // Cancel the query child, keeping the lab supervisor available to join it.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let cx = contexts.query();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let pattern = query("MATCH (n:Person) WHERE n.status = 'ready' RETURN n");
                let full = db
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap();
                assert_eq!(ids(&full.value), vec![VId(0), VId(1)]);
                let exact = GqlQueryPolicy::new(
                    2,
                    2,
                    full.evaluator.work_units,
                    full.evaluator.scratch_entries,
                );
                assert_eq!(
                    db.execute_graph_pattern_governed(&cx, &pattern, exact)
                        .unwrap(),
                    full
                );
                // fgdb-j8mu: the range predicate is served from the maintained
                // property index; the source layer charges one SnapshotRecord
                // per resolved candidate (fgdb-bupm ruling), so the full
                // seven-row domain is no longer read. The fixture's own seed
                // batch fixes the candidate domain at two 'ready' rows
                // (V0, V1) — never engine counters.
                let range = query("MATCH (n:Person) WHERE n.status >= 'r' RETURN n");
                let range_records = 2;
                assert_eq!(
                    db.execute_graph_pattern_governed(&cx, &range, policy())
                        .unwrap()
                        .rows
                        .snapshot_records,
                    range_records
                );
                // Exact candidate-boundary refusals: one below the candidate
                // count refuses, exactly the candidate count succeeds.
                assert!(matches!(
                    db.execute_graph_pattern_governed(
                        &cx,
                        &range,
                        GqlQueryPolicy::new(1, 2, u64::MAX, u64::MAX)
                    ),
                    Err(GqlQueryError::Rows(_))
                ));
                assert!(matches!(
                    db.execute_graph_pattern_governed(
                        &cx,
                        &range,
                        GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX)
                    ),
                    Ok(_)
                ));
                // Mail 979, condition 3: preserve the full-domain scan twin.
                // The independent OPTIONAL domain defeats single-domain index
                // eligibility; identity correlation and DISTINCT preserve n.
                let scan_range = query(
                    "MATCH (n:Person) WHERE n.status >= 'r' \
                     OPTIONAL MATCH (other) WHERE other=n RETURN DISTINCT n",
                );
                let range_records = 7; // All seven vertices from seed().
                let scan = db
                    .execute_graph_pattern_governed(&cx, &scan_range, policy())
                    .unwrap();
                assert_eq!(ids(&scan.value), vec![VId(0), VId(1)]);
                assert_eq!(scan.rows.snapshot_records, range_records);
                assert!(matches!(
                    db.execute_graph_pattern_governed(
                        &cx,
                        &scan_range,
                        GqlQueryPolicy::new(6, 2, u64::MAX, u64::MAX)
                    ),
                    Err(GqlQueryError::Rows(error))
                        if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                            && error.limit == 6 && error.observed == 7
                ));
                assert_eq!(
                    db.execute_graph_pattern_governed(
                        &cx,
                        &scan_range,
                        GqlQueryPolicy::new(7, 2, u64::MAX, u64::MAX)
                    )
                    .unwrap(),
                    scan
                );
                for cap in [
                    GqlQueryPolicy::new(1, 2, u64::MAX, u64::MAX),
                    GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX),
                ] {
                    assert!(matches!(
                        db.execute_graph_pattern_governed(&cx, &pattern, cap),
                        Err(GqlQueryError::Rows(_))
                    ));
                }
                for cap in [
                    GqlQueryPolicy::new(2, 2, full.evaluator.work_units - 1, u64::MAX),
                    GqlQueryPolicy::new(2, 2, u64::MAX, full.evaluator.scratch_entries - 1),
                ] {
                    assert!(matches!(
                        db.execute_graph_pattern_governed(&cx, &pattern, cap),
                        Err(GqlQueryError::Evaluator(_))
                    ));
                }
                root.cancel_with(
                    CancelKind::User,
                    Some("scalar query cancellation regression"),
                );
                assert!(matches!(
                    db.execute_graph_pattern_governed(&cx, &pattern, policy()),
                    Err(GqlQueryError::Interrupted(_))
                ));
            })
            .expect("lab query task can be spawned");
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok(), "the supervisor remains live");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
