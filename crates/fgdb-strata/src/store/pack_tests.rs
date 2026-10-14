//! Store-level laws for commit packs (fgdb-79kyi): what a batch publishes,
//! what every reader finds, and what each crash instant leaves behind.

use super::pack::{self, PACK_DIR, PACK_MIN_OBJECTS};
use super::{
    BLOCK_DIR, BlockStore, BlockStoreCrashPoint, PublishReceipts, StoreError, StoredObjectKind,
};
use crate::{DeltaBlockVersion, block_id};
use asupersync::lab::run_async_under_lab;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use std::future::Future;
use std::path::{Path, PathBuf};

const K_OID: [u8; 32] = [0x6b; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x29; 32]);

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
            // and it stays resident until every object has been read once.
            let unbudgeted = open(&cx, &dir).await;
            let (last, rest) = objects.split_last().unwrap();
            assert!(
                read_all(&cx, &unbudgeted, rest)
                    .await
                    .iter()
                    .all(Result::is_ok)
            );
            assert_eq!(unbudgeted.packs.resident_packs(), 1);
            let final_read = read_all(&cx, &unbudgeted, std::slice::from_ref(last)).await;
            assert_eq!(final_read[0].as_ref().unwrap(), &last.1);
            assert_eq!(
                unbudgeted.packs.resident_packs(),
                0,
                "a drained pack is released"
            );
            // A drained pack still serves every object, one at a time.
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

/// A resident pack is released only once every object in it has been read.
/// Many blocks can name one small object (an identical property patch), so a
/// walk reads that object again and again; those reads must not release the
/// pack before the walk reaches its other objects.
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
            assert_eq!(
                store.packs.resident_packs(),
                0,
                "released once every object was read"
            );
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
                read_all(&cx, &before, &second[1..]).await,
            )
        }
    });
    assert_eq!(reread.0.expect("intact copy"), first[7].1);
    // The store opened before the second pack finds its objects on a miss.
    assert!(reread.1.into_iter().all(|read| read.is_ok()));
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
    let (read, others) = under_lab(0x9a08, {
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
            )
        }
    });
    assert!(others.into_iter().all(|read| read.is_ok()));
    assert_eq!(read.expect("intact older copy"), first[3].1);
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
