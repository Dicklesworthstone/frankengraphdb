#![forbid(unsafe_code)]

use asupersync::{Budget, Cx, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys};
use fgdb_cli::{
    Command, Error, Format, HELP, Invocation, MAX_INPUT_BYTES, Options, Symbols,
    output::{render_result, render_status},
    parse_args, parse_parameters,
    write::{PreparedCliWrite, classify_execution},
};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{context::PurposeContexts, ids::DatabaseSecurityNamespaceId};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Result<Vec<_>, _> = std::env::args_os()
        .skip(1)
        .map(|word| word.into_string())
        .collect();
    let invocation = args.map_err(|_| Error::Usage).and_then(parse_args);
    let mut format = Format::Human;
    let mut writes = false;
    let response = match invocation {
        Ok(Invocation::Help) => Ok(HELP.to_owned()),
        Ok(Invocation::Version) => Ok(format!("fgdb {}\n", env!("CARGO_PKG_VERSION"))),
        Ok(Invocation::Run(options)) => {
            format = options.format;
            writes = options.command.is_write();
            run(options)
        }
        Err(error) => Err(error),
    };
    match response {
        Ok(text) => {
            let mut stdout = io::stdout().lock();
            let result = stdout
                .write_all(text.as_bytes())
                .and_then(|()| stdout.flush());
            match result {
                Ok(()) => ExitCode::SUCCESS,
                // Execution has completed by this point. Lost output never
                // means a write rolled back or is safe to retry automatically.
                Err(_) => report(
                    if writes {
                        Error::WriteOutput
                    } else {
                        Error::Output
                    },
                    Format::Human,
                ),
            }
        }
        Err(error) => report(error, format),
    }
}

fn report(error: Error, format: Format) -> ExitCode {
    // Only a stable code is exported, never an engine Debug/Display value,
    // command line, database path, statement, parameter, key or OS error text.
    if format == Format::Ndjson && !matches!(error, Error::Output | Error::WriteOutput) {
        let _ = writeln!(
            io::stdout().lock(),
            "{{\"version\":1,\"type\":\"error\",\"code\":\"{}\"}}",
            error.code()
        );
    }
    let _ = writeln!(io::stderr().lock(), "fgdb: {}", error.code());
    ExitCode::from(error.exit_code())
}

fn run(options: Options) -> Result<String, Error> {
    let runtime = RuntimeBuilder::new().build().map_err(|_| Error::Context)?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let keys = read_keys(&root, &options.keys_file)?;
    // Admit all external input and success-envelope bounds before opening the
    // database. init/compact must not mutate and THEN discover output refusal.
    let status = match options.command {
        Command::Init => Some(render_status(
            "init",
            options.format,
            options.max_output_bytes,
        )?),
        Command::Compact => Some(render_status(
            "compact",
            options.format,
            options.max_output_bytes,
        )?),
        Command::Query | Command::Write | Command::ImportCsv => None,
    };
    let statement = options
        .query_file
        .as_deref()
        .map(|path| read_text(&root, path, true))
        .transpose()?;
    let symbols = match options.symbols_file.as_deref() {
        Some(path) => Symbols::parse(&read_text(&root, path, false)?)?,
        None => Symbols::default(),
    };
    let parameters = match options.params_file.as_deref() {
        Some(path) => parse_parameters(&read_text(&root, path, false)?)?,
        None => fgdb_gql::GqlParameters::new(),
    };
    // Complete all decoding and native binding before opening storage. The
    // source buffers are dropped here rather than retained across execution.
    let prepared_write = if let Some(write) = &options.write {
        let csv = write
            .csv_file
            .as_deref()
            .map(|path| read_text_limited(&root, path, true, write.max_input_bytes))
            .transpose()?;
        let types = write
            .types_file
            .as_deref()
            .map(|path| read_text(&root, path, false))
            .transpose()?;
        let prepared = PreparedCliWrite::prepare(
            &options,
            statement.as_deref().ok_or(Error::Usage)?,
            &symbols,
            &parameters,
            csv.as_deref(),
            types.as_deref(),
        )?;
        root.checkpoint().map_err(|_| Error::Context)?;
        Some(prepared)
    } else {
        None
    };
    let commit = PurposeContexts::narrow_runtime_root(&root).commit();
    let query = PurposeContexts::narrow_runtime_root(&root).query();
    let txn = PurposeContexts::narrow_runtime_root(&root).txn();
    runtime.block_on(async {
        root.checkpoint().map_err(|_| Error::Context)?;
        match options.command {
            Command::Init => {
                let db = Database::create(&commit, &options.db, keys)
                    .await
                    .map_err(|_| Error::Database)?;
                drop(db);
                Ok(status.ok_or(Error::Usage)?)
            }
            Command::Compact => {
                let mut db = Database::open(&commit, &options.db, keys)
                    .await
                    .map_err(|_| Error::Database)?;
                db.compact(&commit).await.map_err(|_| Error::Database)?;
                drop(db);
                Ok(status.ok_or(Error::Usage)?)
            }
            Command::Query => {
                let db = Database::open(&commit, &options.db, keys)
                    .await
                    .map_err(|_| Error::Database)?;
                let result = db
                    .query(
                        &query,
                        statement.as_deref().ok_or(Error::Usage)?,
                        &parameters,
                        &symbols,
                        GqlQueryPolicy::new(
                            options.max_work,
                            options.max_rows,
                            options.max_work,
                            options.max_work,
                        ),
                    )
                    .map_err(|_| Error::Query)?;
                root.checkpoint().map_err(|_| Error::Context)?;
                render_result(&result, options.format, options.max_output_bytes)
            }
            Command::Write | Command::ImportCsv => {
                let prepared = prepared_write.ok_or(Error::Usage)?;
                let mut db = Database::open(&commit, &options.db, keys)
                    .await
                    .map_err(|_| Error::Database)?;
                let (_, completion) = db
                    .execute_graph_write_program_autocommit_engine_governed(
                        &txn,
                        &query,
                        &commit,
                        prepared.program(),
                        prepared.policy(),
                    )
                    .await
                    .map_err(|source| classify_execution(&source))?;
                // No fallible work or cancellation checkpoint after successful
                // finish: both complete output envelopes were admitted earlier.
                Ok(prepared.into_response(completion))
            }
        }
    })
}

fn read_text(cx: &Cx, path: &Path, allow_stdin: bool) -> Result<String, Error> {
    read_text_limited(cx, path, allow_stdin, MAX_INPUT_BYTES)
}

fn read_text_limited(
    cx: &Cx,
    path: &Path,
    allow_stdin: bool,
    limit: usize,
) -> Result<String, Error> {
    cx.checkpoint().map_err(|_| Error::Context)?;
    if allow_stdin && path == Path::new("-") {
        read_utf8(cx, io::stdin().lock(), limit)
    } else {
        let file = File::open(path).map_err(|_| Error::Input)?;
        if !file.metadata().map_err(|_| Error::Input)?.is_file() {
            return Err(Error::Input);
        }
        read_utf8(cx, file, limit)
    }
}

fn read_utf8(cx: &Cx, mut input: impl Read, limit: usize) -> Result<String, Error> {
    if limit > fgdb_gql::csv_parameters::CsvParameterLimits::HARD.max_input_bytes {
        return Err(Error::Input);
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        cx.checkpoint().map_err(|_| Error::Context)?;
        // One lookahead byte distinguishes exact-limit EOF from truncation.
        // Never append a byte beyond the allowed source size.
        let allowed = (limit - bytes.len() + 1).min(chunk.len());
        let count = match input.read(&mut chunk[..allowed]) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Error::Input),
        };
        if count == 0 {
            break;
        }
        if count > limit - bytes.len() {
            return Err(Error::Input);
        }
        bytes.try_reserve(count).map_err(|_| Error::Input)?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    cx.checkpoint().map_err(|_| Error::Context)?;
    String::from_utf8(bytes).map_err(|_| Error::Input)
}

fn read_keys(cx: &Cx, path: &Path) -> Result<DatabaseKeys, Error> {
    cx.checkpoint().map_err(|_| Error::Context)?;
    let mut file = File::open(path).map_err(|_| Error::Keys)?;
    // Validate the opened handle, not a path that can be replaced between a
    // metadata check and open. No key is accepted from argv or environment.
    let metadata = file.metadata().map_err(|_| Error::Keys)?;
    if !metadata.is_file() || metadata.len() != 96 {
        return Err(Error::Keys);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Keys);
        }
    }
    let mut raw = [0u8; 96];
    let result = (|| {
        file.read_exact(&mut raw).map_err(|_| Error::Keys)?;
        let mut extra = [0u8; 1];
        if file.read(&mut extra).map_err(|_| Error::Keys)? != 0 {
            return Err(Error::Keys);
        }
        cx.checkpoint().map_err(|_| Error::Context)?;
        let mut oid = [0; 32];
        let mut namespace = [0; 32];
        let mut dek = [0; 32];
        oid.copy_from_slice(&raw[..32]);
        namespace.copy_from_slice(&raw[32..64]);
        dek.copy_from_slice(&raw[64..]);
        Ok(DatabaseKeys::new(
            oid,
            DatabaseSecurityNamespaceId(namespace),
            dek,
        ))
    })();
    // Best-effort cleanup of this staging buffer. DatabaseKeys itself uses the
    // existing scrub-on-last-drop SharedSecret; this is not an unsafe eraser.
    raw.fill(0);
    std::hint::black_box(&mut raw);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShortReads<'a> {
        data: &'a [u8],
        interrupted: bool,
        read: usize,
    }
    impl Read for ShortReads<'_> {
        fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = self.data.len().min(into.len()).min(2);
            into[..count].copy_from_slice(&self.data[..count]);
            self.data = &self.data[count..];
            self.read += count;
            Ok(count)
        }
    }

    #[test]
    fn bounded_reader_handles_short_reads_and_checks_exact_utf8_eof() {
        let runtime = RuntimeBuilder::new().build().unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let text = "x\n雪🙂\n";
        let mut input = ShortReads {
            data: text.as_bytes(),
            interrupted: false,
            read: 0,
        };
        assert_eq!(read_utf8(&cx, &mut input, text.len()).unwrap(), text);
        assert_eq!(input.read, text.len());
        let mut input = ShortReads {
            data: text.as_bytes(),
            interrupted: false,
            read: 0,
        };
        assert_eq!(read_utf8(&cx, &mut input, 3), Err(Error::Input));
        assert_eq!(
            input.read, 4,
            "read only one lookahead byte beyond admission"
        );
        assert_eq!(read_utf8(&cx, &b""[..], 0), Ok(String::new()));
        assert_eq!(read_utf8(&cx, &b"x"[..], 0), Err(Error::Input));
        assert_eq!(read_utf8(&cx, &[0xff][..], 1), Err(Error::Input));
    }
}
