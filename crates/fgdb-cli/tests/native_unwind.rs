//! Real CLI processes, ordinary filesystem durability, and robot completion.
//! No in-memory substitute is used for command dispatch or database reopening.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);
const COUNTER: &str = "UNWIND $rows AS row MERGE (n:Person {p:row.p}) \
    ON CREATE SET n.q=0 SET n.q=n.q+row.q";
const READ: &str = "MATCH (n:Person) RETURN n.p AS p, n.q AS q ORDER BY p";

struct Fixture {
    root: PathBuf,
    db: PathBuf,
    key: PathBuf,
}
impl Fixture {
    fn unopened() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fgdb-native-unwind-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let key = root.join("test.keys");
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&key).unwrap();
        use std::io::Write as _;
        writeln!(
            file,
            "{}\n{}\n{}",
            "51".repeat(32),
            "52".repeat(32),
            "53".repeat(32)
        )
        .unwrap();
        Self {
            db: root.join("db"),
            root,
            key,
        }
    }
    fn new() -> Self {
        let fixture = Self::unopened();
        successful(fixture.run("create", &[]));
        fixture
    }
    fn run(&self, command: &str, arguments: &[&str]) -> Output {
        let mut process = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        process
            .args(["--robot", command])
            .arg("--db")
            .arg(&self.db)
            .arg("--key-file")
            .arg(&self.key);
        if command != "create" {
            process.args([
                "--label",
                "Person=1",
                "--property",
                "p=1",
                "--property",
                "q=2",
            ]);
        }
        process.args(arguments).output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn successful(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.matches("\"event\":\"result\"").count(), 1, "{text}");
    assert!(!text.contains("\"event\":\"error\""), "{text}");
    text
}
fn failed(output: Output, code: i32) -> String {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("\"event\":\"error\""), "{text}");
    assert!(!text.contains("\"event\":\"result\""), "{text}");
    text
}
fn has_pair(text: &str, p: i64, q: i64) -> bool {
    text.contains(&format!(
        r#""cells":[{{"type":"int","value":"{p}"}},{{"type":"int","value":"{q}"}}]"#
    ))
}

#[test]
fn cli_native_unwind_updates_and_reopens_without_rows_flag() {
    let fixture = Fixture::new();
    let result = successful(fixture.run(
        "write",
        &[
            "--param",
            "rows=json:[{\"p\":1,\"q\":2},{\"p\":2,\"q\":4},{\"p\":1,\"q\":3}]",
            COUNTER,
        ],
    ));
    assert!(result.contains("\"statements\":3"), "{result}");
    let read = successful(fixture.run("query", &[READ]));
    assert_eq!(read.matches("\"event\":\"row\"").count(), 2, "{read}");
    assert!(has_pair(&read, 1, 5), "{read}");
    assert!(has_pair(&read, 2, 4), "{read}");
}

#[test]
fn cli_late_failure_has_input_coordinate_and_no_durable_prefix() {
    let fixture = Fixture::new();
    successful(fixture.run("write", &["CREATE (n:Person {p:9, q:9223372036854775807})"]));
    let failure = failed(
        fixture.run(
            "write",
            &[
                "--param",
                "rows=json:[{\"p\":8,\"q\":1},{\"p\":9,\"q\":1}]",
                COUNTER,
            ],
        ),
        3,
    );
    assert!(failure.contains("argument set 1"), "{failure}");
    let read = successful(fixture.run("query", &[READ]));
    assert_eq!(read.matches("\"event\":\"row\"").count(), 1, "{read}");
    assert!(has_pair(&read, 9, i64::MAX), "{read}");
}

#[test]
fn cli_bad_rows_refuse_before_opening_a_missing_database() {
    let fixture = Fixture::unopened();
    let failure = failed(
        fixture.run(
            "write",
            &[
                "--param",
                "rows=json:[{\"p\":1,\"q\":2},{\"p\":2,\"q\":true}]",
                COUNTER,
            ],
        ),
        3,
    );
    assert!(failure.contains("IncompatibleFieldTypes"), "{failure}");
    assert!(!fixture.db.exists());
}

#[test]
fn cli_transaction_unwind_reads_its_overlay_and_rollback_hides_output() {
    let fixture = Fixture::new();
    let arguments = [
        "--write",
        COUNTER,
        "--param",
        "rows=json:[{\"p\":1,\"q\":2},{\"p\":1,\"q\":3}]",
        "--query",
        READ,
    ];
    let mut rollback = arguments.to_vec();
    rollback.push("--rollback");
    let result = successful(fixture.run("transaction", &rollback));
    assert!(result.contains("\"kind\":\"rolled_back\""), "{result}");
    assert!(!result.contains("\"event\":\"row\""), "{result}");
    assert!(!result.contains("\"event\":\"statement\""), "{result}");
    let read = successful(fixture.run("query", &[READ]));
    assert!(!read.contains("\"event\":\"row\""), "{read}");
    let committed = successful(fixture.run("transaction", &arguments));
    assert!(committed.contains("\"kind\":\"committed\""), "{committed}");
    assert!(committed.contains("\"statements\":3"), "{committed}");
    assert!(has_pair(&committed, 1, 5), "{committed}");
    assert!(has_pair(&successful(fixture.run("query", &[READ])), 1, 5));
}

#[test]
fn cli_transaction_statement_limit_counts_expanded_input_rows() {
    let fixture = Fixture::new();
    let objects = (0..65)
        .map(|p| format!(r#"{{"p":{p},"q":1}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let rows = format!("rows=json:[{objects}]");
    let failure = failed(
        fixture.run("transaction", &["--write", COUNTER, "--param", &rows]),
        2,
    );
    assert!(
        failure.contains("exceeds 64 native statements"),
        "{failure}"
    );
    let read = successful(fixture.run("query", &[READ]));
    assert!(!read.contains("\"event\":\"row\""), "{read}");
}
