//! FaultVfs shares one inode per canonical path (fgdb-nsmf): `open` keys the
//! inode table by `canonicalize(path)`. A rename or removal named through a
//! different spelling of the same directory (a symlinked parent, or macOS's
//! `/var` for `/private/var`) must move or drop that same inode. Otherwise a
//! later create of the old name reuses the stale inode, its writes and syncs
//! land in the renamed file, and the new name stays empty (fgdb-8h2ml: every
//! batch-published staging inode on macOS).

use asupersync::fs::{OpenOptions, Vfs, VfsFile};
use asupersync::io::AsyncWriteExt;
use asupersync::lab::run_async_under_lab;
use fgdb_sim::vfs::{FaultPlan, FaultVfs};
use std::path::{Path, PathBuf};

/// A real directory and a symlink to it, so the two spellings of every path
/// below differ on every platform.
fn spellings(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "fgdb-fault-vfs-names-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).expect("scratch dir");
    let base = base.canonicalize().expect("canonical scratch dir");
    let real = base.join("real");
    std::fs::create_dir_all(&real).expect("real dir");
    let link = base.join("link");
    if std::fs::symlink_metadata(&link).is_err() {
        std::os::unix::fs::symlink(&real, &link).expect("symlinked parent");
    }
    (real, link)
}

async fn create_synced(vfs: &FaultVfs, path: &Path, bytes: &[u8]) {
    let create = OpenOptions::new().read(true).write(true).create_new(true);
    let mut file = vfs.open(path, &create).await.expect("creates the name");
    file.write_all(bytes).await.expect("writes");
    file.sync_all().await.expect("syncs");
}

#[test]
fn a_name_reused_after_rename_or_removal_through_a_symlinked_parent_gets_its_own_inode() {
    let (real, link) = spellings("reuse");
    let ((), report) = run_async_under_lab(0x8_2b1, move |_root| async move {
        let vfs = FaultVfs::unix(FaultPlan::faultless());
        let (staging, canonical) = (link.join("staging"), link.join("canonical"));

        create_synced(&vfs, &staging, b"first object").await;
        vfs.rename(&staging, &canonical).await.expect("renames");
        create_synced(&vfs, &staging, b"second").await;
        assert_eq!(vfs.read(&staging).await.unwrap(), b"second");
        assert_eq!(vfs.read(&canonical).await.unwrap(), b"first object");
        // The same bytes through the canonical spelling: one file, one inode.
        assert_eq!(vfs.read(&real.join("staging")).await.unwrap(), b"second");

        vfs.remove_file(&staging).await.expect("removes");
        create_synced(&vfs, &staging, b"third!").await;
        assert_eq!(vfs.read(&staging).await.unwrap(), b"third!");
        assert_eq!(vfs.read(&canonical).await.unwrap(), b"first object");
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
}
