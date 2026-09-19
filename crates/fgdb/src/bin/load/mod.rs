//! NDJSON adaptation and durable continuation; all writes use the bulk engine.
use super::{Failure, Options, emit, hex, parameter, quoted};
use asupersync::fs::Vfs;
use fgdb::{
    BulkEdge, BulkLoadCheckpoint, BulkLoadErrorKind, BulkLoadPolicy, BulkRow, BulkVertex, Database,
};
use fgdb_delta_types::{DeltaRow, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::GqlParameterValue;
use fgdb_types::{CanonicalScalar, CommitSeq, EId, PurposeContexts, VId};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    path::Path,
};

fn invalid(message: impl std::fmt::Display) -> Failure {
    Failure::query(format!("InvalidResume: {message}"))
}
fn line_error(line: usize, kind: &str, message: impl std::fmt::Display) -> Failure {
    Failure::query(format!("line {line}: {kind}: {message}"))
}

pub(super) async fn run<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let cx = contexts.query();
    cx.checkpoint().map_err(Failure::io)?;
    let bytes = asupersync::fs::read(options.input.as_ref().expect("required input"))
        .await
        .map_err(Failure::io)?;
    let text = std::str::from_utf8(&bytes).map_err(|e| {
        let line = bytes[..e.valid_up_to()]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        line_error(line, "MalformedRow", "input must be UTF-8")
    })?;
    let mut rows = Vec::new();
    let mut keys = BTreeSet::new();
    let mut vertices = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        cx.checkpoint().map_err(Failure::io)?;
        let row = parse_row(line, options, resolver)
            .map_err(|e| line_error(index + 1, "MalformedRow", e))?;
        let key = match &row {
            BulkRow::Vertex(v) => &v.key,
            BulkRow::Edge(e) => &e.key,
        };
        if !keys.insert(key.clone()) {
            return Err(line_error(index + 1, "DuplicateCallerKey", key));
        }
        match &row {
            BulkRow::Vertex(v) => {
                fgdb_strata::vertex::admit_row_content(&v.labels, &v.props)
                    .map_err(|e| line_error(index + 1, "InvalidVertex", format!("{e:?}")))?;
                vertices.insert(v.key.clone());
            }
            BulkRow::Edge(e) => {
                for endpoint in [&e.source, &e.destination] {
                    if !vertices.contains(endpoint) {
                        return Err(line_error(index + 1, "DanglingEndpointKey", endpoint));
                    }
                }
                fgdb_strata::edge_props::admitted_row_bytes(&e.props)
                    .map_err(|e| line_error(index + 1, "InvalidEdge", format!("{e:?}")))?;
            }
        }
        rows.push(row);
    }
    // The source and bindings identify this import, not the checkpoint maps.
    // Full-source pinning is intentionally stricter than prefix-only pinning.
    let mut digest = fgdb_crypto::blake3::Hasher::new();
    digest.update(&bytes);
    digest.update(
        format!(
            "\n{:?}\n{:?}\n{:?}\n{}",
            options.labels, options.relations, options.properties, options.coordinate.0
        )
        .as_bytes(),
    );
    let source_hash = hex(&digest.finalize().0);
    let current = db.frontier().map_err(Failure::io)?;
    let saved = if let Some(path) = &options.checkpoint {
        cx.checkpoint().map_err(Failure::io)?;
        match asupersync::fs::read_to_string(path).await {
            Ok(text) => Some(Saved::decode(&text).map_err(invalid)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(Failure::io(e)),
        }
    } else {
        None
    };
    let (base, base_marker) = if let Some(saved) = &saved {
        if saved.source_hash != source_hash || saved.rows_per_chunk != options.rows_per_chunk {
            return Err(invalid("source or chunk policy changed"));
        }
        if saved.checkpoint.frontier.0 > current.0 || saved.base.0 > saved.checkpoint.frontier.0 {
            return Err(invalid("checkpoint frontier is not in recovered history"));
        }
        (saved.base, saved.base_marker.clone())
    } else {
        (current, marker(db, current)?)
    };
    if marker(db, base)? != base_marker {
        return Err(invalid("base history identity changed"));
    }
    let mut checkpoint = BulkLoadCheckpoint {
        frontier: base,
        ..BulkLoadCheckpoint::default()
    };
    if let Some(saved) = &saved {
        if saved.checkpoint.frontier == base {
            same_checkpoint(&saved.checkpoint, &checkpoint)?;
        }
        // Reconstruct maps from authenticated creation effects, including the
        // commit -> checkpoint crash window. No checkpoint guesses an identity.
        for batch in db.delta_since(base).map_err(invalid)? {
            cx.checkpoint().map_err(Failure::io)?;
            reconcile(batch, &rows, options.rows_per_chunk, &mut checkpoint)?;
            if checkpoint.frontier == saved.checkpoint.frontier {
                same_checkpoint(&saved.checkpoint, &checkpoint)?;
            }
        }
    }
    let save = |cp: &BulkLoadCheckpoint| -> io::Result<()> {
        if let Some(path) = &options.checkpoint {
            cx.checkpoint().map_err(io::Error::other)?;
            let saved = Saved {
                checkpoint: cp.clone(),
                base,
                base_marker: base_marker.clone(),
                source_hash: source_hash.clone(),
                rows_per_chunk: options.rows_per_chunk,
            };
            persist(&contexts.commit(), path, &saved.encode())?;
        }
        Ok(())
    };
    // Establish the import origin before its first commit. A crash in the very
    // first commit can therefore be reconciled just like later chunks.
    save(&checkpoint).map_err(Failure::io)?;
    if let Some(saved) = &saved {
        // Recovery positively acknowledges history that the interrupted
        // process could not announce. Persist that recovered state first.
        for chunk in saved.checkpoint.committed_chunks..checkpoint.committed_chunks {
            let count = (chunk + 1)
                .saturating_mul(options.rows_per_chunk)
                .min(rows.len());
            let seq = base.0 + chunk as u64 + 1;
            if robot {
                emit(
                    out,
                    &format!("{{\"v\":1,\"event\":\"progress\",\"rows\":{count},\"seq\":{seq}}}"),
                )?;
            } else {
                writeln!(out, "loaded {count} rows (seq {seq})").map_err(Failure::io)?;
            }
            out.flush().map_err(Failure::io)?;
        }
    }
    let crash = match std::env::var("FGDB_LOAD_CRASH_CHUNK") {
        Ok(value) => {
            let index = value
                .parse()
                .map_err(|_| Failure::usage("invalid FGDB_LOAD_CRASH_CHUNK"))?;
            let point = match std::env::var("FGDB_LOAD_CRASH_POINT").as_deref() {
                Ok("before-capsule") => fgdb::CrashPoint::BeforeCapsule,
                Ok("after-d1") => fgdb::CrashPoint::AfterD1,
                Ok("after-marker-sync") | Err(_) => {
                    fgdb::CrashPoint::AfterMarkerFileSyncBeforeDirectorySync
                }
                _ => return Err(Failure::usage("invalid FGDB_LOAD_CRASH_POINT")),
            };
            Some((index, point))
        }
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err(Failure::usage("invalid FGDB_LOAD_CRASH_CHUNK")),
    };
    let policy = BulkLoadPolicy {
        rows_per_chunk: options.rows_per_chunk,
        vertex_relation: options.coordinate,
        resume: Some(checkpoint),
    };
    let result = db
        .bulk_load_with_checkpoint(&cx, &contexts.commit(), rows, policy, crash, |cp| {
            save(cp)?;
            if robot {
                writeln!(
                    out,
                    "{{\"v\":1,\"event\":\"progress\",\"rows\":{},\"seq\":{}}}",
                    cp.next_row, cp.frontier.0
                )?;
            } else {
                writeln!(out, "loaded {} rows (seq {})", cp.next_row, cp.frontier.0)?;
            }
            out.flush()
        })
        .await
        .map_err(|e| match e.kind {
            BulkLoadErrorKind::Checkpoint(error) => Failure::io(error),
            BulkLoadErrorKind::Write(error) => super::execution_failure(error),
            kind => line_error(e.committed.next_row + 1, "BulkLoad", format!("{kind:?}")),
        })?;
    if robot {
        emit(
            out,
            &format!(
                "{{\"v\":1,\"event\":\"result\",\"kind\":\"loaded\",\"seq\":{},\"count\":{},\"statements\":{}}}",
                result.frontier.0, result.next_row, result.committed_chunks
            ),
        )
    } else {
        writeln!(
            out,
            "loaded {} rows in {} chunks (seq {})",
            result.next_row, result.committed_chunks, result.frontier.0
        )
        .map_err(Failure::io)
    }
}

fn marker<V: Vfs + Clone>(db: &Database<V>, seq: CommitSeq) -> Result<String, Failure> {
    if seq.0 == 0 {
        return Ok(String::new());
    }
    let batch = db
        .delta_since(CommitSeq(seq.0 - 1))
        .map_err(invalid)?
        .next()
        .ok_or_else(|| invalid("missing history marker"))?;
    if batch.commit_seq() != seq {
        return Err(invalid("missing history marker"));
    }
    Ok(hex(&batch.commit_marker_identity().marker_oid.0))
}
fn same_checkpoint(a: &BulkLoadCheckpoint, b: &BulkLoadCheckpoint) -> Result<(), Failure> {
    if a.vertices != b.vertices
        || a.edges != b.edges
        || a.next_row != b.next_row
        || a.frontier != b.frontier
        || a.committed_chunks != b.committed_chunks
    {
        return Err(invalid("checkpoint does not match recovered source prefix"));
    }
    Ok(())
}
fn reconcile(
    batch: &fgdb_delta_types::LogicalDeltaBatch,
    rows: &[BulkRow],
    size: usize,
    cp: &mut BulkLoadCheckpoint,
) -> Result<(), Failure> {
    let end = cp.next_row.saturating_add(size).min(rows.len());
    let chunk = rows
        .get(cp.next_row..end)
        .ok_or_else(|| invalid("source shorter than history"))?;
    if chunk.is_empty() || batch.commit_seq().0 != cp.frontier.0 + 1 {
        return Err(invalid("unexpected history after import"));
    }
    let mut vs = BTreeMap::new();
    let mut es = BTreeMap::new();
    for entry in batch.coordinate_entries() {
        for row in &entry.rows {
            match row {
                DeltaRow::CreateVertex {
                    vid,
                    labels,
                    props,
                    valid_time: None,
                    ..
                } => {
                    vs.insert(*vid, (labels, props));
                }
                DeltaRow::CreateEdge {
                    eid,
                    src,
                    dst,
                    relation,
                    props,
                    canonical_key: None,
                    valid_time: None,
                    ..
                } => {
                    es.insert(*eid, (*src, *dst, *relation, props));
                }
                _ => return Err(invalid("history is not a bulk creation chunk")),
            }
        }
    }
    if vs.len() + es.len() != chunk.len() {
        return Err(invalid("history chunk row count changed"));
    }
    // Native identities are allocated monotonically per kind, in source order.
    let mut vs = vs.into_iter();
    let mut es = es.into_iter();
    for row in chunk {
        match row {
            BulkRow::Vertex(v) => {
                let (id, (labels, props)) = vs
                    .next()
                    .ok_or_else(|| invalid("missing vertex creation"))?;
                if labels != &v.labels || props != &v.props {
                    return Err(invalid("vertex source prefix changed"));
                }
                cp.vertices.insert(v.key.clone(), id);
            }
            BulkRow::Edge(e) => {
                let (id, (src, dst, relation, props)) =
                    es.next().ok_or_else(|| invalid("missing edge creation"))?;
                if cp.vertices.get(&e.source) != Some(&src)
                    || cp.vertices.get(&e.destination) != Some(&dst)
                    || relation != e.relation
                    || props != &e.props
                {
                    return Err(invalid("edge source prefix changed"));
                }
                cp.edges.insert(e.key.clone(), id);
            }
        }
    }
    cp.next_row = end;
    cp.committed_chunks += 1;
    cp.frontier = batch.commit_seq();
    Ok(())
}

struct Saved {
    checkpoint: BulkLoadCheckpoint,
    base: CommitSeq,
    base_marker: String,
    source_hash: String,
    rows_per_chunk: usize,
}
impl Saved {
    fn encode(&self) -> String {
        let cp = &self.checkpoint;
        let vertices = cp
            .vertices
            .iter()
            .map(|(k, v)| format!("{}:{}", quoted(k), quoted(&v.0.to_string())))
            .collect::<Vec<_>>()
            .join(",");
        let edges = cp
            .edges
            .iter()
            .map(|(k, v)| format!("{}:{}", quoted(k), quoted(&v.0.to_string())))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"v\":1,\"vertices\":{{{vertices}}},\"edges\":{{{edges}}},\"next_row\":{},\"frontier\":{},\"committed_chunks\":{},\"base_frontier\":{},\"base_marker\":{},\"source_hash\":{},\"rows_per_chunk\":{}}}\n",
            cp.next_row,
            cp.frontier.0,
            cp.committed_chunks,
            self.base.0,
            quoted(&self.base_marker),
            quoted(&self.source_hash),
            self.rows_per_chunk
        )
    }
    fn decode(text: &str) -> Result<Self, String> {
        let json = JsonParser::parse(text)?;
        let fields = object(&json)?;
        if number(field(fields, "v")?)? != 1 || fields.len() != 10 {
            return Err("unknown checkpoint format".into());
        }
        let id = |value: &Json| -> Result<u128, String> {
            let text = string(value)?;
            let value: u128 = text.parse().map_err(|_| "invalid identity")?;
            if text != value.to_string() {
                return Err("noncanonical identity".into());
            }
            Ok(value)
        };
        let vertices = object(field(fields, "vertices")?)?
            .iter()
            .map(|(k, v)| Ok((k.clone(), VId(id(v)?))))
            .collect::<Result<_, String>>()?;
        let edges = object(field(fields, "edges")?)?
            .iter()
            .map(|(k, v)| Ok((k.clone(), EId(id(v)?))))
            .collect::<Result<_, String>>()?;
        let usize_field = |name| {
            usize::try_from(number(field(fields, name)?)?)
                .map_err(|_| "checkpoint counter overflow".to_owned())
        };
        Ok(Self {
            checkpoint: BulkLoadCheckpoint {
                vertices,
                edges,
                next_row: usize_field("next_row")?,
                frontier: CommitSeq(number(field(fields, "frontier")?)?),
                committed_chunks: usize_field("committed_chunks")?,
            },
            base: CommitSeq(number(field(fields, "base_frontier")?)?),
            base_marker: string(field(fields, "base_marker")?)?.to_owned(),
            source_hash: string(field(fields, "source_hash")?)?.to_owned(),
            rows_per_chunk: usize_field("rows_per_chunk")?,
        })
    }
}
fn persist(cx: &fgdb_types::CommitCx, path: &Path, text: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("checkpoint needs a file name"))?;
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".tmp.{}", std::process::id()));
    let temp = parent.join(temp_name);
    cx.checkpoint().map_err(io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)?;
    cx.checkpoint().map_err(io::Error::other)?;
    file.write_all(text.as_bytes())?;
    cx.checkpoint().map_err(io::Error::other)?;
    file.sync_all()?;
    cx.checkpoint().map_err(io::Error::other)?;
    std::fs::rename(&temp, path)?;
    cx.checkpoint().map_err(io::Error::other)?;
    std::fs::File::open(parent)?.sync_all()
}

fn parse_row(
    text: &str,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
) -> Result<BulkRow, String> {
    let json = JsonParser::parse(text)?;
    let fields = object(&json)?;
    let key = string(field(fields, "key")?)?.to_owned();
    if key.is_empty() {
        return Err("caller key must not be empty".into());
    }
    let mut props = Vec::new();
    if let Some(values) = fields.get("props") {
        for (name, value) in object(values)? {
            let id = options
                .properties
                .get(name)
                .ok_or_else(|| format!("UnknownBinding: property {name}"))?;
            let value = parameter(string(value)?, resolver)
                .map_err(|e| format!("InvalidProperty: {}", e.message))?;
            let value = match value {
                GqlParameterValue::Int64(value) => CanonicalScalar::Int(value),
                GqlParameterValue::Scalar(value) => value.value().clone(),
                GqlParameterValue::UInt64(_) | GqlParameterValue::List(_) => {
                    return Err("InvalidProperty: value is not a storable canonical scalar".into());
                }
            };
            props.push((PropertyKeyId(u64::from(*id)), value));
        }
    }
    props.sort_by_key(|(id, _)| *id);
    match string(field(fields, "kind")?)? {
        "vertex" => {
            reject_extra(fields, &["kind", "key", "labels", "props"])?;
            let Json::Array(names) = field(fields, "labels")? else {
                return Err("labels must be an array".into());
            };
            let mut labels = Vec::new();
            for name in names {
                let name = string(name)?;
                let id = options
                    .labels
                    .get(name)
                    .ok_or_else(|| format!("UnknownBinding: label {name}"))?;
                labels.push(LabelId(u64::from(*id)));
            }
            labels.sort();
            if labels.windows(2).any(|w| w[0] == w[1]) {
                return Err("duplicate label".into());
            }
            Ok(BulkRow::Vertex(BulkVertex { key, labels, props }))
        }
        "edge" => {
            reject_extra(
                fields,
                &["kind", "key", "source", "destination", "relation", "props"],
            )?;
            let relation = string(field(fields, "relation")?)?;
            let relation = options
                .relations
                .get(relation)
                .ok_or_else(|| format!("UnknownBinding: relation {relation}"))?;
            Ok(BulkRow::Edge(BulkEdge {
                key,
                source: string(field(fields, "source")?)?.to_owned(),
                destination: string(field(fields, "destination")?)?.to_owned(),
                relation: RelationId(u64::from(*relation)),
                props,
            }))
        }
        _ => Err("kind must be vertex or edge".into()),
    }
}
fn reject_extra(fields: &BTreeMap<String, Json>, names: &[&str]) -> Result<(), String> {
    if fields.keys().any(|k| !names.contains(&k.as_str())) {
        return Err("unknown row field".into());
    }
    Ok(())
}
fn object(json: &Json) -> Result<&BTreeMap<String, Json>, String> {
    if let Json::Object(v) = json {
        Ok(v)
    } else {
        Err("expected object".into())
    }
}
fn string(json: &Json) -> Result<&str, String> {
    if let Json::String(v) = json {
        Ok(v)
    } else {
        Err("expected string".into())
    }
}
fn number(json: &Json) -> Result<u64, String> {
    let Json::Number(v) = json else {
        return Err("expected unsigned number".into());
    };
    let value = v.parse::<u64>().map_err(|_| "expected unsigned number")?;
    if value.to_string() != *v {
        return Err("noncanonical unsigned number".into());
    }
    Ok(value)
}
fn field<'a>(fields: &'a BTreeMap<String, Json>, name: &str) -> Result<&'a Json, String> {
    fields.get(name).ok_or_else(|| format!("missing {name}"))
}

// Same strict, dependency-free JSON grammar as cli_robot's contract reader;
// production input additionally caps nesting before recursive descent.
#[derive(Debug)]
enum Json {
    Null,
    Bool,
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}
struct JsonParser<'a> {
    input: &'a str,
    offset: usize,
    depth: usize,
}
impl<'a> JsonParser<'a> {
    fn parse(input: &'a str) -> Result<Json, String> {
        let mut parser = Self {
            input,
            offset: 0,
            depth: 0,
        };
        let value = parser.value()?;
        parser.whitespace();
        if parser.offset != input.len() {
            return Err(format!("trailing JSON bytes at {}", parser.offset));
        }
        Ok(value)
    }
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.offset).copied()
    }
    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err(format!(
                "expected {:?} at {}",
                char::from(byte),
                self.offset
            ))
        }
    }
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.offset += 1;
        }
    }
    fn value(&mut self) -> Result<Json, String> {
        self.whitespace();
        if self.depth >= 32 {
            return Err("JSON nesting limit exceeded".into());
        }
        self.depth += 1;
        let result = match self.peek() {
            Some(b'"') => self.string().map(Json::String),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b't') => self.literal("true", Json::Bool),
            Some(b'f') => self.literal("false", Json::Bool),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("expected JSON value at {}", self.offset)),
        };
        self.depth -= 1;
        result
    }
    fn literal(&mut self, text: &str, value: Json) -> Result<Json, String> {
        if !self.input[self.offset..].starts_with(text) {
            return Err("invalid JSON literal".into());
        }
        self.offset += text.len();
        Ok(value)
    }
    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        self.whitespace();
        let mut fields = BTreeMap::new();
        if self.consume(b'}') {
            return Ok(Json::Object(fields));
        }
        loop {
            self.whitespace();
            let name = self.string()?;
            self.whitespace();
            self.expect(b':')?;
            let value = self.value()?;
            if fields.insert(name, value).is_some() {
                return Err("duplicate JSON object field".into());
            }
            self.whitespace();
            if self.consume(b'}') {
                return Ok(Json::Object(fields));
            }
            self.expect(b',')?;
        }
    }
    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        self.whitespace();
        let mut values = Vec::new();
        if self.consume(b']') {
            return Ok(Json::Array(values));
        }
        loop {
            values.push(self.value()?);
            self.whitespace();
            if self.consume(b']') {
                return Ok(Json::Array(values));
            }
            self.expect(b',')?;
        }
    }
    fn hex_quad(&mut self) -> Result<u32, String> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = self
                .peek()
                .and_then(|byte| char::from(byte).to_digit(16))
                .ok_or("invalid Unicode escape")?;
            self.offset += 1;
            value = value * 16 + digit;
        }
        Ok(value)
    }
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut text = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated JSON string".into()),
                Some(b'"') => {
                    self.offset += 1;
                    return Ok(text);
                }
                Some(b'\\') => {
                    self.offset += 1;
                    let escape = self.peek().ok_or("unterminated JSON escape")?;
                    self.offset += 1;
                    text.push(match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{08}',
                        b'f' => '\u{0c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex_quad()?;
                            let scalar = if (0xd800..=0xdbff).contains(&first) {
                                self.expect(b'\\')?;
                                self.expect(b'u')?;
                                let second = self.hex_quad()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err("invalid low surrogate".into());
                                }
                                0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
                            } else {
                                first
                            };
                            char::from_u32(scalar).ok_or("invalid Unicode scalar")?
                        }
                        _ => return Err("invalid JSON escape".into()),
                    });
                }
                Some(0..=0x1f) => return Err("unescaped control character".into()),
                Some(_) => {
                    let ch = self.input[self.offset..]
                        .chars()
                        .next()
                        .expect("remaining character");
                    self.offset += ch.len_utf8();
                    text.push(ch);
                }
            }
        }
    }
    fn digits(&mut self) -> Result<(), String> {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        if self.offset == start {
            Err("expected digit".into())
        } else {
            Ok(())
        }
    }
    fn number(&mut self) -> Result<Json, String> {
        let start = self.offset;
        self.consume(b'-');
        if !self.consume(b'0') {
            self.digits()?;
        }
        if self.consume(b'.') {
            self.digits()?;
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            self.digits()?;
        }
        Ok(Json::Number(self.input[start..self.offset].to_owned()))
    }
}
