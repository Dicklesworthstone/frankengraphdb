//! Real CSV scalar imports, reopened native state and packed-embedding retrieval.
//! Expected scalar values and rankings are independent of CSV decoding. Every
//! operation invokes the built CLI; no alternate graph or commit model is used.

use fgdb_protocol::json::{Json, parse_json};
use fgdb_types::CanonicalDecimal;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    directory: PathBuf,
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
    .expect("required JSON field")
}

fn text(value: &Json) -> &str {
    match value {
        Json::String(text) => Some(text.as_str()),
        _ => None,
    }
    .expect("JSON string")
}

fn number(value: &Json) -> u64 {
    match value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
    .expect("unsigned JSON number")
}

fn array(value: &Json) -> &[Json] {
    match value {
        Json::Array(values) => Some(values.as_slice()),
        _ => None,
    }
    .expect("JSON array")
}

fn scalar(kind: &str, value: &str) -> Json {
    Json::Object(BTreeMap::from([
        ("type".into(), Json::String(kind.into())),
        ("value".into(), Json::String(value.into())),
    ]))
}

fn null() -> Json {
    Json::Object(BTreeMap::from([(
        "type".into(),
        Json::String("null".into()),
    )]))
}

impl Outcome {
    fn terminal(&self) -> &Json {
        self.events.last().expect("terminal frame")
    }

    #[track_caller]
    fn success(&self, kind: &str) -> &Self {
        assert_eq!(self.code, 0, "{:?}\n{}", self.events, self.stderr);
        assert!(self.stderr.is_empty(), "{}", self.stderr);
        assert_eq!(text(field(self.terminal(), "event")), "result");
        assert_eq!(text(field(self.terminal(), "kind")), kind);
        assert_eq!(
            self.events
                .iter()
                .filter(|event| { matches!(text(field(event, "event")), "result" | "error") })
                .count(),
            1
        );
        self
    }

    #[track_caller]
    fn refused(&self, code: i32) {
        assert_eq!(self.code, code, "{:?}\n{}", self.events, self.stderr);
        assert_eq!(self.events.len(), 2, "no result prefix: {:?}", self.events);
        assert_eq!(text(field(&self.events[0], "event")), "invocation");
        assert_eq!(text(field(self.terminal(), "event")), "error");
        assert_eq!(
            text(field(self.terminal(), "class")),
            if code == 2 { "usage" } else { "query" }
        );
        assert!(!array(field(self.terminal(), "diagnostics")).is_empty());
    }

    fn seq(&self) -> u64 {
        number(field(self.terminal(), "seq"))
    }

    fn rows(&self) -> Vec<Vec<Json>> {
        self.events
            .iter()
            .filter(|event| text(field(event, "event")) == "row")
            .map(|event| array(field(event, "cells")).to_vec())
            .collect()
    }
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "fgdb-csv-scalars-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        let keys = directory.join("keys");
        std::fs::write(
            &keys,
            format!(
                "{}\n{}\n{}\n",
                "41".repeat(32),
                "42".repeat(32),
                "43".repeat(32),
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let result = Self {
            database: directory.join("db"),
            directory,
            keys,
        };
        result.run("create", &[]).success("created");
        result
            .run("write", &["CREATE (:Keep {name:'sentinel', f:7})"])
            .success("written");
        result
    }

    fn run_at(&self, database: &Path, command: &str, args: &[&str]) -> Outcome {
        let result = Command::new(env!("CARGO_BIN_EXE_fgdb"))
            .args(["--robot", command, "--db"])
            .arg(database)
            .arg("--key-file")
            .arg(&self.keys)
            .args([
                "--label",
                "Doc=1",
                "--label",
                "Keep=2",
                "--relation",
                "R=1",
                "--property",
                "name=1",
                "--property",
                "f=2",
                "--property",
                "d=3",
                "--property",
                "b=4",
                "--property",
                "embedding=5",
                "--property",
                "q=6",
            ])
            .args(args)
            .output()
            .expect("run built fgdb");
        let stdout = std::str::from_utf8(&result.stdout).unwrap();
        assert!(stdout.ends_with('\n'), "complete frames: {stdout:?}");
        Outcome {
            code: result.status.code().expect("no signal"),
            stderr: String::from_utf8(result.stderr).unwrap(),
            events: stdout
                .lines()
                .map(|line| parse_json(line, 100_000, 65_536).unwrap())
                .collect(),
        }
    }

    fn run(&self, command: &str, args: &[&str]) -> Outcome {
        self.run_at(&self.database, command, args)
    }

    fn import_at(
        &self,
        database: &Path,
        types: &str,
        csv: &str,
        statement: &str,
        limits: &[&str],
    ) -> Outcome {
        let input = self.directory.join("input.csv");
        let declarations = self.directory.join("types.tsv");
        std::fs::write(&input, csv).unwrap();
        std::fs::write(&declarations, types).unwrap();
        let mut args = vec![
            "--input",
            input.to_str().unwrap(),
            "--types-file",
            declarations.to_str().unwrap(),
            statement,
        ];
        args.extend_from_slice(limits);
        self.run_at(database, "import-csv", &args)
    }

    fn import(&self, types: &str, csv: &str, statement: &str, limits: &[&str]) -> Outcome {
        self.import_at(&self.database, types, csv, statement, limits)
    }

    fn snapshot(&self) -> (u64, Vec<Vec<Json>>) {
        let result = self.run("query", &[
            "MATCH (n) RETURN n,n.name AS name,n.f AS f,n.d AS d,n.b AS b,n.embedding AS embedding,n.q AS q ORDER BY n",
        ]);
        result.success("rows");
        (result.seq(), result.rows())
    }
}

const TYPES: &str = "text\tname\nfloat\tf\ndecimal\td\nbytes\tb\n";
const INSERT: &str = "CREATE (n:Doc {name:$name,f:$f,d:$d,b:$b})";

#[test]
fn numeric_and_binary_csv_values_survive_reopen_compaction_and_exact_rendering() {
    let db = Fixture::new();
    let before = db.snapshot().0;
    let csv = "\u{feff}b,name,d,f\r\n00AaFf,first,9007199254740993.000000000000000001,1.25e2\r\n\"\",second,-0.000,5e-324\r\n\\N,third,\\N,\\N\r\n";
    let imported = db.import(TYPES, csv, INSERT, &[]);
    imported.success("imported_csv");
    assert_eq!(imported.seq(), before + 1);
    assert_eq!(number(field(imported.terminal(), "records")), 3);
    assert_eq!(number(field(imported.terminal(), "statements")), 3);
    let zero = CanonicalDecimal::from_coefficient(0).unwrap().to_string();
    let expected = vec![
        vec![
            scalar("text", "first"),
            scalar("float", "125"),
            scalar("decimal", "9007199254740993.000000000000000001"),
            scalar("bytes", "00aaff"),
        ],
        vec![
            scalar("text", "second"),
            scalar("float", &f64::from_bits(1).to_string()),
            scalar("decimal", &zero),
            scalar("bytes", ""),
        ],
        vec![scalar("text", "third"), null(), null(), null()],
    ];
    for compact in [false, true] {
        if compact {
            db.run("compact", &[]).success("compacted");
        }
        let rows = db.run(
            "query",
            &["MATCH (n:Doc) RETURN n.name AS name,n.f AS f,n.d AS d,n.b AS b ORDER BY name"],
        );
        rows.success("rows");
        assert_eq!(rows.seq(), imported.seq());
        assert_eq!(rows.rows(), expected);
    }
}

#[test]
fn invalid_late_scalar_input_is_atomic_and_refused_before_database_open() {
    let db = Fixture::new();
    // Positive control: the exact same statement/types have a real successful
    // import. Subsequent refusal comparisons must preserve this pre-existing data.
    db.import(TYPES, "name,f,d,b\nkept,1.25,0.1,00\n", INSERT, &[])
        .success("imported_csv");
    let before = db.snapshot();
    let nonexistent = db.directory.join("must-not-be-opened");
    for last in [
        "bad,NaN,0.2,ff",
        "bad,1e309,0.2,ff",
        "bad,2.5,0.0000000000000000001,ff",
        "bad,2.5,10000000000000000,ff",
        "bad,2.5,1e2,ff",
        "bad,2.5,0.2,0x12",
        "bad,2.5,0.2,0",
        "bad,2.5,0.2,private_payload",
        "bad,2.5,0.2,\"\\N\"",
    ] {
        let csv = format!("name,f,d,b\nprefix,1.25,0.1,00\n{last}\n");
        let result = db.import(TYPES, &csv, INSERT, &[]);
        result.refused(3);
        assert!(!result.stderr.contains("private_payload"));
        assert_eq!(db.snapshot(), before, "{last}");
        // A storage-open error would be class open/code 4, not this value
        // refusal. No database directory may be created for malformed input.
        db.import_at(&nonexistent, TYPES, &csv, INSERT, &[])
            .refused(3);
        assert!(!nonexistent.exists());
    }
}

#[test]
fn scalar_import_execution_failures_and_independent_caps_keep_the_whole_program_atomic() {
    let control = Fixture::new();
    let statement = "CREATE (n:Doc {name:$name,f:$f,d:$d,b:$b}); \
        MATCH (n:Doc) WHERE n.name=$name SET n.q=10/$divisor";
    control
        .import(
            TYPES,
            "name,f,d,b,divisor\none,1.25,0.1,00,2\n",
            statement,
            &[],
        )
        .success("imported_csv");
    let db = Fixture::new();
    let before = db.snapshot();
    let late_failure = "name,f,d,b,divisor\none,1.25,0.1,00,2\ntwo,2.5,0.2,ff,0\n";
    db.import(TYPES, late_failure, statement, &[]).refused(3);
    assert_eq!(db.snapshot(), before);
    let valid = "name,f,d,b\none,1.25,0.1,00\ntwo,2.5,0.2,ff\n";
    db.import(TYPES, valid, INSERT, &["--max-changes", "1"])
        .refused(3);
    assert_eq!(db.snapshot(), before);
    db.import(TYPES, valid, INSERT, &["--max-work-units", "0"])
        .refused(3);
    assert_eq!(db.snapshot(), before);
    let too_small = (valid.len() - 1).to_string();
    db.import(TYPES, valid, INSERT, &["--max-input-bytes", &too_small])
        .refused(3);
    assert_eq!(db.snapshot(), before);
    let exact = valid.len().to_string();
    let imported = db.import(
        TYPES,
        valid,
        INSERT,
        &["--max-changes", "2", "--max-input-bytes", &exact],
    );
    imported.success("imported_csv");
    assert_eq!(imported.seq(), before.0 + 1);
    assert_eq!(number(field(imported.terminal(), "records")), 2);
}

fn packed(vector: &[f32]) -> String {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn csv_imported_384_dimensional_embeddings_feed_native_search_and_pinned_history() {
    let db = Fixture::new();
    let mut same = vec![0_f32; 384];
    same[0] = 1.0;
    let mut orthogonal = vec![0_f32; 384];
    orthogonal[1] = 1.0;
    let mut opposite = vec![0_f32; 384];
    opposite[0] = -1.0;
    let csv = format!(
        "name,embedding\nsame,{}\northogonal,{}\nopposite,{}\n",
        packed(&same),
        packed(&orthogonal),
        packed(&opposite)
    );
    let imported = db.import(
        "text\tname\nbytes\tembedding\n",
        &csv,
        "CREATE (n:Doc {name:$name,embedding:$embedding})",
        &[],
    );
    imported.success("imported_csv");
    let stored = db.run(
        "query",
        &["MATCH (n:Doc) RETURN n.name AS name,n AS node ORDER BY name"],
    );
    stored.success("rows");
    let ids: BTreeMap<String, Json> = stored
        .rows()
        .into_iter()
        .map(|row| (text(field(&row[0], "value")).to_owned(), row[1].clone()))
        .collect();
    let vector = same
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let search = |metric, at: Option<&str>| {
        let mut args = vec![
            "--vector",
            &vector,
            "--vector-property",
            "embedding",
            "--vector-encoding",
            "packed",
            "--vertex-label",
            "Doc",
            "--metric",
            metric,
            "--k",
            "3",
        ];
        if let Some(at) = at {
            args.extend(["--as-of", at]);
        }
        let result = db.run("search", &args);
        result.success("searched");
        result
    };
    for (metric, distances) in [
        ("l2", [0.0, 2.0, 4.0]),
        ("cosine", [0.0, 1.0, 2.0]),
        ("dot", [-1.0, 0.0, 1.0]),
    ] {
        let result = search(metric, None);
        assert_eq!(result.seq(), imported.seq());
        let rows = result.rows();
        assert_eq!(rows.len(), 3);
        for ((row, name), distance) in rows
            .iter()
            .zip(["same", "orthogonal", "opposite"])
            .zip(distances)
        {
            assert_eq!(row[0], ids[name]);
            assert_eq!(text(field(&row[1], "type")), "float");
            assert_eq!(
                text(field(&row[1], "value")).parse::<f64>().unwrap(),
                distance
            );
        }
    }
    let before = search("l2", None).rows();
    let parameter = format!("embedding=bytes:{}", packed(&opposite));
    db.run(
        "write",
        &[
            "MATCH (n:Doc) WHERE n.name='same' SET n.embedding=$embedding",
            "--param",
            &parameter,
        ],
    )
    .success("written");
    let cut = imported.seq().to_string();
    assert_eq!(search("l2", Some(&cut)).rows(), before);
    let current = search("l2", None);
    assert_eq!(current.seq(), imported.seq() + 1);
    assert_eq!(current.rows()[0][0], ids["orthogonal"]);
}
