//! `scrub`: verify every capsule the published history names, restore damaged
//! RaptorQ redundancy in place (object, ciphertext and encoding identity are
//! unchanged), and re-read the admitted Strata generation's blocks, vertex
//! patches and publication metadata. Strata objects carry no local redundancy,
//! so a damaged object is reported, never repaired.
//!
//! Output: one `scrub` record per damaged object, then either a `scrubbed`
//! result (nothing lost, exit 0) or, like fsck, an io-class error after the
//! records when anything is lost (exit 5). Lost objects are never overwritten.
//!
//! Opening authenticates the admitted root closure. Checkpoint open can defer
//! capsule history, which scrub verifies in full. The loss records also cover
//! damage to the admitted generation first discovered during this sweep.
use super::{Failure, emit, execution_failure, hex};
use asupersync::fs::Vfs;
use fgdb::{Database, LostCapsule, ScrubSummary};
use fgdb_chronicle::scrub::LostReason;
use fgdb_types::{CommitCx, ObjectId};
use std::io::Write;

pub(super) async fn run<V: Vfs + Clone>(
    db: &mut Database<V>,
    cx: &CommitCx,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    // Scrub fences the handle when it discovers any lost object. Capture its
    // admitted sequence first so that the fence cannot suppress loss evidence.
    let seq = db.frontier().map_err(Failure::io)?.0;
    let summary = db.scrub(cx).await.map_err(execution_failure)?;
    report(&summary, seq, robot, out)
}

/// Every damaged object is reported before the verdict, so a loss still
/// delivers the evidence even though it ends in an error rather than a result.
fn report(
    summary: &ScrubSummary,
    seq: u64,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    for id in &summary.repaired {
        record(out, robot, "capsule", id, "repaired", None)?;
    }
    for lost in &summary.lost {
        lost_record(out, robot, "capsule", lost)?;
    }
    for lost in &summary.block_lost {
        lost_record(out, robot, "block", lost)?;
    }
    for lost in &summary.metadata_lost {
        lost_record(out, robot, "metadata", lost)?;
    }
    let lost = summary.lost.len() + summary.block_lost.len() + summary.metadata_lost.len();
    if lost > 0 {
        return Err(Failure::io(format!(
            "scrub found {lost} lost object(s); they were reported and left untouched"
        )));
    }
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"scrubbed","seq":{seq},"objects":{},"repaired":{},"block_objects":{},"metadata_objects":{}}}"#,
                summary.objects,
                summary.repaired.len(),
                summary.block_objects,
                summary.metadata_objects
            ),
        )
    } else {
        writeln!(
            out,
            "scrubbed (seq {seq}): {} capsule(s) verified, {} repaired; {} data block(s) and {} metadata object(s) verified",
            summary.objects,
            summary.repaired.len(),
            summary.block_objects,
            summary.metadata_objects
        )
        .map_err(Failure::io)
    }
}

fn lost_record(
    out: &mut impl Write,
    robot: bool,
    kind: &str,
    lost: &LostCapsule,
) -> Result<(), Failure> {
    let reason = match lost.reason {
        LostReason::InsufficientSymbols => "insufficient_symbols",
        LostReason::AuthenticationFailed => "authentication_failed",
        LostReason::IdentityMismatch => "identity_mismatch",
        LostReason::ConflictingSymbols => "conflicting_symbols",
        LostReason::Unusable => "unusable",
    };
    record(out, robot, kind, &lost.object_id, "lost", Some(reason))
}

fn record(
    out: &mut impl Write,
    robot: bool,
    kind: &str,
    id: &ObjectId,
    state: &str,
    reason: Option<&str>,
) -> Result<(), Failure> {
    let object = hex(&id.0);
    if robot {
        let reason = reason.map_or(String::new(), |reason| format!(r#","reason":"{reason}""#));
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"scrub","object":"{object}","kind":"{kind}","state":"{state}"{reason}}}"#
            ),
        )
    } else {
        let reason = reason.map_or(String::new(), |reason| format!(" ({reason})"));
        writeln!(out, "{state} {kind} {object}{reason}").map_err(Failure::io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId([byte; 32])
    }

    fn lost(byte: u8, reason: LostReason) -> LostCapsule {
        LostCapsule {
            object_id: id(byte),
            reason,
        }
    }

    #[test]
    fn a_loss_reports_every_damaged_object_then_refuses_without_a_result() {
        let summary = ScrubSummary {
            objects: 3,
            clean: vec![id(1)],
            repaired: vec![id(2)],
            lost: vec![lost(3, LostReason::InsufficientSymbols)],
            block_objects: 2,
            block_clean: vec![id(4)],
            block_lost: vec![lost(5, LostReason::IdentityMismatch)],
            metadata_objects: 2,
            metadata_clean: vec![id(6)],
            metadata_lost: vec![lost(7, LostReason::Unusable)],
        };
        let mut out = Vec::new();
        let Err(error) = report(&summary, 7, true, &mut out) else {
            panic!("a loss must not end in a result");
        };
        assert_eq!((error.code, error.class), (5, "io"));
        assert!(
            error.message.contains("3 lost object(s)"),
            "{}",
            error.message
        );
        let text = String::from_utf8(out).unwrap();
        let expected = [
            format!(
                r#"{{"v":1,"event":"scrub","object":"{}","kind":"capsule","state":"repaired"}}"#,
                "02".repeat(32)
            ),
            format!(
                r#"{{"v":1,"event":"scrub","object":"{}","kind":"capsule","state":"lost","reason":"insufficient_symbols"}}"#,
                "03".repeat(32)
            ),
            format!(
                r#"{{"v":1,"event":"scrub","object":"{}","kind":"block","state":"lost","reason":"identity_mismatch"}}"#,
                "05".repeat(32)
            ),
            format!(
                r#"{{"v":1,"event":"scrub","object":"{}","kind":"metadata","state":"lost","reason":"unusable"}}"#,
                "07".repeat(32)
            ),
        ];
        assert_eq!(text.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn a_repair_without_loss_ends_in_exactly_one_result() {
        let summary = ScrubSummary {
            objects: 2,
            clean: vec![id(1)],
            repaired: vec![id(2)],
            block_objects: 1,
            block_clean: vec![id(4)],
            metadata_objects: 2,
            metadata_clean: vec![id(6), id(7)],
            ..ScrubSummary::default()
        };
        let mut out = Vec::new();
        report(&summary, 9, true, &mut out).unwrap_or_else(|error| panic!("{}", error.message));
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(
            lines[1],
            r#"{"v":1,"event":"result","kind":"scrubbed","seq":9,"objects":2,"repaired":1,"block_objects":1,"metadata_objects":2}"#
        );
        let mut human = Vec::new();
        report(&summary, 9, false, &mut human).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            String::from_utf8(human).unwrap(),
            format!(
                "repaired capsule {}\nscrubbed (seq 9): 2 capsule(s) verified, 1 repaired; 1 data block(s) and 2 metadata object(s) verified\n",
                "02".repeat(32)
            )
        );
    }

    #[test]
    fn run_delivers_loss_evidence_even_after_scrub_fences_the_database() {
        use asupersync::lab::run_async_under_lab;
        use fgdb::{DatabaseKeys, MemVfs, ReadError, WriteBatch};
        use fgdb_delta_types::RelationId;
        use fgdb_strata::store::BlockStore;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};

        for metadata in [false, true] {
            let ((), report) = run_async_under_lab(0x5c13, move |root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let vfs = MemVfs::new().unwrap();
                let path = vfs.database_dir();
                let namespace = DatabaseSecurityNamespaceId([0x31; 32]);
                let mut db = Database::create_with_vfs(
                    &commit,
                    vfs.clone(),
                    &path,
                    DatabaseKeys::new([0x32; 32], namespace, [0x33; 32]),
                )
                .await
                .unwrap();
                let mut batch = WriteBatch::new(RelationId(1));
                batch.create_vertex(VId(1), Vec::new(), Vec::new());
                db.write(&commit, batch).await.unwrap();
                let store =
                    BlockStore::open_with_vfs(&commit, vfs.clone(), &path, [0x32; 32], namespace)
                        .await
                        .unwrap();
                let target = if metadata {
                    db.manifest().unwrap().0
                } else {
                    store
                        .resolve_manifest(&commit, db.manifest().unwrap())
                        .await
                        .unwrap()[0]
                        .1
                        .vertex_patches[0]
                        .patch_id
                };
                let object = store.path(target);
                let mut bytes = vfs.read(&object).await.unwrap();
                bytes[0] ^= 1;
                vfs.write(&object, &bytes).await.unwrap();
                let mut out = Vec::new();
                let error = run(&mut db, &commit, true, &mut out)
                    .await
                    .expect_err("loss must fail after delivering the object record");
                assert_eq!((error.code, error.class), (5, "io"));
                assert!(error.message.contains("1 lost object(s)"));
                let text = String::from_utf8(out).unwrap();
                let kind = if metadata { "metadata" } else { "block" };
                assert_eq!(
                    text.trim_end(),
                    format!(
                        r#"{{"v":1,"event":"scrub","object":"{}","kind":"{kind}","state":"lost","reason":"identity_mismatch"}}"#,
                        hex(&target.0),
                    )
                );
                assert!(matches!(db.frontier(), Err(ReadError::RecoveryRequired(_))));
                assert_eq!(
                    vfs.read(&object).await.unwrap(),
                    bytes,
                    "loss is never repaired from cache"
                );
            });
            assert!(report.lab_test_passed(), "{report:?}");
        }
    }
}
