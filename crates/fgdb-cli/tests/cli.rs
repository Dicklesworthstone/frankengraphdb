//! Process-level lifecycle contracts for the local-owner `fgdb` binary:
//! create, write, query, compact and reopen each run in a fresh process
//! against the real durable engine. Temporary artifacts are retained for
//! diagnosis; no fixture deletes files. Key files are created owner-only
//! (0600), as the CLI requires on Unix.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::{EId, VId, context::PurposeContexts, ids::DatabaseSecurityNamespaceId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-cli-{name}-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("keys"),
            format!(
                "{}\n{}\n{}\n",
                "41".repeat(32),
                "42".repeat(32),
                "43".repeat(32)
            ),
        )
        .unwrap();
        // Key files must be owner-only (the CLI refuses group/other bits).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("keys"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.join("db")
    }

    fn run(&self, robot: bool, operation: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command.current_dir(&self.dir);
        if robot {
            command.arg("--robot");
        }
        command
            .arg(operation)
            .arg("--db")
            .arg(self.db())
            .args(["--key-file", "keys"])
            .args(["--label", "Person=1", "--property", "id=1"])
            .args(args);
        command.output().unwrap()
    }

    fn robot(&self, operation: &str, args: &[&str]) -> Output {
        self.run(true, operation, args)
    }
}

#[track_caller]
fn terminal(output: &Output) -> String {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    stdout.lines().last().unwrap_or("").to_owned()
}

#[track_caller]
fn succeeded(output: &Output, kind: &str) -> u64 {
    let last = terminal(output);
    assert!(
        output.status.success() && last.contains(&format!("\"kind\":\"{kind}\"")),
        "status {:?}\nstdout {}\nstderr {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let seq = last.split("\"seq\":").nth(1).unwrap();
    seq[..seq.find(|c: char| !c.is_ascii_digit()).unwrap()]
        .parse()
        .unwrap()
}

#[track_caller]
fn refused(output: &Output, code: i32, class: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout {}\nstderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let last = terminal(output);
    assert!(
        last.contains(&format!("\"event\":\"error\",\"class\":\"{class}\"")),
        "{last}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\"event\":\"result\""));
}

/// Every file under `root` with its bytes, so any publication is visible.
fn directory_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// The keys `Fixture` writes, for the embedded library.
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}

/// Commit through the embedded library, not the CLI.
fn seed_through_the_library(path: &Path) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).commit();
    runtime.block_on(async {
        let mut db = Database::open(&cx, path, keys()).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(1), vec![LabelId(17)], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&cx, batch).await.unwrap();
    });
}

fn rows(output: &Output) -> Vec<String> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .filter(|line| line.contains("\"event\":\"row\""))
        .map(str::to_owned)
        .collect()
}

#[test]
fn compact_preserves_every_query_result_across_process_reopens() {
    let fixture = Fixture::new("compact");
    let created = succeeded(&fixture.robot("create", &[]), "created");
    let mut last = created;
    for id in 1..=6 {
        let param = format!("id=int:{id}");
        last = succeeded(
            &fixture.robot("write", &["--param", &param, "CREATE (n:Person {id:$id})"]),
            "written",
        );
    }
    succeeded(
        &fixture.robot("write", &["MATCH (n:Person) WHERE n.id=3 DELETE n"]),
        "written",
    );
    let read = "MATCH (n:Person) RETURN n.id AS id ORDER BY id";
    let before = fixture.robot("query", &[read]);
    succeeded(&before, "rows");
    assert_eq!(rows(&before).len(), 5);

    let published = directory_bytes(&fixture.db());
    let compacted = succeeded(&fixture.robot("compact", &[]), "compacted");
    assert!(compacted >= last, "compaction must not rewind the frontier");
    // A real compaction publishes a successor slot generation; a dispatcher
    // that reported success without compacting would leave the store as-is.
    let after_compact = directory_bytes(&fixture.db());
    assert!(
        published != after_compact,
        "compact must publish a new generation; files: {:?}",
        after_compact.keys()
    );
    let after = fixture.robot("query", &[read]);
    succeeded(&after, "rows");
    assert_eq!(
        rows(&after),
        rows(&before),
        "compaction changed a query result"
    );

    // Compaction is repeatable and remains readable by a later process.
    succeeded(&fixture.robot("compact", &[]), "compacted");
    assert_eq!(rows(&fixture.robot("query", &[read])), rows(&before));
    // A write after compaction still lands and is visible.
    succeeded(
        &fixture.robot("write", &["CREATE (n:Person {id:7})"]),
        "written",
    );
    assert_eq!(rows(&fixture.robot("query", &[read])).len(), 6);

    let human = fixture.run(false, "compact", &[]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).starts_with("compacted (seq "));
    assert!(created < compacted);
}

#[test]
fn compact_refuses_arguments_and_a_missing_database() {
    let fixture = Fixture::new("compact-refusals");
    refused(&fixture.robot("compact", &[]), 4, "open");
    assert!(
        !fixture.db().exists(),
        "compact must never create a database"
    );
    succeeded(&fixture.robot("create", &[]), "created");
    refused(
        &fixture.robot("compact", &["MATCH (n) RETURN n"]),
        2,
        "usage",
    );
    refused(
        &fixture.robot("compact", &["--param", "x=int:1"]),
        2,
        "usage",
    );
}

#[test]
fn query_refuses_a_write_statement_without_mutating_or_echoing_it() {
    let fixture = Fixture::new("read-only");
    succeeded(&fixture.robot("create", &[]), "created");
    let statement = "CREATE (n:Person {id:424242})";
    let output = fixture.robot("query", &[statement]);
    refused(&output, 3, "query");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("424242"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("424242"));
    let count = fixture.robot("query", &["MATCH (n:Person) RETURN COUNT(n) AS total"]);
    succeeded(&count, "rows");
    assert!(rows(&count)[0].contains("\"type\":\"count\",\"value\":\"0\""));
}

#[test]
fn query_does_not_silently_create_a_missing_database() {
    let fixture = Fixture::new("missing");
    refused(
        &fixture.robot("query", &["MATCH (n) RETURN COUNT(n) AS total"]),
        4,
        "open",
    );
    assert!(!fixture.db().exists());
}

#[test]
fn help_needs_neither_database_nor_keys() {
    let output = Command::new(env!("CARGO_BIN_EXE_fgdb"))
        .arg("help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for command in ["create", "compact", "import-csv", "query", "write"] {
        assert!(
            text.contains(&format!("  {command} ")),
            "help omits {command}"
        );
    }
}

/// The catalog names a label the statement text never mentions, so
/// `labels(a)` can only answer from the supplied bindings.
const CATALOG: [&str; 4] = [
    "--relation",
    "KNOWS=1",
    "--label",
    "UnmentionedStoredLabel=17",
];

#[test]
fn library_writes_are_read_by_cli_processes_with_catalog_reflection() {
    let fixture = Fixture::new("library");
    succeeded(&fixture.robot("create", &[]), "created");
    seed_through_the_library(&fixture.db());
    let query = |text: &str| {
        let mut args = CATALOG.to_vec();
        args.push(text);
        fixture.robot("query", &args)
    };
    let edge = query("MATCH (a)-[:KNOWS]->(b) RETURN b");
    succeeded(&edge, "rows");
    assert_eq!(
        rows(&edge),
        [r#"{"v":1,"event":"row","cells":[{"type":"vertex","value":"2"}]}"#]
    );
    let labels = query("MATCH (a)-[:KNOWS]->(b) RETURN labels(a) AS l");
    succeeded(&labels, "rows");
    assert_eq!(
        rows(&labels),
        [
            r#"{"v":1,"event":"row","cells":[{"type":"list","value":[{"type":"text","value":"UnmentionedStoredLabel"}]}]}"#
        ]
    );
    let explain = query("EXPLAIN MATCH (a)-[:KNOWS]->(b) RETURN b");
    assert!(
        explain.status.success() && terminal(&explain).contains("\"event\":\"result\""),
        "{}",
        String::from_utf8_lossy(&explain.stdout)
    );
}

#[test]
fn invalid_key_files_are_refused_without_creating_a_database() {
    let line = "ab".repeat(32);
    let cases: [(&str, Vec<u8>); 7] = [
        ("empty", Vec::new()),
        ("two lines", format!("{line}\n{line}\n").into_bytes()),
        (
            "four lines",
            format!("{line}\n{line}\n{line}\n{line}\n").into_bytes(),
        ),
        (
            "short line",
            format!("{line}\n{line}\n{}\n", &line[1..]).into_bytes(),
        ),
        (
            "non-hex",
            format!("{line}\n{line}\n{}zz\n", &line[2..]).into_bytes(),
        ),
        // The retired binary's raw 96-byte key file is not silently accepted.
        ("legacy raw 96 bytes", vec![0x41; 96]),
        // Valid keys padded past the 64 KiB cap by a comment: refused unread.
        (
            "oversized",
            format!("{line}\n{line}\n{line}\n# {}\n", "x".repeat(70_000)).into_bytes(),
        ),
    ];
    for (name, bytes) in cases {
        let fixture = Fixture::new("bad-keys");
        std::fs::write(fixture.dir.join("keys"), bytes).unwrap();
        let output = fixture.robot("create", &[]);
        assert_eq!(output.status.code(), Some(4), "{name}");
        refused(&output, 4, "open");
        assert!(!fixture.db().exists(), "{name}");
        // Diagnostics never echo key material.
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!text.contains(&line[..16]), "{name}: {text}");
    }
    let fixture = Fixture::new("missing-keys");
    let output = Command::new(env!("CARGO_BIN_EXE_fgdb"))
        .current_dir(&fixture.dir)
        .args(["--robot", "create", "--db"])
        .arg(fixture.db())
        .args(["--key-file", "no-such-keys"])
        .output()
        .unwrap();
    refused(&output, 4, "open");
    assert!(!fixture.db().exists());
}

#[test]
fn diff_output_cap_refuses_rather_than_reporting_a_partial_diff() {
    let fixture = Fixture::new("diff-cap");
    // The cap is diff-only: offering it to create is a usage error that
    // fires before any database exists.
    refused(
        &fixture.robot("create", &["--max-output-bytes", "1"]),
        2,
        "usage",
    );
    assert!(!fixture.db().exists());
    let before = succeeded(&fixture.robot("create", &[]), "created").to_string();
    let after = succeeded(
        &fixture.robot("write", &["CREATE (n:Person {id:1})"]),
        "written",
    )
    .to_string();
    let diff = |cap: &str| {
        fixture.robot(
            "diff",
            &[
                "--before",
                &before,
                "--after",
                &after,
                "--max-output-bytes",
                cap,
                "MATCH (n:Person) RETURN n.id AS id",
            ],
        )
    };
    // Control: uncapped, the diff reports the added row and completes.
    let whole = diff("16777216");
    let stdout = String::from_utf8_lossy(&whole.stdout);
    assert!(whole.status.success(), "{stdout}");
    assert!(terminal(&whole).contains("\"kind\":\"diff\""), "{stdout}");
    assert!(stdout.contains("\"event\":\"change\""), "{stdout}");
    // One byte admits no frame: an error, never a complete or partial diff.
    let capped = diff("1");
    let stdout = String::from_utf8_lossy(&capped.stdout);
    assert_ne!(capped.status.code(), Some(0), "{stdout}");
    assert!(
        terminal(&capped).contains("\"event\":\"error\""),
        "{stdout}"
    );
    assert!(!stdout.contains("\"event\":\"result\""), "{stdout}");
    assert!(!stdout.contains("\"event\":\"change\""), "{stdout}");
}

#[cfg(unix)]
#[test]
fn key_files_accessible_to_group_or_others_are_refused_without_creating_a_database() {
    use std::os::unix::fs::PermissionsExt;
    // Whoever can read the key file can read the database, so any group or
    // other bit refuses, exactly as the retired binary did.
    for mode in [0o640, 0o604, 0o620, 0o644, 0o660] {
        let fixture = Fixture::new("key-mode");
        std::fs::set_permissions(
            fixture.dir.join("keys"),
            std::fs::Permissions::from_mode(mode),
        )
        .unwrap();
        let output = fixture.robot("create", &[]);
        assert_eq!(output.status.code(), Some(4), "mode {mode:o}");
        refused(&output, 4, "open");
        assert!(!fixture.db().exists(), "mode {mode:o}");
    }
    // Control: the same bytes at 0600 create the database.
    let fixture = Fixture::new("key-mode-owner");
    succeeded(&fixture.robot("create", &[]), "created");
    // A directory is not a key file, whatever its mode.
    let fixture = Fixture::new("key-directory");
    std::fs::create_dir(fixture.dir.join("key-directory")).unwrap();
    std::fs::set_permissions(
        fixture.dir.join("key-directory"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fgdb"))
        .current_dir(&fixture.dir)
        .args(["--robot", "create", "--db"])
        .arg(fixture.db())
        .args(["--key-file", "key-directory"])
        .output()
        .unwrap();
    refused(&output, 4, "open");
    assert!(!fixture.db().exists());
}

#[cfg(target_os = "linux")]
#[test]
fn a_fifo_key_file_is_refused_without_blocking() {
    // Opening a FIFO blocks until a writer appears, so the refusal must come
    // before the open. A regression shows up here as a hang, bounded below.
    let fixture = Fixture::new("key-fifo");
    let fifo = fixture.dir.join("key-fifo");
    assert!(
        Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_fgdb"))
        .current_dir(&fixture.dir)
        .args(["--robot", "create", "--db"])
        .arg(fixture.db())
        .args(["--key-file", "key-fifo"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("a FIFO key file must be refused, not opened and read");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    refused(&output, 4, "open");
    assert!(!fixture.db().exists());
}
