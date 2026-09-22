#![forbid(unsafe_code)]

use asupersync::{Budget, Cx, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys};
use fgdb_cli::{Command, Error, Format, HELP, Invocation, MAX_INPUT_BYTES, Options, Symbols,
    output::{render_result, render_status}, parse_args, parse_parameters};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{context::PurposeContexts, ids::DatabaseSecurityNamespaceId};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Result<Vec<_>, _> = std::env::args_os().skip(1).map(|word| word.into_string()).collect();
    let invocation = args.map_err(|_| Error::Usage).and_then(parse_args);
    let mut format = Format::Human;
    let response = match invocation {
        Ok(Invocation::Help) => Ok(HELP.to_owned()),
        Ok(Invocation::Version) => Ok(format!("fgdb {}\n", env!("CARGO_PKG_VERSION"))),
        Ok(Invocation::Run(options)) => {
            format = options.format;
            run(options)
        }
        Err(error) => Err(error),
    };
    match response {
        Ok(text) => {
            let mut stdout = io::stdout().lock();
            let result = stdout.write_all(text.as_bytes()).and_then(|()| stdout.flush());
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => report(Error::Output, Format::Human),
            }
        }
        Err(error) => report(error, format),
    }
}

fn report(error: Error, format: Format) -> ExitCode {
    // Only a stable code is exported, never an engine Debug/Display value,
    // command line, database path, statement, parameter, key or OS error text.
    if format == Format::Ndjson && error != Error::Output {
        let _ = writeln!(io::stdout().lock(), "{{\"version\":1,\"type\":\"error\",\"code\":\"{}\"}}", error.code());
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
        Command::Init => Some(render_status("init", options.format, options.max_output_bytes)?),
        Command::Compact => Some(render_status("compact", options.format, options.max_output_bytes)?),
        Command::Query => None,
    };
    let statement = options.query_file.as_deref().map(|path| read_text(&root, path, true)).transpose()?;
    let symbols = match options.symbols_file.as_deref() {
        Some(path) => Symbols::parse(&read_text(&root, path, false)?)?,
        None => Symbols::default(),
    };
    let parameters = match options.params_file.as_deref() {
        Some(path) => parse_parameters(&read_text(&root, path, false)?)?,
        None => fgdb_gql::GqlParameters::new(),
    };
    let commit = PurposeContexts::narrow_runtime_root(&root).commit();
    let query = PurposeContexts::narrow_runtime_root(&root).query();
    runtime.block_on(async {
        root.checkpoint().map_err(|_| Error::Context)?;
        match options.command {
            Command::Init => {
                let db = Database::create(&commit, &options.db, keys).await.map_err(|_| Error::Database)?;
                drop(db);
                Ok(status.ok_or(Error::Usage)?)
            }
            Command::Compact => {
                let mut db = Database::open(&commit, &options.db, keys).await.map_err(|_| Error::Database)?;
                db.compact(&commit).await.map_err(|_| Error::Database)?;
                drop(db);
                Ok(status.ok_or(Error::Usage)?)
            }
            Command::Query => {
                let db = Database::open(&commit, &options.db, keys).await.map_err(|_| Error::Database)?;
                let result = db.query(&query, statement.as_deref().ok_or(Error::Usage)?, &parameters,
                    &symbols,
                    GqlQueryPolicy::new(options.max_work, options.max_rows, options.max_work, options.max_work))
                    .map_err(|_| Error::Query)?;
                root.checkpoint().map_err(|_| Error::Context)?;
                render_result(&result, options.format, options.max_output_bytes)
            }
        }
    })
}

fn read_text(cx: &Cx, path: &Path, allow_stdin: bool) -> Result<String, Error> {
    cx.checkpoint().map_err(|_| Error::Context)?;
    let mut bytes = Vec::new();
    if allow_stdin && path == Path::new("-") {
        io::stdin().lock().take((MAX_INPUT_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|_| Error::Input)?;
    } else {
        let file = File::open(path).map_err(|_| Error::Input)?;
        if !file.metadata().map_err(|_| Error::Input)?.is_file() { return Err(Error::Input); }
        file.take((MAX_INPUT_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|_| Error::Input)?;
    }
    cx.checkpoint().map_err(|_| Error::Context)?;
    if bytes.len() > MAX_INPUT_BYTES { return Err(Error::Input); }
    String::from_utf8(bytes).map_err(|_| Error::Input)
}

fn read_keys(cx: &Cx, path: &Path) -> Result<DatabaseKeys, Error> {
    cx.checkpoint().map_err(|_| Error::Context)?;
    let mut file = File::open(path).map_err(|_| Error::Keys)?;
    // Validate the opened handle, not a path that can be replaced between a
    // metadata check and open. No key is accepted from argv or environment.
    let metadata = file.metadata().map_err(|_| Error::Keys)?;
    if !metadata.is_file() || metadata.len() != 96 { return Err(Error::Keys); }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 { return Err(Error::Keys); }
    }
    let mut raw = [0u8; 96];
    let result = (|| {
        file.read_exact(&mut raw).map_err(|_| Error::Keys)?;
        let mut extra = [0u8; 1];
        if file.read(&mut extra).map_err(|_| Error::Keys)? != 0 { return Err(Error::Keys); }
        cx.checkpoint().map_err(|_| Error::Context)?;
        let mut oid = [0; 32];
        let mut namespace = [0; 32];
        let mut dek = [0; 32];
        oid.copy_from_slice(&raw[..32]);
        namespace.copy_from_slice(&raw[32..64]);
        dek.copy_from_slice(&raw[64..]);
        Ok(DatabaseKeys::new(oid, DatabaseSecurityNamespaceId(namespace), dek))
    })();
    // Best-effort cleanup of this staging buffer. DatabaseKeys itself uses the
    // existing scrub-on-last-drop SharedSecret; this is not an unsafe eraser.
    raw.fill(0);
    std::hint::black_box(&mut raw);
    result
}
