//! Bounded JSON collection parameters, using the ordinary typed binders.
//! File bytes are input transport, never query text or a second value format.

use super::{Failure, Options};
use asupersync::io::AsyncReadExt;
use fgdb_types::QueryCx;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(super) const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const READ_BYTES: usize = 8192;

pub(super) fn parse(raw: &str) -> Result<(String, PathBuf), Failure> {
    let (name, path) = raw
        .split_once('=')
        .ok_or_else(|| Failure::usage("--param-file needs name=path"))?;
    let identifier = name
        .bytes()
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
    let numeric = !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit());
    if (!identifier && !numeric) || path.is_empty() {
        return Err(Failure::usage(
            "--param-file needs a parameter name (without $) and a path",
        ));
    }
    Ok((name.to_owned(), PathBuf::from(path)))
}

pub(super) fn validate_names<'a>(
    files: &'a [(String, PathBuf)],
    inline: impl IntoIterator<Item = &'a str>,
) -> Result<(), Failure> {
    let mut names: BTreeSet<&str> = inline.into_iter().collect();
    for (name, _) in files {
        if !names.insert(name) {
            return Err(Failure::usage("duplicate inline/file parameter name"));
        }
    }
    Ok(())
}

/// Read all of one scope into private storage before its caller publishes any
/// new parameters. `remaining` is shared across every scope in an invocation.
pub(super) async fn load<'a>(
    cx: &QueryCx,
    files: &'a [(String, PathBuf)],
    inline: impl IntoIterator<Item = &'a str>,
    remaining: &mut usize,
) -> Result<Vec<(String, String)>, Failure> {
    validate_names(files, inline)?;
    let mut loaded = Vec::new();
    for (name, path) in files {
        let text = read(cx, path, *remaining).await?;
        *remaining -= text.len();
        loaded.push((name.clone(), format!("json:{text}")));
    }
    Ok(loaded)
}

async fn read(cx: &QueryCx, path: &Path, limit: usize) -> Result<String, Failure> {
    let too_large = || Failure::usage("parameter files exceed the shared 4 MiB input limit");
    cx.checkpoint().map_err(Failure::io)?;
    let named = asupersync::fs::metadata(path)
        .await
        .map_err(|_| Failure::io("cannot inspect --param-file"))?;
    if !named.is_file() {
        return Err(Failure::usage("--param-file must be a regular file"));
    }
    if named.len() > limit as u64 {
        return Err(too_large());
    }
    cx.checkpoint().map_err(Failure::io)?;
    let mut file = asupersync::fs::File::open(path)
        .await
        .map_err(|_| Failure::io("cannot open --param-file"))?;
    cx.checkpoint().map_err(Failure::io)?;
    let opened = file.metadata().await.map_err(Failure::io)?;
    if !opened.is_file() {
        return Err(Failure::usage("--param-file must be a regular file"));
    }
    if opened.len() > limit as u64 {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; READ_BYTES];
    loop {
        cx.checkpoint().map_err(Failure::io)?;
        // Reading one extra byte detects growth after either metadata check.
        let capacity = buffer.len().min(limit - bytes.len() + 1);
        let count = match file.read(&mut buffer[..capacity]).await {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Failure::io("cannot read --param-file")),
        };
        if count == 0 {
            break;
        }
        if count > limit - bytes.len() {
            return Err(too_large());
        }
        let required = bytes.len() + count;
        if bytes.capacity() < required {
            let capacity = bytes
                .capacity()
                .saturating_mul(2)
                .max(READ_BYTES)
                .max(required)
                .min(limit);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .map_err(Failure::io)?;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    cx.checkpoint().map_err(Failure::io)?;
    String::from_utf8(bytes).map_err(|_| Failure::usage("--param-file must be UTF-8"))
}

pub(super) async fn load_local(options: &mut Options, cx: &QueryCx) -> Result<(), Failure> {
    let mut remaining = MAX_INPUT_BYTES;
    let loaded = load(
        cx,
        &options.param_files,
        options.raw_params.iter().map(|(name, _)| name.as_str()),
        &mut remaining,
    )
    .await?;
    options.raw_params.extend(loaded);
    for step in &mut options.steps {
        let loaded = load(
            cx,
            &step.param_files,
            step.raw_params.iter().map(|(name, _)| name.as_str()),
            &mut remaining,
        )
        .await?;
        // Transaction preparation otherwise runs after writable open. File
        // JSON must refuse before opening storage, including a late step.
        // Collections do not require a timezone resolver.
        for (_, raw) in &loaded {
            super::parameter(raw, None)?;
        }
        step.raw_params.extend(loaded);
    }
    if !options.steps.is_empty() {
        // Count loaded contents, not the path spellings, against the existing
        // transaction-wide 1 MiB input allowance.
        super::transaction::validate_input(&options.steps)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(command: &str, flags: &[&str]) -> Result<Options, Failure> {
        let mut args: Vec<String> = ["--db", "db", "--key-file", "keys"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        args.extend(flags.iter().map(|value| (*value).to_owned()));
        super::super::parse(&args, command)
    }

    #[test]
    fn parameter_files_bind_to_their_statement_scope_and_preserve_path_equals() {
        for command in ["query", "write", "diff"] {
            let mut flags = vec![
                "--param-file",
                "rows=part=1.json",
                "UNWIND $rows AS row RETURN row",
            ];
            if command == "diff" {
                flags.extend(["--before", "0", "--after", "1"]);
            }
            let options =
                local(command, &flags).unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(
                options.param_files,
                [("rows".into(), PathBuf::from("part=1.json"))]
            );
        }
        assert!(
            local(
                "replay",
                &["--certificate", "proof", "--param-file", "1=one.json"]
            )
            .is_ok()
        );
        let options = local(
            "transaction",
            &[
                "--query",
                "UNWIND $rows AS row RETURN row",
                "--param-file",
                "rows=first.json",
                "--query",
                "UNWIND $rows AS row RETURN row",
                "--param-file",
                "rows=second.json",
            ],
        )
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert!(options.param_files.is_empty());
        assert_eq!(
            options.steps[0].param_files,
            [("rows".into(), PathBuf::from("first.json"))]
        );
        assert_eq!(
            options.steps[1].param_files,
            [("rows".into(), PathBuf::from("second.json"))]
        );
    }

    #[test]
    fn duplicate_or_inapplicable_file_parameters_refuse_before_io() {
        for raw in [
            "rows",
            "=rows.json",
            "rows=",
            "$rows=rows.json",
            "1x=rows.json",
        ] {
            assert!(parse(raw).is_err(), "{raw}");
        }
        for flags in [
            vec![
                "--param",
                "rows=json:[]",
                "--param-file",
                "rows=missing.json",
                "RETURN $rows",
            ],
            vec![
                "--param-file",
                "rows=missing.json",
                "--param",
                "rows=json:[]",
                "RETURN $rows",
            ],
            vec![
                "--param-file",
                "rows=one.json",
                "--param-file",
                "rows=two.json",
                "RETURN $rows",
            ],
        ] {
            assert_eq!(local("query", &flags).err().unwrap().code, 2);
        }
        for command in ["create", "load", "import-csv", "compact", "scrub", "search"] {
            assert_eq!(
                local(command, &["--param-file", "rows=rows.json"])
                    .err()
                    .unwrap()
                    .code,
                2
            );
        }
        for flags in [
            vec!["--param-file", "rows=rows.json", "--query", "RETURN 1"],
            vec!["--savepoint", "held", "--param-file", "rows=rows.json"],
        ] {
            assert_eq!(local("transaction", &flags).err().unwrap().code, 2);
        }
    }
}
