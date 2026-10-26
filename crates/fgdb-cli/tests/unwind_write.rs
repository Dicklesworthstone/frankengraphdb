//! Public-binary regressions for bounded native UNWIND writes and the other
//! driver ingest idioms (map parameters as property maps). Every command
//! reopens the database, so successful reads also exercise durable publication.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const UPSERT: &str = "UNWIND $rows AS row MERGE (n:Entity {id:row.id}) SET n.name=row.name";

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
        writeln!(
            keys,
            "{}\n{}\n{}",
            "11".repeat(32),
            "22".repeat(32),
            "33".repeat(32)
        )
        .unwrap();
        drop(keys);
        let fixture = Self {
            db: root.join("database"),
            root,
            key_file,
        };
        if create {
            success(&fixture.run("create", &[]));
        }
        fixture
    }

    fn run(&self, verb: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command
            .arg("--robot")
            .arg(verb)
            .arg("--db")
            .arg(&self.db)
            .arg("--key-file")
            .arg(&self.key_file);
        if verb != "create" {
            command.args([
                "--label",
                "Entity=1",
                "--relation",
                "LINK=1",
                "--property",
                "id=1",
                "--property",
                "name=2",
                "--property",
                "score=3",
                "--property",
                "weight=4",
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
    format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    )
}
fn success(result: &Output) {
    assert!(
        result.status.success(),
        "{}: {}",
        result.status,
        output(result)
    );
}
fn row_count(result: &Output) -> usize {
    String::from_utf8_lossy(&result.stdout)
        .lines()
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

/// The same batch through UNWIND text and through `write --rows`, which binds
/// one statement instance per object, leaves the same graph (fgdb-bapr6
/// acceptance). The repeated key upserts in row order on both paths.
/// Compared by property values, never by identities, which each path issues
/// for itself.
#[test]
fn unwind_upsert_and_write_rows_leave_the_same_graph() {
    let rows = r#"[{"id":1,"name":"Original"},{"id":2,"name":"Bob"},{"id":1,"name":"Updated"},{"id":3,"name":"Cy"}]"#;
    let unwind = Fixture::new(true);
    success(&unwind.write(rows, UPSERT));
    let per_row = Fixture::new(true);
    let per_row_rows = format!("json:{rows}");
    success(&per_row.run(
        "write",
        &[
            "--rows",
            &per_row_rows,
            "MERGE (n:Entity {id:$id}) SET n.name=$name",
        ],
    ));
    let graph = |fixture: &Fixture| -> Vec<String> {
        String::from_utf8_lossy(
            &fixture
                .query("MATCH (n:Entity) RETURN n.id AS id, n.name AS name ORDER BY id")
                .stdout,
        )
        .lines()
        .filter(|line| line.contains("\"event\":\"row\""))
        .map(str::to_owned)
        .collect()
    };
    let expected = graph(&unwind);
    assert_eq!(expected.len(), 3, "{expected:?}");
    for (row, name) in expected.iter().zip(["Updated", "Bob", "Cy"]) {
        assert!(row.contains(name), "{row}");
    }
    assert_eq!(graph(&per_row), expected);
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
    success(&fixture.write(
        r#"[{"id":1,"name":"A"},{"id":2,"name":"B"},{"id":3,"name":"C"}]"#,
        UPSERT,
    ));
    // A MERGE's SET takes literal or parameter values only, so the computed
    // value is set through the adapter's MATCH mutation form.
    let text = "UNWIND $rows AS row MATCH (n:Entity {id:row.id}) \
        SET n.score=10 / row.divisor";
    // Positive control: the same expression and binding shape really execute.
    success(&fixture.write(r#"[{"id":1,"divisor":2}]"#, text));
    let before = fixture.query("MATCH (n:Entity) RETURN n.id, n.score");
    let refused = fixture.write(r#"[{"id":2,"divisor":2},{"id":3,"divisor":0}]"#, text);
    assert!(!refused.status.success(), "{}", output(&refused));
    assert!(!output(&refused).contains("\"kind\":\"written\""));
    let after = fixture.query("MATCH (n:Entity) RETURN n.id, n.score");
    assert_eq!(
        before.stdout, after.stdout,
        "failed batch changed durable rows or frontier"
    );
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
    let selected =
        fixture.query("MATCH (a:Entity)-[e:LINK]->(b:Entity) WHERE e.weight=3 RETURN e.weight");
    assert_eq!(row_count(&selected), 1);
}

#[test]
fn existing_native_create_and_create_return_paths_are_preserved() {
    let fixture = Fixture::new(true);
    success(&fixture.run(
        "write",
        &["UNWIND [1,2] AS id CREATE (n:Entity {id:id, name:'Native'})"],
    ));
    let returning = fixture.run(
        "write",
        &["CREATE (n:Entity {id:3, name:'Returned'}) RETURN n.name"],
    );
    success(&returning);
    assert_eq!(row_count(&returning), 1);
    assert!(output(&returning).contains("Returned"));
    assert_eq!(row_count(&fixture.query("MATCH (n:Entity) RETURN n.id")), 3);
}

/// A driver sends a record as one map parameter (fgdb-0s2pq). `CREATE (n
/// $props)`, `SET n += $props`, `SET n = $props` and `MERGE ... ON CREATE SET
/// n += $props` bind as the literal map of the argument's keys, with or
/// without RETURN, and `$props.key` beside them still reads the argument. A
/// key the catalog does not bind refuses at the parameter's own byte.
#[test]
fn a_map_parameter_binds_as_the_property_map_it_holds() {
    let fixture = Fixture::new(true);
    let write = |json: &str, text: &str| {
        let parameter = format!("props=json:{json}");
        fixture.run("write", &["--param", &parameter, text])
    };
    // Each row event of `result` holds exactly the cells of one expected row.
    let rows = |result: &Output, expected: &[&[String]]| {
        let lines: Vec<String> = String::from_utf8_lossy(&result.stdout)
            .lines()
            .filter(|line| line.contains("\"event\":\"row\""))
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), expected.len(), "{}", output(result));
        for (line, cells) in lines.iter().zip(expected) {
            let cells = format!("\"cells\":[{}]", cells.join(","));
            assert!(line.contains(&cells), "{line} lacks {cells}");
        }
    };
    let int = |value: i64| format!(r#"{{"type":"int","value":"{value}"}}"#);
    let text = |value: &str| format!(r#"{{"type":"text","value":"{value}"}}"#);
    let null = || r#"{"type":"null"}"#.to_owned();

    success(&write(
        r#"{"id":1,"name":"Ada","score":3}"#,
        "CREATE (n:Entity $props)",
    ));
    let created = write(
        r#"{"id":2,"name":"Bob"}"#,
        "CREATE (n:Entity $props) RETURN n.id, n.name",
    );
    success(&created);
    rows(&created, &[&[int(2), text("Bob")]]);
    // += keeps every key the map does not name.
    let merged = write(
        r#"{"score":7,"weight":2}"#,
        "MATCH (n:Entity {id: 1}) SET n += $props RETURN n.name, n.score, n.weight",
    );
    success(&merged);
    rows(&merged, &[&[text("Ada"), int(7), int(2)]]);
    // = replaces every property with the map's own.
    success(&write(
        r#"{"id":2,"score":5}"#,
        "MATCH (n:Entity {id: 2}) SET n = $props",
    ));
    let upsert = "MERGE (n:Entity {id: 3}) ON CREATE SET n += $props \
                  ON MATCH SET n.score = n.score + 1 RETURN n.name, n.score";
    let first = write(r#"{"name":"Cy","score":1}"#, upsert);
    success(&first);
    rows(&first, &[&[text("Cy"), int(1)]]);
    let second = write(r#"{"name":"Cy","score":1}"#, upsert);
    success(&second);
    rows(&second, &[&[text("Cy"), int(2)]]);
    let both = write(
        r#"{"score":9}"#,
        "MATCH (n:Entity {id: 3}) SET n += $props, n.weight = $props.score RETURN n.weight",
    );
    success(&both);
    rows(&both, &[&[int(9)]]);
    let graph = fixture.query(
        "MATCH (n:Entity) RETURN n.id AS id, n.name AS name, n.score AS score, \
         n.weight AS weight ORDER BY id",
    );
    rows(
        &graph,
        &[
            &[int(1), text("Ada"), int(7), int(2)],
            &[int(2), null(), int(5), null()],
            &[int(3), text("Cy"), int(9), int(9)],
        ],
    );

    for statement in [
        "CREATE (n:Entity $props)",
        "CREATE (n:Entity $props) RETURN n.id",
    ] {
        let refused = write(r#"{"id":4,"zz":1}"#, statement);
        assert!(!refused.status.success(), "{statement}");
        let at = format!("\"offset\":{}", statement.find('$').unwrap());
        assert!(output(&refused).contains(&at), "{}", output(&refused));
    }
    assert_eq!(row_count(&fixture.query("MATCH (n:Entity) RETURN n")), 3);

    // A record's array is a stored List, through the spliced map, with or
    // without RETURN.
    let list = || {
        r#"{"type":"list","value":[{"type":"int","value":"1"},{"type":"int","value":"2"}]}"#
            .to_owned()
    };
    success(&write(
        r#"{"id":5,"name":"Eve","weight":[1,2]}"#,
        "CREATE (n:Entity $props)",
    ));
    let listed = write(
        r#"{"id":6,"name":"Fay","weight":[1,2]}"#,
        "CREATE (n:Entity $props) RETURN n.weight",
    );
    success(&listed);
    rows(&listed, &[&[list()]]);
    let weights = fixture.query("MATCH (n:Entity) WHERE n.id >= 5 RETURN n.weight ORDER BY n.id");
    rows(&weights, &[&[list()], &[list()]]);
}

#[path = "unwind_write/document_roots.rs"]
mod document_roots;
