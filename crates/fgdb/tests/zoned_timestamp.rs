//! Without an engine resolver, zoned writes must refuse before publication.
//! Phase diagnostics preserve baseline observations across both recovery paths.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, QueryResult, QueryWriteError, WriteBatch, WriteError,
    WriteMismatchPolicy, WriteTxnError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphMutationProgramError, GraphSymbol,
    GraphSymbolKind, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalScalar, CanonicalTimestamp, CommitSeq, DatabaseSecurityNamespaceId, EId, ObjectId,
    PurposeContexts, QueryCx, TzdbResolver, VId,
};
use std::fmt::Debug;

const R: RelationId = RelationId(1);
const TIMESTAMP: LabelId = LabelId(1);
const BASELINE: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(7);
const BASELINE_ID: VId = VId(1);
const TARGET_ID: VId = VId(2);
const TZDB_OID: ObjectId = ObjectId([0x9d; 32]);
const ZONE: &str = "America/New_York";
const INSTANT: i128 = 1_720_008_000_123_456_789;
const OFFSET: i32 = -14_400;

struct FixtureTzdb;

impl TzdbResolver for FixtureTzdb {
    fn contains_tzdb(&self, oid: &ObjectId) -> bool {
        *oid == TZDB_OID
    }

    fn canonical_utc_offset_seconds(
        &self,
        oid: &ObjectId,
        zone: &str,
        instant: i128,
    ) -> Option<i32> {
        (*oid == TZDB_OID && zone == ZONE && instant == INSTANT).then_some(OFFSET)
    }
}

fn zoned() -> CanonicalScalar {
    CanonicalScalar::Timestamp(
        CanonicalTimestamp::zoned(INSTANT, OFFSET, ZONE, TZDB_OID, &FixtureTzdb).unwrap(),
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
        let mut failures = Vec::new();
        // Separate durable fixtures: fast open cannot repair or rewrite the
        // files whose independent rebuilding outcome we need to observe.
        for rebuilding in [false, true] {
            let mode = if rebuilding { "rebuilding" } else { "fast" };
            let phase = format!("{path:?}/{mode}");
            let dir = std::env::temp_dir()
                .join(format!("fgdb-zoned-{}-{seed}-{mode}", std::process::id()));
            eprintln!("{phase}/database: {}", dir.display());
            let mut db = Database::create(&commit, &dir, keys()).await.unwrap();
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
            inspect(
                &db,
                &query,
                &format!("{phase}/before"),
                target,
                before,
                before,
                prior.as_ref(),
                prior.as_ref(),
                &mut failures,
            );
            let expected = zoned();
            let refused = match path {
                WritePath::Native => {
                    let mut batch = WriteBatch::new(R);
                    batch.create_vertex(target, vec![TIMESTAMP], vec![(P, expected.clone())]);
                    let result = db.prepare_write(batch);
                    eprintln!("{phase}/prepare: {result:?}");
                    matches!(result, Err(WriteError::ZonedTimestampRequiresResolver { tzdb_oid }) if tzdb_oid == TZDB_OID)
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
                    let result = db
                        .query_write(
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
                        .await;
                    eprintln!("{phase}/write: {result:?}");
                    matches!(result,
                        Err(QueryWriteError::Execute(GraphWriteScriptExecutionError::Program(
                            GraphWriteProgramError::Insert { statement: 0, source:
                                fgdb_gql::GqlQueryError::Source(fgdb_gql::insertion::GraphInsertError::Source(
                                    WriteTxnError::Write(WriteError::ZonedTimestampRequiresResolver { tzdb_oid })
                                ))
                            }
                        ))) if tzdb_oid == TZDB_OID)
                        || matches!(result,
                        Err(QueryWriteError::Execute(GraphWriteScriptExecutionError::Program(
                            GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                                statement: 0, source:
                                fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphMutationError::Source(
                                    WriteTxnError::Write(WriteError::ZonedTimestampRequiresResolver { tzdb_oid })
                                ))
                            })
                        ))) if tzdb_oid == TZDB_OID)
                }
            };
            eprintln!(
                "{phase}/typed-refusal: {refused}; before: {before:?}; after: {:?}",
                db.frontier()
            );
            let expected_frontier = before;
            let current = prior.as_ref();
            inspect(
                &db,
                &query,
                &format!("{phase}/live"),
                target,
                before,
                expected_frontier,
                prior.as_ref(),
                current,
                &mut failures,
            );
            drop(db);
            let reopened = if rebuilding {
                Database::open_rebuilding(&commit, &dir, keys()).await
            } else {
                Database::open(&commit, &dir, keys()).await
            };
            match reopened {
                Ok(db) => {
                    eprintln!("{phase}/open: Ok");
                    inspect(
                        &db,
                        &query,
                        &format!("{phase}/reopened"),
                        target,
                        before,
                        expected_frontier,
                        prior.as_ref(),
                        current,
                        &mut failures,
                    );
                }
                Err(error) => {
                    eprintln!("{phase}/open: Err({error:?})");
                    failures.push(format!("{phase}/open: {error:?}"));
                }
            }
            if !refused {
                failures.push(format!(
                    "{phase}: expected exact ZonedTimestampRequiresResolver with fixture tzdb OID"
                ));
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
        "zoned timestamp phase failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn native_zoned_timestamp_refusal_preserves_baseline() {
    reproduce(WritePath::Native, 0x209e_0001);
}

#[test]
fn gql_insert_zoned_timestamp_refusal_preserves_baseline() {
    reproduce(WritePath::Insert, 0x209e_0002);
}

#[test]
fn gql_set_zoned_timestamp_refusal_preserves_baseline() {
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
                if !matches!(result, Err(WriteError::ZonedTimestampRequiresResolver { tzdb_oid }) if tzdb_oid == TZDB_OID)
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
