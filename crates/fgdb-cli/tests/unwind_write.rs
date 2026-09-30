//! Public-binary regressions for bounded native UNWIND writes. Every command
//! reopens the database, so successful reads also exercise durable publication.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const UPSERT: &str =
    "UNWIND $rows AS row MERGE (n:Entity {id:row.id}) SET n.name=row.name";

struct Fixture {
    root: PathBuf,
    db: PathBuf,
    key_file: PathBuf,
}
impl Fixture {
    fn new(create: bool) -> Self {
        let root = std::env::temp_dir().join(format!(
            "fgdb-native-unwind-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let key_file = root.join("keys");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut keys = options.open(&key_file).unwrap();
        writeln!(keys, "{}\n{}\n{}", "11".repeat(32), "22".repeat(32), "33".repeat(32))
            .unwrap();
        drop(keys);
        let fixture = Self { db: root.join("database"), root, key_file };
        if create {
            success(&fixture.run("create", &[]));
        }
        fixture
    }

    fn run(&self, verb: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command.arg("--robot").arg(verb)
            .arg("--db").arg(&self.db)
            .arg("--key-file").arg(&self.key_file);
        if verb != "create" {
            command.args([
                "--label", "Entity=1",
                "--relation", "LINK=1",
                "--property", "id=1",
                "--property", "name=2",
                "--property", "score=3",
                "--property", "weight=4",
            ]);
        }
        command.args(args).output().unwrap()
    }

    fn write(&self, json: &str, text: &str) -> Output {
        let parameter = format!("rows=json:{json}");
        self.run("write", &["--param", &parameter, text])
    }

    fn query(&self, text: &str) -> Output {
        let result = self.run("query", &[text]);
        success(&result);
        result
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only the fresh directory owned by this fixture is removed.
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn output(result: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr))
}
fn success(result: &Output) {
    assert!(result.status.success(), "{}: {}", result.status, output(result));
}
fn row_count(result: &Output) -> usize {
    String::from_utf8_lossy(&result.stdout).lines()
        .filter(|line| line.contains("\"event\":\"row\""))
        .count()
}

#[test]
fn duplicate_input_keys_upsert_sequentially_in_one_program() {
    let fixture = Fixture::new(true);
    let written = fixture.write(
        r#"[{"id":1,"name":"Original"},{"id":2,"name":"Bob"},{"id":1,"name":"Updated"}]"#,
        UPSERT,
    );
    success(&written);
    assert!(output(&written).contains("\"statements\":3"));
    let result = fixture.query("MATCH (n:Entity) RETURN n.name");
    assert_eq!(row_count(&result), 2);
    assert!(output(&result).contains("Updated"));
    assert!(output(&result).contains("Bob"));
    assert!(!output(&result).contains("Original"));
}

#[test]
fn row_values_are_not_query_text_and_missing_fields_remain_null() {
    let fixture = Fixture::new(true);
    success(&fixture.write(
        r#"[{"id":1,"name":"Ada'; MATCH (n) DETACH DELETE n; //"},{"id":2,"name":"Keep"}]"#,
        UPSERT,
    ));
    let before = fixture.query("MATCH (n:Entity) RETURN n.name");
    assert_eq!(row_count(&before), 2);
    assert!(output(&before).contains("DETACH DELETE"));
    success(&fixture.write(r#"[{"id":1},{"id":2,"name":"Keep"}]"#, UPSERT));
    let after = fixture.query("MATCH (n:Entity) RETURN n.name");
    assert_eq!(row_count(&after), 2);
    assert!(output(&after).contains("Keep"));
    assert!(!output(&after).contains("DETACH DELETE"));
}

#[test]
fn invalid_late_rows_are_refused_before_opening_the_database() {
    let fixture = Fixture::new(false);
    for json in [
        r#"[{"id":1,"name":"Good"},{"id":2,"name":true}]"#,
        r#"[{"id":1,"name":"Good"},42]"#,
        "[]",
    ] {
        let result = fixture.write(json, UPSERT);
        assert_eq!(result.status.code(), Some(3), "{}", output(&result));
        assert!(!fixture.db.exists());
        assert!(!output(&result).contains("\"kind\":\"written\""));
    }
}

#[test]
fn a_late_arithmetic_failure_publishes_no_prefix() {
    let fixture = Fixture::new(true);
    let text = "UNWIND $rows AS row MERGE (n:Entity {id:row.id}) \
        SET n.score=10 / row.divisor";
    // Positive control: the same expression and binding shape really execute.
    success(&fixture.write(r#"[{"id":1,"divisor":2}]"#, text));
    let before = fixture.query("MATCH (n:Entity) RETURN n.id, n.score");
    let refused = fixture.write(r#"[{"id":2,"divisor":2},{"id":3,"divisor":0}]"#, text);
    assert!(!refused.status.success(), "{}", output(&refused));
    assert!(!output(&refused).contains("\"kind\":\"written\""));
    let after = fixture.query("MATCH (n:Entity) RETURN n.id, n.score");
    assert_eq!(before.stdout, after.stdout, "failed batch changed durable rows or frontier");
}

#[test]
fn relationship_merge_reads_the_prior_row_overlay() {
    let fixture = Fixture::new(true);
    success(&fixture.write(r#"[{"id":1,"name":"A"},{"id":2,"name":"B"}]"#, UPSERT));
    let text = "UNWIND $rows AS row MATCH (a:Entity), (b:Entity) \
        WHERE a.id=row.source AND b.id=row.target \
        MERGE (a)-[e:LINK]->(b) SET e.weight=row.weight";
    success(&fixture.write(
        r#"[{"source":1,"target":2,"weight":1},{"source":1,"target":2,"weight":3}]"#,
        text,
    ));
    let result = fixture.query("MATCH (a:Entity)-[e:LINK]->(b:Entity) RETURN e.weight");
    assert_eq!(row_count(&result), 1);
    let selected = fixture.query(
        "MATCH (a:Entity)-[e:LINK]->(b:Entity) WHERE e.weight=3 RETURN e.weight",
    );
    assert_eq!(row_count(&selected), 1);
}

#[test]
fn existing_native_create_and_create_return_paths_are_preserved() {
    let fixture = Fixture::new(true);
    success(&fixture.run("write", &[
        "UNWIND [1,2] AS id CREATE (n:Entity {id:id, name:'Native'})",
    ]));
    let returning = fixture.run("write", &[
        "CREATE (n:Entity {id:3, name:'Returned'}) RETURN n.name",
    ]);
    success(&returning);
    assert_eq!(row_count(&returning), 1);
    assert!(output(&returning).contains("Returned"));
    assert_eq!(row_count(&fixture.query("MATCH (n:Entity) RETURN n.id")), 3);
}
