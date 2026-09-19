//! Inode durability sequences with an independent, byte-level reference model.
//! The oracle has no sectors, backing paths, or FaultVfs implementation state.

use asupersync::fs::{OpenOptions, UnixVfs, Vfs, VfsFile};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::{AutoAdvanceTermination, LabConfig, LabRuntime};
use asupersync::runtime::RuntimeBuilder;
use asupersync::types::Budget;
use fgdb_sim::vfs::{FaultFile, FaultKind, FaultPlan, FaultVfs, Trigger};
use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, poll_fn};
use std::io::{self, SeekFrom};
use std::path::Path;
use std::pin::Pin;
use std::task::Poll;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug)]
enum OpenMode {
    Existing,
    Create,
    Truncate,
}

#[derive(Clone, Copy, Debug)]
enum Step {
    Open(usize, &'static str, OpenMode),
    Close(usize),
    Write(usize, &'static [u8]),
    Seek(usize, u64),
    Read(usize, usize),
    SyncAll(usize),
    SyncData(usize),
    Rename(&'static str, &'static str),
    SyncDirectory,
    Crash,
    RefuseStale(usize),
}

#[derive(Clone, Copy)]
enum Lies {
    Never,
    First,
    Always,
}

impl Lies {
    fn trigger(self) -> Trigger {
        match self {
            Self::Never => Trigger::Never,
            Self::First => Trigger::At(1),
            Self::Always => Trigger::Always,
        }
    }

    fn fires(self, eligible: usize) -> bool {
        match self {
            Self::Never => false,
            Self::First => eligible == 1,
            Self::Always => true,
        }
    }
}

struct Case {
    name: &'static str,
    initial: &'static [(&'static str, &'static [u8])],
    file_lies: Lies,
    directory_lies: Lies,
    lose_names: bool,
    steps: &'static [Step],
}

struct Inode {
    volatile: Vec<u8>,
    durable: Vec<u8>,
    dirty: bool,
}

struct Handle {
    inode: usize,
    cursor: usize,
    generation: usize,
}

struct Reference {
    inodes: Vec<Inode>,
    names: BTreeMap<&'static str, usize>,
    durable_names: BTreeMap<&'static str, usize>,
    handles: BTreeMap<usize, Handle>,
    generation: usize,
    pending_names: bool,
    eligible_file_syncs: usize,
    eligible_directory_syncs: usize,
    file_lies: usize,
    directory_lies: usize,
}

impl Reference {
    fn new(initial: &[(&'static str, &'static [u8])]) -> Self {
        let mut model = Self {
            inodes: Vec::new(),
            names: BTreeMap::new(),
            durable_names: BTreeMap::new(),
            handles: BTreeMap::new(),
            generation: 0,
            pending_names: false,
            eligible_file_syncs: 0,
            eligible_directory_syncs: 0,
            file_lies: 0,
            directory_lies: 0,
        };
        for &(name, bytes) in initial {
            let inode = model.inodes.len();
            model.inodes.push(Inode {
                volatile: bytes.to_vec(),
                durable: bytes.to_vec(),
                dirty: false,
            });
            model.names.insert(name, inode);
        }
        model.durable_names.clone_from(&model.names);
        model
    }

    fn open(&mut self, id: usize, name: &'static str, mode: OpenMode) {
        assert!(
            !self.handles.contains_key(&id),
            "table reused an open handle"
        );
        let inode = if let Some(&inode) = self.names.get(name) {
            inode
        } else {
            assert!(matches!(mode, OpenMode::Create), "table opened absent name");
            let inode = self.inodes.len();
            self.inodes.push(Inode {
                volatile: Vec::new(),
                durable: Vec::new(),
                dirty: false,
            });
            self.names.insert(name, inode);
            self.pending_names = true;
            inode
        };
        if matches!(mode, OpenMode::Truncate) {
            self.inodes[inode].volatile.clear();
            self.inodes[inode].dirty = true;
        }
        self.handles.insert(
            id,
            Handle {
                inode,
                cursor: 0,
                generation: self.generation,
            },
        );
    }

    fn write(&mut self, id: usize, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let handle = self.handles.get_mut(&id).expect("model handle");
        let inode = &mut self.inodes[handle.inode];
        let end = handle.cursor + bytes.len();
        if inode.volatile.len() < end {
            inode.volatile.resize(end, 0);
        }
        inode.volatile[handle.cursor..end].copy_from_slice(bytes);
        inode.dirty = true;
        handle.cursor = end;
    }

    fn read(&mut self, id: usize, count: usize) -> Vec<u8> {
        let handle = self.handles.get_mut(&id).expect("model handle");
        let image = &self.inodes[handle.inode].volatile;
        let start = handle.cursor.min(image.len());
        let end = (start + count).min(image.len());
        handle.cursor += end - start;
        image[start..end].to_vec()
    }

    fn sync_file(&mut self, id: usize, lies: Lies) {
        let inode = &mut self.inodes[self.handles[&id].inode];
        if !inode.dirty {
            return;
        }
        self.eligible_file_syncs += 1;
        if lies.fires(self.eligible_file_syncs) {
            self.file_lies += 1;
        } else {
            inode.durable.clone_from(&inode.volatile);
            inode.dirty = false;
        }
    }

    fn sync_directory(&mut self, lies: Lies) {
        if !self.pending_names {
            return;
        }
        self.eligible_directory_syncs += 1;
        if lies.fires(self.eligible_directory_syncs) {
            self.directory_lies += 1;
        } else {
            self.durable_names.clone_from(&self.names);
            self.pending_names = false;
        }
    }

    fn crash(&mut self, lose_names: bool) {
        self.generation += 1;
        if lose_names {
            self.names.clone_from(&self.durable_names);
        } else {
            self.durable_names.clone_from(&self.names);
        }
        self.pending_names = false;
        for inode in &mut self.inodes {
            inode.volatile.clone_from(&inode.durable);
            inode.dirty = false;
        }
    }
}

async fn check_state(
    vfs: &FaultVfs,
    root: &Path,
    model: &Reference,
    handles: &BTreeMap<usize, FaultFile<UnixVfs>>,
    names: &BTreeSet<&str>,
    context: &str,
) -> io::Result<()> {
    for name in names {
        let actual = vfs.read(&root.join(name)).await;
        if let Some(&inode) = model.names.get(name) {
            assert_eq!(
                actual?, model.inodes[inode].durable,
                "{context}: durable {name}"
            );
        } else {
            assert_eq!(
                actual.expect_err(context).kind(),
                io::ErrorKind::NotFound,
                "{context}: absent {name}"
            );
        }
    }
    for (&id, actual) in handles {
        let expected = &model.handles[&id];
        if expected.generation == model.generation {
            assert_eq!(
                actual.image()?,
                model.inodes[expected.inode].volatile,
                "{context}: volatile handle {id}"
            );
        } else {
            assert!(actual.image().is_err(), "{context}: stale image {id}");
        }
    }
    let events = vfs.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, FaultKind::FsyncLie { .. }))
            .count(),
        model.file_lies,
        "{context}: eligible file sync lies"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.kind, FaultKind::DirentSyncLie { .. }))
            .count(),
        model.directory_lies,
        "{context}: eligible directory sync lies"
    );
    Ok(())
}

async fn refuse_stale(file: &mut FaultFile<UnixVfs>, context: &str) {
    assert!(file.image().is_err(), "{context}: stale image");
    assert!(file.sync_all().await.is_err(), "{context}: stale sync_all");
    assert!(
        file.sync_data().await.is_err(),
        "{context}: stale sync_data"
    );
    assert!(file.metadata().await.is_err(), "{context}: stale metadata");
    assert!(file.set_len(0).await.is_err(), "{context}: stale set_len");
    assert!(
        poll_fn(|cx| Pin::new(&mut *file).poll_write(cx, b"bad"))
            .await
            .is_err(),
        "{context}: stale write"
    );
    assert!(
        poll_fn(|cx| Pin::new(&mut *file).poll_seek(cx, SeekFrom::Start(0)))
            .await
            .is_err(),
        "{context}: stale seek"
    );
    let mut bytes = [0; 1];
    let mut buffer = ReadBuf::new(&mut bytes);
    assert!(
        poll_fn(|cx| Pin::new(&mut *file).poll_read(cx, &mut buffer))
            .await
            .is_err(),
        "{context}: stale read"
    );
    assert!(
        poll_fn(|cx| Pin::new(&mut *file).poll_flush(cx))
            .await
            .is_err(),
        "{context}: stale flush"
    );
    assert!(
        poll_fn(|cx| Pin::new(&mut *file).poll_shutdown(cx))
            .await
            .is_err(),
        "{context}: stale shutdown"
    );
}

async fn run_case(case: &Case, root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    // Initial files are the already-durable starting state, not VFS actions.
    for &(name, bytes) in case.initial {
        std::fs::write(root.join(name), bytes)?;
    }
    let vfs = FaultVfs::unix(FaultPlan {
        fsync_lie: case.file_lies.trigger(),
        dirent_lie: case.directory_lies.trigger(),
        dirent_loss: if case.lose_names {
            Trigger::Always
        } else {
            Trigger::Never
        },
        ..FaultPlan::faultless()
    });
    let mut model = Reference::new(case.initial);
    let mut handles = BTreeMap::new();
    let mut names: BTreeSet<&str> = case.initial.iter().map(|(name, _)| *name).collect();
    for step in case.steps {
        match step {
            Step::Open(_, name, _) => {
                names.insert(name);
            }
            Step::Rename(from, to) => {
                names.insert(from);
                names.insert(to);
            }
            _ => {}
        }
    }
    for (index, &step) in case.steps.iter().enumerate() {
        let context = format!("{} step {index}: {step:?}", case.name);
        match step {
            Step::Open(id, name, mode) => {
                let options = OpenOptions::new().read(true).write(true);
                let options = match mode {
                    OpenMode::Existing => options,
                    OpenMode::Create => options.create(true),
                    OpenMode::Truncate => options.truncate(true),
                };
                let file = vfs.open(&root.join(name), &options).await?;
                model.open(id, name, mode);
                handles.insert(id, file);
            }
            Step::Close(id) => {
                drop(handles.remove(&id).expect("actual handle"));
                model.handles.remove(&id).expect("model handle");
            }
            Step::Write(id, bytes) => {
                let file = handles.get_mut(&id).expect("actual handle");
                let count = poll_fn(|cx| Pin::new(&mut *file).poll_write(cx, bytes)).await?;
                assert_eq!(count, bytes.len(), "{context}: accepted bytes");
                model.write(id, bytes);
            }
            Step::Seek(id, offset) => {
                let file = handles.get_mut(&id).expect("actual handle");
                assert_eq!(
                    poll_fn(|cx| Pin::new(&mut *file).poll_seek(cx, SeekFrom::Start(offset)))
                        .await?,
                    offset,
                    "{context}: cursor"
                );
                model.handles.get_mut(&id).expect("model handle").cursor = offset as usize;
            }
            Step::Read(id, count) => {
                let file = handles.get_mut(&id).expect("actual handle");
                let mut bytes = vec![0; count];
                let mut buffer = ReadBuf::new(&mut bytes);
                poll_fn(|cx| Pin::new(&mut *file).poll_read(cx, &mut buffer)).await?;
                assert_eq!(
                    buffer.filled(),
                    model.read(id, count),
                    "{context}: cursor read"
                );
            }
            Step::SyncAll(id) => {
                handles[&id].sync_all().await?;
                model.sync_file(id, case.file_lies);
            }
            Step::SyncData(id) => {
                handles[&id].sync_data().await?;
                model.sync_file(id, case.file_lies);
            }
            Step::Rename(from, to) => {
                vfs.rename(&root.join(from), &root.join(to)).await?;
                let inode = model.names.remove(from).expect("model rename source");
                model.names.insert(to, inode);
                model.pending_names = true;
            }
            Step::SyncDirectory => {
                let directory = vfs.open(root, &OpenOptions::new().read(true)).await?;
                directory.sync_all().await?;
                model.sync_directory(case.directory_lies);
            }
            Step::Crash => {
                vfs.crash().await?;
                model.crash(case.lose_names);
            }
            Step::RefuseStale(id) => {
                assert_ne!(
                    model.handles[&id].generation, model.generation,
                    "{context}: table must select a pre-crash handle"
                );
                refuse_stale(handles.get_mut(&id).expect("actual handle"), &context).await;
            }
        }
        check_state(&vfs, root, &model, &handles, &names, &context).await?;
    }
    Ok(())
}

#[test]
fn inode_dirty_reference_sequences() {
    use OpenMode::{Create, Existing, Truncate};
    use Step::{
        Close, Crash, Open, Read, RefuseStale, Rename, Seek, SyncAll, SyncData, SyncDirectory,
        Write,
    };

    let cases = [
        Case {
            name: "simultaneous_handles_share_image_not_cursors",
            initial: &[("file", b"")],
            file_lies: Lies::Never,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Existing),
                Open(1, "file", Existing),
                Write(0, b"abcd"),
                Read(1, 2),
                Write(1, b"XY"),
                Write(0, b"ef"),
                Seek(1, 0),
                Read(1, 6),
                SyncData(1),
                Crash,
                RefuseStale(0),
                RefuseStale(1),
                Open(2, "file", Existing),
                Read(2, 6),
            ],
        },
        Case {
            name: "close_reopen_retains_dirty_after_one_lie",
            initial: &[("file", b"")],
            file_lies: Lies::First,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Existing),
                Write(0, b"payload"),
                SyncAll(0),
                Close(0),
                Open(1, "file", Existing),
                Read(1, 7),
                SyncData(1),
                Close(1),
                Crash,
                Open(2, "file", Existing),
                Read(2, 7),
            ],
        },
        Case {
            name: "staging_lie_close_rename_reopen_honest_sync_survives",
            initial: &[],
            file_lies: Lies::First,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "staging", Create),
                Write(0, b"published bytes"),
                SyncAll(0),
                Close(0),
                Rename("staging", "published"),
                Open(1, "published", Existing),
                SyncData(1),
                SyncDirectory,
                Crash,
                RefuseStale(1),
                Open(2, "published", Existing),
                Read(2, 15),
            ],
        },
        Case {
            name: "staging_both_file_syncs_lie_loses_bytes_not_name",
            initial: &[],
            file_lies: Lies::Always,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "staging", Create),
                Write(0, b"lost bytes"),
                SyncAll(0),
                Close(0),
                Rename("staging", "published"),
                Open(1, "published", Existing),
                SyncData(1),
                SyncDirectory,
                Crash,
                RefuseStale(1),
                Open(2, "published", Existing),
                Read(2, 10),
            ],
        },
        Case {
            name: "staging_file_and_directory_syncs_lie_loses_name",
            initial: &[],
            file_lies: Lies::Always,
            directory_lies: Lies::Always,
            lose_names: true,
            steps: &[
                Open(0, "staging", Create),
                Write(0, b"lost bytes"),
                SyncAll(0),
                Close(0),
                Rename("staging", "published"),
                Open(1, "published", Existing),
                SyncData(1),
                SyncDirectory,
                Crash,
                RefuseStale(1),
            ],
        },
        Case {
            name: "old_handle_sync_after_rename_persists_current_inode",
            initial: &[("staging", b"")],
            file_lies: Lies::First,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "staging", Existing),
                Write(0, b"before"),
                SyncData(0),
                Rename("staging", "published"),
                Open(1, "published", Existing),
                Seek(1, 6),
                Write(1, b"-after"),
                SyncAll(0),
                SyncDirectory,
                Crash,
                RefuseStale(0),
                Open(2, "published", Existing),
                Read(2, 12),
            ],
        },
        Case {
            name: "overwrites_and_sparse_suffix_from_multiple_handles",
            initial: &[("file", b"abcdefgh")],
            file_lies: Lies::First,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Existing),
                Open(1, "file", Existing),
                Seek(0, 2),
                Write(0, b"WXYZ"),
                Seek(1, 4),
                Write(1, b"12"),
                SyncAll(0),
                Seek(1, 513),
                Write(1, b"end"),
                Close(1),
                SyncData(0),
                Crash,
                RefuseStale(0),
                Open(2, "file", Existing),
                Read(2, 8),
                Seek(2, 510),
                Read(2, 6),
            ],
        },
        Case {
            name: "plain_create_open_and_empty_write_do_not_consume_first_lie",
            initial: &[("file", b"old")],
            file_lies: Lies::First,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Existing),
                SyncAll(0),
                Open(1, "file", Create),
                Write(1, b""),
                SyncData(1),
                Write(0, b"new"),
                Open(2, "file", Create),
                SyncAll(2),
                Close(0),
                Close(1),
                Close(2),
                Open(3, "file", Existing),
                SyncData(3),
                Crash,
                Open(4, "file", Existing),
            ],
        },
        Case {
            name: "truncation_to_zero_lies_then_other_handle_honestly_syncs",
            initial: &[("file", b"durable contents")],
            file_lies: Lies::First,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Existing),
                Open(1, "file", Truncate),
                SyncData(1),
                Close(1),
                Open(2, "file", Existing),
                SyncAll(0),
                Crash,
                RefuseStale(0),
                RefuseStale(2),
                Open(3, "file", Existing),
                Read(3, 16),
            ],
        },
        Case {
            name: "lying_truncation_restores_durable_contents_on_crash",
            initial: &[("file", b"durable contents")],
            file_lies: Lies::Always,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Truncate),
                SyncAll(0),
                Close(0),
                Open(1, "file", Existing),
                SyncData(1),
                Crash,
                RefuseStale(1),
                Open(2, "file", Existing),
                Read(2, 16),
            ],
        },
        Case {
            name: "truncate_unsynced_image_with_zero_durable_length_is_eligible",
            initial: &[("file", b"")],
            file_lies: Lies::First,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Existing),
                Write(0, b"volatile"),
                Open(1, "file", Truncate),
                SyncData(1),
                Close(1),
                Open(2, "file", Create),
                SyncAll(2),
                Write(0, b"suffix"),
                SyncData(2),
                Crash,
                Open(3, "file", Existing),
                Read(3, 20),
            ],
        },
        Case {
            name: "durable_prefix_then_unsynced_suffix_is_lost",
            initial: &[("file", b"")],
            file_lies: Lies::Never,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "file", Existing),
                Write(0, b"prefix"),
                SyncData(0),
                Write(0, b"-suffix"),
                Close(0),
                Open(1, "file", Existing),
                Read(1, 13),
                Crash,
                RefuseStale(1),
                Open(2, "file", Existing),
                Read(2, 13),
            ],
        },
        Case {
            name: "directory_noop_then_lie_then_honest_settles_publish",
            initial: &[],
            file_lies: Lies::Never,
            directory_lies: Lies::First,
            lose_names: true,
            steps: &[
                SyncDirectory,
                Open(0, "staging", Create),
                Write(0, b"data"),
                SyncAll(0),
                Rename("staging", "published"),
                SyncDirectory,
                SyncDirectory,
                Crash,
                Open(1, "published", Existing),
                Read(1, 4),
            ],
        },
        Case {
            name: "rename_replacement_rollback_restores_both_durable_inodes",
            initial: &[("staging", b"source"), ("published", b"target")],
            file_lies: Lies::Never,
            directory_lies: Lies::Never,
            lose_names: true,
            steps: &[
                Open(0, "staging", Existing),
                Write(0, b"newsrc"),
                SyncData(0),
                Rename("staging", "published"),
                Open(1, "published", Existing),
                Read(1, 6),
                Crash,
                RefuseStale(0),
                RefuseStale(1),
                Open(2, "staging", Existing),
                Open(3, "published", Existing),
                Read(2, 6),
                Read(3, 6),
            ],
        },
    ];
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("fgdb-inode-dirty-{}-{unique}", std::process::id()));
    let runtime = RuntimeBuilder::new().build().expect("runtime builds");
    for case in &cases {
        let result = runtime.block_on(run_case(case, &root.join(case.name)));
        assert!(result.is_ok(), "{}: {result:?}", case.name);
    }
}

#[derive(Clone, Copy, Debug)]
enum DuringSync {
    Write,
    Crash,
    Cancel,
}

fn run_delayed_case(action: DuringSync) {
    let mut runtime = LabRuntime::new(LabConfig::new(0xd17e).with_auto_advance());
    let region = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, mut handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "fgdb-inode-delay-{}-{unique}-{action:?}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root)?;
            let path = root.join("file");
            std::fs::write(&path, b"")?;
            let cx = asupersync::Cx::current().expect("lab task has Cx");
            let vfs = FaultVfs::unix_with_clock(
                FaultPlan {
                    latency: Trigger::Always,
                    latency_micros: 1_000,
                    ..FaultPlan::faultless()
                },
                cx,
            );
            let options = OpenOptions::new().read(true).write(true);
            let mut writer = vfs.open(&path, &options).await?;
            let observer = vfs.open(&path, &options).await?;
            let accepted = poll_fn(|cx| Pin::new(&mut writer).poll_write(cx, b"original")).await?;
            assert_eq!(accepted, 8, "{action:?}: initial write");
            assert_eq!(observer.image()?, b"original", "{action:?}: observer image");
            let mut pending = Box::pin(observer.sync_all());
            let first_poll = poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx))).await;
            assert!(
                first_poll.is_pending(),
                "{action:?}: sync must really suspend"
            );
            assert_eq!(
                vfs.pending_latency_paths(),
                vec![path.clone()],
                "{action:?}: waiter"
            );
            assert_eq!(
                vfs.read(&path).await?,
                b"",
                "{action:?}: no premature durability"
            );
            match action {
                DuringSync::Write => {
                    poll_fn(|cx| Pin::new(&mut writer).poll_seek(cx, SeekFrom::Start(0))).await?;
                    let accepted =
                        poll_fn(|cx| Pin::new(&mut writer).poll_write(cx, b"replacement-plus"))
                            .await?;
                    assert_eq!(accepted, 16, "write during delay");
                    assert_eq!(
                        observer.image()?,
                        b"replacement-plus",
                        "shared image during delay"
                    );
                    pending.await?;
                    // A sync may include the racing write or leave it dirty, but must not
                    // forget it. A subsequent honest barrier must always persist it.
                    observer.sync_data().await?;
                    assert_eq!(
                        vfs.read(&path).await?,
                        b"replacement-plus",
                        "racing write was not forgotten"
                    );
                    vfs.crash().await?;
                    assert_eq!(
                        vfs.open(&path, &options).await?.image()?,
                        b"replacement-plus"
                    );
                }
                DuringSync::Crash => {
                    vfs.crash().await?;
                    assert!(
                        pending.await.is_err(),
                        "pre-crash delayed sync must refuse after wake"
                    );
                    assert_eq!(
                        vfs.read(&path).await?,
                        b"",
                        "resumed old sync cannot resurrect bytes"
                    );
                    assert_eq!(vfs.open(&path, &options).await?.image()?, b"");
                    refuse_stale(&mut writer, "crash during delayed sync").await;
                }
                DuringSync::Cancel => {
                    drop(pending);
                    assert!(
                        vfs.pending_latency_paths().is_empty(),
                        "cancel retires waiter"
                    );
                    assert!(
                        vfs.events().is_empty(),
                        "cancel is not a completed delay or sync"
                    );
                    drop(writer);
                    drop(observer);
                    let reopened = vfs.open(&path, &options).await?;
                    assert_eq!(
                        reopened.image()?,
                        b"original",
                        "cancel and close retain dirty inode"
                    );
                    reopened.sync_data().await?;
                    vfs.crash().await?;
                    assert_eq!(
                        vfs.read(&path).await?,
                        b"original",
                        "another handle persists cancelled sync data"
                    );
                }
            }
            assert!(
                vfs.pending_latency_paths().is_empty(),
                "{action:?}: no waiter leak"
            );
            Ok::<(), io::Error>(())
        })
        .expect("lab task spawns");
    runtime
        .scheduler
        .lock()
        .schedule(task_id, Budget::INFINITE.priority);
    let report = runtime.run_with_auto_advance();
    assert!(
        matches!(report.termination, AutoAdvanceTermination::Quiescent),
        "{action:?}: lab did not quiesce: {report:?}"
    );
    let lab_report = runtime.report();
    assert!(
        lab_report.lab_test_passed(),
        "{action:?}: lab failed: {lab_report:?}"
    );
    let result = handle
        .try_join()
        .expect("task joined")
        .expect("task finished");
    assert!(result.is_ok(), "{action:?}: {result:?}");
}

#[test]
fn delayed_sync_does_not_forget_another_handles_write() {
    run_delayed_case(DuringSync::Write);
}

#[test]
fn crash_during_delayed_sync_cannot_resurrect_bytes() {
    run_delayed_case(DuringSync::Crash);
}

#[test]
fn cancelled_delayed_sync_preserves_dirty_inode_after_all_handles_close() {
    run_delayed_case(DuringSync::Cancel);
}
