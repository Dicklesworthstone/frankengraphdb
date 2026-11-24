//! Public signed sessions over real buffered publications. Physical cold
//! admission remains host-bounded; hidden candidates never spend visible query
//! budgets or reach property evaluation. No resident source is used as fallback.
#![recursion_limit = "256"]

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    AuthorizedBufferedError, AuthorizedBufferedReadSource, AuthorizedReadSession, BufferLimits,
    BufferedReadLimits, Database, DatabaseKeys, MemVfs, MemVfsFile, MemoryPool, QueryError,
    WriteBatch,
};
use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbolResolver};
use fgdb_strata::store::BlockStore;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Scope};
use std::future::{Future, poll_fn};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xa6; 32]);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(7);
const S: RelationId = RelationId(8);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa5; 32], NS, [0xa7; 32])
}
fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0xa601), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant(nodes: u64) -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: nodes,
            max_work: 1_000_000,
            max_rows: 10,
        },
    );
    grant.labels = Scope::only([L]);
    grant.relations = Scope::only([R]);
    grant.properties = Scope::only([P]);
    grant
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(2, 10, 100_000, 100_000)
}
fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 64,
        max_vertex_patches: 64,
        max_work: 100_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}
fn memory() -> MemoryPool {
    MemoryPool::new(16 * 1024 * 1024, 0).unwrap()
}

#[derive(Clone, Debug)]
struct GateVfs {
    memory: MemVfs,
    reads: Arc<AtomicUsize>,
    pause: Arc<AtomicBool>,
    refuse: Arc<AtomicBool>,
}
impl GateVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            reads: Arc::default(),
            pause: Arc::default(),
            refuse: Arc::default(),
        }
    }
    fn strata(path: &Path) -> bool {
        path.components()
            .any(|part| part.as_os_str() == "strata-blocks")
    }
    fn reset(&self) {
        self.reads.store(0, Ordering::SeqCst);
    }
}
impl Vfs for GateVfs {
    type File = MemVfsFile;
    async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
        self.memory.open(path, options).await
    }
    async fn open_read(&self, path: &Path) -> io::Result<Self::File> {
        if Self::strata(path) {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.refuse.load(Ordering::SeqCst) {
                return Err(io::Error::other("test stopped cold payload observation"));
            }
            poll_fn(|_| {
                if self.pause.load(Ordering::SeqCst) {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
        }
        self.memory.open_read(path).await
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.memory.metadata(path).await
    }
    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.memory.symlink_metadata(path).await
    }
    async fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        self.memory.set_permissions(path, permissions).await
    }
    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.memory.create_dir(path).await
    }
    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.memory.create_dir_all(path).await
    }
    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.memory.remove_dir(path).await
    }
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.memory.remove_file(path).await
    }
    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.memory.read_dir(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.memory.remove_dir_all(path).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.memory.rename(from, to).await
    }
    async fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.memory.copy(from, to).await
    }
    async fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.memory.hard_link(from, to).await
    }
    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.memory.canonicalize(path).await
    }
    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.memory.read_link(path).await
    }
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.memory.read(path).await
    }
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.memory.read_to_string(path).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.memory.write(path, bytes).await
    }
}

async fn fixture(
    cx: &CommitCx,
    hidden: bool,
) -> (GateVfs, Database<GateVfs>, CommitSeq, CommitSeq) {
    let vfs = GateVfs::new();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.memory.database_dir(), keys())
        .await
        .unwrap();
    let mut catalog = SymbolCatalog::default();
    catalog
        .bind_all([
            ("L", CatalogSymbol::Label(L)),
            ("H", CatalogSymbol::Label(H)),
            ("R", CatalogSymbol::Relation(R)),
            ("S", CatalogSymbol::Relation(S)),
            ("score", CatalogSymbol::Property(P)),
            ("secret", CatalogSymbol::Property(SECRET)),
        ])
        .unwrap();
    db.register_symbols(cx, &catalog).await.unwrap();
    let mut birth = WriteBatch::new(R);
    for id in [1, 2] {
        birth.create_vertex(
            VId(id),
            vec![L, H],
            vec![
                (P, CanonicalScalar::Int(id as i64)),
                (SECRET, CanonicalScalar::Int(999)),
            ],
        );
    }
    if hidden {
        for id in 10..18 {
            birth.create_vertex(
                VId(id),
                vec![H],
                vec![
                    (P, CanonicalScalar::Int(-1)),
                    (SECRET, CanonicalScalar::Int(999)),
                ],
            );
            birth.add_edge(
                EId(id),
                VId(id),
                VId(1),
                vec![
                    (P, CanonicalScalar::Int(-1)),
                    (SECRET, CanonicalScalar::Int(999)),
                ],
            );
        }
    }
    birth.add_edge(
        EId(101),
        VId(1),
        VId(2),
        vec![
            (P, CanonicalScalar::Int(7)),
            (SECRET, CanonicalScalar::Int(999)),
        ],
    );
    let old = db.write(cx, birth).await.unwrap();
    let mut edit = WriteBatch::new(R);
    edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
    edit.set_edge_property(EId(101), P, Some(CanonicalScalar::Int(17)));
    db.write(cx, edit).await.unwrap();
    let mut denied = WriteBatch::new(S);
    denied.add_edge(
        EId(201),
        VId(1),
        VId(2),
        vec![(P, CanonicalScalar::Int(77))],
    );
    let current = db.write(cx, denied).await.unwrap();
    (vfs, db, old, current)
}

#[allow(clippy::too_many_arguments)]
async fn session<'a, C: FnMut() -> u64 + Send>(
    commit: &CommitCx,
    cx: &QueryCx,
    vfs: &GateVfs,
    pool: &MemoryPool,
    authority: &'a Authority,
    token: &CapabilityToken,
    at: CommitSeq,
    clock: C,
) -> AuthorizedReadSession<
    'a,
    impl GraphSymbolResolver + use<C>,
    C,
    AuthorizedBufferedReadSource<GateVfs>,
> {
    Database::open_buffered_publication_with_vfs(
        commit,
        vfs.clone(),
        vfs.memory.database_dir(),
        keys(),
        pool.clone(),
        limits(),
    )
    .await
    .unwrap()
    .into_authorized_read_session(commit, cx, authority, token, "main", at, policy(), clock)
    .await
    .unwrap()
}

async fn collect<R: GraphSymbolResolver, C: FnMut() -> u64 + Send>(
    session: &mut AuthorizedReadSession<'_, R, C, AuthorizedBufferedReadSource<GateVfs>>,
    cx: &QueryCx,
    text: &str,
) -> Vec<GraphValueRow> {
    let args = GqlParameters::new();
    let prepared = session.prepare(cx, text, &args).unwrap();
    let mut cursor = session.stream(cx, &prepared, &args).unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().await {
        rows.push(row.unwrap().as_ref().clone());
    }
    assert_eq!(cursor.state(), VertexScanState::Exhausted);
    assert!(cursor.next().await.is_none());
    rows
}

#[test]
fn clean_and_hidden_sources_have_identical_rows_and_visible_budget_boundaries() {
    let ((), report) = run_async_under_lab(0xa6_0101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixture(&commit, hidden).await;
            for (at, first, weight) in [(old, 1, 7), (current, 11, 17)] {
                let pool = memory();
                let mut session =
                    session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
                let rows = collect(&mut session, &cx, "MATCH (n) RETURN n,n.score,n.secret").await;
                assert_eq!(
                    rows,
                    vec![
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Vertex(VId(1)),
                            GraphValue::Scalar(CanonicalScalar::Int(first)),
                            GraphValue::Scalar(CanonicalScalar::Null)
                        ]),
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Vertex(VId(2)),
                            GraphValue::Scalar(CanonicalScalar::Int(2)),
                            GraphValue::Scalar(CanonicalScalar::Null)
                        ]),
                    ]
                );
                assert!(
                    collect(&mut session, &cx, "MATCH (n:H) RETURN n")
                        .await
                        .is_empty(),
                    "admitted vertices still have hidden label memberships masked"
                );
                let edges = collect(
                    &mut session,
                    &cx,
                    "MATCH (a)-[r:R]->(b) RETURN r,a,b,r.score,r.secret",
                )
                .await;
                assert_eq!(
                    edges,
                    vec![GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(101)),
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Vertex(VId(2)),
                        GraphValue::Scalar(CanonicalScalar::Int(weight)),
                        GraphValue::Scalar(CanonicalScalar::Null),
                    ])]
                );
                vfs.reset();
                vfs.refuse.store(true, Ordering::SeqCst);
                assert!(
                    collect(&mut session, &cx, "MATCH (a)-[r:S]->(b) RETURN r")
                        .await
                        .is_empty()
                );
                assert_eq!(
                    vfs.reads.load(Ordering::SeqCst),
                    0,
                    "denied relation has no source reads"
                );
                vfs.refuse.store(false, Ordering::SeqCst);
                drop(session);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn catalog_and_graph_stay_on_one_cut_and_profile_refusal_precedes_payloads() {
    let ((), report) = run_async_under_lab(0xa6_0102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, mut db, old, current) = fixture(&commit, false).await;
        let pool = memory();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let publication = Database::open_buffered_publication_with_vfs(
            &commit,
            vfs.clone(),
            vfs.memory.database_dir(),
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let mut extra = SymbolCatalog::default();
        extra
            .bind_all([("later", CatalogSymbol::Property(PropertyKeyId(3)))])
            .unwrap();
        db.register_symbols(&commit, &extra).await.unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(111)));
        db.write(&commit, edit).await.unwrap();
        let mut session = publication
            .into_authorized_read_session(
                &commit,
                &cx,
                &authority,
                &token,
                "main",
                old,
                policy(),
                || 100,
            )
            .await
            .unwrap();
        let args = GqlParameters::new();
        assert!(
            session
                .prepare(&cx, "MATCH (n) RETURN n,n.later", &args)
                .is_err(),
            "later catalog bindings cannot enter the selected publication/cut"
        );
        let text = format!("MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN n", current.0);
        let future = session.prepare(&cx, &text, &args).unwrap();
        vfs.reset();
        vfs.refuse.store(true, Ordering::SeqCst);
        assert!(matches!(session.stream(&cx, &future, &args),
            Err(AuthorizedBufferedError::DifferentCut { requested, bound }) if requested == current && bound == old));
        for text in [
            "MATCH (n) RETURN n.score",
            "MATCH (n) RETURN n,n.score ORDER BY n.score",
            "MATCH (a)-[r]->(b) RETURN r",
            "MATCH (a)-[r:R]->(b) RETURN r,a.score",
            "MATCH (n) RETURN n,42 AS value",
        ] {
            let prepared = session.prepare(&cx, text, &args).unwrap();
            assert!(session.stream(&cx, &prepared, &args).is_err(), "{text}");
        }
        let parameters = GqlParameters::new().with_int64("value", 42).unwrap();
        let projected = session
            .prepare(&cx, "MATCH (n) RETURN n,$value AS value", &parameters)
            .unwrap();
        assert!(matches!(
            session.stream(&cx, &projected, &parameters),
            Err(AuthorizedBufferedError::Query(
                QueryError::StreamingUnsupported { .. }
            ))
        ));
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert!(!session.is_closed());
        vfs.refuse.store(false, Ordering::SeqCst);
        let rows = collect(&mut session, &cx, "MATCH (n) RETURN n,n.score LIMIT 1").await;
        assert_eq!(
            rows[0].values(),
            &[
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
            ]
        );
        drop(session);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn namespace_and_read_rights_are_checked_before_catalog_or_graph_admission() {
    let ((), report) = run_async_under_lab(0xa6_0103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, _db, _, at) = fixture(&commit, false).await;
        for wrong_namespace in [false, true] {
            let pool = memory();
            let publication = Database::open_buffered_publication_with_vfs(
                &commit,
                vfs.clone(),
                vfs.memory.database_dir(),
                keys(),
                pool.clone(),
                limits(),
            )
            .await
            .unwrap();
            let authority = if wrong_namespace {
                Authority::new(
                    AuthKey::from_seed(0xa601),
                    DatabaseSecurityNamespaceId([0xff; 32]),
                    "graph",
                    SchemaEpoch(1),
                    1,
                )
                .unwrap()
            } else {
                issuer()
            };
            let mut grant = grant(2);
            if !wrong_namespace {
                grant.rights = fgdb_warden::Rights::None;
            }
            let token = authority.issue_at(&grant, 100).unwrap();
            vfs.reset();
            vfs.refuse.store(true, Ordering::SeqCst);
            let result = publication
                .into_authorized_read_session(
                    &commit,
                    &cx,
                    &authority,
                    &token,
                    "main",
                    at,
                    policy(),
                    || 100,
                )
                .await;
            assert!(matches!(
                result,
                Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                    Error::WrongAuthority | Error::PermissionDenied
                )))
            ));
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
            assert_eq!(pool.used(), 0);
            vfs.refuse.store(false, Ordering::SeqCst);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_candidate_refusal_precedes_corrupt_edge_values_and_admitted_corruption_stays_error() {
    let ((), report) = run_async_under_lab(0xa6_0104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, db, _, at) = fixture(&commit, false).await;
        let authority = issuer();
        let low_token = authority.issue_at(&grant(1), 100).unwrap();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let low_pool = memory();
        let pool = memory();
        let mut low = session(
            &commit,
            &cx,
            &vfs,
            &low_pool,
            &authority,
            &low_token,
            at,
            || 100,
        )
        .await;
        let mut admitted = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let args = GqlParameters::new();
        let text = "MATCH (a)-[r:R]->(b) RETURN r,r.score";
        let low_query = low.prepare(&cx, text, &args).unwrap();
        let query = admitted.prepare(&cx, text, &args).unwrap();
        let store = BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            vfs.memory.database_dir(),
            [0xa5; 32],
            NS,
        )
        .await
        .unwrap();
        let root = store
            .get_root(&commit, db.partition_root().unwrap())
            .await
            .unwrap();
        let mut damaged = 0;
        for reference in root.blocks {
            let bytes = store
                .get_bytes(&commit, fgdb_strata::DeltaBlockVersion(reference.block_id))
                .await
                .unwrap();
            let (entries, properties) = fgdb_strata::decode_block_with_properties(&bytes).unwrap();
            if entries.iter().any(|entry| entry.eid == EId(101))
                && let Some((property_id, _)) = properties
            {
                vfs.memory
                    .write(&store.path(property_id), b"corrupt property sidecar")
                    .await
                    .unwrap();
                damaged += 1;
            }
        }
        assert!(damaged > 0);
        let mut cursor = low.stream(&cx, &low_query, &args).unwrap();
        assert!(matches!(
            cursor.next().await.unwrap(),
            Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                Error::LimitExceeded(LimitDimension::Nodes)
            )))
        ));
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert!(
            !low.is_closed(),
            "ordinary budget refusal permits a new execution"
        );
        let mut cursor = admitted.stream(&cx, &query, &args).unwrap();
        assert!(matches!(
            cursor.next().await.unwrap(),
            Err(AuthorizedBufferedError::Read(_))
        ));
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(low);
        drop(admitted);
        assert_eq!(low_pool.used(), 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unpolled_and_closed_cursors_preserve_sessions_but_dropped_pending_reads_release_them() {
    let ((), report) = run_async_under_lab(0xa6_0105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, _db, _, at) = fixture(&commit, false).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let pool = memory();
        let mut session = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let args = GqlParameters::new();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n,n.score", &args)
            .unwrap();
        let baseline = pool.used();
        vfs.reset();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        drop(cursor.next());
        cursor.close();
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert_eq!(pool.used(), baseline);
        assert!(!session.is_closed());

        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        vfs.pause.store(true, Ordering::SeqCst);
        let mut pending = Box::pin(cursor.next());
        poll_fn(|task| {
            assert!(pending.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(vfs.reads.load(Ordering::SeqCst) > 0);
        drop(pending);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert!(session.is_closed());
        assert_eq!(
            pool.used(),
            0,
            "a pending pull leaves no backend in its return slot"
        );
        assert!(matches!(
            session.prepare(&cx, "MATCH (n) RETURN n", &args),
            Err(QueryError::Authorization(Error::ExecutionStopped))
        ));
        vfs.pause.store(false, Ordering::SeqCst);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn last_row_source_drop_precedes_live_delivery_and_output_reservations_outlive_session() {
    let ((), report) = run_async_under_lab(0xa6_0106, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c
            .query()
            .with_host_clock_interval(std::num::NonZeroU32::new(1024).unwrap());
        let (vfs, mut db, _, _) = fixture(&commit, false).await;
        let mut large = WriteBatch::new(R);
        large.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::bytes(vec![0x7a; 4096]).unwrap()),
        );
        let at = db.write(&commit, large).await.unwrap();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let args = GqlParameters::new();
        let text = "MATCH (n) RETURN n,n.score LIMIT 1";
        let calls = Arc::new(AtomicUsize::new(0));
        let clock_calls = calls.clone();
        let pool = memory();
        let mut first = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            at,
            move || {
                clock_calls.fetch_add(1, Ordering::SeqCst);
                100
            },
        )
        .await;
        let prepared = first.prepare(&cx, text, &args).unwrap();
        let mut cursor = first.stream(&cx, &prepared, &args).unwrap();
        calls.store(0, Ordering::SeqCst);
        let held = cursor.next().await.unwrap().unwrap();
        let final_call = calls.load(Ordering::SeqCst);
        assert!(final_call > 0);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        assert!(
            !first.is_closed(),
            "successful final native source Drop restores the session"
        );
        let retained = held.values().to_vec();
        drop(first);
        assert!(
            pool.used() >= 4096,
            "large row owns copy and evaluator reservations"
        );
        assert_eq!(held.values(), retained.as_slice());
        drop(held);
        assert_eq!(pool.used(), 0);

        let calls = Arc::new(AtomicUsize::new(0));
        let threshold = Arc::new(AtomicUsize::new(usize::MAX));
        let clock_calls = calls.clone();
        let clock_threshold = threshold.clone();
        let mut expiring = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            at,
            move || {
                let call = clock_calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call >= clock_threshold.load(Ordering::SeqCst) {
                    1001
                } else {
                    100
                }
            },
        )
        .await;
        let prepared = expiring.prepare(&cx, text, &args).unwrap();
        let mut cursor = expiring.stream(&cx, &prepared, &args).unwrap();
        calls.store(0, Ordering::SeqCst);
        threshold.store(final_call, Ordering::SeqCst);
        assert!(matches!(
            cursor.next().await.unwrap(),
            Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                Error::Expired
            )))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert!(expiring.is_closed());
        assert_eq!(
            pool.used(),
            0,
            "late expiry discards parked backend and private output"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
