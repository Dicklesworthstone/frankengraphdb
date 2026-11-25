//! Public signed sessions over real buffered publications. Physical cold
//! admission remains host-bounded; hidden candidates never spend visible query
//! budgets or reach property evaluation. No resident source is used as fallback.
#![recursion_limit = "256"]

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    AuthorizedBufferedError, AuthorizedBufferedReadSource, AuthorizedReadSession, BufferLimits,
    BufferedOpenError, BufferedReadLimits, Database, DatabaseKeys, DerivedPublicationStage, MemVfs,
    MemVfsFile, MemoryPool, PreparedNativeRead, QueryError, WriteBatch,
};
use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::symbol_bindings::CatalogSymbolResolver;
use fgdb_gql::{
    GqlMapArgumentText, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, PreparedGraphBranchText, ReverseSymbolCatalog,
};
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
const T: RelationId = RelationId(9);
const U: RelationId = RelationId(10);

#[path = "authorized_buffered_read/vertex_probes.rs"]
mod vertex_probes;

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

struct HostCatalog {
    catalog: SymbolCatalog,
    lookups: Arc<AtomicUsize>,
}
impl GraphSymbolResolver for HostCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let name = if kind == GraphSymbolKind::Label && name == "Visible" {
            "L"
        } else {
            name
        };
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

async fn relation_fixture(
    cx: &CommitCx,
    hidden: bool,
) -> (GateVfs, Database<GateVfs>, CommitSeq, CommitSeq) {
    let (vfs, mut db, _, _) = fixture(cx, hidden).await;
    let mut extra = SymbolCatalog::default();
    extra
        .bind_all([
            ("T", CatalogSymbol::Relation(T)),
            ("U", CatalogSymbol::Relation(U)),
        ])
        .unwrap();
    db.register_symbols(cx, &extra).await.unwrap();
    let mut parallel = WriteBatch::new(R);
    parallel.add_edge(EId(102), VId(1), VId(2), vec![(P, CanonicalScalar::Int(9))]);
    parallel.add_edge(EId(103), VId(1), VId(1), vec![(P, CanonicalScalar::Int(5))]);
    db.write(cx, parallel).await.unwrap();
    let mut other = WriteBatch::new(T);
    other.add_edge(
        EId(150),
        VId(2),
        VId(1),
        vec![
            (P, CanonicalScalar::Int(13)),
            (SECRET, CanonicalScalar::Int(999)),
        ],
    );
    if hidden {
        other.add_edge(
            EId(151),
            VId(1),
            VId(10),
            vec![(P, CanonicalScalar::Int(-1))],
        );
    }
    let old = db.write(cx, other).await.unwrap();
    let mut edit = WriteBatch::new(R);
    edit.set_edge_property(EId(101), P, Some(CanonicalScalar::Int(27)));
    edit.delete_edge(EId(102));
    let current = db.write(cx, edit).await.unwrap();
    (vfs, db, old, current)
}

fn relation_rows(rows: &[(u128, u128, u128, i64)]) -> Vec<GraphValueRow> {
    rows.iter()
        .map(|&(edge, source, target, score)| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Edge(EId(edge)),
                GraphValue::Vertex(VId(source)),
                GraphValue::Vertex(VId(target)),
                GraphValue::Scalar(CanonicalScalar::Int(score)),
                GraphValue::Scalar(CanonicalScalar::Null),
            ])
        })
        .collect()
}

#[test]
fn any_and_finite_relation_sources_preserve_occurrences_scopes_and_history() {
    let ((), report) = run_async_under_lab(0xa6_0111, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let authority = issuer();
        for hidden in [false, true] {
            let (_, db, old, current) = relation_fixture(&commit, hidden).await;
            for at in [old, current] {
                // Each non-self edge admits two endpoints; a self-loop one.
                // Exact allowances catch charging hidden or unselected rows.
                let mut allowed = grant(if at == old { 7 } else { 5 });
                allowed.relations = Scope::only([R, T]);
                let token = authority.issue_at(&allowed, 100).unwrap();
                let pool = memory();
                let mut session = db
                    .buffered_read_publication(&cx, pool.clone(), limits())
                    .unwrap()
                    .into_authorized_read_session(
                        &commit,
                        &cx,
                        &authority,
                        &token,
                        "main",
                        at,
                        GqlQueryPolicy::new(4, 10, 100_000, 100_000),
                        || 100,
                    )
                    .await
                    .unwrap();
                let weight = if at == old { 17 } else { 27 };
                let mut directed = vec![(101, 1, 2, weight)];
                if at == old {
                    directed.push((102, 1, 2, 9));
                }
                directed.extend([(103, 1, 1, 5), (150, 2, 1, 13)]);
                let any = "MATCH (a)-[r]->(b) RETURN r,a,b,r.score,r.secret";
                assert_eq!(
                    collect(&mut session, &cx, any).await,
                    relation_rows(&directed)
                );
                let historical = format!(
                    "MATCH (a)-[r]->(b) FOR SYSTEM_TIME AS OF SEQ {} RETURN r,a,b,r.score,r.secret",
                    at.0,
                );
                assert_eq!(
                    collect(&mut session, &cx, &historical).await,
                    relation_rows(&directed)
                );
                let incoming: Vec<_> = directed
                    .iter()
                    .map(|&(edge, a, b, score)| (edge, b, a, score))
                    .collect();
                assert_eq!(
                    collect(
                        &mut session,
                        &cx,
                        "MATCH (a)<-[r]-(b) RETURN r,a,b,r.score,r.secret"
                    )
                    .await,
                    relation_rows(&incoming)
                );
                let mut undirected = vec![(101, 1, 2, weight), (101, 2, 1, weight)];
                if at == old {
                    undirected.extend([(102, 1, 2, 9), (102, 2, 1, 9)]);
                }
                undirected.extend([(103, 1, 1, 5), (150, 1, 2, 13), (150, 2, 1, 13)]);
                assert_eq!(
                    collect(
                        &mut session,
                        &cx,
                        "MATCH (a)-[r]-(b) RETURN r,a,b,r.score,r.secret"
                    )
                    .await,
                    relation_rows(&undirected)
                );
                let endpoint_rows: Vec<_> = undirected
                    .iter()
                    .map(|&(edge, a, b, score)| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Edge(EId(edge)),
                            GraphValue::Vertex(VId(a)),
                            GraphValue::Vertex(VId(b)),
                            GraphValue::Scalar(CanonicalScalar::Int(if a == 1 { 11 } else { 2 })),
                            GraphValue::Scalar(CanonicalScalar::Int(if b == 1 { 11 } else { 2 })),
                            GraphValue::Scalar(CanonicalScalar::Int(score)),
                            GraphValue::Scalar(CanonicalScalar::Null),
                            GraphValue::Scalar(CanonicalScalar::Null),
                        ])
                    })
                    .collect();
                assert_eq!(
                    collect(
                        &mut session,
                        &cx,
                        "MATCH (a)-[r]-(b) RETURN r,a,b,a.score,b.score,r.score,a.secret,b.secret"
                    )
                    .await,
                    endpoint_rows,
                    "physical endpoint fields must follow each native orientation, including self-loops"
                );
                drop(session);
                assert_eq!(pool.used(), 0);

                // T is permitted and has real rows, but is NOT in R|S.
                // S is in the selector but hidden by the capability.
                allowed.limits.max_nodes = if at == old { 5 } else { 3 };
                let token = authority.issue_at(&allowed, 100).unwrap();
                let mut session = db
                    .buffered_read_publication(&cx, pool.clone(), limits())
                    .unwrap()
                    .into_authorized_read_session(
                        &commit,
                        &cx,
                        &authority,
                        &token,
                        "main",
                        at,
                        GqlQueryPolicy::new(3, 10, 100_000, 100_000),
                        || 100,
                    )
                    .await
                    .unwrap();
                let expected = relation_rows(&directed[..directed.len() - 1]);
                for selector in [":R", ":R|S", ":R|S|R"] {
                    let text =
                        format!("MATCH (a)-[r{selector}]->(b) RETURN r,a,b,r.score,r.secret");
                    assert_eq!(
                        collect(&mut session, &cx, &text).await,
                        expected,
                        "{selector}"
                    );
                }
                drop(session);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn wholly_denied_finite_relations_open_no_source_and_do_not_spend_nodes() {
    let ((), report) = run_async_under_lab(0xa6_0112, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, db, _, at) = relation_fixture(&commit, true).await;
        let authority = issuer();
        let mut denied = grant(0);
        denied.relations = Scope::only([T]);
        let token = authority.issue_at(&denied, 100).unwrap();
        let pool = memory();
        let mut session = db
            .buffered_read_publication(&cx, pool.clone(), limits())
            .unwrap()
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
            .await
            .unwrap();
        vfs.reset();
        vfs.refuse.store(true, Ordering::SeqCst);
        for selector in [":R|S", ":S|U", ":S|R|S"] {
            let text = format!("MATCH (a)-[r{selector}]->(b) RETURN r,r.score");
            assert!(collect(&mut session, &cx, &text).await.is_empty());
        }
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert!(!session.is_closed());
        vfs.refuse.store(false, Ordering::SeqCst);
        let args = GqlParameters::new();
        let admitted = session
            .prepare(&cx, "MATCH (a)-[r]->(b) RETURN r", &args)
            .unwrap();
        let mut cursor = session.stream(&cx, &admitted, &args).unwrap();
        assert!(matches!(
            cursor.next().await.unwrap(),
            Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                Error::LimitExceeded(LimitDimension::Nodes)
            )))
        ));
        drop(cursor);
        drop(session);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn trusted_catalog_binding_uses_the_selected_cut_and_retains_host_names() {
    let ((), report) = run_async_under_lab(0xa6_0110, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (_, mut db, old, _) = fixture(&commit, true).await;
        let mut later = SymbolCatalog::default();
        later
            .bind_all([("later", CatalogSymbol::Property(PropertyKeyId(3)))])
            .unwrap();
        db.register_symbols(&commit, &later).await.unwrap();
        // Historical names use the allocator floors of the selected publication.
        let old_catalog = db.symbol_catalog_at(old).unwrap();
        let later_id = CatalogSymbol::Property(PropertyKeyId(3));
        assert_eq!(db.symbol_catalog().unwrap().name(later_id), Some("later"));
        assert_eq!(old_catalog.name(later_id), None);
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let pool = memory();
        let bindings = AtomicUsize::new(0);
        let lookups = Arc::new(AtomicUsize::new(0));
        let mut session = db
            .buffered_read_publication(&cx, pool.clone(), limits())
            .unwrap()
            .into_authorized_read_session_with_resolver(
                &commit,
                &cx,
                &authority,
                &token,
                "main",
                old,
                policy(),
                || 100,
                |catalog| {
                    bindings.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(catalog, old_catalog);
                    Ok(HostCatalog {
                        catalog,
                        lookups: lookups.clone(),
                    })
                },
            )
            .await
            .unwrap();
        assert_eq!(bindings.load(Ordering::SeqCst), 1);
        assert_eq!(
            collect(
                &mut session,
                &cx,
                "MATCH (n:Visible) RETURN n,n.score ORDER BY n",
            )
            .await,
            [1, 2]
                .into_iter()
                .map(|id| GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(id)),
                    GraphValue::Scalar(CanonicalScalar::Int(id as i64)),
                ]))
                .collect::<Vec<_>>()
        );
        assert!(lookups.load(Ordering::SeqCst) > 0);
        assert!(
            session
                .prepare(&cx, "MATCH (n) RETURN n,n.later", &GqlParameters::new())
                .is_err()
        );
        assert_eq!(bindings.load(Ordering::SeqCst), 1);
        drop(session);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn catalog_callbacks_follow_authorization_and_cannot_hide_late_expiry() {
    let ((), report) = run_async_under_lab(0xa6_0111, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts
            .query()
            .with_host_clock_interval(std::num::NonZeroU32::new(1024).unwrap());
        let (vfs, db, _, at) = fixture(&commit, false).await;
        let authority = issuer();
        let pool = memory();
        vfs.reset();
        let selected = db
            .buffered_read_publication(&cx, pool.clone(), limits())
            .unwrap()
            .symbol_catalog(&commit)
            .await
            .unwrap()
            .symbol_catalog_at(at)
            .unwrap();
        let catalog_reads = vfs.reads.load(Ordering::SeqCst);
        assert!(catalog_reads > 0);
        for denied in [true, false] {
            let mut allowed = grant(2);
            if denied {
                allowed.rights = fgdb_warden::Rights::None;
            }
            let token = authority.issue_at(&allowed, 100).unwrap();
            let calls = AtomicUsize::new(0);
            vfs.reset();
            vfs.refuse.store(denied, Ordering::SeqCst);
            let result = db
                .buffered_read_publication(&cx, pool.clone(), limits())
                .unwrap()
                .into_authorized_read_session_with_resolver(
                    &commit,
                    &cx,
                    &authority,
                    &token,
                    "main",
                    at,
                    policy(),
                    || 100,
                    |catalog| -> Result<HostCatalog, String> {
                        assert_eq!(catalog, selected);
                        calls.fetch_add(1, Ordering::SeqCst);
                        vfs.refuse.store(true, Ordering::SeqCst);
                        Err("host rejected stale names".into())
                    },
                )
                .await;
            if denied {
                assert!(matches!(
                    result,
                    Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                        Error::PermissionDenied
                    )))
                ));
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            } else {
                assert!(matches!(
                    result,
                    Err(AuthorizedBufferedError::CatalogBinding(_))
                ));
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            assert_eq!(
                vfs.reads.load(Ordering::SeqCst),
                if denied { 0 } else { catalog_reads }
            );
            assert_eq!(pool.used(), 0);
        }
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let now = AtomicUsize::new(100);
        vfs.reset();
        vfs.refuse.store(false, Ordering::SeqCst);
        let result = db
            .buffered_read_publication(&cx, pool.clone(), limits())
            .unwrap()
            .into_authorized_read_session_with_resolver(
                &commit,
                &cx,
                &authority,
                &token,
                "main",
                at,
                policy(),
                || now.load(Ordering::SeqCst) as u64,
                |catalog| -> Result<HostCatalog, String> {
                    assert_eq!(catalog, selected);
                    now.store(1001, Ordering::SeqCst);
                    vfs.refuse.store(true, Ordering::SeqCst);
                    Err("expiry must take precedence".into())
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                Error::Expired
            )))
        ));
        assert_eq!(vfs.reads.load(Ordering::SeqCst), catalog_reads);
        assert_eq!(pool.used(), 0);
        vfs.refuse.store(false, Ordering::SeqCst);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_prepared_buffered_reads_rebind_maps_and_history_without_reparsing() {
    check_host_prepared_read(
        "AT BRANCH $route MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut \
         WHERE n.score = $record.score RETURN n,n.score ORDER BY n",
        false,
    );
    check_host_prepared_read(
        "AT BRANCH $route MATCH (n:L {score: $record.score}) FOR SYSTEM_TIME AS OF SEQ $cut \
         RETURN n,n.score ORDER BY n",
        true,
    );
}

fn check_host_prepared_read(text: &'static str, expect_splice: bool) {
    let ((), report) = run_async_under_lab(0xa6_0112, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, db, old, current) = fixture(&commit, true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let pool = memory();
        let lookups = Arc::new(AtomicUsize::new(0));
        let catalog = db.symbol_catalog_at(old).unwrap();
        let arguments = |cut: CommitSeq, score| {
            GqlParameters::new()
                .with_text("route", "main")
                .unwrap()
                .with_uint64("cut", cut.0)
                .unwrap()
                .with_map(
                    "record",
                    vec![(
                        "score".into(),
                        GraphValue::Scalar(CanonicalScalar::Int(score)),
                    )],
                )
                .unwrap()
        };
        let args = arguments(old, 1);
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
        let prepared_lookups = lookups.load(Ordering::SeqCst);
        assert!(prepared_lookups > 0);
        let mut foreign = None;
        for (at, score) in [(old, 1), (current, 11), (old, 1)] {
            let args = arguments(at, score);
            let (mut session, prepared) = db
                .buffered_read_publication(&cx, pool.clone(), limits())
                .unwrap()
                .into_authorized_prepared_read_session_with_resolver(
                    &commit,
                    &cx,
                    &authority,
                    &token,
                    "main",
                    at,
                    policy(),
                    || 100,
                    |selected| {
                        if selected != catalog {
                            return Err("prepared symbol catalog changed".into());
                        }
                        Ok(HostCatalog {
                            catalog: selected,
                            lookups: lookups.clone(),
                        })
                    },
                    selector.clone(),
                    native.clone(),
                    lowered.clone(),
                )
                .await
                .unwrap();
            if let Some(foreign) = &foreign {
                assert!(matches!(
                    session.stream(&cx, foreign, &args),
                    Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                        Error::WrongAuthority
                    )))
                ));
                assert!(!session.is_closed());
            }
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            assert_eq!(cursor.snapshot_seq(), at);
            assert_eq!(
                cursor.next().await.unwrap().unwrap().as_ref(),
                &GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Scalar(CanonicalScalar::Int(score)),
                ])
            );
            assert!(cursor.next().await.is_none());
            drop(cursor);
            assert_eq!(lookups.load(Ordering::SeqCst), prepared_lookups);
            vfs.reset();
            vfs.refuse.store(true, Ordering::SeqCst);
            let different = if at == old { current } else { old };
            assert!(matches!(
                session.stream(&cx, &prepared, &arguments(different, score)),
                Err(AuthorizedBufferedError::DifferentCut { requested, bound })
                    if requested == different && bound == at
            ));
            assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
            vfs.refuse.store(false, Ordering::SeqCst);
            drop(session);
            assert_eq!(
                pool.used(),
                0,
                "a retained template never pins graph storage"
            );
            foreign = Some(prepared);
        }
        let mismatched = Arc::new(PreparedGraphBranchText::prepare("MATCH (n) RETURN n").unwrap());
        let result = db
            .buffered_read_publication(&cx, pool.clone(), limits())
            .unwrap()
            .into_authorized_prepared_read_session_with_resolver(
                &commit,
                &cx,
                &authority,
                &token,
                "main",
                old,
                policy(),
                || 100,
                |catalog| Ok(HostCatalog { catalog, lookups }),
                mismatched,
                native,
                lowered,
            )
            .await;
        assert!(matches!(
            result,
            Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                Error::WrongAuthority
            )))
        ));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_publication_keeps_one_cut_without_retaining_the_writer_or_resident_snapshot() {
    let ((), report) = run_async_under_lab(0xa6_0107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _, at) = fixture(&commit, true).await;
        let expected_root = db.partition_root().unwrap();
        let expected_manifest = db.manifest().unwrap();
        let weak = db.pinned_read_view().unwrap().downgrade();
        let pool = memory();
        vfs.reset();
        vfs.refuse.store(true, Ordering::SeqCst);
        let publication = db
            .buffered_read_publication(&cx, pool.clone(), limits())
            .unwrap();
        let physical = db
            .buffered_read_publication(&cx, memory(), limits())
            .unwrap();
        assert_eq!(publication.frontier(), at);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert_eq!(pool.used(), 0, "capture does not admit graph storage");
        vfs.refuse.store(false, Ordering::SeqCst);

        let mut extra = SymbolCatalog::default();
        extra
            .bind_all([("later", CatalogSymbol::Property(PropertyKeyId(3)))])
            .unwrap();
        db.register_symbols(&commit, &extra).await.unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(111)));
        edit.set_edge_property(EId(101), P, Some(CanonicalScalar::Int(117)));
        let later = db.write(&commit, edit).await.unwrap();
        db.compact(&commit).await.unwrap();
        assert!(
            weak.upgrade().is_none(),
            "capture retained a resident snapshot"
        );
        drop(db);
        // The captures own no writer lease: a new writer opens while they live.
        let reopened =
            Database::open_with_vfs(&commit, vfs.clone(), vfs.memory.database_dir(), keys())
                .await
                .unwrap();
        assert_eq!(reopened.frontier().unwrap(), later);
        let physical = physical.into_read_view(&commit).await.unwrap();
        assert_eq!(physical.partition_root(), expected_root);
        assert_eq!(physical.manifest(), expected_manifest);
        assert_eq!(physical.frontier(), at);
        drop(physical);

        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let mut session = publication
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
            .await
            .unwrap();
        assert!(
            session
                .prepare(&cx, "MATCH (n) RETURN n,n.later", &GqlParameters::new())
                .is_err(),
            "later catalog names leaked into the captured cut"
        );
        assert_eq!(
            collect(&mut session, &cx, "MATCH (n) RETURN n,n.score").await,
            [
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Scalar(CanonicalScalar::Int(11)),
                ]),
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(2)),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                ]),
            ]
        );
        assert_eq!(
            collect(&mut session, &cx, "MATCH (a)-[r:R]->(b) RETURN r,r.score").await,
            [GraphValueRow::from_owned_values(vec![
                GraphValue::Edge(EId(101)),
                GraphValue::Scalar(CanonicalScalar::Int(17)),
            ])]
        );
        drop(session);
        assert_eq!(pool.used(), 0);
        drop(reopened);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_publication_preserves_authorization_and_cold_memory_admission() {
    let ((), report) = run_async_under_lab(0xa6_0108, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, db, _, at) = fixture(&commit, false).await;
        for wrong_namespace in [false, true] {
            let pool = memory();
            vfs.reset();
            vfs.refuse.store(true, Ordering::SeqCst);
            let publication = db
                .buffered_read_publication(&cx, pool.clone(), limits())
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
            let mut denied = grant(2);
            if !wrong_namespace {
                denied.rights = fgdb_warden::Rights::None;
            }
            let token = authority.issue_at(&denied, 100).unwrap();
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
        }
        vfs.refuse.store(false, Ordering::SeqCst);
        let pool = MemoryPool::new(1, 0).unwrap();
        let publication = db
            .buffered_read_publication(&cx, pool.clone(), limits())
            .unwrap();
        let result = publication.into_read_view(&commit).await;
        assert!(matches!(
            result,
            Err(BufferedOpenError::Read(fgdb::BufferedReadError::Memory(_)))
        ));
        assert_eq!(
            pool.used(),
            0,
            "refused admission releases every reservation"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_publication_refuses_a_fenced_writer_instead_of_exporting_its_stale_root() {
    let ((), report) = run_async_under_lab(0xa6_0109, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _, _) = fixture(&commit, false).await;
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(111)));
        assert!(
            db.write_with_publication_failure(
                &commit,
                edit,
                DerivedPublicationStage::PublishRootSlot,
            )
            .await
            .is_err()
        );
        vfs.reset();
        vfs.refuse.store(true, Ordering::SeqCst);
        assert!(matches!(
            db.buffered_read_publication(&cx, memory(), limits()),
            Err(BufferedOpenError::RecoveryRequired)
        ));
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        vfs.refuse.store(false, Ordering::SeqCst);
    });
    assert!(report.lab_test_passed(), "{report:?}");
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
fn cold_endpoint_fields_preserve_history_predicates_scope_and_node_admission() {
    let ((), report) = run_async_under_lab(0xa6_0113, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixture(&commit, hidden).await;
            for (at, source, weight) in [(old, 1, 7), (current, 11, 17)] {
                let pool = memory();
                let mut reader =
                    session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
                for (text, first, second) in [
                    (
                        "MATCH (a)-[r:R]->(b) RETURN r,a.score,b.score,r.score,a.secret,b.secret",
                        source,
                        2,
                    ),
                    (
                        "MATCH (a)<-[r:R]-(b) RETURN r,a.score,b.score,r.score,a.secret,b.secret",
                        2,
                        source,
                    ),
                ] {
                    assert_eq!(
                        collect(&mut reader, &cx, text).await,
                        vec![GraphValueRow::from_owned_values(vec![
                            GraphValue::Edge(EId(101)),
                            GraphValue::Scalar(CanonicalScalar::Int(first)),
                            GraphValue::Scalar(CanonicalScalar::Int(second)),
                            GraphValue::Scalar(CanonicalScalar::Int(weight)),
                            GraphValue::Scalar(CanonicalScalar::Null),
                            GraphValue::Scalar(CanonicalScalar::Null),
                        ])],
                        "{text}; cut={at:?}; hidden={hidden}"
                    );
                }
                // This was the original unsupported source shape. It now
                // uses the same selected endpoint owner as the full tuple.
                assert_eq!(
                    collect(&mut reader, &cx, "MATCH (a)-[r:R]->(b) RETURN r,a.score").await,
                    vec![GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(101)),
                        GraphValue::Scalar(CanonicalScalar::Int(source)),
                    ])]
                );
                let filtered = collect(
                    &mut reader,
                    &cx,
                    "MATCH (a)-[r:R]->(b) WHERE a.score>b.score AND a.secret IS NULL \
                     AND b.secret IS NULL RETURN r,a.score,b.score",
                )
                .await;
                let expected = if at == old {
                    vec![]
                } else {
                    vec![GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(101)),
                        GraphValue::Scalar(CanonicalScalar::Int(11)),
                        GraphValue::Scalar(CanonicalScalar::Int(2)),
                    ])]
                };
                assert_eq!(filtered, expected);
                assert!(
                    collect(
                        &mut reader,
                        &cx,
                        "MATCH (a)-[r:R]->(b) WHERE a.secret=999 RETURN r,a.score"
                    )
                    .await
                    .is_empty()
                );
                drop(reader);
                assert_eq!(pool.used(), 0);

                let tight = authority.issue_at(&grant(1), 100).unwrap();
                let mut reader =
                    session(&commit, &cx, &vfs, &pool, &authority, &tight, at, || 100).await;
                let prepared = reader
                    .prepare(&cx, "MATCH (a)-[r:R]->(b) RETURN r,a.score,b.score", &args)
                    .unwrap();
                let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
                assert!(matches!(
                    cursor.next().await.unwrap(),
                    Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                        Error::LimitExceeded(LimitDimension::Nodes)
                    )))
                ));
                assert!(cursor.next().await.is_none());
                drop(cursor);
                assert!(!reader.is_closed());
                assert!(
                    collect(
                        &mut reader,
                        &cx,
                        "MATCH (a)-[r:R]->(b) RETURN r,a.score,b.score LIMIT 0"
                    )
                    .await
                    .is_empty()
                );
                drop(reader);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn returned_endpoint_payloads_retain_their_memory_after_source_and_session_drop() {
    let ((), report) = run_async_under_lab(0xa6_0114, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _, _) = fixture(&commit, false).await;
        let left = CanonicalScalar::bytes(vec![0x31; 4096]).unwrap();
        let right = CanonicalScalar::bytes(vec![0x32; 4096]).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(1), P, Some(left.clone()));
        edit.set_vertex_property(VId(2), P, Some(right.clone()));
        let at = db.write(&commit, edit).await.unwrap();
        let pool = memory();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let mut reader = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let args = GqlParameters::new();
        let prepared = reader
            .prepare(
                &cx,
                "MATCH (a)-[r:R]->(b) RETURN r,a.score,b.score LIMIT 1",
                &args,
            )
            .unwrap();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        drop(reader);
        assert_eq!(
            held.values(),
            &[
                GraphValue::Edge(EId(101)),
                GraphValue::Scalar(left),
                GraphValue::Scalar(right),
            ]
        );
        assert!(
            pool.used() >= 8192,
            "both returned endpoint payloads stay charged"
        );
        drop(held);
        assert_eq!(pool.used(), 0);
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
            "MATCH (a)-[r*1..2]->(b) RETURN r",
            "MATCH (a)-[r:R]->(b) RETURN r,properties(a)",
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

async fn fixed_join_fixture(
    cx: &CommitCx,
    hidden: bool,
) -> (GateVfs, Database<GateVfs>, CommitSeq, CommitSeq) {
    let (vfs, mut db, _, _) = fixture(cx, hidden).await;
    let mut chain = WriteBatch::new(R);
    chain.create_vertex(
        VId(3),
        vec![L, H],
        vec![
            (P, CanonicalScalar::Int(3)),
            (SECRET, CanonicalScalar::Int(999)),
        ],
    );
    for (eid, from, to, score) in [
        (102, 2, 3, 5),
        (103, 2, 3, 9),
        (104, 3, 1, 4),
        (105, 2, 2, 6),
    ] {
        chain.add_edge(
            EId(eid),
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
            // These child identities precede the first visible successor.
            // An adapter must advance privately without a candidate/node debit.
            chain.add_edge(
                EId(id + 70),
                VId(2),
                VId(id),
                vec![(P, CanonicalScalar::Int(999))],
            );
        }
    }
    let old = db.write(cx, chain).await.unwrap();
    let mut edit = WriteBatch::new(R);
    edit.set_edge_property(EId(102), P, Some(CanonicalScalar::Int(50)));
    edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(30)));
    edit.delete_edge(EId(103));
    let current = db.write(cx, edit).await.unwrap();
    (vfs, db, old, current)
}

#[test]
fn cold_fixed_joins_keep_last_root_children_parallel_edges_and_historical_fields() {
    let ((), report) = run_async_under_lab(0xa6_0115, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(100), 100).unwrap();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixed_join_fixture(&commit, hidden).await;
            for (at, latest) in [(old, false), (current, true)] {
                let pool = memory();
                let mut reader = Database::open_buffered_publication_with_vfs(
                    &commit,
                    vfs.clone(),
                    vfs.memory.database_dir(),
                    keys(),
                    pool.clone(),
                    limits(),
                )
                .await
                .unwrap()
                .into_authorized_read_session(
                    &commit,
                    &cx,
                    &authority,
                    &token,
                    "main",
                    at,
                    GqlQueryPolicy::new(100, 10, 100_000, 100_000),
                    || 100,
                )
                .await
                .unwrap();
                let rows = collect(
                    &mut reader,
                    &cx,
                    "MATCH (a)-[r:R]->(b)-[s]->(c) \
                     RETURN r,a,s,b,c,r.score,s.score,a.score,b.score,c.score,a.secret,s.secret",
                )
                .await;
                // Literal occurrence table: parallel edges multiply rows,
                // a self-loop is used once, and the final root 105 still expands.
                let paths = [
                    (101, 1, 102, 2, 3),
                    (101, 1, 103, 2, 3),
                    (101, 1, 105, 2, 2),
                    (102, 2, 104, 3, 1),
                    (103, 2, 104, 3, 1),
                    (104, 3, 101, 1, 2),
                    (105, 2, 102, 2, 3),
                    (105, 2, 103, 2, 3),
                ];
                let vertex_score = |id| match id {
                    1 => 11,
                    2 => 2,
                    3 if latest => 30,
                    3 => 3,
                    _ => unreachable!(),
                };
                let edge_score = |id| match id {
                    101 => 17,
                    102 if latest => 50,
                    102 => 5,
                    103 => 9,
                    104 => 4,
                    105 => 6,
                    _ => unreachable!(),
                };
                let expected: Vec<_> = paths
                    .into_iter()
                    .filter(|&(r, _, s, _, _)| !latest || (r != 103 && s != 103))
                    .map(|(r, a, s, b, c)| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Edge(EId(r)),
                            GraphValue::Vertex(VId(a)),
                            GraphValue::Edge(EId(s)),
                            GraphValue::Vertex(VId(b)),
                            GraphValue::Vertex(VId(c)),
                            GraphValue::Scalar(CanonicalScalar::Int(edge_score(r))),
                            GraphValue::Scalar(CanonicalScalar::Int(edge_score(s))),
                            GraphValue::Scalar(CanonicalScalar::Int(vertex_score(a))),
                            GraphValue::Scalar(CanonicalScalar::Int(vertex_score(b))),
                            GraphValue::Scalar(CanonicalScalar::Int(vertex_score(c))),
                            GraphValue::Scalar(CanonicalScalar::Null),
                            GraphValue::Scalar(CanonicalScalar::Null),
                        ])
                    })
                    .collect();
                assert_eq!(rows, expected, "hidden={hidden}; cut={at:?}");
                assert_eq!(
                    collect(
                        &mut reader,
                        &cx,
                        "MATCH (a)<-[r:R]-(b)<-[s:R]-(c) RETURN r,a,s,b,c LIMIT 1",
                    )
                    .await,
                    vec![GraphValueRow::from_owned_values(vec![
                        GraphValue::Edge(EId(101)),
                        GraphValue::Vertex(VId(2)),
                        GraphValue::Edge(EId(104)),
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Vertex(VId(3)),
                    ])],
                );
                drop(reader);
                assert_eq!(pool.used(), 0);
            }
            // A child's typed relation is independent of the root restriction.
            // The S edge 201 belongs to the existing fixture, after R edge 104.
            let mut scope = grant(100);
            scope.relations = Scope::only([R, S]);
            let mixed = authority.issue_at(&scope, 100).unwrap();
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
            let mut reader = publication
                .into_authorized_read_session(
                    &commit,
                    &cx,
                    &authority,
                    &mixed,
                    "main",
                    old,
                    GqlQueryPolicy::new(100, 10, 100_000, 100_000),
                    || 100,
                )
                .await
                .unwrap();
            assert_eq!(
                collect(
                    &mut reader,
                    &cx,
                    "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN r,a,s,b,c LIMIT 1",
                )
                .await,
                vec![GraphValueRow::from_owned_values(vec![
                    GraphValue::Edge(EId(104)),
                    GraphValue::Vertex(VId(3)),
                    GraphValue::Edge(EId(201)),
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(2)),
                ])],
            );
            drop(reader);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_fixed_join_visible_node_boundary_and_output_owner_ignore_hidden_children() {
    let ((), report) = run_async_under_lab(0xa6_0116, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, at, _) = fixed_join_fixture(&commit, hidden).await;
            for nodes in [3, 4] {
                let token = authority.issue_at(&grant(nodes), 100).unwrap();
                let pool = memory();
                let mut reader =
                    session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
                // Exactly two visible records and four endpoint admissions.
                let prepared = reader
                    .prepare(
                        &cx,
                        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,a,s,s.score LIMIT 1",
                        &args,
                    )
                    .unwrap();
                let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
                let result = cursor.next().await.unwrap();
                if nodes == 3 {
                    assert!(matches!(
                        result,
                        Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                            Error::LimitExceeded(LimitDimension::Nodes)
                        )))
                    ));
                    assert_eq!(cursor.state(), VertexScanState::Failed);
                    assert!(cursor.next().await.is_none());
                    drop(cursor);
                    assert!(!reader.is_closed());
                    drop(reader);
                    assert_eq!(pool.used(), 0);
                } else {
                    let row = result.unwrap();
                    assert_eq!(
                        row.as_ref().values(),
                        &[
                            GraphValue::Edge(EId(101)),
                            GraphValue::Vertex(VId(1)),
                            GraphValue::Edge(EId(102)),
                            GraphValue::Scalar(CanonicalScalar::Int(5)),
                        ],
                    );
                    assert!(cursor.next().await.is_none());
                    drop(cursor);
                    assert!(!reader.is_closed());
                    drop(reader);
                    assert!(
                        pool.used() > 0,
                        "the delivered multi-hop copy retains its owner"
                    );
                    drop(row);
                    assert_eq!(pool.used(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_fixed_join_pending_drop_releases_source_owners() {
    let ((), report) = run_async_under_lab(0xa6_0117, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let (vfs, _db, at, _) = fixed_join_fixture(&commit, false).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(4), 100).unwrap();
        let pool = memory();
        let mut reader = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let args = GqlParameters::new();
        let prepared = reader
            .prepare(
                &cx,
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,a,s LIMIT 1",
                &args,
            )
            .unwrap();
        let baseline = pool.used();
        vfs.reset();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        drop(cursor.next());
        cursor.close();
        drop(cursor);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert_eq!(pool.used(), baseline);
        assert!(!reader.is_closed());

        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
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
        assert!(reader.is_closed());
        assert_eq!(pool.used(), 0);
        vfs.pause.store(false, Ordering::SeqCst);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_edge_probes_use_scoped_selected_fields_at_the_pinned_cut() {
    let ((), report) = run_async_under_lab(0xa6_0118, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(100), 100).unwrap();
        for hidden in [false, true] {
            let (vfs, _db, old, current) = fixed_join_fixture(&commit, hidden).await;
            for (at, matches, absent) in [
                (old, vec![101, 105], vec![102, 103, 104]),
                (current, vec![], vec![101, 102, 104, 105]),
            ] {
                let pool = memory();
                let mut reader = Database::open_buffered_publication_with_vfs(
                    &commit,
                    vfs.clone(),
                    vfs.memory.database_dir(),
                    keys(),
                    pool.clone(),
                    limits(),
                )
                .await
                .unwrap()
                .into_authorized_read_session(
                    &commit,
                    &cx,
                    &authority,
                    &token,
                    "main",
                    at,
                    GqlQueryPolicy::new(100, 10, 100_000, 100_000),
                    || 100,
                )
                .await
                .unwrap();
                for (prefix, expected) in [("EXISTS", matches), ("NOT EXISTS", absent)] {
                    let text = format!(
                        "MATCH (a)-[r:R]->(b) WHERE {prefix} {{ \
                         MATCH (b)-[s:R]->(c) WHERE c.score = 3 AND s.score = 5 \
                         }} RETURN r",
                    );
                    let rows = collect(&mut reader, &cx, &text).await;
                    let expected: Vec<_> = expected
                        .into_iter()
                        .map(|id| GraphValueRow::from_owned_values(vec![GraphValue::Edge(EId(id))]))
                        .collect();
                    assert_eq!(rows, expected, "hidden={hidden}; cut={at:?}; {prefix}");
                }
                // Selected fields still pass the property mask, and an
                // invisible endpoint cannot supply a witness or a visible
                // budget debit even when its incident edge is visible by type.
                for predicate in ["c.secret = 999 OR s.secret = 999", "c.score = -1"] {
                    let text = format!(
                        "MATCH (a)-[r:R]->(b) WHERE EXISTS {{ \
                         MATCH (b)-[s:R]->(c) WHERE {predicate} }} RETURN r",
                    );
                    assert!(collect(&mut reader, &cx, &text).await.is_empty());
                }
                assert!(
                    collect(
                        &mut reader,
                        &cx,
                        "MATCH (a)-[r:R]->(b) WHERE EXISTS { \
                         MATCH (b)-[:S]->(c) } RETURN r",
                    )
                    .await
                    .is_empty()
                );
                assert!(!reader.is_closed());
                drop(reader);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_edge_probe_node_limit_is_exact_and_refusal_allows_retry() {
    let ((), report) = run_async_under_lab(0xa6_0119, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let cx = c.query();
        let authority = issuer();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let (vfs, _db, at, _) = fixed_join_fixture(&commit, hidden).await;
            for nodes in [3, 4] {
                let token = authority.issue_at(&grant(nodes), 100).unwrap();
                let pool = memory();
                let mut reader =
                    session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
                let prepared = reader
                    .prepare(
                        &cx,
                        "MATCH (a)-[r:R]->(b) WHERE EXISTS { \
                         MATCH (b)-[s:R]->(c) WHERE s.score = 5 AND c.score = 3 \
                         } RETURN r LIMIT 1",
                        &args,
                    )
                    .unwrap();
                let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
                let result = cursor.next().await.unwrap();
                if nodes == 3 {
                    assert!(matches!(
                        result,
                        Err(AuthorizedBufferedError::Query(QueryError::Authorization(
                            Error::LimitExceeded(LimitDimension::Nodes)
                        )))
                    ));
                    assert_eq!(cursor.state(), VertexScanState::Failed);
                } else {
                    assert_eq!(
                        result.unwrap().as_ref().values(),
                        &[GraphValue::Edge(EId(101))]
                    );
                }
                assert!(cursor.next().await.is_none());
                drop(cursor);
                assert!(!reader.is_closed());
                // A new execution has a fresh signed allowance and reuses the
                // same affine publication after the exhausted probe or refusal.
                assert_eq!(
                    collect(&mut reader, &cx, "MATCH (a)-[r:R]->(b) RETURN r LIMIT 1").await,
                    vec![GraphValueRow::from_owned_values(vec![GraphValue::Edge(
                        EId(101)
                    )])],
                );
                drop(reader);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
