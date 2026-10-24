//! Invoke the real CLI: preparation, writable open, one native transaction,
//! robot completion and subsequent independent reads all remain in the path.

use fgdb_protocol::json::{Json, parse_json};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    database: PathBuf,
    keys: PathBuf,
}
struct Outcome {
    code: i32,
    stderr: String,
    events: Vec<Json>,
}

fn field<'a>(value: &'a Json, key: &str) -> &'a Json {
    match value {
        Json::Object(fields) => fields.get(key),
        _ => None,
    }
    .expect("required frame field")
}
fn string(value: &Json) -> &str {
    match value {
        Json::String(value) => Some(value.as_str()),
        _ => None,
    }
    .expect("string frame field")
}
fn number(value: &Json) -> u64 {
    match value {
        Json::Number(value) => value.parse().ok(),
        _ => None,
    }
    .expect("integer frame field")
}
fn array(value: &Json) -> &[Json] {
    match value {
        Json::Array(value) => Some(value.as_slice()),
        _ => None,
    }
    .expect("array frame field")
}
impl Outcome {
    fn terminal(&self) -> &Json {
        self.events.last().expect("terminal frame")
    }
    fn seq(&self) -> u64 {
        number(field(self.terminal(), "seq"))
    }
    #[track_caller]
    fn success(&self, kind: &str) -> &Self {
        assert_eq!(self.code, 0, "{:?}\n{}", self.events, self.stderr);
        assert!(self.stderr.is_empty(), "{}", self.stderr);
        assert_eq!(string(field(self.terminal(), "event")), "result");
        assert_eq!(string(field(self.terminal(), "kind")), kind);
        assert_eq!(
            self.events
                .iter()
                .filter(|e| { matches!(string(field(e, "event")), "result" | "error") })
                .count(),
            1
        );
        self
    }
    #[track_caller]
    fn refused(&self, code: i32) {
        assert_eq!(self.code, code, "{:?}\n{}", self.events, self.stderr);
        assert_eq!(self.events.len(), 2, "no partial output: {:?}", self.events);
        assert_eq!(string(field(&self.events[0], "event")), "invocation");
        assert_eq!(string(field(self.terminal(), "event")), "error");
        assert_eq!(
            string(field(self.terminal(), "class")),
            if code == 2 { "usage" } else { "query" }
        );
    }
    fn rows(&self) -> Vec<Vec<Json>> {
        self.events
            .iter()
            .filter(|e| string(field(e, "event")) == "row")
            .map(|e| array(field(e, "cells")).to_vec())
            .collect()
    }
}
impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fgdb-cli-query-write-{name}-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let keys = root.join("keys");
        std::fs::write(
            &keys,
            format!(
                "{}\n{}\n{}\n",
                "41".repeat(32),
                "42".repeat(32),
                "43".repeat(32)
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let fixture = Self {
            database: root.join("db"),
            keys,
        };
        fixture.run("create", &[]).success("created");
        fixture
    }
    fn run(&self, command: &str, args: &[&str]) -> Outcome {
        let output = Command::new(env!("CARGO_BIN_EXE_fgdb"))
            .args(["--robot", command, "--db"])
            .arg(&self.database)
            .arg("--key-file")
            .arg(&self.keys)
            .args([
                "--label",
                "Person=1",
                "--label",
                "City=2",
                "--property",
                "bornIn=1",
                "--property",
                "name=2",
                "--property",
                "seen=3",
            ])
            .args(args)
            .output()
            .expect("run built fgdb binary");
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        assert!(stdout.ends_with('\n'), "complete frames: {stdout:?}");
        Outcome {
            code: output.status.code().expect("no signal"),
            stderr: String::from_utf8(output.stderr).unwrap(),
            events: stdout
                .lines()
                .map(|line| parse_json(line, 10_000, 65_536).unwrap())
                .collect(),
        }
    }
    fn state(&self) -> (u64, Vec<Vec<Json>>) {
        let read = self.run(
            "query",
            &["MATCH (n) RETURN n,n.bornIn AS origin,n.name AS name,n.seen AS seen ORDER BY n"],
        );
        read.success("rows");
        (read.seq(), read.rows())
    }
}

#[test]
fn duplicate_text_keys_freeze_source_and_publish_one_durable_commit() {
    let fixture = Fixture::new("duplicate");
    fixture.run("write", &[
        "CREATE (:Person {bornIn:'Paris'}),(:Person {bornIn:'Rome'}),(:Person {bornIn:'Paris'})",
    ]).success("written");
    let before = fixture.state().0;
    let output = fixture.run(
        "write",
        &[
            "--from-query",
            "MATCH (p:Person) RETURN p.bornIn AS city ORDER BY city",
            "--feed-type",
            "city=text",
            "--max-batch-statements",
            "6",
            "MERGE (c:City {name:$city}) ON CREATE SET c.seen=1 ON MATCH SET c.seen=c.seen+1; \
         CREATE (:Person {bornIn:$city})",
        ],
    );
    output.success("written");
    assert_eq!(output.events.len(), 2);
    assert_eq!(output.seq(), before + 1);
    assert_eq!(number(field(output.terminal(), "records")), 3);
    assert_eq!(number(field(output.terminal(), "statements")), 6);
    let rows = fixture.run(
        "query",
        &["MATCH (c:City) RETURN c.name AS name,c.seen AS seen ORDER BY name"],
    );
    rows.success("rows");
    let cities = rows.rows();
    assert_eq!(cities.len(), 2);
    for (cells, (name, count)) in cities.iter().zip([("Paris", "2"), ("Rome", "1")]) {
        assert_eq!(string(field(&cells[0], "type")), "text");
        assert_eq!(string(field(&cells[0], "value")), name);
        assert_eq!(string(field(&cells[1], "type")), "int");
        assert_eq!(string(field(&cells[1], "value")), count);
    }
    let people = fixture.run("query", &["MATCH (p:Person) RETURN p"]);
    people.success("rows");
    assert_eq!(
        people.rows().len(),
        6,
        "writes must not rerun and enlarge source input"
    );
    let expected = fixture.state();
    fixture.run("compact", &[]).success("compacted");
    assert_eq!(
        fixture.state(),
        expected,
        "fresh-process reopen after compaction"
    );
}

#[test]
fn late_binding_and_action_failures_leave_no_prefix_or_success_record() {
    let fixture = Fixture::new("rollback");
    let before = fixture.state();
    for source in [
        "UNWIND [1,null] AS key RETURN key",
        "UNWIND [1,[2]] AS key RETURN key",
        "UNWIND [1,'wrong'] AS key RETURN key",
    ] {
        fixture
            .run(
                "write",
                &["--from-query", source, "MERGE (c:City {name:$key})"],
            )
            .refused(3);
        assert_eq!(fixture.state(), before);
    }
    fixture
        .run(
            "write",
            &[
                "--from-query",
                "UNWIND [2,0] AS denominator RETURN 1 AS key,denominator ORDER BY denominator DESC",
                "MERGE (c:City {name:$key}) ON CREATE SET c.seen=10 \
         ON MATCH SET c.seen=c.seen/$denominator",
            ],
        )
        .refused(3);
    assert_eq!(fixture.state(), before);
    let retry = fixture.run(
        "write",
        &[
            "--from-query",
            "RETURN 1 AS key",
            "MERGE (c:City {name:$key})",
        ],
    );
    retry.success("written");
    assert_eq!(retry.seq(), before.0 + 1);
    assert_eq!(fixture.state().1.len(), 1);
}

#[test]
fn source_binding_and_writes_share_limits_and_empty_input_closes_without_a_commit() {
    let fixture = Fixture::new("limits");
    fixture
        .run(
            "write",
            &["CREATE (:Person {bornIn:1}),(:Person {bornIn:2}),(:Person {bornIn:1})"],
        )
        .success("written");
    const SOURCE: &str = "MATCH (p:Person) RETURN p.bornIn AS key";
    const SCRIPT: &str =
        "MERGE (c:City {name:$key}) ON CREATE SET c.seen=1 ON MATCH SET c.seen=c.seen+1";
    fixture
        .run("write", &["--from-query", SOURCE, SCRIPT])
        .success("written");
    let before = fixture.state();
    for flag in [
        "--max-snapshot-records",
        "--max-result-rows",
        "--max-work-units",
        "--max-scratch-entries",
    ] {
        fixture
            .run("write", &["--from-query", SOURCE, flag, "0", SCRIPT])
            .refused(3);
        assert_eq!(fixture.state(), before, "{flag}");
    }
    // Three read rows spend the whole selected-row allowance: the script
    // cannot obtain a fresh three rows just because execution changes phase.
    fixture
        .run(
            "write",
            &["--from-query", SOURCE, "--max-result-rows", "3", SCRIPT],
        )
        .refused(3);
    assert_eq!(fixture.state(), before);
    fixture
        .run(
            "write",
            &[
                "--from-query",
                SOURCE,
                "--max-batch-statements",
                "2",
                SCRIPT,
            ],
        )
        .refused(3);
    assert_eq!(fixture.state(), before);
    let repeated = format!("{SCRIPT}; {SCRIPT}");
    fixture
        .run(
            "write",
            &[
                "--from-query",
                SOURCE,
                "--max-batch-statements",
                "5",
                &repeated,
            ],
        )
        .refused(3);
    assert_eq!(fixture.state(), before);
    let empty = fixture.run(
        "write",
        &[
            "--from-query",
            "UNWIND [] AS key RETURN key",
            "--max-batch-statements",
            "0",
            "--max-result-rows",
            "0",
            SCRIPT,
        ],
    );
    empty.success("written");
    assert_eq!(number(field(empty.terminal(), "records")), 0);
    assert_eq!(number(field(empty.terminal(), "statements")), 0);
    assert_eq!(empty.seq(), before.0);
    assert_eq!(fixture.state(), before);
}

#[test]
fn source_parameters_are_explicit_and_invalid_flag_combinations_refuse() {
    let fixture = Fixture::new("parameters");
    let success = fixture.run(
        "write",
        &[
            "--from-query",
            "RETURN $key AS key",
            "--param",
            "key=int:7",
            "--feed-type",
            "key=int64",
            "MERGE (c:City {name:$key})",
        ],
    );
    success.success("written");
    assert_eq!(number(field(success.terminal(), "records")), 1);
    let before = fixture.state();
    fixture
        .run(
            "write",
            &[
                "--from-query",
                "RETURN $key AS other",
                "--param",
                "key=int:7",
                "MERGE (c:City {name:$key})",
            ],
        )
        .refused(3);
    for flags in [
        vec!["--from-query", "RETURN 1 AS key", "--rows", "json:[]"],
        vec!["--feed-type", "key=text"],
        vec!["--max-batch-statements", "1"],
        vec![
            "--from-query",
            "RETURN 1 AS key",
            "--from-query",
            "RETURN 2 AS key",
        ],
    ] {
        let mut args = flags;
        args.push("MERGE (c:City {name:$key})");
        fixture.run("write", &args).refused(2);
    }
    assert_eq!(fixture.state(), before);
}
