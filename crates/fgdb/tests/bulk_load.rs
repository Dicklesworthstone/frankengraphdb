//! Bulk ingestion is compared with native CREATE programs, never a WriteBatch oracle.
//! Crash assertions pin individual capsule boundaries, not just the eventual graph.

use asupersync::lab::run_async_under_lab;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    BulkEdge, BulkEndpoint, BulkLoadCheckpoint, BulkLoadErrorKind, BulkLoadPolicy, BulkRow,
    BulkVertex, CrashPoint, Database, DatabaseKeys,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphInsertText, PreparedGraphText, PreparedGraphWriteProgram,
};
use fgdb_strata::edge_props::EdgePropertyPatchError;
use fgdb_strata::vertex::VertexPatchError;
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const OTHER: LabelId = LabelId(2);
const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const GENERATED_VERTICES: usize = 16;
const GENERATED_EDGES: usize = 10_000;
const CHUNK: usize = 128;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb7; 32],
        DatabaseSecurityNamespaceId([0xb8; 32]),
        [0xb9; 32],
    )
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fgdb-bulk-{}-{name}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn under_lab<Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static)
where
    Fut: Future<Output = ()> + Send + 'static,
{
    let ((), report) = run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root)).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Other") => Some(GraphSymbol::Label(OTHER)),
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(K)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 100_000_000, 100_000_000)
}

fn int_property(props: &[(PropertyKeyId, CanonicalPropertyValue)], key: PropertyKeyId) -> i64 {
    match props
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, value)| value)
    {
        Some(CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value))) => *value,
        other => panic!("expected integer property {key:?}, got {other:?}"),
    }
}

fn vertex(n: usize, payload: i64) -> BulkRow {
    BulkRow::Vertex(BulkVertex {
        key: format!("v{n}"),
        labels: vec![if n.is_multiple_of(2) { PERSON } else { OTHER }],
        props: vec![
            (K, CanonicalScalar::Int(n as i64).into()),
            (P, CanonicalScalar::Int(payload).into()),
        ],
    })
}

fn edge(
    n: usize,
    source: usize,
    destination: usize,
    relation: RelationId,
    payload: i64,
) -> BulkRow {
    BulkRow::Edge(BulkEdge {
        key: format!("e{n}"),
        source: format!("v{source}").into(),
        destination: format!("v{destination}").into(),
        relation,
        props: vec![
            (K, CanonicalScalar::Int(n as i64).into()),
            (P, CanonicalScalar::Int(payload).into()),
        ],
    })
}

fn generated(seed: u64) -> Vec<BulkRow> {
    let mut state = seed;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 17
    };
    let mut rows = Vec::with_capacity(GENERATED_VERTICES + GENERATED_EDGES);
    for n in 0..GENERATED_VERTICES {
        rows.push(vertex(n, (next() % 997) as i64));
    }
    for n in 0..GENERATED_EDGES {
        let source = (next() % GENERATED_VERTICES as u64) as usize;
        let destination = (next() % GENERATED_VERTICES as u64) as usize;
        let relation = if n % 2 == 0 { R } else { S };
        rows.push(edge(
            n,
            source,
            destination,
            relation,
            (next() % 997) as i64,
        ));
    }
    rows
}

fn crash_rows() -> Vec<BulkRow> {
    let mut rows: Vec<_> = (0..4).map(|n| vertex(n, n as i64 * 7)).collect();
    for n in 0..8 {
        rows.push(edge(
            n,
            n % 4,
            (n + 1) % 4,
            if n % 2 == 0 { R } else { S },
            n as i64,
        ));
    }
    rows
}

type LogicalVertices =
    BTreeMap<String, (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalPropertyValue)>)>;
type LogicalEdges = BTreeMap<
    String,
    (
        String,
        String,
        RelationId,
        Vec<(PropertyKeyId, CanonicalPropertyValue)>,
    ),
>;

fn logical_state(
    db: &Database,
    checkpoint: &BulkLoadCheckpoint,
) -> (LogicalVertices, LogicalEdges) {
    let reverse_vertices: BTreeMap<VId, String> = checkpoint
        .vertices
        .iter()
        .map(|(key, id)| (*id, key.clone()))
        .collect();
    let reverse_edges: BTreeMap<EId, String> = checkpoint
        .edges
        .iter()
        .map(|(key, id)| (*id, key.clone()))
        .collect();
    assert_eq!(
        reverse_vertices.len(),
        checkpoint.vertices.len(),
        "distinct keys need distinct VIds"
    );
    assert_eq!(
        reverse_edges.len(),
        checkpoint.edges.len(),
        "distinct keys need distinct EIds"
    );
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    assert_eq!(vertices.len(), reverse_vertices.len());
    assert_eq!(edges.len(), reverse_edges.len());
    (
        vertices
            .into_iter()
            .map(|row| (reverse_vertices[&row.vid].clone(), (row.labels, row.props)))
            .collect(),
        edges
            .into_iter()
            .map(|row| {
                (
                    reverse_edges[&row.entry.eid].clone(),
                    (
                        reverse_vertices[&row.entry.src].clone(),
                        reverse_vertices[&row.entry.dst].clone(),
                        row.entry.relation,
                        row.props,
                    ),
                )
            })
            .collect(),
    )
}

fn expected_state(rows: &[BulkRow]) -> (LogicalVertices, LogicalEdges) {
    let mut vertices = BTreeMap::new();
    let mut edges = BTreeMap::new();
    for row in rows {
        match row {
            BulkRow::Vertex(row) => {
                assert!(
                    vertices
                        .insert(row.key.clone(), (row.labels.clone(), row.props.clone()))
                        .is_none()
                );
            }
            BulkRow::Edge(row) => {
                assert!(
                    edges
                        .insert(
                            row.key.clone(),
                            (
                                row.source.key().unwrap().to_owned(),
                                row.destination.key().unwrap().to_owned(),
                                row.relation,
                                row.props.clone(),
                            )
                        )
                        .is_none()
                );
            }
        }
    }
    (vertices, edges)
}

fn native_key_mapping(db: &Database) -> BulkLoadCheckpoint {
    let mut mapping = BulkLoadCheckpoint::default();
    for row in db.vertices().unwrap() {
        assert!(
            mapping
                .vertices
                .insert(format!("v{}", int_property(&row.props, K)), row.vid)
                .is_none()
        );
    }
    for row in db.edges().unwrap() {
        assert!(
            mapping
                .edges
                .insert(format!("e{}", int_property(&row.props, K)), row.entry.eid)
                .is_none()
        );
    }
    mapping
}

async fn native_create(db: &mut Database, contexts: &PurposeContexts, rows: &[BulkRow]) {
    // One native CREATE statement per source row. Group same-relation statements
    // into a transaction to avoid 10k fsync pairs without replacing the native
    // compiler, governed program evaluator, engine allocator or commit path.
    for relation in [R, S] {
        let texts: Vec<String> = rows
            .iter()
            .filter_map(|row| match row {
                BulkRow::Vertex(row) if relation == R => Some(format!(
                    "CREATE (n:{} {{k:{},p:{}}})",
                    if row.labels == vec![PERSON] {
                        "Person"
                    } else {
                        "Other"
                    },
                    int_property(&row.props, K),
                    int_property(&row.props, P),
                )),
                BulkRow::Edge(row) if row.relation == relation => Some(format!(
                    "MATCH (a),(b) WHERE a.k={} AND b.k={} CREATE (a)-[:{} {{k:{},p:{}}}]->(b)",
                    row.source.key().unwrap().strip_prefix('v').unwrap(),
                    row.destination.key().unwrap().strip_prefix('v').unwrap(),
                    if relation == R { "R" } else { "S" },
                    int_property(&row.props, K),
                    int_property(&row.props, P),
                )),
                _ => None,
            })
            .collect();
        for chunk in texts.chunks(fgdb_gql::MAX_GRAPH_MUTATION_STATEMENTS) {
            let statements = chunk
                .iter()
                .map(|text| {
                    PreparedGraphInsertText::prepare(text, relation, symbols)
                        .unwrap()
                        .bind_parameters(&GqlParameters::new())
                        .unwrap()
                        .into()
                })
                .collect();
            let program = PreparedGraphWriteProgram::prepare(statements).unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let stats = txn
                .execute_graph_write_program_engine_governed(
                    db,
                    &contexts.query(),
                    &program,
                    GraphWriteProgramPolicy::new(
                        query_policy(),
                        CHUNK as u64,
                        CHUNK as u64,
                        CHUNK as u64,
                    ),
                )
                .unwrap();
            assert_eq!(stats.completed_statements, chunk.len());
            assert_eq!(
                (stats.created_vertices + stats.created_edges) as usize,
                chunk.len()
            );
            txn.commit(db, &contexts.commit()).await.unwrap();
        }
    }
}

fn query_answers(db: &Database, query_cx: &QueryCx, relation: RelationId) -> Vec<Vec<i64>> {
    // The accepted text surface projects vertex variables with UNIQUE column
    // names (the corpus form is "RETURN ALL a AS owner, c.amount AS amount"):
    // edge-variable projections like "e.k" are refused by the parser, and two
    // bare property projections of the same key collide on column name "k".
    // Edge payloads are verified exhaustively through logical_state instead;
    // this differential pins the same GQL pattern answers over both graphs.
    let text = format!(
        "MATCH (a)-[:{}]->(b) RETURN ALL a.k AS ak, b.k AS bk",
        if relation == R { "R" } else { "S" }
    );
    let query = PreparedGraphText::prepare(&text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let mut answer: Vec<Vec<i64>> = db
        .execute_graph_pattern_governed(query_cx, &query, query_policy())
        .unwrap()
        .value
        .iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|value| match value {
                    GraphValue::Scalar(CanonicalScalar::Int(value)) => *value,
                    other => panic!("expected integer result, got {other:?}"),
                })
                .collect()
        })
        .collect();
    answer.sort();
    answer
}

#[test]
fn three_seed_ten_thousand_edge_bulk_matches_native_create_and_gql() {
    // The native evaluator intentionally does 30,000 real CREATE statements.
    // Production-runtime authority avoids exhausting the small lab poll budget;
    // both databases remain disk-backed and use ordinary durable commits.
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        for seed in [1_u64, 0xfeed_beef, 0x1234_5678_9abc_def0] {
            let rows = generated(seed);
            let bulk_path = scratch("equivalence-bulk");
            let native_path = scratch("equivalence-native");
            let mut bulk = Database::create(&contexts.commit(), &bulk_path, keys())
                .await
                .unwrap();
            let mut native = Database::create(&contexts.commit(), &native_path, keys())
                .await
                .unwrap();
            let basis = bulk.frontier().unwrap();
            let checkpoint = bulk
                .bulk_load(
                    &contexts.query(),
                    &contexts.commit(),
                    rows.clone(),
                    BulkLoadPolicy::new(CHUNK, R),
                )
                .await
                .unwrap();
            assert_eq!(checkpoint.vertices.len(), GENERATED_VERTICES);
            assert_eq!(checkpoint.edges.len(), GENERATED_EDGES);
            assert_eq!(checkpoint.next_row, rows.len());
            assert_eq!(checkpoint.committed_chunks, rows.len().div_ceil(CHUNK));
            assert_eq!(
                checkpoint.frontier,
                CommitSeq(basis.0 + rows.len().div_ceil(CHUNK) as u64)
            );
            assert_eq!(
                bulk.delta_since(basis).unwrap().count(),
                checkpoint.committed_chunks
            );
            native_create(&mut native, &contexts, &rows).await;
            // Native relation grouping intentionally allocates EIds in another
            // order. Only caller keys, never coincident numeric identities, join.
            let native_mapping = native_key_mapping(&native);
            assert_ne!(checkpoint.edges, native_mapping.edges);
            let expected = expected_state(&rows);
            assert_eq!(
                logical_state(&bulk, &checkpoint),
                expected,
                "bulk seed={seed}"
            );
            assert_eq!(
                logical_state(&native, &native_mapping),
                expected,
                "native seed={seed}"
            );
            for relation in [R, S] {
                let bulk_answer = query_answers(&bulk, &contexts.query(), relation);
                let native_answer = query_answers(&native, &contexts.query(), relation);
                // Matches the fixture projection exactly: RETURN ALL a.k AS
                // ak, b.k AS bk yields one [source_k, destination_k] row.
                let mut expected_answer: Vec<Vec<i64>> = rows
                    .iter()
                    .filter_map(|row| match row {
                        BulkRow::Edge(row) if row.relation == relation => Some(vec![
                            row.source.key().unwrap()[1..].parse().unwrap(),
                            row.destination.key().unwrap()[1..].parse().unwrap(),
                        ]),
                        _ => None,
                    })
                    .collect();
                expected_answer.sort();
                assert_eq!(bulk_answer.len(), 5_000);
                assert_eq!(
                    bulk_answer, expected_answer,
                    "bulk GQL seed={seed} relation={relation:?}"
                );
                assert_eq!(
                    native_answer, expected_answer,
                    "native GQL seed={seed} relation={relation:?}"
                );
            }
        }
    });
}

#[test]
fn chunk_one_crash_matrix_recovers_exact_whole_chunks_and_resumes() {
    // Surviving unflushed markers are a byte-survival case, not a power-loss
    // guarantee; the paired torn-tail case removes their trailer explicitly.
    for (case, point, tear, durable_chunks) in [
        ("before-capsule", CrashPoint::BeforeCapsule, false, 1_usize),
        ("before-d1", CrashPoint::AfterCapsuleBeforeD1, false, 1),
        ("after-d1", CrashPoint::AfterD1, false, 1),
        ("marker-survives", CrashPoint::AfterMarkerBeforeD2, false, 2),
        ("marker-torn", CrashPoint::AfterMarkerBeforeD2, true, 1),
        (
            "marker-synced",
            CrashPoint::AfterMarkerFileSyncBeforeDirectorySync,
            false,
            2,
        ),
    ] {
        under_lab(
            0xb71c_0001 + durable_chunks as u64,
            move |contexts| async move {
                let path = scratch(case);
                let mut db = Database::create(&contexts.commit(), &path, keys())
                    .await
                    .unwrap();
                let basis = db.frontier().unwrap();
                let rows = crash_rows();
                let error = db
                    .bulk_load_with_crash(
                        &contexts.query(),
                        &contexts.commit(),
                        rows.clone(),
                        BulkLoadPolicy::new(4, R),
                        Some((1, point)),
                    )
                    .await
                    .expect_err("the seam inside the second chunk must be reached");
                assert!(
                    matches!(&error.kind, BulkLoadErrorKind::Write(_)),
                    "{error:?}"
                );
                assert_eq!(error.committed.next_row, 4, "{case}");
                assert_eq!(error.committed.committed_chunks, 1, "{case}");
                assert_eq!(error.committed.frontier, CommitSeq(basis.0 + 1), "{case}");
                assert_eq!(error.committed.vertices.len(), 4);
                assert_eq!(error.committed.edges.len(), 0);
                let pending = error
                    .pending
                    .as_ref()
                    .expect("candidate mappings must survive an interrupted commit");
                assert_eq!(pending.next_row, 8, "{case}");
                assert_eq!(pending.committed_chunks, 2, "{case}");
                assert_eq!(pending.frontier, CommitSeq(basis.0 + 2), "{case}");
                assert_eq!(pending.vertices.len(), 4);
                assert_eq!(pending.edges.len(), 4);
                drop(db);
                if tear {
                    fgdb_chronicle::CommitCoordinator::<asupersync::fs::UnixVfs>::tear_log_tail_for_test(&path, 1).unwrap();
                }
                let mut db = Database::open(&contexts.commit(), &path, keys())
                    .await
                    .unwrap();
                db.ensure_delta_window(&contexts.commit(), basis)
                    .await
                    .unwrap();
                let recovered = db.frontier().unwrap();
                assert_eq!(
                    recovered,
                    CommitSeq(basis.0 + durable_chunks as u64),
                    "{case}"
                );
                assert_eq!(db.vertices().unwrap().len(), 4, "{case}");
                assert_eq!(
                    db.edges().unwrap().len(),
                    (durable_chunks - 1) * 4,
                    "{case}"
                );
                assert_eq!(
                    db.delta_since(basis).unwrap().count(),
                    durable_chunks,
                    "{case}"
                );
                let resume = if pending.frontier == recovered {
                    pending.clone()
                } else {
                    error.committed.clone()
                };
                assert_eq!(resume.next_row, durable_chunks * 4);
                assert_eq!(
                    logical_state(&db, &resume),
                    expected_state(&rows[..durable_chunks * 4])
                );
                let mut policy = BulkLoadPolicy::new(4, R);
                policy.resume = Some(resume);
                let finished = db
                    .bulk_load(&contexts.query(), &contexts.commit(), rows.clone(), policy)
                    .await
                    .unwrap();
                assert_eq!(finished.next_row, 12);
                assert_eq!(finished.committed_chunks, 3);
                assert_eq!(finished.frontier, CommitSeq(basis.0 + 3));
                assert_eq!(db.delta_since(basis).unwrap().count(), 3);
                assert_eq!(logical_state(&db, &finished), expected_state(&rows));
                drop(db);
                let rebuilt = Database::open_rebuilding(&contexts.commit(), &path, keys())
                    .await
                    .unwrap();
                assert_eq!(rebuilt.frontier().unwrap(), finished.frontier);
                assert_eq!(logical_state(&rebuilt, &finished), expected_state(&rows));
            },
        );
    }
}

async fn refusal_preserves_graph(
    contexts: PurposeContexts,
    rows: Vec<BulkRow>,
    check: impl FnOnce(&BulkLoadErrorKind),
) {
    let path = scratch("refusal");
    let mut db = Database::create(&contexts.commit(), &path, keys())
        .await
        .unwrap();
    // Preserve an already nonempty graph as well as its commit frontier.
    let seed = vec![vertex(100, 31), vertex(101, 32), edge(100, 100, 101, S, 33)];
    db.bulk_load(
        &contexts.query(),
        &contexts.commit(),
        seed,
        BulkLoadPolicy::new(4, R),
    )
    .await
    .unwrap();
    let before = db.frontier().unwrap();
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let error = db
        .bulk_load(
            &contexts.query(),
            &contexts.commit(),
            rows,
            BulkLoadPolicy::new(4, R),
        )
        .await
        .unwrap_err();
    check(&error.kind);
    assert_eq!(error.committed.next_row, 0);
    assert_eq!(error.committed.committed_chunks, 0);
    assert_eq!(error.committed.frontier, before);
    assert!(error.committed.vertices.is_empty());
    assert!(error.committed.edges.is_empty());
    assert!(error.pending.is_none());
    assert_eq!(db.frontier().unwrap(), before);
    assert_eq!(db.vertices().unwrap(), vertices);
    assert_eq!(db.edges().unwrap(), edges);
    assert_eq!(db.delta_since(before).unwrap().count(), 0);
    drop(db);
    let reopened = Database::open(&contexts.commit(), &path, keys())
        .await
        .unwrap();
    assert_eq!(reopened.frontier().unwrap(), before);
    assert_eq!(reopened.vertices().unwrap(), vertices);
    assert_eq!(reopened.edges().unwrap(), edges);
}

#[test]
fn dangling_endpoint_beyond_first_chunk_refuses_before_any_commit() {
    under_lab(0xb71c_0002, |contexts| async move {
        let mut rows = crash_rows();
        rows.push(edge(8, 0, 999, R, 4));
        refusal_preserves_graph(contexts, rows, |kind| assert!(matches!(kind,
            BulkLoadErrorKind::DanglingEndpointKey { edge, endpoint } if edge == "e8" && endpoint == "v999"
        ))).await;
    });
}

#[test]
fn endpoint_declared_later_is_not_a_preceding_vertex() {
    under_lab(0xb71c_0003, |contexts| async move {
        let mut rows = crash_rows();
        rows.push(edge(8, 999, 0, R, 4));
        rows.push(vertex(999, 4));
        refusal_preserves_graph(contexts, rows, |kind| assert!(matches!(kind,
            BulkLoadErrorKind::DanglingEndpointKey { edge, endpoint } if edge == "e8" && endpoint == "v999"
        ))).await;
    });
}

#[test]
fn duplicate_vertex_key_beyond_first_chunk_refuses_before_any_commit() {
    under_lab(0xb71c_0004, |contexts| async move {
        let mut rows = crash_rows();
        rows.push(vertex(0, 999));
        refusal_preserves_graph(contexts, rows, |kind| {
            assert!(matches!(kind,
                BulkLoadErrorKind::DuplicateCallerKey { key } if key == "v0"
            ))
        })
        .await;
    });
}

#[test]
fn duplicate_key_namespace_is_global_across_vertices_and_edges() {
    under_lab(0xb71c_0005, |contexts| async move {
        let mut rows = crash_rows();
        let mut duplicate = match edge(8, 0, 1, S, 4) {
            BulkRow::Edge(row) => row,
            _ => unreachable!(),
        };
        duplicate.key = "v0".to_owned();
        rows.push(BulkRow::Edge(duplicate));
        refusal_preserves_graph(contexts, rows, |kind| {
            assert!(matches!(kind,
                BulkLoadErrorKind::DuplicateCallerKey { key } if key == "v0"
            ))
        })
        .await;
    });
}

fn oversized_props() -> Vec<(PropertyKeyId, CanonicalPropertyValue)> {
    // Each scalar is valid; their combined indivisible storage row is not.
    vec![
        (K, CanonicalScalar::bytes(vec![0x41; 8000]).unwrap().into()),
        (P, CanonicalScalar::bytes(vec![0x42; 9000]).unwrap().into()),
    ]
}

#[test]
fn oversized_vertex_row_beyond_first_chunk_is_typed_and_atomic() {
    under_lab(0xb71c_0006, |contexts| async move {
        let mut rows = crash_rows();
        rows.push(BulkRow::Vertex(BulkVertex {
            key: "huge".to_owned(),
            labels: vec![PERSON],
            props: oversized_props(),
        }));
        refusal_preserves_graph(contexts, rows, |kind| assert!(matches!(kind,
            BulkLoadErrorKind::InvalidVertex { key, source: VertexPatchError::RowExceedsStorageLimit { bytes, limit } }
                if key == "huge" && bytes > limit
        ))).await;
    });
}

#[test]
fn oversized_edge_row_beyond_first_chunk_is_typed_and_atomic() {
    under_lab(0xb71c_0007, |contexts| async move {
        let mut rows = crash_rows();
        rows.push(BulkRow::Edge(BulkEdge {
            key: "huge".to_owned(),
            source: "v0".into(),
            destination: "v1".into(),
            relation: S,
            props: oversized_props(),
        }));
        refusal_preserves_graph(contexts, rows, |kind| assert!(matches!(kind,
            BulkLoadErrorKind::InvalidEdge { key, source: EdgePropertyPatchError::RowExceedsStorageLimit { bytes, limit } }
                if key == "huge" && bytes > limit
        ))).await;
    });
}

#[test]
fn vertex_only_chunks_empty_input_and_completed_resume_do_not_invent_commits() {
    under_lab(0xb71c_0008, |contexts| async move {
        let path = scratch("vertex-only");
        let mut db = Database::create(&contexts.commit(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let empty = db
            .bulk_load(
                &contexts.query(),
                &contexts.commit(),
                Vec::<BulkRow>::new(),
                BulkLoadPolicy::new(2, S),
            )
            .await
            .unwrap();
        assert_eq!((empty.next_row, empty.committed_chunks), (0, 0));
        assert_eq!(empty.frontier, basis);
        assert!(empty.vertices.is_empty() && empty.edges.is_empty());
        assert!(db.vertices().unwrap().is_empty() && db.edges().unwrap().is_empty());
        let rows: Vec<_> = (0..5).map(|n| vertex(n, n as i64)).collect();
        let checkpoint = db
            .bulk_load(
                &contexts.query(),
                &contexts.commit(),
                rows.clone(),
                BulkLoadPolicy::new(2, S),
            )
            .await
            .unwrap();
        assert_eq!((checkpoint.next_row, checkpoint.committed_chunks), (5, 3));
        assert_eq!(checkpoint.frontier, CommitSeq(basis.0 + 3));
        assert_eq!(db.delta_since(basis).unwrap().count(), 3);
        assert_eq!(logical_state(&db, &checkpoint), expected_state(&rows));
        let mut policy = BulkLoadPolicy::new(2, S);
        policy.resume = Some(checkpoint.clone());
        let completed = db
            .bulk_load(&contexts.query(), &contexts.commit(), rows.clone(), policy)
            .await
            .unwrap();
        assert_eq!(completed.vertices, checkpoint.vertices);
        assert_eq!(completed.edges, checkpoint.edges);
        assert_eq!(
            (
                completed.next_row,
                completed.committed_chunks,
                completed.frontier
            ),
            (5, 3, checkpoint.frontier)
        );
        let empty = db
            .bulk_load(
                &contexts.query(),
                &contexts.commit(),
                Vec::<BulkRow>::new(),
                BulkLoadPolicy::new(2, R),
            )
            .await
            .unwrap();
        assert_eq!(empty.frontier, checkpoint.frontier);
        assert_eq!((empty.next_row, empty.committed_chunks), (0, 0));
        assert_eq!(db.frontier().unwrap(), checkpoint.frontier);
        assert_eq!(db.delta_since(basis).unwrap().count(), 3);
        assert_eq!(logical_state(&db, &checkpoint), expected_state(&rows));
    });
}

fn zone(key: &str, label: LabelId, k: i64) -> BulkRow {
    BulkRow::Vertex(BulkVertex {
        key: key.to_owned(),
        labels: vec![label],
        props: vec![(K, CanonicalScalar::Int(k).into())],
    })
}

fn matched(label: Option<LabelId>, k: i64) -> BulkEndpoint {
    BulkEndpoint::matching(label, K, CanonicalScalar::Int(k))
}

fn trip(n: i64, source: BulkEndpoint, destination: BulkEndpoint) -> BulkRow {
    BulkRow::Edge(BulkEdge {
        key: format!("t{n}"),
        source,
        destination,
        relation: R,
        props: vec![(P, CanonicalScalar::Int(n).into())],
    })
}

/// Ten PERSON zones k=0..9 plus an OTHER vertex that shares k=5.
fn zone_rows() -> Vec<BulkRow> {
    (0..10)
        .map(|k| zone(&format!("z{k}"), PERSON, k))
        .chain([zone("other5", OTHER, 5)])
        .collect()
}

/// A second load, one row per chunk: the vertex `fresh7` (also a PERSON with
/// k=7) commits before the trips that name k=7 by match, which must still
/// resolve to the zone committed before this load began.
fn trip_rows() -> Vec<BulkRow> {
    vec![
        zone("fresh7", PERSON, 7),
        trip(0, matched(Some(PERSON), 1), matched(Some(PERSON), 2)),
        trip(1, matched(Some(PERSON), 7), "fresh7".into()),
        trip(2, matched(Some(OTHER), 5), matched(None, 9)),
        trip(3, "fresh7".into(), matched(Some(PERSON), 7)),
    ]
}

fn assert_trips<V: asupersync::fs::Vfs + Clone>(
    db: &Database<V>,
    zones: &BulkLoadCheckpoint,
    trips: &BulkLoadCheckpoint,
) {
    let zone = |key: &str| zones.vertices[key];
    let fresh = trips.vertices["fresh7"];
    assert_ne!(fresh, zone("z7"));
    for (key, src, dst) in [
        ("t0", zone("z1"), zone("z2")),
        ("t1", zone("z7"), fresh),
        ("t2", zone("other5"), zone("z9")),
        ("t3", fresh, zone("z7")),
    ] {
        let row = db.edge(trips.edges[key]).unwrap().unwrap();
        assert_eq!(
            (row.entry.src, row.entry.dst, row.entry.relation),
            (src, dst, R),
            "{key}"
        );
    }
}

/// An edge endpoint can name a vertex an EARLIER load committed, by label and
/// property value, resolved as of this load's basis (fgdb-srnr7).
#[test]
fn match_endpoints_name_vertices_committed_before_the_load() {
    under_lab(0xb71c_0101, |contexts| async move {
        let (query, commit) = (contexts.query(), contexts.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let zones = db
            .bulk_load(&query, &commit, zone_rows(), BulkLoadPolicy::new(4, R))
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let vertices = db.vertices().unwrap();
        // Refusals happen in preflight: nothing commits.
        for (endpoint, unresolved) in [
            (matched(Some(PERSON), 99), true),
            (matched(Some(OTHER), 7), true),
            (BulkEndpoint::matching(None, K, CanonicalScalar::Null), true),
            // Both the PERSON zone and the OTHER vertex carry k=5.
            (matched(None, 5), false),
        ] {
            let mut rows = trip_rows();
            rows.push(trip(9, "fresh7".into(), endpoint.clone()));
            let error = db
                .bulk_load(&query, &commit, rows, BulkLoadPolicy::new(1, R))
                .await
                .unwrap_err();
            match (&error.kind, unresolved) {
                (
                    BulkLoadErrorKind::UnresolvedEndpoint {
                        row,
                        edge,
                        endpoint: e,
                    },
                    true,
                )
                | (
                    BulkLoadErrorKind::AmbiguousEndpoint {
                        row,
                        edge,
                        endpoint: e,
                    },
                    false,
                ) => {
                    // The refused edge is the sixth source row.
                    assert_eq!((*row, edge.as_str(), e), (5, "t9", &endpoint));
                }
                other => panic!("{endpoint:?}: {other:?}"),
            }
            assert_eq!(error.committed.committed_chunks, 0, "{endpoint:?}");
            assert!(error.pending.is_none(), "{endpoint:?}");
            assert_eq!(db.frontier().unwrap(), before, "{endpoint:?}");
            assert_eq!(db.vertices().unwrap(), vertices, "{endpoint:?}");
        }
        let trips = db
            .bulk_load(&query, &commit, trip_rows(), BulkLoadPolicy::new(1, R))
            .await
            .unwrap();
        assert_eq!(trips.committed_chunks, 5);
        assert_trips(&db, &zones, &trips);
    });
}

/// A crash after the load's own k=7 vertex committed and a resume from the
/// recovered checkpoint reach the same edges as the uninterrupted load: the
/// basis is recomputed from the checkpoint, never from the advanced frontier.
#[test]
fn match_endpoints_resume_against_the_original_basis() {
    under_lab(0xb71c_0102, |contexts| async move {
        let (query, commit) = (contexts.query(), contexts.commit());
        let path = scratch("match-resume");
        let mut db = Database::create(&commit, &path, keys()).await.unwrap();
        let zones = db
            .bulk_load(&query, &commit, zone_rows(), BulkLoadPolicy::new(4, R))
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let error = db
            .bulk_load_with_crash(
                &query,
                &commit,
                trip_rows(),
                BulkLoadPolicy::new(1, R),
                Some((2, CrashPoint::BeforeCapsule)),
            )
            .await
            .expect_err("the crash seam in the third chunk must be reached");
        assert_eq!(error.committed.committed_chunks, 2);
        drop(db);
        let mut db = Database::open(&commit, &path, keys()).await.unwrap();
        db.ensure_delta_window(&commit, basis).await.unwrap();
        let recovered = db.frontier().unwrap();
        let resume = match &error.pending {
            Some(pending) if pending.frontier == recovered => pending.clone(),
            _ => error.committed.clone(),
        };
        let mut policy = BulkLoadPolicy::new(1, R);
        policy.resume = Some(resume);
        let trips = db
            .bulk_load(&query, &commit, trip_rows(), policy)
            .await
            .unwrap();
        assert_eq!(trips.frontier, CommitSeq(basis.0 + 5));
        assert_trips(&db, &zones, &trips);
    });
}

/// Repeated property endpoints keep their exact scalar variant, label and
/// property coordinate through preflight and multiple publication chunks.
#[test]
fn repeated_match_endpoints_keep_exact_identity_and_do_not_escape_the_load() {
    under_lab(0xb71c_0103, |contexts| async move {
        use fgdb_types::{CanonicalDecimal, CanonicalF64};

        let (query, commit) = (contexts.query(), contexts.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let targets = [
            (PERSON, K, CanonicalScalar::Int(7)),
            (PERSON, K, CanonicalScalar::Float(CanonicalF64::new(7.0))),
            (
                PERSON,
                K,
                CanonicalScalar::Decimal(CanonicalDecimal::from_integer(7).unwrap()),
            ),
            (PERSON, K, CanonicalScalar::ucs_basic_text("7").unwrap()),
            (PERSON, K, CanonicalScalar::bytes(vec![7]).unwrap()),
            (OTHER, K, CanonicalScalar::Int(7)),
            (PERSON, P, CanonicalScalar::Int(7)),
        ];
        let initial: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(index, (label, key, value))| {
                BulkRow::Vertex(BulkVertex {
                    key: format!("endpoint-{index}"),
                    labels: vec![*label],
                    props: vec![(*key, value.clone().into())],
                })
            })
            .collect();
        let committed = db
            .bulk_load(&query, &commit, initial, BulkLoadPolicy::new(32, R))
            .await
            .unwrap();
        let endpoint = |index: usize| {
            let (label, key, value) = &targets[index];
            BulkEndpoint::matching(Some(*label), *key, value.clone())
        };
        let input: Vec<_> = (0..128)
            .map(|row| {
                trip(
                    row as i64,
                    endpoint(row % targets.len()),
                    endpoint((row + 1) % targets.len()),
                )
            })
            .collect();
        let loaded = db
            .bulk_load(&query, &commit, input, BulkLoadPolicy::new(32, R))
            .await
            .unwrap();
        assert_eq!(loaded.committed_chunks, 4);
        for row in 0..128 {
            let edge = db.edge(loaded.edges[&format!("t{row}")]).unwrap().unwrap();
            // Independently known seed identities, not another endpoint-cache
            // lookup, are the oracle for every materialized edge.
            assert_eq!(
                (edge.entry.src, edge.entry.dst),
                (
                    committed.vertices[&format!("endpoint-{}", row % targets.len())],
                    committed.vertices[&format!("endpoint-{}", (row + 1) % targets.len())],
                ),
                "edge {row}",
            );
        }

        // A later invocation has a new basis. A previously unique exact key
        // is now ambiguous; no earlier invocation's memo may hide the change.
        db.bulk_load(
            &query,
            &commit,
            [zone("duplicate", PERSON, 7)],
            BulkLoadPolicy::new(1, R),
        )
        .await
        .unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .bulk_load(
                &query,
                &commit,
                [trip(999, endpoint(0), endpoint(1))],
                BulkLoadPolicy::new(1, R),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::AmbiguousEndpoint { row: 0, .. }
        ));
        assert_eq!(error.committed.committed_chunks, 0);
        assert!(error.pending.is_none());
        assert_eq!(db.frontier().unwrap(), before);
    });
}
