//! fgdb-79kyi incident (2026-10-14): a pack-era fgdbd open of a database written
//! before manifest V3 republished its root slot to name a re-encoded manifest
//! that was never stored, and every binary then refused the database. These
//! laws open a database in exactly the state a pre-pack binary leaves it: the
//! root slot names a V2 manifest object at its content address.

use super::*;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5b; 32],
        DatabaseSecurityNamespaceId([0x78; 32]),
        [0x3d; 32],
    )
}

fn batch(at: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(100 + at), vec![], vec![]);
    batch.add_edge(EId(at), VId(1), VId(100 + at), vec![]);
    batch
}

/// Rewrite the database's current manifest as format V2, store it loose at its
/// own content address and point the root slot at it: the state a pre-pack
/// binary leaves. Returns that manifest's identity.
async fn as_written_before_v3(cx: &CommitCx, vfs: &MemVfs, dir: &Path) -> ManifestVersion {
    let slots = RootStore::with_vfs(vfs.clone(), dir);
    let Ok(RootSelection::Selected { slot, .. } | RootSelection::IdenticalPair { slot }) =
        slots.recover(cx).await
    else {
        panic!("a healthy database has a selected root slot");
    };
    let store = open_block_store(cx, vfs, dir, &keys()).await.unwrap();
    let current = ManifestVersion(ObjectId(slot.root_manifest_oid));
    let mut bytes = store.get_manifest_bytes(cx, current).await.unwrap();
    bytes[4..6].copy_from_slice(&fgdb_strata::manifest::MANIFEST_FORMAT_V2.to_be_bytes());
    let v2 = ManifestVersion(fgdb_strata::manifest::manifest_id(
        keys().k_oid(),
        keys().namespace,
        &bytes,
    ));
    assert_ne!(v2, current, "V2 and V3 encodings have distinct addresses");
    vfs.write(&store.path(v2.0), &bytes).await.unwrap();
    let generation = next_slot_generation(slot.slot_generation).unwrap();
    slots
        .publish_evidenced(cx, &spine_slot(&keys(), generation, v2, bytes.len() as u64))
        .await
        .unwrap();
    // A pre-pack binary never wrote a V3 manifest: drop this fixture's
    // in-memory copy, so a re-encoded V3 identity names an absent object, as
    // it did in the incident.
    vfs.remove_file(&store.path(current.0)).await.unwrap();
    v2
}

/// **OPENING A PRE-V3 DATABASE CHANGES NO SLOT AND LEAVES IT OPENABLE.** A
/// writable open, a read view and a second writable open each find the
/// database exactly as written: `manifest.root` stays byte-identical, the V2
/// manifest stays selected, and the graph reads back. The first commit then
/// writes a V3 manifest of its own, and the database reopens.
#[test]
fn opening_a_pre_v3_database_leaves_its_root_slot_byte_identical() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    let vfs = MemVfs::new().unwrap();
    let dir = vfs.database_dir();
    let mut db = runtime
        .block_on(Database::create_with_vfs(&cx, vfs.clone(), &dir, keys()))
        .unwrap();
    let mut origin = WriteBatch::new(RelationId(1));
    origin.create_vertex(VId(1), vec![], vec![]);
    runtime.block_on(db.write(&cx, origin)).unwrap();
    for at in 1..=5 {
        runtime.block_on(db.write(&cx, batch(at))).unwrap();
    }
    let vertices = db.vertices().unwrap().len();
    drop(db);

    let v2 = runtime.block_on(as_written_before_v3(&cx, &vfs, &dir));
    let slot_file = dir.join("manifest.root");
    let written = runtime.block_on(vfs.read(&slot_file)).unwrap();

    for open in ["writable", "read view", "writable again"] {
        let read = if open == "read view" {
            runtime
                .block_on(Database::open_read_view_with_vfs(
                    &cx,
                    vfs.clone(),
                    &dir,
                    keys(),
                ))
                .unwrap()
                .vertices()
                .unwrap()
                .len()
        } else {
            let db = runtime
                .block_on(Database::open_with_vfs(&cx, vfs.clone(), &dir, keys()))
                .unwrap_or_else(|error| panic!("{open} open: {error}"));
            assert_eq!(
                db.manifest().unwrap(),
                v2,
                "{open}: the stored V2 manifest stays selected"
            );
            db.vertices().unwrap().len()
        };
        assert_eq!(read, vertices, "{open}");
        assert_eq!(
            runtime.block_on(vfs.read(&slot_file)).unwrap(),
            written,
            "{open}: an open with no heal due must not touch the root slot"
        );
    }

    let mut db = runtime
        .block_on(Database::open_with_vfs(&cx, vfs.clone(), &dir, keys()))
        .unwrap();
    runtime.block_on(db.write(&cx, batch(6))).unwrap();
    let advanced = db.manifest().unwrap();
    assert_ne!(advanced, v2, "a commit publishes a manifest of its own");
    drop(db);
    let db = runtime
        .block_on(Database::open_with_vfs(&cx, vfs.clone(), &dir, keys()))
        .unwrap();
    assert_eq!(db.manifest().unwrap(), advanced);
    assert_eq!(db.vertices().unwrap().len(), vertices + 1);
}
