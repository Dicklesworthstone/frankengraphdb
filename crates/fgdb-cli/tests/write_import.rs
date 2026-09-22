//! Real CLI processes exercise durable reopen and the native atomic writer.
//! Temporary artifacts are retained for diagnosis; no fixture deletes files.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture { dir: PathBuf }
impl Fixture {
    fn new() -> Self {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("fgdb-cli-import-{}-{now}-{}",
            std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&dir).unwrap();
        let fixture = Self { dir };
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let mut keys = open.open(fixture.dir.join("keys")).unwrap();
        keys.write_all(&[0x31; 32]).unwrap();
        keys.write_all(&[0x32; 32]).unwrap();
        keys.write_all(&[0x33; 32]).unwrap();
        drop(keys);
        fixture.file("symbols", "label\tPerson\t1\nproperty\tid\t1\nproperty\tname\t2\nproperty\tscore\t3\n");
        success(fixture.run("init", &[]));
        fixture
    }
    fn file(&self, name: &str, text: &str) {
        let mut file = OpenOptions::new().write(true).create_new(true)
            .open(self.dir.join(name)).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
    fn command(&self, operation: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command.current_dir(&self.dir).arg(operation)
            .arg("--db").arg(self.dir.join("db"))
            .args(["--keys-file", "keys", "--format", "ndjson"]);
        if operation != "init" {
            command.args(["--symbols-file", "symbols"]);
        }
        if matches!(operation, "write" | "import-csv") {
            command.args(["--relation", "1"]);
        }
        command
    }
    fn run(&self, operation: &str, args: &[&str]) -> Output {
        self.command(operation).args(args).output().unwrap()
    }
    fn count(&self, expected: u64) {
        let name = format!("count-{}.gql", NEXT.fetch_add(1, Ordering::Relaxed));
        self.file(&name, "MATCH (n:Person) RETURN COUNT(n) AS total");
        let out = success(self.run("query", &["--query-file", &name]));
        assert!(out.contains(&format!("\"type\":\"count\",\"value\":\"{expected}\"")), "{out}");
    }
}

fn success(output: Output) -> String {
    assert!(output.status.success(), "status {:?}\nstdout {}\nstderr {}",
        output.status.code(), String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}
fn refusal(output: Output, code: &str) {
    assert!(!output.status.success());
    let out = String::from_utf8(output.stdout).unwrap();
    assert!(out.contains(&format!("\"type\":\"error\",\"code\":\"{code}\"")), "{out}");
    assert!(!out.contains("\"type\":\"complete\""));
}

#[test]
fn typed_writes_and_csv_imports_survive_separate_process_reopens() {
    let fixture = Fixture::new();
    fixture.file("write.gql", "CREATE (n:Person {id:$id,name:$name})");
    fixture.file("arguments", "int64\tid\t7\ntext\tname\t'; MATCH (n) DELETE n; --\n");
    let written = success(fixture.run("write", &["--query-file", "write.gql", "--params-file", "arguments"]));
    assert!(written.contains("\"completion\":\"write_committed\""));
    fixture.count(1);
    fixture.file("types", "text\tname\n");
    fixture.file("data.csv", "name,id\n\"quoted, \"\"name\"\"\",8\n\"multi\n雪\",9\n");
    let imported = success(fixture.run("import-csv", &["--query-file", "write.gql",
        "--csv-file", "data.csv", "--types-file", "types"]));
    assert!(imported.contains("\"records\":2,\"statements\":2"));
    fixture.count(3);
    fixture.file("read.gql", "MATCH (n:Person) WHERE n.id=9 RETURN COUNT(n) AS total");
    let out = success(fixture.run("query", &["--query-file", "read.gql"]));
    assert!(out.contains("\"type\":\"count\",\"value\":\"1\""));
}

#[test]
fn a_late_execution_failure_rolls_back_every_earlier_csv_record() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person {id:$id}); MATCH (n:Person) WHERE n.id=$id SET n.score=100/$denom");
    fixture.file("bad.csv", "id,denom\n1,1\n2,0\n");
    refusal(fixture.run("import-csv", &["--query-file", "script.gql", "--csv-file", "bad.csv"]), "write_failed");
    fixture.count(0);
    fixture.file("good.csv", "id,denom\n1,2\n2,4\n");
    let out = success(fixture.run("import-csv", &["--query-file", "script.gql", "--csv-file", "good.csv"]));
    assert!(out.contains("\"records\":2,\"statements\":4"));
    fixture.count(2);
    fixture.file("scores.gql", "MATCH (n:Person) WHERE n.score=25 RETURN COUNT(n) AS total");
    let scores = success(fixture.run("query", &["--query-file", "scores.gql"]));
    assert!(scores.contains("\"type\":\"count\",\"value\":\"1\""));
}

#[test]
fn malformed_last_record_and_output_limits_refuse_before_mutation() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person {id:$id})");
    fixture.file("bad.csv", "id\n1\n2\nsensitive-invalid-integer\n");
    let output = fixture.run("import-csv", &["--query-file", "script.gql", "--csv-file", "bad.csv"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sensitive"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive"));
    refusal(output, "invalid_input");
    fixture.count(0);
    fixture.file("good.csv", "id\n1\n2\n");
    refusal(fixture.run("import-csv", &["--query-file", "script.gql", "--csv-file", "good.csv",
        "--max-output-bytes", "1"]), "output_limit");
    fixture.count(0);
    refusal(fixture.run("import-csv", &["--query-file", "script.gql", "--csv-file", "good.csv",
        "--max-input-bytes", "3"]), "invalid_input");
    fixture.count(0);
}

#[test]
fn engine_identities_remain_spent_after_delete_and_reopen() {
    let fixture = Fixture::new();
    fixture.file("create.gql", "CREATE (n:Person)");
    success(fixture.run("write", &["--query-file", "create.gql"]));
    success(fixture.run("write", &["--query-file", "create.gql"]));
    fixture.file("delete.gql", "MATCH (n:Person) DELETE n");
    success(fixture.run("write", &["--query-file", "delete.gql"]));
    fixture.count(0);
    success(fixture.run("write", &["--query-file", "create.gql"]));
    fixture.file("ids.gql", "MATCH (n:Person) RETURN n");
    let out = success(fixture.run("query", &["--query-file", "ids.gql"]));
    assert!(out.contains("\"type\":\"vertex\",\"value\":\"3\""), "{out}");
    fixture.count(1);
}

#[test]
fn read_command_never_falls_back_to_write_and_no_effect_writes_close_read_only() {
    let fixture = Fixture::new();
    fixture.file("create.gql", "CREATE (n:Person)");
    refusal(fixture.run("query", &["--query-file", "create.gql"]), "query_refused");
    fixture.count(0);
    fixture.file("none.gql", "MATCH (n:Person) SET n.score=1");
    let out = success(fixture.run("write", &["--query-file", "none.gql"]));
    assert!(out.contains("\"completion\":\"read_closed\""), "{out}");
    fixture.count(0);
}

#[test]
fn whole_program_creation_budget_does_not_reset_between_records() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person {id:$id})");
    fixture.file("data.csv", "id\n1\n2\n");
    refusal(fixture.run("import-csv", &["--query-file", "script.gql", "--csv-file", "data.csv",
        "--max-changes", "1"]), "write_failed");
    fixture.count(0);
    success(fixture.run("import-csv", &["--query-file", "script.gql", "--csv-file", "data.csv",
        "--max-changes", "2"]));
    fixture.count(2);
}

#[test]
fn csv_stdin_is_bounded_and_uses_the_same_atomic_path() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person {id:$id})");
    let mut child = fixture.command("import-csv").args(["--query-file", "script.gql", "--csv-file", "-"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"id\n1\n2\n").unwrap();
    drop(stdin);
    success(child.wait_with_output().unwrap());
    fixture.count(2);
}

#[cfg(target_os = "linux")]
#[test]
fn failed_stdout_does_not_relabel_a_durable_write_as_rolled_back() {
    let fixture = Fixture::new();
    fixture.file("script.gql", "CREATE (n:Person)");
    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    let output = fixture.command("write").args(["--query-file", "script.gql"])
        .stdout(Stdio::from(full)).stderr(Stdio::piped()).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(String::from_utf8(output.stderr).unwrap(), "fgdb: write_completed_output_failed\n");
    fixture.count(1);
}
