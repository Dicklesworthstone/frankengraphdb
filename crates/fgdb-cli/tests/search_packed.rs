//! Packed embedding ingress through the real CLI, database and Beacon decoder.
//! Each command is a fresh process; no Python model or alternate index is used.
//! These tests compare unchanged coordinate search and the native GQL procedure
//! with the packed CLI path, including ranking and the complete row encoding.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fgdb-search-packed-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let key = root.join("keys");
        std::fs::write(
            &key,
            format!(
                "{}\n{}\n{}\n",
                "5a".repeat(32),
                "77".repeat(32),
                "3c".repeat(32)
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self { root }
    }

    fn command(&self, command: &str, parts: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fgdb"))
            .args(["--robot", command, "--db"])
            .arg(self.root.join("db"))
            .arg("--key-file")
            .arg(self.root.join("keys"))
            .args([
                "--label",
                "Document=1",
                "--relation",
                "LINK=1",
                "--property",
                "x=1",
                "--property",
                "y=2",
                "--property",
                "embedding=3",
                "--property",
                "bio=4",
                "--property",
                "name=5",
            ])
            .args(parts)
            .output()
            .expect("execute built fgdb binary")
    }

    fn okay(&self, command: &str, parts: &[&str]) -> String {
        let output = self.command(command, parts);
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout
                .lines()
                .last()
                .unwrap()
                .starts_with("{\"v\":1,\"event\":\"result\",")
        );
        stdout
    }

    fn seed(&self) -> u64 {
        self.okay("create", &[]);
        let output = self.okay(
            "write",
            &[
                "CREATE (a:Document {name:'far', bio:'other', x:6, y:0, embedding:$far}), \
                    (b:Document {name:'near', bio:'graph memory', x:2, y:0, embedding:$near}), \
                    (c:Document {name:'middle', bio:'graph', x:3, y:2, embedding:$middle}), \
                    (d:Document {name:'missing', bio:'graph'}), (b)-[:LINK]->(c)",
                "--param",
                "far=vector:6,0",
                "--param",
                "near=vector:2,0",
                "--param",
                "middle=vector:3,2",
            ],
        );
        terminal_number(&output, "seq")
    }

    fn search(&self, packed: bool, extra: &[&str]) -> String {
        let mut args = vec!["--vector", "1,0"];
        if packed {
            args.extend_from_slice(&[
                "--vector-encoding",
                "packed",
                "--vector-property",
                "embedding",
            ]);
        } else {
            // The pre-change public coordinate path is the live control.
            args.extend_from_slice(&["--vector-property", "x", "--vector-property", "y"]);
        }
        args.extend_from_slice(extra);
        self.okay("search", &args)
    }
}

// Restrict extraction to the renderer's fixed frame prefix, not arbitrary cell
// substrings. Full row lines (including every typed cell) are compared verbatim.
fn rows(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| line.starts_with("{\"v\":1,\"event\":\"row\","))
        .collect()
}

fn terminal_number(stdout: &str, name: &str) -> u64 {
    let prefix = format!("\"{name}\":");
    stdout
        .lines()
        .last()
        .unwrap()
        .split_once(&prefix)
        .unwrap()
        .1
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

fn first_vertex(stdout: &str) -> &str {
    let row = rows(stdout)
        .into_iter()
        .next()
        .expect("at least one result row");
    row.split_once("\"type\":\"vertex\",\"value\":\"")
        .unwrap()
        .1
        .split('"')
        .next()
        .unwrap()
}

#[test]
fn packed_cli_matches_coordinate_search_after_reopen_in_all_retrieval_modes() {
    let db = Fixture::new();
    let seq = db.seed();
    let before = std::fs::read(db.root.join("db/manifest.root")).unwrap();
    for metric in ["l2", "cosine", "dot"] {
        for ann in [false, true] {
            let mut args = vec!["--metric", metric, "--k", "2"];
            if ann {
                args.extend_from_slice(&["--ann", "32"]);
            }
            let packed = db.search(true, &args);
            let coordinate = db.search(false, &args);
            assert_eq!(packed, coordinate, "metric={metric} ann={ann}");
            assert_eq!(rows(&packed).len(), 2);
            assert_eq!(terminal_number(&packed, "seq"), seq);
        }
    }
    let vector = db.search(true, &["--k", "1"]);
    let seed = first_vertex(&vector);
    for graph in [false, true] {
        let mut args = vec![
            "--text",
            "graph",
            "--text-property",
            "bio",
            "--k",
            "3",
            "--candidates",
            "3",
        ];
        if graph {
            args.extend_from_slice(&[
                "--expand-from",
                seed,
                "--max-hops",
                "1",
                "--expand-relation",
                "LINK",
            ]);
        }
        let packed = db.search(true, &args);
        assert_eq!(packed, db.search(false, &args), "graph={graph}");
        assert_eq!(rows(&packed).len(), 3);
    }
    assert_eq!(
        std::fs::read(db.root.join("db/manifest.root")).unwrap(),
        before,
        "search must not publish an index or advance the database root"
    );
}

#[test]
fn packed_cli_preserves_eligibility_and_historical_generation_after_a_write() {
    let db = Fixture::new();
    let seq = db.seed();
    let before = db.search(true, &["--k", "3"]);
    let near = first_vertex(&before);
    let selected = db.search(true, &["--eligible", near, "--k", "1"]);
    assert_eq!(rows(&selected), vec![rows(&before)[0]]);
    assert!(rows(&db.search(true, &["--eligible", "", "--k", "1"])).is_empty());
    db.okay(
        "write",
        &[
            "MATCH (n:Document {name:'far'}) SET n.x=1, n.y=0, n.embedding=$new",
            "--param",
            "new=vector:1,0",
        ],
    );
    let after = db.search(true, &["--k", "3"]);
    assert_ne!(first_vertex(&after), near);
    assert_eq!(after, db.search(false, &["--k", "3"]));
    let selected_seq = seq.to_string();
    let historical = db.search(true, &["--as-of", &selected_seq, "--k", "3"]);
    assert_eq!(historical, before);
    assert_eq!(
        historical,
        db.search(false, &["--as-of", &selected_seq, "--k", "3"])
    );
}

#[test]
fn packed_cli_searches_high_dimensional_parameter_bytes_and_matches_native_gql() {
    let db = Fixture::new();
    db.okay("create", &[]);
    let vector = std::iter::repeat_n("0.25", 384)
        .collect::<Vec<_>>()
        .join(",");
    let parameter = format!("e=vector:{vector}");
    db.okay(
        "write",
        &["CREATE (:Document {embedding:$e})", "--param", &parameter],
    );
    let packed = db.okay(
        "search",
        &[
            "--vector",
            &vector,
            "--vector-property",
            "embedding",
            "--vector-encoding",
            "packed",
            "--k",
            "1",
        ],
    );
    assert_eq!(rows(&packed).len(), 1);
    assert!(rows(&packed)[0].ends_with("{\"type\":\"float\",\"value\":\"0\"}]}"));
    // Without ORDER BY, CALL hybrid.search preserves its rank order, including
    // across implied vertex property reads (fgdb-euy8y).
    let query = format!(
        "CALL hybrid.search(vector => [{vector}], vector_property => 'embedding', k => 1) \
        YIELD node, vector_distance RETURN node AS vertex, vector_distance AS distance"
    );
    let gql = db.okay("query", &[&query]);
    assert_eq!(rows(&packed), rows(&gql));
    assert_eq!(
        terminal_number(&packed, "seq"),
        terminal_number(&gql, "seq")
    );
    let seeded = Fixture::new();
    seeded.seed();
    let multi_packed = seeded.search(true, &["--k", "3"]);
    let multi_gql = seeded.okay(
        "query",
        &[
            "CALL hybrid.search(vector => [1, 0], vector_property => 'embedding', k => 3) \
             YIELD node, vector_distance RETURN node AS vertex, vector_distance AS distance",
        ],
    );
    assert_eq!(rows(&multi_packed), rows(&multi_gql));
    let multi_props = seeded.okay(
        "query",
        &[
            "CALL hybrid.search(vector => [1, 0], vector_property => 'embedding', k => 3) \
             YIELD node, vector_distance RETURN node.name AS title, vector_distance AS distance",
        ],
    );
    let titles: Vec<&str> = rows(&multi_props)
        .into_iter()
        .map(|row| {
            row.split_once("\"type\":\"text\",\"value\":\"")
                .unwrap()
                .1
                .split('"')
                .next()
                .unwrap()
        })
        .collect();
    assert_eq!(titles, ["near", "middle", "far"]);
}

#[test]
fn packed_cli_refuses_bad_shapes_before_open_and_bad_stored_values_without_rows() {
    let db = Fixture::new();
    for args in [
        vec!["--vector", "1,2", "--vector-property", "embedding"],
        vec![
            "--vector",
            "1,2",
            "--vector-property",
            "x",
            "--vector-property",
            "y",
            "--vector-encoding",
            "packed",
        ],
        vec![
            "--text",
            "graph",
            "--text-property",
            "bio",
            "--vector-encoding",
            "packed",
        ],
        vec![
            "--vector",
            "1,2",
            "--vector-property",
            "embedding",
            "--vector-encoding",
            "packed",
            "--vector-encoding",
            "packed",
        ],
    ] {
        let output = db.command("search", &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!db.root.join("db").exists());
    }
    db.seed();
    for parameter in ["bad=vector:1", "bad=vector:1,2,3", "bad=int:7"] {
        db.okay(
            "write",
            &[
                "MATCH (n:Document {name:'middle'}) SET n.embedding=$bad",
                "--param",
                parameter,
            ],
        );
        let before = std::fs::read(db.root.join("db/manifest.root")).unwrap();
        let output = db.command(
            "search",
            &[
                "--vector",
                "1,0",
                "--vector-property",
                "embedding",
                "--vector-encoding",
                "packed",
                "--k",
                "1",
            ],
        );
        assert_eq!(output.status.code(), Some(3));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            rows(&stdout).is_empty(),
            "a bad stored vector must not leak a partial answer"
        );
        assert_eq!(
            stdout.lines().count(),
            2,
            "invocation and error only: {stdout}"
        );
        assert!(
            stdout
                .lines()
                .last()
                .unwrap()
                .starts_with("{\"v\":1,\"event\":\"error\",")
        );
        assert_eq!(
            std::fs::read(db.root.join("db/manifest.root")).unwrap(),
            before
        );
        assert_eq!(
            rows(&db.search(false, &["--k", "1"])).len(),
            1,
            "the independent coordinate lane must remain usable"
        );
    }
}
