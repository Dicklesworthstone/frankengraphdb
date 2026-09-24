//! Process-level lifecycle contracts for the local-owner `fgdb` binary:
//! create, write, query, compact and reopen each run in a fresh process
//! against the real durable engine. Temporary artifacts are retained for
//! diagnosis; no fixture deletes files.

use std::path::PathBuf;
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

    let compacted = succeeded(&fixture.robot("compact", &[]), "compacted");
    assert!(compacted >= last, "compaction must not rewind the frontier");
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
