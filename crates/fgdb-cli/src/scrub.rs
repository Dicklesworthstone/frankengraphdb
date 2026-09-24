//! `scrub`: verify every capsule the published history names, restore damaged
//! RaptorQ redundancy in place (object, ciphertext and encoding identity are
//! unchanged), and re-read every published Strata block. Blocks carry no
//! local redundancy, so a damaged block is reported, never repaired.
//!
//! Output: one `scrub` record per damaged object, then either a `scrubbed`
//! result (nothing lost, exit 0) or, like fsck, an io-class error after the
//! records when anything is lost (exit 5). Lost objects are never overwritten.
//!
//! Opening the database already folds every capsule and loads every root
//! block, so damage beyond repair that exists at startup is refused at open,
//! naming the object, before scrub runs. The lost records here cover damage
//! found by the scrub pass itself.
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
    let summary = db.scrub(cx).await.map_err(execution_failure)?;
    let seq = db.frontier().map_err(Failure::io)?.0;
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
    let lost = summary.lost.len() + summary.block_lost.len();
    if lost > 0 {
        return Err(Failure::io(format!(
            "scrub found {lost} lost object(s); they were reported and left untouched"
        )));
    }
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"scrubbed","seq":{seq},"objects":{},"repaired":{},"block_objects":{}}}"#,
                summary.objects,
                summary.repaired.len(),
                summary.block_objects
            ),
        )
    } else {
        writeln!(
            out,
            "scrubbed (seq {seq}): {} capsule(s) verified, {} repaired; {} block(s) verified",
            summary.objects,
            summary.repaired.len(),
            summary.block_objects
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
        };
        let mut out = Vec::new();
        let Err(error) = report(&summary, 7, true, &mut out) else {
            panic!("a loss must not end in a result");
        };
        assert_eq!((error.code, error.class), (5, "io"));
        assert!(
            error.message.contains("2 lost object(s)"),
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
            ..ScrubSummary::default()
        };
        let mut out = Vec::new();
        report(&summary, 9, true, &mut out).unwrap_or_else(|error| panic!("{}", error.message));
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(
            lines[1],
            r#"{"v":1,"event":"result","kind":"scrubbed","seq":9,"objects":2,"repaired":1,"block_objects":1}"#
        );
        let mut human = Vec::new();
        report(&summary, 9, false, &mut human).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            String::from_utf8(human).unwrap(),
            format!(
                "repaired capsule {}\nscrubbed (seq 9): 2 capsule(s) verified, 1 repaired; 1 block(s) verified\n",
                "02".repeat(32)
            )
        );
    }
}
