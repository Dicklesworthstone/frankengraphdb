//! Store-level laws for commit packs (fgdb-79kyi): what a batch publishes,
//! what every reader finds, and what each crash instant leaves behind.

use super::pack::{self, PACK_DIR, PACK_MIN_OBJECTS};
use super::{
    BLOCK_DIR, BlockStore, BlockStoreCrashPoint, PublishReceipts, StoreError, StoredObjectKind,
};
use crate::{DeltaBlockVersion, block_id};
use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, UnixVfs, Vfs};
use asupersync::lab::run_async_under_lab;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering::SeqCst};

const K_OID: [u8; 32] = [0x6b; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x29; 32]);

const INTERRUPT_OPEN: u8 = 1;
const INTERRUPT_REFRESH: u8 = 2;
const FAIL_SECOND_NEW_PACK: u8 = 3;

#[derive(Default)]
struct ReadFaults {
    mode: AtomicU8,
    opens: AtomicUsize,
    pack_opens: AtomicUsize,
    directories: AtomicUsize,
    failures: AtomicUsize,
}

impl ReadFaults {
    fn arm(&self, mode: u8) {
        self.opens.store(0, SeqCst);
        self.pack_opens.store(0, SeqCst);
        self.directories.store(0, SeqCst);
        self.failures.store(0, SeqCst);
        self.mode.store(mode, SeqCst);
    }
}

// Fault the physical read/open or discovery boundary, keeping file contents,
// copy selection, proof, and recovery on the production path.
struct ReadFaultVfs(Arc<ReadFaults>);

impl Vfs for ReadFaultVfs {
    type File = <UnixVfs as Vfs>::File;

    async fn open(&self, path: &Path, opts: &OpenOptions) -> io::Result<Self::File> {
        self.0.opens.fetch_add(1, SeqCst);
        if self
            .0
            .mode
            .compare_exchange(INTERRUPT_OPEN, 0, SeqCst, SeqCst)
            .is_ok()
        {
            self.0.failures.fetch_add(1, SeqCst);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "interrupted copy read",
            ));
        }
        if self.0.mode.load(SeqCst) == FAIL_SECOND_NEW_PACK
            && path
                .extension()
                .is_some_and(|extension| extension == "pack")
            && self.0.pack_opens.fetch_add(1, SeqCst) == 1
        {
            self.0.mode.store(0, SeqCst);
            self.0.failures.fetch_add(1, SeqCst);
            return Err(io::Error::other("unrelated new pack is unreadable"));
        }
        UnixVfs.open(path, opts).await
    }

    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        UnixVfs.metadata(path).await
    }
    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        UnixVfs.symlink_metadata(path).await
    }
    async fn set_permissions(&self, path: &Path, perm: Permissions) -> io::Result<()> {
        UnixVfs.set_permissions(path, perm).await
    }
    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        UnixVfs.create_dir(path).await
    }
    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        UnixVfs.create_dir_all(path).await
    }
    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        UnixVfs.remove_dir(path).await
    }
    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        UnixVfs.remove_dir_all(path).await
    }
    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.0.directories.fetch_add(1, SeqCst);
        if self
            .0
            .mode
            .compare_exchange(INTERRUPT_REFRESH, 0, SeqCst, SeqCst)
            .is_ok()
        {
            self.0.failures.fetch_add(1, SeqCst);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "interrupted pack discovery",
            ));
        }
        UnixVfs.read_dir(path).await
    }
    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        UnixVfs.remove_file(path).await
    }
    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        UnixVfs.rename(from, to).await
    }
    async fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        UnixVfs.copy(from, to).await
    }
    async fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        UnixVfs.hard_link(from, to).await
    }
    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        UnixVfs.canonicalize(path).await
    }
    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        UnixVfs.read_link(path).await
    }
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        UnixVfs.read(path).await
    }
    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        UnixVfs.read_to_string(path).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        UnixVfs.write(path, bytes).await
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-packs-{}-{name}", std::process::id()));
    if dir.exists() {
        panic!("scratch dir {} already exists", dir.display());
    }
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    T: Send + 'static,
    Fut: Future<Output = T> + Send,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    output
}

/// `count` distinct objects of varying length, with their block identities.
fn objects(seed: u8, count: usize) -> Vec<(ObjectId, Vec<u8>)> {
    (0..count)
        .map(|at| {
            let mut bytes = format!("object {seed} {at} ").into_bytes();
            bytes.resize(bytes.len() + at % 37, seed);
            (block_id(&K_OID, NAMESPACE, &bytes), bytes)
        })
        .collect()
}

async fn open(cx: &CommitCx, dir: &Path) -> BlockStore {
    BlockStore::open(cx, dir, K_OID, NAMESPACE)
        .await
        .expect("open store")
}

async fn publish(
    cx: &CommitCx,
    store: &BlockStore,
    objects: &[(ObjectId, Vec<u8>)],
    crash_at: Option<BlockStoreCrashPoint>,
) -> Result<(), StoreError> {
    let mut receipts = PublishReceipts::new();
    let mut batch = store.publication_batch(cx, &mut receipts, crash_at)?;
    for (id, bytes) in objects {
        batch
            .put_object(cx, StoredObjectKind::Block, *id, bytes)
            .await?;
    }
    batch.finish(cx).await
}

async fn read_all(
    cx: &CommitCx,
    store: &BlockStore,
    objects: &[(ObjectId, Vec<u8>)],
) -> Vec<Result<Vec<u8>, StoreError>> {
    let mut out = Vec::new();
    for (id, _) in objects {
        out.push(store.get_bytes(cx, DeltaBlockVersion(*id)).await);
    }
    out
}

fn packs_in(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir.join(BLOCK_DIR).join(PACK_DIR)) {
        Ok(entries) => entries
            .map(|entry| entry.expect("pack dir entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Where object `at` of `objects` sits in the pack they were published as.
fn offset_in_pack(objects: &[(ObjectId, Vec<u8>)], at: usize) -> usize {
    12 + objects.len() * 44
        + objects[..at]
            .iter()
            .map(|(_, bytes)| bytes.len())
            .sum::<usize>()
}

#[test]
fn a_large_batch_publishes_one_pack_that_every_store_reads() {
    let dir = scratch_dir("one-pack");
    let objects = objects(1, PACK_MIN_OBJECTS + 36);
    let (written, reopened) = under_lab(0x9a01, {
        let dir = dir.clone();
        let objects = objects.clone();
        move |cx| async move {
            let store = open(&cx, &dir).await;
            publish(&cx, &store, &objects, None).await.expect("publish");
            let written = read_all(&cx, &store, &objects).await;
            // A fresh store finds the pack through its directory alone.
            let reopened = read_all(&cx, &open(&cx, &dir).await, &objects).await;
            (written, reopened)
        }
    });
    for reads in [written, reopened] {
        let reads: Vec<Vec<u8>> = reads.into_iter().map(|read| read.expect("read")).collect();
        assert_eq!(
            reads,
            objects
                .iter()
                .map(|(_, bytes)| bytes.clone())
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(packs_in(&dir).len(), 1);
    for (id, _) in &objects {
        let store_path = dir.join(BLOCK_DIR).join(format!("{}.block", hex(id)));
        assert!(!store_path.exists(), "a packed object has no loose file");
    }
    // Nothing but the pack itself is left in the pack directory.
    let names: Vec<_> = std::fs::read_dir(dir.join(BLOCK_DIR).join(PACK_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1);
}

/// Past the resident budget a pack is never loaded whole: each object is
/// read alone at its offset, and still verifies (or is refused) by identity.
#[test]
fn past_the_resident_budget_each_object_is_read_alone() {
    let dir = scratch_dir("budget-spent");
    let objects = objects(11, PACK_MIN_OBJECTS + 9);
    let reads = under_lab(0x9a09, {
        let dir = dir.clone();
        let objects = objects.clone();
        move |cx| async move {
            publish(&cx, &open(&cx, &dir).await, &objects, None)
                .await
                .expect("publish");
            let store = open(&cx, &dir).await;
            store.packs.spend_resident_budget();
            let reads = read_all(&cx, &store, &objects).await;
            // Nothing was admitted as resident.
            assert_eq!(store.packs.resident_packs(), 0);
            // Without the spent budget the first read loads the pack whole,
            // and it stays resident once every object has been read.
            let unbudgeted = open(&cx, &dir).await;
            assert!(
                read_all(&cx, &unbudgeted, &objects)
                    .await
                    .iter()
                    .all(Result::is_ok)
            );
            assert_eq!(unbudgeted.packs.resident_packs(), 1);
            // A released pack still serves every object, one at a time.
            unbudgeted.packs.release();
            assert_eq!(unbudgeted.packs.resident_packs(), 0);
            let again = read_all(&cx, &unbudgeted, &objects).await;
            assert!(
                again
                    .iter()
                    .zip(&objects)
                    .all(|(read, (_, bytes))| read.as_ref().unwrap() == bytes)
            );
            assert_eq!(unbudgeted.packs.resident_packs(), 0);
            reads
        }
    });
    let reads: Vec<Vec<u8>> = reads.into_iter().map(|read| read.expect("read")).collect();
    assert_eq!(
        reads,
        objects
            .iter()
            .map(|(_, bytes)| bytes.clone())
            .collect::<Vec<_>>()
    );
}

/// A resident pack is evicted only once every object in it has been read,
/// and only when another pack needs its bytes. Many blocks can name one small
/// object (an identical property patch), so a walk reads that object again and
/// again, before and after the pack's other objects; neither may cost the
/// pack its residency while the budget has room.
#[test]
fn rereading_one_object_does_not_release_its_pack() {
    let dir = scratch_dir("reread");
    let objects = objects(12, PACK_MIN_OBJECTS + 3);
    under_lab(0x9a0a, {
        let dir = dir.clone();
        let objects = objects.clone();
        move |cx| async move {
            publish(&cx, &open(&cx, &dir).await, &objects, None)
                .await
                .expect("publish");
            let store = open(&cx, &dir).await;
            // As many reads as the pack holds objects, all of the first one.
            let first = std::slice::from_ref(&objects[0]);
            for _ in 0..objects.len() {
                let read = read_all(&cx, &store, first).await;
                assert_eq!(read[0].as_ref().unwrap(), &objects[0].1);
            }
            assert_eq!(
                store.packs.resident_packs(),
                1,
                "rereads of one object do not release its pack"
            );
            let (last, rest) = objects.split_last().unwrap();
            assert!(
                read_all(&cx, &store, rest)
                    .await
                    .iter()
                    .zip(rest)
                    .all(|(read, (_, bytes))| read.as_ref().unwrap() == bytes)
            );
            assert_eq!(store.packs.resident_packs(), 1);
            let final_read = read_all(&cx, &store, std::slice::from_ref(last)).await;
            assert_eq!(final_read[0].as_ref().unwrap(), &last.1);
            for _ in 0..3 {
                assert_eq!(
                    read_all(&cx, &store, first).await[0].as_ref().unwrap(),
                    &objects[0].1
                );
            }
            assert_eq!(
                store.packs.resident_packs(),
                1,
                "a spent pack stays resident while the budget has room"
            );
            assert_eq!(store.packs.whole_reads(), 1);
        }
    });
}

#[test]
fn verification_reads_do_not_reuse_warm_payloads_or_previously_admitted_tables() {
    let dir = scratch_dir("verify-physical");
    let objects = objects(21, PACK_MIN_OBJECTS + 3);
    under_lab(0x9a21, move |cx| async move {
        let store = open(&cx, &dir).await;
        publish(&cx, &store, &objects, None).await.unwrap();
        assert!(
            read_all(&cx, &store, &objects)
                .await
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(store.packs.resident_packs(), 1);

        let verifier = store.verification_reader(&cx).await.unwrap();
        assert!(
            read_all(&cx, &verifier, &objects)
                .await
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(verifier.packs.resident_packs(), 0);
        assert_eq!(verifier.packs.whole_reads(), 0);
        let [pack] = &packs_in(&dir)[..] else {
            panic!("one pack");
        };
        let original = std::fs::read(pack).unwrap();
        let mut damaged = original.clone();
        damaged[offset_in_pack(&objects, 5)] ^= 0x40;
        std::fs::write(pack, &damaged).unwrap();
        let target = DeltaBlockVersion(objects[5].0);

        // The ordinary cache is a control: re-hashing its old bytes is not a
        // new observation of storage, even though those bytes remain valid.
        assert_eq!(store.get_bytes(&cx, target).await.unwrap(), objects[5].1);
        let fresh = store.verification_reader(&cx).await.unwrap();
        for reader in [&verifier, &fresh] {
            assert!(matches!(
                reader.get_bytes(&cx, target).await,
                Err(StoreError::IdentityMismatch { expected, .. }) if expected == target.0
            ));
            assert_eq!(reader.packs.resident_packs(), 0);
        }
        assert_eq!(std::fs::read(pack).unwrap(), damaged);

        // Payloads alone are not enough: a previously admitted pack table
        // must not make an unopenable on-disk container look usable either.
        let mut damaged = original;
        damaged[0] ^= 1;
        std::fs::write(pack, &damaged).unwrap();
        assert_eq!(store.get_bytes(&cx, target).await.unwrap(), objects[5].1);
        let verifier = store.verification_reader(&cx).await.unwrap();
        assert!(matches!(
            verifier.get_bytes(&cx, target).await,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
        assert_eq!(std::fs::read(pack).unwrap(), damaged);
        assert_eq!(store.packs.resident_packs(), 1);
    });
}

/// A pack that does not fit the budget evicts a spent one, and never one
/// with objects still unread.
#[test]
fn a_spent_pack_is_evicted_to_admit_another_and_an_unspent_one_is_not() {
    let dir = scratch_dir("evict");
    let first = objects(14, PACK_MIN_OBJECTS + 5);
    let second = objects(15, PACK_MIN_OBJECTS + 5);
    under_lab(0x9a0b, {
        let dir = dir.clone();
        move |cx| async move {
            let store = open(&cx, &dir).await;
            publish(&cx, &store, &first, None).await.expect("publish");
            publish(&cx, &store, &second, None).await.expect("publish");
            assert_eq!(packs_in(&dir).len(), 2);
            let store = open(&cx, &dir).await;
            // The first pack is resident with one object unread; the rest of
            // the budget is gone.
            let (last, rest) = first.split_last().unwrap();
            assert!(read_all(&cx, &store, rest).await.iter().all(Result::is_ok));
            store.packs.spend_resident_budget();
            assert!(read_all(&cx, &store, &second[..1]).await[0].is_ok());
            assert_eq!(
                (store.packs.resident_packs(), store.packs.whole_reads()),
                (1, 1),
                "an unspent pack is not evicted: the second pack is read one object at a time"
            );
            // Spent, the first pack gives its bytes to the second.
            assert!(read_all(&cx, &store, std::slice::from_ref(last)).await[0].is_ok());
            let reads = read_all(&cx, &store, &second).await;
            assert!(
                reads
                    .iter()
                    .zip(&second)
                    .all(|(read, (_, bytes))| read.as_ref().unwrap() == bytes)
            );
            assert_eq!(
                (store.packs.resident_packs(), store.packs.whole_reads()),
                (1, 2),
                "the second pack is loaded whole in the first one's place"
            );
            // The evicted pack still serves its objects, one at a time.
            assert!(
                read_all(&cx, &store, &first)
                    .await
                    .iter()
                    .zip(&first)
                    .all(|(read, (_, bytes))| read.as_ref().unwrap() == bytes)
            );
            assert_eq!(store.packs.whole_reads(), 2);
        }
    });
}

#[test]
fn a_small_batch_stays_loose_and_creates_no_pack_directory() {
    let dir = scratch_dir("loose");
    let objects = objects(2, PACK_MIN_OBJECTS - 1);
    let reads = under_lab(0x9a02, {
        let dir = dir.clone();
        let objects = objects.clone();
        move |cx| async move {
            let store = open(&cx, &dir).await;
            publish(&cx, &store, &objects, None).await.expect("publish");
            read_all(&cx, &open(&cx, &dir).await, &objects).await
        }
    });
    assert!(reads.into_iter().all(|read| read.is_ok()));
    assert!(!dir.join(BLOCK_DIR).join(PACK_DIR).exists());
    for (id, _) in &objects {
        assert!(
            dir.join(BLOCK_DIR)
                .join(format!("{}.block", hex(id)))
                .is_file()
        );
    }
}

#[test]
fn a_damaged_packed_object_is_refused_by_its_identity_alone() {
    let dir = scratch_dir("damage");
    let objects = objects(3, PACK_MIN_OBJECTS);
    under_lab(0x9a03, {
        let dir = dir.clone();
        let objects = objects.clone();
        move |cx| async move {
            publish(&cx, &open(&cx, &dir).await, &objects, None)
                .await
                .expect("publish");
        }
    });
    let [pack] = &packs_in(&dir)[..] else {
        panic!("one pack");
    };
    let mut bytes = std::fs::read(pack).unwrap();
    bytes[offset_in_pack(&objects, 5)] ^= 0x40;
    std::fs::write(pack, &bytes).unwrap();
    let reads = under_lab(0x9a04, {
        let dir = dir.clone();
        let objects = objects.clone();
        move |cx| async move { read_all(&cx, &open(&cx, &dir).await, &objects).await }
    });
    for (at, read) in reads.into_iter().enumerate() {
        if at == 5 {
            assert!(
                matches!(read, Err(StoreError::IdentityMismatch { expected, .. }) if expected == objects[5].0),
                "{read:?}"
            );
        } else {
            assert_eq!(read.expect("undamaged object"), objects[at].1);
        }
    }
}

#[test]
fn a_newer_intact_copy_is_read_instead_of_a_damaged_older_one() {
    let dir = scratch_dir("copies");
    let first = objects(4, PACK_MIN_OBJECTS + 3);
    // A later batch republishes object 7 beside enough new ones to pack.
    let mut second = objects(5, PACK_MIN_OBJECTS);
    second.insert(0, first[7].clone());
    under_lab(0x9a05, {
        let dir = dir.clone();
        let first = first.clone();
        move |cx| async move {
            publish(&cx, &open(&cx, &dir).await, &first, None)
                .await
                .expect("publish");
        }
    });
    let [older] = &packs_in(&dir)[..] else {
        panic!("one pack");
    };
    let mut bytes = std::fs::read(older).unwrap();
    bytes[offset_in_pack(&first, 7)] ^= 0x01;
    std::fs::write(older, &bytes).unwrap();
    let reread = under_lab(0x9a06, {
        let dir = dir.clone();
        let second = second.clone();
        let id = first[7].0;
        move |cx| async move {
            // A store opened before the republication, and one opened after.
            let before = open(&cx, &dir).await;
            assert!(matches!(
                before.get_bytes(&cx, DeltaBlockVersion(id)).await,
                Err(StoreError::IdentityMismatch { .. })
            ));
            publish(&cx, &open(&cx, &dir).await, &second, None)
                .await
                .expect("republish");
            let after = open(&cx, &dir).await;
            (
                after.get_bytes(&cx, DeltaBlockVersion(id)).await,
                // No unrelated object miss may be needed to discover this
                // repair. The same live handle has already cached the bad
                // original pack above.
                before.get_bytes(&cx, DeltaBlockVersion(id)).await,
                read_all(&cx, &before, &second[1..]).await,
            )
        }
    });
    assert_eq!(reread.0.expect("intact copy"), first[7].1);
    assert_eq!(reread.1.expect("live handle discovers repair"), first[7].1);
    // The store opened before the second pack finds its objects on a miss.
    assert!(reread.2.into_iter().all(|read| read.is_ok()));
    assert_eq!(packs_in(&dir).len(), 2);
}

/// The newest copy is not trusted for being newest: a store whose refresh
/// indexes a later pack holding a damaged copy still reads the intact one.
#[test]
fn a_damaged_newest_copy_falls_back_to_an_intact_older_one() {
    let dir = scratch_dir("damaged-newest");
    let first = objects(9, PACK_MIN_OBJECTS);
    let mut second = objects(10, PACK_MIN_OBJECTS);
    let mut damaged = first[3].1.clone();
    damaged[0] ^= 0x20;
    second.insert(0, (first[3].0, damaged));
    let (read, others, verified) = under_lab(0x9a08, {
        let dir = dir.clone();
        let first = first.clone();
        let second = second.clone();
        move |cx| async move {
            publish(&cx, &open(&cx, &dir).await, &first, None)
                .await
                .expect("publish");
            let before = open(&cx, &dir).await;
            publish(&cx, &open(&cx, &dir).await, &second, None)
                .await
                .expect("publish the damaged copy");
            // A miss on the second pack's objects refreshes `before`, which
            // then indexes the damaged copy as the newest.
            let others = read_all(&cx, &before, &second[1..]).await;
            (
                before.get_bytes(&cx, DeltaBlockVersion(first[3].0)).await,
                others,
                before
                    .verification_reader(&cx)
                    .await
                    .unwrap()
                    .get_bytes(&cx, DeltaBlockVersion(first[3].0))
                    .await,
            )
        }
    });
    assert!(others.into_iter().all(|read| read.is_ok()));
    assert_eq!(read.expect("intact older copy"), first[3].1);
    assert_eq!(
        verified.expect("physically verified intact copy"),
        first[3].1
    );
}

#[test]
fn live_reads_use_a_loose_repair_of_a_damaged_missing_or_truncated_pack() {
    for (case, seed) in [("damaged", 21), ("missing", 22), ("truncated", 23)] {
        let dir = scratch_dir(&format!("loose-repair-{case}"));
        let objects = objects(seed, PACK_MIN_OBJECTS);
        under_lab(0x9c00 + u64::from(seed), move |cx| async move {
            let publisher = open(&cx, &dir).await;
            publish(&cx, &publisher, &objects, None)
                .await
                .expect("original pack");
            let reader = open(&cx, &dir).await;
            let physical = reader
                .verification_reader(&cx)
                .await
                .expect("physical reader");
            let pack = packs_in(&dir).pop().expect("one pack");
            let original = std::fs::read(&pack).expect("pack bytes");
            match case {
                "damaged" => {
                    let mut damaged = original.clone();
                    damaged[offset_in_pack(&objects, 5)] ^= 0x40;
                    std::fs::write(&pack, damaged).expect("damage payload");
                }
                "missing" => {
                    std::fs::rename(&pack, pack.with_extension("kept")).expect("retain lost pack");
                }
                "truncated" => {
                    std::fs::write(&pack, &original[..12]).expect("truncate fixture");
                }
                _ => unreachable!(),
            }
            let (id, bytes) = &objects[5];
            assert!(reader.get_bytes(&cx, DeltaBlockVersion(*id)).await.is_err());
            assert_eq!(
                publisher
                    .put(&cx, bytes)
                    .await
                    .expect("publish loose repair"),
                DeltaBlockVersion(*id)
            );
            for store in [&reader, &physical] {
                assert_eq!(
                    store
                        .get_bytes(&cx, DeltaBlockVersion(*id))
                        .await
                        .expect("repaired read"),
                    *bytes,
                    "{case}"
                );
                let mut exact = vec![0; bytes.len()];
                store
                    .read_object_exact(
                        &cx,
                        *id,
                        crate::tiered::buffer::extent_checksum(bytes),
                        &mut exact,
                    )
                    .await
                    .expect("admitted extent uses the same repair");
                assert_eq!(exact, *bytes);
                let limit = bytes.len() as u64 - 1;
                assert!(
                    store
                        .read_metadata_bytes(&cx, *id, StoredObjectKind::Block, limit)
                        .await
                        .is_err(),
                    "{case}: an intact repair cannot bypass the original byte ceiling"
                );
            }
        });
    }
}

#[test]
fn failed_repairs_preserve_the_first_error_and_the_requested_identity_family() {
    let dir = scratch_dir("repair-family");
    under_lab(0x9c30, move |cx| async move {
        let value = b"a legitimate vertex-patch transcript, not a block".to_vec();
        let id = crate::vertex::vertex_patch_id(&K_OID, NAMESPACE, &value);
        let mut first = objects(24, PACK_MIN_OBJECTS);
        let bad = b"the original damaged copy".to_vec();
        first.push((id, bad.clone()));
        let publisher = open(&cx, &dir).await;
        publish(&cx, &publisher, &first, None)
            .await
            .expect("damaged pack fixture");
        let reader = open(&cx, &dir).await;
        let original_actual = block_id(&K_OID, NAMESPACE, &bad);
        assert!(matches!(
            reader.get_bytes(&cx, DeltaBlockVersion(id)).await,
            Err(StoreError::IdentityMismatch { expected, actual })
                if expected == id && actual == original_actual
        ));
        // This copy really has the requested identity, but only under the
        // vertex-patch family. A generic any-family predicate must not let
        // it satisfy a block reader or replace that reader's original error.
        publisher
            .put_patch(&cx, &value)
            .await
            .expect("loose patch copy");
        let mut second = objects(25, PACK_MIN_OBJECTS);
        second.push((id, value.clone()));
        publish(&cx, &publisher, &second, None)
            .await
            .expect("packed patch copy");
        assert!(matches!(
            reader.get_bytes(&cx, DeltaBlockVersion(id)).await,
            Err(StoreError::IdentityMismatch { expected, actual })
                if expected == id && actual == original_actual
        ));
        assert_eq!(
            reader
                .get_patch_bytes(&cx, crate::vertex::VertexPatchVersion(id))
                .await
                .expect("exact-family read"),
            value
        );
    });
}

#[test]
fn an_interrupted_preferred_copy_never_falls_through_to_another_intact_copy() {
    let dir = scratch_dir("interrupted-copy");
    under_lab(0x9c31, move |cx| async move {
        let first = objects(26, PACK_MIN_OBJECTS);
        let mut second = objects(27, PACK_MIN_OBJECTS);
        second.push(first[7].clone());
        let publisher = open(&cx, &dir).await;
        publish(&cx, &publisher, &first, None)
            .await
            .expect("first pack");
        publish(&cx, &publisher, &second, None)
            .await
            .expect("second pack");
        let faults = Arc::new(ReadFaults::default());
        let reader = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            ReadFaultVfs(Arc::clone(&faults)),
            &dir,
            K_OID,
            NAMESPACE,
        )
        .await
        .expect("faultable reader");
        assert!(matches!(
            reader.packs.locate(first[7].0),
            pack::Located::Several(_)
        ));
        faults.arm(INTERRUPT_OPEN);
        assert!(matches!(
            reader.get_bytes(&cx, DeltaBlockVersion(first[7].0)).await,
            Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::Interrupted
        ));
        assert_eq!(faults.failures.load(SeqCst), 1);
        assert_eq!(
            faults.opens.load(SeqCst),
            1,
            "no second physical read after interruption"
        );
        assert_eq!(
            faults.directories.load(SeqCst),
            0,
            "no discovery after interruption"
        );
        assert_eq!(
            reader
                .get_bytes(&cx, DeltaBlockVersion(first[7].0))
                .await
                .expect("later read"),
            first[7].1
        );
    });
}

#[test]
fn recovery_obeys_interrupted_discovery_and_uses_a_partially_refreshed_index() {
    for mode in [INTERRUPT_REFRESH, FAIL_SECOND_NEW_PACK] {
        let dir = scratch_dir(&format!("discovery-fault-{mode}"));
        under_lab(0x9c40 + u64::from(mode), move |cx| async move {
            let first = objects(28, PACK_MIN_OBJECTS);
            let publisher = open(&cx, &dir).await;
            publish(&cx, &publisher, &first, None)
                .await
                .expect("first pack");
            let path = packs_in(&dir).pop().expect("one pack");
            let mut damaged = std::fs::read(&path).expect("pack");
            damaged[offset_in_pack(&first, 7)] ^= 0x20;
            std::fs::write(&path, damaged).expect("damage preferred payload");
            let faults = Arc::new(ReadFaults::default());
            let reader = BlockStore::open_existing_for_read_with_vfs(
                &cx,
                ReadFaultVfs(Arc::clone(&faults)),
                &dir,
                K_OID,
                NAMESPACE,
            )
            .await
            .expect("faultable reader");
            assert!(matches!(
                reader.get_bytes(&cx, DeltaBlockVersion(first[7].0)).await,
                Err(StoreError::IdentityMismatch { .. })
            ));
            // Both new packs carry the repair. Whichever directory entry
            // comes first is admitted before the next entry faults, so the
            // partial-refresh law does not depend on filesystem ordering.
            for seed in [29, 30] {
                let mut repair = objects(seed, PACK_MIN_OBJECTS);
                repair.push(first[7].clone());
                publish(&cx, &publisher, &repair, None)
                    .await
                    .expect("new repair pack");
            }
            faults.arm(mode);
            let result = reader.get_bytes(&cx, DeltaBlockVersion(first[7].0)).await;
            assert_eq!(faults.failures.load(SeqCst), 1, "the requested fault ran");
            if mode == INTERRUPT_REFRESH {
                assert!(
                    matches!(result, Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::Interrupted)
                );
                assert_eq!(faults.directories.load(SeqCst), 1);
            } else {
                assert_eq!(
                    result.expect("repair admitted before unrelated failure"),
                    first[7].1
                );
                assert_eq!(faults.pack_opens.load(SeqCst), 2);
            }
            assert_eq!(
                reader
                    .get_bytes(&cx, DeltaBlockVersion(first[7].0))
                    .await
                    .expect("subsequent read"),
                first[7].1
            );
        });
    }
}

#[test]
fn root_windows_recover_block_and_hosted_patch_proofs_from_older_copies() {
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};

    for (case, patch_target, missing) in [
        (0, false, false),
        (1, true, false),
        (2, false, true),
        (3, true, true),
    ] {
        let dir = scratch_dir(&format!("window-repair-{case}"));
        under_lab(0x9c50 + case, move |cx| async move {
            let edge = crate::AdjacencyEntry {
                src: VId(1),
                relation: RelationId(1),
                dst: VId(2),
                eid: EId(1),
                created_at: CommitSeq(1),
                retired_at: None,
            };
            let plain = crate::encode_block(0, None, &[edge]).expect("plain block");
            let plain_id = block_id(&K_OID, NAMESPACE, &plain);
            let rows = vec![vec![(PropertyKeyId(1), CanonicalScalar::Int(42).into())]];
            let patch = crate::edge_props::encode_property_patch(&rows).expect("patch");
            let patch_id = crate::edge_props::property_patch_id(&K_OID, NAMESPACE, &patch);
            let hosted =
                crate::encode_block_with_properties(0, None, &[edge], patch_id, &[1], &rows)
                    .expect("hosted block");
            let hosted_id = block_id(&K_OID, NAMESPACE, &hosted);
            let mut first = objects(31, PACK_MIN_OBJECTS);
            first.extend([
                (plain_id, plain),
                (hosted_id, hosted.clone()),
                (patch_id, patch.clone()),
            ]);
            let publisher = open(&cx, &dir).await;
            publish(&cx, &publisher, &first, None)
                .await
                .expect("original objects");
            let original_pack = packs_in(&dir).pop().expect("original pack");
            let reader = open(&cx, &dir).await;
            let (id, mut bytes) = if patch_target {
                (patch_id, patch)
            } else {
                (hosted_id, hosted)
            };
            if !missing {
                bytes[0] ^= 0x20;
            }
            let mut second = objects(32, PACK_MIN_OBJECTS);
            second.push((id, bytes));
            publish(&cx, &publisher, &second, None)
                .await
                .expect("newer copy");
            assert!(
                reader
                    .packs
                    .refresh(&cx, &reader.vfs)
                    .await
                    .expect("index newer copy")
            );
            if missing {
                let newer = packs_in(&dir)
                    .into_iter()
                    .find(|path| *path != original_pack)
                    .expect("newer pack");
                std::fs::rename(&newer, newer.with_extension("kept")).expect("retain lost pack");
            }
            let mut window = reader
                .read_block_window(&cx, vec![plain_id, hosted_id])
                .await;
            assert_eq!(window.len(), 2);
            let ((_, decoded), prefetched) = window.remove(0).expect("plain block in window");
            assert_eq!(decoded.expect("plain decoded"), (vec![edge], None));
            assert!(prefetched.is_none());
            let ((_, decoded), prefetched) = window.remove(0).expect("hosted block in window");
            assert_eq!(
                decoded.expect("hosted decoded"),
                (vec![edge], Some((patch_id, vec![1])))
            );
            let Some(super::PrefetchedPatch::Read(_, proof)) = prefetched else {
                panic!("the window must retain its hosted patch proof");
            };
            assert_eq!(proof.expect("recovered window patch"), rows);
            let (_, prefetched) = reader
                .read_block_decoded(&cx, hosted_id, true)
                .await
                .expect("single-block recovery path");
            let Some(super::PrefetchedPatch::Read(_, proof)) = prefetched else {
                panic!("the single read must retain its hosted patch proof");
            };
            assert_eq!(proof.expect("recovered single-read patch"), rows);
        });
    }
}

#[test]
fn a_republished_pack_is_resynced_and_a_squatter_is_never_rewritten() {
    let dir = scratch_dir("republish");
    let squatted = objects(7, PACK_MIN_OBJECTS);
    let objects = objects(6, PACK_MIN_OBJECTS);
    let squat_name = {
        let entries: Vec<(ObjectId, &[u8])> = squatted
            .iter()
            .map(|(id, bytes)| (*id, bytes.as_slice()))
            .collect();
        pack::name(&pack::encode(&entries))
    };
    let (again, squat) = under_lab(0x9a07, {
        let dir = dir.clone();
        let objects = objects.clone();
        let squatted = squatted.clone();
        let squat_name = squat_name.clone();
        move |cx| async move {
            let store = open(&cx, &dir).await;
            publish(&cx, &store, &objects, None).await.expect("publish");
            let again = publish(&cx, &store, &objects, None).await;
            std::fs::write(
                dir.join(BLOCK_DIR).join(PACK_DIR).join(&squat_name),
                b"squatter",
            )
            .unwrap();
            let squat = publish(&cx, &store, &squatted, None).await;
            (again, squat)
        }
    });
    again.expect("identical republication");
    assert!(
        matches!(squat, Err(StoreError::Collision { .. })),
        "{squat:?}"
    );
    assert_eq!(
        std::fs::read(dir.join(BLOCK_DIR).join(PACK_DIR).join(&squat_name)).unwrap(),
        b"squatter"
    );
    assert_eq!(packs_in(&dir).len(), 2);
}

/// At every crash instant of a pack flush, the pack is either absent (its
/// objects read as missing) or complete under its name; a later publication
/// of the same objects then succeeds, and every object reads back.
#[test]
fn every_pack_crash_instant_leaves_absence_or_the_complete_pack() {
    for (at, (crash, canonical)) in [
        (BlockStoreCrashPoint::AfterBatchStagingWrite, false),
        (
            BlockStoreCrashPoint::AfterStagingFileSyncBeforePublication,
            false,
        ),
        (
            BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync,
            true,
        ),
        (BlockStoreCrashPoint::AfterBatchRenames, true),
        (BlockStoreCrashPoint::AfterBatchFileSyncs, true),
        (BlockStoreCrashPoint::AfterBatchDirectorySync, true),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = scratch_dir(&format!("crash-{at}"));
        let objects = objects(8 + at as u8, PACK_MIN_OBJECTS + at);
        let (failed, after_crash, retried, after_retry) = under_lab(0x9b00 + at as u64, {
            let dir = dir.clone();
            let objects = objects.clone();
            move |cx| async move {
                let failed = publish(&cx, &open(&cx, &dir).await, &objects, Some(crash)).await;
                let after_crash = read_all(&cx, &open(&cx, &dir).await, &objects).await;
                let retried = publish(&cx, &open(&cx, &dir).await, &objects, None).await;
                let after_retry = read_all(&cx, &open(&cx, &dir).await, &objects).await;
                (failed, after_crash, retried, after_retry)
            }
        });
        assert!(failed.is_err(), "{crash:?} must stop the publication");
        if canonical {
            assert!(after_crash.iter().all(Result::is_ok), "{crash:?}");
        } else {
            assert!(
                after_crash.iter().all(|read| matches!(read, Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)),
                "{crash:?}: {after_crash:?}"
            );
        }
        retried.unwrap_or_else(|error| panic!("{crash:?}: retry {error:?}"));
        let reads: Vec<Vec<u8>> = after_retry.into_iter().map(|read| read.unwrap()).collect();
        assert_eq!(
            reads,
            objects
                .iter()
                .map(|(_, bytes)| bytes.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(packs_in(&dir).len(), 1, "{crash:?}");
    }
}

fn hex(id: &ObjectId) -> String {
    id.0.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Every root walk reads each pack whole once, however many walks came
/// before it in the store's life: compaction re-walks the root its open just
/// walked, and the objects the two share sit in packs the first walk drained.
/// A walk releases what it left resident, including a pack holding an object
/// the walked root does not name, which would otherwise never drain.
#[test]
fn every_root_walk_reads_each_pack_whole_once_and_releases_it() {
    use crate::writer::BlockWriter;
    use fgdb_delta_types::{DeltaRow, RelationId};
    use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};
    let dir = scratch_dir("rewalk");
    under_lab(0x9a11, {
        let dir = dir.clone();
        move |cx| async move {
            let keys = (&K_OID, NAMESPACE);
            let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
            for vid in 1..=80u128 {
                let row = DeltaRow::CreateVertex {
                    vid: VId(vid),
                    birth_ordinal: vid as u64,
                    labels: vec![],
                    props: vec![],
                    valid_time: None,
                };
                writer
                    .apply(keys, CommitSeq(1), &row)
                    .expect("creates a vertex");
            }
            for src in 1..80u128 {
                let row = DeltaRow::CreateEdge {
                    eid: EId(src),
                    birth_ordinal: 100 + src as u64,
                    src: VId(src),
                    relation: RelationId(1),
                    dst: VId(src + 1),
                    canonical_key: None,
                    props: vec![],
                    valid_time: None,
                };
                writer
                    .apply(keys, CommitSeq(2), &row)
                    .expect("creates an edge");
            }
            let (root, blocks, patches) = writer.publish(keys, CommitSeq(2)).expect("publishes");
            assert!(blocks.len() + patches.len() >= PACK_MIN_OBJECTS);
            let store = open(&cx, &dir).await;
            let mut receipts = PublishReceipts::new();
            let mut batch = store
                .publication_batch(&cx, &mut receipts, None)
                .expect("batch");
            for block in &blocks {
                batch
                    .put_object(&cx, StoredObjectKind::Block, block.block_id, &block.bytes)
                    .await
                    .expect("stages a block");
            }
            for patch in &patches {
                batch
                    .put_object(
                        &cx,
                        StoredObjectKind::VertexPatch,
                        patch.patch_id,
                        &patch.bytes,
                    )
                    .await
                    .expect("stages a patch");
            }
            // An object this root does not name, as a superseded block is.
            let (orphan, orphan_bytes) = objects(12, 1).remove(0);
            batch
                .put_object(&cx, StoredObjectKind::Block, orphan, &orphan_bytes)
                .await
                .expect("stages the orphan");
            batch.finish(&cx).await.expect("publishes the pack");
            let root_id = store.put_root(&cx, &root).await.expect("stores the root");
            assert_eq!(packs_in(&dir).len(), 1);

            let store = open(&cx, &dir).await;
            for walk in 1..=3 {
                store.reopen(&cx, root_id).await.expect("reopens");
                assert_eq!(
                    store.packs.whole_reads(),
                    walk,
                    "walk {walk} reads the pack whole exactly once"
                );
                assert_eq!(
                    store.packs.resident_packs(),
                    0,
                    "walk {walk} leaves nothing resident"
                );
            }
        }
    });
}

/// Within a root walk a pack is spent once it has served every object the
/// walked root names in it: an object the root no longer names (a superseded
/// block left behind by compaction) does not keep the pack resident against
/// the next one. Outside a walk every object counts.
#[test]
fn a_walk_spends_a_pack_once_it_has_served_what_the_root_names_there() {
    let dir = scratch_dir("named");
    let first = objects(16, PACK_MIN_OBJECTS + 5);
    let second = objects(17, PACK_MIN_OBJECTS + 5);
    under_lab(0x9a0c, {
        let dir = dir.clone();
        move |cx| async move {
            let store = open(&cx, &dir).await;
            publish(&cx, &store, &first, None).await.expect("publish");
            publish(&cx, &store, &second, None).await.expect("publish");
            // The first pack's last object is the one no root names.
            let (orphan, named) = first.split_last().unwrap();
            for walk in [false, true] {
                let store = open(&cx, &dir).await;
                if walk {
                    store.packs.rearm(
                        named
                            .iter()
                            .chain(&second)
                            .map(|(id, _)| *id)
                            .collect::<Vec<_>>(),
                    );
                }
                assert!(read_all(&cx, &store, named).await.iter().all(Result::is_ok));
                store.packs.spend_resident_budget();
                assert!(read_all(&cx, &store, &second[..1]).await[0].is_ok());
                assert_eq!(
                    store.packs.whole_reads(),
                    if walk { 2 } else { 1 },
                    "walk={walk}: the second pack is loaded whole only once the first is spent"
                );
                assert_eq!(store.packs.resident_packs(), 1, "walk={walk}");
                // The unnamed object is still served, from wherever it is.
                let read = read_all(&cx, &store, std::slice::from_ref(orphan)).await;
                assert_eq!(read[0].as_ref().unwrap(), &orphan.1);
            }
        }
    });
}
