//! Actual process-level MATCH/UNWIND MERGE, not the explicit --from-query
//! interface. Results are checked against independently reopened durable state.

use fgdb_protocol::json::{Json, parse_json};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const PEOPLE: &str = "CREATE (:Person {bornIn:'Paris'}),(:Person {bornIn:'Rome'}),\
    (:Person {bornIn:'Paris'})";
const MERGE: &str = "MATCH (p:Person) MERGE (c:City {name:p.bornIn}) \
    ON CREATE SET c.seen=1 ON MATCH SET c.seen=c.seen+1";

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
        Json::String(value) => value,
        _ => panic!("expected a string field: {value:?}"),
    }
}

fn number(value: &Json) -> u64 {
    match value {
        Json::Number(value) => value.parse().unwrap(),
        _ => panic!("expected a numeric field: {value:?}"),
    }
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
                .filter(|event| { matches!(string(field(event, "event")), "result" | "error") })
                .count(),
            1,
        );
        self
    }

    #[track_caller]
    fn refused(&self) -> &str {
        assert_eq!(self.code, 3, "{:?}\n{}", self.events, self.stderr);
        assert_eq!(
            self.events.len(),
            2,
            "no buffered prefix: {:?}",
            self.events
        );
        assert_eq!(string(field(&self.events[0], "event")), "invocation");
        assert_eq!(string(field(self.terminal(), "event")), "error");
        assert_eq!(string(field(self.terminal(), "class")), "query");
        let Json::Array(diagnostics) = field(self.terminal(), "diagnostics") else {
            panic!("expected error diagnostics")
        };
        assert_eq!(diagnostics.len(), 1);
        string(&diagnostics[0])
    }

    fn rows(&self) -> Vec<Vec<Json>> {
        self.events
            .iter()
            .filter(|event| string(field(event, "event")) == "row")
            .map(|event| match field(event, "cells") {
                Json::Array(cells) => cells.clone(),
                _ => panic!("expected row cells"),
            })
            .collect()
    }

    #[track_caller]
    fn integer_rows(&self, expected: &[&[i64]]) {
        let rows = self.rows();
        assert_eq!(rows.len(), expected.len(), "{:?}", self.events);
        for (row, expected) in rows.iter().zip(expected) {
            assert_eq!(row.len(), expected.len());
            for (cell, expected) in row.iter().zip(*expected) {
                assert_eq!(string(field(cell, "type")), "int");
                assert_eq!(string(field(cell, "value")), expected.to_string());
            }
        }
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
            "fgdb-cli-merge-final-{name}-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let keys = root.join("keys");
        std::fs::write(
            &keys,
            format!(
                "{}\n{}\n{}\n",
                "51".repeat(32),
                "52".repeat(32),
                "53".repeat(32),
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
                "--label",
                "Extra=3",
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
        assert!(stdout.ends_with('\n'), "complete NDJSON: {stdout:?}");
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

    #[track_caller]
    fn cities(&self, expected: &[(&str, i64)]) {
        let read = self.run(
            "query",
            &["MATCH (c:City) RETURN c.name AS name,c.seen AS seen ORDER BY name,seen"],
        );
        read.success("rows");
        let rows = read.rows();
        assert_eq!(rows.len(), expected.len());
        for (cells, &(name, value)) in rows.iter().zip(expected) {
            assert_eq!(string(field(&cells[0], "type")), "text");
            assert_eq!(string(field(&cells[0], "value")), name);
            assert_eq!(string(field(&cells[1], "type")), "int");
            assert_eq!(string(field(&cells[1], "value")), value.to_string());
        }
    }
}

#[test]
fn single_statement_merge_groups_pages_and_plain_writes_keep_every_occurrence() {
    let fixture = Fixture::new("standalone");
    fixture.run("write", &[PEOPLE]).success("written");
    let before = fixture.state().0;
    let grouped = format!("{MERGE} RETURN count(c) AS total,sum(c.seen) AS seen");
    let written = fixture.run("write", &["--max-result-rows", "1", &grouped]);
    written.success("written").integer_rows(&[&[3, 5]]);
    assert_eq!(written.seq(), before + 1);
    fixture.cities(&[("Paris", 2), ("Rome", 1)]);

    let paged = format!("{MERGE} RETURN c.name AS name LIMIT 0");
    let written = fixture.run("write", &["--max-result-rows", "0", &paged]);
    written.success("written").integer_rows(&[]);
    assert_eq!(written.seq(), before + 2);
    fixture.cities(&[("Paris", 4), ("Rome", 2)]);

    let plain = fixture.run("write", &["--max-result-rows", "0", MERGE]);
    plain.success("written").integer_rows(&[]);
    assert_eq!(plain.seq(), before + 3);
    fixture.cities(&[("Paris", 6), ("Rome", 3)]);

    let unwound = fixture.run(
        "write",
        &[
            "--max-result-rows",
            "1",
            "UNWIND ['Paris','Rome','Paris'] AS city MERGE (c:City {name:city}) \
         ON CREATE SET c.seen=1 ON MATCH SET c.seen=c.seen+1 \
         RETURN count(c) AS total,sum(c.seen) AS seen",
        ],
    );
    unwound.success("written").integer_rows(&[&[3, 20]]);
    assert_eq!(unwound.seq(), before + 4);
    fixture.cities(&[("Paris", 8), ("Rome", 4)]);

    let expected = fixture.state();
    fixture.run("compact", &[]).success("compacted");
    assert_eq!(
        fixture.state(),
        expected,
        "independent reopen after compaction"
    );
}

#[test]
fn transaction_merge_sees_earlier_creates_and_updates_all_existing_matches() {
    let fixture = Fixture::new("staged");
    fixture
        .run(
            "write",
            &[
                "CREATE (:City {name:'Paris',seen:4}),(:City {name:'Paris',seen:7}),\
         (:City {name:'Tokyo',seen:99})",
            ],
        )
        .success("written");
    let before = fixture.state().0;
    let grouped = format!("{MERGE} RETURN count(c) AS total,sum(c.seen) AS seen");
    let result = fixture.run(
        "transaction",
        &[
            "--max-result-rows",
            "1",
            "--write",
            PEOPLE,
            "--write",
            &grouped,
        ],
    );
    result.success("committed").integer_rows(&[&[5, 31]]);
    assert_eq!(result.seq(), before + 1, "one commit for both writes");
    fixture.cities(&[("Paris", 6), ("Paris", 9), ("Rome", 1), ("Tokyo", 99)]);
    let read = fixture.run("query", &["MATCH (p:Person) RETURN p"]);
    read.success("rows");
    assert_eq!(read.rows().len(), 3);
}

#[test]
fn late_return_refusal_discards_earlier_transaction_writes_and_buffered_rows() {
    let fixture = Fixture::new("abort");
    fixture.run("write", &[PEOPLE]).success("written");
    let before = fixture.state();
    const FIRST: &str = "CREATE (x:Extra {seen:99}) RETURN x.seen AS seen";
    for (tail, row_refusal) in [
        ("RETURN c.name AS name", true),
        ("RETURN 1/(c.seen-1) AS bad LIMIT 0", false),
    ] {
        let second = format!("{MERGE} {tail}");
        let failure = fixture.run(
            "transaction",
            &[
                "--max-result-rows",
                "1",
                "--write",
                FIRST,
                "--write",
                &second,
            ],
        );
        let message = failure.refused();
        assert!(message.contains("transaction step 2"), "{message}");
        assert_eq!(message.contains("ResultRows"), row_refusal, "{message}");
        assert_eq!(fixture.state(), before);
    }
    // Both steps succeed with the same types and source; no refusal can pass
    // merely because the prefix or whole statement was never executable.
    let second = format!("{MERGE} RETURN count(c) AS total,sum(c.seen) AS seen");
    let retry = fixture.run(
        "transaction",
        &[
            "--max-result-rows",
            "1",
            "--write",
            FIRST,
            "--write",
            &second,
        ],
    );
    retry.success("committed").integer_rows(&[&[99], &[3, 5]]);
    assert_eq!(retry.seq(), before.0 + 1);
    fixture.cities(&[("Paris", 2), ("Rome", 1)]);
}
