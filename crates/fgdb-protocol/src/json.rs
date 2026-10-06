//! JSON for the surfaces that speak it: a strict, dependency-free, bounded
//! parser, and the one cell encoding every JSON surface uses for wire values.
//!
//! The grammar is RFC 8259 without extensions: no comments, no trailing
//! commas, no duplicate object fields, no unescaped control characters, and
//! surrogate escapes only in valid pairs. Every document is bounded by a value
//! count, a per-token byte limit and a nesting depth of 32 before recursion.
//! Numbers keep their source text, so no integer is rounded through a float.
//!
//! The cell encoding is the CLI robot contract's (`{"type":..., "value":...}`),
//! so a client cannot tell a local `fgdb query` row from a remote one.

use crate::body::{WireTimestamp, WireValue};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// One parsed JSON value. Object fields are ordered by UTF-8 bytes, which is
/// exactly the canonical order of a wire map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}
/// One bounded JSON document: at most `values` values and `token_bytes`
/// bytes per string or number token, nested at most 32 deep.
pub fn parse_json(input: &str, values: usize, token_bytes: usize) -> Result<Json, String> {
    JsonParser::parse_limited(input, values, token_bytes)
}
struct JsonParser<'a> {
    input: &'a str,
    offset: usize,
    depth: usize,
    remaining_values: usize,
    max_token_bytes: usize,
}
impl<'a> JsonParser<'a> {
    fn parse_limited(input: &'a str, values: usize, token_bytes: usize) -> Result<Json, String> {
        let mut parser = Self {
            input,
            offset: 0,
            depth: 0,
            remaining_values: values,
            max_token_bytes: token_bytes,
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
        if self.remaining_values == 0 {
            return Err("JSON value limit exceeded".into());
        }
        self.remaining_values -= 1;
        if self.depth >= 32 {
            return Err("JSON nesting limit exceeded".into());
        }
        self.depth += 1;
        let result = match self.peek() {
            Some(b'"') => self.string().map(Json::String),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
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
                    let ch = match escape {
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
                    };
                    self.push_character(&mut text, ch)?;
                }
                Some(0..=0x1f) => return Err("unescaped control character".into()),
                Some(_) => {
                    let ch = self.input[self.offset..]
                        .chars()
                        .next()
                        .expect("remaining character");
                    self.offset += ch.len_utf8();
                    self.push_character(&mut text, ch)?;
                }
            }
        }
    }
    fn push_character(&self, text: &mut String, ch: char) -> Result<(), String> {
        if text
            .len()
            .checked_add(ch.len_utf8())
            .is_none_or(|n| n > self.max_token_bytes)
        {
            return Err("JSON string limit exceeded".into());
        }
        text.push(ch);
        Ok(())
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
        if self.offset - start > self.max_token_bytes {
            return Err("JSON number limit exceeded".into());
        }
        Ok(Json::Number(self.input[start..self.offset].to_owned()))
    }
}

/// A JSON string literal for `text`, escaping quotes, backslashes and every
/// control character.
#[must_use]
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Shortest round-trip float text; non-finite values are the tokens `NaN`,
/// `Infinity` and `-Infinity`, quoted by the cell encoding.
#[must_use]
pub fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        value.to_string()
    }
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn timestamp(value: &WireTimestamp) -> String {
    let zone = value.zone.as_ref().map_or_else(
        || "null".to_owned(),
        |zone| {
            format!(
                r#"{{"identifier":{},"tzdb_oid":"{}"}}"#,
                quote(&zone.identifier),
                hex(&zone.tzdb_oid)
            )
        },
    );
    format!(
        r#"{{"instant_utc_nanos":"{}","utc_offset_seconds":{},"zone":{zone}}}"#,
        value.instant_utc_nanos, value.utc_offset_seconds
    )
}

/// The robot-contract JSON cell for one wire value. Integers wider than the
/// interoperable JSON range travel as decimal strings, as do identities.
#[must_use]
pub fn cell(value: &WireValue) -> String {
    let ids = |ids: &[u128]| {
        ids.iter()
            .map(|id| quote(&id.to_string()))
            .collect::<Vec<_>>()
            .join(",")
    };
    match value {
        WireValue::Null => r#"{"type":"null"}"#.to_owned(),
        WireValue::Bool(v) => format!(r#"{{"type":"bool","value":{v}}}"#),
        WireValue::Int(v) => format!(r#"{{"type":"int","value":"{v}"}}"#),
        WireValue::Text(v) => format!(r#"{{"type":"text","value":{}}}"#, quote(v)),
        WireValue::Decimal(v) => format!(r#"{{"type":"decimal","value":"{v}"}}"#),
        WireValue::Float(v) => format!(r#"{{"type":"float","value":{}}}"#, quote(&float_text(*v))),
        WireValue::Timestamp(v) => format!(r#"{{"type":"timestamp","value":{}}}"#, timestamp(v)),
        WireValue::Bytes(v) => format!(r#"{{"type":"bytes","value":{}}}"#, quote(&hex(v))),
        WireValue::Vertex(v) => format!(r#"{{"type":"vertex","value":"{v}"}}"#),
        WireValue::Edge(v) => format!(r#"{{"type":"edge","value":"{v}"}}"#),
        WireValue::Path { start, steps } => {
            let mut nodes = vec![quote(&start.to_string())];
            let mut edges = Vec::new();
            for (edge, vertex) in steps {
                edges.push(quote(&edge.to_string()));
                nodes.push(quote(&vertex.to_string()));
            }
            format!(
                r#"{{"type":"path","value":{{"nodes":[{}],"edges":[{}]}}}}"#,
                nodes.join(","),
                edges.join(",")
            )
        }
        WireValue::Vertices(v) => format!(r#"{{"type":"vertices","value":[{}]}}"#, ids(v)),
        WireValue::Edges(v) => format!(r#"{{"type":"edges","value":[{}]}}"#, ids(v)),
        WireValue::List(items) => format!(
            r#"{{"type":"list","value":[{}]}}"#,
            items.iter().map(cell).collect::<Vec<_>>().join(",")
        ),
        WireValue::Map(entries) => format!(
            r#"{{"type":"map","value":{{{}}}}}"#,
            entries
                .iter()
                .map(|(key, value)| format!("{}:{}", quote(key), cell(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        WireValue::Count(v) => format!(r#"{{"type":"count","value":"{v}"}}"#),
        WireValue::WideInt(v) => format!(r#"{{"type":"wideint","value":"{v}"}}"#),
        WireValue::Average {
            numerator,
            denominator,
        } => format!(r#"{{"type":"average","value":"{numerator}/{denominator}"}}"#),
    }
}

/// A statement argument from plain JSON: an integer is `int`, any other finite
/// number `float`, a string `text`, `true`/`false` `bool`, `null` null, an
/// array a list and an object a map. A non-finite or out-of-range number is
/// refused, never rounded.
pub fn argument(json: &Json) -> Result<WireValue, String> {
    Ok(match json {
        Json::Null => WireValue::Null,
        Json::Bool(value) => WireValue::Bool(*value),
        Json::Number(text) if text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) => {
            let value: f64 = text.parse().map_err(|_| "invalid number".to_owned())?;
            if !value.is_finite() {
                return Err("float out of range".into());
            }
            WireValue::Float(value)
        }
        Json::Number(text) => WireValue::Int(
            text.parse()
                .map_err(|_| "integer out of range".to_owned())?,
        ),
        Json::String(text) => WireValue::Text(text.clone()),
        Json::Array(items) => {
            WireValue::List(items.iter().map(argument).collect::<Result<_, _>>()?)
        }
        Json::Object(fields) => WireValue::Map(
            fields
                .iter()
                .map(|(key, value)| Ok((key.clone(), argument(value)?)))
                .collect::<Result<_, String>>()?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_parse_strictly_and_within_bounds() {
        let json = parse_json(r#"{"b": [1, 2.5, "x\u00e9"], "a": null}"#, 16, 64).unwrap();
        let Json::Object(fields) = &json else {
            panic!("object")
        };
        assert_eq!(fields.keys().collect::<Vec<_>>(), ["a", "b"]);
        for bad in [
            r#"{"a":1,"a":2}"#,
            "[1,]",
            "01",
            "\"\u{1}\"",
            "[1] 2",
            r#""\ud800""#,
        ] {
            assert!(parse_json(bad, 16, 64).is_err(), "{bad}");
        }
        assert!(
            parse_json("[1,2,3]", 3, 64).is_err(),
            "four values exceed three"
        );
        assert!(
            parse_json(&"[".repeat(40), 1000, 64).is_err(),
            "depth bound"
        );
    }

    #[test]
    fn arguments_and_cells_round_the_value_lattice() {
        let json = parse_json(r#"[{"name":"Ann","age":30},1e3,true]"#, 32, 64).unwrap();
        assert_eq!(
            argument(&json).unwrap(),
            WireValue::List(vec![
                WireValue::Map(vec![
                    ("age".into(), WireValue::Int(30)),
                    ("name".into(), WireValue::Text("Ann".into())),
                ]),
                WireValue::Float(1000.0),
                WireValue::Bool(true),
            ])
        );
        assert!(argument(&parse_json("99999999999999999999", 2, 64).unwrap()).is_err());
        assert_eq!(cell(&WireValue::Int(-2)), r#"{"type":"int","value":"-2"}"#);
        assert_eq!(
            cell(&WireValue::Text("a\"b\n".into())),
            r#"{"type":"text","value":"a\"b\n"}"#
        );
        assert_eq!(
            cell(&WireValue::Float(f64::NAN)),
            r#"{"type":"float","value":"NaN"}"#
        );
    }
}
