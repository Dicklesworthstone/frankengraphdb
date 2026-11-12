//! End-to-end verification for `fgdb-rc1111-strata-ecs-coding-29bud`:
//! Strata Encrypt-then-Code (`FGEC` v1) extents across loose & packed `FGSB`,
//! `FGSP`, `FGVP`, `FGRN`, `Manifest`, `Root`, and `Pack` objects, `O(1)`
//! single-frame random reads, `Database::scrub` repair-in-place, and `FGKE-V1`
//! Argon2id passphrase-wrapped key envelopes.

use asupersync::fs::UnixVfs;
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferedReadLimits, Database, DatabaseKeys, ExtentError, KeyEnvelopeError, MemoryPool,
    SealPublicationError, StrataEcsConfig, WriteBatch, is_key_envelope_v1,
};
use fgdb_chronicle::LostReason;
use fgdb_crypto::zeroize::SharedSecret;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::store::{BLOCK_DIR, BlockStore, PACK_DIR, RootReadLimits, StoreError};
use fgdb_strata::tiered::sealed::{AdjRunSegmentVersion, SealedLimits};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalPropertyValue, CanonicalScalar, EId, GraphId, VId};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn scratch_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "fgdb-strata-ecs-{tag}-{}-{nanos}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn sample_keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}

fn collect_regular_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_regular_files(&path, out);
        } else if meta.is_file() {
            out.push(path);
        }
    }
}

#[test]
fn loose_and_packed_strata_extents_heal_bit_rot_and_truncation_and_scrub_repairs_in_place() {
    let temp = scratch_dir("heal");
    let db_path = temp.join("ecs_db");
    let keys = sample_keys().with_default_strata_ecs();

    let db_path_c1 = db_path.clone();
    let keys_c1 = keys.clone();
    let ((manifest_id, root_id, run_id), _) = run_async_under_lab(0xEC51_2026, move |root| {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db_path = db_path_c1.clone();
        let keys = keys_c1.clone();
        async move {
            let cx = contexts.commit();
            let qcx = contexts.query();
            let mut db = Database::create(&cx, &db_path, keys.clone())
                .await
                .expect("create FGEC database");

            // 1. Small batch -> loose FGEC extents (FGSB, FGSP, FGVP, Manifest, Root).
            let mut small = WriteBatch::new(RelationId(1));
            small.create_vertex(
                VId(1),
                vec![LabelId(10)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(100))],
            );
            small.create_vertex(
                VId(2),
                vec![LabelId(10)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(200))],
            );
            small.add_edge(
                EId(1),
                VId(1),
                VId(2),
                vec![(PropertyKeyId(2), CanonicalScalar::Int(7))],
            );
            db.write(&cx, small).await.expect("write loose batch");

            // 2. Large batch (> 64 distinct vertex patches + blocks) -> packed FGEC Pack extent!
            let mut large = WriteBatch::new(RelationId(1));
            for i in 10..90u128 {
                large.create_vertex(
                    VId(i),
                    vec![LabelId(10)],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(i as i64))],
                );
            }
            for i in 10..89u128 {
                large.add_edge(
                    EId(1000 + i),
                    VId(i),
                    VId(i + 1),
                    vec![(PropertyKeyId(2), CanonicalScalar::Int(1))],
                );
            }
            db.write(&cx, large).await.expect("write packed batch");

            // 3. Seal RelationId(1) -> durable FGRN AdjRun FGEC extent!
            let run = db
                .seal_relation(
                    &cx,
                    &qcx,
                    RelationId(1),
                    fgdb_types::CommitSeq(0),
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap_or_else(|err: SealPublicationError| panic!("seal relation: {err}"));

            let manifest_id = db.manifest().expect("manifest").0;
            let root_id = db.partition_root().expect("partition_root").0;
            (manifest_id, root_id, run.0)
        }
    });

    // Verify EVERY regular object file under strata-blocks/ begins with FGEC magic!
    let blocks_dir = db_path.join(BLOCK_DIR);
    let mut block_files = Vec::new();
    collect_regular_files(&blocks_dir, &mut block_files);
    assert!(
        !block_files.is_empty(),
        "expected loose and packed files under strata-blocks/"
    );
    let mut pack_files = Vec::new();
    for file in &block_files {
        let bytes = std::fs::read(file).expect("read block file");
        assert!(
            bytes.starts_with(b"FGEC"),
            "every Strata object/pack file must be FGEC-encoded: {}",
            file.display()
        );
        if file
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == PACK_DIR)
        {
            pack_files.push(file.clone());
        }
    }
    assert!(
        !pack_files.is_empty(),
        "large commit must publish at least one FGEC pack file"
    );

    let loose_file = |oid: fgdb_types::ids::ObjectId| -> PathBuf {
        let hex: String = oid.0.iter().map(|b| format!("{b:02x}")).collect();
        blocks_dir.join(format!("{hex}.block"))
    };
    let manifest_file = loose_file(manifest_id);
    let root_file = loose_file(root_id);
    let run_file = loose_file(run_id);

    // Corrupt:
    // (a) Manifest loose file: flip bytes inside frame 0 body AND truncate half the EOF mirror header!
    {
        let mut raw = std::fs::read(&manifest_file).unwrap();
        let body = raw.len() - 128 - 24;
        raw[body] ^= 0xFF;
        raw[body + 1] ^= 0xAA;
        let truncated = raw.len() - 64;
        raw.truncate(truncated);
        std::fs::write(&manifest_file, &raw).unwrap();
    }
    // (b) Root loose file: corrupt primary header (offset 16) + frame 0 body so mirror header + RaptorQ take over!
    {
        let mut raw = std::fs::read(&root_file).unwrap();
        raw[16] ^= 0x55;
        let body = raw.len() - 128 - 24;
        raw[body] ^= 0x33;
        std::fs::write(&root_file, &raw).unwrap();
    }
    // (c) AdjRun loose file: flip bytes inside frame 0 body
    {
        let mut raw = std::fs::read(&run_file).unwrap();
        let body = raw.len() - 128 - 200;
        raw[body] ^= 0xFF;
        std::fs::write(&run_file, &raw).unwrap();
    }
    // (d) Packed file: flip bytes inside frame body AND truncate into EOF mirror header + tail!
    {
        let mut raw = std::fs::read(&pack_files[0]).unwrap();
        let body = raw.len() - 128 - 300;
        raw[body] ^= 0xFF;
        let truncated = raw.len() - 160;
        raw.truncate(truncated);
        std::fs::write(&pack_files[0], &raw).unwrap();
    }

    // Prove transparent RaptorQ read healing across read-only view, buffered view, and full open + scrub!
    let db_path_c2 = db_path.clone();
    let keys_c2 = keys.clone();
    let _ = run_async_under_lab(0xEC52_2026, move |root| {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db_path = db_path_c2.clone();
        let keys = keys_c2.clone();
        async move {
            let cx = contexts.commit();
            let qcx = contexts.query();

            // 1. Read-only view heals transparently without writing!
            let view = Database::open_read_view(&cx, &db_path, keys.clone())
                .await
                .expect("open_read_view must heal corrupted FGEC extents transparently");
            assert_eq!(
                view.neighbours(VId(1), RelationId(1)).unwrap(),
                vec![VId(2)]
            );
            assert_eq!(
                view.neighbours(VId(10), RelationId(1)).unwrap(),
                vec![VId(11)]
            );
            drop(view);

            // 2. Buffered cold read view heals transparently on extent fault!
            let pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let limits = BufferedReadLimits {
                max_root_bytes: 512 * 1024,
                max_source_bytes: 32 * 1024 * 1024,
                max_blocks: 4096,
                max_vertex_patches: 4096,
                max_work: 1_000_000,
                buffer: fgdb::BufferLimits {
                    max_frames: 32,
                    max_ghost_entries: 64,
                    max_extent_bytes: 16 * 1024,
                },
            };
            let mut bview =
                Database::open_buffered_read_view(&cx, &db_path, keys.clone(), pool, limits)
                    .await
                    .expect("open_buffered_read_view must heal corrupted FGEC extents");
            let v50 = bview
                .vertex(&qcx, VId(50))
                .await
                .expect("buffered vertex fault")
                .expect("VId(50) exists");
            assert_eq!(v50.labels, vec![LabelId(10)]);
            assert_eq!(
                v50.props,
                vec![(
                    PropertyKeyId(1),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(50))
                )]
            );
            drop(v50);
            drop(bview);

            // 3. Writable Database::open + Database::scrub repairs degraded loose & packed files in place!
            let mut db = Database::open(&cx, &db_path, keys.clone())
                .await
                .expect("Database::open must heal FGEC extents");
            assert_eq!(db.neighbours(VId(1), RelationId(1)).unwrap(), vec![VId(2)]);
            assert_eq!(
                db.neighbours(VId(88), RelationId(1)).unwrap(),
                vec![VId(89)]
            );

            let first_scrub = db.scrub(&cx).await.expect("first scrub");
            assert!(first_scrub.lost.is_empty());
            assert!(first_scrub.block_lost.is_empty());
            assert!(first_scrub.metadata_lost.is_empty());
            assert!(
                !first_scrub.block_repaired.is_empty(),
                "expected repaired data/pack/run objects"
            );
            assert!(
                first_scrub.block_repaired.contains(&run_id),
                "AdjRun must be reported in block_repaired"
            );
            assert!(
                first_scrub.metadata_repaired.contains(&manifest_id)
                    && first_scrub.metadata_repaired.contains(&root_id),
                "Manifest and Root must be reported in metadata_repaired"
            );

            // 4. Second scrub proves physical files on disk were atomically rewritten clean!
            let second_scrub = db.scrub(&cx).await.expect("second scrub");
            assert!(second_scrub.block_repaired.is_empty());
            assert!(second_scrub.metadata_repaired.is_empty());
            assert!(second_scrub.block_lost.is_empty());
            assert!(second_scrub.metadata_lost.is_empty());
            assert_eq!(second_scrub.block_clean.len(), second_scrub.block_objects);
            assert_eq!(
                second_scrub.metadata_clean.len(),
                second_scrub.metadata_objects
            );
        }
    });

    // 5. Corrupt beyond R=4 symbols in one stripe of AdjRun and verify scrub reports InsufficientSymbols!
    {
        let mut raw = std::fs::read(&run_file).unwrap();
        let end = raw.len() - 128;
        for byte in &mut raw[128..end] {
            *byte ^= 0xFF;
        }
        std::fs::write(&run_file, &raw).unwrap();
    }

    let _ = run_async_under_lab(0xEC53_2026, move |root| {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db_path = db_path.clone();
        let keys = keys.clone();
        async move {
            let cx = contexts.commit();
            let mut db = Database::open(&cx, &db_path, keys)
                .await
                .expect("open still succeeds from Tier-D blocks when optional AdjRun is damaged");
            let scrub = db
                .scrub(&cx)
                .await
                .expect("scrub completes and reports loss");
            assert_eq!(scrub.block_lost.len(), 1);
            assert_eq!(scrub.block_lost[0].object_id, run_id);
            assert_eq!(scrub.block_lost[0].reason, LostReason::InsufficientSymbols);
        }
    });
}

#[test]
fn adj_run_single_frame_random_read_is_o1_and_heals_corrupted_frame() {
    let temp = scratch_dir("frame");
    let db_path = temp.join("run_db");
    let keys = sample_keys().with_default_strata_ecs();

    let _ = run_async_under_lab(0xEC54_2026, move |root| {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db_path = db_path.clone();
        let keys = keys.clone();
        async move {
            let cx = contexts.commit();
            let qcx = contexts.query();
            let mut db = Database::create(&cx, &db_path, keys.clone())
                .await
                .expect("create");
            let mut batch = WriteBatch::new(RelationId(7));
            for i in 1..=4_500u128 {
                batch.create_vertex(VId(i), vec![], vec![]);
            }
            for i in 1..4_500u128 {
                batch.add_edge(
                    EId(i),
                    VId(i),
                    VId(i + 1),
                    vec![(PropertyKeyId(9), CanonicalScalar::Int((i * 17) as i64))],
                );
            }
            db.write(&cx, batch)
                .await
                .expect("write 4500 vertices/edges");
            let version: AdjRunSegmentVersion = db
                .seal_relation(
                    &cx,
                    &qcx,
                    RelationId(7),
                    fgdb_types::CommitSeq(0),
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .expect("seal multi-frame relation");
            drop(db);

            let store = BlockStore::open_existing_for_read_with_vfs_and_ecs(
                &cx,
                UnixVfs::new(),
                &db_path,
                SharedSecret::new(*keys.k_oid()),
                keys.namespace,
                keys.strata_ecs().cloned(),
            )
            .await
            .unwrap();

            let encoded = store
                .get_adj_run_bytes(&cx, version)
                .await
                .expect("get_adj_run_bytes");
            assert!(
                encoded.len() > 2 * 16 * 1024,
                "expected multi-frame FGRN segment, got {} bytes",
                encoded.len()
            );

            // Random single-frame read of frame 1 (healthy fast path: was_repaired == false)
            let (frame, was_repaired) = store
                .read_adj_run_frame(&cx, version, 1)
                .await
                .expect("O(1) single-frame read");
            assert!(!was_repaired);
            assert_eq!(&frame[100..612], &encoded[16 * 1024 + 100..16 * 1024 + 612]);

            // Corrupt frame 1 ciphertext on disk and verify read_adj_run_frame heals via stripe RaptorQ!
            let run_path = store.path(version.0);
            let mut disk = std::fs::read(&run_path).unwrap();
            let frame_count = encoded.len().div_ceil(16 * 1024);
            let last_plain_len = encoded.len() - (frame_count - 1) * (16 * 1024);
            let full_record_bytes = 32 + 16 * 1024 + 16;
            let last_record_bytes = 32 + last_plain_len + 16;
            let frames_total_bytes = (frame_count - 1) * full_record_bytes + last_record_bytes;
            let frames_start_offset = disk.len() - 128 - frames_total_bytes;
            let frame1_disk_offset = frames_start_offset + full_record_bytes + 64;
            disk[frame1_disk_offset] ^= 0xFF;
            disk[frame1_disk_offset + 1] ^= 0x77;
            std::fs::write(&run_path, &disk).unwrap();

            let (healed, healed_flag) = store
                .read_adj_run_frame(&cx, version, 1)
                .await
                .expect("RaptorQ stripe heal on single-frame read");
            assert!(healed_flag);
            assert_eq!(
                &healed[100..612],
                &encoded[16 * 1024 + 100..16 * 1024 + 612]
            );
        }
    });
}

#[test]
fn passphrase_envelope_and_wrong_dek_or_cross_coordinate_swap_refuse_closed() {
    let temp = scratch_dir("envelope");
    let db_path = temp.join("env_db");

    let base_keys = sample_keys();
    let envelope = base_keys
        .seal_passphrase_envelope(b"correct-horse-battery-staple", [0xAA; 16], [0xBB; 24])
        .expect("seal FGKE-V1");
    assert!(is_key_envelope_v1(&envelope));

    assert_eq!(
        DatabaseKeys::open_passphrase_envelope(b"wrong-passphrase", &envelope).unwrap_err(),
        KeyEnvelopeError::AuthenticationFailed
    );

    let unwrapped =
        DatabaseKeys::open_passphrase_envelope(b"correct-horse-battery-staple", &envelope)
            .expect("unwrap FGKE-V1");
    assert!(unwrapped.strata_ecs().is_some());

    let _ = run_async_under_lab(0xEC55_2026, move |root| {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db_path = db_path.clone();
        async move {
            let cx = contexts.commit();
            let mut db = Database::create(&cx, &db_path, unwrapped.clone())
                .await
                .expect("create with unwrapped FGKE-V1 keys");
            let mut batch = WriteBatch::new(RelationId(1));
            batch.create_vertex(VId(1), vec![], vec![]);
            db.write(&cx, batch).await.expect("write");
            drop(db);

            // 1. Opening without Strata ECS refuses with AuthenticationFailed.
            let Err(plain_err) = Database::open(&cx, &db_path, sample_keys()).await else {
                panic!("plain open must refuse FGEC files");
            };
            assert!(
                matches!(
                    plain_err,
                    fgdb::OpenError::Store(StoreError::Extent(ExtentError::AuthenticationFailed))
                ),
                "unexpected error: {plain_err:?}"
            );

            // 2. Opening with wrong Strata DEK refuses with AuthenticationFailed.
            let wrong_strata = sample_keys().with_strata_ecs(
                StrataEcsConfig::new(SharedSecret::new([0x99; 32])).with_coordinates(
                    GraphId(1),
                    BranchId(1),
                    1,
                ),
            );
            let Err(wrong_dek_err) = Database::open(&cx, &db_path, wrong_strata).await else {
                panic!("wrong Strata DEK must fail closed");
            };
            assert!(
                matches!(
                    wrong_dek_err,
                    fgdb::OpenError::Store(StoreError::Extent(ExtentError::AuthenticationFailed))
                ),
                "unexpected error: {wrong_dek_err:?}"
            );

            // 3. Opening with same Strata DEK on wrong BranchId refuses (AAD / header MAC mismatch).
            let right_dek = unwrapped.strata_ecs().unwrap().shared_dek();
            let wrong_coord = sample_keys().with_strata_ecs(
                StrataEcsConfig::new(right_dek).with_coordinates(GraphId(1), BranchId(2), 1),
            );
            let Err(swap_err) = Database::open(&cx, &db_path, wrong_coord).await else {
                panic!("cross-branch coordinate swap must fail closed");
            };
            assert!(
                matches!(
                    swap_err,
                    fgdb::OpenError::Store(StoreError::Extent(ExtentError::AuthenticationFailed))
                ),
                "unexpected error: {swap_err:?}"
            );
        }
    });
}
