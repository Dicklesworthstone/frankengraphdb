//! fgdb-eslqh: a database whose root slot names a manifest in a format this
//! build does not read (a newer binary wrote it) is refused by name, by every
//! open path, with nothing written.

use super::*;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5c; 32],
        DatabaseSecurityNamespaceId([0x79; 32]),
        [0x3e; 32],
    )
}

/// Every file under `dir` with its bytes, so any write shows.
async fn files(vfs: &MemVfs, dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = vfs.read_dir(&directory).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            if entry.file_type().await.unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                let bytes = vfs.read(&entry.path()).await.unwrap();
                out.push((entry.path(), bytes));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn a_manifest_in_a_format_this_build_does_not_read_is_refused_by_name() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    let vfs = MemVfs::new().unwrap();
    let dir = vfs.database_dir();
    let mut db = runtime
        .block_on(Database::create_with_vfs(&cx, vfs.clone(), &dir, keys()))
        .unwrap();
    for vid in 1..=3 {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(vid), vec![], vec![]);
        runtime.block_on(db.write(&cx, batch)).unwrap();
    }
    drop(db);

    // The root slot names the same record in a format no build reads.
    let newer = runtime.block_on(async {
        let slots = RootStore::with_vfs(vfs.clone(), &dir);
        let Ok(RootSelection::Selected { slot, .. } | RootSelection::IdenticalPair { slot }) =
            slots.recover(&cx).await
        else {
            panic!("a healthy database has a selected root slot");
        };
        let store = open_block_store(&cx, &vfs, &dir, &keys()).await.unwrap();
        let current = ManifestVersion(ObjectId(slot.root_manifest_oid));
        let mut bytes = store.get_manifest_bytes(&cx, current).await.unwrap();
        bytes[4..6].copy_from_slice(&u16::MAX.to_be_bytes());
        let newer = ManifestVersion(fgdb_strata::manifest::manifest_id(
            keys().k_oid(),
            keys().namespace,
            &bytes,
        ));
        vfs.write(&store.path(newer.0), &bytes).await.unwrap();
        let generation = next_slot_generation(slot.slot_generation).unwrap();
        slots
            .publish_evidenced(
                &cx,
                &spine_slot(&keys(), generation, newer, bytes.len() as u64),
            )
            .await
            .unwrap();
        newer
    });
    let before = runtime.block_on(files(&vfs, &dir));

    let refused = |error: OpenError, open: &str| match error {
        OpenError::UnsupportedManifestFormat {
            slot_manifest,
            format,
            ..
        } => {
            assert_eq!((slot_manifest, format), (newer.0, u16::MAX), "{open}");
        }
        other => panic!("{open}: expected the named refusal, got {other:?}"),
    };
    let Err(writable) = runtime.block_on(Database::open_with_vfs(&cx, vfs.clone(), &dir, keys()))
    else {
        panic!("the writable open refuses");
    };
    refused(writable, "writable");
    let Err(read) = runtime.block_on(Database::open_read_view_with_vfs(
        &cx,
        vfs.clone(),
        &dir,
        keys(),
    )) else {
        panic!("the read view refuses");
    };
    refused(read, "read view");
    assert_eq!(
        runtime.block_on(files(&vfs, &dir)),
        before,
        "a refused open writes nothing"
    );
}
