//! Process-level fuzz campaign for the fgdb CLI robot-mode contract.
//! Every invocation spawns the BUILT binary (`CARGO_BIN_EXE_fgdb` by
//! default; `CLI_FUZZ_BIN` overrides for negative-control drills only —
//! a substitute that violates the contract FAILS, never passes) and the
//! frozen NDJSON contract is asserted with a dependency-free parser.
//! Knobs: `CLI_FUZZ_SEEDS` (default 3, >=3 asserted), `CLI_FUZZ_ITERS`
//! (default 86 per seed/family). The floor is 1,500 process invocations;
//! the campaign asserts a 180-second wall bound. Fixtures are retained in /tmp.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

const ROBOT_SCHEMA: &str = concat!(
    r##"{"v":1,"event":"schema","events":{"invocation":["v","event"],"columns":["v","event","columns"],"row":["v","event","cells"],"progress":["v","event","rows","seq"],"result":["v","event","kind","seq","count","statements"],"error":["v","event","class","diagnostics"],"schema":["v","event","events","exit_codes","key_file","bindings","cell_types","result_kinds"]},"exit_codes":{"success":0,"usage":2,"query":3,"open":4,"io":5},"key_file":"Three nonempty lines of 64 hexadecimal characters: object-id key, security namespace, encryption key; # starts a comment. Keys are never printed.","bindings":"Repeat --label name=u32, --relation name=u32, --property name=u32 on each invocation; --write-relation u32 defaults to 1. No implicit catalog.","cell_types":["null","bool","int","text","list","count","wideint","average","decimal","float","timestamp","bytes","vertex","edge","path","vertices","edges"],"result_kinds":["created","written","rows","replayed","help","schema","loaded"]}"##,
    "\n",
);

/// Exit codes the frozen contract admits; anything else aborts the campaign.
const FROZEN_CODES: [i32; 5] = [0, 2, 3, 4, 5];
use std::path::PathBuf;
#[derive(Debug, PartialEq, Eq)]
enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    fn object(&self) -> &BTreeMap<String, Json> {
        match self {
            Self::Object(value) => value,
            _ => Option::<&BTreeMap<String, Json>>::None
                .expect("expected JSON object at this access site"),
        }
    }

    fn array(&self) -> &[Json] {
        match self {
            Self::Array(value) => value,
            _ => Option::<&Vec<Json>>::None.expect("expected JSON array at this access site"),
        }
    }

    fn string(&self) -> &str {
        match self {
            Self::String(value) => value,
            _ => Option::<&String>::None.expect("expected JSON string at this access site"),
        }
    }

    fn unsigned(&self) -> u64 {
        let text = match self {
            Self::Number(value) => value,
            _ => Option::<&String>::None.expect("expected JSON number at this access site"),
        };
        text.parse().expect("unsigned integer JSON number")
    }

    fn get(&self, name: &str) -> &Json {
        match self.object().get(name) {
            Some(value) => value,
            None => Option::<&Json>::None.expect(&format!("missing {name:?} in JSON object")),
        }
    }
}

struct JsonParser<'a> {
    input: &'a str,
    offset: usize,
}

type ParseResult<T> = Result<T, String>;

impl<'a> JsonParser<'a> {
    fn parse(input: &'a str) -> ParseResult<Json> {
        let mut parser = Self { input, offset: 0 };
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

    fn expect(&mut self, byte: u8) -> ParseResult<()> {
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

    fn value(&mut self) -> ParseResult<Json> {
        self.whitespace();
        match self.peek() {
            Some(b'"') => self.string().map(Json::String),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("expected JSON value at {}", self.offset)),
        }
    }

    fn literal(&mut self, text: &str, value: Json) -> ParseResult<Json> {
        if !self.input[self.offset..].starts_with(text) {
            return Err(format!("invalid JSON literal at {}", self.offset));
        }
        self.offset += text.len();
        Ok(value)
    }

    fn object(&mut self) -> ParseResult<Json> {
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

    fn array(&mut self) -> ParseResult<Json> {
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

    fn hex_quad(&mut self) -> ParseResult<u32> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = self
                .peek()
                .and_then(|byte| char::from(byte).to_digit(16))
                .ok_or_else(|| format!("invalid Unicode escape at {}", self.offset))?;
            self.offset += 1;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn string(&mut self) -> ParseResult<String> {
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
                    let character = self.input[self.offset..].chars().next().unwrap();
                    self.offset += character.len_utf8();
                    text.push(character);
                }
            }
        }
    }

    fn digits(&mut self) -> ParseResult<()> {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        if self.offset == start {
            Err(format!("expected digit at {}", start))
        } else {
            Ok(())
        }
    }

    fn number(&mut self) -> ParseResult<Json> {
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

fn json(input: &str) -> Json {
    JsonParser::parse(input).unwrap_or_else(|error| fail(&format!("{error}: {input:?}")))
}

fn fail<T>(message: &str) -> T {
    Option::<T>::None.expect(message)
}

fn exact_fields(value: &Json, fields: &[&str]) {
    let mut expected = fields.to_vec();
    expected.sort_unstable();
    assert_eq!(
        value
            .object()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        expected
    );
}

fn check_cell(cell: &Json, schema: &Json) {
    let kind = cell.get("type").string();
    assert!(
        schema
            .get("cell_types")
            .array()
            .iter()
            .any(|value| value.string() == kind)
    );
    if kind == "null" {
        exact_fields(cell, &["type"]);
        return;
    }
    exact_fields(cell, &["type", "value"]);
    let value = cell.get("value");
    match kind {
        "bool" => assert!(matches!(value, Json::Bool(_))),
        "text" => {
            value.string();
        }
        "list" => {
            for item in value.array() {
                check_cell(item, schema);
            }
        }
        "int" => assert_eq!(
            value.string().parse::<i64>().unwrap().to_string(),
            value.string()
        ),
        "count" => {
            value.string().parse::<u64>().unwrap();
        }
        "wideint" => {
            value.string().parse::<i128>().unwrap();
        }
        "average" | "decimal" | "float" | "bytes" | "vertex" | "edge" => {
            value.string();
        }
        "timestamp" => {
            exact_fields(value, &["instant_utc_nanos", "utc_offset_seconds", "zone"]);
        }
        "vertices" | "edges" => {
            for id in value.array() {
                id.string().parse::<u128>().unwrap();
            }
        }
        "path" => {
            exact_fields(value, &["nodes", "edges"]);
            for field in ["nodes", "edges"] {
                for id in value.get(field).array() {
                    id.string().parse::<u128>().unwrap();
                }
            }
        }
        kind => fail(&format!("unknown cell type {kind}")),
    }
}

/// The frozen robot contract for ONE process invocation.
fn check_robot_events(stdout: &str, code: i32, schema: &Json) -> Vec<Json> {
    assert!(
        stdout.ends_with('\n'),
        "NDJSON must end with newline: {stdout:?}"
    );
    let events: Vec<_> = stdout
        .strip_suffix('\n')
        .unwrap()
        .split('\n')
        .map(json)
        .collect();
    assert!(events.len() >= 2, "invocation and terminal required");
    assert_eq!(events[0].get("event").string(), "invocation");
    let mut terminals = 0;
    let mut columns = None;
    let mut rows = 0;
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.get("v").unsigned(), 1);
        let name = event.get("event").string();
        assert!(
            schema.get("events").object().contains_key(name),
            "event {name:?} not in frozen schema"
        );
        let allowed = schema.get("events").get(name).array();
        for field in event.object().keys() {
            assert!(
                allowed.iter().any(|value| value.string() == field),
                "unlisted {name}.{field}"
            );
        }
        match name {
            "invocation" => {
                assert_eq!(index, 0, "exactly one initial invocation");
                exact_fields(event, &["v", "event"]);
            }
            "columns" => {
                assert_eq!(index, 1);
                exact_fields(event, &["v", "event", "columns"]);
                let names = event.get("columns").array();
                for name in names {
                    name.string();
                }
                columns = Some(names.len());
            }
            "row" => {
                exact_fields(event, &["v", "event", "cells"]);
                let cells = event.get("cells").array();
                assert_eq!(Some(cells.len()), columns);
                for cell in cells {
                    check_cell(cell, schema);
                }
                rows += 1;
            }
            "progress" => {
                exact_fields(event, &["v", "event", "rows", "seq"]);
                event.get("rows").unsigned();
                event.get("seq").unsigned();
            }
            "schema" => {
                assert_eq!(index, 1);
                assert_eq!(event, schema);
            }
            "result" => {
                terminals += 1;
                assert_eq!(index, events.len() - 1, "terminal must be last");
                assert_eq!(code, 0, "success terminal requires exit code 0");
                match event.get("kind").string() {
                    "rows" => {
                        exact_fields(event, &["v", "event", "kind", "seq", "count"]);
                        assert!(columns.is_some());
                        assert_eq!(event.get("count").unsigned(), rows);
                        event.get("seq").unsigned();
                    }
                    "loaded" => {
                        exact_fields(event, &["v", "event", "kind", "seq", "count", "statements"]);
                        event.get("seq").unsigned();
                        event.get("count").unsigned();
                        event.get("statements").unsigned();
                    }
                    "written" => {
                        exact_fields(event, &["v", "event", "kind", "seq", "statements"]);
                        assert_eq!(index, 1);
                        event.get("seq").unsigned();
                        event.get("statements").unsigned();
                    }
                    "created" => {
                        exact_fields(event, &["v", "event", "kind", "seq"]);
                        assert_eq!(index, 1);
                        event.get("seq").unsigned();
                    }
                    "help" => {
                        exact_fields(event, &["v", "event", "kind"]);
                        assert_eq!(index, 1);
                    }
                    "schema" => {
                        exact_fields(event, &["v", "event", "kind"]);
                        assert_eq!(index, 2);
                        assert_eq!(&events[1], schema);
                    }
                    other => fail(&format!("unknown result kind {other}")),
                }
            }
            "error" => {
                terminals += 1;
                assert_eq!(index, events.len() - 1, "terminal must be last");
                exact_fields(event, &["v", "event", "class", "diagnostics"]);
                let class = event.get("class").string();
                assert_ne!(class, "success");
                assert_eq!(
                    code as u64,
                    schema.get("exit_codes").get(class).unsigned(),
                    "error.class must match the exit code"
                );
                assert!(!event.get("diagnostics").array().is_empty());
                for diagnostic in event.get("diagnostics").array() {
                    assert!(!diagnostic.string().trim().is_empty());
                }
            }
            _ => fail(&format!("unhandled schema event {name}")),
        }
    }
    assert_eq!(terminals, 1, "exactly one terminal event");
    events
}

struct Outcome {
    code: i32,
    stdout: String,
    stderr: String,
    #[allow(dead_code)]
    events: Vec<Json>,
}

const PANIC_MARKERS: [&str; 3] = ["panicked at", "RUST_BACKTRACE", "stack overflow"];

/// One process invocation with the campaign's contract assertions.
/// `keys`: hex material that must never leak onto stdout/stderr.
fn invoke(args: &[String], robot: bool, schema: &Json, secrets: &[String]) -> Outcome {
    let started = Instant::now();
    let binary =
        std::env::var("CLI_FUZZ_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_fgdb").to_owned());
    let mut command = Command::new(binary);
    if robot {
        command.arg("--robot");
    }
    command.args(args);
    let output = command.output().expect("spawn built fgdb binary");
    let elapsed = started.elapsed();
    assert!(
        elapsed <= Duration::from_secs(10),
        "single invocation exceeded wall bound: {elapsed:?} for {args:?}"
    );
    let code = output
        .status
        .code()
        .expect("fgdb must exit by code, never by signal (panic/abort)");
    assert!(
        FROZEN_CODES.contains(&code),
        "exit code {code} outside frozen table for {args:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
    for marker in PANIC_MARKERS {
        assert!(
            !stdout.contains(marker) && !stderr.contains(marker),
            "panic text leaked for {args:?}: stdout={stdout:?} stderr={stderr:?}"
        );
    }
    for secret in secrets {
        let needle = secret.to_ascii_lowercase();
        assert!(
            !stdout.to_ascii_lowercase().contains(&needle),
            "key material leaked on stdout for {args:?}"
        );
        assert!(
            !stderr.to_ascii_lowercase().contains(&needle),
            "key material leaked on stderr for {args:?}"
        );
    }
    let events = if robot {
        check_robot_events(&stdout, code, schema)
    } else {
        // Human mode: no NDJSON event lines may appear on stdout.
        for line in stdout.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('{') {
                if JsonParser::parse(trimmed).is_ok_and(
                    |value| matches!(value, Json::Object(map) if map.contains_key("event")),
                ) {
                    fail::<()>(&format!("NDJSON event on human stdout: {trimmed:?}"));
                }
            }
        }
        Vec::new()
    };
    Outcome {
        code,
        stdout,
        stderr,
        events,
    }
}

// ---------------------------------------------------------------------------
// Seeded generator
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut x = self.0;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Families of the campaign; each contributes successes AND typed failures.
#[derive(Clone, Copy, Debug)]
enum Family {
    Argv,
    Params,
    Bindings,
    KeyFile,
    DbPath,
    GqlText,
}

const FAMILIES: [Family; 6] = [
    Family::Argv,
    Family::Params,
    Family::Bindings,
    Family::KeyFile,
    Family::DbPath,
    Family::GqlText,
];

struct FamilyCounts {
    success: usize,
    refused: usize,
}

struct Workspace {
    root: PathBuf,
    db: String,
    key: String,
    secrets: Vec<String>,
}

impl Workspace {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("fgdb-u6ba-{}-{unique}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let secrets = ["5a", "77", "3c", "ab"].map(|s| s.repeat(32)).to_vec();
        let good = format!("{}\n{}\n{}\n", secrets[0], secrets[1], secrets[2]);
        let variants = [
            good.clone(),
            format!(
                "# keys\n{} # object\n\n{}\n{}\n",
                secrets[0], secrets[1], secrets[2]
            ),
            // CRLF succeeds (Rust lines() strips \r); four lines refuse.
            format!("{}\r\n{}\r\n{}\r\n", secrets[0], secrets[1], secrets[2]),
            String::new(),
            format!("{}\n", secrets[0]),
            format!("{good}{}\n", secrets[3]),
            format!("{}z\n{}\n{}\n", &secrets[0][..63], secrets[1], secrets[2]),
            // Valid keys under a DIFFERENT security namespace: a legitimate
            // key file for another database; doubles as the foreign-db key.
            format!("{}\n{}\n{}\n", secrets[0], secrets[1], secrets[3]),
        ];
        for (index, contents) in variants.iter().enumerate() {
            std::fs::write(root.join(format!("key-{index}")), contents).unwrap();
        }
        std::fs::create_dir(root.join("key-dir")).unwrap();
        std::fs::create_dir(root.join("empty-db")).unwrap();
        std::fs::write(root.join("plain-file"), b"not a database").unwrap();
        Self {
            db: root.join("db").to_string_lossy().into_owned(),
            key: root.join("key-0").to_string_lossy().into_owned(),
            root,
            secrets,
        }
    }

    fn args(&self, command: &str, text: Option<&str>) -> Vec<String> {
        let mut args = vec![
            command.into(),
            "--db".into(),
            self.db.clone(),
            "--key-file".into(),
            self.key.clone(),
        ];
        if let Some(text) = text {
            args.push(text.into());
        }
        args
    }
}

/// OS argv cannot contain NUL. Byte mutations exclude NUL but may split UTF-8;
/// lossy decoding then makes a valid OS argument without hiding parser errors.
fn mutate(rng: &mut Rng, text: String) -> String {
    let mut bytes = text.into_bytes();
    let at = rng.below(bytes.len() + 1);
    match rng.below(4) {
        0 => bytes.truncate(at),
        1 => bytes.insert(at, *rng.pick(b"'()[];\\\n\xff")),
        2 if at < bytes.len() => {
            bytes[at] = 1 + rng.below(255) as u8;
        }
        _ => bytes.extend_from_slice(b" RETURN )"),
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn generated_case(
    ws: &Workspace,
    rng: &mut Rng,
    family: Family,
    seed: usize,
    index: usize,
) -> (Vec<String>, Option<bool>) {
    let mut args = ws.args("query", Some("MATCH (n) RETURN n"));
    match family {
        Family::Argv => {
            match index % 8 {
                0 => return (vec!["help".into()], Some(true)),
                1 => {
                    // Permute complete flag/value pairs around the text;
                    // the second half exercises write + --write-relation.
                    if index % 16 < 8 {
                        args = vec![
                            "query".into(),
                            "MATCH (n) RETURN n".into(),
                            "--key-file".into(),
                            ws.key.clone(),
                            "--db".into(),
                            ws.db.clone(),
                        ];
                    } else {
                        args = vec![
                            "write".into(),
                            "INSERT (n)".into(),
                            "--key-file".into(),
                            ws.key.clone(),
                            "--db".into(),
                            ws.db.clone(),
                            "--write-relation".into(),
                            "1".into(),
                        ];
                    }
                    return (args, Some(true));
                }
                2 => args.extend(["--db".into(), ws.db.clone()]),
                3 => args.push("--key-file".into()),
                4 => {
                    if index % 16 < 8 {
                        args.extend(["--unknown-fuzz".into(), format!("value{}", rng.next())]);
                    } else {
                        args[0] = "write".into();
                        args[5] = "INSERT (n)".into();
                        args.extend(["--write-relation".into(), "abc".into()]);
                    }
                }
                5 => args[0] = format!("query{}", rng.next()),
                6 => args.extend(["--param".into(), "\u{7f}\u{feff}\tλ".into()]),
                _ => {
                    args.swap(0, 1);
                }
            }
            (args, Some(false))
        }
        Family::Params => {
            let (value, text) = match index % 12 {
                0 => (
                    format!("int:{}", rng.below(5)),
                    "MATCH (n) WHERE n.p = $p RETURN n",
                ),
                1 => (
                    format!("uint:{}", rng.below(5)),
                    "MATCH (n) RETURN n LIMIT $p",
                ),
                2 => (
                    format!("text:quote\" slash\\ newline\nλ{}", rng.below(32)),
                    "MATCH (n) RETURN $p AS value",
                ),
                3 => ("bool:true".into(), "MATCH (n) RETURN $p AS value"),
                4 => ("bool:false".into(), "MATCH (n) RETURN $p AS value"),
                5 => ("null".into(), "MATCH (n) RETURN $p AS value"),
                6 => ("int:9223372036854775808".into(), "MATCH (n) RETURN n"),
                7 => ("uint:-1".into(), "MATCH (n) RETURN n"),
                8 => ("int:\\uXYZ".into(), "MATCH (n) RETURN n"),
                9 => (
                    format!("text:{}", "x".repeat(8192 + rng.below(100))),
                    "MATCH (n) RETURN $p AS value",
                ),
                10 => ("bool:TRUE".into(), "MATCH (n) RETURN n"),
                _ => (format!("invalid:{}", rng.next()), "MATCH (n) RETURN n"),
            };
            args = ws.args("query", Some(text));
            args.extend(["--param".into(), format!("p={value}")]);
            args.extend(["--property".into(), "p=1".into()]);
            (
                args,
                if index % 12 < 2 {
                    Some(true)
                } else if matches!(index % 12, 6..=8 | 10..=11) {
                    Some(false)
                } else {
                    None
                },
            )
        }
        Family::Bindings => {
            let kind = ["--label", "--relation", "--property"][(index / 5) % 3];
            let id = 1 + rng.below(1000);
            let name = format!("symbol{}", rng.below(1000));
            args.extend([kind.into(), format!("{name}={id}")]);
            match index % 5 {
                0 => return (args, Some(true)),
                1 => args.extend([kind.into(), format!("{name}={}", id + 1)]),
                2 => args.extend([kind.into(), format!("other={id}")]),
                3 => args.extend([kind.into(), "bad=4294967296".into()]),
                _ => args.extend([kind.into(), "bad=-1".into()]),
            }
            (args, Some(false))
        }
        Family::KeyFile => {
            let variant = index % 10;
            let path = match variant {
                8 => ws.root.join("missing-key"),
                9 => ws.root.join("key-dir"),
                _ => ws.root.join(format!("key-{variant}")),
            };
            // A successful `create` consumes the path; use a fresh one per
            // case so key-0..key-2 (the success cases) never collide.
            // Key-file validity is observable through `create` on a fresh
            // db path (query would first refuse the absent database).
            (
                vec![
                    "create".into(),
                    "--db".into(),
                    ws.root
                        .join(format!("db-key{variant}-{seed}-{index}"))
                        .to_string_lossy()
                        .into_owned(),
                    "--key-file".into(),
                    path.to_string_lossy().into_owned(),
                ],
                Some(variant < 3 || variant == 7),
            )
        }
        Family::DbPath => {
            let variant = index % 5;
            if variant != 0 {
                args[2] = ws
                    .root
                    .join(["db", "missing-db", "plain-file", "empty-db", "foreign-db"][variant])
                    .to_string_lossy()
                    .into_owned();
            }
            (args, Some(variant == 0))
        }
        Family::GqlText => {
            let alias = format!("n{}", rng.below(1000));
            let text = match index % 6 {
                0 => format!("MATCH ({alias}) RETURN {alias}"),
                1 => format!("MATCH ({alias}) RETURN {alias} LIMIT {}", rng.below(4)),
                2 => format!("MATCH ({alias}) RETURN count({alias}) AS total"),
                3 => format!("MATCH ({alias}) WITH {alias} AS x RETURN x"),
                4 => format!("MATCH ({alias}) OPTIONAL MATCH ({alias})-[:R]->(m) RETURN {alias},m"),
                _ => format!(
                    "MATCH ({alias}) RETURN [{}, NULL, TRUE] AS values",
                    rng.below(100)
                ),
            };
            let expectation = match index % 3 {
                0 => {
                    args[5] = text;
                    Some(true)
                }
                1 => {
                    args[5] = format!("{text} ) INVALID");
                    Some(false)
                }
                _ => {
                    args[5] = mutate(rng, text);
                    None
                }
            };
            args.extend(["--relation".into(), "R=1".into()]);
            (args, expectation)
        }
    }
}

#[test]
fn cli_fuzz_campaign_keeps_robot_contract() {
    // Knobs increase exploration only; never silently bypass the acceptance floor.
    let seeds: usize =
        std::env::var("CLI_FUZZ_SEEDS").map_or(3, |s| s.parse().expect("seed count"));
    let iterations: usize =
        std::env::var("CLI_FUZZ_ITERS").map_or(86, |s| s.parse().expect("iteration count"));
    assert!(seeds >= 3 && iterations >= 86);
    let schema = json(ROBOT_SCHEMA);
    let started = Instant::now();
    let ws = Workspace::new();
    assert_eq!(
        invoke(&ws.args("create", None), true, &schema, &ws.secrets).code,
        0
    );
    assert_eq!(
        invoke(
            &{
                let mut args = ws.args("write", Some("INSERT (n {p:1})"));
                args.extend(["--property".to_owned(), "p=1".to_owned()]);
                args
            },
            true,
            &schema,
            &ws.secrets
        )
        .code,
        0
    );
    // Real foreign database: created with a different key (key-7), then
    // opened with our key in the DbPath family -> typed `open` refusal.
    assert_eq!(
        invoke(
            &[
                "create".to_owned(),
                "--db".to_owned(),
                ws.root.join("foreign-db").to_string_lossy().into_owned(),
                "--key-file".to_owned(),
                ws.root.join("key-7").to_string_lossy().into_owned(),
            ],
            true,
            &schema,
            &ws.secrets
        )
        .code,
        0
    );
    let mut counts: [[FamilyCounts; 2]; 6] = std::array::from_fn(|_| {
        std::array::from_fn(|_| FamilyCounts {
            success: 0,
            refused: 0,
        })
    });
    let mut total = 0;
    for seed in 0..seeds {
        let mut rng = Rng::new(0x6ba1 + seed as u64);
        for (slot, family) in FAMILIES.into_iter().enumerate() {
            for index in 0..iterations {
                let robot = (index / 12) % 2 == 0;
                let (args, expected) = generated_case(&ws, &mut rng, family, seed, index);
                let outcome = invoke(&args, robot, &schema, &ws.secrets);
                total += 1;
                let count = &mut counts[slot][usize::from(robot)];
                if outcome.code == 0 {
                    count.success += 1;
                } else {
                    count.refused += 1;
                }
                if let Some(expected) = expected {
                    assert_eq!(
                        outcome.code == 0,
                        expected,
                        "seed={seed} family={family:?} index={index} args={args:?} stdout={} stderr={}",
                        outcome.stdout,
                        outcome.stderr
                    );
                }
            }
        }
    }
    assert!(total >= 1500, "executed {total}");
    for (family, modes) in FAMILIES.into_iter().zip(counts) {
        for (mode, count) in modes.into_iter().enumerate() {
            eprintln!(
                "{family:?} robot={}: success={} refused={}",
                mode == 1,
                count.success,
                count.refused
            );
            assert!(
                count.success >= 6 && count.refused >= 6,
                "{family:?} mode={mode} anti-vacuity floor"
            );
        }
    }
    eprintln!(
        "campaign {total} invocations, {seeds} seeds, elapsed={:?}; fixtures={}",
        started.elapsed(),
        ws.root.display()
    );
    assert!(
        started.elapsed() < Duration::from_secs(180),
        "campaign wall budget"
    );
}
