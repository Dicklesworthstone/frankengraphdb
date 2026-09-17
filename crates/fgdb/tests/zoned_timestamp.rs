//! Pinned zoned values survive native/GQL writes and both recovery paths.
//! Missing artifacts refuse before publication, including erased batch inputs.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, PinnedTzdb, QueryResult, TzdbTransition, TzdbZone, WriteBatch,
    WriteError, WriteMismatchPolicy,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, CanonicalTimestamp, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, QueryCx, VId,
};
use std::fmt::Debug;
use std::sync::Arc;

const R: RelationId = RelationId(1);
const TIMESTAMP: LabelId = LabelId(1);
const BASELINE: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(7);
const BASELINE_ID: VId = VId(1);
const TARGET_ID: VId = VId(2);
const ZONE: &str = "America/New_York";
const INSTANT: i128 = 1_720_008_000_123_456_789;
const OFFSET: i32 = -14_400;

fn artifact() -> Arc<PinnedTzdb> {
    Arc::new(
        PinnedTzdb::new(vec![TzdbZone {
            identifier: ZONE.into(),
            initial_offset_seconds: -18_000,
            transitions: vec![
                TzdbTransition {
                    instant_utc_seconds: 1_710_054_000,
                    offset_seconds: OFFSET,
                },
                TzdbTransition {
                    instant_utc_seconds: 1_730_613_600,
                    offset_seconds: -18_000,
                },
            ],
        }])
        .unwrap(),
    )
}

fn zoned() -> CanonicalScalar {
    let artifact = artifact();
    CanonicalScalar::Timestamp(
        CanonicalTimestamp::zoned(
            INSTANT,
            OFFSET,
            ZONE,
            artifact.object_id(),
            artifact.as_ref(),
        )
        .unwrap(),
    )
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Timestamp") => Some(GraphSymbol::Label(TIMESTAMP)),
        (GraphSymbolKind::Label, "Baseline") => Some(GraphSymbol::Label(BASELINE)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn expected_rows(value: Option<&CanonicalScalar>) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["p".into()],
        rows: value
            .into_iter()
            .map(|value| {
                vec![GraphAggregateValue::Value(GraphValue::Scalar(
                    value.clone(),
                ))]
            })
            .collect(),
    }
}

fn record<T: Debug, E: Debug>(
    phase: &str,
    result: &Result<T, E>,
    matches: impl FnOnce(&T) -> bool,
    failures: &mut Vec<String>,
) {
    eprintln!("{phase}: {result:?}");
    if !result.as_ref().is_ok_and(matches) {
        failures.push(format!("{phase}: {result:?}"));
    }
}

// Both native reads and the public text facade are attempted even if frontier()
// itself refuses. A poisoned current snapshot must not conceal historical loss.
#[allow(clippy::too_many_arguments)]
fn inspect(
    db: &Database,
    cx: &QueryCx,
    phase: &str,
    target: VId,
    before: CommitSeq,
    expected_frontier: CommitSeq,
    prior: Option<&CanonicalScalar>,
    current: Option<&CanonicalScalar>,
    failures: &mut Vec<String>,
) {
    record(
        &format!("{phase}/frontier"),
        &db.frontier(),
        |seq| *seq == expected_frontier,
        failures,
    );
    for (name, seq, value) in [
        ("current", None, current),
        ("historical", Some(before), prior),
    ] {
        let native = match seq {
            Some(seq) => db.vertex_at(target, seq),
            None => db.vertex(target),
        };
        record(
            &format!("{phase}/{name}/native-target"),
            &native,
            |row| match (row, value) {
                (None, None) => true,
                (Some(row), Some(value)) => {
                    row.vid == target
                        && row.labels == [TIMESTAMP]
                        && row.props == [(P, value.clone())]
                }
                _ => false,
            },
            failures,
        );
        let baseline = match seq {
            Some(seq) => db.vertex_at(BASELINE_ID, seq),
            None => db.vertex(BASELINE_ID),
        };
        record(
            &format!("{phase}/{name}/native-baseline"),
            &baseline,
            |row| {
                row.as_ref().is_some_and(|row| {
                    row.labels == [BASELINE] && row.props == [(P, CanonicalScalar::Int(99))]
                })
            },
            failures,
        );
        let temporal = seq.map_or_else(String::new, |seq| {
            format!(" FOR SYSTEM_TIME AS OF SEQ {}", seq.0)
        });
        for (label, expected) in [
            ("Timestamp", expected_rows(value)),
            ("Baseline", expected_rows(Some(&CanonicalScalar::Int(99)))),
        ] {
            let query = format!("MATCH (n:{label}){temporal} RETURN n.p AS p");
            let result = db.query(cx, &query, &GqlParameters::new(), symbols, policy());
            record(
                &format!("{phase}/{name}/gql-{label}"),
                &result,
                |actual| *actual == expected,
                failures,
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum WritePath {
    Native,
    Insert,
    Set,
}

fn reproduce(path: WritePath, seed: u64) {
    let (failures, report) = run_async_under_lab(seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let artifact = artifact();
        let pinned_keys = keys().with_scalar_resolver(artifact.clone());
        let mut failures = Vec::new();
        for rebuilding in [false, true] {
            let mode = if rebuilding { "rebuilding" } else { "fast" };
            let phase = format!("{path:?}/{mode}");
            let dir = std::env::temp_dir()
                .join(format!("fgdb-zoned-{}-{seed}-{mode}", std::process::id()));
            let mut db = Database::create(&commit, &dir, pinned_keys.clone())
                .await
                .unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(
                BASELINE_ID,
                vec![BASELINE],
                vec![(P, CanonicalScalar::Int(99))],
            );
            let prior = matches!(path, WritePath::Set).then_some(CanonicalScalar::Int(7));
            if let Some(value) = &prior {
                batch.create_vertex(TARGET_ID, vec![TIMESTAMP], vec![(P, value.clone())]);
            }
            let before = db.write(&commit, batch).await.unwrap();
            let mut target = TARGET_ID;
            let expected = zoned();
            match path {
                WritePath::Native => {
                    let mut batch = WriteBatch::new(R);
                    batch.create_vertex(target, vec![TIMESTAMP], vec![(P, expected.clone())]);
                    db.write(&commit, batch)
                        .await
                        .expect("native pinned write commits");
                }
                WritePath::Insert | WritePath::Set => {
                    let mut reserved = if matches!(path, WritePath::Insert) {
                        let id = db
                            .allocate_identity(
                                &query,
                                GraphInsertRequest::Vertex { row: 0, vertex: 0 },
                            )
                            .unwrap();
                        let ElementId::Vertex(vid) = id else {
                            panic!("vertex allocation returned {id:?}")
                        };
                        target = vid;
                        Some(id)
                    } else {
                        None
                    };
                    let text = if matches!(path, WritePath::Insert) {
                        "INSERT (:Timestamp {p: $stamp})"
                    } else {
                        "MATCH (n:Timestamp) SET n.p = $stamp"
                    };
                    let params = GqlParameters::new()
                        .with_scalar("stamp", expected.clone())
                        .unwrap();
                    db.query_write(
                        &txn,
                        &query,
                        &commit,
                        text,
                        &params,
                        symbols,
                        R,
                        GraphWriteProgramPolicy::new(policy(), 100, 100, 100),
                        |_| reserved.take().ok_or("unexpected identity request"),
                    )
                    .await
                    .expect("GQL pinned write commits");
                }
            }
            let at = db.frontier().unwrap();
            assert!(at > before);
            inspect(
                &db,
                &query,
                &format!("{phase}/live"),
                target,
                before,
                at,
                prior.as_ref(),
                Some(&expected),
                &mut failures,
            );
            // Advance with a real update: AS OF must recover the zoned value
            // from history rather than reading the current frontier again.
            let current = CanonicalScalar::Int(42);
            let mut later = WriteBatch::new(R);
            later.set_vertex_property(target, P, Some(current.clone()));
            let after = db.write(&commit, later).await.unwrap();
            assert!(after > at);
            inspect(
                &db,
                &query,
                &format!("{phase}/later"),
                target,
                at,
                after,
                Some(&expected),
                Some(&current),
                &mut failures,
            );
            drop(db);
            // Load independent artifact bytes, not the live writer's resolver.
            let reopened_keys = keys().with_scalar_resolver(Arc::new(
                PinnedTzdb::decode(artifact.canonical_bytes()).unwrap(),
            ));
            let db = if rebuilding {
                Database::open_rebuilding(&commit, &dir, reopened_keys).await
            } else {
                Database::open(&commit, &dir, reopened_keys).await
            }
            .expect("pinned timestamp history reopens");
            inspect(
                &db,
                &query,
                &format!("{phase}/reopened"),
                target,
                at,
                after,
                Some(&expected),
                Some(&current),
                &mut failures,
            );
            inspect(
                &db,
                &query,
                &format!("{phase}/baseline-history"),
                target,
                before,
                after,
                prior.as_ref(),
                Some(&current),
                &mut failures,
            );
        }
        failures
    });
    assert!(
        report.lab_test_passed(),
        "{report:?}; outcomes: {failures:?}"
    );
    assert!(
        failures.is_empty(),
        "zoned timestamp phase failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn native_pinned_timestamp_persists_through_recovery() {
    reproduce(WritePath::Native, 0x209e_0001);
}

#[test]
fn gql_insert_pinned_timestamp_persists_through_recovery() {
    reproduce(WritePath::Insert, 0x209e_0002);
}

#[test]
fn gql_set_pinned_timestamp_persists_through_recovery() {
    reproduce(WritePath::Set, 0x209e_0003);
}

#[test]
fn native_preparation_refuses_every_zoned_scalar_before_noops_and_netfold() {
    type Case = (&'static str, fn(&mut WriteBatch));
    let cases: &[Case] = &[
        ("vertex", |b| {
            b.create_vertex(VId(3), vec![TIMESTAMP], vec![(P, zoned())]);
        }),
        ("edge", |b| {
            b.add_edge(EId(2), BASELINE_ID, TARGET_ID, vec![(P, zoned())]);
        }),
        ("set-vertex", |b| {
            b.set_vertex_property(TARGET_ID, P, Some(zoned()));
        }),
        ("set-edge", |b| {
            b.set_edge_property(EId(1), P, Some(zoned()));
        }),
        ("cas-vertex-expected-noop", |b| {
            b.compare_and_set_vertex_property(
                TARGET_ID,
                P,
                Some(zoned()),
                CanonicalScalar::Int(8),
                WriteMismatchPolicy::NoOp,
            );
        }),
        ("cas-vertex-value-noop", |b| {
            b.compare_and_set_vertex_property(
                TARGET_ID,
                P,
                Some(CanonicalScalar::Int(404)),
                zoned(),
                WriteMismatchPolicy::NoOp,
            );
        }),
        ("cas-edge-expected-noop", |b| {
            b.compare_and_set_edge_property(
                EId(1),
                P,
                Some(zoned()),
                CanonicalScalar::Int(8),
                WriteMismatchPolicy::NoOp,
            );
        }),
        ("cas-edge-value-noop", |b| {
            b.compare_and_set_edge_property(
                EId(1),
                P,
                Some(CanonicalScalar::Int(404)),
                zoned(),
                WriteMismatchPolicy::NoOp,
            );
        }),
        ("ensure-vertex-noop", |b| {
            b.ensure_vertex(TARGET_ID, vec![TIMESTAMP], vec![(P, zoned())]);
        }),
        ("ensure-edge-noop", |b| {
            b.ensure_edge_by_triple(EId(2), BASELINE_ID, TARGET_ID, vec![(P, zoned())]);
        }),
        ("overwritten-vertex-property", |b| {
            b.set_vertex_property(TARGET_ID, P, Some(zoned()));
            b.set_vertex_property(TARGET_ID, P, Some(CanonicalScalar::Int(8)));
        }),
        ("overwritten-edge-property", |b| {
            b.set_edge_property(EId(1), P, Some(zoned()));
            b.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(8)));
        }),
        ("netfold-vertex", |b| {
            b.create_vertex(VId(3), vec![TIMESTAMP], vec![(P, zoned())]);
            b.delete_vertex(VId(3));
        }),
        ("netfold-edge", |b| {
            b.add_edge(EId(2), BASELINE_ID, TARGET_ID, vec![(P, zoned())]);
            b.delete_edge(EId(2));
        }),
    ];
    let (failures, report) = run_async_under_lab(0x209e_0004, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut failures = Vec::new();
        let offset =
            CanonicalScalar::Timestamp(CanonicalTimestamp::offset_only(INSTANT, OFFSET).unwrap());
        for rebuilding in [false, true] {
            let mode = if rebuilding { "rebuilding" } else { "fast" };
            let dir = std::env::temp_dir()
                .join(format!("fgdb-zoned-prepare-{}-{mode}", std::process::id()));
            let mut db = Database::create(&commit, &dir, keys()).await.unwrap();
            let mut baseline = WriteBatch::new(R);
            baseline.create_vertex(
                BASELINE_ID,
                vec![BASELINE],
                vec![(P, CanonicalScalar::Int(99))],
            );
            baseline.create_vertex(TARGET_ID, vec![TIMESTAMP], vec![(P, offset.clone())]);
            baseline.add_edge(EId(1), BASELINE_ID, TARGET_ID, vec![(P, offset.clone())]);
            // The same instant and offset without a zone is supported and commits.
            let before = db.write(&commit, baseline).await.unwrap();
            let original_vertices = db.vertices().unwrap();
            let original_edges = db.edges().unwrap();
            assert_eq!(
                db.vertex(TARGET_ID).unwrap().unwrap().props,
                [(P, offset.clone())]
            );
            assert_eq!(
                db.edge(EId(1)).unwrap().unwrap().props,
                [(P, offset.clone())]
            );
            for &(name, build) in cases {
                let phase = format!("prepare/{mode}/{name}");
                let mut batch = WriteBatch::new(R);
                build(&mut batch);
                let result = db.prepare_write(batch);
                eprintln!("{phase}: {result:?}");
                if !matches!(result, Err(WriteError::ZonedTimestampRequiresResolver { tzdb_oid }) if tzdb_oid == artifact().object_id())
                {
                    failures.push(format!("{phase}: expected exact typed resolver refusal"));
                }
                inspect(
                    &db,
                    &query,
                    &phase,
                    TARGET_ID,
                    before,
                    before,
                    Some(&offset),
                    Some(&offset),
                    &mut failures,
                );
                record(
                    &format!("{phase}/vertices"),
                    &db.vertices(),
                    |rows| *rows == original_vertices,
                    &mut failures,
                );
                record(
                    &format!("{phase}/edges"),
                    &db.edges(),
                    |rows| *rows == original_edges,
                    &mut failures,
                );
                record(
                    &format!("{phase}/historical-edges"),
                    &db.edges_at(before),
                    |rows| *rows == original_edges,
                    &mut failures,
                );
            }
            drop(db);
            let reopened = if rebuilding {
                Database::open_rebuilding(&commit, &dir, keys()).await
            } else {
                Database::open(&commit, &dir, keys()).await
            };
            match reopened {
                Ok(db) => {
                    let phase = format!("prepare/{mode}/reopened");
                    inspect(
                        &db,
                        &query,
                        &phase,
                        TARGET_ID,
                        before,
                        before,
                        Some(&offset),
                        Some(&offset),
                        &mut failures,
                    );
                    record(
                        &format!("{phase}/vertices"),
                        &db.vertices(),
                        |rows| *rows == original_vertices,
                        &mut failures,
                    );
                    record(
                        &format!("{phase}/edges"),
                        &db.edges(),
                        |rows| *rows == original_edges,
                        &mut failures,
                    );
                    record(
                        &format!("{phase}/historical-edges"),
                        &db.edges_at(before),
                        |rows| *rows == original_edges,
                        &mut failures,
                    );
                }
                Err(error) => failures.push(format!("prepare/{mode}/open: {error:?}")),
            }
        }
        failures
    });
    assert!(
        report.lab_test_passed(),
        "{report:?}; outcomes: {failures:?}"
    );
    assert!(
        failures.is_empty(),
        "zoned preparation failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn native_preparation_rejects_unknown_oid_with_other_valid_resolver() {
    let (_, report) = run_async_under_lab(0x209e_0005, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let available = artifact();
        let other = PinnedTzdb::new(vec![TzdbZone {
            identifier: ZONE.into(),
            initial_offset_seconds: OFFSET,
            transitions: vec![],
        }])
        .unwrap();
        assert_ne!(available.object_id(), other.object_id());
        let unsupported = CanonicalScalar::Timestamp(
            CanonicalTimestamp::zoned(INSTANT, OFFSET, ZONE, other.object_id(), &other).unwrap(),
        );
        let pinned_keys = keys().with_scalar_resolver(available);
        for rebuilding in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "fgdb-zoned-unknown-{}-{rebuilding}",
                std::process::id(),
            ));
            let mut db = Database::create(&commit, &dir, pinned_keys.clone())
                .await
                .unwrap();
            let mut baseline = WriteBatch::new(R);
            baseline.create_vertex(
                BASELINE_ID,
                vec![BASELINE],
                vec![(P, CanonicalScalar::Int(99))],
            );
            baseline.create_vertex(
                TARGET_ID,
                vec![TIMESTAMP],
                vec![(P, CanonicalScalar::Int(7))],
            );
            let before = db.write(&commit, baseline).await.unwrap();
            let mut batch = WriteBatch::new(R);
            batch.set_vertex_property(TARGET_ID, P, Some(unsupported.clone()));
            assert!(
                matches!(
                    db.prepare_write(batch),
                    Err(WriteError::ZonedTimestampRequiresResolver { tzdb_oid })
                        if tzdb_oid == other.object_id()
                ),
                "unknown artifact must fail preparation"
            );
            let mut batch = WriteBatch::new(R);
            batch.set_vertex_property(TARGET_ID, P, Some(unsupported.clone()));
            assert!(
                matches!(
                    db.write(&commit, batch).await,
                    Err(WriteError::ZonedTimestampRequiresResolver { tzdb_oid })
                        if tzdb_oid == other.object_id()
                ),
                "unknown artifact must not publish"
            );
            let mut failures = Vec::new();
            inspect(
                &db,
                &query,
                "unknown/live",
                TARGET_ID,
                before,
                before,
                Some(&CanonicalScalar::Int(7)),
                Some(&CanonicalScalar::Int(7)),
                &mut failures,
            );
            drop(db);
            let db = if rebuilding {
                Database::open_rebuilding(&commit, &dir, pinned_keys.clone()).await
            } else {
                Database::open(&commit, &dir, pinned_keys.clone()).await
            }
            .expect("rejected artifact cannot poison recovery");
            inspect(
                &db,
                &query,
                "unknown/reopened",
                TARGET_ID,
                before,
                before,
                Some(&CanonicalScalar::Int(7)),
                Some(&CanonicalScalar::Int(7)),
                &mut failures,
            );
            assert!(failures.is_empty(), "{failures:?}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
