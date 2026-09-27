// The lab body's Send check nests past the default depth (next trait solver).
#![recursion_limit = "256"]

//! Drop the PUBLIC completion future while real VFS operations are suspended.
//! The wrapper changes only scheduling, never bytes, durability or graph state.

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs, VfsFile};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, AsyncWriteExt, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, DatabaseState, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_types::{
    CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, EmbeddedTxnState,
    PurposeContexts, VId,
};
use std::future::Future;
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

type MemoryFile = <MemVfs as Vfs>::File;

#[derive(Default)]
struct Pause {
    mode: AtomicUsize,
    reached: AtomicBool,
}

impl Pause {
    fn arm(&self, mode: usize) {
        self.reached.store(false, Ordering::SeqCst);
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn blocks(&self, path: &Path, sync: bool) -> bool {
        let mode = self.mode.load(Ordering::SeqCst);
        let matches = match mode {
            1 if !sync => path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "capsules"),
            2 if sync => path.file_name().is_some_and(|name| name == "commits.log"),
            3 if !sync => path.file_name().is_some_and(|name| name == "manifest.root"),
            _ => false,
        };
        if matches {
            self.reached.store(true, Ordering::SeqCst);
        }
        matches
    }
}

#[derive(Clone)]
struct PausingVfs {
    inner: MemVfs,
    pause: Arc<Pause>,
}

struct PausingFile {
    inner: MemoryFile,
    path: PathBuf,
    pause: Arc<Pause>,
}

impl core::fmt::Debug for PausingVfs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PausingVfs")
    }
}

impl core::fmt::Debug for PausingFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PausingFile")
    }
}

impl AsyncRead for PausingFile {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PausingFile {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.pause.blocks(&this.path, false) {
            return Poll::Pending;
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl AsyncSeek for PausingFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.get_mut().inner).poll_seek(cx, pos)
    }
}

impl VfsFile for PausingFile {
    async fn metadata(&self) -> io::Result<Metadata> {
        self.inner.metadata().await
    }

    async fn sync_all(&self) -> io::Result<()> {
        std::future::poll_fn(|_| {
            if self.pause.blocks(&self.path, true) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        self.inner.sync_all().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        std::future::poll_fn(|_| {
            if self.pause.blocks(&self.path, true) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        self.inner.sync_data().await
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.set_len(size).await
    }
    async fn set_permissions(&self, perm: Permissions) -> io::Result<()> {
        self.inner.set_permissions(perm).await
    }
}

impl Vfs for PausingVfs {
    type File = PausingFile;

    async fn open(&self, path: &Path, opts: &OpenOptions) -> io::Result<Self::File> {
        Ok(PausingFile {
            inner: self.inner.open(path, opts).await?,
            path: path.to_path_buf(),
            pause: Arc::clone(&self.pause),
        })
    }
    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.metadata(path).await
    }
    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.symlink_metadata(path).await
    }
    async fn set_permissions(&self, path: &Path, perm: Permissions) -> io::Result<()> {
        self.inner.set_permissions(path, perm).await
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
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path).await
    }
    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.inner.read_dir(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir_all(path).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to).await
    }
    async fn copy(&self, src: &Path, dst: &Path) -> io::Result<u64> {
        self.inner.copy(src, dst).await
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
        let mut file = self
            .open(
                path,
                &OpenOptions::new().write(true).create(true).truncate(true),
            )
            .await?;
        file.write_all(contents).await?;
        file.flush().await
    }
}

#[test]
fn dropping_commit_or_finish_future_releases_pin_at_each_real_durability_phase() {
    let ((), report) = run_async_under_lab(0xf171_0201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for legacy_commit in [false, true] {
            for mode in 1..=3 {
                let memory = MemVfs::new().unwrap();
                let path = memory.database_dir();
                let pause = Arc::new(Pause::default());
                let vfs = PausingVfs {
                    inner: memory,
                    pause: Arc::clone(&pause),
                };
                let keys = DatabaseKeys::new(
                    [0x81; 32],
                    DatabaseSecurityNamespaceId([0x82; 32]),
                    [0x83; 32],
                );
                let mut database = Database::create_with_vfs(&cx, vfs, &path, keys)
                    .await
                    .unwrap();
                let basis = database.frontier().unwrap();
                let mut transaction = database.begin(&txcx).unwrap();
                let mut batch = WriteBatch::new(RelationId(1));
                batch.create_vertex(VId(1), vec![], vec![]);
                batch.create_vertex(VId(2), vec![], vec![]);
                batch.add_edge(EId(10), VId(1), VId(2), vec![]);
                transaction.write(&mut database, batch).unwrap();
                pause.arm(mode);
                let mut future = Box::pin(async {
                    if legacy_commit {
                        transaction
                            .commit(&mut database, &cx)
                            .await
                            .map(|commit_seq| EmbeddedTxnCompletion::WriteCommitted { commit_seq })
                    } else {
                        transaction.finish(&mut database, &cx).await
                    }
                });
                std::future::poll_fn(|task| match future.as_mut().poll(task) {
                    Poll::Pending if pause.reached.load(Ordering::SeqCst) => Poll::Ready(()),
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        panic!("durability pause {mode} was not reached: {result:?}")
                    }
                })
                .await;
                drop(future);
                assert_eq!(
                    txcx.outstanding_obligations(),
                    0,
                    "the transaction is still retained"
                );
                let expected = if mode == 3 {
                    EmbeddedTxnState::CommittedNeedsRecovery {
                        commit_seq: CommitSeq(basis.0 + 1),
                    }
                } else {
                    EmbeddedTxnState::CommitOutcomeUnknown {
                        published_frontier: basis,
                    }
                };
                assert_eq!(transaction.state(), expected);
                assert!(matches!(
                    transaction.finish(&mut database, &cx).await,
                    Err(WriteTxnError::Finished)
                ));
                assert!(matches!(
                    transaction.vertex(&database, VId(1)),
                    Err(WriteTxnError::Finished)
                ));
                if mode == 3 {
                    assert!(matches!(
                        database.state(),
                        DatabaseState::NeedsAuthoritativeRecovery(_)
                    ));
                } else {
                    assert!(matches!(
                        database.state(),
                        DatabaseState::CommitOutcomeUnknown { .. }
                    ));
                }
                // Explicit recovery alone resolves Chronicle. MemVfs retains
                // all written bytes, so a complete marker before sync survives.
                pause.mode.store(0, Ordering::SeqCst);
                let recovered = database.recover_authoritatively(&cx).await.unwrap();
                if mode == 1 {
                    assert_eq!(recovered.frontier().unwrap(), basis);
                    assert!(recovered.vertices().unwrap().is_empty());
                    assert!(recovered.edges().unwrap().is_empty());
                } else {
                    assert_eq!(recovered.frontier().unwrap(), CommitSeq(basis.0 + 1));
                    assert_eq!(
                        recovered
                            .vertices()
                            .unwrap()
                            .iter()
                            .map(|row| row.vid)
                            .collect::<Vec<_>>(),
                        vec![VId(1), VId(2)]
                    );
                    assert_eq!(
                        recovered.neighbours(VId(1), RelationId(1)).unwrap(),
                        vec![VId(2)]
                    );
                    assert_eq!(recovered.edges().unwrap().len(), 1);
                }
                assert_eq!(transaction.state(), expected);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}


#[test]
fn dropping_authorized_session_commands_closes_handles_without_guessing_commit_outcomes() {
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy};
    use fgdb_warden::{Authority, Error, Grant, QueryLimits, Rights, Scope};

    let ((), report) = run_async_under_lab(0xf171_0202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        for batch_mode in [false, true] {
            for mode in 1..=3 {
                let memory = MemVfs::new().unwrap();
                let path = memory.database_dir();
                let pause = Arc::new(Pause::default());
                let vfs = PausingVfs { inner: memory, pause: Arc::clone(&pause) };
                let namespace = DatabaseSecurityNamespaceId([0x85; 32]);
                let keys = DatabaseKeys::new([0x84; 32], namespace, [0x86; 32]);
                let mut database = Database::create_with_vfs(&commit, vfs, &path, keys).await.unwrap();
                let basis = database.frontier().unwrap();
                let authority = Authority::new(AuthKey::from_seed(0xf171_0202), namespace,
                    "host-graph", SchemaEpoch(1), 1).unwrap();
                let token = authority.issue_at(&Grant {
                    branch: "host-route".into(),
                    labels: Scope::All,
                    relations: Scope::All,
                    properties: Scope::All,
                    rights: Rights::ReadWrite,
                    limits: QueryLimits { max_nodes: 100_000, max_work: 1_000_000, max_rows: 100 },
                    expires_at_ms: 10_000,
                }, 100).unwrap();
                let policy = GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 100_000), 100, 100, 100,
                );
                let mut session = database.authorized_write_session(
                    &txn, &commit, &authority, &token, "host-route",
                    |kind, name| if kind == GraphSymbolKind::Relation && name == "R" {
                        Some(GraphSymbol::Relation(RelationId(1)))
                    } else { None },
                    RelationId(1), policy, 64, || 100,
                ).unwrap();
                let text = "CREATE (a), (b), (a)-[:R]->(b)";
                let params = GqlParameters::new();
                let prepared = session.prepare(&query, text, &params).unwrap();
                let arguments = [GqlParameters::new(), GqlParameters::new()];
                assert_eq!(txn.outstanding_obligations(), 0);
                pause.arm(mode);
                let mut future = Box::pin(async {
                    if batch_mode {
                        session.execute_batch(&query, &prepared, &arguments).await
                    } else {
                        session.query(&query, text, &params).await
                    }
                });
                std::future::poll_fn(|task| match future.as_mut().poll(task) {
                    Poll::Pending if pause.reached.load(Ordering::SeqCst) => Poll::Ready(()),
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => panic!("session durability pause {mode} did not fire: {result:?}"),
                }).await;
                drop(future);
                assert!(session.is_closed(), "dropping a polled command cannot reopen its request handle");
                assert_eq!(txn.outstanding_obligations(), 0);
                let error = session.query(&query, text, &params).await.unwrap_err();
                let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(&error);
                let mut stopped = false;
                while let Some(error) = cause {
                    stopped |= matches!(error.downcast_ref::<WriteTxnError>(),
                        Some(WriteTxnError::Authorization(Error::ExecutionStopped)));
                    cause = error.source();
                }
                assert!(stopped, "closed session must refuse rather than silently retrying");
                drop(session);
                if mode == 3 {
                    assert!(matches!(database.state(), DatabaseState::NeedsAuthoritativeRecovery(_)));
                } else {
                    assert!(matches!(database.state(), DatabaseState::CommitOutcomeUnknown { .. }));
                }
                pause.mode.store(0, Ordering::SeqCst);
                let recovered = database.recover_authoritatively(&commit).await.unwrap();
                if mode == 1 {
                    assert_eq!(recovered.frontier().unwrap(), basis);
                    assert!(recovered.vertices().unwrap().is_empty());
                    assert!(recovered.edges().unwrap().is_empty());
                } else {
                    assert_eq!(recovered.frontier().unwrap(), CommitSeq(basis.0 + 1));
                    let records = if batch_mode { 2 } else { 1 };
                    assert_eq!(recovered.vertices().unwrap().len(), 2 * records);
                    assert_eq!(recovered.edges().unwrap().len(), records);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
