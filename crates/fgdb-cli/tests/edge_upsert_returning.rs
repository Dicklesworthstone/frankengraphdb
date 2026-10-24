//! CLI parity for native relationship MERGE with ON/SET and RETURN.
//! Each operation invokes the real binary against a durable database. The
//! expected property values are independent of the query implementation.

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

fn field<'a>(value: &'a Json, name: &str) -> &'a Json {
    match value {
        Json::Object(fields) => fields.get(name),
        _ => None,
    }
    .expect("required frame field")
}

fn string(value: &Json) -> &str {
    match value {
        Json::String(text) => Some(text.as_str()),
        _ => None,
    }
    .expect("string frame field")
}

fn number(value: &Json) -> u64 {
    match value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
    .expect("unsigned frame field")
}

fn array(value: &Json) -> &[Json] {
    match value {
        Json::Array(values) => Some(values.as_slice()),
        _ => None,
    }
    .expect("array frame field")
}

impl Outcome {
    #[track_caller]
    fn success(&self, kind: &str) -> &Self {
        assert_eq!(self.code, 0, "{:?}\n{}", self.events, self.stderr);
        assert!(self.stderr.is_empty(), "{}", self.stderr);
        assert_eq!(
            self.events
                .iter()
                .filter(|event| matches!(string(field(event, "event")), "error" | "result"))
                .count(),
            1,
        );
        assert_eq!(string(field(self.terminal(), "event")), "result");
        assert_eq!(string(field(self.terminal(), "kind")), kind);
        self
    }

    #[track_caller]
    fn refusal(&self) {
        assert_eq!(self.code, 3, "{:?}\n{}", self.events, self.stderr);
        assert_eq!(
            self.events.len(),
            2,
            "no staged output may escape: {:?}",
            self.events,
        );
        assert_eq!(string(field(&self.events[0], "event")), "invocation");
        assert_eq!(string(field(self.terminal(), "event")), "error");
        assert_eq!(string(field(self.terminal(), "class")), "query");
        assert!(!array(field(self.terminal(), "diagnostics")).is_empty());
    }

    fn terminal(&self) -> &Json {
        self.events.last().expect("terminal frame")
    }

    fn seq(&self) -> u64 {
        number(field(self.terminal(), "seq"))
    }

    fn rows(&self, step: Option<u64>) -> Vec<Vec<Json>> {
        self.events
            .iter()
            .filter(|event| {
                string(field(event, "event")) == "row"
                    && step.is_none_or(|step| number(field(event, "statement")) == step)
            })
            .map(|event| array(field(event, "cells")).to_vec())
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
            "fgdb-cli-edge-upsert-{name}-{}-{now}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let keys = root.join("keys");
        std::fs::write(
            &keys,
            format!(
                "{}\n{}\n{}\n",
                "61".repeat(32),
                "62".repeat(32),
                "63".repeat(32),
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
            .run("write", &["CREATE (:Person {p:1}), (:Person {p:2})"])
            .success("written");
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
                "--relation",
                "R=1",
                "--property",
                "p=1",
                "--property",
                "q=2",
            ])
            .args(args)
            .output()
            .expect("run built fgdb binary");
        let text = std::str::from_utf8(&output.stdout).unwrap();
        assert!(text.ends_with('\n'), "complete NDJSON frames: {text:?}");
        Outcome {
            code: output.status.code().expect("no signal"),
            stderr: String::from_utf8(output.stderr).unwrap(),
            events: text
                .lines()
                .map(|line| parse_json(line, 10_000, 65_536).unwrap())
                .collect(),
        }
    }

    fn edges(&self) -> Outcome {
        let output = self.run(
            "query",
            &["MATCH ()-[r:R]->() RETURN r,r.p AS p,r.q AS q ORDER BY r"],
        );
        output.success("rows");
        output
    }

    fn state(&self) -> (u64, Vec<Vec<Json>>, Vec<Vec<Json>>) {
        let vertices = self.run(
            "query",
            &["MATCH (n:Person) RETURN n,n.p AS p,n.q AS q ORDER BY n"],
        );
        vertices.success("rows");
        let edges = self.edges();
        assert_eq!(vertices.seq(), edges.seq());
        (edges.seq(), vertices.rows(None), edges.rows(None))
    }
}

fn scalar_int(value: i64) -> Json {
    parse_json(&format!(r#"{{"type":"int","value":"{value}"}}"#), 8, 64).unwrap()
}

const UPSERT: &str = "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 \
    MERGE (a)-[r:R {q:7}]->(b) ON CREATE SET r.p=1 ON MATCH SET r.p=r.p+10 \
    SET r.p=r.p+$delta RETURN r,r.p AS p";

#[test]
fn create_match_and_reversed_keyed_upserts_return_the_final_durable_property() {
    let db = Fixture::new("branches");
    let basis = db.edges().seq();
    let first = db.run("write", &[UPSERT, "--param", "delta=int:2"]);
    first.success("written");
    let first_rows = first.rows(None);
    assert_eq!(first_rows.len(), 1);
    assert_eq!(first_rows[0][1], scalar_int(3));
    let edge = first_rows[0][0].clone();
    assert_eq!(string(field(&edge, "type")), "edge");
    assert_eq!(first.seq(), basis + 1);

    let second = db.run("write", &[UPSERT, "--param", "delta=int:4"]);
    second.success("written");
    assert_eq!(second.rows(None), [vec![edge.clone(), scalar_int(17)]]);
    assert_eq!(second.seq(), first.seq() + 1);

    let reversed = db.run(
        "write",
        &["MATCH (a:Person),(b:Person) WHERE a.p=2 AND b.p=1 \
         MERGE (a)-[r:R {q:7}]-(b) ON CREATE SET r.p=999 \
         ON MATCH SET r.p=r.p+1 RETURN r,r.p AS p"],
    );
    reversed.success("written");
    assert_eq!(reversed.rows(None), [vec![edge.clone(), scalar_int(18)]]);
    assert_eq!(
        db.edges().rows(None),
        [vec![edge, scalar_int(18), scalar_int(7)]],
    );
}

#[test]
fn transaction_rows_are_frozen_and_savepoint_rollback_removes_discarded_previews() {
    let db = Fixture::new("savepoints");
    let basis = db.edges().seq();
    let output = db.run(
        "transaction",
        &[
            "--write",
            UPSERT,
            "--param",
            "delta=int:2",
            "--savepoint",
            "s",
            "--write",
            UPSERT,
            "--param",
            "delta=int:4",
            "--rollback-to",
            "s",
            "--write",
            UPSERT,
            "--param",
            "delta=int:1",
            "--query",
            "MATCH ()-[r:R]->() RETURN r.p AS p",
        ],
    );
    output.success("committed");
    assert_eq!(output.seq(), basis + 1, "one transaction, one commit");
    let first = output.rows(Some(1));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0][1], scalar_int(3));
    assert!(output.rows(Some(3)).is_empty());
    assert_eq!(
        output.rows(Some(5)),
        [vec![first[0][0].clone(), scalar_int(14)]],
    );
    assert_eq!(output.rows(Some(6)), [vec![scalar_int(14)]]);
    assert_eq!(
        db.edges().rows(None),
        [vec![first[0][0].clone(), scalar_int(14), scalar_int(7)]],
    );

    let before = db.state();
    let rollback = db.run(
        "transaction",
        &["--write", UPSERT, "--param", "delta=int:9", "--rollback"],
    );
    rollback.success("rolled_back");
    assert!(rollback.rows(None).is_empty());
    assert_eq!(
        rollback.events.len(),
        2,
        "no rolled-back statement previews"
    );
    assert_eq!(db.state(), before);
}

#[test]
fn late_return_failure_aborts_plain_writes_and_the_entire_transaction_prefix() {
    let db = Fixture::new("return-failure");
    db.run("write", &[UPSERT, "--param", "delta=int:2"])
        .success("written");
    let failing = "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 \
        MERGE (a)-[r:R {q:7}]->(b) ON MATCH SET r.p=0 \
        ON CREATE SET r.p=0 RETURN 10/r.p AS bad";
    let before = db.state();
    db.run("write", &[failing]).refusal();
    assert_eq!(db.state(), before);
    db.run(
        "transaction",
        &[
            "--write",
            "MATCH (n:Person) SET n.q=99",
            "--query",
            "MATCH (n:Person) RETURN n.q AS q",
            "--write",
            failing,
        ],
    )
    .refusal();
    assert_eq!(db.state(), before, "earlier statements must be aborted too");

    // A different identity key stages a NEW edge before the same RETURN failure.
    db.run(
        "write",
        &["MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 \
         MERGE (a)-[r:R {q:8}]->(b) ON CREATE SET r.p=0 RETURN 10/r.p AS bad"],
    )
    .refusal();
    assert_eq!(db.state(), before, "a failed create branch left an edge");
}

#[test]
fn no_input_skips_actions_but_limit_zero_does_not_skip_effects_or_failures() {
    let db = Fixture::new("no-input-limit");
    let absent = db.run(
        "write",
        &["MATCH (a:Person),(b:Person) WHERE a.p=999 AND b.p=2 \
         MERGE (a)-[r:R]->(b) ON CREATE SET r.p=1/0 RETURN count(*) AS total"],
    );
    absent.success("written");
    let rows = absent.rows(None);
    assert_eq!(rows.len(), 1);
    assert_eq!(string(field(&rows[0][0], "value")), "0");
    assert!(db.edges().rows(None).is_empty());

    let limited = db.run(
        "write",
        &["MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 \
         MERGE (a)-[r:R {q:7}]->(b) ON CREATE SET r.p=9 RETURN r.p AS p LIMIT 0"],
    );
    limited.success("written");
    assert!(limited.rows(None).is_empty());
    let edges = db.edges().rows(None);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0][1], scalar_int(9));
    let before = db.state();
    db.run(
        "write",
        &["MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 \
         MERGE (a)-[r:R {q:7}]->(b) ON MATCH SET r.p=0 \
         RETURN 10/r.p AS bad LIMIT 0"],
    )
    .refusal();
    assert_eq!(db.state(), before);
}

#[test]
fn multi_row_upserts_preserve_every_occurrence_and_plain_merge_reuses_them() {
    let db = Fixture::new("multi-row");
    db.run("write", &["CREATE (:Person {p:1})"])
        .success("written");

    let upsert = db.run("write", &[UPSERT, "--param", "delta=int:2"]);
    upsert.success("written");
    let rows = upsert.rows(None);
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0][0], rows[1][0]);
    assert!(rows.iter().all(|row| row[1] == scalar_int(3)));
    let after_upsert = db.state();
    assert_eq!(after_upsert.2.len(), 2);

    let plain = db.run(
        "write",
        &["MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=2 \
         MERGE (a)-[r:R]->(b) RETURN r"],
    );
    plain.success("written");
    let plain_rows = plain.rows(None);
    assert_eq!(plain_rows.len(), 2);
    assert_ne!(plain_rows[0], plain_rows[1]);
    assert_eq!(db.edges().rows(None).len(), 2);
}
