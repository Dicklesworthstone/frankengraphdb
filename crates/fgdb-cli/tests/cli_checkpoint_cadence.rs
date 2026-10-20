//! Fresh-process recovery of lagging Saved V1/V2 checkpoints (fgdb-28oee).
//! The real CLI commits every chunk. Native reopened state is checked by caller
//! keys, while already durable marker identities must survive every resume.
//! No checkpoint decoder, graph evaluator or commit implementation is mocked.
// The binary root (src/main.rs) takes the same limit: proving `Send` for
// an in-process open here descends the whole verified-reopen future.
#![recursion_limit = "256"]

use asupersync::Budget;
use fgdb::{BulkLoadCheckpoint, Database, DatabaseKeys};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_protocol::json::{Json, parse_json};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const ROWS: usize = 26;
const CHUNK: usize = 3;
const CHUNKS: usize = 9;
const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);

type Vertices = BTreeMap<String, (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>;
type Edges = BTreeMap<
    String,
    (
        String,
        String,
        RelationId,
        Vec<(PropertyKeyId, CanonicalScalar)>,
    ),
>;

fn object(value: &Json) -> &BTreeMap<String, Json> {
    let Json::Object(fields) = value else {
        panic!("expected object: {value:?}")
    };
    fields
}
fn field<'a>(value: &'a Json, name: &str) -> &'a Json {
    object(value).get(name).expect("required field")
}
fn string(value: &Json) -> &str {
    let Json::String(value) = value else {
        panic!("expected string: {value:?}")
    };
    value
}
fn number(value: &Json) -> u64 {
    let Json::Number(value) = value else {
        panic!("expected number: {value:?}")
    };
    value.parse().unwrap()
}

struct Outcome {
    output: Output,
    events: Vec<Json>,
}
impl Outcome {
    fn new(output: Output) -> Self {
        let text = std::str::from_utf8(&output.stdout).unwrap();
        assert!(text.ends_with('\n'), "incomplete NDJSON: {text:?}");
        let events = text
            .lines()
            .map(|line| parse_json(line, 10_000, 65_536).unwrap())
            .collect();
        Self { output, events }
    }
    fn terminal(&self) -> &Json {
        self.events.last().expect("terminal event")
    }
    fn success(&self, kind: &str) {
        assert!(self.output.status.success(), "{:?}", self.output);
        assert_eq!(string(field(self.terminal(), "event")), "result");
        assert_eq!(string(field(self.terminal(), "kind")), kind);
        assert_eq!(
            self.events
                .iter()
                .filter(|event| { matches!(string(field(event, "event")), "error" | "result") })
                .count(),
            1
        );
    }
    fn loaded(&self, basis: u64) {
        self.success("loaded");
        assert_eq!(number(field(self.terminal(), "count")), ROWS as u64);
        assert_eq!(number(field(self.terminal(), "statements")), CHUNKS as u64);
        assert_eq!(number(field(self.terminal(), "seq")), basis + CHUNKS as u64);
    }
    fn progress(&self) -> Vec<(usize, u64)> {
        self.events
            .iter()
            .filter(|event| string(field(event, "event")) == "progress")
            .map(|event| {
                (
                    number(field(event, "rows")) as usize,
                    number(field(event, "seq")),
                )
            })
            .collect()
    }
    fn invalid_resume(&self) {
        assert_eq!(self.output.status.code(), Some(3), "{:?}", self.output);
        assert_eq!(
            self.events.len(),
            2,
            "no recovery progress before validation"
        );
        assert_eq!(string(field(self.terminal(), "event")), "error");
        assert_eq!(string(field(self.terminal(), "class")), "query");
        assert!(format!("{:?}", field(self.terminal(), "diagnostics")).contains("InvalidResume"));
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}
fn source() -> String {
    let mut text = String::new();
    for n in 0..3 {
        text.push_str(&format!(
            "{{\"kind\":\"vertex\",\"key\":\"v{n}\",\"labels\":[\"Person\"],\"props\":{{\"k\":\"int:{n}\",\"p\":\"int:{}\"}}}}\n",
            n * 7,
        ));
    }
    // Parallel edges, loops, both orientations and two relation coordinates.
    for n in 0..23 {
        text.push_str(&format!(
            "{{\"kind\":\"edge\",\"key\":\"e{n}\",\"source\":\"v{}\",\"destination\":\"v{}\",\"relation\":\"{}\",\"props\":{{\"k\":\"int:{}\",\"p\":\"int:{}\"}}}}\n",
            n % 3, (n * 2 + 1) % 3, if n % 2 == 0 { "R" } else { "S" },
            100 + n, -1 - n,
        ));
    }
    text
}
fn expected() -> (Vertices, Edges) {
    let vertices = (0..3)
        .map(|n| {
            (
                format!("v{n}"),
                (
                    vec![LabelId(1)],
                    vec![
                        (K, CanonicalScalar::Int(n)),
                        (P, CanonicalScalar::Int(n * 7)),
                    ],
                ),
            )
        })
        .collect();
    let edges = (0..23)
        .map(|n| {
            (
                format!("e{n}"),
                (
                    format!("v{}", n % 3),
                    format!("v{}", (n * 2 + 1) % 3),
                    RelationId(if n % 2 == 0 { 1 } else { 2 }),
                    vec![
                        (K, CanonicalScalar::Int(100 + n)),
                        (P, CanonicalScalar::Int(-1 - n)),
                    ],
                ),
            )
        })
        .collect();
    (vertices, edges)
}

struct Fixture {
    database: PathBuf,
    key_file: PathBuf,
    input: PathBuf,
    checkpoint: PathBuf,
    basis: u64,
}
impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fgdb-checkpoint-cadence-{name}-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let key_file = root.join("keys");
        std::fs::write(
            &key_file,
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
            std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut fixture = Self {
            database: root.join("db"),
            input: root.join("source.ndjson"),
            checkpoint: root.join("checkpoint"),
            key_file,
            basis: 0,
        };
        std::fs::write(&fixture.input, source()).unwrap();
        fixture.run("create", &[]).success("created");
        // A nonzero base and unrelated pre-existing vertex must survive too.
        let seeded = fixture.run("write", &["CREATE (:Other {k:999,p:777})"]);
        seeded.success("written");
        fixture.basis = number(field(seeded.terminal(), "seq"));
        fixture
    }
    fn command(&self, verb: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command
            .env_remove("FGDB_LOAD_CRASH_CHUNK")
            .env_remove("FGDB_LOAD_CRASH_POINT")
            .args(["--robot", verb, "--db"])
            .arg(&self.database)
            .arg("--key-file")
            .arg(&self.key_file)
            .args([
                "--label",
                "Person=1",
                "--label",
                "Other=2",
                "--relation",
                "R=1",
                "--relation",
                "S=2",
                "--property",
                "k=1",
                "--property",
                "p=2",
            ]);
        command
    }
    fn run(&self, verb: &str, args: &[&str]) -> Outcome {
        Outcome::new(self.command(verb).args(args).output().unwrap())
    }
    fn load_command(&self) -> Command {
        let mut command = self.command("load");
        command
            .arg("--input")
            .arg(&self.input)
            .arg("--checkpoint")
            .arg(&self.checkpoint)
            .arg("--rows-per-chunk")
            .arg(CHUNK.to_string());
        command
    }
    fn load(&self) -> Outcome {
        Outcome::new(self.load_command().output().unwrap())
    }
    fn crash(&self, chunk: usize, point: &str) -> Outcome {
        let outcome = Outcome::new(
            self.load_command()
                .env("FGDB_LOAD_CRASH_CHUNK", chunk.to_string())
                .env("FGDB_LOAD_CRASH_POINT", point)
                .output()
                .unwrap(),
        );
        assert!(
            !outcome.output.status.success(),
            "{point}/{chunk}: crash seam must refuse"
        );
        outcome
    }
    fn checkpoint(&self) -> BulkLoadCheckpoint {
        let text = std::fs::read_to_string(&self.checkpoint).unwrap();
        let value = parse_json(&text, 10_000, 65_536).unwrap();
        assert_eq!(number(field(&value, "base_frontier")), self.basis);
        assert_eq!(string(field(&value, "base_marker")).len(), 64);
        if number(field(&value, "v")) == 2 {
            assert!(text.len() <= 1024);
            assert!(!object(&value).contains_key("vertices"));
            assert!(!object(&value).contains_key("edges"));
            let mut cp = BulkLoadCheckpoint {
                next_row: number(field(&value, "next_row")) as usize,
                committed_chunks: number(field(&value, "committed_chunks")) as usize,
                frontier: CommitSeq(number(field(&value, "frontier"))),
                ..BulkLoadCheckpoint::default()
            };
            // The fixture's independent K property identifies each source
            // row. Do not reuse production issue-order replay to recover IDs.
            let ordinal = |props: &[(PropertyKeyId, CanonicalScalar)]| {
                let (_, CanonicalScalar::Int(value)) =
                    props.iter().find(|(key, _)| *key == K).unwrap()
                else {
                    panic!("fixture ordinal")
                };
                *value
            };
            self.inspect(|db| {
                for row in db.vertices_at(cp.frontier).unwrap() {
                    let n = ordinal(&row.props);
                    if n == 999 {
                        assert_eq!(row.labels, [LabelId(2)]);
                        assert_eq!(
                            row.props,
                            [
                                (K, CanonicalScalar::Int(999)),
                                (P, CanonicalScalar::Int(777))
                            ]
                        );
                        continue;
                    }
                    assert!((0..3).contains(&n));
                    assert!(cp.vertices.insert(format!("v{n}"), row.vid).is_none());
                }
                for row in db.edges_at(cp.frontier).unwrap() {
                    let n = ordinal(&row.props) - 100;
                    assert!((0..23).contains(&n));
                    assert!(cp.edges.insert(format!("e{n}"), row.entry.eid).is_none());
                }
            });
            assert_eq!(cp.vertices.len(), cp.next_row.min(3));
            assert_eq!(cp.edges.len(), cp.next_row.saturating_sub(3));
            let domain = b"fgdb.cli.load.identity-prefix.v2\0";
            let mut digest = fgdb_crypto::hash(domain).0;
            for line in source().lines().take(cp.next_row) {
                let row = parse_json(line, 100, 1024).unwrap();
                let key = string(field(&row, "key"));
                let (kind, id) = match string(field(&row, "kind")) {
                    "vertex" => (0_u8, cp.vertices[key].0),
                    "edge" => (1_u8, cp.edges[key].0),
                    _ => panic!("source kind"),
                };
                let mut transcript = domain.to_vec();
                transcript.extend_from_slice(&digest);
                transcript.push(kind);
                transcript.extend_from_slice(&(key.len() as u64).to_be_bytes());
                transcript.extend_from_slice(key.as_bytes());
                transcript.extend_from_slice(&id.to_be_bytes());
                digest = fgdb_crypto::hash(&transcript).0;
            }
            let expected: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(string(field(&value, "identity_hash")), expected);
            return cp;
        }
        assert_eq!(number(field(&value, "v")), 1);
        BulkLoadCheckpoint {
            vertices: object(field(&value, "vertices"))
                .iter()
                .map(|(key, id)| (key.clone(), VId(string(id).parse().unwrap())))
                .collect(),
            edges: object(field(&value, "edges"))
                .iter()
                .map(|(key, id)| (key.clone(), EId(string(id).parse().unwrap())))
                .collect(),
            next_row: number(field(&value, "next_row")) as usize,
            committed_chunks: number(field(&value, "committed_chunks")) as usize,
            frontier: CommitSeq(number(field(&value, "frontier"))),
        }
    }
    fn inspect<T>(&self, inspect: impl FnOnce(&Database) -> T) -> T {
        let runtime = fgdb::runtime_builder().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        runtime.block_on(async {
            let mut db = Database::open(&contexts.commit(), &self.database, keys())
                .await
                .unwrap();
            db.ensure_delta_window(&contexts.commit(), CommitSeq(self.basis))
                .await
                .unwrap();
            inspect(&db)
        })
    }
    fn markers(&self) -> Vec<[u8; 32]> {
        self.inspect(|db| {
            db.delta_since(CommitSeq(self.basis))
                .unwrap()
                .map(|batch| batch.commit_marker_identity().marker_oid.0)
                .collect()
        })
    }
    fn graph(&self, checkpoint: &BulkLoadCheckpoint) -> (Vertices, Edges) {
        self.inspect(|db| {
            let vertices: BTreeMap<_, _> = checkpoint
                .vertices
                .iter()
                .map(|(key, &id)| (id, key.clone()))
                .collect();
            let edges: BTreeMap<_, _> = checkpoint
                .edges
                .iter()
                .map(|(key, &id)| (id, key.clone()))
                .collect();
            assert_eq!(
                vertices.len(),
                checkpoint.vertices.len(),
                "no aliased identities"
            );
            assert_eq!(edges.len(), checkpoint.edges.len());
            let mut loaded = BTreeMap::new();
            let mut sentinel = 0;
            for row in db.vertices().unwrap() {
                if let Some(key) = vertices.get(&row.vid) {
                    assert!(
                        loaded
                            .insert(key.clone(), (row.labels, row.props))
                            .is_none()
                    );
                } else {
                    assert_eq!(row.labels, [LabelId(2)]);
                    assert_eq!(
                        row.props,
                        [
                            (K, CanonicalScalar::Int(999)),
                            (P, CanonicalScalar::Int(777))
                        ]
                    );
                    sentinel += 1;
                }
            }
            assert_eq!(sentinel, 1, "pre-existing vertex remains unchanged");
            assert_eq!(loaded.len(), vertices.len());
            let mut relationships = BTreeMap::new();
            for row in db.edges().unwrap() {
                let key = edges[&row.entry.eid].clone();
                assert!(
                    relationships
                        .insert(
                            key,
                            (
                                vertices[&row.entry.src].clone(),
                                vertices[&row.entry.dst].clone(),
                                row.entry.relation,
                                row.props,
                            )
                        )
                        .is_none()
                );
            }
            assert_eq!(relationships.len(), edges.len());
            (loaded, relationships)
        })
    }
}

#[test]
fn every_chunk_boundary_recovers_lagging_maps_without_duplicate_commits() {
    let control = Fixture::new("control");
    control.load().loaded(control.basis);
    let reference = control.graph(&control.checkpoint());
    assert_eq!(reference, expected());
    // Independent enumeration of the last persisted prefix before each fault.
    for (chunk, saved_chunks) in [0, 1, 2, 2, 4, 4, 4, 4, 8].into_iter().enumerate() {
        for point in ["before-capsule", "after-marker-sync"] {
            let fixture = Fixture::new("boundary");
            let failed = fixture.crash(chunk, point);
            assert_eq!(failed.progress().len(), chunk);
            let saved = fixture.checkpoint();
            assert_eq!(saved.committed_chunks, saved_chunks, "{point}/{chunk}");
            assert_eq!(saved.next_row, saved_chunks * CHUNK);
            let before = fixture.markers();
            assert_eq!(
                before.len(),
                chunk + usize::from(point == "after-marker-sync")
            );
            let resumed = fixture.load();
            resumed.loaded(fixture.basis);
            assert_eq!(
                resumed.progress(),
                ((saved_chunks + 1)..=CHUNKS)
                    .map(|n| ((n * CHUNK).min(ROWS), fixture.basis + n as u64))
                    .collect::<Vec<_>>()
            );
            let completed = fixture.checkpoint();
            assert_eq!(
                completed.next_row, ROWS,
                "final partial chunk is checkpointed"
            );
            assert_eq!(completed.committed_chunks, CHUNKS);
            for (key, id) in &saved.vertices {
                assert_eq!(completed.vertices.get(key), Some(id));
            }
            for (key, id) in &saved.edges {
                assert_eq!(completed.edges.get(key), Some(id));
            }
            assert_eq!(fixture.graph(&completed), reference);
            let after = fixture.markers();
            assert_eq!(after.len(), CHUNKS);
            assert!(
                after.starts_with(&before),
                "never rewrite an already durable marker"
            );
            let bytes = std::fs::read(&fixture.checkpoint).unwrap();
            let again = fixture.load();
            again.loaded(fixture.basis);
            assert!(again.progress().is_empty());
            assert_eq!(std::fs::read(&fixture.checkpoint).unwrap(), bytes);
            assert_eq!(fixture.markers(), after);
        }
    }
}

#[test]
fn a_recovered_non_geometric_prefix_is_persisted_before_the_next_attempt() {
    let fixture = Fixture::new("repeat-crash");
    fixture.crash(7, "before-capsule");
    assert_eq!(fixture.checkpoint().committed_chunks, 4);
    let before = fixture.markers();
    assert_eq!(before.len(), 7);
    let failed_again = fixture.crash(7, "before-capsule");
    assert_eq!(
        fixture.checkpoint().committed_chunks,
        7,
        "reconciliation must be forced"
    );
    assert_eq!(
        failed_again.progress(),
        (5..=7)
            .map(|n| (n * CHUNK, fixture.basis + n as u64))
            .collect::<Vec<_>>()
    );
    assert_eq!(fixture.markers(), before);
    fixture.load().loaded(fixture.basis);
    assert_eq!(fixture.graph(&fixture.checkpoint()), expected());
}

#[test]
fn a_forged_lagging_identity_commitment_cannot_be_replaced_by_recovery() {
    let fixture = Fixture::new("forged");
    fixture.crash(7, "before-capsule");
    let saved = fixture.checkpoint();
    assert_eq!(saved.committed_chunks, 4);
    let original = std::fs::read_to_string(&fixture.checkpoint).unwrap();
    let value = parse_json(&original, 10_000, 65_536).unwrap();
    let old = format!(
        "\"identity_hash\":\"{}\"",
        string(field(&value, "identity_hash"))
    );
    assert_eq!(original.matches(&old).count(), 1);
    let changed = format!("\"identity_hash\":\"{}\"", "0".repeat(64));
    assert_ne!(old, changed);
    let forged = original.replacen(&old, &changed, 1);
    std::fs::write(&fixture.checkpoint, &forged).unwrap();
    let before = fixture.markers();
    fixture.load().invalid_resume();
    assert_eq!(
        std::fs::read_to_string(&fixture.checkpoint).unwrap(),
        forged
    );
    assert_eq!(fixture.markers(), before);
}

#[test]
fn a_changed_source_is_refused_even_when_its_commits_run_ahead_of_the_hint() {
    let fixture = Fixture::new("changed-source");
    fixture.crash(7, "before-capsule");
    let saved = std::fs::read(&fixture.checkpoint).unwrap();
    let before = fixture.markers();
    let original = source();
    assert_eq!(original.matches("\"p\":\"int:-1\"").count(), 1);
    std::fs::write(
        &fixture.input,
        original.replacen("\"p\":\"int:-1\"", "\"p\":\"int:-9\"", 1),
    )
    .unwrap();
    fixture.load().invalid_resume();
    assert_eq!(std::fs::read(&fixture.checkpoint).unwrap(), saved);
    assert_eq!(fixture.markers(), before);
}

#[test]
fn unresolved_match_refuses_before_creating_the_import_receipt() {
    let fixture = Fixture::new("unresolved-before-receipt");
    std::fs::write(
        &fixture.input,
        concat!(
            "{\"kind\":\"edge\",\"key\":\"e0\",",
            "\"source\":{\"label\":\"Other\",\"property\":\"k\",\"value\":\"int:999\"},",
            "\"destination\":{\"label\":\"Other\",\"property\":\"k\",\"value\":\"int:1234\"},",
            "\"relation\":\"R\",\"props\":{}}\n",
        ),
    )
    .unwrap();
    let before = fixture.markers();
    let refused = fixture.load();
    assert_eq!(
        refused.output.status.code(),
        Some(3),
        "{:?}",
        refused.output
    );
    assert!(
        format!("{:?}", field(refused.terminal(), "diagnostics")).contains("UnresolvedEndpoint")
    );
    assert!(refused.progress().is_empty());
    assert!(
        !fixture.checkpoint.exists(),
        "failed preflight cannot publish an import origin"
    );
    assert_eq!(fixture.markers(), before);
}

#[test]
fn wrong_matched_endpoint_in_recovered_history_cannot_replace_or_acknowledge_a_receipt() {
    for legacy in [false, true] {
        let mut fixture = Fixture::new("mismatched-recovered-endpoint");
        let seeded = fixture.run("write", &["CREATE (:Person {k:998,p:888})"]);
        seeded.success("written");
        fixture.basis = number(field(seeded.terminal(), "seq"));
        // Both selectors below are unique at this nonzero original basis.
        // Our source intends the Other vertex's loop, not the Person's loop.
        std::fs::write(
            &fixture.input,
            concat!(
                "{\"kind\":\"edge\",\"key\":\"e0\",",
                "\"source\":{\"label\":\"Other\",\"property\":\"k\",\"value\":\"int:999\"},",
                "\"destination\":{\"label\":\"Other\",\"property\":\"k\",\"value\":\"int:999\"},",
                "\"relation\":\"R\",\"props\":{\"k\":\"int:100\",\"p\":\"int:-1\"}}\n",
            ),
        )
        .unwrap();
        let stopped = fixture.crash(0, "before-capsule");
        assert!(stopped.progress().is_empty());
        let compact = std::fs::read_to_string(&fixture.checkpoint).unwrap();
        let parsed = parse_json(&compact, 100, 1024).unwrap();
        assert_eq!(number(field(&parsed, "next_row")), 0);
        if legacy {
            // A failed resume must not upgrade even a valid legacy origin.
            let old = format!(
                "{{\"v\":1,\"vertices\":{{}},\"edges\":{{}},\"next_row\":0,\"frontier\":{},\"committed_chunks\":0,\"base_frontier\":{},\"base_marker\":\"{}\",\"source_hash\":\"{}\",\"rows_per_chunk\":{}}}\n",
                fixture.basis,
                fixture.basis,
                string(field(&parsed, "base_marker")),
                string(field(&parsed, "source_hash")),
                CHUNK,
            );
            std::fs::write(&fixture.checkpoint, old).unwrap();
        }
        let saved = std::fs::read(&fixture.checkpoint).unwrap();
        let runtime = fgdb::runtime_builder().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (wrong_id, wrong_vertex) = runtime.block_on(async {
            let mut db = Database::open(&contexts.commit(), &fixture.database, keys())
                .await
                .unwrap();
            let wrong =
                fgdb::BulkEndpoint::matching(Some(LabelId(1)), K, CanonicalScalar::Int(998));
            let cp = db
                .bulk_load(
                    &contexts.query(),
                    &contexts.commit(),
                    [fgdb::BulkRow::Edge(fgdb::BulkEdge {
                        key: "e0".into(),
                        source: wrong.clone(),
                        destination: wrong,
                        relation: RelationId(1),
                        props: vec![
                            (K, CanonicalScalar::Int(100)),
                            (P, CanonicalScalar::Int(-1)),
                        ],
                    })],
                    fgdb::BulkLoadPolicy::new(CHUNK, RelationId(1)),
                )
                .await
                .unwrap();
            assert_eq!(cp.frontier, CommitSeq(fixture.basis + 1));
            let edge = db.edge(cp.edges["e0"]).unwrap().unwrap();
            assert_eq!(edge.entry.src, edge.entry.dst);
            (edge.entry.eid, edge.entry.src)
        });
        let before = fixture.markers();
        assert_eq!(before.len(), 1);
        // The native creation's kind, ID, relation, properties and row count
        // pass Chronicle reconstruction. Only engine resume validation binds
        // its endpoints to the source's Match selectors at the original basis.
        fixture.load().invalid_resume();
        assert_eq!(
            std::fs::read(&fixture.checkpoint).unwrap(),
            saved,
            "invalid reconstructed prefix cannot replace V1/V2 receipt"
        );
        assert_eq!(fixture.markers(), before);
        fixture.inspect(|db| {
            assert_eq!(db.frontier().unwrap(), CommitSeq(fixture.basis + 1));
            let edges = db.edges().unwrap();
            assert_eq!(edges.len(), 1);
            assert_eq!(edges[0].entry.eid, wrong_id);
            assert_eq!(edges[0].entry.src, wrong_vertex);
            assert_eq!(edges[0].entry.dst, wrong_vertex);
        });
    }
}
