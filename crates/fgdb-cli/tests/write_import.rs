//! Real CLI processes exercise durable reopen, typed writes and the atomic
//! CSV import: one CSV file is one native program, so any record's failure
//! commits nothing. Temporary artifacts are retained for diagnosis; no
//! fixture deletes files.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    dir: PathBuf,
    created: u64,
}

impl Fixture {
    fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-cli-import-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("keys"),
            format!(
                "{}\n{}\n{}\n",
                "31".repeat(32),
                "32".repeat(32),
                "33".repeat(32)
            ),
        )
        .unwrap();
        let mut fixture = Self { dir, created: 0 };
        fixture.created = seq(&success(fixture.run("create", &[])), "created");
        fixture
    }

    fn file(&self, name: &str, text: &str) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.dir.join(name))
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn command(&self, robot: bool, operation: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command.current_dir(&self.dir);
        if robot {
            command.arg("--robot");
        }
        command
            .arg(operation)
            .arg("--db")
            .arg(self.dir.join("db"))
            .args(["--key-file", "keys"])
            .args(["--label", "Person=1"])
            .args([
                "--property",
                "id=1",
                "--property",
                "name=2",
                "--property",
                "score=3",
            ]);
        command
    }

    fn run(&self, operation: &str, args: &[&str]) -> Output {
        self.command(true, operation).args(args).output().unwrap()
    }

    #[track_caller]
    fn count(&self, expected: u64) {
        let out = success(self.run("query", &["MATCH (n:Person) RETURN COUNT(n) AS total"]));
        assert!(
            out.contains(&format!("\"type\":\"count\",\"value\":\"{expected}\"")),
            "{out}"
        );
    }
}

#[track_caller]
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "status {:?}\nstdout {}\nstderr {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[track_caller]
fn refusal(output: &Output, code: i32, class: &str) {
    let out = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(code),
        "{out}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        out.contains(&format!("\"event\":\"error\",\"class\":\"{class}\"")),
        "{out}"
    );
    assert!(!out.contains("\"event\":\"result\""), "{out}");
}

#[track_caller]
fn seq(out: &str, kind: &str) -> u64 {
    let last = out.lines().last().unwrap();
    assert!(last.contains(&format!("\"kind\":\"{kind}\"")), "{last}");
    let tail = last.split("\"seq\":").nth(1).unwrap();
    tail[..tail.find(|c: char| !c.is_ascii_digit()).unwrap()]
        .parse()
        .unwrap()
}

#[test]
fn typed_writes_and_csv_imports_survive_separate_process_reopens() {
    let fixture = Fixture::new();
    let statement = "CREATE (n:Person {id:$id,name:$name})";
    // A parameter is a typed value, never spliced into the statement text.
    success(fixture.run(
        "write",
        &[
            "--param",
            "id=int:7",
            "--param",
            "name=text:'; MATCH (n) DELETE n; --",
            statement,
        ],
    ));
    fixture.count(1);
    fixture.file("write.gql", statement);
    fixture.file("types", "text\tname\n");
    fixture.file(
        "data.csv",
        "name,id\n\"quoted, \"\"name\"\"\",8\n\"multi\n雪\",9\n",
    );
    let imported = success(fixture.run(
        "import-csv",
        &[
            "--query-file",
            "write.gql",
            "--input",
            "data.csv",
            "--types-file",
            "types",
        ],
    ));
    assert!(
        imported.contains("\"records\":2,\"statements\":2"),
        "{imported}"
    );
    fixture.count(3);
    let out = success(fixture.run(
        "query",
        &["MATCH (n:Person) WHERE n.id=9 RETURN n.name AS name"],
    ));
    assert!(
        out.contains("\"type\":\"text\",\"value\":\"multi\\n雪\""),
        "{out}"
    );
    let quoted = success(fixture.run(
        "query",
        &["MATCH (n:Person) WHERE n.id=8 RETURN n.name AS name"],
    ));
    assert!(
        quoted.contains("\"value\":\"quoted, \\\"name\\\"\""),
        "{quoted}"
    );
}

#[test]
fn a_late_execution_failure_rolls_back_every_earlier_csv_record() {
    let fixture = Fixture::new();
    fixture.file(
        "script.gql",
        "CREATE (n:Person {id:$id}); MATCH (n:Person) WHERE n.id=$id SET n.score=100/$denom",
    );
    fixture.file("bad.csv", "id,denom\n1,1\n2,0\n");
    refusal(
        &fixture.run(
            "import-csv",
            &["--query-file", "script.gql", "--input", "bad.csv"],
        ),
        3,
        "query",
    );
    fixture.count(0);
    fixture.file("good.csv", "id,denom\n1,2\n2,4\n");
    let out = success(fixture.run(
        "import-csv",
        &["--query-file", "script.gql", "--input", "good.csv"],
    ));
    assert!(out.contains("\"records\":2,\"statements\":4"), "{out}");
    fixture.count(2);
    let scores = success(fixture.run(
        "query",
        &["MATCH (n:Person) WHERE n.score=25 RETURN COUNT(n) AS total"],
    ));
    assert!(
        scores.contains("\"type\":\"count\",\"value\":\"1\""),
        "{scores}"
    );
}

#[test]
fn malformed_records_and_input_limits_refuse_before_mutation_without_echoing_values() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person {id:$id})");
    fixture.file("types", "int64\tid\n");
    fixture.file("bad.csv", "id\n1\n2\nsensitive-invalid-integer\n");
    let output = fixture.run(
        "import-csv",
        &[
            "--query-file",
            "script.gql",
            "--types-file",
            "types",
            "--input",
            "bad.csv",
        ],
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sensitive"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive"));
    refusal(&output, 3, "query");
    fixture.count(0);
    fixture.file("good.csv", "id\n1\n2\n");
    refusal(
        &fixture.run(
            "import-csv",
            &[
                "--query-file",
                "script.gql",
                "--input",
                "good.csv",
                "--max-input-bytes",
                "3",
            ],
        ),
        3,
        "query",
    );
    fixture.count(0);
    // Exactly one statement source, and no --param: CSV supplies arguments.
    refusal(
        &fixture.run("import-csv", &["--input", "good.csv"]),
        2,
        "usage",
    );
    refusal(
        &fixture.run(
            "import-csv",
            &[
                "--query-file",
                "script.gql",
                "--input",
                "good.csv",
                "CREATE (n:Person)",
            ],
        ),
        2,
        "usage",
    );
    refusal(
        &fixture.run(
            "import-csv",
            &[
                "--query-file",
                "script.gql",
                "--input",
                "good.csv",
                "--param",
                "id=int:1",
            ],
        ),
        2,
        "usage",
    );
    fixture.count(0);
    success(fixture.run(
        "import-csv",
        &["--query-file", "script.gql", "--input", "good.csv"],
    ));
    fixture.count(2);
}

#[test]
fn engine_identities_remain_spent_after_delete_and_reopen() {
    let fixture = Fixture::new();
    success(fixture.run("write", &["CREATE (n:Person)"]));
    success(fixture.run("write", &["CREATE (n:Person)"]));
    success(fixture.run("write", &["MATCH (n:Person) DELETE n"]));
    fixture.count(0);
    success(fixture.run("write", &["CREATE (n:Person)"]));
    let out = success(fixture.run("query", &["MATCH (n:Person) RETURN n"]));
    assert!(out.contains("\"type\":\"vertex\",\"value\":\"3\""), "{out}");
    fixture.count(1);
}

#[test]
fn read_command_never_falls_back_to_write_and_no_effect_writes_do_not_commit() {
    let fixture = Fixture::new();
    refusal(&fixture.run("query", &["CREATE (n:Person)"]), 3, "query");
    fixture.count(0);
    let out = success(fixture.run("write", &["MATCH (n:Person) SET n.score=1"]));
    assert_eq!(
        seq(&out, "written"),
        fixture.created,
        "a no-effect write committed"
    );
    fixture.count(0);
    // A no-effect import likewise leaves the frontier where it was.
    fixture.file("none.gql", "MATCH (n:Person) WHERE n.id=$id SET n.score=1");
    fixture.file("ids.csv", "id\n1\n2\n");
    let out = success(fixture.run(
        "import-csv",
        &["--query-file", "none.gql", "--input", "ids.csv"],
    ));
    assert_eq!(seq(&out, "imported_csv"), fixture.created);
}

#[test]
fn whole_program_creation_budget_does_not_reset_between_records() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person {id:$id})");
    fixture.file("data.csv", "id\n1\n2\n");
    refusal(
        &fixture.run(
            "import-csv",
            &[
                "--query-file",
                "script.gql",
                "--input",
                "data.csv",
                "--max-changes",
                "1",
            ],
        ),
        3,
        "query",
    );
    fixture.count(0);
    success(fixture.run(
        "import-csv",
        &[
            "--query-file",
            "script.gql",
            "--input",
            "data.csv",
            "--max-changes",
            "2",
        ],
    ));
    fixture.count(2);
}

#[test]
fn csv_stdin_is_bounded_and_uses_the_same_atomic_path() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person {id:$id})");
    let mut child = fixture
        .command(true, "import-csv")
        .args(["--query-file", "script.gql", "--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"id\n1\n2\n").unwrap();
    drop(stdin);
    let out = success(child.wait_with_output().unwrap());
    assert!(out.contains("\"records\":2,\"statements\":2"), "{out}");
    fixture.count(2);
    // Only one input may read stdin.
    refusal(
        &fixture.run("import-csv", &["--query-file", "-", "--input", "-"]),
        2,
        "usage",
    );
    fixture.count(2);
}

#[cfg(target_os = "linux")]
#[test]
fn failed_stdout_does_not_relabel_a_durable_write_as_rolled_back() {
    let fixture = Fixture::new();
    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    // Human mode: the write executes, then the success line cannot be written.
    let output = fixture
        .command(false, "write")
        .arg("CREATE (n:Person)")
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(5),
        "an output failure is an I/O failure"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("rolled"), "{stderr}");
    fixture.count(1);
}
