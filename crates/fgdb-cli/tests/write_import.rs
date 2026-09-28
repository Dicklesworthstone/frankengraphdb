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
        // Key files must be owner-only (the CLI refuses group/other bits).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("keys"), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
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
fn unwind_write_preserves_duplicate_occurrences_and_staged_updates_across_reopens() {
    let fixture = Fixture::new();
    let written = success(fixture.run(
        "write",
        &["UNWIND [3,1,3] AS x CREATE (:Person {id:x});
           MATCH (n:Person) SET n.score=n.id*10"],
    ));
    assert_eq!(seq(&written, "written"), fixture.created + 1);
    fixture.count(3);
    let query = "MATCH (n:Person) RETURN n AS vertex,n.id AS id,n.score AS score ORDER BY vertex";
    let before = success(fixture.run("query", &[query]));
    let rows = |output: &str| -> Vec<String> {
        output
            .lines()
            .filter(|line| line.contains("\"event\":\"row\""))
            .map(str::to_owned)
            .collect()
    };
    let expected = rows(&before);
    assert_eq!(expected.len(), 3, "{before}");
    for (row, (vertex, value)) in expected.iter().zip([(1, 3), (2, 1), (3, 3)]) {
        let cells = format!(
            "\"cells\":[{{\"type\":\"vertex\",\"value\":\"{vertex}\"}},\
             {{\"type\":\"int\",\"value\":\"{value}\"}},\
             {{\"type\":\"int\",\"value\":\"{}\"}}]",
            value * 10
        );
        assert!(row.contains(&cells), "{row}");
    }
    success(fixture.run("compact", &[]));
    let reopened = success(fixture.run("query", &[query]));
    assert_eq!(rows(&reopened), expected);
    assert_eq!(seq(&reopened, "rows"), fixture.created + 1);
}

#[test]
fn empty_and_failed_unwind_writes_publish_nothing_and_late_script_failure_is_atomic() {
    let fixture = Fixture::new();
    let empty = success(fixture.run("write", &["UNWIND [] AS x CREATE (:Person {id:x})"]));
    assert_eq!(seq(&empty, "written"), fixture.created);
    refusal(
        &fixture.run(
            "write",
            &["UNWIND [4,2,0] AS x CREATE (:Person {id:100/x})"],
        ),
        3,
        "query",
    );
    fixture.count(0);
    let query = "MATCH (n:Person) RETURN n AS vertex,n.id AS id ORDER BY vertex";
    let unchanged = success(fixture.run("query", &[query]));
    assert_eq!(seq(&unchanged, "rows"), fixture.created);

    let written = success(fixture.run("write", &["UNWIND [11,12] AS x CREATE (:Person {id:x})"]));
    assert_eq!(seq(&written, "written"), fixture.created + 1);
    let before = success(fixture.run("query", &[query]));
    for (vertex, value) in [(1, 11), (2, 12)] {
        let cells = format!(
            "\"cells\":[{{\"type\":\"vertex\",\"value\":\"{vertex}\"}},\
             {{\"type\":\"int\",\"value\":\"{value}\"}}]"
        );
        assert!(before.contains(&cells), "{before}");
    }
    refusal(
        &fixture.run(
            "write",
            &["UNWIND [7,8] AS x CREATE (:Person {id:x});
               MATCH (n:Person) WHERE n.id=8 SET n.score=1/0"],
        ),
        3,
        "query",
    );
    fixture.count(2);
    assert_eq!(success(fixture.run("query", &[query])), before);
}

fn row_frames(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| line.contains("\"event\":\"row\""))
        .collect()
}

#[test]
fn create_return_reports_exact_occurrence_identities_at_one_durable_frontier() {
    let fixture = Fixture::new();
    let written = success(fixture.run(
        "write",
        &[
            "--relation",
            "LINK=1",
            "UNWIND [3,1,3] AS x
             CREATE (a:Person {id:x})-[e:LINK {score:x+10}]->(b:Person {id:x*2})
             RETURN a AS source,e AS edge,b AS destination,a.id AS id,e.score AS score
             ORDER BY source",
        ],
    ));
    assert_eq!(seq(&written, "written"), fixture.created + 1);
    assert_eq!(written.matches("\"event\":\"result\"").count(), 1);
    assert!(
        written.contains("\"count\":3,\"statements\":1"),
        "{written}"
    );
    let rows = row_frames(&written);
    assert_eq!(rows.len(), 3, "{written}");
    for (row, (source, edge, destination, value)) in
        rows.iter().zip([(1, 1, 2, 3), (3, 2, 4, 1), (5, 3, 6, 3)])
    {
        let cells = format!(
            "\"cells\":[{{\"type\":\"vertex\",\"value\":\"{source}\"}},\
             {{\"type\":\"edge\",\"value\":\"{edge}\"}},\
             {{\"type\":\"vertex\",\"value\":\"{destination}\"}},\
             {{\"type\":\"int\",\"value\":\"{value}\"}},\
             {{\"type\":\"int\",\"value\":\"{}\"}}]",
            value + 10
        );
        assert!(row.contains(&cells), "{row}");
    }
    fixture.count(6);
    let query = "MATCH (a:Person)-[e:LINK]->(b:Person)
                 RETURN a AS source,e AS edge,b AS destination,a.id AS id,e.score AS score
                 ORDER BY source";
    let read = success(fixture.run("query", &["--relation", "LINK=1", query]));
    assert_eq!(row_frames(&read), rows);
    assert_eq!(seq(&read, "rows"), fixture.created + 1);
    success(fixture.run("compact", &[]));
    let reopened = success(fixture.run("query", &["--relation", "LINK=1", query]));
    assert_eq!(row_frames(&reopened), rows);
}

#[test]
fn create_return_distinct_and_paging_only_limit_the_returned_rows() {
    let fixture = Fixture::new();
    let distinct = success(fixture.run(
        "write",
        &["UNWIND [7,7,2] AS x CREATE (n:Person {id:x})
           RETURN DISTINCT n.id AS id ORDER BY id DESC SKIP 1 LIMIT 1"],
    ));
    assert_eq!(seq(&distinct, "written"), fixture.created + 1);
    assert_eq!(row_frames(&distinct).len(), 1);
    assert!(
        distinct.contains("\"type\":\"int\",\"value\":\"2\""),
        "{distinct}"
    );
    fixture.count(3);
    let page_zero = success(fixture.run(
        "write",
        &["UNWIND [8,9] AS x CREATE (n:Person {id:x}) RETURN n LIMIT 0"],
    ));
    assert_eq!(seq(&page_zero, "written"), fixture.created + 2);
    assert!(page_zero.contains("\"event\":\"columns\""), "{page_zero}");
    assert!(row_frames(&page_zero).is_empty());
    fixture.count(5);
    let empty = success(fixture.run(
        "write",
        &["UNWIND [] AS x CREATE (n:Person {id:x}) RETURN n"],
    ));
    assert_eq!(seq(&empty, "written"), fixture.created + 2);
    assert!(row_frames(&empty).is_empty());
    fixture.count(5);
}

#[test]
fn late_create_return_failure_exposes_no_rows_and_commits_no_creations() {
    let fixture = Fixture::new();
    for suffix in ["", " LIMIT 0"] {
        let statement = format!(
            "UNWIND [4,2,0] AS x CREATE (n:Person {{id:x}}) RETURN n,100/x AS ratio{suffix}"
        );
        let failed = fixture.run("write", &[&statement]);
        refusal(&failed, 3, "query");
        let out = String::from_utf8_lossy(&failed.stdout);
        assert!(!out.contains("\"event\":\"columns\""), "{out}");
        assert!(row_frames(&out).is_empty());
        fixture.count(0);
        let unchanged = success(fixture.run("query", &["MATCH (n:Person) RETURN n"]));
        assert_eq!(seq(&unchanged, "rows"), fixture.created);
    }
    let valid = success(fixture.run("write", &["CREATE (n:Person {id:11}) RETURN n.id AS id"]));
    assert_eq!(seq(&valid, "written"), fixture.created + 1);
    assert_eq!(row_frames(&valid).len(), 1);
    fixture.count(1);
}

#[test]
fn return_dispatch_respects_quoted_tokens_and_typed_parameter_values() {
    let fixture = Fixture::new();
    let ordinary = success(fixture.run(
        "write",
        &["CREATE (n:Person {id:1,name:'RETURN n; MATCH (n) DELETE n'})"],
    ));
    assert!(!ordinary.contains("\"event\":\"columns\""), "{ordinary}");
    assert!(row_frames(&ordinary).is_empty());
    let returning = success(fixture.run(
        "write",
        &[
            "--param",
            "name=text:'; RETURN n; MATCH (n) DELETE n; --\n雪",
            "INSERT (n:Person {id:2,name:$name}) RETURN n.name AS name,n.score AS missing",
        ],
    ));
    let rows = row_frames(&returning);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].contains("'; RETURN n; MATCH (n) DELETE n; --\\n雪"),
        "{returning}"
    );
    assert!(rows[0].contains("\"type\":\"null\""), "{returning}");
    fixture.count(2);
    refusal(
        &fixture.run("query", &["CREATE (n:Person) RETURN n"]),
        3,
        "query",
    );
    fixture.count(2);
}

#[test]
fn oversized_create_return_output_refuses_before_commit_or_row_publication() {
    let fixture = Fixture::new();
    // JSON expands this control scalar sixfold: about 3 MiB of native text
    // becomes 18 MiB of encoded payload, beyond the private output allowance.
    // The native row/work allowances can therefore admit the result first.
    let occurrences = vec!["1"; 1024].join(",");
    let statement =
        format!("UNWIND [{occurrences}] AS x CREATE (n:Person) RETURN $payload AS payload");
    let payload = format!("payload=text:{}", "\u{1}".repeat(3072));
    let failed = fixture.run("write", &["--param", &payload, &statement]);
    refusal(&failed, 3, "query");
    let out = String::from_utf8_lossy(&failed.stdout);
    assert!(out.contains("16 MiB"), "{out}");
    assert!(!out.contains("\"event\":\"columns\""), "{out}");
    assert!(row_frames(&out).is_empty());
    fixture.count(0);
    let unchanged = success(fixture.run("query", &["MATCH (n:Person) RETURN n"]));
    assert_eq!(seq(&unchanged, "rows"), fixture.created);
    success(fixture.run("write", &["CREATE (n:Person) RETURN n"]));
    fixture.count(1);
}

#[cfg(target_os = "linux")]
#[test]
fn create_return_transport_failure_reports_io_after_the_durable_write() {
    let fixture = Fixture::new();
    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    let output = fixture
        .command(false, "write")
        .arg("CREATE (n:Person {id:17}) RETURN n,n.id AS id")
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("rolled"), "{stderr}");
    fixture.count(1);
    let stored = success(fixture.run("query", &["MATCH (n:Person) RETURN n.id AS id"]));
    assert_eq!(seq(&stored, "rows"), fixture.created + 1);
    assert!(
        stored.contains("\"type\":\"int\",\"value\":\"17\""),
        "{stored}"
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

#[test]
fn an_undeclared_text_column_names_the_types_file_fix_and_imports_once_declared() {
    let fixture = Fixture::new();
    let statement = "CREATE (n:Person {id:$id,name:$name})";
    fixture.file("people.csv", "name,id\nEdsger,1\nBarbara,2\n");
    // Types come from the statement, never from values: `$name` is an
    // undeclared property parameter, hence int64, and `Edsger` refuses.
    let undeclared = fixture.run("import-csv", &["--input", "people.csv", statement]);
    refusal(&undeclared, 3, "query");
    let out = String::from_utf8_lossy(&undeclared.stdout);
    assert!(
        out.contains("int64") && out.contains("--types-file"),
        "{out}"
    );
    assert!(
        !out.contains("Edsger"),
        "a refusal never echoes a value: {out}"
    );
    fixture.count(0);
    fixture.file("types", "text\tname\n");
    let imported = success(fixture.run(
        "import-csv",
        &["--input", "people.csv", "--types-file", "types", statement],
    ));
    assert!(imported.contains("\"records\":2"), "{imported}");
    fixture.count(2);
}
