//! Public cold ordered sessions retain the native relational owners, live
//! authority, one affine source and both input/output memory reservations.
#![recursion_limit = "256"]

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    AuthorizedAggregateSpoolCursor, AuthorizedBufferedError, AuthorizedBufferedReadSource,
    AuthorizedOrderedReadSession, AuthorizedSortLimits, AuthorizedSpoolCursor, BufferLimits,
    BufferedReadLimits, Database, DatabaseKeys, MemVfs, MemVfsFile, MemoryPool, NativeSpoolError,
    PreparedNativeRead, QueryError, WriteBatch,
};
use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::scan_stream::ScanState;
use fgdb_gql::symbol_bindings::CatalogSymbolResolver;
use fgdb_gql::{
    GqlMapArgumentText, GqlParameters, GqlQueryPolicy, GraphAggregateTextSlot, GraphAggregateValue,
    GraphExactAverage, GraphSymbol, GraphSymbolKind, GraphSymbolResolver, PreparedGraphBranchText,
    ReverseSymbolCatalog,
};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use fgdb_warden::{Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Scope};
use std::future::{Future, poll_fn};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xa8; 32]);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(7);
const S: RelationId = RelationId(8);
const T: RelationId = RelationId(9);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa9; 32], NS, [0xaa; 32])
}

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0xa801), NS, "graph", SchemaEpoch(1), 1).unwrap()
}

fn grant(nodes: u64, rows: u64) -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: nodes,
            max_work: 10_000_000,
            max_rows: rows,
        },
    );
    grant.labels = Scope::only([L]);
    grant.relations = Scope::only([R]);
    grant.properties = Scope::only([P]);
    grant
}

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 1_000_000, 1_000_000)
}

fn limits() -> AuthorizedSortLimits {
    AuthorizedSortLimits {
        group_capacity: 2,
        max_partitions: 32,
        run_rows: 2,
        max_runs: 32,
        page_bytes: 31,
        max_row_bytes: 16 * 1024,
        max_input_rows: 100,
        max_sort_work: 1_000_000,
    }
}

fn cold_limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 128,
        max_vertex_patches: 128,
        max_work: 100_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

fn memory() -> MemoryPool {
    MemoryPool::new(32 * 1024 * 1024, 0).unwrap()
}

struct HostCatalog {
    catalog: SymbolCatalog,
    lookups: Arc<AtomicUsize>,
}

impl GraphSymbolResolver for HostCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        CatalogSymbolResolver::new(&self.catalog).resolve_symbol(kind, name)
    }
    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        CatalogSymbolResolver::new(&self.catalog).time_zones()
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        CatalogSymbolResolver::new(&self.catalog).reverse_catalog()
    }
    fn reverse_label(&self, id: LabelId) -> Option<String> {
        CatalogSymbolResolver::new(&self.catalog).reverse_label(id)
    }
    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        CatalogSymbolResolver::new(&self.catalog).reverse_relation(id)
    }
}

#[derive(Clone, Debug)]
struct GateVfs {
    memory: MemVfs,
    reads: Arc<AtomicUsize>,
    pause: Arc<AtomicBool>,
}

impl GateVfs {
    fn new() -> Self {
        Self {
            memory: MemVfs::new().unwrap(),
            reads: Arc::default(),
            pause: Arc::default(),
        }
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
        if path
            .components()
            .any(|part| part.as_os_str() == "strata-blocks")
        {
            self.reads.fetch_add(1, Ordering::SeqCst);
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
            ("T", CatalogSymbol::Relation(T)),
            ("score", CatalogSymbol::Property(P)),
            ("secret", CatalogSymbol::Property(SECRET)),
        ])
        .unwrap();
    db.register_symbols(cx, &catalog).await.unwrap();
    let mut birth = WriteBatch::new(R);
    for (id, score) in [(1, 3), (2, 1), (3, 3), (4, -1), (5, 2)] {
        birth.create_vertex(
            VId(id),
            vec![L, H],
            vec![
                (P, CanonicalScalar::Int(score)),
                (SECRET, CanonicalScalar::Int(999)),
            ],
        );
    }
    for (id, from, to, score) in [(101, 1, 2, 7), (102, 1, 2, 9), (103, 2, 3, 5)] {
        birth.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![
                (P, CanonicalScalar::Int(score)),
                (SECRET, CanonicalScalar::Int(999)),
            ],
        );
    }
    if hidden {
        for id in 10..18 {
            birth.create_vertex(VId(id), vec![H], vec![(P, CanonicalScalar::Int(999))]);
            birth.add_edge(
                EId(id),
                VId(id),
                VId(1),
                vec![(P, CanonicalScalar::Int(999))],
            );
        }
    }
    let old = db.write(cx, birth).await.unwrap();
    let mut edit = WriteBatch::new(R);
    edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(30)));
    edit.set_edge_property(EId(101), P, Some(CanonicalScalar::Int(17)));
    db.write(cx, edit).await.unwrap();
    let mut denied = WriteBatch::new(S);
    denied.add_edge(
        EId(201),
        VId(1),
        VId(2),
        vec![(P, CanonicalScalar::Int(999))],
    );
    db.write(cx, denied).await.unwrap();
    let mut third = WriteBatch::new(T);
    third.add_edge(
        EId(301),
        VId(1),
        VId(2),
        vec![
            (P, CanonicalScalar::Int(13)),
            (SECRET, CanonicalScalar::Int(999)),
        ],
    );
    if hidden {
        third.add_edge(
            EId(302),
            VId(10),
            VId(1),
            vec![(P, CanonicalScalar::Int(999))],
        );
    }
    let current = db.write(cx, third).await.unwrap();
    (vfs, db, old, current)
}

#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    operations: usize,
    pending_read: bool,
    pending_write: bool,
    pending_on_payload: Option<&'static [u8]>,
    expire_after_read: Option<Arc<AtomicU64>>,
}

#[derive(Clone, Default)]
struct File(Arc<Mutex<FileState>>);

impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.operations += 1;
        if state.pending_read {
            return Poll::Pending;
        }
        let at = state.bytes.position() as usize;
        let count = out
            .remaining()
            .min(state.bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + count]);
            state.bytes.set_position((at + count) as u64);
        }
        if let Some(now) = &state.expire_after_read {
            now.store(1001, Ordering::SeqCst);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        state.operations += 1;
        if state
            .pending_on_payload
            .is_some_and(|payload| bytes.windows(payload.len()).any(|part| part == payload))
        {
            state.pending_write = true;
        }
        if state.pending_write {
            Poll::Pending
        } else {
            Poll::Ready(std::io::Write::write(&mut state.bytes, bytes))
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for File {
    fn poll_seek(self: Pin<&mut Self>, _: &mut Context<'_>, at: SeekFrom) -> Poll<io::Result<u64>> {
        let mut state = self.0.lock().unwrap();
        state.operations += 1;
        Poll::Ready(state.bytes.seek(at))
    }
}

async fn file(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let backing = File::default();
    let file = SpillFile::new(
        cx,
        backing.clone(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 16_000_000,
            max_runs: 1024,
            max_run_bytes: 4_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
async fn session<'a, C: FnMut() -> u64 + Send>(
    commit: &CommitCx,
    cx: &QueryCx,
    vfs: &GateVfs,
    pool: &MemoryPool,
    authority: &'a Authority,
    token: &CapabilityToken,
    at: CommitSeq,
    clock: C,
    policy: GqlQueryPolicy,
) -> (
    AuthorizedOrderedReadSession<
        'a,
        impl GraphSymbolResolver + use<C>,
        C,
        File,
        File,
        File,
        AuthorizedBufferedReadSource<GateVfs>,
    >,
    [File; 3],
) {
    let (scratch, a) = file(cx, pool).await;
    let (destination, b) = file(cx, pool).await;
    let (auxiliary, c) = file(cx, pool).await;
    let ordered = Database::open_buffered_publication_with_vfs(
        commit,
        vfs.clone(),
        vfs.memory.database_dir(),
        keys(),
        pool.clone(),
        cold_limits(),
    )
    .await
    .unwrap()
    .into_authorized_ordered_read_session(
        commit,
        cx,
        authority,
        token,
        "main",
        at,
        policy,
        clock,
        scratch,
        destination,
        auxiliary,
        limits(),
    )
    .await
    .unwrap();
    (ordered, [a, b, c])
}

fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

fn assert_send<T: Send>(_: &T) {}

fn observations(files: &[File; 3]) -> [usize; 3] {
    files
        .each_ref()
        .map(|file| file.0.lock().unwrap().operations)
}

async fn values(
    cursor: &mut AuthorizedSpoolCursor<'_, File, AuthorizedBufferedReadSource<GateVfs>>,
) -> Vec<Vec<GraphValue>> {
    let mut rows = Vec::new();
    while let Some(bytes) = cursor.next_row().await.unwrap() {
        rows.push(
            GraphValueRow::decode_canonical(bytes.as_ref())
                .unwrap()
                .values()
                .to_vec(),
        );
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    rows
}

async fn aggregates(
    cursor: &mut AuthorizedAggregateSpoolCursor<'_, File, AuthorizedBufferedReadSource<GateVfs>>,
) -> Vec<Vec<GraphAggregateValue>> {
    let slots = cursor.output_slots().to_vec();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next_row().await.unwrap() {
        rows.push(
            slots
                .iter()
                .map(|slot| match *slot {
                    GraphAggregateTextSlot::GroupKey(at) => {
                        GraphAggregateValue::Value(row.keys()[at].clone())
                    }
                    GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                })
                .collect(),
        );
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    rows
}

#[test]
fn cold_ordered_host_templates_rebind_maps_and_history_without_reparsing() {
    check_ordered_template(
        "AT BRANCH $route MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut \
         WHERE n.score >= $filter.minimum RETURN n,n.score AS score \
         ORDER BY score DESC,n LIMIT $page",
        false,
        [&[(1, 3), (3, 3), (5, 2)], &[(1, 30), (3, 3)], &[(1, 3)]],
    );
    check_ordered_template(
        "AT BRANCH $route MATCH (n:L {score: $filter.minimum}) FOR SYSTEM_TIME AS OF SEQ $cut \
         RETURN n,n.score AS score ORDER BY score DESC,n LIMIT $page",
        true,
        [&[(5, 2)], &[(3, 3)], &[(2, 1)]],
    );
}

fn check_ordered_template(
    text: &'static str,
    expect_splice: bool,
    expected: [&'static [(u128, i64)]; 3],
) {
    let ((), report) = run_async_under_lab(0xa8_0209, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, db, old, current) = fixture(&commit, true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(5, 3), 100).unwrap();
        let pool = memory();
        let catalog = db.symbol_catalog_at(old).unwrap();
        let lookups = Arc::new(AtomicUsize::new(0));
        let arguments = |at: CommitSeq, minimum, page| {
            GqlParameters::new()
                .with_text("route", "main")
                .unwrap()
                .with_uint64("cut", at.0)
                .unwrap()
                .with_uint64("page", page)
                .unwrap()
                .with_map("filter", vec![("minimum".into(), scalar(minimum))])
                .unwrap()
        };
        let args = arguments(old, 2, 3);
        let lowered = GqlMapArgumentText::lower_fields(text, &args);
        assert_eq!(lowered.is_some(), expect_splice);
        let (statement, derived) = lowered
            .as_ref()
            .map_or((text, &args), |(fields, derived)| (fields.text(), derived));
        let selector = Arc::new(PreparedGraphBranchText::prepare(statement).unwrap());
        let native = {
            let selected = selector.bind_parameters(derived).unwrap();
            Arc::new(
                PreparedNativeRead::prepare(
                    selected.statement(),
                    selected.parameters(),
                    HostCatalog {
                        catalog: catalog.clone(),
                        lookups: lookups.clone(),
                    },
                )
                .unwrap(),
            )
        };
        let lowered = lowered.map(|(fields, _)| Arc::new(fields));
        let bound_lookups = lookups.load(Ordering::SeqCst);
        assert!(bound_lookups > 0);
        let mut foreign = None;
        for ((at, minimum, page), expected) in [(old, 2, 3), (current, 3, 2), (old, 1, 1)]
            .into_iter()
            .zip(expected)
        {
            let args = arguments(at, minimum, page);
            let (scratch, a) = file(&cx, &pool).await;
            let (destination, b) = file(&cx, &pool).await;
            let (auxiliary, c) = file(&cx, &pool).await;
            let files = [a, b, c];
            let (mut ordered, prepared) = db
                .buffered_read_publication(&cx, pool.clone(), cold_limits())
                .unwrap()
                .into_authorized_prepared_ordered_read_session_with_resolver(
                    &commit,
                    &cx,
                    &authority,
                    &token,
                    "main",
                    at,
                    policy(3),
                    || 100,
                    |selected| {
                        if selected != catalog {
                            return Err("prepared catalog changed".into());
                        }
                        Ok(HostCatalog {
                            catalog: selected,
                            lookups: lookups.clone(),
                        })
                    },
                    selector.clone(),
                    native.clone(),
                    lowered.clone(),
                    scratch,
                    destination,
                    auxiliary,
                    limits(),
                )
                .await
                .unwrap();
            if let Some(foreign) = &foreign {
                let before = observations(&files);
                vfs.reset();
                assert!(matches!(ordered.execute_ordered(&cx, foreign, &args),
                    Err(NativeSpoolError::Authorized(error))
                        if matches!(*error, QueryError::Authorization(Error::WrongAuthority))));
                assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
                assert_eq!(observations(&files), before);
                assert!(!ordered.is_closed());
            }
            let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
            assert_send(&execution);
            assert_eq!(execution.snapshot_seq(), at);
            let mut cursor = execution.await.unwrap();
            assert_eq!(
                values(&mut cursor).await,
                expected
                    .iter()
                    .map(|&(id, score)| vec![GraphValue::Vertex(VId(id)), scalar(score)])
                    .collect::<Vec<_>>()
            );
            drop(cursor);
            assert_eq!(lookups.load(Ordering::SeqCst), bound_lookups);
            let other = if at == old { current } else { old };
            let before = observations(&files);
            vfs.reset();
            assert!(matches!(
                ordered.execute_ordered(&cx, &prepared, &arguments(other, minimum, page)),
                Err(NativeSpoolError::BufferedSession(error))
                    if matches!(*error, AuthorizedBufferedError::DifferentCut { requested, bound }
                        if requested == other && bound == at)
            ));
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
            assert_eq!(observations(&files), before);
            drop(ordered);
            assert_eq!(
                pool.used(),
                0,
                "a retained template does not retain its source"
            );
            foreign = Some(prepared);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_ordered_catalog_admission_and_live_expiry_precede_invalid_sort_limits() {
    let ((), report) = run_async_under_lab(0xa8_0210, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, db, at, _) = fixture(&commit, true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(5, 3), 100).unwrap();
        let pool = memory();
        vfs.reset();
        let selected = db
            .buffered_read_publication(&cx, pool.clone(), cold_limits())
            .unwrap()
            .symbol_catalog(&commit)
            .await
            .unwrap()
            .symbol_catalog_at(at)
            .unwrap();
        let catalog_reads = vfs.reads.load(Ordering::SeqCst);
        assert!(catalog_reads > 0);
        for scenario in 0..3 {
            let now = Arc::new(AtomicU64::new(if scenario == 0 { 1001 } else { 100 }));
            let clock = Arc::clone(&now);
            let calls = Arc::new(AtomicUsize::new(0));
            let (scratch, a) = file(&cx, &pool).await;
            let (destination, b) = file(&cx, &pool).await;
            let (auxiliary, c) = file(&cx, &pool).await;
            let files = [a, b, c];
            let before = observations(&files);
            let mut invalid = limits();
            invalid.run_rows = 0;
            vfs.reset();
            let result = db
                .buffered_read_publication(&cx, pool.clone(), cold_limits())
                .unwrap()
                .into_authorized_ordered_read_session_with_resolver(
                    &commit,
                    &cx,
                    &authority,
                    &token,
                    "main",
                    at,
                    policy(3),
                    move || clock.load(Ordering::SeqCst),
                    |catalog| -> Result<HostCatalog, String> {
                        assert_eq!(catalog, selected);
                        calls.fetch_add(1, Ordering::SeqCst);
                        if scenario == 2 {
                            now.store(1001, Ordering::SeqCst);
                        }
                        Err("prepared catalog changed".into())
                    },
                    scratch,
                    destination,
                    auxiliary,
                    invalid,
                )
                .await;
            if scenario == 1 {
                assert!(
                    matches!(result, Err(NativeSpoolError::BufferedSession(error))
                    if matches!(*error, AuthorizedBufferedError::CatalogBinding(_)))
                );
            } else {
                assert!(matches!(result, Err(NativeSpoolError::Authorized(error))
                    if matches!(*error, QueryError::Authorization(Error::Expired))));
            }
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(scenario != 0));
            assert_eq!(
                vfs.reads.load(Ordering::SeqCst),
                if scenario == 0 { 0 } else { catalog_reads }
            );
            assert_eq!(observations(&files), before);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_relations_keep_masked_bags_scalar_stages_and_selected_products() {
    let ((), report) = run_async_under_lab(0xa8_0201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        // UNION reads five visible vertices twice. Hidden physical candidates
        // must not change this signed boundary, or the final three-row limit.
        let token = authority.issue_at(&grant(10, 3), 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, at, _) = fixture(&commit, hidden).await;
            let pool = memory();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                policy(3),
            )
            .await;
            for (text, expected) in [
                (
                    "MATCH (n:L) RETURN n.score AS score,n.secret AS hidden \
                     ORDER BY score DESC SKIP 1 LIMIT 2",
                    vec![vec![scalar(3), null()], vec![scalar(2), null()]],
                ),
                (
                    "MATCH (n:L) WITH n.score AS score ORDER BY score DESC SKIP 1 LIMIT 2 \
                     RETURN score",
                    vec![vec![scalar(2)], vec![scalar(3)]],
                ),
                (
                    "MATCH (n:L) RETURN n,n.score AS score,n.secret AS hidden \
                     ORDER BY score DESC,n SKIP 1 LIMIT 2",
                    vec![
                        vec![GraphValue::Vertex(VId(3)), scalar(3), null()],
                        vec![GraphValue::Vertex(VId(5)), scalar(2), null()],
                    ],
                ),
                (
                    "MATCH (n:L) WITH n,n.score AS score \
                     UNWIND [score,score+10] AS expanded \
                     RETURN expanded ORDER BY expanded DESC SKIP 1 LIMIT 3",
                    vec![vec![scalar(13)], vec![scalar(12)], vec![scalar(11)]],
                ),
                (
                    "MATCH (n:L) WITH n.score AS score,count(*) AS rows \
                     RETURN score,rows ORDER BY rows DESC,score LIMIT 3",
                    vec![
                        vec![scalar(3), scalar(2)],
                        vec![scalar(-1), scalar(1)],
                        vec![scalar(1), scalar(1)],
                    ],
                ),
                (
                    "MATCH (a)-[r:R]->(b) RETURN r,r.score AS score,r.secret AS hidden \
                     ORDER BY score DESC",
                    vec![
                        vec![GraphValue::Edge(EId(102)), scalar(9), null()],
                        vec![GraphValue::Edge(EId(101)), scalar(7), null()],
                        vec![GraphValue::Edge(EId(103)), scalar(5), null()],
                    ],
                ),
                (
                    "MATCH (n:L) RETURN n,n.score AS score UNION ALL \
                     MATCH (n:L) RETURN n,n.score AS score \
                     ORDER BY score DESC,n SKIP 2 LIMIT 3",
                    vec![
                        vec![GraphValue::Vertex(VId(3)), scalar(3)],
                        vec![GraphValue::Vertex(VId(3)), scalar(3)],
                        vec![GraphValue::Vertex(VId(5)), scalar(2)],
                    ],
                ),
                (
                    "UNWIND [2,1,2] AS wanted MATCH (n:L) \
                     WITH wanted,n.score AS score WHERE wanted=score \
                     RETURN wanted,score ORDER BY wanted DESC",
                    vec![
                        vec![scalar(2), scalar(2)],
                        vec![scalar(2), scalar(2)],
                        vec![scalar(1), scalar(1)],
                    ],
                ),
                (
                    "MATCH (n:L) WITH n,n.score AS score \
                     CALL(score) { UNWIND [score,score+1] AS value \
                     RETURN value ORDER BY value DESC LIMIT 1 } \
                     RETURN n,value ORDER BY value DESC,n LIMIT 2",
                    vec![
                        vec![GraphValue::Vertex(VId(1)), scalar(4)],
                        vec![GraphValue::Vertex(VId(3)), scalar(4)],
                    ],
                ),
            ] {
                let prepared = ordered.prepare(&cx, text, &args).unwrap();
                let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
                assert_send(&execution);
                assert_eq!(execution.snapshot_seq(), at);
                let columns = execution.columns().to_vec();
                let mut cursor = execution.await.unwrap();
                assert_send(&cursor);
                assert_eq!(cursor.columns(), columns);
                assert_eq!(
                    values(&mut cursor).await,
                    expected,
                    "{text}; hidden={hidden}"
                );
                drop(cursor);
                assert!(
                    !ordered.is_closed(),
                    "successful source returns to the same session"
                );
            }
            drop(ordered);
            assert_eq!(pool.used(), 0);

            let tight = authority.issue_at(&grant(4, 3), 100).unwrap();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &tight,
                at,
                || 100,
                policy(3),
            )
            .await;
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH (n:L) RETURN n,n.score AS score ORDER BY score DESC LIMIT 1",
                    &args,
                )
                .unwrap();
            let error = ordered
                .execute_ordered(&cx, &prepared, &args)
                .unwrap()
                .await
                .unwrap_err();
            assert!(matches!(error, NativeSpoolError::Authorized(error)
                if matches!(*error, QueryError::Authorization(Error::LimitExceeded(LimitDimension::Nodes)))));
            assert!(
                !ordered.is_closed(),
                "a query allowance refusal is not lifetime revocation"
            );
            drop(ordered);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_aggregates_keep_exact_cells_complete_groups_and_empty_input_semantics() {
    use GraphAggregateValue::{Average, Count, Integer, Value};
    let ((), report) = run_async_under_lab(0xa8_0202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(10, 2), 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, at, _) = fixture(&commit, hidden).await;
            let pool = memory();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                policy(2),
            )
            .await;
            // Five input occurrences and four groups exceed both the final
            // two-row allowance and the two-group in-memory reducer capacity.
            for (text, expected) in [
                (
                    "MATCH (n:L) RETURN count(*) AS rows,sum(n.score) AS total, \
                     avg(n.score) AS mean,collect(n.score) AS items,count(n.secret) AS hidden",
                    vec![vec![
                        Count(5),
                        Integer(8),
                        Average(GraphExactAverage::new(8, 5).unwrap()),
                        Value(GraphValue::List(
                            [3, 1, 3, -1, 2].into_iter().map(scalar).collect(),
                        )),
                        Count(0),
                    ]],
                ),
                (
                    "MATCH (n:L) RETURN n.score AS score,count(*) AS rows \
                     GROUP BY n.score HAVING sum(n.score)>0 \
                     ORDER BY rows DESC,score SKIP 1 LIMIT 2",
                    vec![
                        vec![Value(scalar(1)), Count(1)],
                        vec![Value(scalar(2)), Count(1)],
                    ],
                ),
                (
                    "MATCH (a)-[r:R]->(b) RETURN count(*) AS rows,sum(r.score) AS total, \
                     avg(r.score) AS mean,count(r.secret) AS hidden",
                    vec![vec![
                        Count(3),
                        Integer(21),
                        Average(GraphExactAverage::new(7, 1).unwrap()),
                        Count(0),
                    ]],
                ),
                (
                    "MATCH (n:L) WHERE n.score>100 RETURN count(*) AS rows, \
                     sum(n.score) AS total,avg(n.score) AS mean,collect(n.score) AS items",
                    vec![vec![
                        Count(0),
                        Value(null()),
                        Value(null()),
                        Value(GraphValue::List(Box::new([]))),
                    ]],
                ),
                (
                    "MATCH (n:L) WHERE n.score>100 RETURN n.score AS score,count(*) AS rows GROUP BY n.score",
                    vec![],
                ),
                (
                    "UNWIND [9223372036854775807,9223372036854775807,2] AS value \
                     RETURN count(*) AS rows,sum(value) AS total,avg(value) AS mean",
                    vec![vec![
                        Count(3),
                        Integer(1_i128 << 64),
                        Average(GraphExactAverage::new(1_i128 << 64, 3).unwrap()),
                    ]],
                ),
            ] {
                let prepared = ordered.prepare(&cx, text, &args).unwrap();
                let execution = ordered
                    .execute_ordered_aggregate(&cx, &prepared, &args)
                    .unwrap();
                assert_send(&execution);
                let mut cursor = execution.await.unwrap();
                assert_send(&cursor);
                assert_eq!(cursor.snapshot_seq(), at);
                assert_eq!(
                    aggregates(&mut cursor).await,
                    expected,
                    "{text}; hidden={hidden}"
                );
                drop(cursor);
                assert!(!ordered.is_closed());
            }
            drop(ordered);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_endpoint_fields_compose_with_sorting_scalar_stages_and_exact_aggregates() {
    use GraphAggregateValue::{Count, Integer, Value};
    let ((), report) = run_async_under_lab(0xa8_0210, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let authority = issuer();
        // Three physical edges admit their two endpoints once each. The
        // hidden fixture must retain this exact visible-node boundary.
        let token = authority.issue_at(&grant(6, 3), 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixture(&commit, hidden).await;
            for (at, source) in [(old, 3), (current, 30)] {
                let pool = memory();
                let (mut ordered, _) = session(
                    &commit,
                    &cx,
                    &vfs,
                    &pool,
                    &authority,
                    &token,
                    at,
                    || 100,
                    policy(3),
                )
                .await;
                for (text, expected) in [
                    (
                        "MATCH (a)-[r:R]->(b) RETURN r,a.score AS origin,b.score AS destination, \
                         a.secret AS hidden ORDER BY destination DESC,origin,r",
                        vec![
                            vec![GraphValue::Edge(EId(103)), scalar(1), scalar(3), null()],
                            vec![
                                GraphValue::Edge(EId(101)),
                                scalar(source),
                                scalar(1),
                                null(),
                            ],
                            vec![
                                GraphValue::Edge(EId(102)),
                                scalar(source),
                                scalar(1),
                                null(),
                            ],
                        ],
                    ),
                    (
                        "MATCH (a)-[r:R]->(b) WHERE a.score>b.score AND b.secret IS NULL \
                         RETURN r,a.score,b.score ORDER BY r DESC",
                        vec![
                            vec![GraphValue::Edge(EId(102)), scalar(source), scalar(1)],
                            vec![GraphValue::Edge(EId(101)), scalar(source), scalar(1)],
                        ],
                    ),
                    (
                        "MATCH (a)-[r:R]->(b) WITH a.score AS origin,b.score AS destination \
                         RETURN origin+destination AS score ORDER BY score DESC",
                        vec![
                            vec![scalar(source + 1)],
                            vec![scalar(source + 1)],
                            vec![scalar(4)],
                        ],
                    ),
                    (
                        "MATCH (a)-[r:R]->(b) RETURN r,b.score AS score LIMIT 0",
                        vec![],
                    ),
                ] {
                    let prepared = ordered.prepare(&cx, text, &args).unwrap();
                    let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
                    assert_send(&execution);
                    let mut cursor = execution.await.unwrap();
                    assert_eq!(cursor.snapshot_seq(), at);
                    assert_eq!(
                        values(&mut cursor).await,
                        expected,
                        "{text}; hidden={hidden}"
                    );
                    drop(cursor);
                    assert!(!ordered.is_closed());
                }
                for (text, expected) in [
                    (
                        "MATCH (a)-[r:R]->(b) RETURN a.score AS origin,count(*) AS edges, \
                         sum(b.score) AS total GROUP BY a.score ORDER BY origin DESC",
                        vec![
                            vec![Value(scalar(source)), Count(2), Integer(2)],
                            vec![Value(scalar(1)), Count(1), Integer(3)],
                        ],
                    ),
                    (
                        "MATCH (a)-[r:R]->(b) RETURN sum(a.score) AS total, \
                         collect(b.score) AS destinations,count(a.secret) AS hidden",
                        vec![vec![
                            Integer(i128::from(2 * source + 1)),
                            // Captured-edge input orders complete projected rows.
                            // The tuple with origin 1 and destination 3 comes first.
                            Value(GraphValue::List(Box::new([
                                scalar(3),
                                scalar(1),
                                scalar(1),
                            ]))),
                            Count(0),
                        ]],
                    ),
                ] {
                    let prepared = ordered.prepare(&cx, text, &args).unwrap();
                    let execution = ordered
                        .execute_ordered_aggregate(&cx, &prepared, &args)
                        .unwrap();
                    assert_send(&execution);
                    let mut cursor = execution.await.unwrap();
                    assert_eq!(cursor.snapshot_seq(), at);
                    assert_eq!(
                        aggregates(&mut cursor).await,
                        expected,
                        "{text}; hidden={hidden}"
                    );
                    drop(cursor);
                    assert!(!ordered.is_closed());
                }
                drop(ordered);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_effect_and_cut_admission_precedes_payloads_and_scratch_even_under_empty_pages() {
    let ((), report) = run_async_under_lab(0xa8_0203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, _db, old, current) = fixture(&commit, true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(0, 2), 100).unwrap();
        let pool = memory();
        let (mut ordered, files) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            old,
            || 100,
            policy(2),
        )
        .await;
        let args = GqlParameters::new();
        for (text, aggregate) in [
            (
                "MATCH (n:L) RETURN n,properties(n) AS fields LIMIT 0",
                false,
            ),
            (
                "MATCH (n:L) WITH n AS item RETURN properties([item][0]) AS fields LIMIT 0",
                false,
            ),
            (
                "MATCH (n:L) WITH n AS item RETURN properties(item) AS fields LIMIT 0",
                false,
            ),
            (
                "MATCH (n:L) WITH n AS item RETURN CASE WHEN true THEN {} ELSE properties([item][0]) END AS fields LIMIT 0",
                false,
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN r,properties(b) AS fields LIMIT 0",
                false,
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN a,r,b,s,c,properties(c) LIMIT 0",
                false,
            ),
            ("MATCH (a)-[r:R*1..2]->(b) RETURN a,b LIMIT 0", false),
            (
                "UNWIND [] AS item CALL() { MATCH (n:L) WITH n AS carried RETURN properties([carried][0]) AS fields } RETURN fields LIMIT 0",
                false,
            ),
            (
                "MATCH (n:L) RETURN collect(n)[0].score AS score LIMIT 0",
                true,
            ),
        ] {
            let prepared = ordered.prepare(&cx, text, &args).unwrap();
            let before = observations(&files);
            let bytes = pool.used();
            vfs.reset();
            let refused = if aggregate {
                ordered
                    .execute_ordered_aggregate(&cx, &prepared, &args)
                    .map(|_| ())
            } else {
                ordered.execute_ordered(&cx, &prepared, &args).map(|_| ())
            };
            assert!(
                matches!(refused, Err(NativeSpoolError::Authorized(error))
                if matches!(*error, QueryError::StreamingUnsupported { .. })),
                "{text}"
            );
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0, "{text}");
            assert_eq!(observations(&files), before, "{text}");
            assert_eq!(pool.used(), bytes, "{text}");
            assert!(!ordered.is_closed());
        }
        // The selected endpoint leaf is supported before execution, even
        // with no node allowance. An unpolled external execution is inert;
        // once polled, its private input still owns the ordinary node quota.
        let endpoint = ordered
            .prepare(
                &cx,
                "MATCH (a)-[r:R]->(b) RETURN r,b.score AS score LIMIT 0",
                &args,
            )
            .unwrap();
        vfs.reset();
        let before = observations(&files);
        let execution = ordered.execute_ordered(&cx, &endpoint, &args).unwrap();
        drop(execution);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert_eq!(observations(&files), before);
        assert!(!ordered.is_closed());
        // The original fixed-hop LIMIT-zero input is now supported. It still
        // cannot suppress admission failures once execution is polled.
        let joined = ordered
            .prepare(
                &cx,
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN a,r,b,s,c LIMIT 0",
                &args,
            )
            .unwrap();
        let before = observations(&files);
        let bytes = pool.used();
        let execution = ordered.execute_ordered(&cx, &joined, &args).unwrap();
        drop(execution);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert_eq!(observations(&files), before);
        assert_eq!(pool.used(), bytes);
        assert!(matches!(
            ordered.execute_ordered(&cx, &joined, &args).unwrap().await,
            Err(NativeSpoolError::Authorized(error))
                if matches!(*error, QueryError::Authorization(
                    Error::LimitExceeded(LimitDimension::Nodes)
                ))
        ));
        assert!(!ordered.is_closed());

        let selected = GqlParameters::new().with_uint64("cut", current.0).unwrap();
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n,n.score ORDER BY n",
                &selected,
            )
            .unwrap();
        let before = observations(&files);
        vfs.reset();
        assert!(matches!(ordered.execute_ordered(&cx, &prepared, &selected),
            Err(NativeSpoolError::BufferedSession(error))
                if matches!(*error, AuthorizedBufferedError::DifferentCut { requested, bound }
                    if requested == current && bound == old)));
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert_eq!(observations(&files), before);
        assert!(!ordered.is_closed());
        // The exact same owner is still usable after all ordinary refusals;
        // a denied relation has no visible candidates despite max_nodes zero.
        let denied = ordered
            .prepare(
                &cx,
                "MATCH (a)-[r:S]->(b) RETURN r,r.score ORDER BY r",
                &args,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &denied, &args)
            .unwrap()
            .await
            .unwrap();
        assert!(values(&mut cursor).await.is_empty());
        drop(cursor);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        drop(ordered);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_pending_source_scratch_and_completed_output_drops_close_the_affine_session() {
    let ((), report) = run_async_under_lab(0xa8_0204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(10, 10), 100).unwrap();
        let args = GqlParameters::new();
        for phase in 0..3 {
            let (vfs, _db, at, _) = fixture(&commit, false).await;
            let pool = memory();
            let (mut ordered, files) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                policy(10),
            )
            .await;
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH (n:L) RETURN n,n.score ORDER BY n LIMIT 1",
                    &args,
                )
                .unwrap();
            let before = observations(&files);
            let baseline = pool.used();
            vfs.reset();
            let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
            drop(execution);
            assert_eq!(observations(&files), before);
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
            assert_eq!(pool.used(), baseline);
            assert!(
                !ordered.is_closed(),
                "unpolled drop restores the source without I/O"
            );

            if phase == 2 {
                let mut cursor = ordered
                    .execute_ordered(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                for file in &files {
                    file.0.lock().unwrap().pending_read = true;
                }
                let mut pending = Box::pin(cursor.next_row());
                poll_fn(|task| {
                    assert!(pending.as_mut().poll(task).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(pending);
                assert_eq!(cursor.state(), ScanState::Failed);
                assert!(cursor.next_row().await.unwrap().is_none());
                drop(cursor);
            } else {
                if phase == 0 {
                    vfs.pause.store(true, Ordering::SeqCst);
                } else {
                    for file in &files {
                        file.0.lock().unwrap().pending_write = true;
                    }
                }
                let mut pending = Box::pin(ordered.execute_ordered(&cx, &prepared, &args).unwrap());
                poll_fn(|task| {
                    assert!(pending.as_mut().poll(task).is_pending());
                    Poll::Ready(())
                })
                .await;
                if phase == 0 {
                    assert!(vfs.reads.load(Ordering::SeqCst) > 0);
                } else {
                    assert_ne!(observations(&files), before);
                }
                drop(pending);
            }
            assert!(ordered.is_closed(), "phase {phase}");
            assert_eq!(
                pool.used(),
                0,
                "phase {phase}: no parked source or private row remains"
            );
            assert!(matches!(
                ordered.prepare(&cx, "RETURN 1 AS value", &args),
                Err(QueryError::Authorization(Error::ExecutionStopped))
            ));
            vfs.pause.store(false, Ordering::SeqCst);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_final_delivery_keeps_row_limits_live_expiry_and_retained_output_reservations() {
    let ((), report) = run_async_under_lab(0xa8_0205, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, _db, old, current) = fixture(&commit, true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(10, 2), 100).unwrap();
        let args = GqlParameters::new();
        let pool = memory();
        let text = "MATCH (n:L) RETURN n,n.score AS score ORDER BY n";
        let (mut ordered, _) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            old,
            || 100,
            policy(10),
        )
        .await;
        let prepared = ordered.prepare(&cx, text, &args).unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &args)
            .unwrap()
            .await
            .unwrap();
        for (id, score) in [(1, 3), (2, 1)] {
            let row = cursor.next_row().await.unwrap().unwrap();
            assert_eq!(
                GraphValueRow::decode_canonical(row.as_ref())
                    .unwrap()
                    .values(),
                &[GraphValue::Vertex(VId(id)), scalar(score)]
            );
        }
        assert!(
            matches!(cursor.next_row().await, Err(NativeSpoolError::Authorized(error))
            if matches!(*error, QueryError::Authorization(Error::LimitExceeded(LimitDimension::Rows))))
        );
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(!ordered.is_closed());
        let selected = ordered
            .prepare(
                &cx,
                "MATCH (n:L) RETURN n,n.score AS score ORDER BY n LIMIT 1",
                &args,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &selected, &args)
            .unwrap()
            .await
            .unwrap();
        let retained = cursor.next_row().await.unwrap().unwrap();
        assert_eq!(
            GraphValueRow::decode_canonical(retained.as_ref())
                .unwrap()
                .values(),
            &[GraphValue::Vertex(VId(1)), scalar(3)]
        );
        drop(cursor);
        drop(ordered);
        assert!(
            pool.used() >= retained.len(),
            "returned bytes own their spill-pool reservation"
        );
        drop(retained);
        assert_eq!(pool.used(), 0);

        let now = Arc::new(AtomicU64::new(100));
        let sampled = Arc::clone(&now);
        let (mut expiring, files) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            current,
            move || sampled.load(Ordering::SeqCst),
            policy(10),
        )
        .await;
        let prepared = expiring.prepare(&cx, text, &args).unwrap();
        let mut cursor = expiring
            .execute_ordered(&cx, &prepared, &args)
            .unwrap()
            .await
            .unwrap();
        // Expiration during a successful physical frame read must win before
        // exposing the frame, even though private execution already completed.
        for file in &files {
            file.0.lock().unwrap().expire_after_read = Some(Arc::clone(&now));
        }
        assert!(
            matches!(cursor.next_row().await, Err(NativeSpoolError::Authorized(error))
            if matches!(*error, QueryError::Authorization(Error::Expired)))
        );
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(expiring.is_closed());
        assert_eq!(pool.used(), 0);
        drop(expiring);

        let (mut current_session, _) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            current,
            || 100,
            policy(2),
        )
        .await;
        let prepared = current_session
            .prepare(
                &cx,
                "MATCH (n:L) RETURN sum(n.score) AS total,count(n.secret) AS hidden",
                &args,
            )
            .unwrap();
        let mut cursor = current_session
            .execute_ordered_aggregate(&cx, &prepared, &args)
            .unwrap()
            .await
            .unwrap();
        let retained = cursor.next_row().await.unwrap().unwrap();
        assert_eq!(
            retained.values(),
            &[
                GraphAggregateValue::Integer(35),
                GraphAggregateValue::Count(0)
            ]
        );
        drop(cursor);
        drop(current_session);
        assert!(
            pool.used() > 0,
            "exact aggregate output retains its decoded reservation"
        );
        assert_eq!(retained.values()[0], GraphAggregateValue::Integer(35));
        drop(retained);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_projected_payload_stays_charged_through_a_pending_spill_append() {
    let ((), report) = run_async_under_lab(0xa8_0206, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, mut db, _, _) = fixture(&commit, false).await;
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::bytes(vec![0x7a; 4096]).unwrap()),
        );
        let at = db.write(&commit, change).await.unwrap();
        let authority = issuer();
        let token = authority.issue_at(&grant(10, 1), 100).unwrap();
        let pool = memory();
        let (mut ordered, files) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            at,
            || 100,
            policy(1),
        )
        .await;
        let args = GqlParameters::new();
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n:L) RETURN n,n.score AS payload ORDER BY n LIMIT 1",
                &args,
            )
            .unwrap();
        let baseline = pool.used();
        // Canonical byte strings insert a marker after each eight-byte group.
        for file in &files {
            file.0.lock().unwrap().pending_on_payload = Some(&[0x7a; 8]);
        }
        let mut pending = Box::pin(ordered.execute_ordered(&cx, &prepared, &args).unwrap());
        let mut polls = 0;
        poll_fn(|task| {
            polls += 1;
            assert!(
                polls <= 1024,
                "the bounded fixture must reach its payload append"
            );
            assert!(pending.as_mut().poll(task).is_pending());
            if files
                .iter()
                .any(|file| file.0.lock().unwrap().pending_write)
            {
                Poll::Ready(())
            } else {
                task.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        assert!(
            pool.used() >= baseline + 4096,
            "source output, evaluator and encoded bytes stay admitted across await"
        );
        drop(pending);
        assert!(ordered.is_closed());
        assert_eq!(
            pool.used(),
            0,
            "pending drop releases source, row and all private spill buffers"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_any_and_finite_relations_keep_selector_scope_and_external_composition_distinct() {
    use GraphAggregateValue::{Average, Count, Integer, Value};
    let ((), report) = run_async_under_lab(0xa8_0207, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, _, at) = fixture(&commit, hidden).await;
            let pool = memory();
            // R, S and T all have real edges at this cut. A finite selector
            // excludes T even though the capability allows it, whereas Any
            // includes T and excludes S through the capability predicate.
            for (selector, nodes, expected, count, total, collected) in [
                (
                    "",
                    8,
                    vec![
                        vec![GraphValue::Edge(EId(101)), scalar(17), null()],
                        vec![GraphValue::Edge(EId(301)), scalar(13), null()],
                        vec![GraphValue::Edge(EId(102)), scalar(9), null()],
                        vec![GraphValue::Edge(EId(103)), scalar(5), null()],
                    ],
                    4,
                    44,
                    &[5, 9, 13, 17][..],
                ),
                (
                    ":R|S",
                    6,
                    vec![
                        vec![GraphValue::Edge(EId(101)), scalar(17), null()],
                        vec![GraphValue::Edge(EId(102)), scalar(9), null()],
                        vec![GraphValue::Edge(EId(103)), scalar(5), null()],
                    ],
                    3,
                    31,
                    &[5, 9, 17][..],
                ),
            ] {
                let mut allowed = grant(nodes, 4);
                allowed.relations = Scope::only([R, T]);
                let token = authority.issue_at(&allowed, 100).unwrap();
                let (mut ordered, _) = session(
                    &commit,
                    &cx,
                    &vfs,
                    &pool,
                    &authority,
                    &token,
                    at,
                    || 100,
                    policy(4),
                )
                .await;
                let text = format!(
                    "MATCH (a)-[r{selector}]->(b) RETURN r,r.score AS score,r.secret AS hidden \
                     ORDER BY score DESC,r"
                );
                let prepared = ordered.prepare(&cx, &text, &args).unwrap();
                let mut cursor = ordered
                    .execute_ordered(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(
                    values(&mut cursor).await,
                    expected,
                    "{selector}; hidden={hidden}"
                );
                drop(cursor);
                assert!(!ordered.is_closed());

                let paged = format!("{text} SKIP 1 LIMIT 2");
                let prepared = ordered.prepare(&cx, &paged, &args).unwrap();
                let mut cursor = ordered
                    .execute_ordered(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(values(&mut cursor).await, expected[1..3]);
                drop(cursor);

                let text = format!(
                    "MATCH (a)-[r{selector}]->(b) RETURN count(*) AS rows,sum(r.score) AS total, \
                     avg(r.score) AS mean,count(r.secret) AS hidden"
                );
                let prepared = ordered.prepare(&cx, &text, &args).unwrap();
                let mut cursor = ordered
                    .execute_ordered_aggregate(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(
                    aggregates(&mut cursor).await,
                    vec![vec![
                        Count(count),
                        Integer(total),
                        Average(GraphExactAverage::new(total, count).unwrap()),
                        Count(0),
                    ]],
                    "{selector}; hidden={hidden}"
                );
                drop(cursor);
                let text = format!(
                    "MATCH (a)-[r{selector}]->(b) RETURN count(*) AS rows,sum(r.score) AS total, \
                     collect(r.score) AS items"
                );
                let prepared = ordered.prepare(&cx, &text, &args).unwrap();
                let mut cursor = ordered
                    .execute_ordered_aggregate(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(
                    aggregates(&mut cursor).await,
                    vec![vec![
                        Count(count),
                        Integer(total),
                        Value(GraphValue::List(
                            collected.iter().copied().map(scalar).collect()
                        )),
                    ]],
                    "{selector}; hidden={hidden}"
                );
                drop(cursor);
                drop(ordered);
                assert_eq!(pool.used(), 0);
            }

            // The two selected bags share R occurrences. UNION ALL keeps
            // those duplicates through a final page; only three output rows
            // spend the signed allowance, while seven input edges are legal.
            let mut allowed = grant(14, 3);
            allowed.relations = Scope::only([R, T]);
            let token = authority.issue_at(&allowed, 100).unwrap();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                policy(3),
            )
            .await;
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH (a)-[r]->(b) RETURN r,r.score AS score UNION ALL \
                 MATCH (a)-[r:R|S]->(b) RETURN r,r.score AS score \
                 ORDER BY score DESC,r SKIP 1 LIMIT 3",
                    &args,
                )
                .unwrap();
            let mut cursor = ordered
                .execute_ordered(&cx, &prepared, &args)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(
                values(&mut cursor).await,
                vec![
                    vec![GraphValue::Edge(EId(101)), scalar(17)],
                    vec![GraphValue::Edge(EId(301)), scalar(13)],
                    vec![GraphValue::Edge(EId(102)), scalar(9)],
                ]
            );
            drop(cursor);
            drop(ordered);
            assert_eq!(pool.used(), 0);

            let mut denied = grant(0, 1);
            denied.relations = Scope::only([T]);
            let token = authority.issue_at(&denied, 100).unwrap();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                policy(1),
            )
            .await;
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH (a)-[r:R|S]->(b) RETURN r,r.score ORDER BY r",
                    &args,
                )
                .unwrap();
            vfs.reset();
            let mut cursor = ordered
                .execute_ordered(&cx, &prepared, &args)
                .unwrap()
                .await
                .unwrap();
            assert!(values(&mut cursor).await.is_empty());
            drop(cursor);
            assert_eq!(
                vfs.reads.load(Ordering::SeqCst),
                0,
                "a finite selector with no authorized member opens no cold candidate scan"
            );
            let prepared = ordered.prepare(
                &cx,
                "MATCH (a)-[r:R|S]->(b) RETURN count(*) AS rows,sum(r.score) AS total,collect(r.score) AS items",
                &args,
            ).unwrap();
            let mut cursor = ordered
                .execute_ordered_aggregate(&cx, &prepared, &args)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(
                aggregates(&mut cursor).await,
                vec![vec![
                    Count(0),
                    Value(null()),
                    Value(GraphValue::List(Box::new([])))
                ]]
            );
            drop(cursor);
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
            assert!(!ordered.is_closed());
            drop(ordered);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_carried_fields_compose_with_lazy_scalars_calls_and_aggregate_inputs() {
    use GraphAggregateValue::{Average, Count, Integer};
    let ((), report) = run_async_under_lab(0xa8_0212, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let authority = issuer();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixture(&commit, hidden).await;
            for at in [old, current] {
                let first = if at == old { 3 } else { 30 };
                let edge_rows = if at == old {
                    vec![
                        vec![scalar(9), scalar(3), scalar(1)],
                        vec![scalar(7), scalar(3), scalar(1)],
                        vec![scalar(5), scalar(1), scalar(3)],
                    ]
                } else {
                    vec![
                        vec![scalar(17), scalar(30), scalar(1)],
                        vec![scalar(9), scalar(30), scalar(1)],
                        vec![scalar(5), scalar(1), scalar(3)],
                    ]
                };
                for (nodes, text, expected) in [
                    (
                        10,
                        "MATCH (n:L) WITH n AS item RETURN \
                         CASE WHEN true THEN [item][0].score ELSE [item][0].secret END AS score, \
                         [item][0].secret AS hidden ORDER BY score DESC LIMIT 3",
                        vec![
                            vec![scalar(first), null()],
                            vec![scalar(3), null()],
                            vec![scalar(2), null()],
                        ],
                    ),
                    (
                        18,
                        "MATCH (a)-[r:R]->(b) WITH a,r,b RETURN [r][0].score AS weight, \
                         [a][0].score AS source,[b][0].score AS target ORDER BY weight DESC",
                        edge_rows,
                    ),
                    (
                        15,
                        "MATCH (n:L) WITH n AS item CALL(item) { \
                         RETURN [member IN [item,item] | member.score] AS scores } \
                         UNWIND scores AS score RETURN score ORDER BY score DESC LIMIT 3",
                        vec![vec![scalar(first)], vec![scalar(first)], vec![scalar(3)]],
                    ),
                ] {
                    let token = authority.issue_at(&grant(nodes, 3), 100).unwrap();
                    let pool = memory();
                    let (mut ordered, _) = session(
                        &commit,
                        &cx,
                        &vfs,
                        &pool,
                        &authority,
                        &token,
                        at,
                        || 100,
                        policy(3),
                    )
                    .await;
                    let prepared = ordered.prepare(&cx, text, &args).unwrap();
                    let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
                    assert_send(&execution);
                    let mut cursor = execution.await.unwrap();
                    assert_eq!(cursor.snapshot_seq(), at);
                    assert_eq!(
                        values(&mut cursor).await,
                        expected,
                        "{text}; hidden={hidden}; at={at:?}"
                    );
                    drop(cursor);
                    assert!(!ordered.is_closed());
                    drop(ordered);
                    assert_eq!(pool.used(), 0);
                }
                // Selected fields are ordinary relational input cells before
                // grouping; terminal aggregate output metadata stays refused.
                let token = authority.issue_at(&grant(10, 1), 100).unwrap();
                let pool = memory();
                let (mut ordered, _) = session(
                    &commit,
                    &cx,
                    &vfs,
                    &pool,
                    &authority,
                    &token,
                    at,
                    || 100,
                    GqlQueryPolicy::new(10, 1, 1_000_000, 1_000_000),
                )
                .await;
                let prepared = ordered
                    .prepare(
                        &cx,
                        "MATCH (n:L) WITH n AS item WITH [item][0].score AS score \
                         RETURN count(*) AS rows,sum(score) AS total,avg(score) AS mean",
                        &args,
                    )
                    .unwrap();
                let mut cursor = ordered
                    .execute_ordered_aggregate(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                let sum = if at == old { 8 } else { 35 };
                assert_eq!(
                    aggregates(&mut cursor).await,
                    vec![vec![
                        Count(5),
                        Integer(sum),
                        Average(GraphExactAverage::new(sum, 5).unwrap()),
                    ]]
                );
                drop(cursor);
                drop(ordered);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_captured_identity_fields_recheck_scope_existence_and_the_pinned_cut() {
    let ((), report) = run_async_under_lab(0xa8_0213, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, old, current) = fixture(&commit, true).await;
        let mut birth = WriteBatch::new(R);
        birth.create_vertex(VId(20), vec![L], vec![]);
        let born = db.write(&commit, birth).await.unwrap();
        let mut deletion = WriteBatch::new(R);
        deletion.delete_vertex(VId(1));
        let retired = db.write(&commit, deletion).await.unwrap();
        let authority = issuer();
        // These are the ordinary admitted list parameters, not a host reader
        // or injected prepared plan. Every returned identity came from caller
        // data and must independently satisfy the same-cut visibility check.
        let vertices = GqlParameters::new()
            .with_list(
                "items",
                [1, 10, 20, 99]
                    .map(|id| GraphValue::Vertex(VId(id)))
                    .to_vec(),
            )
            .unwrap();
        let edges = GqlParameters::new()
            .with_list(
                "items",
                [10, 101, 201, 999]
                    .map(|id| GraphValue::Edge(EId(id)))
                    .to_vec(),
            )
            .unwrap();
        let text = "UNWIND $items AS item RETURN item,[item][0].score AS score ORDER BY item";
        for (at, nodes, first) in [
            (old, 1, scalar(3)),
            (current, 1, scalar(30)),
            (born, 2, scalar(30)),
            (retired, 1, null()),
        ] {
            let token = authority.issue_at(&grant(nodes, 4), 100).unwrap();
            let pool = memory();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                GqlQueryPolicy::new(nodes, 4, 1_000_000, 1_000_000),
            )
            .await;
            let prepared = ordered.prepare(&cx, text, &vertices).unwrap();
            let mut cursor = ordered
                .execute_ordered(&cx, &prepared, &vertices)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(
                values(&mut cursor).await,
                vec![
                    vec![GraphValue::Vertex(VId(1)), first],
                    vec![GraphValue::Vertex(VId(10)), null()],
                    vec![GraphValue::Vertex(VId(20)), null()],
                    vec![GraphValue::Vertex(VId(99)), null()],
                ]
            );
            drop(cursor);
            drop(ordered);
            assert_eq!(pool.used(), 0);
        }
        for (at, nodes, records, visible) in [
            (old, 2, 1, scalar(7)),
            (current, 2, 1, scalar(17)),
            (retired, 0, 0, null()),
        ] {
            let token = authority.issue_at(&grant(nodes, 4), 100).unwrap();
            let pool = memory();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                GqlQueryPolicy::new(records, 4, 1_000_000, 1_000_000),
            )
            .await;
            let prepared = ordered.prepare(&cx, text, &edges).unwrap();
            let mut cursor = ordered
                .execute_ordered(&cx, &prepared, &edges)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(
                values(&mut cursor).await,
                vec![
                    vec![GraphValue::Edge(EId(10)), null()],
                    vec![GraphValue::Edge(EId(101)), visible],
                    vec![GraphValue::Edge(EId(201)), null()],
                    vec![GraphValue::Edge(EId(999)), null()],
                ]
            );
            drop(cursor);
            drop(ordered);
            assert_eq!(pool.used(), 0);
        }
        // A denied key reveals neither existence nor topology. It opens no
        // point payload even for caller-supplied visible and hidden identities.
        let token = authority.issue_at(&grant(0, 4), 100).unwrap();
        let pool = memory();
        let (mut ordered, _) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            current,
            || 100,
            GqlQueryPolicy::new(0, 4, 1_000_000, 1_000_000),
        )
        .await;
        for args in [&vertices, &edges] {
            let prepared = ordered
                .prepare(
                    &cx,
                    "UNWIND $items AS item RETURN [item][0].secret AS hidden",
                    args,
                )
                .unwrap();
            vfs.reset();
            let mut cursor = ordered
                .execute_ordered(&cx, &prepared, args)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(values(&mut cursor).await, vec![vec![null()]; 4]);
            drop(cursor);
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
            assert!(!ordered.is_closed());
        }
        drop(ordered);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_point_allowances_are_cumulative_across_stages_and_retryable() {
    let ((), report) = run_async_under_lab(0xa8_0214, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _db, at, _) = fixture(&commit, true).await;
        let authority = issuer();
        let args = GqlParameters::new()
            .with_list("items", vec![GraphValue::Vertex(VId(1))])
            .unwrap();
        let twice = "UNWIND $items AS item WITH item,[item][0].score AS first \
                     RETURN first,[item][0].score AS second";
        for (nodes, records) in [(2, 2), (1, 2), (2, 1)] {
            let token = authority.issue_at(&grant(nodes, 1), 100).unwrap();
            let pool = memory();
            let (mut ordered, _) = session(
                &commit,
                &cx,
                &vfs,
                &pool,
                &authority,
                &token,
                at,
                || 100,
                GqlQueryPolicy::new(records, 1, 1_000_000, 1_000_000),
            )
            .await;
            let prepared = ordered.prepare(&cx, twice, &args).unwrap();
            let result = ordered
                .execute_ordered(&cx, &prepared, &args)
                .unwrap()
                .await;
            if nodes == 2 && records == 2 {
                let mut cursor = result.unwrap();
                assert_eq!(values(&mut cursor).await, vec![vec![scalar(3), scalar(3)]]);
            } else {
                let error = result.unwrap_err();
                if nodes == 1 {
                    assert!(matches!(error, NativeSpoolError::Authorized(error)
                        if matches!(*error, QueryError::Authorization(
                            Error::LimitExceeded(LimitDimension::Nodes)))));
                } else {
                    assert!(matches!(error, NativeSpoolError::Authorized(error)
                        if matches!(*error, QueryError::Set(fgdb_gql::GqlQueryError::Rows(error))
                            if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                                && error.limit == 1 && error.observed == 2)));
                }
            }
            assert!(!ordered.is_closed());
            let once = ordered
                .prepare(
                    &cx,
                    "UNWIND $items AS item RETURN [item][0].score AS score",
                    &args,
                )
                .unwrap();
            let mut cursor = ordered
                .execute_ordered(&cx, &once, &args)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(values(&mut cursor).await, vec![vec![scalar(3)]]);
            drop(cursor);
            drop(ordered);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_point_pending_reads_and_copied_payloads_keep_the_affine_owner() {
    let ((), report) = run_async_under_lab(0xa8_0215, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, at, _) = fixture(&commit, true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(1, 1), 100).unwrap();
        let args = GqlParameters::new()
            .with_list("items", vec![GraphValue::Vertex(VId(1))])
            .unwrap();
        let text = "UNWIND $items AS item RETURN [item][0].score AS payload";
        let pool = memory();
        let (mut ordered, files) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            at,
            || 100,
            GqlQueryPolicy::new(1, 1, 1_000_000, 1_000_000),
        )
        .await;
        let prepared = ordered.prepare(&cx, text, &args).unwrap();
        let baseline = pool.used();
        let before = observations(&files);
        vfs.reset();
        drop(ordered.execute_ordered(&cx, &prepared, &args).unwrap());
        assert!(!ordered.is_closed());
        assert_eq!(pool.used(), baseline);
        assert_eq!(observations(&files), before);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        vfs.pause.store(true, Ordering::SeqCst);
        let mut pending = Box::pin(ordered.execute_ordered(&cx, &prepared, &args).unwrap());
        poll_fn(|task| {
            assert!(pending.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        assert!(vfs.reads.load(Ordering::SeqCst) > 0);
        drop(pending);
        assert!(ordered.is_closed());
        drop(ordered);
        assert_eq!(pool.used(), 0);
        vfs.pause.store(false, Ordering::SeqCst);

        let mut change = WriteBatch::new(R);
        change.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::bytes(vec![0x7a; 4096]).unwrap()),
        );
        let at = db.write(&commit, change).await.unwrap();
        let (mut ordered, files) = session(
            &commit,
            &cx,
            &vfs,
            &pool,
            &authority,
            &token,
            at,
            || 100,
            GqlQueryPolicy::new(1, 1, 1_000_000, 1_000_000),
        )
        .await;
        let prepared = ordered.prepare(&cx, text, &args).unwrap();
        let baseline = pool.used();
        for file in &files {
            file.0.lock().unwrap().pending_on_payload = Some(&[0x7a; 8]);
        }
        let mut pending = Box::pin(ordered.execute_ordered(&cx, &prepared, &args).unwrap());
        let mut polls = 0;
        poll_fn(|task| {
            polls += 1;
            assert!(polls <= 1024, "point payload must reach its bounded append");
            assert!(pending.as_mut().poll(task).is_pending());
            if files
                .iter()
                .any(|file| file.0.lock().unwrap().pending_write)
            {
                Poll::Ready(())
            } else {
                task.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
        assert!(
            pool.used() >= baseline + 4096,
            "copied point payload remains charged until the suspended append releases it"
        );
        drop(pending);
        assert!(ordered.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_fixed_joins_compose_with_external_ordering_and_exact_aggregates() {
    use GraphAggregateValue::{Average, Count, Integer};
    let ((), report) = run_async_under_lab(0xa8_0216, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        // Three roots and two child occurrences admit ten endpoints. Native
        // source rows remain private; only the two final rows spend max_rows.
        let token = authority.issue_at(&grant(10, 2), 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixture(&commit, hidden).await;
            for (at, first, high, low, total) in
                [(old, 3, 9, 7, 16), (current, 30, 17, 9, 26)]
            {
                let pool = memory();
                let (mut ordered, _) = session(
                    &commit,
                    &cx,
                    &vfs,
                    &pool,
                    &authority,
                    &token,
                    at,
                    || 100,
                    policy(2),
                )
                .await;
                let text =
                    "MATCH (a)-[r:R]->(b)-[s:R]->(c) \
                     RETURN a.score AS first,b.score AS middle,c.score AS last, \
                     r.score AS weight,s.score AS following,s.secret AS hidden \
                     ORDER BY weight DESC";
                let prepared = ordered.prepare(&cx, text, &args).unwrap();
                let execution = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
                assert_send(&execution);
                let mut cursor = execution.await.unwrap();
                assert_send(&cursor);
                assert_eq!(cursor.snapshot_seq(), at);
                assert_eq!(
                    values(&mut cursor).await,
                    vec![
                        vec![scalar(first), scalar(1), scalar(3), scalar(high), scalar(5), null()],
                        vec![scalar(first), scalar(1), scalar(3), scalar(low), scalar(5), null()],
                    ],
                    "hidden={hidden}; cut={at:?}",
                );
                drop(cursor);
                assert!(!ordered.is_closed());
                let empty = ordered
                    .prepare(
                        &cx,
                        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN a,r,b,s,c LIMIT 0",
                        &args,
                    )
                    .unwrap();
                let mut cursor = ordered
                    .execute_ordered(&cx, &empty, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert!(values(&mut cursor).await.is_empty());
                drop(cursor);
                assert!(!ordered.is_closed());

                let prepared = ordered
                    .prepare(
                        &cx,
                        "MATCH (a)-[r:R]->(b)-[s:R]->(c) \
                         RETURN count(*) AS rows,sum(r.score) AS total, \
                         sum(c.score) AS endpoints,avg(s.score) AS mean, \
                         count(s.secret) AS hidden",
                        &args,
                    )
                    .unwrap();
                let execution = ordered
                    .execute_ordered_aggregate(&cx, &prepared, &args)
                    .unwrap();
                assert_send(&execution);
                let mut cursor = execution.await.unwrap();
                assert_eq!(
                    aggregates(&mut cursor).await,
                    vec![vec![
                        Count(2),
                        Integer(total),
                        Integer(6),
                        Average(GraphExactAverage::new(5, 1).unwrap()),
                        Count(0),
                    ]],
                    "hidden={hidden}; cut={at:?}",
                );
                drop(cursor);
                assert!(!ordered.is_closed());
                drop(ordered);
                assert_eq!(pool.used(), 0);

                let tight = authority.issue_at(&grant(9, 2), 100).unwrap();
                let (mut ordered, _) = session(
                    &commit,
                    &cx,
                    &vfs,
                    &pool,
                    &authority,
                    &tight,
                    at,
                    || 100,
                    policy(2),
                )
                .await;
                let prepared = ordered.prepare(&cx, text, &args).unwrap();
                assert!(matches!(
                    ordered.execute_ordered(&cx, &prepared, &args).unwrap().await,
                    Err(NativeSpoolError::Authorized(error))
                        if matches!(*error, QueryError::Authorization(
                            Error::LimitExceeded(LimitDimension::Nodes)
                        ))
                ));
                assert!(!ordered.is_closed());
                drop(ordered);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
