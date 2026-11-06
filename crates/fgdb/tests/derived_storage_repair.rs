//! Explicit reconstruction of damaged derived data from authenticated Chronicle.
//! Faults change stored bytes, not the recovered graph or the repair evaluator.

#![recursion_limit = "256"]

use asupersync::fs::Vfs;
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, EmbeddedReadView, MemVfs, OpenError, RelationBind,
    VertexRow, WriteBatch,
};
use fgdb_chronicle::commit::{CAPSULE_DIR, COMMIT_LOG_NAME, CommitCoordinator};
use fgdb_chronicle::marker::EffectSource;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::store::{BLOCK_DIR, BlockStore, PACK_DIR, PACK_MIN_OBJECTS};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    ObjectId, PurposeContexts, VId,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const K_OID: [u8; 32] = [0xd1; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xd2; 32]);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const L: LabelId = LabelId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0xd3; 32])
}

async fn fixture(
    cx: &CommitCx,
    vfs: &MemVfs,
    packed: bool,
) -> (Database<MemVfs>, EmbeddedReadView) {
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let sources = if packed {
        PACK_MIN_OBJECTS as u128 + 1
    } else {
        3
    };
    let sink = sources + 1;
    let mut batch = WriteBatch::new(R);
    for id in 1..=sink {
        batch.create_vertex(VId(id), vec![L], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    for id in 1..=sources {
        batch.add_edge(
            EId(id),
            VId(id),
            VId(sink),
            vec![(P, CanonicalScalar::Int(id as i64))],
        );
    }
    batch.add_edge(
        EId(sources + 1),
        VId(1),
        VId(sink),
        vec![(P, CanonicalScalar::Int(80))],
    );
    batch.add_edge(
        EId(sources + 2),
        VId(sink),
        VId(sink),
        vec![(P, CanonicalScalar::Int(90))],
    );
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq::FIRST);
    let pinned = db.read_session().unwrap();
    // The neighbour projection is distinct, while the lossless edge face must
    // retain both parallel identities before any repair or deletion.
    assert_eq!(pinned.neighbours(VId(1), R).unwrap(), vec![VId(sink)]);
    let parallel: Vec<_> = pinned
        .edges()
        .unwrap()
        .into_iter()
        .filter(|edge| edge.entry.src == VId(1) && edge.entry.relation == R)
        .map(|edge| (edge.entry.eid, edge.entry.dst))
        .collect();
    assert_eq!(
        parallel,
        vec![(EId(1), VId(sink)), (EId(sources + 1), VId(sink))]
    );
    assert_eq!(pinned.neighbours(VId(sink), R).unwrap(), vec![VId(sink)]);

    let mut update = WriteBatch::new(R);
    update.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(222)));
    update.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(111)));
    update.delete_edge(EId(sources + 1));
    update.create_vertex(VId(sink + 1), vec![L], vec![(P, CanonicalScalar::Int(333))]);
    update.add_edge(
        EId(sources + 3),
        VId(2),
        VId(sink + 1),
        vec![(P, CanonicalScalar::Int(444))],
    );
    assert_eq!(db.write(cx, update).await.unwrap(), CommitSeq(2));
    assert_eq!(db.vertices().unwrap().len(), sink as usize + 1);
    assert_eq!(db.edges().unwrap().len(), sources as usize + 2);
    assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(sink)]);
    assert_eq!(
        db.edge(EId(1)).unwrap().unwrap().props,
        vec![(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(111)))]
    );
    (db, pinned)
}

#[derive(Debug, PartialEq)]
struct Answers {
    vertices: Vec<VertexRow>,
    edges: Vec<EdgeRecord>,
    query: Vec<VId>,
}

fn answers(db: &Database<MemVfs>) -> Vec<Answers> {
    (0..=db.frontier().unwrap().0)
        .map(|seq| Answers {
            vertices: db.vertices_at(CommitSeq(seq)).unwrap(),
            edges: db.edges_at(CommitSeq(seq)).unwrap(),
            query: db
                .execute_gql_at(
                    "MATCH (a)-[:R]->(b) RETURN b",
                    &RelationBind::new().with_relation("R", R),
                    CommitSeq(seq),
                )
                .unwrap(),
        })
        .collect()
}

struct Object {
    id: ObjectId,
    path: PathBuf,
    bytes: Vec<u8>,
}

// Select the data through the published root before damage. No repair report
// or cached post-damage lookup chooses the expected inventory.
async fn data_objects(cx: &CommitCx, vfs: &MemVfs, db: &Database<MemVfs>) -> Vec<Object> {
    let store = BlockStore::open_with_vfs(cx, vfs.clone(), vfs.database_dir(), K_OID, NAMESPACE)
        .await
        .unwrap();
    let root = store
        .get_root(cx, db.partition_root().unwrap())
        .await
        .unwrap();
    let block = root.blocks[0].block_id;
    let bytes = store
        .get_bytes(cx, fgdb_strata::DeltaBlockVersion(block))
        .await
        .unwrap();
    let property = fgdb_strata::decode_block_with_properties(&bytes)
        .unwrap()
        .1
        .unwrap()
        .0;
    let vertex = root.vertex_patches[0].patch_id;
    vec![
        Object {
            id: block,
            path: store.path(block),
            bytes,
        },
        Object {
            id: property,
            path: store.path(property),
            bytes: store
                .get_edge_property_patch_bytes(cx, property)
                .await
                .unwrap(),
        },
        Object {
            id: vertex,
            path: store.path(vertex),
            bytes: store
                .get_patch_bytes(cx, fgdb_strata::vertex::VertexPatchVersion(vertex))
                .await
                .unwrap(),
        },
    ]
}

// Locate a packed realization independently from the store's lookup/cache.
// The public FGPK table uses a 12-byte header and 44-byte entries.
async fn damage_packed(vfs: &MemVfs, object: &Object) -> (PathBuf, Vec<u8>) {
    assert!(
        vfs.metadata(&object.path).await.is_err(),
        "fixture must be packed"
    );
    let mut files = vfs
        .read_dir(&object.path.parent().unwrap().join(PACK_DIR))
        .await
        .unwrap();
    while let Some(file) = files.next_entry().await.unwrap() {
        let path = file.path();
        if path.extension().is_none_or(|extension| extension != "pack") {
            continue;
        }
        let mut bytes = vfs.read(&path).await.unwrap();
        assert_eq!(&bytes[..4], b"FGPK");
        let count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        for at in 0..count {
            let row = &bytes[12 + at * 44..12 + (at + 1) * 44];
            if row[..32] != object.id.0 {
                continue;
            }
            let offset = u64::from_le_bytes(row[32..40].try_into().unwrap()) as usize;
            let length = u32::from_le_bytes(row[40..44].try_into().unwrap()) as usize;
            assert_eq!(&bytes[offset..offset + length], object.bytes.as_slice());
            bytes[offset + length / 2] ^= 0x40;
            vfs.write(&path, &bytes).await.unwrap();
            return (path, bytes);
        }
    }
    panic!("no packed realization for {:?}", object.id);
}

async fn loose_files(vfs: &MemVfs, dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    let mut files = vfs.read_dir(&dir.join(BLOCK_DIR)).await.unwrap();
    while let Some(file) = files.next_entry().await.unwrap() {
        let path = file.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "block")
        {
            result.insert(path.clone(), vfs.read(&path).await.unwrap());
        }
    }
    result
}

async fn repaired_case(cx: &CommitCx, packed: bool) {
    let vfs = MemVfs::new().unwrap();
    let dir = vfs.database_dir();
    let (db, pinned) = fixture(cx, &vfs, packed).await;
    let expected = answers(&db);
    let versions = db.element_versions().unwrap().clone();
    let root = db.partition_root().unwrap();
    let manifest = db.manifest().unwrap();
    let old_vertices = pinned.vertices().unwrap();
    let old_edges = pinned.edges().unwrap();
    let objects = data_objects(cx, &vfs, &db).await;
    let markers = vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap();
    let slot = vfs.read(&dir.join("manifest.root")).await.unwrap();
    drop(db);
    let mut damaged_packs = BTreeMap::new();
    for object in &objects {
        if packed && vfs.metadata(&object.path).await.is_err() {
            let (path, bytes) = damage_packed(&vfs, object).await;
            damaged_packs.insert(path, bytes);
        } else {
            assert_eq!(vfs.read(&object.path).await.unwrap(), object.bytes);
            let mut bytes = object.bytes.clone();
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0x40;
            vfs.write(&object.path, &bytes).await.unwrap();
        }
    }
    assert_eq!(!damaged_packs.is_empty(), packed);
    assert!(
        Database::open_with_vfs(cx, vfs.clone(), &dir, keys())
            .await
            .is_err(),
        "ordinary open must not silently repair corrupted derived data"
    );
    let mut repaired = Database::open_repairing_derived_with_vfs(cx, vfs.clone(), &dir, keys())
        .await
        .unwrap();
    assert_eq!(repaired.frontier().unwrap(), CommitSeq(2));
    assert_eq!(repaired.partition_root().unwrap(), root);
    assert_eq!(repaired.manifest().unwrap(), manifest);
    assert_eq!(repaired.element_versions().unwrap(), &versions);
    assert_eq!(answers(&repaired), expected);
    assert_eq!(repaired.delta_since(CommitSeq::ORIGIN).unwrap().count(), 2);
    assert_eq!(vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap(), markers);
    assert_eq!(vfs.read(&dir.join("manifest.root")).await.unwrap(), slot);
    for object in &objects {
        assert_eq!(
            vfs.read(&object.path).await.unwrap(),
            object.bytes,
            "repair restores the exact originally admitted content"
        );
    }
    for (path, bytes) in damaged_packs {
        assert_eq!(
            vfs.read(&path).await.unwrap(),
            bytes,
            "a loose authenticated replacement must not rewrite a shared pack"
        );
    }
    let summary = repaired.scrub(cx).await.unwrap();
    assert!(summary.lost.is_empty());
    assert!(summary.block_lost.is_empty());
    assert!(summary.metadata_lost.is_empty());
    for object in &objects {
        assert!(summary.block_clean.contains(&object.id));
    }
    assert_eq!(pinned.frontier(), CommitSeq::FIRST);
    assert_eq!(pinned.vertices().unwrap(), old_vertices);
    assert_eq!(pinned.edges().unwrap(), old_edges);
    drop(repaired);
    let mut reopened = Database::open_with_vfs(cx, vfs.clone(), &dir, keys())
        .await
        .unwrap();
    assert_eq!(answers(&reopened), expected);
    let clean = reopened.scrub(cx).await.unwrap();
    assert!(clean.block_lost.is_empty() && clean.metadata_lost.is_empty());
    let mut next = WriteBatch::new(R);
    next.create_vertex(VId(10_000), vec![L], vec![]);
    assert_eq!(reopened.write(cx, next).await.unwrap(), CommitSeq(3));
    assert_eq!(pinned.vertices().unwrap(), old_vertices);
    assert_eq!(pinned.edges().unwrap(), old_edges);
}

#[test]
fn repairs_loose_graph_and_property_data_without_changing_history() {
    let ((), report) = run_async_under_lab(0xd370_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        repaired_case(&contexts.commit(), false).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repairs_packed_payloads_with_authenticated_loose_realizations() {
    let ((), report) = run_async_under_lab(0xd370_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        repaired_case(&contexts.commit(), true).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unavailable_capsule_refuses_before_any_derived_repair_or_root_publication() {
    let ((), report) = run_async_under_lab(0xd370_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let (db, pinned) = fixture(&cx, &vfs, false).await;
        let object = data_objects(&cx, &vfs, &db).await.remove(0);
        let expected = answers(&db);
        let pinned_edges = pinned.edges().unwrap();
        // The old generation cannot know the later content retirement/delete.
        // A retrospective read from the new generation carries those ends;
        // neither is the baseline for mutating the other generation's snapshot.
        for eid in [EId(1), EId(4)] {
            let original = pinned_edges
                .iter()
                .find(|edge| edge.entry.eid == eid)
                .unwrap();
            let retrospective = expected[1]
                .edges
                .iter()
                .find(|edge| edge.entry.eid == eid)
                .unwrap();
            assert_eq!(original.entry.retired_at, None);
            assert_eq!(retrospective.entry.retired_at, Some(CommitSeq(2)));
        }
        drop(db);
        let coordinator =
            CommitCoordinator::open_with_vfs(&cx, vfs.clone(), &dir, keys().capsule_keys())
                .await
                .unwrap();
        let EffectSource::Local { capsule_ref, .. } = &coordinator
            .chain()
            .entries()
            .last()
            .unwrap()
            .marker
            .effect_source;
        let hex: String = capsule_ref
            .0
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let capsule = dir.join(CAPSULE_DIR).join(format!("{hex}.capsule"));
        drop(coordinator);
        let original = vfs.read(&capsule).await.unwrap();
        vfs.write(&capsule, b"truncated retained capsule")
            .await
            .unwrap();
        vfs.write(&object.path, b"damaged derived data")
            .await
            .unwrap();
        let before = loose_files(&vfs, &dir).await;
        let slot = vfs.read(&dir.join("manifest.root")).await.unwrap();
        let markers = vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap();
        let error = Database::open_repairing_derived_with_vfs(&cx, vfs.clone(), &dir, keys())
            .await
            .expect_err("a complete checkpoint cannot authorize a partial replay");
        assert!(matches!(error, OpenError::Rebuild(_)), "{error:?}");
        assert_eq!(loose_files(&vfs, &dir).await, before);
        assert_eq!(vfs.read(&dir.join("manifest.root")).await.unwrap(), slot);
        assert_eq!(vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap(), markers);
        assert_eq!(
            vfs.read(&capsule).await.unwrap(),
            b"truncated retained capsule"
        );
        assert_eq!(pinned.frontier(), CommitSeq::FIRST);
        assert_eq!(pinned.edges().unwrap(), pinned_edges);
        // Restore only the fixture's source loss; the product API must still
        // reconstruct the damaged derived object through authenticated replay.
        vfs.write(&capsule, &original).await.unwrap();
        let repaired = Database::open_repairing_derived_with_vfs(&cx, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        assert_eq!(answers(&repaired), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn corrupt_selected_metadata_cannot_authorize_data_replacement() {
    let ((), report) = run_async_under_lab(0xd370_0004, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        for manifest in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let dir = vfs.database_dir();
            let (db, _) = fixture(&cx, &vfs, false).await;
            let object = data_objects(&cx, &vfs, &db).await.remove(0);
            let store = BlockStore::open_with_vfs(&cx, vfs.clone(), &dir, K_OID, NAMESPACE)
                .await
                .unwrap();
            let id = if manifest {
                db.manifest().unwrap().0
            } else {
                db.partition_root().unwrap().0
            };
            let metadata = store.path(id);
            drop(store);
            drop(db);
            vfs.write(&metadata, b"damaged selected metadata")
                .await
                .unwrap();
            vfs.write(&object.path, b"damaged derived data")
                .await
                .unwrap();
            let before = loose_files(&vfs, &dir).await;
            let slot = vfs.read(&dir.join("manifest.root")).await.unwrap();
            assert!(
                Database::open_repairing_derived_with_vfs(&cx, vfs.clone(), &dir, keys())
                    .await
                    .is_err()
            );
            assert_eq!(loose_files(&vfs, &dir).await, before);
            assert_eq!(vfs.read(&dir.join("manifest.root")).await.unwrap(), slot);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[cfg(unix)]
#[test]
fn explicit_repair_refuses_a_symlink_at_a_derived_object_path() {
    let ((), report) = run_async_under_lab(0xd370_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        // Reserve the parent atomically: retained fixtures and concurrent
        // attempts must never be opened or overwritten after a name collision.
        let reserve = || -> std::io::Result<PathBuf> {
            for ordinal in 0..u64::MAX {
                let parent = std::env::temp_dir().join(format!(
                    "fgdb-derived-repair-symlink-{}-{ordinal}",
                    std::process::id(),
                ));
                match std::fs::create_dir(&parent) {
                    Ok(()) => return Ok(parent),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            Err(std::io::Error::other("fixture namespace exhausted"))
        };
        let occupied = reserve().unwrap();
        let marker = occupied.join("retained-owner");
        std::fs::write(&marker, b"retained fixture owner").unwrap();
        let parent = reserve().unwrap();
        assert_ne!(parent, occupied);
        assert_eq!(std::fs::read(&marker).unwrap(), b"retained fixture owner");
        let dir = parent.join("database");
        let mut db = Database::create(&cx, &dir, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![L], vec![]);
        batch.create_vertex(VId(2), vec![L], vec![]);
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        db.write(&cx, batch).await.unwrap();
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE).await.unwrap();
        let root = store
            .get_root(&cx, db.partition_root().unwrap())
            .await
            .unwrap();
        let id = root.blocks[0].block_id;
        let object = Object {
            id,
            path: store.path(id),
            bytes: store
                .get_bytes(&cx, fgdb_strata::DeltaBlockVersion(id))
                .await
                .unwrap(),
        };
        drop(store);
        drop(db);
        let retained = object.path.with_extension("retained-fixture");
        std::fs::rename(&object.path, &retained).unwrap();
        let target = dir.join("protected-fixture");
        std::fs::write(&target, b"unrelated protected bytes").unwrap();
        std::os::unix::fs::symlink(&target, &object.path).unwrap();
        let slot = std::fs::read(dir.join("manifest.root")).unwrap();
        assert!(
            Database::open_repairing_derived(&cx, &dir, keys())
                .await
                .is_err()
        );
        assert!(
            std::fs::symlink_metadata(&object.path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"unrelated protected bytes"
        );
        assert_eq!(std::fs::read(&retained).unwrap(), object.bytes);
        assert_eq!(std::fs::read(dir.join("manifest.root")).unwrap(), slot);
        assert_eq!(std::fs::read(&marker).unwrap(), b"retained fixture owner");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn missing_selected_root_cannot_authorize_data_replacement() {
    let ((), report) = run_async_under_lab(0xd370_0006, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let (db, _) = fixture(&cx, &vfs, false).await;
        let object = data_objects(&cx, &vfs, &db).await.remove(0);
        drop(db);
        let slot = dir.join("manifest.root");
        let retained_slot = dir.join("manifest.retained-fixture");
        let slot_bytes = vfs.read(&slot).await.unwrap();
        let markers = vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap();
        vfs.rename(&slot, &retained_slot).await.unwrap();
        vfs.write(&object.path, b"damaged derived data")
            .await
            .unwrap();
        let before = loose_files(&vfs, &dir).await;
        assert!(
            Database::open_repairing_derived_with_vfs(&cx, vfs.clone(), &dir, keys())
                .await
                .is_err()
        );
        assert_eq!(loose_files(&vfs, &dir).await, before);
        assert_eq!(
            vfs.metadata(&slot).await.unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(vfs.read(&retained_slot).await.unwrap(), slot_bytes);
        assert_eq!(vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap(), markers);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repair_reconciles_a_compacted_generation_without_losing_semantic_history() {
    let ((), report) = run_async_under_lab(0xd370_0007, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let (mut db, pinned) = fixture(&cx, &vfs, false).await;
        let expected = answers(&db);
        let versions = db.element_versions().unwrap().clone();
        let original_root = db.partition_root().unwrap();
        let old_vertices = pinned.vertices().unwrap();
        let old_edges = pinned.edges().unwrap();
        db.compact(&cx).await.unwrap();
        assert_ne!(db.partition_root().unwrap(), original_root);
        assert_eq!(answers(&db), expected);
        let objects = data_objects(&cx, &vfs, &db).await;
        let markers = vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap();
        drop(db);
        for object in &objects {
            vfs.write(&object.path, b"damaged compacted data")
                .await
                .unwrap();
        }
        assert!(
            Database::open_with_vfs(&cx, vfs.clone(), &dir, keys())
                .await
                .is_err()
        );
        let mut repaired =
            Database::open_repairing_derived_with_vfs(&cx, vfs.clone(), &dir, keys())
                .await
                .unwrap();
        // Replaying the same Chronicle may restore its pre-compaction layout.
        // The selected metadata authorizes reconciliation, never a new history.
        assert_eq!(repaired.frontier().unwrap(), CommitSeq(2));
        assert_eq!(answers(&repaired), expected);
        assert_eq!(repaired.element_versions().unwrap(), &versions);
        assert_eq!(vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap(), markers);
        let summary = repaired.scrub(&cx).await.unwrap();
        assert!(summary.lost.is_empty());
        assert!(summary.block_lost.is_empty());
        assert!(summary.metadata_lost.is_empty());
        assert_eq!(pinned.vertices().unwrap(), old_vertices);
        assert_eq!(pinned.edges().unwrap(), old_edges);
        drop(repaired);
        let mut reopened = Database::open_with_vfs(&cx, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        assert_eq!(answers(&reopened), expected);
        let summary = reopened.scrub(&cx).await.unwrap();
        assert!(summary.lost.is_empty());
        assert!(summary.block_lost.is_empty());
        assert!(summary.metadata_lost.is_empty());
        assert_eq!(vfs.read(&dir.join(COMMIT_LOG_NAME)).await.unwrap(), markers);
        let mut next = WriteBatch::new(R);
        next.create_vertex(VId(10_000), vec![L], vec![]);
        assert_eq!(reopened.write(&cx, next).await.unwrap(), CommitSeq(3));
        assert_eq!(pinned.vertices().unwrap(), old_vertices);
        assert_eq!(pinned.edges().unwrap(), old_edges);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
