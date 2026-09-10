//! Declared scalar arguments execute against real snapshots and canonical effects.
use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
    PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CanonicalScalarKind, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const STATUS: PropertyKeyId = PropertyKeyId(1);
const ACTIVE: PropertyKeyId = PropertyKeyId(2);
const CATEGORY: PropertyKeyId = PropertyKeyId(3);
const HIGH: VId = VId((1_u128 << 100) + 7);
const WANTED: &str = "partner's \u{1f980} '$status";
const HEAD: &str = "MATCH (p:Person) WHERE p.status=$status AND p.active=$active \
    OPTIONAL MATCH (p)-[:R]->(c) WHERE c.category=$category";
const TYPES: [(&str, GqlParameterType); 3] = [
    (
        "status",
        GqlParameterType::Scalar(CanonicalScalarKind::Text),
    ),
    (
        "active",
        GqlParameterType::Scalar(CanonicalScalarKind::Bool),
    ),
    (
        "category",
        GqlParameterType::Scalar(CanonicalScalarKind::Text),
    ),
];
type Plain = (VId, Option<VId>, Option<CanonicalScalar>);
type Summary = (VId, u64, u64, u64);

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "status") => Some(GraphSymbol::Property(STATUS)),
        (GraphSymbolKind::Property, "active") => Some(GraphSymbol::Property(ACTIVE)),
        (GraphSymbolKind::Property, "category") => Some(GraphSymbol::Property(CATEGORY)),
        _ => None,
    }
}
fn arguments() -> GqlParameters {
    GqlParameters::new()
        .with_text("status", "ready")
        .unwrap()
        .with_bool("active", true)
        .unwrap()
        .with_text("category", WANTED)
        .unwrap()
        .with_uint64("take", 100)
        .unwrap()
}
fn template() -> PreparedGraphText {
    PreparedGraphText::prepare_with_parameter_types(
        &format!("{HEAD} RETURN p,c,c.category AS category LIMIT $take"),
        &TYPES,
        symbols,
    )
    .unwrap()
}
fn aggregate_template() -> PreparedGraphAggregateText {
    PreparedGraphAggregateText::prepare_with_parameter_types(&format!(
        "{HEAD} RETURN p,COUNT(*) AS occurrences,COUNT(c) AS present,COUNT(DISTINCT c) AS companies \
         GROUP BY p HAVING occurrences >= $minimum ORDER BY present DESC,p ASC LIMIT $take"
    ), &TYPES, symbols).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (vid, status, active) in [
        (VId(0), "ready", true),
        (VId(1), "ready", true),
        (VId(2), "other", true),
        (VId(3), "ready", false),
        (HIGH, "ready", true),
    ] {
        batch.create_vertex(
            vid,
            vec![PERSON],
            vec![
                (STATUS, text(status)),
                (ACTIVE, CanonicalScalar::Bool(active)),
            ],
        );
    }
    batch.create_vertex(VId(10), vec![], vec![(CATEGORY, text(WANTED))]);
    batch.create_vertex(VId(11), vec![], vec![(CATEGORY, text("other"))]);
    for (eid, source, destination) in [
        (1, VId(0), VId(10)),
        (2, VId(0), VId(10)),
        (3, VId(2), VId(10)),
        (4, HIGH, VId(11)),
    ] {
        batch.add_edge(EId(eid), source, destination, vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn property(row: &VertexRow, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    row.props
        .iter()
        .find(|(found, _)| *found == key)
        .map(|(_, value)| value)
}
// Independent owned-row oracle: no parser, GLA, scalar-predicate or query call.
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let ready = text("ready");
    let active = CanonicalScalar::Bool(true);
    let wanted = text(WANTED);
    let mut result = Vec::new();
    for person in vertices.iter().filter(|row| {
        row.labels.contains(&PERSON)
            && property(row, STATUS) == Some(&ready)
            && property(row, ACTIVE) == Some(&active)
    }) {
        let mut matched = false;
        for edge in edges
            .iter()
            .filter(|edge| edge.entry.src == person.vid && edge.entry.relation == R)
        {
            let company = vertices
                .iter()
                .find(|row| row.vid == edge.entry.dst)
                .unwrap();
            if property(company, CATEGORY) == Some(&wanted) {
                matched = true;
                result.push((person.vid, Some(company.vid), Some(wanted.clone())));
            }
        }
        if !matched {
            result.push((person.vid, None, None));
        }
    }
    result.sort();
    result
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter()
        .map(|row| {
            let category = row.get(2).unwrap();
            (
                row.get(0).unwrap().as_vertex().unwrap(),
                row.get(1).unwrap().as_vertex(),
                (!category.is_null()).then(|| category.as_scalar().unwrap().clone()),
            )
        })
        .collect()
}
fn expected_summary(rows: &[Plain]) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, Vec<Option<VId>>> = BTreeMap::new();
    for row in rows {
        groups.entry(row.0).or_default().push(row.1);
    }
    let mut result: Vec<_> = groups
        .into_iter()
        .map(|(owner, values)| {
            let present: Vec<_> = values.iter().flatten().copied().collect();
            (
                owner,
                values.len() as u64,
                present.len() as u64,
                present.into_iter().collect::<BTreeSet<_>>().len() as u64,
            )
        })
        .collect();
    result.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    result
}
fn summaries(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_count().unwrap(),
                row.get(2).unwrap().as_count().unwrap(),
            )
        })
        .collect()
}

#[test]
fn typed_arguments_cover_all_read_surfaces_staging_and_retained_generations() {
    let ((), report) = run_async_under_lab(0x5ca1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let prepared = template();
        let args = arguments();
        let pattern = prepared.bind_parameters(&args).unwrap();
        let frozen = pattern.canonical_bytes();
        let summary = aggregate_template()
            .bind_parameters(&args.clone().with_int64("minimum", 1).unwrap())
            .unwrap();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(old.len(), 4);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [
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
        ] {
            assert_eq!(plain(&result.value), old);
        }
        for result in [
            db.execute_graph_aggregate_governed(&cx, &summary, policy())
                .unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &summary, basis, policy())
                .unwrap(),
            view.execute_graph_aggregate_governed(&cx, &summary, policy())
                .unwrap(),
            view.execute_graph_aggregate_governed_at(&cx, &summary, basis, policy())
                .unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &summary, policy())
                .unwrap(),
        ] {
            assert_eq!(summaries(&result.value), expected_summary(&old));
        }
        let null_args = GqlParameters::new()
            .with_text("status", "ready")
            .unwrap()
            .with_bool("active", true)
            .unwrap()
            .with_null("category")
            .unwrap()
            .with_uint64("take", 100)
            .unwrap();
        let null_pattern = prepared.bind_parameters(&null_args).unwrap();
        let null_rows = db
            .execute_graph_pattern_governed(&cx, &null_pattern, policy())
            .unwrap();
        assert_eq!(
            plain(&null_rows.value),
            vec![
                (VId(0), None, None),
                (VId(1), None, None),
                (HIGH, None, None)
            ]
        );
        assert_eq!(pattern.canonical_bytes(), frozen);

        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(1));
        changes.ensure_edge_by_triple(EId(999), VId(0), VId(10), vec![]);
        changes.set_vertex_property(VId(1), STATUS, Some(text("paused")));
        changes.set_vertex_property(VId(10), CATEGORY, None);
        changes.set_vertex_property(VId(11), CATEGORY, Some(text(WANTED)));
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(
            expected,
            vec![
                (VId(0), None, None),
                (HIGH, Some(VId(11)), Some(text(WANTED)))
            ]
        );
        assert_eq!(
            plain(
                &txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            summaries(
                &txn.execute_graph_aggregate_governed(&db, &cx, &summary, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&expected)
        );
        assert_eq!(
            plain(
                &db.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            summaries(
                &reopened
                    .execute_graph_aggregate_governed(&cx, &summary, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&expected)
        );
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            plain(
                &view
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            summaries(
                &view
                    .execute_graph_aggregate_governed(&cx, &summary, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&old)
        );
        assert_eq!(pattern.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bound_scalar_reads_keep_rejected_values_and_absence_in_the_conflict_footprint() {
    let ((), report) = run_async_under_lab(0x5ca1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let pattern = template().bind_parameters(&arguments()).unwrap();
        for refused in [false, true] {
            for change in 0..5 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                // The unlabelled staged vertex cannot affect the answer. Use
                // database reads for the oracle, never broad transaction reads
                // that could supply the very conflict witnesses being tested.
                let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let result = txn.execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &pattern,
                    GqlQueryPolicy::new(100, if refused { 0 } else { 100 }, 1_000_000, 100_000),
                );
                if refused {
                    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
                } else {
                    assert_eq!(plain(&result.unwrap().value), old);
                }
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => winner.create_vertex(VId(88), vec![], vec![]),
                    1 => winner.set_vertex_property(VId(11), CATEGORY, Some(text(WANTED))),
                    2 => winner.add_edge(EId(88), VId(1), VId(10), vec![]),
                    3 => winner.create_vertex(
                        VId(88),
                        vec![PERSON],
                        vec![
                            (STATUS, text("ready")),
                            (ACTIVE, CanonicalScalar::Bool(true)),
                        ],
                    ),
                    _ => winner.set_vertex_property(
                        VId(3),
                        ACTIVE,
                        Some(CanonicalScalar::Bool(true)),
                    ),
                };
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // Commit immediately: another query here could mask loss of
                // dependencies during the earlier output-budget refusal.
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
fn exact_limits_and_runtime_cancellation_preserve_source_and_argument_errors() {
    let ((), report) = run_async_under_lab(0x5ca1_0003, |root| async move {
        // Cancel the query child, keeping the lab supervisor available to join it.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let cx = contexts.query();
                let commit = contexts.commit();
                let txn_cx = contexts.txn();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let prepared = template();
                let pattern = prepared.bind_parameters(&arguments()).unwrap();
                let full = db
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap();
                let exact = GqlQueryPolicy::new(
                    full.rows.snapshot_records,
                    full.rows.result_rows,
                    full.evaluator.work_units,
                    full.evaluator.scratch_entries,
                );
                assert_eq!(
                    db.execute_graph_pattern_governed(&cx, &pattern, exact)
                        .unwrap(),
                    full
                );
                for cap in [
                    GqlQueryPolicy::new(full.rows.snapshot_records - 1, 100, u64::MAX, u64::MAX),
                    GqlQueryPolicy::new(100, full.rows.result_rows - 1, u64::MAX, u64::MAX),
                    GqlQueryPolicy::new(100, 100, full.evaluator.work_units - 1, u64::MAX),
                    GqlQueryPolicy::new(100, 100, u64::MAX, full.evaluator.scratch_entries - 1),
                ] {
                    assert!(
                        db.execute_graph_pattern_governed(&cx, &pattern, cap)
                            .is_err()
                    );
                }
                let wrong = GqlParameters::new()
                    .with_text("status", "ready")
                    .unwrap()
                    .with_text("active", "true")
                    .unwrap()
                    .with_text("category", WANTED)
                    .unwrap()
                    .with_uint64("take", 100)
                    .unwrap();
                assert!(matches!(
                    prepared.bind_parameters(&wrong).unwrap_err().kind,
                    GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
                ));
                let foreign = Database::open_memory(&commit, keys()).await.unwrap();
                let txn = db.begin(&txn_cx).unwrap();
                let view = db.read_session().unwrap();
                let mut later = WriteBatch::new(R);
                later.set_vertex_property(VId(10), CATEGORY, Some(text("no longer selected")));
                db.write(&commit, later).await.unwrap();
                assert_eq!(
                    txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy())
                        .unwrap()
                        .value,
                    full.value
                );
                assert_ne!(
                    db.execute_graph_pattern_governed(&cx, &pattern, policy())
                        .unwrap()
                        .value,
                    full.value
                );
                root.cancel_with(
                    CancelKind::User,
                    Some("scalar argument cancellation ordering"),
                );
                assert!(matches!(
                    txn.execute_graph_pattern_governed(&foreign, &cx, &pattern, policy()),
                    Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
                ));
                let future = CommitSeq(db.frontier().unwrap().0 + 1);
                assert!(matches!(
                    db.execute_graph_pattern_governed_at(&cx, &pattern, future, policy()),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    view.execute_graph_pattern_governed_at(&cx, &pattern, future, policy()),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    db.execute_graph_pattern_governed(&cx, &pattern, policy()),
                    Err(GqlQueryError::Interrupted(_))
                ));
                txn.abort();
            })
            .expect("lab query task can be spawned");
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok(), "the supervisor remains live");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
