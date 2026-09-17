//! Published FGSB/FGSP one-bit damage: cached answers are not storage verification.
//! Every record is printed only after its path's exact assertions have passed.

use asupersync::fs::Vfs;
use asupersync::io::{AsyncRead, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BulkEdge, BulkLoadCheckpoint, BulkLoadErrorKind, BulkLoadPolicy, BulkRow, BulkVertex, Database,
    DatabaseKeys, GqlError, OpenError, ReadError, RebuildError, WriteBatch,
};
use fgdb_chronicle::scrub::LostReason;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphText, PreparedTemporalGraphAggregateText,
    PreparedTemporalGraphText,
};
use fgdb_sim::vfs::{FaultKind, FaultPlan, FaultVfs, Trigger};
use fgdb_strata::edge_props::EdgePropertyPatchError;
use fgdb_strata::store::{BlockStore, StoreError};
use fgdb_strata::{DeltaBlockVersion, decode_block_with_properties};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, ObjectId,
    PurposeContexts, QueryCx, VId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const SEEDS: [u64; 3] = [3, 17, 91];
const PATTERN: &str = "MATCH (a)-[e:R]->(b) RETURN e.p AS ep,a.p AS ap,b.p AS bp ORDER BY ep";
const TEMPORAL_PATTERN: &str = "MATCH (a)-[e:R]->(b) FOR SYSTEM_TIME AS OF SEQ $at RETURN e.p AS ep,a.p AS ap,b.p AS bp ORDER BY ep";
const AGGREGATE: &str = "MATCH (a)-[e:R]->(b) RETURN COUNT(*) AS c,SUM(e.p) AS s";
const TEMPORAL_AGGREGATE: &str =
    "MATCH (a)-[e:R]->(b) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS c,SUM(e.p) AS s";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fgdb-block-bit-rot-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 4_000_000, 4_000_000)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Fgsb,
    Fgsp,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Fgsb => "FGSB",
            Self::Fgsp => "FGSP",
        }
    }
}

#[derive(Clone, Copy)]
struct Case {
    seed: u64,
    family: Family,
    lying: bool,
}

impl Case {
    fn record(self, path: &str, outcome: &str, detail: impl std::fmt::Debug) {
        println!(
            "BLOCK_ROT|seed={}|family={}|sync={}|path={path}|outcome={outcome}|detail={detail:?}",
            self.seed,
            self.family.name(),
            if self.lying { "lying" } else { "honest" }
        );
    }

    fn vfs(self) -> FaultVfs {
        FaultVfs::unix(FaultPlan {
            seed: self.seed,
            fsync_lie: if self.lying {
                Trigger::At(1)
            } else {
                Trigger::Never
            },
            ..FaultPlan::faultless()
        })
    }
}

struct Object {
    family: Family,
    id: ObjectId,
    path: PathBuf,
    bytes: Vec<u8>,
}

// Only authenticated publication references choose the targets. Orphan .block
// files, root bytes, manifests and vertex patches are deliberately not candidates.
async fn inventory(
    db: &Database<FaultVfs>,
    store: &BlockStore<FaultVfs>,
    query: &QueryCx,
) -> Vec<Object> {
    let roots = store
        .resolve_manifest(query, db.manifest().unwrap())
        .await
        .unwrap();
    let root_id = db.partition_root().unwrap();
    assert!(roots.iter().any(|(record, _)| record.root == root_id));
    let mut found = BTreeMap::new();
    for (_, root) in roots {
        for reference in root.blocks {
            let id = reference.block_id;
            let bytes = store.get_bytes(query, DeltaBlockVersion(id)).await.unwrap();
            assert_eq!(&bytes[..4], b"FGSB");
            if let Some((patch_id, _)) = decode_block_with_properties(&bytes).unwrap().1 {
                let patch = store
                    .get_edge_property_patch_bytes(query, patch_id)
                    .await
                    .unwrap();
                assert_eq!(&patch[..4], b"FGSP");
                found.insert(
                    patch_id,
                    Object {
                        family: Family::Fgsp,
                        id: patch_id,
                        path: store.path(patch_id),
                        bytes: patch,
                    },
                );
            }
            found.insert(
                id,
                Object {
                    family: Family::Fgsb,
                    id,
                    path: store.path(id),
                    bytes,
                },
            );
        }
    }
    for family in [Family::Fgsb, Family::Fgsp] {
        assert!(found.values().any(|object| object.family == family));
    }
    found.into_values().collect()
}

async fn visible_bytes(vfs: &FaultVfs, path: &Path) -> Vec<u8> {
    let mut file = vfs.open_read(path).await.unwrap();
    let mut result = Vec::new();
    loop {
        let mut bytes = [0; 4096];
        let mut buffer = ReadBuf::new(&mut bytes);
        poll_fn(|cx| Pin::new(&mut file).poll_read(cx, &mut buffer))
            .await
            .unwrap();
        if buffer.filled().is_empty() {
            break;
        }
        result.extend_from_slice(buffer.filled());
    }
    result
}

async fn mutate(case: Case, vfs: &FaultVfs, target: &Object) -> Vec<u8> {
    assert_eq!(vfs.read(&target.path).await.unwrap(), target.bytes);
    assert_eq!(visible_bytes(vfs, &target.path).await, target.bytes);
    // Preserve the framing magic while varying the damaged byte and bit by seed.
    let offset = 8 + case.seed as usize % (target.bytes.len() - 8);
    let mask = 1_u8 << (case.seed % 8);
    let mut damaged = target.bytes.clone();
    damaged[offset] ^= mask;
    let differences: Vec<_> = target
        .bytes
        .iter()
        .zip(&damaged)
        .enumerate()
        .filter_map(|(at, (a, b))| (a != b).then_some((at, a ^ b)))
        .collect();
    assert_eq!(differences, vec![(offset, mask)]);
    assert_eq!(mask.count_ones(), 1);
    let event_start = vfs.events().len();
    vfs.write(&target.path, &damaged).await.unwrap();
    assert_eq!(visible_bytes(vfs, &target.path).await, damaged);
    assert_eq!(
        vfs.read(&target.path).await.unwrap(),
        if case.lying { &target.bytes } else { &damaged }.clone()
    );
    let lies = vfs.events()[event_start..]
        .iter()
        .filter(|event| matches!(event.kind, FaultKind::FsyncLie { .. }))
        .count();
    assert_eq!(lies, usize::from(case.lying));
    case.record(
        "mutation",
        if case.lying {
            "volatile-only-damage"
        } else {
            "durable-damage"
        },
        (target.id, offset, mask),
    );
    case.record("fault-vfs-inode-cache", "visible-one-bit-damage", target.id);
    damaged
}

fn identity(error: &StoreError, expected_id: ObjectId) {
    match error {
        StoreError::IdentityMismatch { expected, actual } => {
            assert_eq!(*expected, expected_id);
            assert_ne!(*actual, expected_id);
        }
        other => panic!("expected direct IdentityMismatch naming {expected_id:?}, got {other:?}"),
    }
}

async fn direct_refusal(
    case: Case,
    store: &BlockStore<FaultVfs>,
    query: &QueryCx,
    target: &Object,
) {
    let error = match target.family {
        Family::Fgsb => store
            .get_bytes(query, DeltaBlockVersion(target.id))
            .await
            .unwrap_err(),
        Family::Fgsp => store
            .get_edge_property_patch_bytes(query, target.id)
            .await
            .unwrap_err(),
    };
    identity(&error, target.id);
    case.record(
        match target.family {
            Family::Fgsb => "BlockStore::get_bytes",
            Family::Fgsp => "BlockStore::get_edge_property_patch_bytes",
        },
        "typed-identity-refusal",
        error,
    );
}

fn cold_refusal(case: Case, error: OpenError, id: ObjectId, path: &str) {
    match &error {
        OpenError::Rebuild(RebuildError::Store(StoreError::RootBlockLoad { error, .. }))
            if case.family == Family::Fgsb =>
        {
            identity(error, id)
        }
        OpenError::Rebuild(RebuildError::Store(StoreError::MalformedEdgePropertyPatch(
            EdgePropertyPatchError::IdentityMismatch { expected, actual },
        ))) if case.family == Family::Fgsp => {
            assert_eq!(*expected, id);
            assert_ne!(*actual, id);
        }
        other => panic!(
            "expected cold {} identity refusal naming {id:?}, got {other:?}",
            case.family.name()
        ),
    }
    case.record(path, "typed-identity-refusal", error);
}

async fn fixture(commit: &CommitCx, path: &Path) {
    let vfs = FaultVfs::unix(FaultPlan::faultless());
    let mut db = Database::create_with_vfs(commit, vfs.clone(), path, keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(R);
    for (id, value) in [(1, 10), (2, 20), (3, 30)] {
        first.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    first.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(5))]);
    first.add_edge(EId(11), VId(2), VId(3), vec![(P, CanonicalScalar::Int(7))]);
    assert_eq!(db.write(commit, first).await.unwrap(), CommitSeq(1));
    let mut second = WriteBatch::new(R);
    second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(40)));
    second.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(6)));
    second.delete_edge(EId(11));
    second.add_edge(EId(12), VId(1), VId(3), vec![(P, CanonicalScalar::Int(9))]);
    assert_eq!(db.write(commit, second).await.unwrap(), CommitSeq(2));
    assert!(db.verify_snapshot_indexes().unwrap());
    assert_eq!(vfs.pending_dirent_ops(), 0);
    drop(db);
    vfs.crash().await.unwrap();
}

fn integer_rows(
    result: &fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
) -> Vec<Vec<i64>> {
    result
        .value
        .iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|value| match value.as_scalar() {
                    Some(CanonicalScalar::Int(value)) => *value,
                    other => panic!("expected integer graph column, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

fn aggregate_values(
    result: &fgdb_gql::GqlQueryExecution<fgdb_gql::GraphAggregateRow>,
) -> (u64, i128) {
    assert_eq!(result.value.len(), 1);
    let values = result.value[0].values();
    assert_eq!(values.len(), 2);
    (
        values[0].as_count().unwrap(),
        values[1].as_integer().unwrap(),
    )
}

fn retained_answers(case: Case, db: &Database<FaultVfs>, query: &QueryCx, outcome: &str) {
    let pattern = PreparedGraphText::prepare(PATTERN, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let aggregate = PreparedGraphAggregateText::prepare(AGGREGATE, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let historical_pattern = PreparedTemporalGraphText::prepare(TEMPORAL_PATTERN, symbols).unwrap();
    let historical_aggregate =
        PreparedTemporalGraphAggregateText::prepare(TEMPORAL_AGGREGATE, symbols).unwrap();
    let current_rows = vec![vec![6, 40, 20], vec![9, 40, 30]];
    let live = db
        .execute_graph_pattern_governed(query, &pattern, policy())
        .unwrap();
    assert_eq!(integer_rows(&live), current_rows);
    case.record("gql-pattern-live", outcome, integer_rows(&live));
    let live_sum = db
        .execute_graph_aggregate_governed(query, &aggregate, policy())
        .unwrap();
    assert_eq!(aggregate_values(&live_sum), (2, 15));
    case.record("gql-aggregate-live", outcome, aggregate_values(&live_sum));
    for (at, expected_rows, expected_sum, neighbours, edge_value) in [
        (
            CommitSeq(1),
            vec![vec![5, 10, 20], vec![7, 20, 30]],
            (2, 12),
            vec![VId(2)],
            5,
        ),
        (CommitSeq(2), current_rows, (2, 15), vec![VId(2), VId(3)], 6),
    ] {
        let parameters = GqlParameters::new().with_uint64("at", at.0).unwrap();
        let bound = historical_pattern.bind_parameters(&parameters).unwrap();
        let rows = db
            .execute_temporal_graph_text_governed(query, &bound, policy())
            .unwrap();
        assert_eq!(integer_rows(&rows), expected_rows);
        case.record(
            "gql-pattern-FOR-SYSTEM_TIME-AS-OF",
            outcome,
            (at, integer_rows(&rows)),
        );
        let bound = historical_aggregate.bind_parameters(&parameters).unwrap();
        let sum = db
            .execute_temporal_graph_aggregate_text_governed(query, &bound, policy())
            .unwrap();
        assert_eq!(aggregate_values(&sum), expected_sum);
        case.record(
            "gql-aggregate-FOR-SYSTEM_TIME-AS-OF",
            outcome,
            (at, aggregate_values(&sum)),
        );
        assert_eq!(db.neighbours_at(VId(1), R, at).unwrap(), neighbours);
        let incoming = if at == CommitSeq(1) {
            vec![VId(2)]
        } else {
            vec![VId(1)]
        };
        assert_eq!(db.in_neighbours_at(VId(3), R, at).unwrap(), incoming);
        case.record("native-expansion-at", outcome, (at, &neighbours, incoming));
        let edge = db.edge_at(EId(10), at).unwrap().unwrap();
        assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(edge_value))]);
        case.record("native-edge-property-at", outcome, (at, edge.props));
    }
    assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(2), VId(3)]);
    assert_eq!(db.in_neighbours(VId(3), R).unwrap(), vec![VId(1)]);
    case.record(
        "native-expansion-live",
        outcome,
        (vec![VId(2), VId(3)], vec![VId(1)]),
    );
    let edge = db.edge(EId(10)).unwrap().unwrap();
    assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(6))]);
    case.record("native-edge-property-live", outcome, edge.props);
    assert!(db.verify_snapshot_indexes().unwrap());
    // Equality selects a maintained property posting; its bound expansion uses
    // the maintained adjacency index. A second commit changes both postings.
    let indexed = PreparedGraphText::prepare(
        "MATCH (a)-[e:R]->(b) WHERE a.p = 40 RETURN e.p AS ep,b.p AS bp ORDER BY ep",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let rows = db
        .execute_graph_pattern_governed(query, &indexed, policy())
        .unwrap();
    assert_eq!(integer_rows(&rows), vec![vec![6, 20], vec![9, 30]]);
    case.record("maintained-index-acp4", outcome, integer_rows(&rows));
    let range = PreparedTemporalGraphText::prepare(
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.p >= 20 RETURN n.p AS p ORDER BY p",
        symbols,
    )
    .unwrap();
    for (at, expected) in [
        (1, vec![vec![20], vec![30]]),
        (2, vec![vec![20], vec![30], vec![40]]),
    ] {
        let bound = range
            .bind_parameters(&GqlParameters::new().with_uint64("at", at).unwrap())
            .unwrap();
        let rows = db
            .execute_temporal_graph_text_governed(query, &bound, policy())
            .unwrap();
        assert_eq!(integer_rows(&rows), expected);
        case.record("maintained-index-j8mu", outcome, (at, integer_rows(&rows)));
    }
}

fn retained_fenced(case: Case, db: &Database<FaultVfs>, query: &QueryCx) {
    let pattern = PreparedGraphText::prepare(PATTERN, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let aggregate = PreparedGraphAggregateText::prepare(AGGREGATE, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    macro_rules! gql_fenced {
        ($path:expr, $result:expr) => {
            assert!(matches!(
                $result,
                Err(GqlQueryError::Source(GqlError::Read(
                    ReadError::RecoveryRequired(_)
                )))
            ));
            case.record($path, "post-scrub-recovery-fence", ());
        };
    }
    gql_fenced!(
        "gql-pattern-live",
        db.execute_graph_pattern_governed(query, &pattern, policy())
    );
    assert!(matches!(
        db.execute_graph_aggregate_governed(query, &aggregate, policy()),
        Err(GqlQueryError::Source(
            fgdb_gql::GraphAggregateError::Source(GqlError::Read(ReadError::RecoveryRequired(_)))
        ))
    ));
    case.record("gql-aggregate-live", "post-scrub-recovery-fence", ());
    let indexed = PreparedGraphText::prepare(
        "MATCH (a)-[e:R]->(b) WHERE a.p = 40 RETURN e.p AS ep,b.p AS bp ORDER BY ep",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    gql_fenced!(
        "maintained-index-acp4",
        db.execute_graph_pattern_governed(query, &indexed, policy())
    );
    for at in [CommitSeq(1), CommitSeq(2)] {
        let parameters = GqlParameters::new().with_uint64("at", at.0).unwrap();
        let bound = PreparedTemporalGraphText::prepare(TEMPORAL_PATTERN, symbols)
            .unwrap()
            .bind_parameters(&parameters)
            .unwrap();
        gql_fenced!(
            "gql-pattern-FOR-SYSTEM_TIME-AS-OF",
            db.execute_temporal_graph_text_governed(query, &bound, policy())
        );
        let bound = PreparedTemporalGraphAggregateText::prepare(TEMPORAL_AGGREGATE, symbols)
            .unwrap()
            .bind_parameters(&parameters)
            .unwrap();
        assert!(matches!(
            db.execute_temporal_graph_aggregate_text_governed(query, &bound, policy()),
            Err(GqlQueryError::Source(
                fgdb_gql::GraphAggregateError::Source(GqlError::Read(ReadError::RecoveryRequired(
                    _
                )))
            ))
        ));
        case.record(
            "gql-aggregate-FOR-SYSTEM_TIME-AS-OF",
            "post-scrub-recovery-fence",
            (),
        );
        let bound = PreparedTemporalGraphText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.p >= 20 RETURN n.p AS p ORDER BY p",
            symbols,
        )
        .unwrap()
        .bind_parameters(&parameters)
        .unwrap();
        gql_fenced!(
            "maintained-index-j8mu",
            db.execute_temporal_graph_text_governed(query, &bound, policy())
        );
        assert!(matches!(
            db.neighbours_at(VId(1), R, at),
            Err(ReadError::RecoveryRequired(_))
        ));
        assert!(matches!(
            db.in_neighbours_at(VId(3), R, at),
            Err(ReadError::RecoveryRequired(_))
        ));
        assert!(matches!(
            db.edge_at(EId(10), at),
            Err(ReadError::RecoveryRequired(_))
        ));
        case.record("native-expansion-at", "post-scrub-recovery-fence", at);
        case.record("native-edge-property-at", "post-scrub-recovery-fence", at);
    }
    assert!(matches!(
        db.neighbours(VId(1), R),
        Err(ReadError::RecoveryRequired(_))
    ));
    assert!(matches!(
        db.edge(EId(10)),
        Err(ReadError::RecoveryRequired(_))
    ));
    assert!(matches!(
        db.verify_snapshot_indexes(),
        Err(ReadError::RecoveryRequired(_))
    ));
    case.record("native-expansion-live", "post-scrub-recovery-fence", ());
    case.record("native-edge-property-live", "post-scrub-recovery-fence", ());
}

#[test]
fn published_block_bit_rot_three_seed_read_scrub_and_sync_matrix() {
    for seed in SEEDS {
        for family in [Family::Fgsb, Family::Fgsp] {
            for lying in [false, true] {
                let case = Case {
                    seed,
                    family,
                    lying,
                };
                let ((), report) = run_async_under_lab(seed, move |root| async move {
                    let contexts = PurposeContexts::narrow_runtime_root(&root);
                    let commit = contexts.commit();
                    let query = contexts.query();
                    let path = scratch();
                    fixture(&commit, &path).await;
                    // Start the fault plan only after the two commits are durable.
                    let vfs = case.vfs();
                    let mut db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                    let store =
                        BlockStore::open_with_vfs(&commit, vfs.clone(), &path, K_OID, NAMESPACE)
                            .await
                            .unwrap();
                    let objects = inventory(&db, &store, &query).await;
                    let candidates: Vec<_> = objects
                        .iter()
                        .filter(|object| object.family == family)
                        .collect();
                    let target = candidates[seed as usize % candidates.len()];
                    retained_answers(case, &db, &query, "pristine-baseline");
                    let damaged = mutate(case, &vfs, target).await;
                    direct_refusal(case, &store, &query, target).await;
                    retained_answers(case, &db, &query, "cache-only/not-detected");
                    let summary = db.scrub(&commit).await.unwrap();
                    assert_eq!(summary.objects, 2);
                    assert_eq!(summary.clean.len(), 2);
                    assert!(summary.repaired.is_empty());
                    assert!(summary.lost.is_empty());
                    assert_eq!(summary.block_objects, objects.len());
                    assert_eq!(summary.block_lost.len(), 1);
                    assert_eq!(summary.block_lost[0].object_id, target.id);
                    assert_eq!(summary.block_lost[0].reason, LostReason::IdentityMismatch);
                    let clean: BTreeSet<_> = summary.block_clean.iter().copied().collect();
                    assert_eq!(clean.len(), summary.block_clean.len());
                    assert_eq!(
                        clean,
                        objects
                            .iter()
                            .filter(|object| object.id != target.id)
                            .map(|object| object.id)
                            .collect()
                    );
                    assert_eq!(visible_bytes(&vfs, &target.path).await, damaged);
                    assert_eq!(
                        vfs.read(&target.path).await.unwrap(),
                        if lying { &target.bytes } else { &damaged }.clone()
                    );
                    case.record(
                        "Database::scrub",
                        "target-lost-no-repair-bytes-unchanged",
                        &summary,
                    );
                    retained_fenced(case, &db, &query);
                    drop(db);
                    let error = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                        .await
                        .err()
                        .expect("visible corrupted bytes must refuse reopen");
                    cold_refusal(case, error, target.id, "reopen-visible-before-crash");
                    vfs.crash().await.unwrap();
                    assert_eq!(
                        visible_bytes(&vfs, &target.path).await,
                        if lying { &target.bytes } else { &damaged }.clone()
                    );
                    if lying {
                        let db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                            .await
                            .unwrap();
                        retained_answers(case, &db, &query, "pristine-rollback-not-healing");
                        case.record(
                            "cold-reopen-after-crash",
                            "pristine-rollback-not-healing",
                            target.id,
                        );
                        drop(db);
                    } else {
                        let error = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                            .await
                            .err()
                            .expect("durable one-bit corruption must refuse cold reopen");
                        cold_refusal(case, error, target.id, "cold-reopen-after-crash");
                    }
                    drop(store);
                });
                assert!(report.lab_test_passed(), "{report:?}");
            }
        }
    }
}

fn bulk_rows() -> Vec<BulkRow> {
    vec![
        BulkRow::Vertex(BulkVertex {
            key: "a".into(),
            labels: vec![],
            props: vec![(P, CanonicalScalar::Int(10))],
        }),
        BulkRow::Vertex(BulkVertex {
            key: "b".into(),
            labels: vec![],
            props: vec![(P, CanonicalScalar::Int(20))],
        }),
        BulkRow::Edge(BulkEdge {
            key: "ab".into(),
            source: "a".into(),
            destination: "b".into(),
            relation: R,
            props: vec![(P, CanonicalScalar::Int(5))],
        }),
        BulkRow::Edge(BulkEdge {
            key: "ba".into(),
            source: "b".into(),
            destination: "a".into(),
            relation: R,
            props: vec![(P, CanonicalScalar::Int(7))],
        }),
        // A distinct property row avoids incidental FGSP deduplication against
        // the damaged prefix; this measures resume, not immutable-put reuse.
        BulkRow::Edge(BulkEdge {
            key: "aa".into(),
            source: "a".into(),
            destination: "a".into(),
            relation: R,
            props: vec![(P, CanonicalScalar::Int(99))],
        }),
    ]
}

async fn bulk_prefix(
    db: &mut Database<FaultVfs>,
    commit: &CommitCx,
    query: &QueryCx,
) -> BulkLoadCheckpoint {
    let error = db
        .bulk_load_with_checkpoint(
            query,
            commit,
            bulk_rows(),
            BulkLoadPolicy::new(4, R),
            None,
            |_| {
                Err(std::io::Error::other(
                    "stop after acknowledged four-row prefix",
                ))
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error.kind, BulkLoadErrorKind::Checkpoint(_)));
    assert!(error.pending.is_none());
    assert_eq!(error.committed.next_row, 4);
    assert_eq!(error.committed.committed_chunks, 1);
    assert_eq!(error.committed.frontier, CommitSeq(1));
    bulk_answers(db, &error.committed, false);
    error.committed
}

fn bulk_answers(db: &Database<FaultVfs>, checkpoint: &BulkLoadCheckpoint, finished: bool) {
    let expected_vertices: BTreeSet<_> = ["a".to_owned(), "b".to_owned()].into_iter().collect();
    assert_eq!(
        checkpoint.vertices.keys().cloned().collect::<BTreeSet<_>>(),
        expected_vertices
    );
    let expected_edges: BTreeSet<_> = if finished {
        vec!["ab", "ba", "aa"]
    } else {
        vec!["ab", "ba"]
    }
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        checkpoint.edges.keys().cloned().collect::<BTreeSet<_>>(),
        expected_edges
    );
    assert_eq!(db.vertices().unwrap().len(), 2);
    assert_eq!(db.edges().unwrap().len(), expected_edges.len());
    for (key, value) in [("a", 10), ("b", 20)] {
        let vertex = db.vertex(checkpoint.vertices[key]).unwrap().unwrap();
        assert!(vertex.labels.is_empty());
        assert_eq!(vertex.props, vec![(P, CanonicalScalar::Int(value))]);
    }
    for (key, source, destination, value) in [
        ("ab", "a", "b", 5),
        ("ba", "b", "a", 7),
        ("aa", "a", "a", 99),
    ] {
        if key == "aa" && !finished {
            continue;
        }
        let edge = db.edge(checkpoint.edges[key]).unwrap().unwrap();
        assert_eq!(edge.entry.src, checkpoint.vertices[source]);
        assert_eq!(edge.entry.dst, checkpoint.vertices[destination]);
        assert_eq!(edge.entry.relation, R);
        assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(value))]);
    }
}

#[test]
fn published_block_bit_rot_three_seed_bulk_resume_matrix() {
    for seed in SEEDS {
        for family in [Family::Fgsb, Family::Fgsp] {
            let ((), report) = run_async_under_lab(seed, move |root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let query = contexts.query();
                // Keep the same handle that published the prefix: reopening
                // would discard its publication receipts and test another path.
                let case = Case {
                    seed,
                    family,
                    lying: false,
                };
                let path = scratch();
                let vfs = case.vfs();
                let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                let checkpoint = bulk_prefix(&mut db, &commit, &query).await;
                let store =
                    BlockStore::open_with_vfs(&commit, vfs.clone(), &path, K_OID, NAMESPACE)
                        .await
                        .unwrap();
                let objects = inventory(&db, &store, &query).await;
                let target = objects
                    .iter()
                    .find(|object| object.family == family)
                    .unwrap();
                let damaged = mutate(case, &vfs, target).await;
                direct_refusal(case, &store, &query, target).await;
                bulk_answers(&db, &checkpoint, false);
                let mut resume = BulkLoadPolicy::new(4, R);
                resume.resume = Some(checkpoint);
                let finished = db
                    .bulk_load(&query, &commit, bulk_rows(), resume)
                    .await
                    .unwrap();
                assert_eq!(finished.next_row, 5);
                assert_eq!(finished.committed_chunks, 2);
                assert_eq!(finished.frontier, CommitSeq(2));
                bulk_answers(&db, &finished, true);
                assert_eq!(visible_bytes(&vfs, &target.path).await, damaged);
                assert_eq!(vfs.read(&target.path).await.unwrap(), damaged);
                direct_refusal(case, &store, &query, target).await;
                case.record(
                    "bulk-resume-retained",
                    "cache-only/not-detected",
                    (
                        &finished,
                        "exact two vertices and three propertied edges; damaged prefix unchanged",
                    ),
                );
                drop(db);
                vfs.crash().await.unwrap();
                let error = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .err()
                    .expect("resumed import still names damaged prefix");
                cold_refusal(
                    case,
                    error,
                    target.id,
                    "bulk-resume-retained-then-cold-open",
                );
                drop(store);

                // A reopened handle retains decoded rows but no publication
                // receipts. Its resumed put must detect the dirty prefix even
                // when the mutation's first dirty sync lied. Later commit syncs
                // are honest, so the failed publication has a durable suffix.
                let case = Case {
                    seed,
                    family,
                    lying: true,
                };
                let path = scratch();
                let setup = FaultVfs::unix(FaultPlan::faultless());
                let mut db = Database::create_with_vfs(&commit, setup.clone(), &path, keys())
                    .await
                    .unwrap();
                let checkpoint = bulk_prefix(&mut db, &commit, &query).await;
                drop(db);
                setup.crash().await.unwrap();
                let vfs = case.vfs();
                let mut db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                let store =
                    BlockStore::open_with_vfs(&commit, vfs.clone(), &path, K_OID, NAMESPACE)
                        .await
                        .unwrap();
                let objects = inventory(&db, &store, &query).await;
                let target = objects
                    .iter()
                    .find(|object| object.family == family)
                    .unwrap();
                let damaged = mutate(case, &vfs, target).await;
                direct_refusal(case, &store, &query, target).await;
                bulk_answers(&db, &checkpoint, false);
                let mut resume = BulkLoadPolicy::new(4, R);
                resume.resume = Some(checkpoint);
                let error = db
                    .bulk_load(&query, &commit, bulk_rows(), resume)
                    .await
                    .unwrap_err();
                match &error.kind {
                    BulkLoadErrorKind::Write(fgdb::WriteTxnError::Write(
                        fgdb::WriteError::CommittedNeedsRecovery { source, .. },
                    )) => match source.as_ref() {
                        RebuildError::Store(StoreError::DamagedExisting { expected, actual }) => {
                            assert_eq!(*expected, target.id);
                            assert_ne!(*actual, target.id);
                        }
                        other => panic!(
                            "expected immutable-put damage naming {:?}, got {other:?}",
                            target.id
                        ),
                    },
                    other => {
                        panic!("expected committed derived-publication refusal, got {other:?}")
                    }
                }
                assert_eq!(error.committed.next_row, 4);
                let pending = error
                    .pending
                    .as_ref()
                    .expect("durable attempted successor checkpoint");
                assert_eq!(pending.next_row, 5);
                assert_eq!(pending.frontier, CommitSeq(2));
                assert_eq!(visible_bytes(&vfs, &target.path).await, damaged);
                assert_eq!(vfs.read(&target.path).await.unwrap(), target.bytes);
                case.record(
                    "bulk-resume-retained-reopened-handle",
                    "typed-put-refusal-committed-needs-recovery",
                    &error,
                );
                assert!(matches!(db.edges(), Err(ReadError::RecoveryRequired(_))));
                drop(db);
                vfs.crash().await.unwrap();
                assert_eq!(visible_bytes(&vfs, &target.path).await, target.bytes);
                let db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                bulk_answers(&db, pending, true);
                assert_eq!(db.frontier().unwrap(), CommitSeq(2));
                case.record(
                    "bulk-resume-retained-reopened-after-crash",
                    "pristine-rollback-plus-durable-suffix-not-healing",
                    pending,
                );
                drop(db);
                drop(store);

                // Cold resume must first pass storage admission. Measure both
                // durable rot and a lying mutation's pristine crash rollback.
                for lying in [false, true] {
                    let case = Case {
                        seed,
                        family,
                        lying,
                    };
                    let path = scratch();
                    let setup = FaultVfs::unix(FaultPlan::faultless());
                    let mut db = Database::create_with_vfs(&commit, setup.clone(), &path, keys())
                        .await
                        .unwrap();
                    let checkpoint = bulk_prefix(&mut db, &commit, &query).await;
                    drop(db);
                    setup.crash().await.unwrap();
                    let vfs = case.vfs();
                    let db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                    let store =
                        BlockStore::open_with_vfs(&commit, vfs.clone(), &path, K_OID, NAMESPACE)
                            .await
                            .unwrap();
                    let objects = inventory(&db, &store, &query).await;
                    let target = objects
                        .iter()
                        .find(|object| object.family == family)
                        .unwrap();
                    let damaged = mutate(case, &vfs, target).await;
                    bulk_answers(&db, &checkpoint, false);
                    direct_refusal(case, &store, &query, target).await;
                    drop(db);
                    let error = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                        .await
                        .err()
                        .expect("visible corruption must gate cold bulk resume");
                    cold_refusal(case, error, target.id, "bulk-resume-cold-gate-before-crash");
                    vfs.crash().await.unwrap();
                    if lying {
                        assert_eq!(visible_bytes(&vfs, &target.path).await, target.bytes);
                        // The next VFS is honest: do not inject new unrelated
                        // lies into the continuation's commit protocol.
                        let honest = FaultVfs::unix(FaultPlan::faultless());
                        let mut db = Database::open_with_vfs(&commit, honest, &path, keys())
                            .await
                            .unwrap();
                        bulk_answers(&db, &checkpoint, false);
                        let mut resume = BulkLoadPolicy::new(4, R);
                        resume.resume = Some(checkpoint);
                        let finished = db
                            .bulk_load(&query, &commit, bulk_rows(), resume)
                            .await
                            .unwrap();
                        assert_eq!(finished.next_row, 5);
                        assert_eq!(finished.frontier, CommitSeq(2));
                        bulk_answers(&db, &finished, true);
                        assert_eq!(vfs.read(&target.path).await.unwrap(), target.bytes);
                        case.record(
                            "bulk-resume-cold-gate-after-crash",
                            "pristine-rollback-not-healing",
                            finished,
                        );
                        drop(db);
                    } else {
                        assert_eq!(visible_bytes(&vfs, &target.path).await, damaged);
                        let error = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                            .await
                            .err()
                            .expect("durable corrupted prefix gates resume");
                        cold_refusal(case, error, target.id, "bulk-resume-cold-gate-after-crash");
                    }
                    drop(store);
                }
            });
            assert!(report.lab_test_passed(), "{report:?}");
        }
    }
}
