//! Verify a Chronicle recovery cut without opening a writer or repairing storage.

pub(super) mod head;

use super::{Failure, emit, hex, read_keys};
use asupersync::Budget;
use asupersync::fs::UnixVfs;
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_types::PurposeContexts;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::Path;

/// A local write precondition checked under the database's exclusive writer lease.
pub(super) enum BranchGuard {
    Empty,
    At(fgdb_types::MarkerRef),
}

impl BranchGuard {
    pub(super) fn parse(raw: &str) -> Result<Self, Failure> {
        if raw == "empty" {
            return Ok(Self::Empty);
        }
        Ok(Self::At(parse_marker(
            raw,
            "--if-head requires empty or <positive seq>@<64 hexadecimal digits>",
        )?))
    }

    pub(super) fn check<V: asupersync::fs::Vfs + Clone>(
        &self,
        db: &fgdb::Database<V>,
    ) -> Result<(), Failure> {
        let expected = match self {
            Self::Empty => None,
            Self::At(marker) => Some(*marker),
        };
        db.check_branch_head(expected).map_err(|error| {
            let conflict = match &error {
                fgdb::WriteError::BranchHeadUnrecorded { .. } => true,
                fgdb::WriteError::Commit(error) => matches!(
                    error.as_ref(),
                    fgdb_chronicle::CommitError::Chain(
                        fgdb_chronicle::ChainError::HeadCasMismatch(_)
                    )
                ),
                _ => false,
            };
            if conflict {
                Failure::coded(
                    super::refusal::Code::Conflict,
                    "the expected branch head is not current",
                )
            } else {
                super::execution_failure(error)
            }
        })
    }
}

pub(super) fn parse_marker(raw: &str, requirement: &str) -> Result<fgdb_types::MarkerRef, Failure> {
    let invalid = || Failure::usage(requirement);
    let (seq, hash) = raw.split_once('@').ok_or_else(invalid)?;
    let seq = super::symbol_ordinal(seq)
        .filter(|&seq| seq != 0)
        .ok_or_else(invalid)?;
    let oid = parse_chain_hash(hash).ok_or_else(invalid)?.0;
    Ok(fgdb_types::MarkerRef {
        commit_seq: fgdb_types::CommitSeq(seq),
        marker_oid: fgdb_types::ObjectId(oid),
    })
}

fn revision_open_failure(error: fgdb::BranchRevisionOpenError) -> Failure {
    match error {
        fgdb::BranchRevisionOpenError::Open(error) => super::open_failure(error),
        fgdb::BranchRevisionOpenError::Catalog(error) => super::execution_failure(error),
        fgdb::BranchRevisionOpenError::Unpublished { .. } => Failure::open(error),
    }
}

fn parse_chain_hash(raw: &str) -> Option<fgdb_crypto::Digest> {
    if raw.len() != 64 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 32];
    for (at, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&raw[2 * at..2 * at + 2], 16).ok()?;
    }
    Some(fgdb_crypto::Digest(bytes))
}

pub(super) async fn query_revision(
    contexts: &PurposeContexts,
    options: &mut super::Options,
    keys: fgdb::DatabaseKeys,
    marker: fgdb_types::MarkerRef,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let revision =
        fgdb::Database::open_branch_revision(&contexts.commit(), &options.db, keys, marker)
            .await
            .map_err(revision_open_failure)?
            .ok_or_else(|| {
                Failure::coded(
                    super::refusal::Code::NotFound,
                    "unknown recorded branch revision",
                )
            })?;
    super::symbols::merge(options, revision.symbol_catalog())?;
    let result = revision
        .query_with_resolver(
            &contexts.query(),
            &options.text,
            &options.params,
            &*options,
            options.budget.policy(),
        )
        .map_err(|error| super::read_failure(options, error))?;
    super::render(result, marker.commit_seq.0, "rows", robot, out)
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Command {
    VerifyHistory,
    BackupHistory,
    ListCapsules,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Self::BackupHistory => "backup-history",
            Self::VerifyHistory => "verify-history",
            Self::ListCapsules => "list-capsules",
        }
    }
}

enum Source<'a> {
    Verified {
        backup: Option<(&'a Path, fgdb_crypto::Digest)>,
        key: &'a Path,
        passphrase: Option<&'a Path>,
    },
    List,
}

struct Options<'a> {
    db: &'a Path,
    source: Source<'a>,
    through: NonZeroU64,
    expected: Option<fgdb_crypto::Digest>,
}

fn parse(command: Command, args: &[String]) -> Result<Options<'_>, Failure> {
    let name = command.name();
    let mut db = None;
    let mut key = None;
    let mut passphrase = None;
    let mut output = None;
    let mut through = None;
    let mut expected = None;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Failure::usage(format!("{flag} requires a value")))?;
        match flag.as_str() {
            "--db" if db.is_none() => db = Some(Path::new(value)),
            "--key-file" if command != Command::ListCapsules && key.is_none() => {
                key = Some(Path::new(value))
            }
            "--kek-passphrase-file" if command != Command::ListCapsules && passphrase.is_none() => {
                passphrase = Some(Path::new(value))
            }
            "--output" if command == Command::BackupHistory && output.is_none() => {
                output = Some(Path::new(value));
            }
            "--through" if through.is_none() => {
                if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(Failure::usage(
                        "--through requires a positive u64 commit sequence",
                    ));
                }
                through = Some(value.parse::<NonZeroU64>().map_err(|_| {
                    Failure::usage("--through requires a positive u64 commit sequence")
                })?);
            }
            "--expect-chain" if expected.is_none() => {
                expected = Some(parse_chain_hash(value).ok_or_else(|| {
                    Failure::usage("--expect-chain requires 64 hexadecimal characters")
                })?);
            }
            _ => {
                return Err(Failure::usage(format!(
                    "unknown or repeated {name} option {flag}"
                )));
            }
        }
    }
    Ok(Options {
        db: db.ok_or_else(|| Failure::usage(format!("{name} requires --db")))?,
        source: match command {
            Command::VerifyHistory | Command::BackupHistory => Source::Verified {
                backup: if command == Command::BackupHistory {
                    Some((
                        output.ok_or_else(|| Failure::usage("backup-history requires --output"))?,
                        expected.ok_or_else(|| {
                            Failure::usage("backup-history requires --expect-chain")
                        })?,
                    ))
                } else {
                    None
                },
                key: key.ok_or_else(|| Failure::usage(format!("{name} requires --key-file")))?,
                passphrase,
            },
            Command::ListCapsules => Source::List,
        },
        through: through.ok_or_else(|| Failure::usage(format!("{name} requires --through")))?,
        expected,
    })
}

pub(super) fn run(
    command: Command,
    args: &[String],
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let options = parse(command, args)?;
    let runtime = fgdb::runtime_builder().build().map_err(Failure::io)?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let hash =
        runtime.block_on(async {
            let cx = contexts.commit();
            let vfs = UnixVfs::new();
            match options.source {
                Source::Verified {
                    key,
                    passphrase,
                    backup,
                } => {
                    let keys = read_keys(&contexts.query(), key, passphrase, false).await?;
                    let capsule_keys = keys.capsule_keys();
                    if let Some((destination, expected)) = backup {
                        CommitCoordinator::backup_history_through(
                            &cx,
                            &vfs,
                            options.db,
                            destination,
                            &capsule_keys,
                            (options.through, expected),
                            fgdb::template_digest,
                        )
                        .await
                    } else {
                        CommitCoordinator::verify_history_through(
                            &cx,
                            &vfs,
                            options.db,
                            &capsule_keys,
                            options.through,
                            fgdb::template_digest,
                        )
                        .await
                    }
                }
                Source::List => CommitCoordinator::visit_capsule_refs_through(
                    &cx,
                    &vfs,
                    options.db,
                    options.through,
                    |seq, id| {
                        let seq = seq.0;
                        let object = hex(&id.0);
                        if robot {
                            writeln!(
                                out,
                                r#"{{"v":1,"event":"capsule_ref","seq":{seq},"object":"{object}"}}"#
                            )
                        } else {
                            writeln!(out, "{seq} {object}")
                        }
                    },
                )
                .await,
            }
            .map_err(Failure::io)
        })?;
    let seq = options.through;
    if options.expected.is_some_and(|expected| expected != hash) {
        return Err(Failure::io(format!(
            "marker chain through commit {seq} differs from --expect-chain"
        )));
    }
    let hash = hex(&hash.0);
    let (kind, description) = match command {
        Command::BackupHistory => ("history_backup", "backed up marker history and capsules"),
        Command::VerifyHistory => ("verified_history", "verified marker history and capsules"),
        Command::ListCapsules => ("capsule_inventory", "listed marker capsule references"),
    };
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"{kind}","seq":{seq},"chain_hash":"{hash}"}}"#
            ),
        )
    } else {
        writeln!(out, "{description} through commit {seq}, chain {hash}").map_err(Failure::io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_requires_a_new_destination_and_exact_chain_commitment() {
        let mut args = ["--db", "db", "--key-file", "keys", "--through", "3"]
            .map(str::to_owned)
            .to_vec();
        assert!(parse(Command::BackupHistory, &args).is_err());
        args.extend(["--output", "archive"].map(str::to_owned));
        assert!(parse(Command::BackupHistory, &args).is_err());
        args.extend(["--expect-chain".to_owned(), "AB".repeat(32)]);
        assert!(
            matches!(parse(Command::BackupHistory, &args).unwrap().source,
            Source::Verified { backup: Some((path, hash)), .. } if path == Path::new("archive") && hash.0 == [0xab; 32])
        );
        assert!(parse(Command::VerifyHistory, &args).is_err());
        assert!(parse(Command::ListCapsules, &args).is_err());
        args.extend(["--output", "second"].map(str::to_owned));
        assert!(parse(Command::BackupHistory, &args).is_err());
    }

    #[test]
    fn require_exact_nonempty_cut_and_unambiguous_anchor() {
        let args = ["--db", "db", "--key-file", "keys", "--through", "1"].map(str::to_owned);
        assert!(parse(Command::VerifyHistory, &args).is_ok());
        for invalid in ["0", "-1", "+1", "18446744073709551616"] {
            let mut bad = args.clone();
            bad[5] = invalid.to_owned();
            assert!(parse(Command::VerifyHistory, &bad).is_err());
        }
        assert!(parse(Command::VerifyHistory, &args[..4]).is_err());
        for extra in [
            ["--through", "1"],
            ["--expect-chain", "abc"],
            ["--overwrite", "yes"],
        ] {
            let mut bad = args.to_vec();
            bad.extend(extra.map(str::to_owned));
            assert!(parse(Command::VerifyHistory, &bad).is_err());
        }
    }
}
