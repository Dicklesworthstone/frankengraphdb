//! **The spine, differentially tested against the oracle** (`fgdb-j0vu`).
//!
//! `crates/fgdb/tests/spine.rs` proves the engine agrees with ITSELF across a
//! reopen. That is necessary and it is not sufficient: an engine that folded
//! adjacency wrongly but consistently passes every law in that file. The
//! question this file asks is the other one — **does the graph the engine serves
//! equal the graph the durable history MEANS?**
//!
//! The answer comes from `fgdb-reference`, which §15.2 licenses to be simple and
//! never optimized, and from `fgdb_sim::replay`, which materializes the same
//! commit stream into it. Both already exist and are already trusted; j0vu asks
//! for the existing differential to be REUSED rather than for a new oracle, and
//! that is what this is.
//!
//! **WHY THIS FILE IS HERE AND NOT IN `crates/fgdb/tests/`.** `fgdb-reference`
//! carries a registered dependency allowlist (§15.2) naming `fgdb-chronicle` a
//! CI-rejected import, precisely so the differential cannot be gutted by code
//! sharing. The verification layer is the only place the engine and the oracle
//! may both be visible, and making `fgdb` depend on the oracle — even as a
//! dev-dependency — would erode exactly the independence that makes agreement
//! mean something.
//!
//! **THE TWO SIDES MUST SHARE NOTHING BUT BYTES ON DISK.** The engine writes
//! through `fgdb::Database`; the oracle is fed by opening a *separate*
//! `CommitCoordinator` over the same directory after the `Database` has been
//! dropped. No handle, no fold, no block list and no snapshot crosses between
//! them — only the durable stream, which is the only thing they are supposed to
//! agree about.

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs, VfsFile};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BlockStoreCrashPoint, CAPSULE_OBJECT_KIND, Database, DatabaseCreateCrashPoint, DatabaseKeys,
    DatabaseState, DerivedPublicationStage, OpenError, ReadError, RebuildError, WriteBatch,
    WriteError, WriteMismatchPolicy,
};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CAPSULE_DIR, COMMIT_LOG_NAME, CommitCoordinator, CrashPoint};
use fgdb_chronicle::identity::{VerificationOperation, VerificationOutcome};
use fgdb_chronicle::store::{ROOT_FILE_NAME, StoreError as SlotStoreError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_sim::{
    replay,
    vfs::{FaultKind, FaultPlan, FaultVfs, Trigger},
};
use fgdb_strata::store::StoreError as BlockStoreError;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, VId};
use std::future::{Future, poll_fn};
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const KNOWS: RelationId = RelationId(1);
const WORKS_WITH: RelationId = RelationId(2);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

/// The oracle side opens the stream itself. These must be the keys the engine
/// wrote under or the capsules will not open — which is a property worth having
/// exercised rather than hidden behind a shared constructor.
fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        K_OID,
        NAMESPACE,
        DEK,
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-spine-diff-{}-{name}", std::process::id()))
}

/// A test authority that can hide exactly one namespace entry while every
/// other operation reaches the real faulting VFS. `Database::open_with_vfs`
/// must observe this refusal before Chronicle or Strata can escape to the
/// backing Unix namespace.
#[derive(Clone)]
struct GatedNamespaceVfs<V> {
    inner: V,
    gated_path: PathBuf,
    error_kind: io::ErrorKind,
}

impl<V> GatedNamespaceVfs<V> {
    fn new(inner: V, gated_path: PathBuf, error_kind: io::ErrorKind) -> Self {
        Self {
            inner,
            gated_path,
            error_kind,
        }
    }
}

impl<V: Vfs> Vfs for GatedNamespaceVfs<V> {
    type File = V::File;

    async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
        self.inner.open(path, options).await
    }

    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.metadata(path).await
    }

    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        if path == self.gated_path {
            return Err(io::Error::new(
                self.error_kind,
                "planted namespace-authority refusal",
            ));
        }
        self.inner.symlink_metadata(path).await
    }

    async fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        self.inner.set_permissions(path, permissions).await
    }

    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir(path).await
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path).await
    }

    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path).await
    }

    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir_all(path).await
    }

    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.inner.read_dir(path).await
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.inner.copy(from, to).await
    }

    async fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
        self.inner.hard_link(original, link).await
    }

    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(path).await
    }

    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.read_link(path).await
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path).await
    }

    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.inner.read_to_string(path).await
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.inner.write(path, contents).await
    }
}

/// A VFS gate that suspends one selected file sync only after the inner VFS
/// has completed it successfully. Pre-sync latency proves the lost-marker arm
/// of an interrupted D2; this gate proves the opposite arm, where storage has
/// the marker but the borrowed caller never observes the sync's return.
#[derive(Clone)]
struct PostSyncGateVfs<V> {
    inner: V,
    gate: Arc<PostSyncGate>,
}

struct PostSyncGate {
    path: PathBuf,
    target_sync: u32,
    state: Mutex<PostSyncGateState>,
}

#[derive(Default)]
struct PostSyncGateState {
    matching_syncs: u32,
    pending: bool,
}

impl<V> PostSyncGateVfs<V> {
    fn new(inner: V, path: PathBuf, target_sync: u32) -> Self {
        Self {
            inner,
            gate: Arc::new(PostSyncGate {
                path,
                target_sync,
                state: Mutex::new(PostSyncGateState::default()),
            }),
        }
    }

    fn pending_paths(&self) -> Vec<PathBuf> {
        let state = self.gate.state();
        state
            .pending
            .then(|| self.gate.path.clone())
            .into_iter()
            .collect()
    }

    fn matching_syncs(&self) -> u32 {
        self.gate.state().matching_syncs
    }
}

impl PostSyncGate {
    fn state(&self) -> MutexGuard<'_, PostSyncGateState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn arm_after_sync(&self, path: &Path) -> bool {
        if path != self.path {
            return false;
        }
        let mut state = self.state();
        state.matching_syncs = state.matching_syncs.saturating_add(1);
        if state.matching_syncs != self.target_sync {
            return false;
        }
        state.pending = true;
        true
    }

    fn retire(&self) {
        self.state().pending = false;
    }
}

struct PendingPostSync {
    gate: Arc<PostSyncGate>,
}

impl Drop for PendingPostSync {
    fn drop(&mut self) {
        self.gate.retire();
    }
}

struct PostSyncGateFile<F> {
    inner: F,
    path: PathBuf,
    gate: Arc<PostSyncGate>,
}

impl<F: VfsFile> AsyncRead for PostSyncGateFile<F> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<F: VfsFile> AsyncWrite for PostSyncGateFile<F> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<F: VfsFile> AsyncSeek for PostSyncGateFile<F> {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.inner).poll_seek(cx, pos)
    }
}

impl<F: VfsFile + Sync> VfsFile for PostSyncGateFile<F> {
    async fn metadata(&self) -> io::Result<Metadata> {
        self.inner.metadata().await
    }

    async fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all().await?;
        if self.gate.arm_after_sync(&self.path) {
            let _pending = PendingPostSync {
                gate: Arc::clone(&self.gate),
            };
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data().await
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.set_len(size).await
    }

    async fn set_permissions(&self, permissions: Permissions) -> io::Result<()> {
        self.inner.set_permissions(permissions).await
    }
}

impl<V: Vfs> Vfs for PostSyncGateVfs<V>
where
    V::File: Sync,
{
    type File = PostSyncGateFile<V::File>;

    async fn open(&self, path: &Path, options: &OpenOptions) -> io::Result<Self::File> {
        Ok(PostSyncGateFile {
            inner: self.inner.open(path, options).await?,
            path: path.to_path_buf(),
            gate: Arc::clone(&self.gate),
        })
    }

    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.metadata(path).await
    }

    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.symlink_metadata(path).await
    }

    async fn set_permissions(&self, path: &Path, permissions: Permissions) -> io::Result<()> {
        self.inner.set_permissions(path, permissions).await
    }

    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir(path).await
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path).await
    }

    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path).await
    }

    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir_all(path).await
    }

    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.inner.read_dir(path).await
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to).await
    }

    async fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.inner.copy(from, to).await
    }

    async fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
        self.inner.hard_link(original, link).await
    }

    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(path).await
    }

    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.read_link(path).await
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path).await
    }

    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.inner.read_to_string(path).await
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.inner.write(path, contents).await
    }
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn under_lab_with_root<T, Fut>(
    seed: u64,
    test: impl FnOnce(asupersync::Cx, CommitCx) -> Fut + Send + 'static,
) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(root, contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn assert_recovery_fence<T>(
    stage: DerivedPublicationStage,
    recovery: fgdb::RecoveryRequired,
    result: Result<T, ReadError>,
) {
    match result {
        Err(ReadError::RecoveryRequired(found)) => assert_eq!(
            found, recovery,
            "{stage:?}: every state-bearing read must carry the same recovery evidence"
        ),
        unexpected => assert!(
            matches!(&unexpected, Err(ReadError::RecoveryRequired(_))),
            "{stage:?}: every state-bearing read must carry recovery evidence"
        ),
    }
}

#[test]
fn database_create_waits_for_the_root_names_parent_barrier() {
    let parent = scratch("database-root-dirent");
    std::fs::create_dir_all(&parent).expect("existing parent");
    let interrupted = parent.join("interrupted");
    let durable = parent.join("durable");

    under_lab(0x599_0001, move |cx| async move {
        let interrupted_vfs = FaultVfs::unix(FaultPlan {
            dirent_loss: Trigger::Always,
            ..FaultPlan::faultless()
        });
        let error = Database::create_with_vfs_at_crash(
            &cx,
            interrupted_vfs.clone(),
            &interrupted,
            engine_keys(),
            DatabaseCreateCrashPoint::AfterDatabaseDirectorySyncBeforeParentSync,
        )
        .await
        .expect_err("the planted crash point must stop creation");
        assert!(
            matches!(
                error,
                OpenError::InjectedCreateCrash(
                    DatabaseCreateCrashPoint::AfterDatabaseDirectorySyncBeforeParentSync
                )
            ),
            "wrong planted crash result: {error}"
        );
        assert_eq!(
            interrupted_vfs.pending_dirent_ops(),
            1,
            "syncing the new directory inode cannot settle its name in the parent"
        );
        interrupted_vfs
            .crash()
            .await
            .expect("roll back the volatile database root name");
        let error = interrupted_vfs
            .symlink_metadata(&interrupted)
            .await
            .expect_err("the root name was legally losable before parent sync");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);

        let durable_vfs = FaultVfs::unix(FaultPlan {
            dirent_loss: Trigger::Always,
            ..FaultPlan::faultless()
        });
        let database = Database::create_with_vfs(&cx, durable_vfs.clone(), &durable, engine_keys())
            .await
            .expect("ordinary creation crosses the root barrier");
        assert_eq!(
            durable_vfs.pending_dirent_ops(),
            0,
            "a successful create cannot leave any namespace operation volatile"
        );
        drop(database);

        durable_vfs
            .crash()
            .await
            .expect("faultless namespace after successful create");
        assert!(
            durable_vfs
                .symlink_metadata(&durable)
                .await
                .expect("durable database root")
                .file_type()
                .is_dir(),
            "the database root must survive after create returns"
        );
        let reopened = Database::open_with_vfs(&cx, durable_vfs, &durable, engine_keys())
            .await
            .expect("the surviving root contains a reopenable database");
        assert_eq!(
            reopened.state(),
            DatabaseState::Healthy {
                published_frontier: CommitSeq(0),
            }
        );
    });
}

#[test]
fn database_open_obeys_the_supplied_vfs_namespace_authority() {
    let dir = scratch("open-vfs-namespace-authority");

    under_lab(0x3_2200_0001, move |cx| async move {
        let vfs = FaultVfs::unix(FaultPlan::faultless());
        let mut database = Database::create_with_vfs(&cx, vfs.clone(), &dir, engine_keys())
            .await
            .expect("create through the same VFS");
        database
            .write(&cx, vfs_fault_batch())
            .await
            .expect("publish one observable row");
        drop(database);

        assert!(
            std::fs::symlink_metadata(&dir)
                .expect("backing root exists")
                .is_dir(),
            "the planted refusal must come from the supplied VFS, not the backing namespace"
        );
        assert!(
            dir.join(CAPSULE_DIR).is_dir(),
            "the backing capsule directory must exist before the VFS hides it"
        );

        let deny_root =
            GatedNamespaceVfs::new(vfs.clone(), dir.clone(), io::ErrorKind::PermissionDenied);
        let refusal = Database::open_with_vfs(&cx, deny_root, &dir, engine_keys())
            .await
            .expect_err("the explicit VFS owns root admission");
        assert!(
            matches!(refusal, OpenError::Io(ref error) if error.kind() == io::ErrorKind::PermissionDenied),
            "root authority refusal must retain its typed I/O cause: {refusal}"
        );

        let hide_capsules =
            GatedNamespaceVfs::new(vfs.clone(), dir.join(CAPSULE_DIR), io::ErrorKind::NotFound);
        let refusal = Database::open_with_vfs(&cx, hide_capsules, &dir, engine_keys())
            .await
            .expect_err("the explicit VFS owns database-shape admission");
        assert!(
            matches!(
                refusal,
                OpenError::NotADatabase {
                    ref path,
                    missing: CAPSULE_DIR,
                } if path == &dir
            ),
            "a VFS-hidden capsule directory must make the root non-database: {refusal}"
        );

        let reopened = Database::open_with_vfs(&cx, vfs, &dir, engine_keys())
            .await
            .expect("the unmodified VFS still opens the database");
        assert_eq!(
            reopened.state(),
            DatabaseState::Healthy {
                published_frontier: CommitSeq(1),
            }
        );
        assert!(
            reopened
                .vertex(VId(1))
                .expect("read through reopened spine")
                .is_some(),
            "the positive path must serve the row written before reopen"
        );
    });
}

fn vfs_fault_batch() -> WriteBatch {
    let mut batch = WriteBatch::new(KNOWS);
    batch.create_vertex(
        VId(1),
        vec![LabelId(3)],
        vec![(PropertyKeyId(7), CanonicalScalar::Int(1))],
    );
    batch
}

fn block_store_fault_batch() -> WriteBatch {
    let mut batch = WriteBatch::new(KNOWS);
    batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch
}

async fn create_genesis(cx: &CommitCx, dir: &Path) {
    drop(
        Database::create(cx, dir, engine_keys())
            .await
            .expect("genesis database"),
    );
}

async fn assert_reopened_vertex_matches_oracle(cx: &CommitCx, dir: &Path) {
    let engine = Database::open(cx, dir, engine_keys())
        .await
        .expect("authoritative reopen repairs the root slot");
    assert_eq!(engine.frontier().expect("healthy frontier").0, 1);
    let engine_vertex = engine.vertex(VId(1)).expect("healthy read");
    let engine_verification = engine.crypto_verification_events().to_vec();
    assert!(
        engine_verification.iter().any(|event| {
            matches!(event.operation, VerificationOperation::ObjectRecovery)
                && matches!(event.outcome, VerificationOutcome::Accepted)
        }),
        "the product reopen must retain successful capsule verification evidence"
    );
    assert!(
        engine_verification
            .iter()
            .all(|event| matches!(event.outcome, VerificationOutcome::Accepted)),
        "a healthy product reopen must not manufacture a rejected verification event"
    );
    drop(engine);

    let coordinator = CommitCoordinator::open(cx, dir, oracle_keys())
        .await
        .expect("oracle opens the durable stream");
    let replayed = replay(cx, &coordinator).await.expect("stream replays");
    assert!(
        replayed.crypto_verification_events.iter().any(|event| {
            matches!(event.operation, VerificationOperation::ObjectRecovery)
                && matches!(event.outcome, VerificationOutcome::Accepted)
        }),
        "independent replay must retain its own successful verification evidence"
    );
    assert!(
        replayed
            .crypto_verification_events
            .iter()
            .all(|event| matches!(event.outcome, VerificationOutcome::Accepted)),
        "healthy replay must not manufacture a rejected verification event"
    );
    let graph = replayed
        .database
        .graph(GRAPH, BRANCH)
        .expect("oracle materialized the coordinate");
    let oracle_vertex = graph.vertex(VId(1)).expect("durable vertex exists");
    let engine_vertex = engine_vertex.expect("engine recovered durable vertex");
    assert_eq!(engine_vertex.labels, vec![LabelId(3)]);
    assert_eq!(
        engine_vertex.props,
        vec![(PropertyKeyId(7), CanonicalScalar::Int(1))]
    );
    assert_eq!(
        engine_vertex.labels,
        oracle_vertex.labels.iter().copied().collect::<Vec<_>>()
    );
    assert_eq!(
        engine_vertex.props,
        oracle_vertex
            .props
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect::<Vec<_>>()
    );
}

/// Write a history through the ENGINE's public surface only.
///
/// Deliberately not a straight line: parallel edges between one pair, a
/// self-loop, a second relation, and a hub with several destinations. A fixture
/// where every vertex has one neighbour would agree under almost any folding
/// mistake.
async fn write_history(cx: &CommitCx, dir: &Path) -> Vec<fgdb_types::CommitSeq> {
    let mut db = Database::create(cx, dir, engine_keys())
        .await
        .expect("creates");

    let mut first = WriteBatch::new(KNOWS);
    // VId(1) carries labels AND properties, VId(2) a label alone, the rest
    // nothing — so the vertex differential below compares three distinct
    // shapes rather than six copies of the empty row (fgdb-3xoi).
    first.create_vertex(
        VId(1),
        vec![LabelId(3), LabelId(5)],
        vec![
            (
                PropertyKeyId(7),
                CanonicalScalar::ucs_basic_text("ada").expect("admissible"),
            ),
            (PropertyKeyId(9), CanonicalScalar::Int(1815)),
        ],
    );
    first.create_vertex(VId(2), vec![LabelId(3)], vec![]);
    for vid in 3..=5u128 {
        first.create_vertex(VId(vid), vec![], vec![]);
    }
    // EDGE PROPERTIES (fgdb-yqor): EId(10) carries two, EId(11) none — the
    // edge differential below compares distinct shapes, not copies of empty.
    first.add_edge(
        EId(10),
        VId(1),
        VId(2),
        vec![
            (PropertyKeyId(11), CanonicalScalar::Int(2019)),
            (
                PropertyKeyId(13),
                CanonicalScalar::ucs_basic_text("close").expect("admissible"),
            ),
        ],
    );
    first.add_edge(EId(11), VId(1), VId(3), vec![]);
    let mut epochs = Vec::new();
    epochs.push(db.write(cx, first).await.expect("first batch commits"));

    // PARALLEL EDGES: same (src, dst), different EId. EId is the unconditional
    // parallel-edge discriminator (§4.1), so a fold keyed on the pair alone
    // collapses these and disagrees here. EId(12) is propertied AND deleted
    // below, so its retirement exercises the tombstone-restates-props path.
    let mut second = WriteBatch::new(KNOWS);
    second.add_edge(
        EId(12),
        VId(1),
        VId(2),
        vec![(PropertyKeyId(11), CanonicalScalar::Int(2020))],
    );
    second.add_edge(EId(13), VId(4), VId(4), vec![]); // self-loop
    epochs.push(db.write(cx, second).await.expect("second batch commits"));

    // A SECOND RELATION over an overlapping vertex set, propertied on the
    // surviving edge so the cross-relation read is compared with content.
    let mut third = WriteBatch::new(WORKS_WITH);
    third.add_edge(EId(14), VId(1), VId(5), vec![]);
    third.add_edge(
        EId(15),
        VId(2),
        VId(3),
        vec![(PropertyKeyId(11), CanonicalScalar::Bool(true))],
    );
    epochs.push(db.write(cx, third).await.expect("third batch commits"));

    // DELETES, with every before-image engine-derived (fgdb-p3ok). This is
    // the differential's sharpest teeth: the oracle's replay REFUSES a wrong
    // `before_version` or an inexact cascade, so these rows are validated at
    // apply time, not merely compared afterwards. VId(6) exists-then-goes in
    // one batch; VId(5) goes with its inbound WORKS_WITH edge cascaded.
    let mut fourth = WriteBatch::new(KNOWS);
    fourth.create_vertex(VId(6), vec![], vec![]);
    fourth.add_edge(EId(16), VId(6), VId(1), vec![]);
    fourth.add_edge(EId(17), VId(2), VId(4), vec![]);
    epochs.push(db.write(cx, fourth).await.expect("fourth batch commits"));
    let mut fifth = WriteBatch::new(KNOWS);
    fifth.delete_edge(EId(12)); // ONE of the two parallel edges — its twin survives
    fifth.delete_vertex(VId(6)); // cascades EId(16)
    fifth.delete_vertex(VId(5)); // cascades EId(14), a cross-relation edge
    epochs.push(db.write(cx, fifth).await.expect("fifth batch commits"));

    // UPDATES (fgdb-stb6), every before-image engine-derived and validated by
    // the oracle at replay (LabelBeforeMismatch / PropertyBeforeMismatch):
    // change a property, unset one, add and remove labels — including a
    // same-batch chain on one vertex so the derivation walks the prefix.
    let mut sixth = WriteBatch::new(KNOWS);
    sixth.set_vertex_property(
        VId(1),
        PropertyKeyId(7),
        Some(CanonicalScalar::ucs_basic_text("lovelace").expect("admissible")),
    );
    sixth.set_vertex_property(VId(1), PropertyKeyId(9), None);
    sixth.set_vertex_label(VId(1), LabelId(9), true);
    sixth.set_vertex_label(VId(1), LabelId(3), false);
    sixth.set_vertex_label(VId(3), LabelId(7), true);
    epochs.push(db.write(cx, sixth).await.expect("sixth batch commits"));

    // EDGE PROPERTY UPDATES (fgdb-ls5b), every before-image engine-derived
    // and oracle-validated at replay: change a value, unset one, add one to a
    // previously propertyless edge — two COMMUTING fields. Same-field
    // chains now fold (fgdb-w5-effects-normal-form-819.2) and are covered
    // by the independent net-effect differential below, not this fixture.
    let mut seventh = WriteBatch::new(KNOWS);
    seventh.set_edge_property(EId(10), PropertyKeyId(11), Some(CanonicalScalar::Int(2021)));
    seventh.set_edge_property(EId(10), PropertyKeyId(13), None);
    seventh.set_edge_property(
        EId(13),
        PropertyKeyId(17),
        Some(CanonicalScalar::Bool(true)),
    );
    seventh.set_edge_property(EId(13), PropertyKeyId(19), Some(CanonicalScalar::Int(3)));
    epochs.push(db.write(cx, seventh).await.expect("seventh batch commits"));
    epochs
}

/// **THE DIFFERENTIAL: the engine's answer equals the oracle's, for every vertex
/// and every relation in the fixture.**
#[test]
fn the_spine_agrees_with_the_reference_oracle() {
    let dir = scratch("agreement");
    under_lab(101, move |cx| async move {
        let cx = &cx;
        let _ = write_history(cx, &dir).await;

        // ENGINE SIDE. A fresh open, so the answers come from the durable path
        // rather than from the writer that produced them.
        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("reopens");
        let probes: Vec<(VId, RelationId)> = (1..=6u128)
            .flat_map(|vid| [(VId(vid), KNOWS), (VId(vid), WORKS_WITH)])
            .collect();
        let engine_answers: Vec<Vec<VId>> = probes
            .iter()
            .map(|(vid, rel)| engine.neighbours(*vid, *rel).expect("engine reads"))
            .collect();
        let engine_vertices: Vec<Option<fgdb::VertexRow>> = (1..=6u128)
            .map(|vid| engine.vertex(VId(vid)).expect("engine vertex reads"))
            .collect();
        let engine_edges: Vec<Option<fgdb::EdgeRecord>> = (10..=17u128)
            .map(|eid| engine.edge(EId(eid)).expect("engine edge reads"))
            .collect();
        drop(engine); // release the single-writer lease before the oracle opens

        // ORACLE SIDE. Its own coordinator over the same directory; nothing but
        // the bytes on disk crosses from the engine.
        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        let replayed = replay(cx, &coordinator).await.expect("the stream replays");
        let graph = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate");

        let mut agreements = 0usize;
        let mut nonempty = 0usize;
        for ((vid, rel), engine_answer) in probes.iter().zip(&engine_answers) {
            let oracle_answer = graph.neighbours(*vid, *rel);
            assert_eq!(
                engine_answer, &oracle_answer,
                "engine and oracle disagree for {vid:?} over {rel:?}"
            );
            agreements += 1;
            if !oracle_answer.is_empty() {
                nonempty += 1;
            }
        }

        // THE VERTEX DIFFERENTIAL (fgdb-3xoi): the engine's durable vertex
        // rows agree with the oracle's materialized vertices — existence,
        // labels, and properties, per vid.
        let mut labeled = 0usize;
        let mut propertied = 0usize;
        for (vid, engine_row) in (1..=6u128).map(VId).zip(&engine_vertices) {
            let oracle_vertex = graph.vertex(vid);
            assert_eq!(
                engine_row.is_some(),
                oracle_vertex.is_some(),
                "engine and oracle disagree about whether {vid:?} exists"
            );
            let (Some(row), Some(vertex)) = (engine_row, oracle_vertex) else {
                continue;
            };
            let oracle_labels: Vec<LabelId> = vertex.labels.iter().copied().collect();
            let oracle_props: Vec<(PropertyKeyId, CanonicalScalar)> = vertex
                .props
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect();
            assert_eq!(
                row.labels, oracle_labels,
                "engine and oracle disagree about {vid:?}'s labels"
            );
            assert_eq!(
                row.props, oracle_props,
                "engine and oracle disagree about {vid:?}'s properties"
            );
            assert_eq!(
                row.birth_ordinal, vertex.birth_ordinal,
                "engine and oracle disagree about {vid:?}'s birth ordinal"
            );
            labeled += usize::from(!row.labels.is_empty());
            propertied += usize::from(!row.props.is_empty());
        }
        assert!(
            labeled >= 2 && propertied >= 1,
            "the fixture must exercise labels and properties or the vertex \
             differential is agreement about emptiness, got {labeled} labeled \
             and {propertied} propertied"
        );

        // THE EDGE-LOOKUP DIFFERENTIAL: existence, endpoints, relation, and
        // properties agree with the oracle per EId — including the deleted
        // parallel edge, its surviving twin, and the cascade-retired edges.
        let mut live_edges = 0usize;
        let mut dead_edges = 0usize;
        let mut propertied_edges = 0usize;
        for (eid, engine_edge) in (10..=17u128).map(EId).zip(&engine_edges) {
            let oracle_edge = graph.edge(eid);
            assert_eq!(
                engine_edge.is_some(),
                oracle_edge.is_some(),
                "engine and oracle disagree about whether {eid:?} exists"
            );
            let (Some(record), Some(edge)) = (engine_edge, oracle_edge) else {
                dead_edges += 1;
                continue;
            };
            assert_eq!(
                (record.entry.src, record.entry.relation, record.entry.dst),
                (edge.src, edge.relation, edge.dst),
                "engine and oracle disagree about {eid:?}'s topology"
            );
            let oracle_props: Vec<_> = edge
                .props
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect();
            assert_eq!(
                record.props, oracle_props,
                "engine and oracle disagree about {eid:?}'s properties"
            );
            live_edges += 1;
            if !record.props.is_empty() {
                propertied_edges += 1;
            }
        }
        assert!(
            live_edges >= 3 && dead_edges >= 3 && propertied_edges >= 2,
            "the fixture must exercise live, retired, and propertied edges, got \
             {live_edges} live, {dead_edges} dead, {propertied_edges} propertied"
        );

        // ANTI-VACUITY. Agreement over twelve empty answers is not agreement
        // about anything: two implementations that both return nothing agree
        // perfectly. Pin that the fixture actually exercises the fold.
        assert_eq!(agreements, 12, "every probe must have been compared");
        assert!(
            nonempty >= 4,
            "the fixture must produce several non-empty answers or this law is \
             agreement about emptiness, got {nonempty}"
        );
    });
}

/// Agreement must survive a reopen on BOTH sides, not just the first read.
#[test]
fn agreement_survives_a_reopen() {
    let dir = scratch("reopen-agreement");
    under_lab(102, move |cx| async move {
        let cx = &cx;
        let _ = write_history(cx, &dir).await;

        let first = {
            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("reopens");
            engine.neighbours(VId(1), KNOWS).expect("reads")
        };
        let second = {
            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("reopens again");
            engine.neighbours(VId(1), KNOWS).expect("reads")
        };
        assert_eq!(first, second, "the engine must not drift across reopens");

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        let replayed = replay(cx, &coordinator).await.expect("the stream replays");
        let oracle = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("materialized")
            .neighbours(VId(1), KNOWS);
        assert_eq!(first, oracle, "and both must equal what the history means");
        assert!(
            !oracle.is_empty(),
            "vertex 1 has parallel KNOWS edges in the fixture; an empty answer here \
             means the fixture stopped exercising the fold"
        );
    });
}

/// Independent reconstructions of the derived delta window must agree: the
/// engine rebuilds `LocalDeltaBatchIndex` from the recovered chain at open,
/// and `fgdb_sim::replay` inserts in the same walk. They share nothing but
/// bytes on disk. A window the engine invented in memory would diverge.
#[test]
fn the_reopened_delta_index_equals_the_independent_replay() {
    let dir = scratch("delta-index-replay");
    under_lab(8198, move |cx| async move {
        let cx = &cx;
        let _ = write_history(cx, &dir).await;

        let (engine_index, engine_seqs, engine_frontier) = {
            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("reopens");
            let engine_index = engine.delta_index().expect("healthy rebuilt index").clone();
            let engine_seqs: Vec<_> = engine
                .delta_since(CommitSeq::ORIGIN)
                .expect("since origin")
                .map(|batch| batch.commit_seq())
                .collect();
            let engine_frontier = engine.delta_frontier().expect("reopened frontier");
            (engine_index, engine_seqs, engine_frontier)
        };
        assert!(
            engine_index.len() >= 2 && engine_frontier.0 >= 2,
            "the fixture must leave a non-trivial window or agreement is about \
             emptiness: frontier={engine_frontier:?} entries={} seqs={engine_seqs:?}",
            engine_index.len()
        );

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        let replayed = replay(cx, &coordinator).await.expect("the stream replays");
        let replay_seqs: Vec<_> = replayed
            .index
            .since(CommitSeq::ORIGIN)
            .expect("replay since origin")
            .map(|batch| batch.commit_seq())
            .collect();
        assert_eq!(
            engine_index,
            replayed.index,
            "engine rebuilt window must equal independent replay: \
             engine frontier={engine_frontier:?} entries={} seqs={engine_seqs:?}; \
             replay frontier={:?} entries={} seqs={replay_seqs:?}",
            engine_index.len(),
            replayed.index.frontier(),
            replayed.index.len()
        );
        assert_eq!(
            engine_seqs, replay_seqs,
            "delta_since(origin) seqs must match the replay walk: \
             engine={engine_seqs:?} replay={replay_seqs:?} frontier={engine_frontier:?}"
        );
    });
}

/// Every derived-publication boundary after Chronicle D2 has the same law:
/// the retained handle is totally fenced, and a fresh open agrees with the
/// independent reference replay about the commit that triggered the failure
/// (`fgdb-l96k`).
#[test]
fn every_post_d2_failure_fences_every_read_face_and_replays_to_the_oracle() {
    const STAGES: [DerivedPublicationStage; 9] = [
        DerivedPublicationStage::FoldCommittedTemplate,
        DerivedPublicationStage::SealPartition,
        DerivedPublicationStage::PublishEdgeBlocks,
        DerivedPublicationStage::PublishVertexPatches,
        DerivedPublicationStage::PublishPartitionRoot,
        DerivedPublicationStage::PublishManifest,
        DerivedPublicationStage::PublishRootSlot,
        DerivedPublicationStage::RefreshEdgeSnapshot,
        DerivedPublicationStage::RefreshVertexSnapshot,
    ];

    for (ordinal, stage) in STAGES.into_iter().enumerate() {
        let dir = scratch(&format!("post-d2-{ordinal}-{stage:?}"));
        under_lab(1_200 + ordinal as u64, move |cx| async move {
            let cx = &cx;
            let mut db = Database::create(cx, &dir, engine_keys())
                .await
                .expect("creates");

            let mut first = WriteBatch::new(KNOWS);
            first.create_vertex(
                VId(1),
                vec![LabelId(3)],
                vec![(PropertyKeyId(7), CanonicalScalar::Int(1))],
            );
            first.create_vertex(VId(2), vec![], vec![]);
            first.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, first).await.expect("first commit publishes");
            let published_view = db
                .pinned_read_view()
                .expect("healthy handle issues a view of the published generation");
            let published_neighbours = published_view
                .neighbours(VId(1), KNOWS)
                .expect("issued view reads its pinned generation");
            let published_delta_frontier = published_view.delta_frontier();
            let published_delta_sequences = published_view
                .delta_since(CommitSeq::ORIGIN)
                .expect("published generation retains its full delta suffix")
                .map(|batch| batch.commit_seq())
                .collect::<Vec<_>>();
            assert_eq!(published_delta_frontier, published_view.frontier());
            assert_eq!(published_delta_sequences, vec![CommitSeq(1)]);
            assert!(std::ptr::eq(
                db.delta_index().expect("healthy delta window"),
                published_view.delta_index()
            ));

            let mut second = WriteBatch::new(KNOWS);
            second.create_vertex(
                VId(3),
                vec![LabelId(5)],
                vec![(PropertyKeyId(7), CanonicalScalar::Int(2))],
            );
            second.add_edge(
                EId(11),
                VId(1),
                VId(3),
                vec![(PropertyKeyId(11), CanonicalScalar::Bool(true))],
            );
            let error = db
                .write_with_publication_failure(cx, second, stage)
                .await
                .expect_err("the named post-D2 stage must fail");
            let (recovery, source) = match error {
                WriteError::CommittedNeedsRecovery { recovery, source } => (recovery, source),
                unexpected => {
                    assert!(
                        matches!(&unexpected, WriteError::CommittedNeedsRecovery { .. }),
                        "{stage:?}: injected failure returned the wrong error: {unexpected:?}"
                    );
                    return;
                }
            };
            assert_eq!(recovery.durable_frontier.0, 2, "{stage:?}");
            assert_eq!(recovery.published_frontier.0, 1, "{stage:?}");
            assert_eq!(recovery.failed_stage, stage);
            match *source {
                RebuildError::InjectedPublicationFailure(found) => assert_eq!(
                    found, stage,
                    "{stage:?}: the source must identify the injection boundary"
                ),
                unexpected => assert!(
                    matches!(&unexpected, RebuildError::InjectedPublicationFailure(_)),
                    "{stage:?}: the source must identify the injection boundary"
                ),
            }
            assert_eq!(
                db.state(),
                DatabaseState::NeedsAuthoritativeRecovery(recovery)
            );

            assert_recovery_fence(stage, recovery, db.frontier());
            assert_recovery_fence(stage, recovery, db.manifest());
            assert_recovery_fence(stage, recovery, db.partition_root());
            assert_recovery_fence(stage, recovery, db.pinned_read_view());
            assert_recovery_fence(stage, recovery, db.neighbours(VId(1), KNOWS));
            assert_recovery_fence(
                stage,
                recovery,
                db.neighbours_at(VId(1), KNOWS, recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.in_neighbours(VId(3), KNOWS));
            assert_recovery_fence(
                stage,
                recovery,
                db.in_neighbours_at(VId(3), KNOWS, recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.edge(EId(11)));
            assert_recovery_fence(
                stage,
                recovery,
                db.edge_at(EId(11), recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.vertex(VId(3)));
            assert_recovery_fence(
                stage,
                recovery,
                db.vertex_at(VId(3), recovery.published_frontier),
            );
            assert_recovery_fence(stage, recovery, db.delta_frontier());
            assert_recovery_fence(stage, recovery, db.delta_index());
            assert_recovery_fence(stage, recovery, db.delta_since(CommitSeq::ORIGIN));
            assert_recovery_fence(stage, recovery, db.vertices());
            assert_recovery_fence(stage, recovery, db.vertices_at(recovery.published_frontier));
            assert_recovery_fence(stage, recovery, db.edges());
            assert_recovery_fence(stage, recovery, db.edges_at(recovery.published_frontier));
            assert_eq!(published_view.frontier(), recovery.published_frontier);
            assert_eq!(
                published_view.delta_frontier(),
                published_delta_frontier,
                "{stage:?}: a pre-fence view's delta frontier moved after D2"
            );
            assert_eq!(
                published_view
                    .delta_since(CommitSeq::ORIGIN)
                    .expect("pre-fence delta suffix remains readable")
                    .map(|batch| batch.commit_seq())
                    .collect::<Vec<_>>(),
                published_delta_sequences,
                "{stage:?}: a pre-fence view absorbed the uncertain commit's delta batch"
            );
            assert_eq!(
                published_view
                    .neighbours(VId(1), KNOWS)
                    .expect("pre-fence view remains readable"),
                published_neighbours,
                "{stage:?}: a view issued before the uncertain commit must retain its exact durable generation"
            );
            assert_eq!(
                published_view
                    .vertex(VId(3))
                    .expect("pre-fence view remains readable"),
                None,
                "{stage:?}: the pre-fence generation must not absorb the uncertain commit"
            );
            let compact_error = db
                .compact(cx)
                .await
                .expect_err("a fenced handle must refuse maintenance");
            let found = match compact_error {
                RebuildError::HandleNotHealthy(found) => found,
                unexpected => {
                    assert!(
                        matches!(&unexpected, RebuildError::HandleNotHealthy(_)),
                        "{stage:?}: maintenance returned the wrong fence: {unexpected:?}"
                    );
                    return;
                }
            };
            assert_eq!(found, DatabaseState::NeedsAuthoritativeRecovery(recovery));
            let mut third = WriteBatch::new(KNOWS);
            third.create_vertex(VId(4), vec![], vec![]);
            match db.write(cx, third).await {
                Err(WriteError::RecoveryRequired(found)) => assert_eq!(found, recovery),
                unexpected => assert!(
                    matches!(&unexpected, Err(WriteError::RecoveryRequired(_))),
                    "{stage:?}: fenced writer returned the wrong outcome: {unexpected:?}"
                ),
            }
            drop(db);

            // ENGINE SIDE: only the directory and keys cross the reopen.
            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("authoritative reopen recovers the durable commit");
            let engine_neighbours = engine.neighbours(VId(1), KNOWS).expect("reads");
            let engine_vertices = engine.vertices().expect("reads");
            let engine_edges = engine.edges().expect("reads");
            let engine_delta_frontier = engine.delta_frontier().expect("rebuilds delta frontier");
            let engine_delta_sequences = engine
                .delta_since(CommitSeq::ORIGIN)
                .expect("rebuilds committed delta suffix")
                .map(|batch| batch.commit_seq())
                .collect::<Vec<_>>();
            assert_eq!(
                engine.frontier().expect("rebuilds graph frontier"),
                CommitSeq(2)
            );
            assert_eq!(engine_delta_frontier, CommitSeq(2));
            assert_eq!(engine_delta_sequences, vec![CommitSeq(1), CommitSeq(2)]);
            drop(engine);

            // ORACLE SIDE: independently replay the Chronicle bytes and compare
            // the whole visible universe, not just one point lookup.
            let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
                .await
                .expect("oracle opens");
            let replayed = replay(cx, &coordinator).await.expect("stream replays");
            let graph = replayed
                .database
                .graph(GRAPH, BRANCH)
                .expect("oracle materialized the coordinate");
            assert_eq!(
                engine_neighbours,
                graph.neighbours(VId(1), KNOWS),
                "{stage:?}"
            );
            assert_eq!(engine_vertices.len(), graph.vertex_count(), "{stage:?}");
            assert_eq!(engine_edges.len(), graph.edge_count(), "{stage:?}");
            for row in &engine_vertices {
                let oracle = graph.vertex(row.vid).expect("engine-only vertex");
                assert_eq!(
                    row.labels,
                    oracle.labels.iter().copied().collect::<Vec<_>>()
                );
                assert_eq!(
                    row.props,
                    oracle
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
            for record in &engine_edges {
                let oracle = graph.edge(record.entry.eid).expect("engine-only edge");
                assert_eq!(
                    (record.entry.src, record.entry.relation, record.entry.dst),
                    (oracle.src, oracle.relation, oracle.dst)
                );
                assert_eq!(
                    record.props,
                    oracle
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(
                engine_vertices.len(),
                3,
                "{stage:?}: second commit must exist"
            );
            assert_eq!(engine_edges.len(), 2, "{stage:?}: second commit must exist");
        });
    }
}

/// The integrated `Database` must not erase the faultable root-store seam.
/// A byte budget measured by the same public write first proves how many bytes
/// an honest write flushes; one byte less must reach Chronicle D2, fail at
/// `manifest.root`, fence the handle, and recover the committed vertex exactly
/// once from the authoritative stream.
#[test]
fn root_slot_enospc_fences_the_database_and_reopen_matches_the_oracle() {
    let control_dir = scratch("database-vfs-enospc-control");
    let faulted_dir = scratch("database-vfs-enospc-faulted");
    under_lab(1_210, move |cx| async move {
        let cx = &cx;
        create_genesis(cx, &control_dir).await;
        create_genesis(cx, &faulted_dir).await;

        let control_vfs = FaultVfs::unix(FaultPlan::faultless());
        let mut control =
            Database::open_with_vfs(cx, control_vfs.clone(), &control_dir, engine_keys())
                .await
                .expect("control opens through the VFS");
        assert_eq!(
            control
                .write(cx, vfs_fault_batch())
                .await
                .expect("control commits")
                .0,
            1
        );
        let honest_bytes = control_vfs.flushed_bytes();
        assert!(
            honest_bytes > 1,
            "a zero-byte control would make the ENOSPC placement vacuous"
        );
        assert_eq!(control_vfs.events(), Vec::new());
        drop(control);

        let faulted_vfs = FaultVfs::unix(FaultPlan {
            space_budget: Some(honest_bytes - 1),
            ..FaultPlan::faultless()
        });
        let mut db = Database::open_with_vfs(cx, faulted_vfs.clone(), &faulted_dir, engine_keys())
            .await
            .expect("faulted database opens");
        let error = db
            .write(cx, vfs_fault_batch())
            .await
            .expect_err("the root-slot barrier must exhaust the measured budget");
        let committed = match &error {
            WriteError::CommittedNeedsRecovery { recovery, source } => {
                Some((*recovery, source.as_ref()))
            }
            _ => None,
        };
        assert!(
            committed.is_some(),
            "root-slot ENOSPC returned the wrong error: {error:?}"
        );
        let Some((recovery, source)) = committed else {
            return;
        };
        assert_eq!(recovery.durable_frontier.0, 1);
        assert_eq!(recovery.published_frontier.0, 0);
        assert_eq!(
            recovery.failed_stage,
            DerivedPublicationStage::PublishRootSlot
        );
        let raw_os_error = match source {
            RebuildError::Slot(SlotStoreError::Io(error)) => error.raw_os_error(),
            _ => None,
        };
        assert_eq!(
            raw_os_error,
            Some(28),
            "root-slot ENOSPC lost its typed source: {source:?}"
        );
        assert_eq!(
            db.state(),
            DatabaseState::NeedsAuthoritativeRecovery(recovery)
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            recovery,
            db.vertex(VId(1)),
        );
        match db.write(cx, vfs_fault_batch()).await {
            Err(WriteError::RecoveryRequired(found)) => assert_eq!(found, recovery),
            unexpected => assert!(
                matches!(&unexpected, Err(WriteError::RecoveryRequired(_))),
                "fenced writer returned the wrong outcome: {unexpected:?}"
            ),
        }

        let events = faulted_vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned fault must fire");
        assert!(matches!(events[0].kind, FaultKind::OutOfSpace { .. }));
        assert_eq!(events[0].path, faulted_dir.join(ROOT_FILE_NAME));

        faulted_vfs.crash().await.expect("simulate process loss");
        drop(db);
        assert_reopened_vertex_matches_oracle(cx, &faulted_dir).await;
    });
}

/// A lying root-slot fsync is harder than an ordinary I/O error: the barrier
/// returns success. `RootStore`'s post-barrier reread must detect the lie,
/// `Database` must fence rather than swap snapshots, and crash/reopen must
/// still derive the acknowledged commit from Chronicle.
#[test]
fn root_slot_fsync_lie_is_detected_fenced_and_recovered_from_chronicle() {
    let dir = scratch("database-vfs-root-slot-lie");
    under_lab(1_211, move |cx| async move {
        let cx = &cx;
        create_genesis(cx, &dir).await;

        // A database write performs D1, D2, then the root-slot barrier. The
        // event-path assertion below pins that arithmetic and fails loudly if
        // another eligible sync is introduced ahead of the slot.
        let vfs = FaultVfs::unix(FaultPlan {
            fsync_lie: Trigger::Nth(3),
            ..FaultPlan::faultless()
        });
        let mut db = Database::open_with_vfs(cx, vfs.clone(), &dir, engine_keys())
            .await
            .expect("faulted database opens");
        let error = db
            .write(cx, vfs_fault_batch())
            .await
            .expect_err("the evidence reread must expose the fsync lie");
        let committed = match &error {
            WriteError::CommittedNeedsRecovery { recovery, source } => {
                Some((*recovery, source.as_ref()))
            }
            _ => None,
        };
        assert!(
            committed.is_some(),
            "root-slot fsync lie returned the wrong error: {error:?}"
        );
        let Some((recovery, source)) = committed else {
            return;
        };
        assert_eq!(recovery.durable_frontier.0, 1);
        assert_eq!(recovery.published_frontier.0, 0);
        assert_eq!(
            recovery.failed_stage,
            DerivedPublicationStage::PublishRootSlot
        );
        assert!(
            matches!(
                source,
                RebuildError::Slot(SlotStoreError::PublicationNotObservable {
                    expected_generation: 2
                })
            ),
            "the reread must name the unobservable generation: {source:?}"
        );
        assert_eq!(
            db.state(),
            DatabaseState::NeedsAuthoritativeRecovery(recovery)
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            recovery,
            db.frontier(),
        );

        let events = vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned lie must fire");
        assert!(matches!(events[0].kind, FaultKind::FsyncLie { .. }));
        assert_eq!(events[0].path, dir.join(ROOT_FILE_NAME));

        vfs.crash().await.expect("simulate process loss");
        drop(db);
        assert_reopened_vertex_matches_oracle(cx, &dir).await;
    });
}

/// Dropping an async write is the cancellation boundary callers actually own.
/// Chronicle poisons its coordinator before appending the marker, but a
/// cancelled outer future cannot execute the error arm that copies that fact
/// into `DatabaseState`. The public handle must therefore be fenced before the
/// first commit await, not after the await reports an outcome.
#[test]
fn commit_d2_cancellation_leaves_the_borrowed_handle_fenced_and_recoverable() {
    let dir = scratch("database-vfs-commit-d2-cancel");
    under_lab_with_root(0x8_5e00_0001, move |root, cx| async move {
        let cx = &cx;
        create_genesis(cx, &dir).await;

        // Make D2's primary file sync report a planted success without
        // clearing the dirty marker bytes. That keeps the reinforcement
        // eligible for FaultVfs latency, so the fifth eligible sync is the
        // exact clean-up barrier the bead names rather than an earlier file or
        // directory boundary.
        let vfs = FaultVfs::unix_with_clock(
            FaultPlan {
                fsync_lie: Trigger::At(2),
                latency: Trigger::At(5),
                latency_micros: 60_000_000,
                ..FaultPlan::faultless()
            },
            root,
        );
        let mut db = Database::open_with_vfs(cx, vfs.clone(), &dir, engine_keys())
            .await
            .expect("faultable database opens");

        // An explicit pre-marker refusal returns through the outer future, so
        // the handle can prove that Chronicle stayed unpoisoned and restore
        // its original healthy state. This positive control also proves the
        // fifth latency trigger still belongs to the ordinary write below.
        let safe_refusal = db
            .write_with_crash(cx, vfs_fault_batch(), Some(CrashPoint::BeforeCapsule))
            .await
            .expect_err("the planted pre-capsule refusal must fire");
        assert!(
            matches!(safe_refusal, WriteError::Commit(_)),
            "pre-marker refusal lost its typed commit error: {safe_refusal:?}"
        );
        assert_eq!(
            db.state(),
            DatabaseState::Healthy {
                published_frontier: CommitSeq(0)
            },
            "an explicit refusal before Chronicle poisoning is safely retryable"
        );

        let mut write = Box::pin(db.write(cx, vfs_fault_batch()));
        let pending = poll_fn(|task_cx| {
            if let Poll::Ready(result) = write.as_mut().poll(task_cx) {
                return Poll::Ready(Err(format!(
                    "write completed before cancellation reached D2 reinforcement: {result:?}"
                )));
            }
            let pending = vfs.pending_latency_paths();
            if pending.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(pending))
            }
        })
        .await
        .expect("the write must suspend at an injected durability boundary");
        assert_eq!(
            pending,
            vec![dir.join(COMMIT_LOG_NAME)],
            "cancellation must target Chronicle's commit-log D2 reinforcement"
        );

        drop(write);
        assert!(
            vfs.pending_latency_paths().is_empty(),
            "dropping the write must retire its pending latency waiter"
        );
        assert_eq!(
            vfs.events().len(),
            1,
            "only the planted D2 primary-sync lie may complete before cancellation"
        );
        assert!(
            matches!(vfs.events()[0].kind, FaultKind::FsyncLie { .. })
                && vfs.events()[0].path == dir.join(COMMIT_LOG_NAME),
            "the completed fault must be the planted commit-log D2 lie: {:?}",
            vfs.events()
        );

        let expected = DatabaseState::CommitOutcomeUnknown {
            published_frontier: CommitSeq(0),
        };
        assert_eq!(
            db.state(),
            expected,
            "cancelled D2 left the pre-commit snapshot callable"
        );
        assert!(matches!(
            db.frontier(),
            Err(ReadError::CommitOutcomeUnknown {
                published_frontier: CommitSeq(0)
            })
        ));
        assert!(matches!(
            db.compact(cx).await,
            Err(RebuildError::HandleNotHealthy(
                DatabaseState::CommitOutcomeUnknown {
                    published_frontier: CommitSeq(0)
                }
            ))
        ));
        let refused = db.write(cx, vfs_fault_batch()).await;
        assert!(
            matches!(
                refused,
                Err(WriteError::HandleCommitOutcomeUnknown {
                    published_frontier: CommitSeq(0)
                })
            ),
            "fenced database accepted another write: {refused:?}"
        );

        // The planted primary-D2 lie leaves the marker volatile and the
        // cancelled reinforcement never repairs it, so process loss removes
        // the marker. The product reopen and independent replay must agree on
        // that absence; the stale handle was not allowed to guess it.
        vfs.crash().await.expect("simulate process loss");
        drop(db);
        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("authoritative reopen decides the cancelled marker");
        let engine_frontier = engine.frontier().expect("healthy recovered frontier");
        let engine_vertex = engine.vertex(VId(1)).expect("healthy recovered read");
        drop(engine);

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens the post-crash stream");
        let replayed = replay(cx, &coordinator).await.expect("stream replays");
        assert_eq!(engine_frontier, replayed.index.frontier());
        assert_eq!(engine_frontier, CommitSeq::ORIGIN);
        assert!(
            engine_vertex.is_none(),
            "the cancelled reinforcement cannot make a lying primary D2 durable"
        );
        assert!(
            replayed.database.graph(GRAPH, BRANCH).is_none(),
            "the independent oracle found a commit the product correctly discarded"
        );
    });
}

/// The surviving half of the same uncertain D2 outcome. The gate lets both
/// commit-log syncs finish against the real fault model, then suspends before
/// the reinforcement future can report success. Dropping the public write at
/// that boundary must preserve the same conservative fence even though a
/// process loss and authoritative reopen ultimately retain the marker.
#[test]
fn commit_d2_cancellation_with_a_durable_marker_recovers_as_committed() {
    let dir = scratch("database-vfs-commit-d2-cancel-durable");
    under_lab(0x8_5e00_0002, move |cx| async move {
        let cx = &cx;
        create_genesis(cx, &dir).await;

        let durable_vfs = FaultVfs::unix(FaultPlan::faultless());
        let gated_vfs = PostSyncGateVfs::new(durable_vfs.clone(), dir.join(COMMIT_LOG_NAME), 2);
        let mut db = Database::open_with_vfs(cx, gated_vfs.clone(), &dir, engine_keys())
            .await
            .expect("post-sync-gated database opens");

        let mut write = Box::pin(db.write(cx, vfs_fault_batch()));
        let pending = poll_fn(|task_cx| {
            if let Poll::Ready(result) = write.as_mut().poll(task_cx) {
                return Poll::Ready(Err(format!(
                    "write completed before the post-D2 gate armed: {result:?}"
                )));
            }
            let pending = gated_vfs.pending_paths();
            if pending.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(pending))
            }
        })
        .await
        .expect("the write must suspend after a completed D2 reinforcement");
        assert_eq!(
            pending,
            vec![dir.join(COMMIT_LOG_NAME)],
            "the post-sync gate must target Chronicle's commit log"
        );
        assert_eq!(
            gated_vfs.matching_syncs(),
            2,
            "the gate must arm after D2's reinforcement, not its primary sync"
        );

        drop(write);
        assert!(
            gated_vfs.pending_paths().is_empty(),
            "dropping the write must retire the post-sync waiter"
        );
        assert_eq!(
            durable_vfs.events(),
            Vec::new(),
            "the surviving outcome must not depend on a planted fault"
        );

        let expected = DatabaseState::CommitOutcomeUnknown {
            published_frontier: CommitSeq(0),
        };
        assert_eq!(
            db.state(),
            expected,
            "a durable marker cannot make a cancelled caller know the outcome"
        );
        assert!(matches!(
            db.frontier(),
            Err(ReadError::CommitOutcomeUnknown {
                published_frontier: CommitSeq(0)
            })
        ));
        assert!(matches!(
            db.compact(cx).await,
            Err(RebuildError::HandleNotHealthy(
                DatabaseState::CommitOutcomeUnknown {
                    published_frontier: CommitSeq(0)
                }
            ))
        ));
        let refused = db.write(cx, vfs_fault_batch()).await;
        assert!(
            matches!(
                refused,
                Err(WriteError::HandleCommitOutcomeUnknown {
                    published_frontier: CommitSeq(0)
                })
            ),
            "fenced database accepted another write: {refused:?}"
        );

        durable_vfs.crash().await.expect("simulate process loss");
        drop(db);
        assert_reopened_vertex_matches_oracle(cx, &dir).await;
    });
}

/// Dropping an async write is the cancellation boundary callers actually own.
/// This drives the ordinary write until the root-slot fsync is observably
/// suspended, drops that future, and then proves the borrowed `Database` was
/// already fenced before the await. Chronicle D2 must survive the simulated
/// process loss and ordinary reopen must recover it exactly once.
#[test]
fn root_slot_cancellation_leaves_the_borrowed_handle_fenced_and_recoverable() {
    let dir = scratch("database-vfs-root-slot-cancel");
    under_lab_with_root(1_212, move |root, cx| async move {
        let cx = &cx;
        create_genesis(cx, &dir).await;

        // Four Chronicle durability boundaries precede derived publication
        // for this reopened database; the root-slot barrier is fifth. The
        // pending-path observation below independently pins that ordinal to
        // manifest.root before the write future is dropped, so protocol drift
        // cannot silently cancel a different operation.
        let vfs = FaultVfs::unix_with_clock(
            FaultPlan {
                latency: Trigger::Nth(5),
                latency_micros: 60_000_000,
                ..FaultPlan::faultless()
            },
            root,
        );
        let mut db = Database::open_with_vfs(cx, vfs.clone(), &dir, engine_keys())
            .await
            .expect("faultable database opens");

        let mut write = Box::pin(db.write(cx, vfs_fault_batch()));
        let pending = poll_fn(|task_cx| {
            if let Poll::Ready(result) = write.as_mut().poll(task_cx) {
                return Poll::Ready(Err(format!(
                    "write completed before cancellation reached root publication: {result:?}"
                )));
            }
            let pending = vfs.pending_latency_paths();
            if pending.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(pending))
            }
        })
        .await
        .expect("the write must suspend at an injected durability boundary");
        assert_eq!(
            pending,
            vec![dir.join(ROOT_FILE_NAME)],
            "cancellation must target the post-D2 root-slot sync"
        );

        // This is the cancellation itself. It releases the exclusive mutable
        // borrow and must leave `db` in the recovery state installed before
        // RootStore's await.
        drop(write);
        assert!(
            vfs.pending_latency_paths().is_empty(),
            "dropping the write must retire its pending latency waiter"
        );
        assert_eq!(
            vfs.events(),
            Vec::new(),
            "a cancelled delay must not be reported as fully awaited"
        );

        let state = db.state();
        assert!(
            matches!(state, DatabaseState::NeedsAuthoritativeRecovery(_)),
            "cancelled post-D2 handle remained callable: {state:?}"
        );
        let DatabaseState::NeedsAuthoritativeRecovery(recovery) = state else {
            return;
        };
        assert_eq!(recovery.durable_frontier.0, 1);
        assert_eq!(recovery.published_frontier.0, 0);
        assert_eq!(
            recovery.failed_stage,
            DerivedPublicationStage::PublishRootSlot
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            recovery,
            db.frontier(),
        );
        let refused = db.write(cx, vfs_fault_batch()).await;
        assert!(
            matches!(refused, Err(WriteError::RecoveryRequired(_))),
            "fenced database accepted another write: {refused:?}"
        );
        let Err(WriteError::RecoveryRequired(found)) = refused else {
            return;
        };
        assert_eq!(found, recovery);

        vfs.crash().await.expect("simulate process loss");
        drop(db);
        assert_reopened_vertex_matches_oracle(cx, &dir).await;
    });
}

/// Compaction reaches the same cancellable `manifest.root` publication as a
/// write, even though Chronicle's semantic frontier does not advance. Dropping
/// that future while the root-slot sync is pending leaves the publication
/// outcome unknown to the borrowed handle: it must not retain its old
/// generation as `Healthy` and later try to reuse the generation that may have
/// reached durable storage.
#[test]
fn compaction_root_slot_cancellation_fences_the_borrowed_handle() {
    let dir = scratch("database-vfs-compact-root-slot-cancel");
    under_lab_with_root(1_213, move |root, cx| async move {
        let cx = &cx;
        create_genesis(cx, &dir).await;
        let mut seeded = Database::open(cx, &dir, engine_keys())
            .await
            .expect("seed database opens");
        seeded
            .write(cx, vfs_fault_batch())
            .await
            .expect("seed commit reaches frontier one");
        drop(seeded);

        // Compaction performs no Chronicle write. Its only eligible sync on
        // this VFS is the replacement root slot, so the first delay is the
        // exact cancellation boundary and the observed path independently
        // prevents this witness from drifting to another operation.
        let vfs = FaultVfs::unix_with_clock(
            FaultPlan {
                latency: Trigger::Nth(1),
                latency_micros: 60_000_000,
                ..FaultPlan::faultless()
            },
            root,
        );
        let mut db = Database::open_with_vfs(cx, vfs.clone(), &dir, engine_keys())
            .await
            .expect("faultable database opens");

        let mut compaction = Box::pin(db.compact(cx));
        let pending = poll_fn(|task_cx| {
            if let Poll::Ready(result) = compaction.as_mut().poll(task_cx) {
                return Poll::Ready(Err(format!(
                    "compaction completed before cancellation reached root publication: {result:?}"
                )));
            }
            let pending = vfs.pending_latency_paths();
            if pending.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(Ok(pending))
            }
        })
        .await
        .expect("compaction must suspend at its root durability boundary");
        assert_eq!(
            pending,
            vec![dir.join(ROOT_FILE_NAME)],
            "compaction cancellation must target the replacement root-slot sync"
        );

        drop(compaction);
        assert!(
            vfs.pending_latency_paths().is_empty(),
            "dropping compaction must retire its pending latency waiter"
        );
        assert_eq!(
            vfs.events(),
            Vec::new(),
            "a cancelled compaction delay must not be reported as fully awaited"
        );

        let state = db.state();
        assert!(
            matches!(state, DatabaseState::NeedsAuthoritativeRecovery(_)),
            "cancelled root publication left the old compaction generation callable: {state:?}"
        );
        let DatabaseState::NeedsAuthoritativeRecovery(fence) = state else {
            return;
        };
        assert_eq!(fence.durable_frontier, CommitSeq(1));
        assert_eq!(fence.published_frontier, CommitSeq(1));
        assert_eq!(fence.failed_stage, DerivedPublicationStage::PublishRootSlot);
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            fence,
            db.frontier(),
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            fence,
            db.vertex(VId(1)),
        );
        assert_recovery_fence(
            DerivedPublicationStage::PublishRootSlot,
            fence,
            db.manifest(),
        );
        let refused = db.write(cx, vfs_fault_batch()).await;
        assert!(
            matches!(refused, Err(WriteError::RecoveryRequired(_))),
            "fenced post-compaction handle accepted another write: {refused:?}"
        );
        let Err(WriteError::RecoveryRequired(found)) = refused else {
            return;
        };
        assert_eq!(found.durable_frontier, fence.durable_frontier);
        assert_eq!(found.published_frontier, fence.published_frontier);
        assert_eq!(found.failed_stage, fence.failed_stage);

        // Process loss discards the unsynced replacement slot. Consuming the
        // fenced handle must reopen from Chronicle plus the last durable root,
        // producing a healthy handle at the same semantic frontier.
        vfs.crash().await.expect("simulate process loss");
        let recovered = db
            .recover_authoritatively(cx)
            .await
            .expect("authoritative same-VFS reopen repairs cancellation");
        assert_eq!(
            recovered.state(),
            DatabaseState::Healthy {
                published_frontier: CommitSeq(1)
            }
        );
        assert!(
            recovered
                .vertex(VId(1))
                .expect("recovered graph is readable")
                .is_some(),
            "authoritative recovery must preserve the committed vertex"
        );
    });
}

/// Strata's production publisher already models the two instants around
/// canonical-name publication. This proves the integrated database does not
/// erase that seam: D2 remains authoritative, the borrowed handle is fenced at
/// `PublishEdgeBlocks`, and reopen repairs both sides of the rename boundary.
#[test]
fn strata_block_publication_crashes_fence_and_recover_the_integrated_spine() {
    let scenarios = [
        (
            "staging-durable",
            BlockStoreCrashPoint::AfterStagingFileSyncBeforePublication,
            "complete staging inode before canonical publication",
        ),
        (
            "canonical-inode-durable",
            BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync,
            "strata block inode durable before directory entry",
        ),
    ];

    under_lab(1_213, move |cx| async move {
        let cx = &cx;
        for (name, crash_at, expected_source) in scenarios {
            let dir = scratch(&format!("database-strata-{name}"));
            create_genesis(cx, &dir).await;
            let mut db = Database::open(cx, &dir, engine_keys())
                .await
                .expect("database opens");

            let error = db
                .write_with_block_store_crash(cx, block_store_fault_batch(), crash_at)
                .await
                .expect_err("the real Strata publication must stop at its crash point");
            let committed = match &error {
                WriteError::CommittedNeedsRecovery { recovery, source } => {
                    Some((*recovery, source.as_ref()))
                }
                _ => None,
            };
            assert!(
                committed.is_some(),
                "{name}: Strata crash returned the wrong error: {error:?}"
            );
            let Some((recovery, source)) = committed else {
                continue;
            };
            assert_eq!(recovery.durable_frontier.0, 1, "{name}");
            assert_eq!(recovery.published_frontier.0, 0, "{name}");
            assert_eq!(
                recovery.failed_stage,
                DerivedPublicationStage::PublishEdgeBlocks,
                "{name}"
            );
            let RebuildError::Store(BlockStoreError::Io(io_error)) = source else {
                assert!(
                    matches!(source, RebuildError::Store(BlockStoreError::Io(_))),
                    "{name}: crash lost its typed Strata source: {source:?}"
                );
                continue;
            };
            assert!(
                io_error.to_string().contains(expected_source),
                "{name}: wrong Strata crash instant: {io_error}"
            );
            assert_eq!(
                db.state(),
                DatabaseState::NeedsAuthoritativeRecovery(recovery),
                "{name}"
            );
            assert_recovery_fence(
                DerivedPublicationStage::PublishEdgeBlocks,
                recovery,
                db.neighbours(VId(1), KNOWS),
            );
            let refused = db.write(cx, block_store_fault_batch()).await;
            assert!(
                matches!(refused, Err(WriteError::RecoveryRequired(_))),
                "{name}: fenced handle accepted a second write: {refused:?}"
            );
            let Err(WriteError::RecoveryRequired(found)) = refused else {
                continue;
            };
            assert_eq!(found, recovery, "{name}");
            drop(db);

            let engine = Database::open(cx, &dir, engine_keys())
                .await
                .expect("authoritative reopen repairs Strata publication");
            assert_eq!(engine.frontier().expect("healthy frontier").0, 1, "{name}");
            let engine_neighbours = engine.neighbours(VId(1), KNOWS).expect("healthy read");
            let engine_vertices = engine.vertices().expect("healthy read");
            let engine_edges = engine.edges().expect("healthy read");
            drop(engine);

            let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
                .await
                .expect("oracle opens the durable stream");
            let replayed = replay(cx, &coordinator).await.expect("stream replays");
            let graph = replayed
                .database
                .graph(GRAPH, BRANCH)
                .expect("oracle materialized the coordinate");
            assert_eq!(engine_neighbours, graph.neighbours(VId(1), KNOWS), "{name}");
            assert_eq!(engine_vertices.len(), graph.vertex_count(), "{name}");
            assert_eq!(engine_edges.len(), graph.edge_count(), "{name}");
            assert_eq!(engine_neighbours, vec![VId(2)], "{name}");
        }
    });
}

/// **ONE I/O PLANE (fgdb-tvg8.1): a filesystem that lies to Chronicle now
/// lies to Strata too, and a crash rolls the whole commit back coherently.**
///
/// Before the `BlockStore` rode the injected `Vfs`, its object files went
/// through `std::fs`: a lab crash discarded Chronicle's unflushed commit
/// while the derived blocks survived on the real filesystem — derived state
/// from a commit the log never durably made, invisible to every crash matrix.
/// This law pins the repair. A session whose every fsync lies completes a
/// commit end to end believing it durable; the lie events must land on
/// Strata's own `.block` content sync as well as Chronicle's files (the
/// witness that the same injected plan bites both), and after the crash a
/// cold reopen must agree with the oracle's replay of the durable stream at
/// the pre-lie frontier — no orphaned derived state, no slot running ahead.
#[test]
fn a_lied_commit_rolls_back_strata_and_chronicle_together() {
    let dir = scratch("one-plane-lie-crash");
    under_lab(1_871, move |cx| async move {
        let cx = &cx;
        // SESSION 1 — honest filesystem: genesis plus one durable commit.
        create_genesis(cx, &dir).await;
        {
            let mut db = Database::open(cx, &dir, engine_keys())
                .await
                .expect("database opens honestly");
            db.write(cx, block_store_fault_batch())
                .await
                .expect("durable baseline commit");
        }

        // SESSION 2 — every sync lies. The engine believes this second commit
        // is durable end to end; not one byte of it is.
        let vfs = FaultVfs::unix(FaultPlan {
            fsync_lie: Trigger::Always,
            ..FaultPlan::faultless()
        });
        {
            let mut db = Database::open_with_vfs(cx, vfs.clone(), &dir, engine_keys())
                .await
                .expect("reopens through the lying filesystem");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(3), vec![], vec![]);
            batch.add_edge(EId(2), VId(1), VId(3), vec![]);
            db.write(cx, batch)
                .await
                .expect("a lied-to writer cannot observe its own loss");
        }

        // THE WITNESS THIS SEAM EXISTS FOR: the lie landed on a Strata block
        // file's own content sync — a boundary no injected filesystem could
        // reach while the store wrote through `std::fs` — and on Chronicle's
        // side of the same plan.
        let lies: Vec<_> = vfs
            .events()
            .into_iter()
            .filter(|event| matches!(event.kind, FaultKind::FsyncLie { .. }))
            .collect();
        assert!(
            lies.iter()
                .any(|event| event.path.extension().is_some_and(|ext| ext == "block")),
            "no fsync lie reached a Strata block content sync: {lies:?}"
        );
        assert!(
            lies.iter().any(|event| {
                !event
                    .path
                    .components()
                    .any(|part| part.as_os_str() == fgdb_strata::store::BLOCK_DIR)
            }),
            "the same plan must also be biting Chronicle's files: {lies:?}"
        );

        // Crash. Capsule, marker, blocks, root, manifest, and slot bytes were
        // all unflushed on ONE plane, so they roll back together.
        vfs.crash().await.expect("crash rolls back the lied commit");

        // Cold reopen: the durable stream ends at the baseline commit, the
        // slot still names the baseline manifest, and no derived object from
        // the lied commit is reachable. The engine and the oracle agree.
        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("reopen after the coherent rollback");
        assert_eq!(
            engine.frontier().expect("healthy frontier").0,
            1,
            "the lied commit must be gone in both planes"
        );
        assert!(
            engine.vertex(VId(3)).expect("healthy read").is_none(),
            "the lied vertex leaked through the rollback"
        );
        let engine_neighbours = engine.neighbours(VId(1), KNOWS).expect("healthy read");
        let engine_vertices = engine.vertices().expect("healthy read");
        let engine_edges = engine.edges().expect("healthy read");
        drop(engine);

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens the durable stream");
        let replayed = replay(cx, &coordinator).await.expect("stream replays");
        let graph = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("oracle materialized the coordinate");
        assert_eq!(engine_neighbours, graph.neighbours(VId(1), KNOWS));
        assert_eq!(engine_vertices.len(), graph.vertex_count());
        assert_eq!(engine_edges.len(), graph.edge_count());
        assert_eq!(engine_neighbours, vec![VId(2)]);
    });
}

/// **THE TIME-TRAVEL DIFFERENTIAL (fgdb-90jx): at EVERY epoch frontier, the
/// engine's as-of answers equal the oracle replayed through that prefix.**
///
/// The frontier differential above cannot see a fold that reaches the right
/// final state through wrong intermediate ones — a delete applied one commit
/// early, an update folded into its predecessor's span. Here the oracle is
/// rebuilt six times, once per prefix, so every intermediate graph the stream
/// ever meant is compared, not just the last.
#[test]
fn the_spine_agrees_with_the_oracle_at_every_epoch() {
    let dir = scratch("epoch-agreement");
    under_lab(107, move |cx| async move {
        let cx = &cx;
        let epochs = write_history(cx, &dir).await;
        assert_eq!(epochs.len(), 7, "the fixture is seven epochs");

        // ENGINE SIDE: every epoch's answers gathered from one fresh open,
        // before the single-writer lease drops.
        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("reopens");
        let probes: Vec<(VId, RelationId)> = (1..=6u128)
            .flat_map(|vid| [(VId(vid), KNOWS), (VId(vid), WORKS_WITH)])
            .collect();
        type EpochAnswers = (
            Vec<Vec<VId>>,
            Vec<Option<fgdb::VertexRow>>,
            Vec<Option<fgdb::EdgeRecord>>,
            Vec<fgdb::VertexRow>,
            Vec<fgdb::EdgeRecord>,
        );
        let engine_epochs: Vec<EpochAnswers> = epochs
            .iter()
            .map(|as_of| {
                (
                    probes
                        .iter()
                        .map(|(vid, rel)| {
                            engine
                                .neighbours_at(*vid, *rel, *as_of)
                                .expect("engine reads")
                        })
                        .collect(),
                    (1..=6u128)
                        .map(|vid| engine.vertex_at(VId(vid), *as_of).expect("engine reads"))
                        .collect(),
                    (10..=17u128)
                        .map(|eid| engine.edge_at(EId(eid), *as_of).expect("engine reads"))
                        .collect(),
                    engine.vertices_at(*as_of).expect("engine scans"),
                    engine.edges_at(*as_of).expect("engine scans"),
                )
            })
            .collect();
        drop(engine);

        // ORACLE SIDE: one prefix replay per epoch, over nothing but the bytes.
        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("oracle opens");
        for (as_of, (hoods, vertices, edges, vertex_scan, edge_scan)) in
            epochs.iter().zip(&engine_epochs)
        {
            let replayed = fgdb_sim::replay_through(cx, &coordinator, *as_of)
                .await
                .expect("the prefix replays");
            let graph = replayed
                .database
                .graph(GRAPH, BRANCH)
                .expect("the oracle materialized the coordinate");

            for ((vid, rel), engine_answer) in probes.iter().zip(hoods) {
                assert_eq!(
                    engine_answer,
                    &graph.neighbours(*vid, *rel),
                    "epoch {as_of:?}: {vid:?} over {rel:?}"
                );
            }
            for (vid, engine_row) in (1..=6u128).map(VId).zip(vertices) {
                let oracle_vertex = graph.vertex(vid);
                assert_eq!(
                    engine_row.is_some(),
                    oracle_vertex.is_some(),
                    "epoch {as_of:?}: {vid:?} existence"
                );
                let (Some(row), Some(vertex)) = (engine_row, oracle_vertex) else {
                    continue;
                };
                assert_eq!(
                    row.labels,
                    vertex.labels.iter().copied().collect::<Vec<_>>(),
                    "epoch {as_of:?}: {vid:?} labels"
                );
                assert_eq!(
                    row.props,
                    vertex
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>(),
                    "epoch {as_of:?}: {vid:?} properties"
                );
            }
            for (eid, engine_edge) in (10..=17u128).map(EId).zip(edges) {
                let oracle_edge = graph.edge(eid);
                assert_eq!(
                    engine_edge.is_some(),
                    oracle_edge.is_some(),
                    "epoch {as_of:?}: {eid:?} existence"
                );
                let (Some(record), Some(edge)) = (engine_edge, oracle_edge) else {
                    continue;
                };
                assert_eq!(
                    (record.entry.src, record.entry.relation, record.entry.dst),
                    (edge.src, edge.relation, edge.dst),
                    "epoch {as_of:?}: {eid:?} topology"
                );
                assert_eq!(
                    record.props,
                    edge.props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>(),
                    "epoch {as_of:?}: {eid:?} properties"
                );
            }

            // THE ENUMERATION DIFFERENTIAL (fgdb-9k5w): the scan agrees with
            // the oracle element-for-element, and the COUNTS close the
            // universe — the engine cannot answer a vertex the oracle lacks,
            // and equal cardinality forbids missing one the oracle has.
            assert_eq!(
                vertex_scan.len(),
                graph.vertex_count(),
                "epoch {as_of:?}: vertex scan cardinality"
            );
            for row in vertex_scan {
                let vertex = graph.vertex(row.vid);
                assert!(
                    vertex.is_some(),
                    "epoch {as_of:?}: scanned {:?} unknown to the oracle",
                    row.vid
                );
                let vertex = vertex.expect("existence just asserted");
                assert_eq!(
                    row.labels,
                    vertex.labels.iter().copied().collect::<Vec<_>>()
                );
                assert_eq!(
                    row.props,
                    vertex
                        .props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(
                edge_scan.len(),
                graph.edge_count(),
                "epoch {as_of:?}: edge scan cardinality"
            );
            for record in edge_scan {
                let edge = graph.edge(record.entry.eid);
                assert!(
                    edge.is_some(),
                    "epoch {as_of:?}: scanned {:?} unknown to the oracle",
                    record.entry.eid
                );
                let edge = edge.expect("existence just asserted");
                assert_eq!(
                    (record.entry.src, record.entry.relation, record.entry.dst),
                    (edge.src, edge.relation, edge.dst)
                );
                assert_eq!(
                    record.props,
                    edge.props
                        .iter()
                        .map(|(key, value)| (*key, value.clone()))
                        .collect::<Vec<_>>()
                );
            }
        }

        // ANTI-VACUITY: six agreements about one unchanging graph would prove
        // nothing about time. Every consecutive epoch pair must differ in at
        // least one gathered answer — each commit changed the observable graph.
        assert!(
            engine_epochs.windows(2).all(|pair| pair[0] != pair[1]),
            "every epoch must observe a different graph from its predecessor"
        );
    });
}

// ---------------------------------------------------------------------------
// Model-based generated histories over the ENGINE (§15 storage oracle)
// ---------------------------------------------------------------------------

/// Deterministic generator state — SplitMix64, so a seed IS the history and a
/// failure report is a repro command, never a coincidence.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// The generator's model: enough state to propose ONLY lawful operations.
/// Validity is maintained, not filtered — a refused batch is a generator
/// defect, and the driver treats it as one.
#[derive(Default)]
struct GenModel {
    next_vid: u128,
    next_eid: u128,
    live_vertices: Vec<u128>,
    /// eid -> (src, dst); endpoints so a vertex delete cascades in the model
    /// exactly as the engine's writer cascades it.
    live_edges: Vec<(u128, u128, u128)>,
    /// Retired identities, so if-present-of-spent is lawful and creates
    /// never reuse a spent vid (`IdentitySpent`).
    spent_vertices: Vec<u128>,
    spent_edges: Vec<u128>,
    /// Current value of PropertyKeyId(7) / (11), so a later CAS match
    /// names the value the engine will actually see.
    vertex_prop: std::collections::BTreeMap<u128, Option<CanonicalScalar>>,
    edge_prop: std::collections::BTreeMap<u128, Option<CanonicalScalar>>,
}

/// **THE GENERATED DIFFERENTIAL: N seeded random histories of creates,
/// deletes, cascades, label flips, BOTH property-update families, and the
/// live ensure / vertex-and-edge CAS / delete-if-present subsets, each
/// compared against the oracle at EVERY epoch with counts closing the
/// universe.** The hand-built fixtures prove the shapes they thought of;
/// this proves the interactions nobody thought of, and a seed reproduces
/// any disagreement exactly.
#[test]
fn generated_histories_agree_with_the_oracle_at_every_epoch() {
    for seed in [11u64, 47, 203] {
        let dir = scratch(&format!("generated-{seed}"));
        under_lab(300 + seed, move |cx| async move {
            let cx = &cx;
            let mut rng = SplitMix64(seed);
            let mut model = GenModel {
                next_vid: 1,
                next_eid: 1000,
                ..GenModel::default()
            };

            // ENGINE SIDE: generate and commit 8 batches of 1..=5 lawful ops,
            // dropping and REOPENING the database halfway — the retained
            // writer's incremental fold and the from-scratch rebuild must be
            // indistinguishable under every random shape, not only under the
            // fixtures that were written knowing the answer.
            let mut db = Database::create(cx, &dir, engine_keys())
                .await
                .expect("creates");
            let mut epochs = Vec::new();
            let mut new_family_hits = 0u32;
            for round in 0..8 {
                if round == 4 {
                    let incremental_index = db.delta_index().expect("reads").clone();
                    drop(db);
                    db = Database::open(cx, &dir, engine_keys())
                        .await
                        .expect("a mid-history reopen rebuilds and continues");
                    assert_eq!(
                        db.delta_index().expect("reads"),
                        &incremental_index,
                        "seed {seed}: mid-history incremental delta window \
                         diverged from rebuild"
                    );
                    // The v3 head witness (GoldBarn, thread fgdb-l96k): the
                    // checkpoint-derived element-version heads must equal the
                    // full fold's on every generated shape — graph answers
                    // cannot see a head that chained through the wrong
                    // statements, so the maps are compared directly.
                    let derived = db.element_versions().expect("reads").clone();
                    drop(db);
                    let control = Database::open_rebuilding(cx, &dir, engine_keys())
                        .await
                        .expect("the rebuild control reopens");
                    assert_eq!(
                        control.element_versions().expect("reads"),
                        &derived,
                        "seed {seed}: checkpoint-derived v3 heads diverged from the fold's"
                    );
                    drop(control);
                    db = Database::open(cx, &dir, engine_keys())
                        .await
                        .expect("the checkpoint-selected session resumes");
                }
                if round == 6 {
                    // Consolidate mid-history: every epoch comparison below
                    // must still hold over the compacted generation — the
                    // answer-preservation law under shapes nobody hand-wrote.
                    db.compact(cx).await.expect("consolidates");
                }
                let mut batch = WriteBatch::new(KNOWS);
                let ops = 1 + rng.below(5);
                // Per-batch order-sensitivity bookkeeping (fgdb-kokz): the
                // model refuses to PROPOSE what the engine refuses to commit,
                // so a refusal stays a generator defect rather than noise.
                let mut touched: std::collections::BTreeSet<(u8, u128)> =
                    std::collections::BTreeSet::new();
                for _ in 0..ops {
                    match rng.below(13) {
                        0 | 1 => {
                            let vid = model.next_vid;
                            model.next_vid += 1;
                            let labels = if rng.below(2) == 0 {
                                vec![LabelId(3)]
                            } else {
                                vec![]
                            };
                            let props = if rng.below(2) == 0 {
                                vec![(PropertyKeyId(7), CanonicalScalar::Int(vid as i64))]
                            } else {
                                vec![]
                            };
                            model
                                .vertex_prop
                                .insert(vid, props.first().map(|(_, value)| value.clone()));
                            batch.create_vertex(VId(vid), labels, props);
                            model.live_vertices.push(vid);
                        }
                        2 if model.live_vertices.len() >= 2 => {
                            let eid = model.next_eid;
                            model.next_eid += 1;
                            let src = model.live_vertices[rng.below(model.live_vertices.len())];
                            let dst = model.live_vertices[rng.below(model.live_vertices.len())];
                            let props = if rng.below(2) == 0 {
                                vec![(PropertyKeyId(11), CanonicalScalar::Int(eid as i64))]
                            } else {
                                vec![]
                            };
                            model
                                .edge_prop
                                .insert(eid, props.first().map(|(_, value)| value.clone()));
                            batch.add_edge(EId(eid), VId(src), VId(dst), props);
                            model.live_edges.push((eid, src, dst));
                        }
                        3 if !model.live_edges.is_empty() => {
                            let at = rng.below(model.live_edges.len());
                            if touched.contains(&(1, model.live_edges[at].0)) {
                                continue; // updated this batch — deletion is order-sensitive
                            }
                            let (eid, _, _) = model.live_edges.remove(at);
                            model.spent_edges.push(eid);
                            model.edge_prop.remove(&eid);
                            batch.delete_edge(EId(eid));
                        }
                        4 if !model.live_vertices.is_empty() => {
                            let at = rng.below(model.live_vertices.len());
                            let vid = model.live_vertices[at];
                            let cascade_touched = touched.contains(&(0, vid))
                                || model.live_edges.iter().any(|(eid, s, d)| {
                                    (*s == vid || *d == vid) && touched.contains(&(1, *eid))
                                });
                            if cascade_touched {
                                continue; // this batch updated it or a cascade member
                            }
                            model.live_vertices.remove(at);
                            model.spent_vertices.push(vid);
                            model.vertex_prop.remove(&vid);
                            batch.delete_vertex(VId(vid));
                            let cascaded: Vec<u128> = model
                                .live_edges
                                .iter()
                                .filter(|(_, s, d)| *s == vid || *d == vid)
                                .map(|(eid, _, _)| *eid)
                                .collect();
                            for eid in &cascaded {
                                model.spent_edges.push(*eid);
                                model.edge_prop.remove(eid);
                            }
                            model.live_edges.retain(|(_, s, d)| *s != vid && *d != vid);
                        }
                        5 if !model.live_vertices.is_empty() => {
                            let vid = model.live_vertices[rng.below(model.live_vertices.len())];
                            if !touched.insert((0, vid)) {
                                continue; // one exact field per element per batch
                            }
                            let value = (rng.below(2) == 0)
                                .then(|| CanonicalScalar::Int(rng.next() as i64 % 1000));
                            model.vertex_prop.insert(vid, value.clone());
                            batch.set_vertex_property(VId(vid), PropertyKeyId(7), value);
                        }
                        6 if !model.live_edges.is_empty() => {
                            let (eid, _, _) = model.live_edges[rng.below(model.live_edges.len())];
                            if !touched.insert((1, eid)) {
                                continue; // one exact field per element per batch
                            }
                            let value = (rng.below(2) == 0)
                                .then(|| CanonicalScalar::Int(rng.next() as i64 % 1000));
                            model.edge_prop.insert(eid, value.clone());
                            batch.set_edge_property(EId(eid), PropertyKeyId(11), value);
                        }
                        7 if !model.live_vertices.is_empty() => {
                            let vid = model.live_vertices[rng.below(model.live_vertices.len())];
                            if !touched.insert((0, vid)) {
                                continue; // coarser than the engine's per-field law: safe
                            }
                            let member = rng.below(2) == 0;
                            batch.set_vertex_label(VId(vid), LabelId(3), member);
                        }
                        8 => {
                            if !model.live_vertices.is_empty() && rng.below(2) == 0 {
                                let vid = model.live_vertices[rng.below(model.live_vertices.len())];
                                batch.ensure_vertex(VId(vid), vec![], vec![]);
                            } else {
                                let vid = model.next_vid;
                                model.next_vid += 1;
                                model.vertex_prop.insert(vid, None);
                                batch.ensure_vertex(VId(vid), vec![], vec![]);
                                model.live_vertices.push(vid);
                            }
                            new_family_hits += 1;
                        }
                        9 if model.live_vertices.len() >= 2 => {
                            if !model.live_edges.is_empty() && rng.below(2) == 0 {
                                let eid = model.next_eid;
                                model.next_eid += 1;
                                let (_, src, dst) =
                                    model.live_edges[rng.below(model.live_edges.len())];
                                batch.ensure_edge_by_triple(EId(eid), VId(src), VId(dst), vec![]);
                            } else {
                                let eid = model.next_eid;
                                model.next_eid += 1;
                                let src = model.live_vertices[rng.below(model.live_vertices.len())];
                                let dst = model.live_vertices[rng.below(model.live_vertices.len())];
                                let exists = model
                                    .live_edges
                                    .iter()
                                    .any(|(_, s, d)| *s == src && *d == dst);
                                batch.ensure_edge_by_triple(EId(eid), VId(src), VId(dst), vec![]);
                                if !exists {
                                    model.edge_prop.insert(eid, None);
                                    model.live_edges.push((eid, src, dst));
                                }
                            }
                            new_family_hits += 1;
                        }
                        10 => {
                            if !model.live_vertices.is_empty() && rng.below(2) == 0 {
                                let at = rng.below(model.live_vertices.len());
                                let vid = model.live_vertices[at];
                                let cascade_touched = touched.contains(&(0, vid))
                                    || model.live_edges.iter().any(|(eid, s, d)| {
                                        (*s == vid || *d == vid) && touched.contains(&(1, *eid))
                                    });
                                if cascade_touched {
                                    continue;
                                }
                                model.live_vertices.remove(at);
                                model.spent_vertices.push(vid);
                                model.vertex_prop.remove(&vid);
                                batch.delete_vertex_if_present(VId(vid));
                                let cascaded: Vec<u128> = model
                                    .live_edges
                                    .iter()
                                    .filter(|(_, s, d)| *s == vid || *d == vid)
                                    .map(|(eid, _, _)| *eid)
                                    .collect();
                                for eid in &cascaded {
                                    model.spent_edges.push(*eid);
                                    model.edge_prop.remove(eid);
                                }
                                model.live_edges.retain(|(_, s, d)| *s != vid && *d != vid);
                                new_family_hits += 1;
                            } else if !model.spent_vertices.is_empty() {
                                let vid =
                                    model.spent_vertices[rng.below(model.spent_vertices.len())];
                                batch.delete_vertex_if_present(VId(vid));
                                new_family_hits += 1;
                            } else if !model.spent_edges.is_empty() {
                                let eid = model.spent_edges[rng.below(model.spent_edges.len())];
                                batch.delete_edge_if_present(EId(eid));
                                new_family_hits += 1;
                            } else if !model.live_edges.is_empty() {
                                let at = rng.below(model.live_edges.len());
                                if touched.contains(&(1, model.live_edges[at].0)) {
                                    continue;
                                }
                                let (eid, _, _) = model.live_edges.remove(at);
                                model.spent_edges.push(eid);
                                model.edge_prop.remove(&eid);
                                batch.delete_edge_if_present(EId(eid));
                                new_family_hits += 1;
                            } else {
                                continue;
                            }
                        }
                        11 if !model.live_vertices.is_empty() => {
                            let vid = model.live_vertices[rng.below(model.live_vertices.len())];
                            if !touched.insert((0, vid)) {
                                continue;
                            }
                            let expected = model.vertex_prop.get(&vid).cloned().flatten();
                            if rng.below(2) == 0 {
                                let value = CanonicalScalar::Int(rng.next() as i64 % 1000);
                                model.vertex_prop.insert(vid, Some(value.clone()));
                                batch.compare_and_set_vertex_property(
                                    VId(vid),
                                    PropertyKeyId(7),
                                    expected,
                                    value,
                                    WriteMismatchPolicy::AbortWrite,
                                );
                            } else {
                                let wrong = Some(CanonicalScalar::Int(i64::MIN));
                                batch.compare_and_set_vertex_property(
                                    VId(vid),
                                    PropertyKeyId(7),
                                    wrong,
                                    CanonicalScalar::Int(0),
                                    WriteMismatchPolicy::NoOp,
                                );
                            }
                            new_family_hits += 1;
                        }
                        12 if !model.live_edges.is_empty() => {
                            let (eid, _, _) = model.live_edges[rng.below(model.live_edges.len())];
                            if !touched.insert((1, eid)) {
                                continue;
                            }
                            let expected = model.edge_prop.get(&eid).cloned().flatten();
                            if rng.below(2) == 0 {
                                let value = CanonicalScalar::Int(rng.next() as i64 % 1000);
                                model.edge_prop.insert(eid, Some(value.clone()));
                                batch.compare_and_set_edge_property(
                                    EId(eid),
                                    PropertyKeyId(11),
                                    expected,
                                    value,
                                    WriteMismatchPolicy::AbortWrite,
                                );
                            } else {
                                let wrong = Some(CanonicalScalar::Int(i64::MIN));
                                batch.compare_and_set_edge_property(
                                    EId(eid),
                                    PropertyKeyId(11),
                                    wrong,
                                    CanonicalScalar::Int(0),
                                    WriteMismatchPolicy::NoOp,
                                );
                            }
                            new_family_hits += 1;
                        }
                        _ => {
                            // The preferred family had no lawful target yet;
                            // create a vertex instead so the batch stays
                            // non-empty and the mix self-heals from empty
                            // models.
                            let vid = model.next_vid;
                            model.next_vid += 1;
                            model.vertex_prop.insert(vid, None);
                            batch.create_vertex(VId(vid), vec![], vec![]);
                            model.live_vertices.push(vid);
                        }
                    }
                }
                if batch.is_empty() {
                    // Continues can empty a batch; EmptyBatch is a generator
                    // defect, not a random-history outcome.
                    let vid = model.next_vid;
                    model.next_vid += 1;
                    model.vertex_prop.insert(vid, None);
                    batch.create_vertex(VId(vid), vec![], vec![]);
                    model.live_vertices.push(vid);
                }
                let frontier = db
                    .write(cx, batch)
                    .await
                    .expect("every generated batch is lawful — a refusal is a generator defect");
                epochs.push(frontier);
            }
            assert!(
                new_family_hits >= 1,
                "seed {seed}: generated mix emitted no ensure/CAS/if-present ops (hits={new_family_hits})"
            );

            // Gather every epoch's engine scans AND every vertex's
            // neighbours before the lease drops — the neighbour merge is its
            // own read path (a contiguous in-place scan), so agreement on
            // edge scans alone would leave it unwitnessed.
            let probe_vids = model.next_vid;
            type GenEpoch = (
                Vec<fgdb::VertexRow>,
                Vec<fgdb::EdgeRecord>,
                Vec<(Vec<VId>, Vec<VId>)>,
            );
            let engine_epochs: Vec<GenEpoch> = epochs
                .iter()
                .map(|as_of| {
                    (
                        db.vertices_at(*as_of).expect("engine scans"),
                        db.edges_at(*as_of).expect("engine scans"),
                        (1..probe_vids)
                            .map(|vid| {
                                (
                                    db.neighbours_at(VId(vid), KNOWS, *as_of)
                                        .expect("engine reads"),
                                    db.in_neighbours_at(VId(vid), KNOWS, *as_of)
                                        .expect("engine reads"),
                                )
                            })
                            .collect(),
                    )
                })
                .collect();
            // The POST-COMPACTION head witness: the loop compacted at round 6,
            // so the checkpoint-selected open below lands on the compacted
            // generation — the one place statement collapse could hand the
            // derivation a shorter chain than the fold's. The round-4 witness
            // above never sees a compacted partition.
            let retained = db.element_versions().expect("reads").clone();
            let retained_index = db.delta_index().expect("reads").clone();
            drop(db);
            let reopened = Database::open(cx, &dir, engine_keys())
                .await
                .expect("reopens on the compacted generation");
            assert_eq!(
                reopened.element_versions().expect("reads"),
                &retained,
                "seed {seed}: post-compaction checkpoint-derived v3 heads \
                 diverged from the retained session's"
            );
            assert_eq!(
                reopened.delta_index().expect("reads"),
                &retained_index,
                "seed {seed}: post-compaction incremental delta window \
                 diverged from checkpoint rebuild"
            );
            drop(reopened);
            let control = Database::open_rebuilding(cx, &dir, engine_keys())
                .await
                .expect("the rebuild control reopens");
            assert_eq!(
                control.element_versions().expect("reads"),
                &retained,
                "seed {seed}: the full fold's v3 heads diverged from the \
                 retained session's"
            );
            assert_eq!(
                control.delta_index().expect("reads"),
                &retained_index,
                "seed {seed}: post-compaction incremental delta window \
                 diverged from the full rebuild"
            );
            drop(control);

            // ORACLE SIDE: one prefix replay per epoch, over nothing but the
            // bytes; counts close the universe in both directions.
            let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
                .await
                .expect("oracle opens");
            for (as_of, (vertex_scan, edge_scan, hoods)) in epochs.iter().zip(&engine_epochs) {
                let replayed = fgdb_sim::replay_through(cx, &coordinator, *as_of)
                    .await
                    .expect("the prefix replays");
                let graph = replayed
                    .database
                    .graph(GRAPH, BRANCH)
                    .expect("the oracle materialized the coordinate");
                assert_eq!(
                    vertex_scan.len(),
                    graph.vertex_count(),
                    "seed {seed} epoch {as_of:?}: vertex cardinality"
                );
                for row in vertex_scan {
                    let vertex = graph.vertex(row.vid);
                    assert!(
                        vertex.is_some(),
                        "seed {seed} epoch {as_of:?}: scanned {:?} unknown to the oracle",
                        row.vid
                    );
                    let vertex = vertex.expect("existence just asserted");
                    assert_eq!(
                        row.labels,
                        vertex.labels.iter().copied().collect::<Vec<_>>(),
                        "seed {seed} epoch {as_of:?}: {:?} labels",
                        row.vid
                    );
                    assert_eq!(
                        row.props,
                        vertex
                            .props
                            .iter()
                            .map(|(key, value)| (*key, value.clone()))
                            .collect::<Vec<_>>(),
                        "seed {seed} epoch {as_of:?}: {:?} properties",
                        row.vid
                    );
                }
                assert_eq!(
                    edge_scan.len(),
                    graph.edge_count(),
                    "seed {seed} epoch {as_of:?}: edge cardinality"
                );
                for record in edge_scan {
                    let edge = graph.edge(record.entry.eid);
                    assert!(
                        edge.is_some(),
                        "seed {seed} epoch {as_of:?}: scanned {:?} unknown to the oracle",
                        record.entry.eid
                    );
                    let edge = edge.expect("existence just asserted");
                    assert_eq!(
                        (record.entry.src, record.entry.dst),
                        (edge.src, edge.dst),
                        "seed {seed} epoch {as_of:?}: {:?} topology",
                        record.entry.eid
                    );
                    assert_eq!(
                        record.props,
                        edge.props
                            .iter()
                            .map(|(key, value)| (*key, value.clone()))
                            .collect::<Vec<_>>(),
                        "seed {seed} epoch {as_of:?}: {:?} properties",
                        record.entry.eid
                    );
                }
                for (vid, (hood, arrivals)) in (1..probe_vids).map(VId).zip(hoods) {
                    assert_eq!(
                        hood,
                        &graph.neighbours(vid, KNOWS),
                        "seed {seed} epoch {as_of:?}: {vid:?} neighbours"
                    );
                    // The oracle has no reverse face; derive arrivals from the
                    // already-agreed edge scan — an independent construction,
                    // which is the point (fgdb-x164).
                    let mut expected: Vec<VId> = edge_scan
                        .iter()
                        .filter(|record| record.entry.dst == vid)
                        .map(|record| record.entry.src)
                        .collect();
                    expected.sort();
                    expected.dedup();
                    assert_eq!(
                        arrivals, &expected,
                        "seed {seed} epoch {as_of:?}: {vid:?} in-neighbours"
                    );
                }
            }
            let replayed = replay(cx, &coordinator).await.expect("full stream replays");
            assert_eq!(
                replayed.index, retained_index,
                "seed {seed}: independent replay delta window diverged from \
                 the incremental index"
            );
        });
    }
}

/// Independent application of the same fold scenarios through the engine
/// WriteBatch path and a standalone reference Transaction — not a replay of
/// the engine stream. Agreement here is what 819.2's differential asked
/// for: same intents, two implementations, same live graph.
#[test]
fn net_effect_fold_agrees_independently_with_reference_transactions() {
    let dir = scratch("nenf-independent");
    under_lab(119, move |cx| async move {
        let cx = &cx;
        let rank = PropertyKeyId(100);

        let mut engine = Database::create(cx, &dir, engine_keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(KNOWS);
        seed.create_vertex(VId(1), vec![], vec![(rank, CanonicalScalar::Int(5))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(8), vec![], vec![(rank, CanonicalScalar::Int(1))]);
        engine.write(cx, seed).await.expect("seeds");

        let mut two_sets = WriteBatch::new(KNOWS);
        two_sets.set_vertex_property(VId(1), rank, Some(CanonicalScalar::Int(3)));
        two_sets.set_vertex_property(VId(1), rank, Some(CanonicalScalar::Int(7)));
        engine.write(cx, two_sets).await.expect("two sets fold");

        let mut create_set_delete = WriteBatch::new(KNOWS);
        create_set_delete.create_vertex(VId(9), vec![], vec![(rank, CanonicalScalar::Int(1))]);
        create_set_delete.set_vertex_property(VId(9), rank, Some(CanonicalScalar::Int(4)));
        create_set_delete.delete_vertex(VId(9));
        engine
            .write(cx, create_set_delete)
            .await
            .expect("create+set+delete cancels");

        let mut set_delete = WriteBatch::new(KNOWS);
        set_delete.set_vertex_property(VId(8), rank, Some(CanonicalScalar::Int(3)));
        set_delete.delete_vertex(VId(8));
        engine
            .write(cx, set_delete)
            .await
            .expect("set+delete of an existing vertex");
        drop(engine);

        let engine = Database::open(cx, &dir, engine_keys())
            .await
            .expect("reopens");
        let engine_v1 = engine.vertex(VId(1)).expect("reads").expect("v1 live");
        assert_eq!(engine_v1.props, vec![(rank, CanonicalScalar::Int(7))]);
        assert!(engine.vertex(VId(8)).expect("reads").is_none());
        assert!(engine.vertex(VId(9)).expect("reads").is_none());
        assert!(engine.vertex(VId(2)).expect("reads").is_some());
        drop(engine);

        let mut oracle = fgdb_reference::ReferenceDatabase::new();
        let semantics = fgdb_types::ObjectId([0x11; 32]);
        let mut txn = fgdb_reference::txn::Transaction::begin_genesis(&oracle, GRAPH, BRANCH)
            .expect("genesis");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(1),
                    labels: vec![],
                    props: vec![(rank, CanonicalScalar::Int(5))],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(2),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(8),
                    labels: vec![],
                    props: vec![(rank, CanonicalScalar::Int(1))],
                },
            ]),
        ])
        .expect("oracle seeds");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(1),
            fgdb_types::LogicalCommandSeq(10),
        )
        .expect("oracle seed commits")
        .committed_parts()
        .expect("oracle seed wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                    name: rank,
                    value: CanonicalScalar::Int(3),
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                    name: rank,
                    value: CanonicalScalar::Int(7),
                },
            ]),
        ])
        .expect("oracle two sets");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(2),
            fgdb_types::LogicalCommandSeq(20),
        )
        .expect("oracle two sets commit")
        .committed_parts()
        .expect("oracle two sets wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(9),
                    labels: vec![],
                    props: vec![(rank, CanonicalScalar::Int(1))],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(9)),
                    name: rank,
                    value: CanonicalScalar::Int(4),
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteVertex { vid: VId(9) },
            ]),
        ])
        .expect("oracle create+set+delete");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(3),
            fgdb_types::LogicalCommandSeq(30),
        )
        .expect("oracle cancel commits")
        .committed_parts()
        .expect("oracle cancel wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::SetProp {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(8)),
                    name: rank,
                    value: CanonicalScalar::Int(3),
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteVertex { vid: VId(8) },
            ]),
        ])
        .expect("oracle set+delete");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(4),
            fgdb_types::LogicalCommandSeq(40),
        )
        .expect("oracle set+delete commits")
        .committed_parts()
        .expect("oracle set+delete wrote");

        let graph = oracle.graph(GRAPH, BRANCH).expect("oracle coordinate");
        assert_eq!(
            graph.vertex(VId(1)).expect("v1").props.get(&rank),
            Some(&CanonicalScalar::Int(7))
        );
        assert!(graph.vertex(VId(8)).is_none());
        assert!(graph.vertex(VId(9)).is_none());
        assert!(graph.vertex(VId(2)).is_some());
    });
}

/// Independent Ensure* agreement: the engine's new write-path subset must
/// match the reference reductions the oracle has had all along.
#[test]
fn ensure_intents_agree_independently_with_the_reference() {
    let dir = scratch("ensure-independent");
    under_lab(8203, move |cx| async move {
        let cx = &cx;
        let mut engine = Database::create(cx, &dir, engine_keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(KNOWS);
        seed.ensure_vertex(VId(1), vec![], vec![]);
        seed.ensure_vertex(VId(2), vec![], vec![]);
        engine
            .write(cx, seed)
            .await
            .expect("ensure-creates vertices");
        let mut again = WriteBatch::new(KNOWS);
        again.ensure_vertex(VId(1), vec![], vec![]);
        again.ensure_edge_by_triple(EId(10), VId(1), VId(2), vec![]);
        engine
            .write(cx, again)
            .await
            .expect("ensure vertex no-op + new triple");
        let mut triple_again = WriteBatch::new(KNOWS);
        triple_again.ensure_edge_by_triple(EId(11), VId(1), VId(2), vec![]);
        engine
            .write(cx, triple_again)
            .await
            .expect("ensure of the live triple is a no-op");
        let engine_neighbours = engine.neighbours(VId(1), KNOWS).expect("reads");
        assert_eq!(
            engine_neighbours,
            vec![VId(2)],
            "engine neighbours of vid=1 must be [2], got {engine_neighbours:?}"
        );
        drop(engine);

        let mut oracle = fgdb_reference::ReferenceDatabase::new();
        let semantics = fgdb_types::ObjectId([0x11; 32]);
        let mut txn = fgdb_reference::txn::Transaction::begin_genesis(&oracle, GRAPH, BRANCH)
            .expect("genesis");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::EnsureVertex {
                    vid: VId(1),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::EnsureVertex {
                    vid: VId(2),
                    labels: vec![],
                    props: vec![],
                },
            ]),
        ])
        .expect("oracle ensure-creates");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(1),
            fgdb_types::LogicalCommandSeq(10),
        )
        .expect("oracle seed commits")
        .committed_parts()
        .expect("oracle seed wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::EnsureVertex {
                    vid: VId(1),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::EnsureEdgeByTriple {
                    eid: EId(10),
                    src: VId(1),
                    etype: KNOWS,
                    dst: VId(2),
                    props: vec![],
                },
            ]),
        ])
        .expect("oracle ensure no-op + triple");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(2),
            fgdb_types::LogicalCommandSeq(20),
        )
        .expect("oracle second commit")
        .committed_parts()
        .expect("oracle second wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[fgdb_reference::intents::Statement::new(vec![
            fgdb_reference::intents::Intent::EnsureEdgeByTriple {
                eid: EId(11),
                src: VId(1),
                etype: KNOWS,
                dst: VId(2),
                props: vec![],
            },
        ])])
        .expect("oracle triple again");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(3),
            fgdb_types::LogicalCommandSeq(30),
        )
        .expect("oracle third commit")
        .committed_parts()
        .expect("oracle third wrote");

        let graph = oracle.graph(GRAPH, BRANCH).expect("oracle coordinate");
        assert_eq!(
            graph.neighbours(VId(1), KNOWS),
            engine_neighbours,
            "engine and oracle must agree about vid=1's KNOWS neighbours"
        );
        assert!(graph.vertex(VId(1)).is_some() && graph.vertex(VId(2)).is_some());
        assert!(graph.edge(EId(11)).is_none());
    });
}

/// Independent dangling-endpoint refusal: engine WriteBatch and reference
/// apply name the same missing endpoint (fgdb-r196).
#[test]
fn dangling_endpoint_agrees_independently_with_the_reference() {
    let dir = scratch("dangling-independent");
    under_lab(8212, move |cx| async move {
        let cx = &cx;
        let mut engine = Database::create(cx, &dir, engine_keys())
            .await
            .expect("creates");
        let origin = engine.frontier().expect("origin");
        let mut dangling = WriteBatch::new(KNOWS);
        dangling.add_edge(EId(10), VId(1), VId(2), vec![]);
        let engine_refusal = engine
            .write(cx, dangling)
            .await
            .expect_err("engine must refuse");
        assert!(
            matches!(
                engine_refusal,
                WriteError::DanglingEndpoint {
                    eid: EId(10),
                    endpoint: VId(1)
                }
            ),
            "engine must name src=1, got {engine_refusal:?}"
        );
        assert_eq!(
            engine.frontier().expect("unchanged"),
            origin,
            "engine dangling refuse must not consume a sequence"
        );
        drop(engine);

        let mut graph = fgdb_reference::ReferenceGraph::new();
        let oracle_refusal = graph.apply_row(&fgdb_delta_types::DeltaRow::CreateEdge {
            eid: EId(10),
            birth_ordinal: 1,
            src: VId(1),
            relation: KNOWS,
            dst: VId(2),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        });
        assert!(
            matches!(
                oracle_refusal,
                Err(fgdb_reference::ApplyError::DanglingEndpoint {
                    eid: EId(10),
                    endpoint: VId(1)
                })
            ),
            "oracle must name src=1, got {oracle_refusal:?}"
        );
    });
}

/// Independent CompareAndSet agreement: engine AbortWrite is the write-batch
/// face of reference TxnAbort; NoOp is the same word on both sides.
#[test]
fn compare_and_set_agrees_independently_with_the_reference() {
    let dir = scratch("cas-independent");
    under_lab(8205, move |cx| async move {
        let cx = &cx;
        let rank = PropertyKeyId(100);
        let mut engine = Database::create(cx, &dir, engine_keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(KNOWS);
        seed.create_vertex(VId(1), vec![], vec![(rank, CanonicalScalar::Int(5))]);
        engine.write(cx, seed).await.expect("seeds");
        let mut hit = WriteBatch::new(KNOWS);
        hit.compare_and_set_vertex_property(
            VId(1),
            rank,
            Some(CanonicalScalar::Int(5)),
            CanonicalScalar::Int(7),
            WriteMismatchPolicy::AbortWrite,
        );
        engine.write(cx, hit).await.expect("CAS match");
        let mut miss = WriteBatch::new(KNOWS);
        miss.compare_and_set_vertex_property(
            VId(1),
            rank,
            Some(CanonicalScalar::Int(5)),
            CanonicalScalar::Int(9),
            WriteMismatchPolicy::AbortWrite,
        );
        assert!(
            matches!(
                engine.write(cx, miss).await,
                Err(WriteError::CompareAndSetMismatch { .. })
            ),
            "engine AbortWrite miss must refuse"
        );
        let mut noop = WriteBatch::new(KNOWS);
        noop.compare_and_set_vertex_property(
            VId(1),
            rank,
            Some(CanonicalScalar::Int(5)),
            CanonicalScalar::Int(9),
            WriteMismatchPolicy::NoOp,
        );
        noop.ensure_vertex(VId(3), vec![], vec![]);
        engine.write(cx, noop).await.expect("NoOp miss + sibling");
        let engine_rank = engine
            .vertex(VId(1))
            .expect("reads")
            .expect("live")
            .props
            .clone();
        let engine_v3 = engine.vertex(VId(3)).expect("reads").is_some();
        drop(engine);

        let mut oracle = fgdb_reference::ReferenceDatabase::new();
        let semantics = fgdb_types::ObjectId([0x11; 32]);
        let mut txn = fgdb_reference::txn::Transaction::begin_genesis(&oracle, GRAPH, BRANCH)
            .expect("genesis");
        txn.execute(&[fgdb_reference::intents::Statement::new(vec![
            fgdb_reference::intents::Intent::CreateVertex {
                vid: VId(1),
                labels: vec![],
                props: vec![(rank, CanonicalScalar::Int(5))],
            },
        ])])
        .expect("oracle seed");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(1),
            fgdb_types::LogicalCommandSeq(10),
        )
        .expect("oracle seed commits")
        .committed_parts()
        .expect("oracle seed wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[fgdb_reference::intents::Statement::new(vec![
            fgdb_reference::intents::Intent::CompareAndSet {
                elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                name: rank,
                expected: Some(CanonicalScalar::Int(5)),
                value: CanonicalScalar::Int(7),
                mismatch: fgdb_reference::intents::MismatchPolicy::TxnAbort,
            },
        ])])
        .expect("oracle CAS match");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(2),
            fgdb_types::LogicalCommandSeq(20),
        )
        .expect("oracle match commits")
        .committed_parts()
        .expect("oracle match wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[fgdb_reference::intents::Statement::new(vec![
            fgdb_reference::intents::Intent::CompareAndSet {
                elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                name: rank,
                expected: Some(CanonicalScalar::Int(5)),
                value: CanonicalScalar::Int(9),
                mismatch: fgdb_reference::intents::MismatchPolicy::TxnAbort,
            },
        ])])
        .expect("oracle Abort execute");
        let aborted = txn
            .commit(
                &mut oracle,
                KNOWS,
                semantics,
                fgdb_types::CommitSeq(3),
                fgdb_types::LogicalCommandSeq(30),
            )
            .expect("oracle abort is a verdict, not an apply error");
        assert!(
            matches!(aborted, fgdb_reference::txn::TxnOutcome::Aborted { .. }),
            "oracle TxnAbort miss must abort, got {aborted:?}"
        );

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CompareAndSet {
                    elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                    name: rank,
                    expected: Some(CanonicalScalar::Int(5)),
                    value: CanonicalScalar::Int(9),
                    mismatch: fgdb_reference::intents::MismatchPolicy::NoOp,
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::EnsureVertex {
                    vid: VId(3),
                    labels: vec![],
                    props: vec![],
                },
            ]),
        ])
        .expect("oracle NoOp + ensure");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(3),
            fgdb_types::LogicalCommandSeq(30),
        )
        .expect("oracle NoOp commits")
        .committed_parts()
        .expect("oracle NoOp wrote");

        let graph = oracle.graph(GRAPH, BRANCH).expect("oracle coordinate");
        let oracle_rank = graph.vertex(VId(1)).expect("v1").props.get(&rank).cloned();
        assert_eq!(
            engine_rank,
            vec![(rank, CanonicalScalar::Int(7))],
            "engine rank must be 7, got {engine_rank:?}"
        );
        assert_eq!(
            oracle_rank,
            Some(CanonicalScalar::Int(7)),
            "oracle rank must be 7, got {oracle_rank:?}"
        );
        assert!(engine_v3, "engine vid=3 must exist after NoOp sibling");
        assert!(
            graph.vertex(VId(3)).is_some(),
            "oracle vid=3 must exist after NoOp sibling"
        );
    });
}

/// Independent edge CompareAndSet: the vertex path cannot stand in for
/// `compare_and_set_edge_property` (fgdb-2zql).
#[test]
fn compare_and_set_edge_agrees_independently_with_the_reference() {
    let dir = scratch("cas-edge-independent");
    under_lab(8209, move |cx| async move {
        let cx = &cx;
        let weight = PropertyKeyId(11);
        let mut engine = Database::create(cx, &dir, engine_keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(KNOWS);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(
            EId(10),
            VId(1),
            VId(2),
            vec![(weight, CanonicalScalar::Int(5))],
        );
        engine.write(cx, seed).await.expect("seeds");
        let mut hit = WriteBatch::new(KNOWS);
        hit.compare_and_set_edge_property(
            EId(10),
            weight,
            Some(CanonicalScalar::Int(5)),
            CanonicalScalar::Int(7),
            WriteMismatchPolicy::AbortWrite,
        );
        engine.write(cx, hit).await.expect("edge CAS match");
        let mut miss = WriteBatch::new(KNOWS);
        miss.compare_and_set_edge_property(
            EId(10),
            weight,
            Some(CanonicalScalar::Int(5)),
            CanonicalScalar::Int(9),
            WriteMismatchPolicy::AbortWrite,
        );
        assert!(
            matches!(
                engine.write(cx, miss).await,
                Err(WriteError::CompareAndSetMismatch { .. })
            ),
            "engine AbortWrite miss must refuse"
        );
        let mut noop = WriteBatch::new(KNOWS);
        noop.compare_and_set_edge_property(
            EId(10),
            weight,
            Some(CanonicalScalar::Int(5)),
            CanonicalScalar::Int(9),
            WriteMismatchPolicy::NoOp,
        );
        noop.ensure_vertex(VId(3), vec![], vec![]);
        engine.write(cx, noop).await.expect("NoOp miss + sibling");
        let engine_weight = engine
            .edge(EId(10))
            .expect("reads")
            .expect("live")
            .props
            .clone();
        let engine_v3 = engine.vertex(VId(3)).expect("reads").is_some();
        drop(engine);

        let mut oracle = fgdb_reference::ReferenceDatabase::new();
        let semantics = fgdb_types::ObjectId([0x11; 32]);
        let mut txn = fgdb_reference::txn::Transaction::begin_genesis(&oracle, GRAPH, BRANCH)
            .expect("genesis");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(1),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(2),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::AddEdge {
                    eid: EId(10),
                    src: VId(1),
                    etype: KNOWS,
                    dst: VId(2),
                    props: vec![(weight, CanonicalScalar::Int(5))],
                },
            ]),
        ])
        .expect("oracle seed");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(1),
            fgdb_types::LogicalCommandSeq(10),
        )
        .expect("oracle seed commits")
        .committed_parts()
        .expect("oracle seed wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[fgdb_reference::intents::Statement::new(vec![
            fgdb_reference::intents::Intent::CompareAndSet {
                elem: fgdb_delta_types::ElementId::Edge(EId(10)),
                name: weight,
                expected: Some(CanonicalScalar::Int(5)),
                value: CanonicalScalar::Int(7),
                mismatch: fgdb_reference::intents::MismatchPolicy::TxnAbort,
            },
        ])])
        .expect("oracle edge CAS match");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(2),
            fgdb_types::LogicalCommandSeq(20),
        )
        .expect("oracle match commits")
        .committed_parts()
        .expect("oracle match wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[fgdb_reference::intents::Statement::new(vec![
            fgdb_reference::intents::Intent::CompareAndSet {
                elem: fgdb_delta_types::ElementId::Edge(EId(10)),
                name: weight,
                expected: Some(CanonicalScalar::Int(5)),
                value: CanonicalScalar::Int(9),
                mismatch: fgdb_reference::intents::MismatchPolicy::TxnAbort,
            },
        ])])
        .expect("oracle Abort execute");
        let aborted = txn
            .commit(
                &mut oracle,
                KNOWS,
                semantics,
                fgdb_types::CommitSeq(3),
                fgdb_types::LogicalCommandSeq(30),
            )
            .expect("oracle abort is a verdict, not an apply error");
        assert!(
            matches!(aborted, fgdb_reference::txn::TxnOutcome::Aborted { .. }),
            "oracle TxnAbort miss must abort, got {aborted:?}"
        );

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CompareAndSet {
                    elem: fgdb_delta_types::ElementId::Edge(EId(10)),
                    name: weight,
                    expected: Some(CanonicalScalar::Int(5)),
                    value: CanonicalScalar::Int(9),
                    mismatch: fgdb_reference::intents::MismatchPolicy::NoOp,
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::EnsureVertex {
                    vid: VId(3),
                    labels: vec![],
                    props: vec![],
                },
            ]),
        ])
        .expect("oracle NoOp + ensure");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(3),
            fgdb_types::LogicalCommandSeq(30),
        )
        .expect("oracle NoOp commits")
        .committed_parts()
        .expect("oracle NoOp wrote");

        let graph = oracle.graph(GRAPH, BRANCH).expect("oracle coordinate");
        let oracle_weight = graph
            .edge(EId(10))
            .expect("e10")
            .props
            .get(&weight)
            .cloned();
        assert_eq!(
            engine_weight,
            vec![(weight, CanonicalScalar::Int(7))],
            "engine weight must be 7, got {engine_weight:?}"
        );
        assert_eq!(
            oracle_weight,
            Some(CanonicalScalar::Int(7)),
            "oracle weight must be 7, got {oracle_weight:?}"
        );
        assert!(engine_v3, "engine vid=3 must exist after NoOp sibling");
        assert!(
            graph.vertex(VId(3)).is_some(),
            "oracle vid=3 must exist after NoOp sibling"
        );
    });
}

/// Independent delete-if-present agreement: engine if-present of a missing
/// identity matches reference Delete* of a missing identity.
#[test]
fn delete_if_present_agrees_independently_with_the_reference() {
    let dir = scratch("delete-if-present-independent");
    under_lab(8207, move |cx| async move {
        let cx = &cx;
        let mut engine = Database::create(cx, &dir, engine_keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(KNOWS);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        engine.write(cx, seed).await.expect("seeds");
        let mut drop_live = WriteBatch::new(KNOWS);
        drop_live.delete_edge_if_present(EId(10));
        drop_live.delete_vertex_if_present(VId(1));
        engine.write(cx, drop_live).await.expect("if-present live");
        let mut missing = WriteBatch::new(KNOWS);
        missing.delete_edge_if_present(EId(10));
        missing.delete_vertex_if_present(VId(1));
        missing.ensure_vertex(VId(3), vec![], vec![]);
        engine
            .write(cx, missing)
            .await
            .expect("if-present missing + sibling");
        let engine_v1 = engine.vertex(VId(1)).expect("reads").is_some();
        let engine_v2 = engine.vertex(VId(2)).expect("reads").is_some();
        let engine_v3 = engine.vertex(VId(3)).expect("reads").is_some();
        let engine_e10 = engine.edge(EId(10)).expect("reads").is_some();
        drop(engine);

        let mut oracle = fgdb_reference::ReferenceDatabase::new();
        let semantics = fgdb_types::ObjectId([0x11; 32]);
        let mut txn = fgdb_reference::txn::Transaction::begin_genesis(&oracle, GRAPH, BRANCH)
            .expect("genesis");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(1),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::CreateVertex {
                    vid: VId(2),
                    labels: vec![],
                    props: vec![],
                },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::AddEdge {
                    eid: EId(10),
                    src: VId(1),
                    etype: KNOWS,
                    dst: VId(2),
                    props: vec![],
                },
            ]),
        ])
        .expect("oracle seed");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(1),
            fgdb_types::LogicalCommandSeq(10),
        )
        .expect("oracle seed commits")
        .committed_parts()
        .expect("oracle seed wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteEdge { eid: EId(10) },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteVertex { vid: VId(1) },
            ]),
        ])
        .expect("oracle live deletes");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(2),
            fgdb_types::LogicalCommandSeq(20),
        )
        .expect("oracle live commits")
        .committed_parts()
        .expect("oracle live wrote");

        let mut txn =
            fgdb_reference::txn::Transaction::begin(&oracle, GRAPH, BRANCH).expect("oracle begin");
        txn.execute(&[
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteEdge { eid: EId(10) },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::DeleteVertex { vid: VId(1) },
            ]),
            fgdb_reference::intents::Statement::new(vec![
                fgdb_reference::intents::Intent::EnsureVertex {
                    vid: VId(3),
                    labels: vec![],
                    props: vec![],
                },
            ]),
        ])
        .expect("oracle missing deletes + ensure");
        txn.commit(
            &mut oracle,
            KNOWS,
            semantics,
            fgdb_types::CommitSeq(3),
            fgdb_types::LogicalCommandSeq(30),
        )
        .expect("oracle missing commits")
        .committed_parts()
        .expect("oracle missing wrote");

        let graph = oracle.graph(GRAPH, BRANCH).expect("oracle coordinate");
        assert!(
            !engine_v1 && graph.vertex(VId(1)).is_none(),
            "vid=1 must be gone on both sides"
        );
        assert!(
            engine_v2 && graph.vertex(VId(2)).is_some(),
            "vid=2 must remain on both sides"
        );
        assert!(
            engine_v3 && graph.vertex(VId(3)).is_some(),
            "vid=3 sibling must exist on both sides"
        );
        assert!(
            !engine_e10 && graph.edge(EId(10)).is_none(),
            "eid=10 must be gone on both sides"
        );
    });
}
