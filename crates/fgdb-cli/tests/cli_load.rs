//! Real subprocess ingestion, checkpoint recovery, and caller-keyed engine differential.
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    BulkEdge, BulkLoadCheckpoint, BulkLoadPolicy, BulkRow, BulkVertex, Database, DatabaseKeys,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}
impl Json {
    fn object(&self) -> &BTreeMap<String, Self> {
        let Self::Object(value) = self else {
            panic!("expected object: {self:?}")
        };
        value
    }
    fn get(&self, key: &str) -> &Self {
        &self.object()[key]
    }
    fn string(&self) -> &str {
        let Self::String(value) = self else {
            panic!("expected string: {self:?}")
        };
        value
    }
    fn unsigned(&self) -> u64 {
        let Self::Number(value) = self else {
            panic!("expected number: {self:?}")
        };
        value.parse().unwrap()
    }
    fn array(&self) -> &[Self] {
        let Self::Array(value) = self else {
            panic!("expected array: {self:?}")
        };
        value
    }
}

// Same dependency-free JSON-reader convention as cli_robot.rs. Parse values,
// not substrings: escaped strings, duplicated fields and trailing bytes matter.
struct Parser<'a> {
    input: &'a str,
    offset: usize,
}
impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.offset).copied()
    }
    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, byte: u8) {
        assert!(
            self.take(byte),
            "expected {byte} at {} in {:?}",
            self.offset,
            self.input
        );
    }
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.offset += 1;
        }
    }
    fn quad(&mut self) -> u32 {
        let mut result = 0;
        for _ in 0..4 {
            result = result * 16 + char::from(self.peek().unwrap()).to_digit(16).unwrap();
            self.offset += 1;
        }
        result
    }
    fn string(&mut self) -> String {
        self.expect(b'"');
        let mut result = String::new();
        loop {
            match self.peek().expect("terminated JSON string") {
                b'"' => {
                    self.offset += 1;
                    return result;
                }
                b'\\' => {
                    self.offset += 1;
                    let escape = self.peek().unwrap();
                    self.offset += 1;
                    result.push(match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'b' => '\u{08}',
                        b'f' => '\u{0c}',
                        b'u' => {
                            let first = self.quad();
                            let scalar = if (0xd800..=0xdbff).contains(&first) {
                                self.expect(b'\\');
                                self.expect(b'u');
                                let second = self.quad();
                                assert!((0xdc00..=0xdfff).contains(&second));
                                0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
                            } else {
                                first
                            };
                            char::from_u32(scalar).unwrap()
                        }
                        _ => panic!("invalid JSON escape"),
                    });
                }
                0..=31 => panic!("unescaped JSON control"),
                _ => {
                    let ch = self.input[self.offset..].chars().next().unwrap();
                    self.offset += ch.len_utf8();
                    result.push(ch);
                }
            }
        }
    }
    fn value(&mut self) -> Json {
        self.space();
        match self.peek().unwrap() {
            b'"' => Json::String(self.string()),
            b'{' => {
                self.offset += 1;
                self.space();
                let mut fields = BTreeMap::new();
                if self.take(b'}') {
                    return Json::Object(fields);
                }
                loop {
                    self.space();
                    let key = self.string();
                    self.space();
                    self.expect(b':');
                    assert!(
                        fields.insert(key, self.value()).is_none(),
                        "duplicate JSON field"
                    );
                    self.space();
                    if self.take(b'}') {
                        break;
                    }
                    self.expect(b',');
                }
                Json::Object(fields)
            }
            b'[' => {
                self.offset += 1;
                self.space();
                let mut values = Vec::new();
                if self.take(b']') {
                    return Json::Array(values);
                }
                loop {
                    values.push(self.value());
                    self.space();
                    if self.take(b']') {
                        break;
                    }
                    self.expect(b',');
                }
                Json::Array(values)
            }
            b'n' | b't' | b'f' => {
                let (literal, value) = match self.peek().unwrap() {
                    b'n' => ("null", Json::Null),
                    b't' => ("true", Json::Bool(true)),
                    _ => ("false", Json::Bool(false)),
                };
                assert!(self.input[self.offset..].starts_with(literal));
                self.offset += literal.len();
                value
            }
            b'-' | b'0'..=b'9' => {
                let start = self.offset;
                self.take(b'-');
                if !self.take(b'0') {
                    let digits = self.offset;
                    while matches!(self.peek(), Some(b'0'..=b'9')) {
                        self.offset += 1;
                    }
                    assert!(self.offset > digits);
                }
                Json::Number(self.input[start..self.offset].to_owned())
            }
            _ => panic!("invalid JSON value"),
        }
    }
}
fn json(input: &str) -> Json {
    let mut parser = Parser { input, offset: 0 };
    let result = parser.value();
    parser.space();
    assert_eq!(parser.offset, input.len());
    result
}
fn quote(input: &str) -> String {
    let mut result = String::from("\"");
    for ch in input.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            _ => {
                assert!(!ch.is_control());
                result.push(ch);
            }
        }
    }
    result.push('"');
    result
}
fn encode(value: &Json) -> String {
    match value {
        Json::Null => "null".into(),
        Json::Bool(value) => value.to_string(),
        Json::Number(value) => value.clone(),
        Json::String(value) => quote(value),
        Json::Array(values) => format!(
            "[{}]",
            values.iter().map(encode).collect::<Vec<_>>().join(",")
        ),
        Json::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!("{}:{}", quote(key), encode(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}
struct Outcome {
    output: Output,
    events: Vec<Json>,
}
impl Outcome {
    fn new(output: Output) -> Self {
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        assert!(stdout.ends_with('\n'), "{stdout:?}");
        let events: Vec<_> = stdout.lines().map(json).collect();
        assert_eq!(events[0], json(r#"{"v":1,"event":"invocation"}"#));
        for event in &events {
            assert_eq!(event.get("v").unsigned(), 1);
        }
        Self { output, events }
    }
    fn success(&self) -> &Self {
        assert!(
            self.output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&self.output.stdout),
            String::from_utf8_lossy(&self.output.stderr)
        );
        assert!(self.output.stderr.is_empty());
        self
    }
    fn terminal(&self) -> &Json {
        self.events.last().unwrap()
    }
    fn progress(&self) -> Vec<(u64, u64)> {
        self.events
            .iter()
            .filter(|event| event.get("event").string() == "progress")
            .map(|event| {
                assert_eq!(
                    event
                        .object()
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    ["event", "rows", "seq", "v"]
                );
                (event.get("rows").unsigned(), event.get("seq").unsigned())
            })
            .collect()
    }
    fn loaded(&self, rows: usize, chunks: usize, frontier: u64) {
        self.success();
        assert_eq!(
            *self.terminal(),
            json(&format!(
                r#"{{"v":1,"event":"result","kind":"loaded","seq":{frontier},"count":{rows},"statements":{chunks}}}"#
            ))
        );
    }
    fn refusal(&self, kind: &str, line: Option<usize>) {
        assert_eq!(self.output.status.code(), Some(3), "{:?}", self.output);
        assert_eq!(self.terminal().get("event").string(), "error");
        assert_eq!(self.terminal().get("class").string(), "query");
        let diagnostics = self.terminal().get("diagnostics");
        let text = format!("{diagnostics:?}");
        assert!(text.contains(kind), "expected {kind}: {text}");
        if let Some(line) = line {
            assert!(
                text.contains(&format!("line {line}:")),
                "expected 1-based line {line}: {text}"
            );
        }
    }
}
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "fgdb-cli-load-{name}-{}-{time}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}
struct Fixture {
    root: PathBuf,
    db: PathBuf,
    key: PathBuf,
    input: PathBuf,
    checkpoint: PathBuf,
    basis: u64,
}
impl Fixture {
    fn new(name: &str, input: &str) -> Self {
        let root = scratch(name);
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
        // Key files must be owner-only (the CLI refuses group/other bits).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut result = Self {
            db: root.join("db"),
            input: root.join("input.ndjson"),
            checkpoint: root.join("checkpoint"),
            key,
            root,
            basis: 0,
        };
        std::fs::write(&result.input, input).unwrap();
        let created = result.run("create", &[]);
        created.success();
        result.basis = created.terminal().get("seq").unsigned();
        result
    }
    fn command(&self, verb: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command
            .env_remove("FGDB_LOAD_CRASH_CHUNK")
            .env_remove("FGDB_LOAD_CRASH_POINT");
        command
            .args(["--robot", verb, "--db"])
            .arg(&self.db)
            .arg("--key-file")
            .arg(&self.key);
        command.args([
            "--label",
            "Person=1",
            "--label",
            "Other=2",
            "--relation",
            "R=1",
            "--relation",
            "S=2",
            "--property",
            "p=1",
            "--property",
            "k=2",
            "--property",
            "text=3",
            "--property",
            "active=4",
            "--property",
            "nullable=5",
        ]);
        command
    }
    fn run(&self, verb: &str, args: &[&str]) -> Outcome {
        Outcome::new(self.command(verb).args(args).output().unwrap())
    }
    fn load_command(&self, chunk: Option<usize>) -> Command {
        let mut command = self.command("load");
        command
            .arg("--input")
            .arg(&self.input)
            .arg("--checkpoint")
            .arg(&self.checkpoint);
        if let Some(chunk) = chunk {
            command.arg("--rows-per-chunk").arg(chunk.to_string());
        }
        command
    }
    fn load(&self, chunk: Option<usize>) -> Outcome {
        Outcome::new(self.load_command(chunk).output().unwrap())
    }
    fn checkpoint(&self) -> BulkLoadCheckpoint {
        let value = json(&std::fs::read_to_string(&self.checkpoint).unwrap());
        assert_eq!(value.get("v").unsigned(), 1);
        BulkLoadCheckpoint {
            vertices: value
                .get("vertices")
                .object()
                .iter()
                .map(|(key, id)| {
                    let n: u128 = id.string().parse().unwrap();
                    assert_eq!(n.to_string(), id.string());
                    (key.clone(), VId(n))
                })
                .collect(),
            edges: value
                .get("edges")
                .object()
                .iter()
                .map(|(key, id)| {
                    let n: u128 = id.string().parse().unwrap();
                    assert_eq!(n.to_string(), id.string());
                    (key.clone(), EId(n))
                })
                .collect(),
            next_row: value.get("next_row").unsigned() as usize,
            frontier: CommitSeq(value.get("frontier").unsigned()),
            committed_chunks: value.get("committed_chunks").unsigned() as usize,
        }
    }
}
const P: PropertyKeyId = PropertyKeyId(1);
const K: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(K)),
        _ => None,
    }
}
fn query_text(relation: RelationId) -> String {
    format!(
        "MATCH (a)-[:{}]->(b) RETURN ALL a.k AS ak, b.k AS bk",
        if relation == R { "R" } else { "S" }
    )
}
fn answers(db: &Database, cx: &QueryCx, relation: RelationId) -> Vec<Vec<i64>> {
    let query = PreparedGraphText::prepare(&query_text(relation), symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let mut rows: Vec<Vec<i64>> = db
        .execute_graph_pattern_governed(
            cx,
            &query,
            GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        )
        .unwrap()
        .value
        .iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|value| {
                    let GraphValue::Scalar(CanonicalScalar::Int(value)) = value else {
                        panic!("expected int {value:?}")
                    };
                    *value
                })
                .collect()
        })
        .collect();
    rows.sort();
    rows
}
fn cli_answers(fixture: &Fixture, relation: RelationId) -> Vec<Vec<i64>> {
    let output = fixture.run("query", &[&query_text(relation)]);
    output.success();
    let mut rows: Vec<Vec<i64>> = output
        .events
        .iter()
        .filter(|row| row.get("event").string() == "row")
        .map(|row| {
            row.get("cells")
                .array()
                .iter()
                .map(|cell| {
                    assert_eq!(cell.get("type").string(), "int");
                    cell.get("value").string().parse().unwrap()
                })
                .collect()
        })
        .collect();
    rows.sort();
    rows
}
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
fn logical(db: &Database, checkpoint: &BulkLoadCheckpoint) -> (Vertices, Edges) {
    let vertices: BTreeMap<_, _> = checkpoint
        .vertices
        .iter()
        .map(|(key, id)| (*id, key.clone()))
        .collect();
    let edges: BTreeMap<_, _> = checkpoint
        .edges
        .iter()
        .map(|(key, id)| (*id, key.clone()))
        .collect();
    assert_eq!(
        vertices.len(),
        checkpoint.vertices.len(),
        "caller keys cannot alias identities"
    );
    assert_eq!(edges.len(), checkpoint.edges.len());
    let actual_vertices = db.vertices().unwrap();
    let actual_edges = db.edges().unwrap();
    assert_eq!(
        actual_vertices.len(),
        vertices.len(),
        "no unmapped duplicate vertices"
    );
    assert_eq!(
        actual_edges.len(),
        edges.len(),
        "no unmapped duplicate edges"
    );
    (
        actual_vertices
            .into_iter()
            .map(|row| (vertices[&row.vid].clone(), (row.labels, row.props)))
            .collect(),
        actual_edges
            .into_iter()
            .map(|row| {
                (
                    edges[&row.entry.eid].clone(),
                    (
                        vertices[&row.entry.src].clone(),
                        vertices[&row.entry.dst].clone(),
                        row.entry.relation,
                        row.props,
                    ),
                )
            })
            .collect(),
    )
}
fn expected(rows: &[BulkRow]) -> (Vertices, Edges) {
    let mut vertices = BTreeMap::new();
    let mut edges = BTreeMap::new();
    for row in rows {
        match row {
            BulkRow::Vertex(row) => {
                assert!(
                    vertices
                        .insert(row.key.clone(), (row.labels.clone(), row.props.clone()))
                        .is_none()
                );
            }
            BulkRow::Edge(row) => {
                assert!(
                    edges
                        .insert(
                            row.key.clone(),
                            (
                                row.source.clone(),
                                row.destination.clone(),
                                row.relation,
                                row.props.clone()
                            )
                        )
                        .is_none()
                );
            }
        }
    }
    (vertices, edges)
}
fn props(n: usize, payload: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![
        (P, CanonicalScalar::Int(payload)),
        (K, CanonicalScalar::Int(n as i64)),
        (
            PropertyKeyId(3),
            CanonicalScalar::ucs_basic_text(&format!("row {n}: \"quoted\" \\ newline\nUnicode λ"))
                .unwrap(),
        ),
        (PropertyKeyId(4), CanonicalScalar::Bool(n % 2 == 0)),
        (PropertyKeyId(5), CanonicalScalar::Null),
    ]
}
fn generated(seed: u64) -> Vec<BulkRow> {
    let mut state = seed;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 17
    };
    let mut rows = Vec::with_capacity(256);
    for n in 0..48 {
        rows.push(BulkRow::Vertex(BulkVertex {
            key: format!("v{n}"),
            labels: vec![LabelId(if n % 3 == 0 { 2 } else { 1 })],
            props: props(n, (next() % 1999) as i64 - 999),
        }));
    }
    for n in 0..208 {
        rows.push(BulkRow::Edge(BulkEdge {
            key: format!("e{n}"),
            source: format!("v{}", next() % 48),
            destination: format!("v{}", next() % 48),
            relation: if n % 2 == 0 { R } else { S },
            props: props(n, (next() % 1999) as i64 - 999),
        }));
    }
    rows
}
fn ndjson(rows: &[BulkRow]) -> String {
    let properties = |props: &[(PropertyKeyId, CanonicalScalar)]| {
        props
            .iter()
            .map(|(key, value)| {
                let name = match key.0 {
                    1 => "p",
                    2 => "k",
                    3 => "text",
                    4 => "active",
                    5 => "nullable",
                    _ => panic!("unknown test property"),
                };
                let value = match value {
                    CanonicalScalar::Int(value) => format!("int:{value}"),
                    CanonicalScalar::Bool(value) => format!("bool:{value}"),
                    CanonicalScalar::Null => "null".into(),
                    _ if *key == PropertyKeyId(3) => {
                        // Derive text from the independent ordinal property; the logical
                        // comparison below checks its actual persisted canonical value.
                        let (_, CanonicalScalar::Int(n)) =
                            props.iter().find(|(key, _)| *key == K).unwrap()
                        else {
                            panic!("ordinal")
                        };
                        format!("text:row {n}: \"quoted\" \\ newline\nUnicode λ")
                    }
                    _ => panic!("unsupported generated value"),
                };
                format!("{}:{}", quote(name), quote(&value))
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    rows.iter().map(|row| match row {
        BulkRow::Vertex(row) => format!("{{\"kind\":\"vertex\",\"key\":{},\"labels\":[{}],\"props\":{{{}}}}}\n", quote(&row.key), row.labels.iter().map(|label| quote(if label.0 == 1 { "Person" } else { "Other" })).collect::<Vec<_>>().join(","), properties(&row.props)),
        BulkRow::Edge(row) => format!("{{\"kind\":\"edge\",\"key\":{},\"source\":{},\"destination\":{},\"relation\":{},\"props\":{{{}}}}}\n", quote(&row.key), quote(&row.source), quote(&row.destination), quote(if row.relation == R { "R" } else { "S" }), properties(&row.props)),
    }).collect()
}
fn runtime_check(test: impl AsyncFnOnce(PurposeContexts)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(test(PurposeContexts::narrow_runtime_root(&root)));
}
async fn open(path: &Path, contexts: &PurposeContexts) -> Database {
    Database::open(&contexts.commit(), path, keys())
        .await
        .unwrap()
}

#[test]
fn load_progress_and_exact_typed_values() {
    let rows = generated(7);
    let fixture = Fixture::new("values", &ndjson(&rows[..53]));
    let outcome = fixture.load(Some(11));
    outcome.loaded(53, 5, fixture.basis + 5);
    assert_eq!(
        outcome.progress(),
        vec![
            (11, fixture.basis + 1),
            (22, fixture.basis + 2),
            (33, fixture.basis + 3),
            (44, fixture.basis + 4),
            (53, fixture.basis + 5)
        ]
    );
    let checkpoint = fixture.checkpoint();
    assert_eq!(
        (
            checkpoint.next_row,
            checkpoint.committed_chunks,
            checkpoint.frontier
        ),
        (53, 5, CommitSeq(fixture.basis + 5))
    );
    runtime_check(async |contexts| {
        let db = open(&fixture.db, &contexts).await;
        assert_eq!(logical(&db, &checkpoint), expected(&rows[..53]));
        assert_eq!(db.delta_since(CommitSeq(fixture.basis)).unwrap().count(), 5);
    });
}

#[test]
fn default_chunk_size_and_completed_resume_do_not_invent_commits() {
    let rows = generated(9);
    let fixture = Fixture::new("default", &ndjson(&rows));
    fixture.load(None).loaded(256, 1, fixture.basis + 1);
    let saved = std::fs::read(&fixture.checkpoint).unwrap();
    let resumed = fixture.load(None);
    resumed.loaded(256, 1, fixture.basis + 1);
    assert!(resumed.progress().is_empty());
    assert_eq!(
        saved,
        std::fs::read(&fixture.checkpoint).unwrap(),
        "completed checkpoint encoding is stable"
    );
}

fn refusal_preserves_frontier(bad: &str, kind: &str) {
    let prefix = "{\"kind\":\"vertex\",\"key\":\"v0\",\"labels\":[\"Person\"],\"props\":{\"p\":\"int:7\"}}\n";
    let fixture = Fixture::new("refusal", &format!("{prefix}{bad}\n"));
    // A pre-existing commit makes an accidental reset-to-zero frontier visible.
    fixture
        .run("write", &["CREATE (n:Person {p:99})"])
        .success();
    let baseline = fixture.run("query", &["MATCH (n:Person) RETURN n.p AS value"]);
    baseline.success();
    let frontier = baseline.terminal().get("seq").unsigned();
    let failure = fixture.load(Some(1));
    failure.refusal(kind, Some(2));
    assert!(
        failure.progress().is_empty(),
        "full-input preflight rejects before committing"
    );
    let after = fixture.run("query", &["MATCH (n:Person) RETURN n.p AS value"]);
    after.success();
    assert_eq!(after.events, baseline.events);
    runtime_check(async |contexts| {
        let db = open(&fixture.db, &contexts).await;
        assert_eq!(db.frontier().unwrap(), CommitSeq(frontier));
        assert_eq!(db.delta_since(CommitSeq(frontier)).unwrap().count(), 0);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert!(db.edges().unwrap().is_empty());
    });
}
#[test]
fn malformed_line_is_typed_line_two_and_atomic() {
    refusal_preserves_frontier("{not-json}", "MalformedRow");
}
#[test]
fn unknown_label_relation_and_property_are_typed_line_two_and_atomic() {
    for row in [
        r#"{"kind":"vertex","key":"v1","labels":["Missing"],"props":{}}"#,
        r#"{"kind":"vertex","key":"v1","labels":["Person"],"props":{"missing":"int:1"}}"#,
        r#"{"kind":"edge","key":"e1","source":"v0","destination":"v0","relation":"Missing","props":{}}"#,
    ] {
        refusal_preserves_frontier(row, "UnknownBinding");
    }
}
#[test]
fn duplicate_caller_key_across_kinds_is_typed_line_two_and_atomic() {
    refusal_preserves_frontier(
        r#"{"kind":"edge","key":"v0","source":"v0","destination":"v0","relation":"R","props":{}}"#,
        "DuplicateCallerKey",
    );
}
#[test]
fn dangling_endpoint_is_typed_line_two_and_atomic() {
    refusal_preserves_frontier(
        r#"{"kind":"edge","key":"e1","source":"v0","destination":"missing","relation":"R","props":{}}"#,
        "DanglingEndpointKey",
    );
}
#[test]
fn unstorable_unsigned_and_invalid_parameter_are_typed_line_two_and_atomic() {
    for value in [
        "uint:18446744073709551615",
        "list:[int:1]",
        "int:not-a-number",
    ] {
        refusal_preserves_frontier(
            &format!(
                "{{\"kind\":\"vertex\",\"key\":\"v1\",\"labels\":[\"Person\"],\"props\":{{\"p\":{}}}}}",
                quote(value)
            ),
            "InvalidProperty",
        );
    }
}

fn resume_scenario(before_capsule: bool) {
    let rows = generated(0xcafe);
    let source = ndjson(&rows);
    let fixture = Fixture::new("resume", &source);
    let uninterrupted = Fixture::new("uninterrupted", &source);
    let mut interrupted_command = fixture.load_command(Some(31));
    interrupted_command.env("FGDB_LOAD_CRASH_CHUNK", "2");
    if before_capsule {
        interrupted_command.env("FGDB_LOAD_CRASH_POINT", "before-capsule");
    }
    let interrupted = Outcome::new(interrupted_command.output().unwrap());
    assert!(
        !interrupted.output.status.success(),
        "production crash seam must interrupt"
    );
    assert_eq!(
        interrupted.progress(),
        vec![(31, fixture.basis + 1), (62, fixture.basis + 2)]
    );
    let checkpoint = fixture.checkpoint();
    // This is the planted-negative witness: checkpoint-before-ack incorrectly
    // exposes the failing chunk and violates these exact durable prefix values.
    assert_eq!(
        (
            checkpoint.next_row,
            checkpoint.committed_chunks,
            checkpoint.frontier
        ),
        (62, 2, CommitSeq(fixture.basis + 2))
    );
    assert_eq!(
        (checkpoint.vertices.len(), checkpoint.edges.len()),
        (48, 14)
    );
    runtime_check(async |contexts| {
        let recovered = open(&fixture.db, &contexts).await;
        let committed = if before_capsule { 2 } else { 3 };
        assert_eq!(
            recovered.frontier().unwrap(),
            CommitSeq(fixture.basis + committed)
        );
        assert_eq!(
            recovered
                .delta_since(CommitSeq(fixture.basis))
                .unwrap()
                .count(),
            committed as usize
        );
        if before_capsule {
            assert_eq!(logical(&recovered, &checkpoint), expected(&rows[..62]));
        }
    });
    let resumed = fixture.load(Some(31));
    resumed.loaded(256, 9, fixture.basis + 9);
    let remaining: Vec<_> = (3..=9)
        .map(|chunk| ((chunk * 31).min(256), fixture.basis + chunk))
        .collect();
    assert_eq!(
        resumed.progress(),
        remaining,
        "recovery acknowledges each formerly unacknowledged chunk exactly once"
    );
    uninterrupted
        .load(Some(31))
        .loaded(256, 9, uninterrupted.basis + 9);
    let completed = fixture.checkpoint();
    let baseline = uninterrupted.checkpoint();
    let cli_actual = [R, S].map(|relation| cli_answers(&fixture, relation));
    let cli_reference = [R, S].map(|relation| cli_answers(&uninterrupted, relation));
    runtime_check(async |contexts| {
        let actual = open(&fixture.db, &contexts).await;
        let reference = open(&uninterrupted.db, &contexts).await;
        assert_eq!(logical(&actual, &completed), expected(&rows));
        assert_eq!(logical(&actual, &completed), logical(&reference, &baseline));
        assert_eq!(
            actual
                .delta_since(CommitSeq(fixture.basis))
                .unwrap()
                .count(),
            9
        );
        for (index, relation) in [R, S].into_iter().enumerate() {
            let answer = answers(&reference, &contexts.query(), relation);
            assert_eq!(answers(&actual, &contexts.query(), relation), answer);
            assert_eq!(cli_actual[index], answer);
            assert_eq!(cli_reference[index], answer);
        }
    });
}
#[test]
fn checkpoint_resumes_in_new_process_without_duplicates() {
    resume_scenario(false);
}
#[test]
fn checkpoint_never_acknowledges_failed_chunk_before_capsule() {
    resume_scenario(true);
}

#[test]
fn checkpoint_future_frontier_is_invalid_resume_and_atomic() {
    let fixture = Fixture::new("future", &ndjson(&generated(11)));
    fixture.load(Some(31)).loaded(256, 9, fixture.basis + 9);
    let mut checkpoint = json(&std::fs::read_to_string(&fixture.checkpoint).unwrap());
    let Json::Object(fields) = &mut checkpoint else {
        unreachable!()
    };
    fields.insert(
        "frontier".into(),
        Json::Number((fixture.basis + 1000).to_string()),
    );
    let forged = encode(&checkpoint);
    std::fs::write(&fixture.checkpoint, &forged).unwrap();
    let failure = fixture.load(Some(31));
    failure.refusal("InvalidResume", None);
    assert!(failure.progress().is_empty());
    assert_eq!(
        std::fs::read_to_string(&fixture.checkpoint).unwrap(),
        forged
    );
    runtime_check(async |contexts| {
        let db = open(&fixture.db, &contexts).await;
        assert_eq!(db.frontier().unwrap(), CommitSeq(fixture.basis + 9));
        assert_eq!(
            db.delta_since(CommitSeq(fixture.basis + 9))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            (db.vertices().unwrap().len(), db.edges().unwrap().len()),
            (48, 208)
        );
    });
}

#[test]
fn three_seed_256_row_subprocess_matches_bulk_load_by_caller_key_and_gql() {
    for seed in [1_u64, 0xfeed_beef, 0x1234_5678_9abc_def0] {
        let rows = generated(seed);
        assert_eq!(rows.len(), 256);
        let fixture = Fixture::new("differential", &ndjson(&rows));
        let outcome = fixture.load(Some(31));
        outcome.loaded(256, 9, fixture.basis + 9);
        assert_eq!(
            outcome.progress(),
            (1..=9)
                .map(|chunk| ((chunk * 31).min(256), fixture.basis + chunk))
                .collect::<Vec<_>>()
        );
        let checkpoint = fixture.checkpoint();
        let cli_actual = [R, S].map(|relation| cli_answers(&fixture, relation));
        runtime_check(async |contexts| {
            let mut reference =
                Database::create(&contexts.commit(), fixture.root.join("reference"), keys())
                    .await
                    .unwrap();
            // Deliberately different chunking: joins use caller keys, never a
            // coincidental common identity allocation or commit sequence.
            let reference_checkpoint = reference
                .bulk_load(
                    &contexts.query(),
                    &contexts.commit(),
                    rows.clone(),
                    BulkLoadPolicy::new(47, R),
                )
                .await
                .unwrap();
            let actual = open(&fixture.db, &contexts).await;
            assert_eq!(
                logical(&actual, &checkpoint),
                expected(&rows),
                "seed={seed}"
            );
            assert_eq!(
                logical(&actual, &checkpoint),
                logical(&reference, &reference_checkpoint),
                "seed={seed}"
            );
            for (index, relation) in [R, S].into_iter().enumerate() {
                let mut expected_answers: Vec<Vec<i64>> = rows
                    .iter()
                    .filter_map(|row| match row {
                        BulkRow::Edge(row) if row.relation == relation => Some(vec![
                            row.source.strip_prefix('v').unwrap().parse().unwrap(),
                            row.destination.strip_prefix('v').unwrap().parse().unwrap(),
                        ]),
                        _ => None,
                    })
                    .collect();
                expected_answers.sort();
                assert_eq!(
                    answers(&reference, &contexts.query(), relation),
                    expected_answers,
                    "oracle seed={seed}"
                );
                assert_eq!(
                    answers(&actual, &contexts.query(), relation),
                    expected_answers,
                    "engine seed={seed}"
                );
                assert_eq!(cli_actual[index], expected_answers, "CLI seed={seed}");
            }
        });
    }
}
