//! The lab VFS fault model (plan §15, bead fgdb-1xtp).
//!
//! Every fault test here is paired with a control, because a fault harness that
//! is only ever exercised with faults on cannot tell "the fault fired" from
//! "the harness is broken". The controls are:
//!
//! * `a_faultless_plan_injects_nothing` — the whole model with every trigger
//!   off must be byte-transparent;
//! * `a_torn_write_needs_an_interior_sector_to_lose` — the eligibility rule
//!   itself, so `Trigger::Always` firing zero times is a *checked* outcome
//!   rather than a silent one;
//! * `a_different_seed_injects_a_different_schedule` — without it, the
//!   determinism test would pass just as well against a generator that always
//!   returned the same answer.

use asupersync::fs::{OpenOptions, Vfs};
use asupersync::io::AsyncWrite;
use asupersync::runtime::{Runtime, RuntimeBuilder};
use fgdb_sim::vfs::{DEFAULT_SECTOR_BYTES, FaultKind, FaultPlan, FaultVfs, Trigger};
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::pin::Pin;

const SECTOR: usize = DEFAULT_SECTOR_BYTES as usize;

fn runtime() -> Runtime {
    RuntimeBuilder::new().build().expect("lab runtime builds")
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-lab-vfs-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A distinct non-zero pattern per sector, so a hole reads as zeros and is
/// distinguishable from "the right bytes landed".
fn sector_pattern(sectors: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(sectors * SECTOR);
    for sector in 0..sectors {
        bytes.extend(std::iter::repeat_n(
            u8::try_from(sector + 1).unwrap_or(0xff),
            SECTOR,
        ));
    }
    bytes
}

/// The lost byte range of a torn write, or `None` for any other fault.
///
/// This and [`flip_site`] exist so a test can assert a fault's SHAPE with
/// `assert!(matches!(..))` — the 8c53adb precedent, and a real assertion with a
/// real message — and then read the fields out of a *total* function. The
/// obvious spelling, `let FaultKind::TornWrite { .. } = k else { panic!(..) }`,
/// puts a panic-class token in the file for what is only a destructure; UBS
/// counts those as critical and the ratchet is at zero. Splitting the check
/// from the extraction keeps both properties.
fn torn_range(kind: FaultKind) -> Option<(u64, u64)> {
    match kind {
        FaultKind::TornWrite { start, end } => Some((start, end)),
        _ => None,
    }
}

/// The damaged offset and bit of a bit flip, or `None` for any other fault.
fn flip_site(kind: FaultKind) -> Option<(u64, u8)> {
    match kind {
        FaultKind::BitFlip { offset, bit } => Some((offset, bit)),
        _ => None,
    }
}

/// Writes `bytes` at offset 0 through a handle and syncs, returning the sync's
/// result so a test can assert on a refused flush.
async fn write_and_sync<V: Vfs>(
    vfs: &FaultVfs<V>,
    path: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut file = vfs
        .open(
            path,
            &OpenOptions::new().write(true).create(true).truncate(true),
        )
        .await?;
    let mut written = 0usize;
    while written < bytes.len() {
        let n = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &bytes[written..])).await?;
        assert!(n > 0, "a lab write must make progress");
        written += n;
    }
    asupersync::fs::VfsFile::sync_all(&file).await
}

// ---------------------------------------------------------------------------
// Control
// ---------------------------------------------------------------------------

#[test]
fn a_faultless_plan_injects_nothing() {
    let dir = scratch_dir("faultless");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan::faultless());

    runtime().block_on(async {
        let bytes = sector_pattern(4);
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(durable, bytes, "a faultless plan must be byte-transparent");
    });

    assert!(
        vfs.events().is_empty(),
        "a faultless plan injected: {:?}",
        vfs.events()
    );
}

/// `FaultVfs::unix` must keep the construction `Cx` so fault-point traces
/// survive the documented ambient-`Cx` hole inside polled futures (fgdb-yevb).
#[test]
fn unix_constructed_under_lab_retains_a_trace_context() {
    let retained = run_and_expect_lab_green(0x7e7b, |_| async move {
        FaultVfs::unix(FaultPlan::faultless()).retains_trace_context()
    });
    assert!(
        retained,
        "construction under a lab task must capture Cx for later poll-path traces"
    );
}

#[test]
fn unix_constructed_outside_lab_has_no_trace_context() {
    let vfs = FaultVfs::unix(FaultPlan::faultless());
    assert!(
        !vfs.retains_trace_context(),
        "outside a capability context there is no Cx to retain"
    );
}

// ---------------------------------------------------------------------------
// The fsync lie
// ---------------------------------------------------------------------------

#[test]
fn a_lying_sync_reports_success_and_persists_nothing() {
    let dir = scratch_dir("fsync-lie");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        fsync_lie: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let bytes = sector_pattern(2);
        // The caller is told the data is durable.
        write_and_sync(&vfs, &path, &bytes)
            .await
            .expect("the lie reports success");
        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&path).await.expect("read after crash");
        assert!(
            durable.is_empty(),
            "the lie persisted {} bytes; it must persist none",
            durable.len()
        );
    });

    let events = vfs.events();
    assert_eq!(events.len(), 1, "expected exactly one lie: {events:?}");
    assert_eq!(
        events[0].kind,
        FaultKind::FsyncLie {
            unflushed_bytes: (2 * SECTOR) as u64
        }
    );
    assert_eq!(events[0].path, path);
    assert_eq!(
        vfs.flushed_bytes(),
        0,
        "a lie must not count against the space budget"
    );
}

#[test]
fn a_later_honest_sync_writes_what_the_lie_left_dirty() {
    let dir = scratch_dir("lie-then-honest");
    let path = dir.join("log");
    // Fires on every 2nd eligible sync: #1 is honest, #2 lies, #3 is honest.
    let vfs = FaultVfs::unix(FaultPlan {
        fsync_lie: Trigger::Nth(2),
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let mut file = vfs
            .open(&path, &OpenOptions::new().write(true).create(true))
            .await
            .expect("open");

        let first = vec![0xaa; SECTOR];
        poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &first))
            .await
            .expect("write 1");
        asupersync::fs::VfsFile::sync_all(&file)
            .await
            .expect("honest sync");

        let second = vec![0xbb; SECTOR];
        poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &second))
            .await
            .expect("write 2");
        asupersync::fs::VfsFile::sync_all(&file)
            .await
            .expect("lying sync reports success");
        assert_eq!(
            file.dirty_sectors().expect("handle alive"),
            vec![1],
            "the lie must leave the bytes dirty, exactly as a write-back cache does"
        );

        // The third sync is honest again and must write what the lie left.
        asupersync::fs::VfsFile::sync_all(&file)
            .await
            .expect("honest sync");
        vfs.crash().await.expect("crash rollback");

        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(durable.len(), 2 * SECTOR);
        assert_eq!(&durable[..SECTOR], &first[..]);
        assert_eq!(
            &durable[SECTOR..],
            &second[..],
            "the honest sync after the lie must persist the still-dirty bytes"
        );
    });

    let events = vfs.events();
    assert_eq!(events.len(), 1, "expected exactly one lie: {events:?}");
    assert!(matches!(events[0].kind, FaultKind::FsyncLie { .. }));
}

/// `Trigger::Nth(0)` is a degenerate schedule with no natural reading — "every
/// zeroth operation" is not a thing — so the code decides it never fires, and
/// this witnesses that decision.
///
/// It is not a formality. Under the `count % n == 0` spelling the explicit
/// `Nth(0)` arm was the only thing standing between this trigger and a
/// divide-by-zero panic; under `count.is_multiple_of(n)` the arm is redundant
/// because `is_multiple_of(0)` is `count == 0`. Without a test, that behaviour
/// silently changed owner from our code to the standard library's definition,
/// and nothing would have failed if a later edit dropped the arm and the
/// standard library's answer had been different.
#[test]
fn nth_zero_never_fires() {
    let dir = scratch_dir("nth-zero");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        fsync_lie: Trigger::Nth(0),
        write_enospc: Trigger::Nth(0),
        torn_write: Trigger::Nth(0),
        bit_flip: Trigger::Nth(0),
        ..FaultPlan::faultless()
    });

    // Five sectors, so the torn write is ELIGIBLE (it needs an interior
    // sector) and its trigger is genuinely consulted rather than skipped.
    let bytes = sector_pattern(5);
    runtime().block_on(async {
        write_and_sync(&vfs, &path, &bytes)
            .await
            .expect("an Nth(0) plan must not lie, tear, or fail the sync");
        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(
            durable, bytes,
            "Nth(0) injected something; it must be inert on every class"
        );
    });

    assert!(vfs.events().is_empty(), "Nth(0) fired: {:?}", vfs.events());
}

// ---------------------------------------------------------------------------
// Torn writes
// ---------------------------------------------------------------------------

#[test]
fn a_torn_write_loses_an_interior_sector_and_keeps_the_bytes_after_it() {
    let dir = scratch_dir("torn");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        seed: seed_1xtp(),
        torn_write: Trigger::Always,
        ..FaultPlan::faultless()
    });

    let bytes = sector_pattern(5);
    runtime().block_on(async {
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        vfs.crash().await.expect("crash rollback");

        let events = vfs.events();
        assert_eq!(events.len(), 1, "expected exactly one tear: {events:?}");
        assert!(
            matches!(events[0].kind, FaultKind::TornWrite { .. }),
            "expected a torn write, got {:?}",
            events[0].kind
        );
        let (start, end) = torn_range(events[0].kind).expect("shape asserted above");
        assert_eq!(end - start, DEFAULT_SECTOR_BYTES);
        assert!(
            start >= DEFAULT_SECTOR_BYTES && end <= 4 * DEFAULT_SECTOR_BYTES,
            "the lost sector must be interior, got [{start}, {end})"
        );

        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(
            durable.len(),
            bytes.len(),
            "sectors after the hole landed, so the file keeps its length"
        );
        let hole = &durable[start as usize..end as usize];
        assert!(
            hole.iter().all(|&b| b == 0),
            "the torn sector must be missing bytes, not stale ones"
        );
        // THE DISCRIMINATION a truncating tear cannot produce: valid bytes on
        // BOTH sides of the hole.
        assert_eq!(
            &durable[..SECTOR],
            &bytes[..SECTOR],
            "the first sector landed"
        );
        assert_eq!(
            &durable[4 * SECTOR..],
            &bytes[4 * SECTOR..],
            "the sector AFTER the hole landed — this is what a truncating tear cannot model"
        );
    });
}

#[test]
fn a_torn_write_needs_an_interior_sector_to_lose() {
    let dir = scratch_dir("torn-ineligible");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        torn_write: Trigger::Always,
        ..FaultPlan::faultless()
    });

    // Two dirty sectors have no interior: dropping either is a truncation, not
    // a tear, so the class is ineligible and must inject nothing.
    let bytes = sector_pattern(2);
    runtime().block_on(async {
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(durable, bytes);
    });

    assert!(
        vfs.events().is_empty(),
        "an ineligible tear injected anyway: {:?}",
        vfs.events()
    );
}

// ---------------------------------------------------------------------------
// Bit flips
// ---------------------------------------------------------------------------

#[test]
fn a_bit_flip_damages_exactly_one_bit_of_durable_data() {
    let dir = scratch_dir("bit-flip");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        seed: 7,
        bit_flip: Trigger::Always,
        ..FaultPlan::faultless()
    });

    let bytes = sector_pattern(2);
    runtime().block_on(async {
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        vfs.crash().await.expect("crash rollback");

        let events = vfs.events();
        assert_eq!(events.len(), 1, "expected exactly one flip: {events:?}");
        assert!(
            matches!(events[0].kind, FaultKind::BitFlip { .. }),
            "expected a bit flip, got {:?}",
            events[0].kind
        );
        let (offset, bit) = flip_site(events[0].kind).expect("shape asserted above");
        assert!(bit < 8);

        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(durable.len(), bytes.len());
        let differing: Vec<usize> = (0..bytes.len())
            .filter(|&i| durable[i] != bytes[i])
            .collect();
        assert_eq!(
            differing,
            vec![offset as usize],
            "exactly the reported byte must differ"
        );
        assert_eq!(
            durable[offset as usize] ^ bytes[offset as usize],
            1u8 << bit,
            "exactly the reported bit must differ"
        );
    });
}

// ---------------------------------------------------------------------------
// ENOSPC
// ---------------------------------------------------------------------------

#[test]
fn write_enospc_accepts_no_bytes_and_an_exact_retry_can_progress() {
    let dir = scratch_dir("write-enospc");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        write_enospc: Trigger::At(1),
        ..FaultPlan::faultless()
    });

    let bytes = sector_pattern(2);
    runtime().block_on(async {
        let mut file = vfs
            .open(&path, &OpenOptions::new().write(true).create(true))
            .await
            .expect("open");
        let error = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &bytes))
            .await
            .expect_err("the selected write must be refused");
        assert_eq!(
            error.raw_os_error(),
            Some(28),
            "a refused write must surface as ENOSPC"
        );
        assert!(
            file.image().expect("handle alive").is_empty(),
            "the refused write must accept no volatile bytes"
        );
        assert!(
            file.dirty_sectors().expect("handle alive").is_empty(),
            "the refused write must dirty no sector"
        );

        let accepted = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &bytes))
            .await
            .expect("the one-shot fault is exhausted");
        assert_eq!(accepted, bytes.len());
        asupersync::fs::VfsFile::sync_all(&file)
            .await
            .expect("retry syncs");
        vfs.crash().await.expect("crash rollback");
        assert_eq!(
            vfs.read(&path).await.expect("read after crash"),
            bytes,
            "the exact retry must persist the complete request"
        );
    });

    let events = vfs.events();
    assert_eq!(events.len(), 1, "expected one write refusal: {events:?}");
    assert_eq!(
        events[0].kind,
        FaultKind::WriteEnospc {
            requested: (2 * SECTOR) as u64,
        }
    );
    assert_eq!(events[0].kind.class(), "write-enospc");
    assert_eq!(events[0].path, path);
}

#[test]
fn out_of_space_refuses_the_flush_and_leaves_the_bytes_dirty() {
    let dir = scratch_dir("enospc");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        space_budget: Some(DEFAULT_SECTOR_BYTES),
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let mut file = vfs
            .open(&path, &OpenOptions::new().write(true).create(true))
            .await
            .expect("open");
        let bytes = sector_pattern(2);
        poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &bytes))
            .await
            .expect("write");

        let error = asupersync::fs::VfsFile::sync_all(&file)
            .await
            .expect_err("a flush past the budget must fail");
        assert_eq!(
            error.raw_os_error(),
            Some(28),
            "a full filesystem must surface as ENOSPC, not an opaque error"
        );
        assert_eq!(
            file.dirty_sectors().expect("handle alive"),
            vec![0, 1],
            "a refused flush must leave every byte dirty"
        );

        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&path).await.expect("read after crash");
        assert!(
            durable.is_empty(),
            "nothing reached the backing store, so nothing survives"
        );
    });

    let events = vfs.events();
    assert_eq!(events.len(), 1, "expected one ENOSPC: {events:?}");
    assert_eq!(
        events[0].kind,
        FaultKind::OutOfSpace {
            requested: (2 * SECTOR) as u64,
            remaining: DEFAULT_SECTOR_BYTES,
        }
    );
}

// ---------------------------------------------------------------------------
// Crash semantics
// ---------------------------------------------------------------------------

#[test]
fn a_handle_does_not_survive_a_crash() {
    let dir = scratch_dir("crash-handle");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan::faultless());

    runtime().block_on(async {
        let mut file = vfs
            .open(&path, &OpenOptions::new().write(true).create(true))
            .await
            .expect("open");
        poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &[1, 2, 3]))
            .await
            .expect("write");

        assert_eq!(vfs.generation(), 0);
        vfs.crash().await.expect("crash rollback");
        assert_eq!(vfs.generation(), 1);

        // The pre-crash handle is gone: its unsynced bytes cannot leak across
        // the crash through a caller that kept the file open.
        poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &[4]))
            .await
            .expect_err("a pre-crash handle must refuse writes");
        asupersync::fs::VfsFile::sync_all(&file)
            .await
            .expect_err("a pre-crash handle must refuse syncs");
        file.image()
            .expect_err("a pre-crash handle exposes nothing");

        // A fresh open works and sees only durable bytes.
        let durable = vfs.read(&path).await.expect("read after crash");
        assert!(durable.is_empty());
        let reopened = vfs
            .open(&path, &OpenOptions::new().write(true))
            .await
            .expect("reopen after crash");
        assert!(reopened.image().expect("fresh handle").is_empty());
    });
}

// ---------------------------------------------------------------------------
// Determinism — and its control
// ---------------------------------------------------------------------------

/// Sixteen write+sync rounds under a coin-flip lie schedule.
fn coin_flip_schedule(seed: u64, name: &str) -> Vec<FaultKind> {
    let dir = scratch_dir(name);
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        seed,
        fsync_lie: Trigger::PerMille(500),
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let mut file = vfs
            .open(
                &path,
                &OpenOptions::new().write(true).create(true).truncate(true),
            )
            .await
            .expect("open");
        for round in 0..16u8 {
            poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &[round; 8]))
                .await
                .expect("write");
            asupersync::fs::VfsFile::sync_all(&file)
                .await
                .expect("sync");
        }
    });

    vfs.events().into_iter().map(|event| event.kind).collect()
}

#[test]
fn the_same_seed_injects_the_same_schedule() {
    let first = coin_flip_schedule(seed_1xtp(), "determinism-a");
    let second = coin_flip_schedule(seed_1xtp(), "determinism-b");
    assert!(
        !first.is_empty(),
        "a coin-flip schedule that injected nothing cannot witness determinism"
    );
    assert_eq!(
        first, second,
        "the same seed must inject the same faults — this is the replay claim"
    );
}

#[test]
fn a_different_seed_injects_a_different_schedule() {
    let first = coin_flip_schedule(seed_1xtp(), "divergence-a");
    let second = coin_flip_schedule(seed_1xtp() ^ 0xffff, "divergence-b");
    assert_ne!(
        first, second,
        "two seeds produced identical schedules; the seed is not driving the stream"
    );
}

/// A fixed, arbitrary seed. Named so a reader knows the constant is a choice
/// and not a magic number derived from anything.
const fn seed_1xtp() -> u64 {
    0x1774_7000_0000_0001
}

// ---------------------------------------------------------------------------
// Dirent durability (fgdb-3a3u)
// ---------------------------------------------------------------------------

/// The pending dirent count of a lie, or `None` for any other fault. Same
/// destructure-without-a-panic-token shape as [`torn_range`].
fn dirent_lie_pending(kind: FaultKind) -> Option<u64> {
    match kind {
        FaultKind::DirentSyncLie { pending_ops } => Some(pending_ops),
        _ => None,
    }
}

/// Opens `dir` and syncs it — the dirent barrier as chronicle spells it.
async fn sync_dir<V: Vfs>(vfs: &FaultVfs<V>, dir: &Path) -> std::io::Result<()> {
    let handle = vfs.open(dir, &OpenOptions::new().read(true)).await?;
    asupersync::fs::VfsFile::sync_all(&handle).await
}

#[test]
fn an_unsynced_dirent_loses_a_synced_files_name_across_a_crash() {
    let dir = scratch_dir("dirent-unsynced");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_loss: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let bytes = sector_pattern(2);
        // The file's CONTENTS are honestly durable...
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        let visible = vfs.read(&path).await.expect("pre-crash read");
        assert_eq!(visible, bytes, "the synced contents are on the platter");
        // ...but the parent directory was never synced, so the NAME is not.
        vfs.crash().await.expect("crash rollback");
        let error = vfs
            .read(&path)
            .await
            .expect_err("the name must not survive the crash");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "fsync-the-file-forget-the-directory must lose the file"
        );
    });
    let events = vfs.events();
    assert_eq!(events.len(), 1, "exactly the loss: {events:?}");
    assert_eq!(events[0].path, path, "the event names the vanished file");
    assert_eq!(events[0].kind, FaultKind::DirentLoss { op: "created" });
    assert_eq!(events[0].kind.class(), "dirent-loss");
}

#[test]
fn an_honest_directory_sync_makes_the_name_immune_to_loss() {
    let dir = scratch_dir("dirent-honest");
    let path = dir.join("log");
    // Loss on every pending operation — but a settled operation is not
    // pending, which is exactly what this control witnesses.
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_loss: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let bytes = sector_pattern(2);
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        assert_eq!(vfs.pending_dirent_ops(), 1, "the create owes the parent");
        sync_dir(&vfs, &dir).await.expect("dir sync");
        assert_eq!(vfs.pending_dirent_ops(), 0, "the honest sync settles it");
        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(durable, bytes, "file sync + dir sync must survive a crash");
    });
    assert!(
        vfs.events().is_empty(),
        "a settled name cannot be lost: {:?}",
        vfs.events()
    );
}

#[test]
fn a_created_directory_owes_its_parent_even_when_its_child_is_durable() {
    let parent = scratch_dir("directory-dirent");
    let first = parent.join("first");
    let leaf = first.join("leaf");
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_loss: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        vfs.create_dir_all(&leaf).await.expect("nested create");
        assert_eq!(
            vfs.pending_dirent_ops(),
            2,
            "each newly created directory name owes its own parent"
        );

        sync_dir(&vfs, &first)
            .await
            .expect("make the leaf name durable");
        assert_eq!(
            vfs.pending_dirent_ops(),
            1,
            "syncing first settles leaf, not first's name in parent"
        );

        vfs.crash().await.expect("crash rollback");
        let error = vfs
            .symlink_metadata(&first)
            .await
            .expect_err("the unsynced ancestor name must disappear");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    });

    let events = vfs.events();
    assert_eq!(events.len(), 1, "only the unsettled ancestor is lost");
    assert_eq!(events[0].path, first);
    assert_eq!(events[0].kind, FaultKind::DirentLoss { op: "created" });
}

#[test]
fn a_lying_directory_sync_settles_nothing() {
    let dir = scratch_dir("dirent-lie");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_lie: Trigger::Always,
        dirent_loss: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let bytes = sector_pattern(2);
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        // The caller is told the namespace is durable.
        sync_dir(&vfs, &dir).await.expect("the lie reports success");
        assert_eq!(
            vfs.pending_dirent_ops(),
            1,
            "the lie must settle nothing, exactly as an fsync lie leaves \
             sectors dirty"
        );
        vfs.crash().await.expect("crash rollback");
        let error = vfs
            .read(&path)
            .await
            .expect_err("the lied-about name must not survive");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    });

    let events = vfs.events();
    assert_eq!(events.len(), 2, "the lie, then the loss: {events:?}");
    assert_eq!(
        events[0].path, dir,
        "the lie must name the DIRECTORY, not the file"
    );
    assert_eq!(
        dirent_lie_pending(events[0].kind),
        Some(1),
        "one pending operation was lied about: {events:?}"
    );
    assert_eq!(events[0].kind.class(), "dirent-sync-lie");
    assert_eq!(events[1].kind, FaultKind::DirentLoss { op: "created" });
}

#[test]
fn a_directory_sync_with_nothing_pending_consumes_no_trigger() {
    let dir = scratch_dir("dirent-noop");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_lie: Trigger::Nth(1),
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        // Nothing is pending: this sync must be an honest no-op that does
        // NOT advance the eligibility counter.
        sync_dir(&vfs, &dir).await.expect("no-op dir sync");
        assert!(
            vfs.events().is_empty(),
            "a no-op dir sync consumed a trigger: {:?}",
            vfs.events()
        );
        // The first ELIGIBLE sync is the one Nth(1) fires on.
        write_and_sync(&vfs, &path, &sector_pattern(1))
            .await
            .expect("sync");
        sync_dir(&vfs, &dir).await.expect("dir sync");
        assert_eq!(
            vfs.events().len(),
            1,
            "Nth(1) must fire on the first eligible sync: {:?}",
            vfs.events()
        );
    });
}

#[test]
fn an_unsynced_rename_reverts_and_restores_the_clobbered_destination() {
    let dir = scratch_dir("dirent-rename");
    let source = dir.join("manifest.tmp");
    let target = dir.join("manifest.root");
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_loss: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let old = sector_pattern(1);
        let new = sector_pattern(2);
        // Both names durable first.
        write_and_sync(&vfs, &target, &old).await.expect("sync old");
        write_and_sync(&vfs, &source, &new).await.expect("sync new");
        sync_dir(&vfs, &dir).await.expect("dir sync");
        // The rename-over is applied but its dirents never synced.
        vfs.rename(&source, &target).await.expect("rename");
        let visible = vfs.read(&target).await.expect("pre-crash read");
        assert_eq!(visible, new, "pre-crash readers see the new namespace");
        vfs.crash().await.expect("crash rollback");
        let reverted = vfs.read(&target).await.expect("target after crash");
        assert_eq!(reverted, old, "the clobbered destination must reappear");
        let restored = vfs.read(&source).await.expect("source after crash");
        assert_eq!(
            restored, new,
            "the renamed file must be back at its old name"
        );
    });
}

#[test]
fn an_unsynced_removal_restores_the_durable_bytes() {
    let dir = scratch_dir("dirent-remove");
    let path = dir.join("log");
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_loss: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let bytes = sector_pattern(2);
        write_and_sync(&vfs, &path, &bytes).await.expect("sync");
        sync_dir(&vfs, &dir).await.expect("dir sync");
        vfs.remove_file(&path).await.expect("remove");
        let error = vfs.read(&path).await.expect_err("pre-crash unlink shows");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&path).await.expect("read after crash");
        assert_eq!(durable, bytes, "an unsettled removal never happened");
    });
}

#[test]
fn a_synced_rename_and_removal_stay_applied_across_a_crash() {
    let dir = scratch_dir("dirent-settled");
    let source = dir.join("a");
    let target = dir.join("b");
    let removed = dir.join("c");
    // Loss armed on every pending operation: only settlement protects.
    let vfs = FaultVfs::unix(FaultPlan {
        dirent_loss: Trigger::Always,
        ..FaultPlan::faultless()
    });

    runtime().block_on(async {
        let bytes = sector_pattern(1);
        write_and_sync(&vfs, &source, &bytes).await.expect("sync a");
        write_and_sync(&vfs, &removed, &bytes)
            .await
            .expect("sync c");
        sync_dir(&vfs, &dir).await.expect("dir sync");
        vfs.rename(&source, &target).await.expect("rename");
        vfs.remove_file(&removed).await.expect("remove");
        sync_dir(&vfs, &dir).await.expect("settling dir sync");
        assert_eq!(vfs.pending_dirent_ops(), 0, "everything settled");
        vfs.crash().await.expect("crash rollback");
        let durable = vfs.read(&target).await.expect("target after crash");
        assert_eq!(durable, bytes, "a settled rename must survive");
        let gone = vfs.read(&source).await.expect_err("source stays gone");
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
        let also_gone = vfs.read(&removed).await.expect_err("removal stays");
        assert_eq!(also_gone.kind(), std::io::ErrorKind::NotFound);
    });
}

// ---------------------------------------------------------------------------
// Injectable latency (fgdb-milt)
// ---------------------------------------------------------------------------

use asupersync::lab::{AutoAdvanceTermination, LabConfig, LabRuntime};
use asupersync::types::Budget;

/// THE WITNESS THE BEAD EXISTS FOR: the delay is AWAITED, not recorded-and-
/// skipped. Under the lab runtime the virtual clock advances only when the
/// scheduler drives an awaited timer, so "elapsed virtual time equals the
/// injected delay" is exactly the property a placebo cannot fake — a latency
/// knob that returns without awaiting leaves the clock untouched and this
/// test red.
#[test]
fn an_injected_latency_is_awaited_in_virtual_time() {
    let dir = scratch_dir("latency-awaited");
    let path = dir.join("log");
    let (elapsed_nanos, events) = run_and_expect_lab_green(0x717a, move |root| async move {
        let vfs = FaultVfs::unix_with_clock(
            FaultPlan {
                latency: Trigger::Always,
                latency_micros: 2_500,
                ..FaultPlan::faultless()
            },
            root.clone(),
        );
        let before = root.now();
        write_and_sync(&vfs, &path, &sector_pattern(1))
            .await
            .expect("delayed sync still succeeds");
        let after = root.now();
        (after.as_nanos() - before.as_nanos(), vfs.events())
    });
    assert_eq!(
        elapsed_nanos, 2_500_000,
        "the sync must advance virtual time by exactly the injected delay; \
         zero means the delay was recorded but never awaited (the placebo)"
    );
    assert_eq!(events.len(), 1, "exactly the awaited delay: {events:?}");
    assert_eq!(events[0].kind, FaultKind::Latency { micros: 2_500 });
    assert_eq!(events[0].kind.class(), "latency");
}

/// The control: a plan without latency must be timing-transparent — same
/// workload, same clock, zero virtual time consumed.
#[test]
fn a_faultless_plan_is_timing_transparent() {
    let dir = scratch_dir("latency-control");
    let path = dir.join("log");
    let (elapsed_nanos, events) = run_and_expect_lab_green(0x717b, move |root| async move {
        let vfs = FaultVfs::unix_with_clock(FaultPlan::faultless(), root.clone());
        let before = root.now();
        write_and_sync(&vfs, &path, &sector_pattern(1))
            .await
            .expect("sync");
        let after = root.now();
        (after.as_nanos() - before.as_nanos(), vfs.events())
    });
    assert_eq!(
        elapsed_nanos, 0,
        "a faultless plan consumed virtual time; the harness is delaying \
         when it was not asked to"
    );
    assert!(events.is_empty(), "faultless plan injected: {events:?}");
}

/// A latency delay composes with a fault at the same sync: slow first, then
/// the lie — and the event order records exactly that.
#[test]
fn a_slow_sync_can_still_lie() {
    let dir = scratch_dir("latency-then-lie");
    let path = dir.join("log");
    let (elapsed_nanos, events, durable_len) =
        run_and_expect_lab_green(0x717c, move |root| async move {
            let vfs = FaultVfs::unix_with_clock(
                FaultPlan {
                    latency: Trigger::Always,
                    latency_micros: 1_000,
                    fsync_lie: Trigger::Always,
                    ..FaultPlan::faultless()
                },
                root.clone(),
            );
            let before = root.now();
            write_and_sync(&vfs, &path, &sector_pattern(1))
                .await
                .expect("slow lying sync still reports success");
            let after = root.now();
            let durable = vfs.read(&path).await.expect("durable read");
            (
                after.as_nanos() - before.as_nanos(),
                vfs.events(),
                durable.len(),
            )
        });
    assert_eq!(elapsed_nanos, 1_000_000, "the delay must still be awaited");
    assert_eq!(events.len(), 2, "the delay, then the lie: {events:?}");
    assert_eq!(events[0].kind, FaultKind::Latency { micros: 1_000 });
    assert!(matches!(events[1].kind, FaultKind::FsyncLie { .. }));
    assert_eq!(durable_len, 0, "the lie must still persist nothing");
}

/// The hard-error rule: a latency-enabled plan without a clock is refused at
/// construction, because degrading to no-delay is the placebo.
#[test]
#[should_panic(expected = "needs a clock")]
fn a_latency_enabled_plan_without_a_clock_is_refused() {
    let _ = FaultVfs::unix(FaultPlan {
        latency: Trigger::Always,
        latency_micros: 100,
        ..FaultPlan::faultless()
    });
}

/// Runs `test` under a lab runtime whose loop AUTO-ADVANCES virtual time to
/// the next timer deadline whenever the scheduler idles.
///
/// `run_async_under_lab` deliberately cannot be used here: its
/// `run_until_quiescent` loop never advances virtual time, so a task parked
/// on a timer spins to the step budget and reports a stall — measured before
/// this helper existed. Timer-bearing lab tests need the auto-advance loop,
/// and this is the public-API spelling of it.
fn run_and_expect_lab_green<T, Fut>(
    seed: u64,
    test: impl FnOnce(asupersync::Cx<asupersync::cx::cap::All>) -> Fut + Send + 'static,
) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut runtime = LabRuntime::new(LabConfig::new(seed).with_auto_advance());
    let root = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, mut handle) = runtime
        .state
        .create_task(root, Budget::INFINITE, async move {
            let cx = asupersync::Cx::current().expect("lab task runs with an ambient Cx");
            test(cx).await
        })
        .expect("lab task spawns");
    runtime
        .scheduler
        .lock()
        .schedule(task_id, Budget::INFINITE.priority);
    let report = runtime.run_with_auto_advance();
    assert!(
        matches!(report.termination, AutoAdvanceTermination::Quiescent),
        "lab run did not quiesce: {report:?}"
    );
    let lab_report = runtime.report();
    assert!(
        lab_report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {lab_report:?}"
    );
    handle
        .try_join()
        .expect("lab task joined")
        .expect("lab task finished")
}
