//! Verify a Chronicle recovery cut without opening a writer or repairing storage.

use super::{Failure, emit, hex, read_keys};
use asupersync::Budget;
use asupersync::fs::UnixVfs;
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_types::PurposeContexts;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::Path;

struct Options<'a> {
    db: &'a Path,
    key: &'a Path,
    passphrase: Option<&'a Path>,
    through: NonZeroU64,
    expected: Option<&'a str>,
}

fn parse(args: &[String]) -> Result<Options<'_>, Failure> {
    let mut db = None;
    let mut key = None;
    let mut passphrase = None;
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
            "--key-file" if key.is_none() => key = Some(Path::new(value)),
            "--kek-passphrase-file" if passphrase.is_none() => passphrase = Some(Path::new(value)),
            "--through" if through.is_none() => {
                if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(Failure::usage("--through requires a positive u64 commit sequence"));
                }
                through = Some(value.parse::<NonZeroU64>().map_err(|_| {
                    Failure::usage("--through requires a positive u64 commit sequence")
                })?);
            }
            "--expect-chain" if expected.is_none() => {
                if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(Failure::usage("--expect-chain requires 64 hexadecimal characters"));
                }
                expected = Some(value.as_str());
            }
            _ => return Err(Failure::usage(format!("unknown or repeated verify-history option {flag}"))),
        }
    }
    Ok(Options {
        db: db.ok_or_else(|| Failure::usage("verify-history requires --db"))?,
        key: key.ok_or_else(|| Failure::usage("verify-history requires --key-file"))?,
        passphrase,
        through: through.ok_or_else(|| Failure::usage("verify-history requires --through"))?,
        expected,
    })
}

pub(super) fn run(args: &[String], robot: bool, out: &mut impl Write) -> Result<(), Failure> {
    let options = parse(args)?;
    let runtime = fgdb::runtime_builder().build().map_err(Failure::io)?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let hash = runtime.block_on(async {
        let keys = read_keys(&contexts.query(), options.key, options.passphrase, false).await?;
        CommitCoordinator::verify_history_through(
            &contexts.commit(),
            &UnixVfs::new(),
            options.db,
            &keys.capsule_keys(),
            options.through,
            fgdb::template_digest,
        )
        .await
        .map_err(Failure::io)
    })?;
    let hash = hex(&hash.0);
    let seq = options.through;
    if options.expected.is_some_and(|expected| !expected.eq_ignore_ascii_case(&hash)) {
        return Err(Failure::io(format!(
            "verified history through commit {seq} differs from --expect-chain"
        )));
    }
    if robot {
        emit(out, &format!(
            r#"{{"v":1,"event":"result","kind":"verified_history","seq":{seq},"chain_hash":"{hash}"}}"#
        ))
    } else {
        writeln!(out, "verified marker history and capsules through commit {seq}, chain {hash}")
            .map_err(Failure::io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_exact_nonempty_cut_and_unambiguous_anchor() {
        let args = ["--db", "db", "--key-file", "keys", "--through", "1"].map(str::to_owned);
        assert!(parse(&args).is_ok());
        for invalid in ["0", "-1", "+1", "18446744073709551616"] {
            let mut bad = args.clone();
            bad[5] = invalid.to_owned();
            assert!(parse(&bad).is_err());
        }
        assert!(parse(&args[..4]).is_err());
        for extra in [["--through", "1"], ["--expect-chain", "abc"], ["--overwrite", "yes"]] {
            let mut bad = args.to_vec();
            bad.extend(extra.map(str::to_owned));
            assert!(parse(&bad).is_err());
        }
    }
}
