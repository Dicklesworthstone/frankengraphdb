//! Read the current authenticated branch write precondition without opening a writer.

use super::{Failure, emit, hex, read_keys, revision_open_failure};
use asupersync::Budget;
use fgdb::{Database, PublishedBranchHead};
use fgdb_types::PurposeContexts;
use std::io::Write;
use std::path::Path;

pub(crate) fn run(args: &[String], robot: bool, out: &mut impl Write) -> Result<(), Failure> {
    let mut db = None;
    let mut key = None;
    let mut passphrase = None;
    let mut strata_ecs = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--db" if db.is_none() => &mut db,
            "--key-file" if key.is_none() => &mut key,
            "--kek-passphrase-file" if passphrase.is_none() => &mut passphrase,
            "--strata-ecs" if !strata_ecs => {
                strata_ecs = true;
                continue;
            }
            _ => {
                return Err(Failure::usage(format!(
                    "unknown or repeated branch-head option {flag}"
                )));
            }
        };
        let value = args
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Failure::usage(format!("{flag} requires a value")))?;
        *slot = Some(Path::new(value));
    }
    let db = db.ok_or_else(|| Failure::usage("branch-head requires --db"))?;
    let key = key.ok_or_else(|| Failure::usage("branch-head requires --key-file"))?;
    let runtime = fgdb::runtime_builder().build().map_err(Failure::io)?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let head = runtime.block_on(async {
        let keys = read_keys(&contexts.query(), key, passphrase, strata_ecs).await?;
        Database::open_current_branch_head(&contexts.commit(), db, keys)
            .await
            .map_err(revision_open_failure)
    })?;
    match head {
        PublishedBranchHead::Empty if robot => emit(
            out,
            r#"{"v":1,"event":"result","kind":"branch_head","seq":0}"#,
        ),
        PublishedBranchHead::Empty => writeln!(out, "empty").map_err(Failure::io),
        PublishedBranchHead::Recorded(marker) => {
            let seq = marker.commit_seq.0;
            let hash = hex(&marker.marker_oid.0);
            if robot {
                emit(
                    out,
                    &format!(
                        r#"{{"v":1,"event":"result","kind":"branch_head","seq":{seq},"chain_hash":"{hash}"}}"#
                    ),
                )
            } else {
                writeln!(out, "{seq}@{hash}").map_err(Failure::io)
            }
        }
        PublishedBranchHead::Unrecorded { frontier } => Err(Failure::coded(
            crate::refusal::Code::Conflict,
            format!(
                "published commit {} has no recorded branch head",
                frontier.0
            ),
        )),
    }
}
