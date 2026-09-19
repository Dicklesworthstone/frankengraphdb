//! Engine capsule scrub acceptance: exact inventory, canonical bytes, loss fencing,
//! retained-snapshot answers, and crashes at every repair I/O boundary.

use asupersync::fs::Vfs;
use asupersync::lab::run_async_under_lab;
use fgdb::{
    CAPSULE_OBJECT_KIND, Database, DatabaseKeys, EdgeRecord, GqlError, OpenError, ReadError,
    RelationBind, ScrubCrashPoint, ScrubSummary, VertexRow, WriteBatch,
};
use fgdb_chronicle::capsule::{
    CAPSULE_HEADER_BYTES_V1, CapsuleKeys, CapsuleProfile, SealedCapsule, decode_container,
    encode_container,
};
use fgdb_chronicle::commit::{CAPSULE_DIR, CommitCoordinator, CommitError};
use fgdb_chronicle::marker::EffectSource;
use fgdb_chronicle::scrub::LostReason;
use fgdb_chronicle::symbol::HEADER_LEN_V1;
use fgdb_delta_types::RelationId;
use fgdb_sim::vfs::{FaultPlan, FaultVfs, Trigger};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CommitSeq, EId, ObjectId, VId};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];
const COMMITS: usize = 4;

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        K_OID,
        NAMESPACE,
        DEK,
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fgdb-scrub-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

fn four_batches() -> [WriteBatch; COMMITS] {
    let mut a = WriteBatch::new(R);
    a.create_vertex(VId(1), vec![], vec![]);
    a.create_vertex(VId(2), vec![], vec![]);
    a.add_edge(EId(10), VId(1), VId(2), vec![]);
    let mut b = WriteBatch::new(R);
    b.create_vertex(VId(3), vec![], vec![]);
    b.add_edge(EId(11), VId(3), VId(2), vec![]);
    let mut c = WriteBatch::new(R);
    c.create_vertex(VId(4), vec![], vec![]);
    c.add_edge(EId(12), VId(4), VId(1), vec![]);
    let mut d = WriteBatch::new(R);
    d.create_vertex(VId(5), vec![], vec![]);
    d.add_edge(EId(13), VId(5), VId(3), vec![]);
    [a, b, c, d]
}

async fn commit_four<V: Vfs + Clone>(database: &mut Database<V>, cx: &CommitCx) {
    for batch in four_batches() {
        database
            .write(cx, batch)
            .await
            .expect("commit distinct capsule");
    }
    assert_eq!(
        database.frontier().expect("frontier"),
        CommitSeq(COMMITS as u64)
    );
}

async fn committed_ids(cx: &CommitCx, dir: &Path) -> Vec<ObjectId> {
    // The expected inventory comes from durable markers, not scrub's output.
    let oracle = CommitCoordinator::open(cx, dir, oracle_keys())
        .await
        .expect("marker oracle");
    let ids: Vec<_> = oracle
        .chain()
        .entries()
        .iter()
        .map(|entry| match &entry.marker.effect_source {
            EffectSource::Local { capsule_ref, .. } => *capsule_ref,
        })
        .collect();
    assert_eq!(ids.len(), COMMITS);
    assert_eq!(
        id_set(&ids).len(),
        COMMITS,
        "all four commits have distinct objects"
    );
    ids
}

fn capsule_disk_path(dir: &Path, oid: ObjectId) -> PathBuf {
    let mut name = String::with_capacity(72);
    for byte in oid.0 {
        name.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
        name.push(char::from_digit(u32::from(byte & 15), 16).expect("nibble"));
    }
    name.push_str(".capsule");
    dir.join(CAPSULE_DIR).join(name)
}

#[derive(Debug, PartialEq, Eq)]
struct Answers {
    at: CommitSeq,
    gql: Vec<VId>,
    vertices: Vec<VertexRow>,
    edges: Vec<EdgeRecord>,
    outgoing: Vec<Vec<VId>>,
    incoming: Vec<Vec<VId>>,
}

fn answers<V: Vfs + Clone>(database: &Database<V>) -> Vec<Answers> {
    (0..=COMMITS as u64)
        .map(|seq| {
            let at = CommitSeq(seq);
            let vertices = database
                .vertices_at(at)
                .expect("native vertices at retained snapshot");
            let edges = database
                .edges_at(at)
                .expect("native edges at retained snapshot");
            // Pin the fixture independently too: a shared before/after omission is not an oracle.
            assert_eq!(vertices.len(), if seq == 0 { 0 } else { seq as usize + 1 });
            assert_eq!(edges.len(), seq as usize);
            for row in &edges {
                assert_eq!(
                    database.edge_at(row.entry.eid, at).expect("native edge"),
                    Some(row.clone())
                );
            }
            Answers {
                at,
                gql: database
                    .execute_gql_at(PINNED, &bind_r(), at)
                    .expect("GQL at retained snapshot"),
                vertices,
                edges,
                outgoing: (1..=5)
                    .map(|vid| {
                        database
                            .neighbours_at(VId(vid), R, at)
                            .expect("native outgoing")
                    })
                    .collect(),
                incoming: (1..=5)
                    .map(|vid| {
                        database
                            .in_neighbours_at(VId(vid), R, at)
                            .expect("native incoming")
                    })
                    .collect(),
            }
        })
        .collect()
}

fn id_set(ids: &[ObjectId]) -> BTreeSet<ObjectId> {
    ids.iter().copied().collect()
}

fn assert_summary(
    summary: &ScrubSummary,
    clean: &[ObjectId],
    repaired: &[ObjectId],
    lost: &[ObjectId],
) {
    assert_eq!(
        summary.objects, COMMITS,
        "every marker-reachable object is enumerated"
    );
    assert_eq!(summary.clean.len(), clean.len(), "clean multiplicity");
    assert_eq!(
        summary.repaired.len(),
        repaired.len(),
        "repair multiplicity"
    );
    assert_eq!(summary.lost.len(), lost.len(), "lost multiplicity");
    assert_eq!(id_set(&summary.clean), id_set(clean));
    assert_eq!(id_set(&summary.repaired), id_set(repaired));
    assert_eq!(
        summary
            .lost
            .iter()
            .map(|item| item.object_id)
            .collect::<BTreeSet<_>>(),
        id_set(lost)
    );
}

fn flip_distinct_symbols(original: &[u8], oid: ObjectId, count: usize, mut seed: u64) -> Vec<u8> {
    let (descriptor, mut symbols) = decode_container(original).expect("decode genuine container");
    assert!(count <= symbols.len());
    let mut indices: Vec<_> = (0..symbols.len()).collect();
    for nth in 0..count {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let pick = nth + (seed as usize % (indices.len() - nth));
        indices.swap(nth, pick);
        let symbol = &mut symbols[indices[nth]];
        // Alter payload, preserving the descriptor, framing, symbol ID and old MAC.
        let payload_at =
            usize::from(HEADER_LEN_V1) + (seed as usize % usize::from(descriptor.symbol_size));
        symbol[payload_at] ^= 1 << ((seed >> 32) % 8);
    }
    let damaged = encode_container(&SealedCapsule {
        object_id: oid,
        descriptor,
        symbols,
    });
    assert_ne!(
        damaged, original,
        "the control really damages distinct authenticated symbols"
    );
    damaged
}

#[test]
fn scrub_enumerates_every_committed_capsule_and_repairs_within_budget() {
    let dir = scratch("enumerate-repair");
    let ((), report) = run_async_under_lab(0xbe01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut database = Database::create(&cx, &dir, engine_keys())
            .await
            .expect("create");
        commit_four(&mut database, &cx).await;
        let before = answers(&database);
        drop(database);
        let ids = committed_ids(&cx, &dir).await;
        let mut database = Database::open(&cx, &dir, engine_keys())
            .await
            .expect("reopen after inventory oracle releases writer lease");
        assert_summary(
            &database.scrub(&cx).await.expect("clean scrub"),
            &ids,
            &[],
            &[],
        );
        let target = capsule_disk_path(&dir, ids[1]);
        let original = std::fs::read(&target).expect("original capsule");
        let damaged = flip_distinct_symbols(&original, ids[1], 1, 0xbe01);
        std::fs::write(&target, damaged).expect("plant payload corruption while handle is open");
        let clean = [ids[0], ids[2], ids[3]];
        assert_summary(
            &database.scrub(&cx).await.expect("repair scrub"),
            &clean,
            &[ids[1]],
            &[],
        );
        // Negative-control insertion target: resealing under any different nonce must fail here.
        assert_eq!(
            std::fs::read(&target).expect("repaired capsule"),
            original,
            "canonical container byte identity, not just recovered plaintext"
        );
        assert_summary(
            &database.scrub(&cx).await.expect("second scrub"),
            &ids,
            &[],
            &[],
        );
        assert_eq!(answers(&database), before);
        drop(database);
        let reopened = Database::open(&cx, &dir, engine_keys())
            .await
            .expect("cold reopen");
        assert_eq!(answers(&reopened), before);
    });
    assert!(report.lab_test_passed(), "lab failed: {report:?}");
}

#[test]
fn scrub_repairs_missing_repair_symbol_inventory_after_truncation() {
    let dir = scratch("missing-symbols");
    let ((), report) = run_async_under_lab(0xbe02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut database = Database::create(&cx, &dir, engine_keys())
            .await
            .expect("create");
        commit_four(&mut database, &cx).await;
        let before = answers(&database);
        drop(database);
        let ids = committed_ids(&cx, &dir).await;
        let mut database = Database::open(&cx, &dir, engine_keys())
            .await
            .expect("reopen before planting truncation");
        let target = capsule_disk_path(&dir, ids[2]);
        let original = std::fs::read(&target).expect("original capsule");
        let (descriptor, symbols) = decode_container(&original).expect("decode original");
        let missing = CapsuleProfile::balanced().erasure_budget();
        assert_eq!(missing, descriptor.repair_symbols as usize);
        let retained = symbols.len() - missing;
        let end = CAPSULE_HEADER_BYTES_V1
            + symbols[..retained]
                .iter()
                .map(|symbol| 4 + symbol.len())
                .sum::<usize>();
        let truncated = &original[..end];
        assert_eq!(
            decode_container(truncated)
                .expect("truncated inventory remains parseable")
                .1
                .len(),
            retained
        );
        std::fs::write(&target, truncated)
            .expect("drop all repair symbols but retain original declared inventory");
        assert_summary(
            &database.scrub(&cx).await.expect("repair missing symbols"),
            &[ids[0], ids[1], ids[3]],
            &[ids[2]],
            &[],
        );
        assert_eq!(
            std::fs::read(&target).expect("restored inventory"),
            original
        );
        assert_summary(
            &database.scrub(&cx).await.expect("second scrub"),
            &ids,
            &[],
            &[],
        );
        drop(database);
        let reopened = Database::open(&cx, &dir, engine_keys())
            .await
            .expect("cold reopen");
        assert_eq!(answers(&reopened), before);
    });
    assert!(report.lab_test_passed(), "lab failed: {report:?}");
}

async fn lost_control(cx: &CommitCx, dir: &Path, missing_file: bool) {
    let mut database = Database::create(cx, dir, engine_keys())
        .await
        .expect("create");
    commit_four(&mut database, cx).await;
    let before = answers(&database);
    drop(database);
    let ids = committed_ids(cx, dir).await;
    let mut database = Database::open(cx, dir, engine_keys())
        .await
        .expect("reopen before planting loss");
    let target = capsule_disk_path(dir, ids[3]);
    let original = std::fs::read(&target).expect("original capsule");
    let ruined = if missing_file {
        // Explicit bead control: only this test's unique scratch capsule is deleted.
        std::fs::remove_file(&target).expect("delete one marker-reachable capsule");
        None
    } else {
        let count = CapsuleProfile::balanced().erasure_budget() + 1;
        let bytes = flip_distinct_symbols(&original, ids[3], count, 0xbe03);
        std::fs::write(&target, &bytes).expect("damage beyond the erasure budget");
        Some(bytes)
    };
    // Do not reopen before scrub: open already reads every capsule to rebuild the delta index.
    let summary = database
        .scrub(cx)
        .await
        .expect("loss is a typed verdict, not a scrub I/O refusal");
    assert_summary(&summary, &ids[..3], &[], &[ids[3]]);
    assert_eq!(summary.lost[0].reason, LostReason::InsufficientSymbols);
    if let Some(bytes) = &ruined {
        assert_eq!(
            &std::fs::read(&target).expect("lost bytes retained"),
            bytes,
            "never rewrite beyond-budget evidence"
        );
    } else {
        assert!(
            !target.exists(),
            "scrub must not recreate a missing object from invented data"
        );
    }
    for snapshot in &before {
        assert!(
            matches!(
                database.edges_at(snapshot.at),
                Err(ReadError::RecoveryRequired(_))
            ),
            "native reads fail closed at every retained snapshot"
        );
        assert!(
            matches!(
                database.execute_gql_at(PINNED, &bind_r(), snapshot.at),
                Err(GqlError::Read(ReadError::RecoveryRequired(_)))
            ),
            "GQL must not return cached partial rows"
        );
    }
    drop(database);
    assert!(
        matches!(
            Database::open(cx, dir, engine_keys()).await,
            Err(OpenError::Rebuild(_))
        ),
        "cold open refuses the unrecoverable committed capsule with a typed rebuild error"
    );
    if let Some(bytes) = ruined {
        assert_eq!(
            std::fs::read(target).expect("evidence after refusal"),
            bytes
        );
    } else {
        assert!(!target.exists());
    }
}

#[test]
fn scrub_reports_deleted_marker_reachable_capsule_and_fences_reads() {
    let dir = scratch("deleted-capsule");
    let ((), report) = run_async_under_lab(0xbe03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        lost_control(&contexts.commit(), &dir, true).await;
    });
    assert!(report.lab_test_passed(), "lab failed: {report:?}");
}

#[test]
fn scrub_beyond_budget_preserves_ruined_bytes_and_fences_reads() {
    let dir = scratch("beyond-budget");
    let ((), report) = run_async_under_lab(0xbe04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        lost_control(&contexts.commit(), &dir, false).await;
    });
    assert!(report.lab_test_passed(), "lab failed: {report:?}");
}

#[test]
fn seeded_fault_matrix_repairs_every_commit_and_preserves_all_retained_answers() {
    for (seed, count) in [(0xbe11, 1), (0xbe12, 2), (0xbe13, 3)] {
        let dir = scratch(&format!("seeded-{seed:x}"));
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = FaultVfs::unix(FaultPlan {
                seed,
                ..FaultPlan::faultless()
            });
            let mut database = Database::create_with_vfs(&cx, vfs.clone(), &dir, engine_keys())
                .await
                .expect("create through FaultVfs");
            commit_four(&mut database, &cx).await;
            let before = answers(&database);
            drop(database);
            let ids = committed_ids(&cx, &dir).await;
            let mut database = Database::open_with_vfs(&cx, vfs.clone(), &dir, engine_keys())
                .await
                .expect("reopen same fault model after inventory oracle");
            assert!(count <= CapsuleProfile::balanced().erasure_budget());
            let mut originals = Vec::new();
            for (index, oid) in ids.iter().copied().enumerate() {
                let path = capsule_disk_path(&dir, oid);
                let original = std::fs::read(&path).expect("original capsule");
                let damaged = flip_distinct_symbols(&original, oid, count, seed + index as u64);
                vfs.write(&path, &damaged)
                    .await
                    .expect("seeded distinct-symbol corruption across all commits");
                originals.push((path, original));
            }
            assert_summary(
                &database.scrub(&cx).await.expect("scrub through FaultVfs"),
                &[],
                &ids,
                &[],
            );
            for (path, original) in &originals {
                assert_eq!(
                    &std::fs::read(path).expect("canonical repaired container"),
                    original
                );
            }
            assert_summary(
                &database.scrub(&cx).await.expect("second scrub"),
                &ids,
                &[],
                &[],
            );
            assert_eq!(answers(&database), before);
            drop(database);
            vfs.crash().await.expect("discard every volatile handle");
            let reopened = Database::open(&cx, &dir, engine_keys())
                .await
                .expect("cold reopen after modeled process loss");
            assert_eq!(
                answers(&reopened),
                before,
                "seed {seed:x}: all retained GQL/native answers"
            );
        });
        assert!(
            report.lab_test_passed(),
            "seed {seed:x}: lab failed: {report:?}"
        );
    }
}

#[test]
fn every_repair_io_crash_preserves_recoverable_old_or_identical_repaired_container() {
    let points = [
        ScrubCrashPoint::AfterTempCreate,
        ScrubCrashPoint::AfterTempWrite,
        ScrubCrashPoint::AfterTempFlush,
        ScrubCrashPoint::AfterTempFileSync,
        ScrubCrashPoint::AfterRename,
        ScrubCrashPoint::AfterDirectorySync,
    ];
    // Never is essential: if rename happens to survive, skipping the temp file
    // fsync must expose an empty inode, not be hidden by rolling the rename back.
    for (loss_index, dirent_loss) in [Trigger::Never, Trigger::Always].into_iter().enumerate() {
        for (point_index, point) in points.into_iter().enumerate() {
            let seed = 0xbe20 + (loss_index * points.len() + point_index) as u64;
            let dir = scratch(&format!("crash-{loss_index}-{point_index}"));
            let ((), report) = run_async_under_lab(seed, move |root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let cx = contexts.commit();
                let vfs = FaultVfs::unix(FaultPlan {
                    seed,
                    dirent_loss,
                    ..FaultPlan::faultless()
                });
                let mut database = Database::create_with_vfs(&cx, vfs.clone(), &dir, engine_keys())
                    .await
                    .expect("create through crash model");
                commit_four(&mut database, &cx).await;
                let before = answers(&database);
                drop(database);
                let ids = committed_ids(&cx, &dir).await;
                let mut database = Database::open_with_vfs(&cx, vfs.clone(), &dir, engine_keys())
                    .await
                    .expect("reopen same crash model before planting damage");
                let originals: Vec<_> = ids
                    .iter()
                    .copied()
                    .map(|oid| {
                        let path = capsule_disk_path(&dir, oid);
                        let bytes = std::fs::read(&path).expect("original capsule");
                        (path, bytes)
                    })
                    .collect();
                let (target, original) = &originals[1];
                let damaged = flip_distinct_symbols(original, ids[1], 2, seed);
                vfs.write(target, &damaged)
                    .await
                    .expect("plant durable recoverable old image through the inode cache");
                let result = database.scrub_with_crash(&cx, Some(point)).await;
                assert!(
                    matches!(&result, Err(CommitError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted),
                    "production repair must reach {point:?}: {result:?}"
                );
                drop(database);
                vfs.crash()
                    .await
                    .expect("real modeled crash at repair boundary");
                let survived = std::fs::read(target).expect("committed capsule name survives");
                // Negative-control insertion target: remove temp sync before rename.
                // AfterRename + dirent_loss Never then leaves neither legal image.
                assert!(
                    survived == damaged || survived == *original,
                    "{point:?}, loss={dirent_loss:?}: neither old recoverable nor exact canonical bytes survived"
                );
                for (index, (path, bytes)) in originals.iter().enumerate() {
                    if index != 1 {
                        assert_eq!(&std::fs::read(path).expect("untouched capsule"), bytes);
                    }
                }
                let mut reopened = Database::open(&cx, &dir, engine_keys())
                    .await
                    .expect("cold reopen must decode whichever image survived");
                assert_eq!(
                    answers(&reopened),
                    before,
                    "all retained answers after {point:?}"
                );
                let summary = reopened
                    .scrub(&cx)
                    .await
                    .expect("resume repair after crash");
                if survived == damaged {
                    assert_summary(&summary, &[ids[0], ids[2], ids[3]], &[ids[1]], &[]);
                } else {
                    assert_summary(&summary, &ids, &[], &[]);
                }
                assert_eq!(
                    &std::fs::read(target).expect("canonical after resumed repair"),
                    original
                );
                assert_summary(
                    &reopened.scrub(&cx).await.expect("resumed second scrub"),
                    &ids,
                    &[],
                    &[],
                );
            });
            assert!(
                report.lab_test_passed(),
                "{point:?}, loss={dirent_loss:?}: lab failed: {report:?}"
            );
        }
    }
}
