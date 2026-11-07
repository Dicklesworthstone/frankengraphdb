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
    match parse_json_controlled(input, values, token_bytes, |_| {
        Ok::<(), std::convert::Infallible>(())
    }) {
        Ok(value) => Ok(value),
        Err(JsonParseError::Invalid(message)) => Err(message),
        Err(JsonParseError::Interrupted(never)) => match never {},
    }
}

/// Source bytes consumed between live control checks. Characters and JSON
/// literals remain indivisible, so a check can occur just before this bound.
pub const JSON_PARSE_RUN_BYTES: usize = 64 * 1024;

/// JSON grammar/limit errors retain their ordinary message; interruption is
/// a separate typed outcome and never a malformed document or a partial value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonParseError<C> {
    Invalid(String),
    Interrupted(C),
}

impl<C> From<String> for JsonParseError<C> {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl<C> From<&str> for JsonParseError<C> {
    fn from(message: &str) -> Self {
        Self::Invalid(message.to_owned())
    }
}

/// Parse the same bounded document as [`parse_json`], with live control before
/// parsing, between at most [`JSON_PARSE_RUN_BYTES`] consumed source bytes,
/// and before returning a complete value. The callback receives the consumed
/// byte offset, always at a UTF-8 boundary. It and its error need not be `Send`.
/// The bound covers source scanning; allocation, map operations and cleanup
/// remain subject to the caller's admitted document and token limits.
pub fn parse_json_controlled<C>(
    input: &str,
    values: usize,
    token_bytes: usize,
    control: impl FnMut(usize) -> Result<(), C>,
) -> Result<Json, JsonParseError<C>> {
    JsonParser::parse_limited(input, values, token_bytes, control)
}

struct JsonParser<'a, F> {
    input: &'a str,
    offset: usize,
    depth: usize,
    remaining_values: usize,
    max_token_bytes: usize,
    remaining_bytes: usize,
    control: F,
}
impl<'a, C, F: FnMut(usize) -> Result<(), C>> JsonParser<'a, F> {
    fn parse_limited(
        input: &'a str,
        values: usize,
        token_bytes: usize,
        control: F,
    ) -> Result<Json, JsonParseError<C>> {
        let mut parser = Self {
            input,
            offset: 0,
            depth: 0,
            remaining_values: values,
            max_token_bytes: token_bytes,
            remaining_bytes: JSON_PARSE_RUN_BYTES,
            control,
        };
        parser.checkpoint()?;
        let value = parser.value()?;
        parser.whitespace()?;
        if parser.offset != input.len() {
            return Err(format!("trailing JSON bytes at {}", parser.offset).into());
        }
        parser.checkpoint()?;
        Ok(value)
    }
    fn checkpoint(&mut self) -> Result<(), JsonParseError<C>> {
        (self.control)(self.offset).map_err(JsonParseError::Interrupted)?;
        self.remaining_bytes = JSON_PARSE_RUN_BYTES;
        Ok(())
    }
    fn advance(&mut self, bytes: usize) -> Result<(), JsonParseError<C>> {
        // Each caller consumes at most one UTF-8 character, one ASCII byte,
        // or a five-byte literal. Admit the next piece before consuming it.
        if bytes > self.remaining_bytes {
            self.checkpoint()?;
        }
        self.offset += bytes;
        self.remaining_bytes -= bytes;
        Ok(())
    }
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.offset).copied()
    }
    fn consume(&mut self, byte: u8) -> Result<bool, JsonParseError<C>> {
        if self.peek() == Some(byte) {
            self.advance(1)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn expect(&mut self, byte: u8) -> Result<(), JsonParseError<C>> {
        if self.consume(byte)? {
            Ok(())
        } else {
            Err(format!("expected {:?} at {}", char::from(byte), self.offset).into())
        }
    }
    fn whitespace(&mut self) -> Result<(), JsonParseError<C>> {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.advance(1)?;
        }
        Ok(())
    }
    fn value(&mut self) -> Result<Json, JsonParseError<C>> {
        self.whitespace()?;
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
            _ => Err(format!("expected JSON value at {}", self.offset).into()),
        };
        self.depth -= 1;
        result
    }
    fn literal(&mut self, text: &str, value: Json) -> Result<Json, JsonParseError<C>> {
        if !self.input[self.offset..].starts_with(text) {
            return Err("invalid JSON literal".into());
        }
        self.advance(text.len())?;
        Ok(value)
    }
    fn object(&mut self) -> Result<Json, JsonParseError<C>> {
        self.expect(b'{')?;
        self.whitespace()?;
        let mut fields = BTreeMap::new();
        if self.consume(b'}')? {
            return Ok(Json::Object(fields));
        }
        loop {
            self.whitespace()?;
            let name = self.string()?;
            self.whitespace()?;
            self.expect(b':')?;
            let value = self.value()?;
            if fields.insert(name, value).is_some() {
                return Err("duplicate JSON object field".into());
            }
            self.whitespace()?;
            if self.consume(b'}')? {
                return Ok(Json::Object(fields));
            }
            self.expect(b',')?;
        }
    }
    fn array(&mut self) -> Result<Json, JsonParseError<C>> {
        self.expect(b'[')?;
        self.whitespace()?;
        let mut values = Vec::new();
        if self.consume(b']')? {
            return Ok(Json::Array(values));
        }
        loop {
            values.push(self.value()?);
            self.whitespace()?;
            if self.consume(b']')? {
                return Ok(Json::Array(values));
            }
            self.expect(b',')?;
        }
    }
    fn hex_quad(&mut self) -> Result<u32, JsonParseError<C>> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = self
                .peek()
                .and_then(|byte| char::from(byte).to_digit(16))
                .ok_or("invalid Unicode escape")?;
            self.advance(1)?;
            value = value * 16 + digit;
        }
        Ok(value)
    }
    fn string(&mut self) -> Result<String, JsonParseError<C>> {
        self.expect(b'"')?;
        let mut text = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated JSON string".into()),
                Some(b'"') => {
                    self.advance(1)?;
                    return Ok(text);
                }
                Some(b'\\') => {
                    self.advance(1)?;
                    let escape = self.peek().ok_or("unterminated JSON escape")?;
                    self.advance(1)?;
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
                    self.advance(ch.len_utf8())?;
                    self.push_character(&mut text, ch)?;
                }
            }
        }
    }
    fn push_character(&self, text: &mut String, ch: char) -> Result<(), JsonParseError<C>> {
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
    fn digits(&mut self) -> Result<(), JsonParseError<C>> {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.advance(1)?;
        }
        if self.offset == start {
            Err("expected digit".into())
        } else {
            Ok(())
        }
    }
    fn number(&mut self) -> Result<Json, JsonParseError<C>> {
        let start = self.offset;
        self.consume(b'-')?;
        if !self.consume(b'0')? {
            self.digits()?;
        }
        if self.consume(b'.')? {
            self.digits()?;
        }
        if self.consume(b'e')? || self.consume(b'E')? {
            if !self.consume(b'+')? {
                self.consume(b'-')?;
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
        WireValue::Decimal(v) => format!(r#"{{"type":"decimal","value":{}}}"#, quote(v)),
        WireValue::Date(v) => format!(r#"{{"type":"date","value":{}}}"#, quote(v)),
        WireValue::LocalTime(v) => format!(r#"{{"type":"localtime","value":{}}}"#, quote(v)),
        WireValue::Time(v) => format!(r#"{{"type":"time","value":{}}}"#, quote(v)),
        WireValue::LocalDateTime(v) => {
            format!(r#"{{"type":"localdatetime","value":{}}}"#, quote(v))
        }
        WireValue::Duration(v) => format!(r#"{{"type":"duration","value":{}}}"#, quote(v)),
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
///
/// `{"$decimal":"<canonical fixed-scale text>"}` explicitly selects a decimal,
/// including inside lists and maps. Its string is preserved exactly, not
/// parsed as a JSON number. This framing layer checks the wire byte bound;
/// the native parameter binder owns decimal scale, range and canonicality.
/// Ordinary JSON numbers and strings keep their existing float/text meanings.
pub fn argument(json: &Json) -> Result<WireValue, String> {
    // JSON has no byte strings. A one-key object whose key starts with `$`
    // (never a property name) spells one: `{"$bytes": "<hex>"}` exactly, or
    // `{"$vector": [numbers]}` as packed little-endian f32 values, each
    // rounded to the nearest f32 and required to stay finite: the stored
    // form of an embedding that `hybrid.search(vector_property => ...)` reads.
    // `{"$uint64": "<decimal>"}` retains the unsigned domain used by temporal
    // selectors and row windows. Ordinary JSON integers keep their signed type.
    if let Json::Object(fields) = json
        && fields.len() == 1
        && let Some((key, value)) = fields.iter().next()
        && key.starts_with('$')
    {
        return match (key.as_str(), value) {
            ("$bytes", Json::String(hex)) => bytes_from_hex(hex).map(WireValue::Bytes),
            ("$uint64", Json::String(text)) => {
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err("a $uint64 holds unsigned decimal text".into());
                }
                text.parse::<u64>()
                    .map(WireValue::Count)
                    .map_err(|_| "a $uint64 is out of range".into())
            }
            ("$uint64", _) => Err("a $uint64 holds unsigned decimal text".into()),
            ("$decimal", Json::String(text)) => {
                if text.len() > crate::body::MAX_SCALAR_BYTES {
                    return Err("a $decimal exceeds the wire scalar byte limit".into());
                }
                Ok(WireValue::Decimal(text.clone()))
            }
            ("$decimal", _) => Err("a $decimal holds canonical fixed-scale text".into()),
            ("$vector", Json::Array(items)) => {
                let mut bytes = Vec::with_capacity(items.len() * 4);
                for item in items {
                    let Json::Number(text) = item else {
                        return Err("a $vector holds numbers".into());
                    };
                    let value: f64 = text.parse().map_err(|_| "invalid number".to_owned())?;
                    let narrowed = value as f32;
                    if !narrowed.is_finite() {
                        return Err("a $vector coordinate is out of f32 range".into());
                    }
                    bytes.extend_from_slice(&narrowed.to_le_bytes());
                }
                Ok(WireValue::Bytes(bytes))
            }
            _ => Err(format!(
                "unknown typed argument {key:?}; use $bytes, $vector, $decimal or $uint64"
            )),
        };
    }
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

/// Bytes from even-length hexadecimal text.
pub fn bytes_from_hex(hex: &str) -> Result<Vec<u8>, String> {
    match bytes_from_hex_controlled(hex, |_| Ok::<(), std::convert::Infallible>(())) {
        Ok(bytes) => Ok(bytes),
        Err(JsonParseError::Invalid(message)) => Err(message),
        Err(JsonParseError::Interrupted(never)) => match never {},
    }
}

/// Decode the same hexadecimal scalar, admitting each real validation and
/// decoding run before work. A run contains at most JSON_PARSE_RUN_BYTES
/// source bytes. Validation still finishes before allocation/decoding, so an
/// ordinary malformed value has the same error as [`bytes_from_hex`].
pub fn bytes_from_hex_controlled<C>(
    hex: &str,
    mut control: impl FnMut(usize) -> Result<(), C>,
) -> Result<Vec<u8>, JsonParseError<C>> {
    if !hex.len().is_multiple_of(2) {
        return Err("$bytes must be even-length hexadecimal".into());
    }
    for chunk in hex.as_bytes().chunks(JSON_PARSE_RUN_BYTES) {
        control(chunk.len()).map_err(JsonParseError::Interrupted)?;
        if !chunk.iter().all(u8::is_ascii_hexdigit) {
            return Err("$bytes must be even-length hexadecimal".into());
        }
    }
    let digit = |b: u8| {
        if b.is_ascii_digit() {
            b - b'0'
        } else {
            b.to_ascii_lowercase() - b'a' + 10
        }
    };
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.as_bytes().chunks(JSON_PARSE_RUN_BYTES) {
        control(chunk.len()).map_err(JsonParseError::Interrupted)?;
        // Both the run size and the validated total length are even.
        bytes.extend(
            chunk
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (digit(pair[0]) << 4) | digit(pair[1])),
        );
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_arguments_preserve_full_width_and_nested_wire_domains() {
        let parse = |text: &str| argument(&parse_json(text, 32, 64).unwrap());
        for (text, value) in [
            (r#"{"$uint64":"0"}"#, 0),
            (r#"{"$uint64":"9007199254740993"}"#, 9_007_199_254_740_993),
            (r#"{"$uint64":"18446744073709551615"}"#, u64::MAX),
        ] {
            assert_eq!(parse(text), Ok(WireValue::Count(value)));
        }
        assert_eq!(
            parse(r#"[{"$uint64":"1"},{"cut":{"$uint64":"18446744073709551615"}}]"#),
            Ok(WireValue::List(vec![
                WireValue::Count(1),
                WireValue::Map(vec![("cut".into(), WireValue::Count(u64::MAX))]),
            ]))
        );
        assert_eq!(parse("1"), Ok(WireValue::Int(1)));
        assert_eq!(parse("-1"), Ok(WireValue::Int(-1)));
        assert_eq!(parse(r#""1""#), Ok(WireValue::Text("1".into())));
        assert_eq!(
            parse(r#"{"$uint64":"1","ordinary":2}"#),
            Ok(WireValue::Map(vec![
                ("$uint64".into(), WireValue::Text("1".into())),
                ("ordinary".into(), WireValue::Int(2)),
            ]))
        );
    }

    #[test]
    fn unsigned_argument_tags_reject_coercion_malformed_text_and_overflow() {
        let parse = |text: &str| argument(&parse_json(text, 16, 64).unwrap());
        for text in [
            r#"{"$uint64":1}"#,
            r#"{"$uint64":1.0}"#,
            r#"{"$uint64":null}"#,
            r#"{"$uint64":true}"#,
            r#"{"$uint64":[]}"#,
            r#"{"$uint64":{}}"#,
            r#"{"$uint64":""}"#,
            r#"{"$uint64":"-1"}"#,
            r#"{"$uint64":"+1"}"#,
            r#"{"$uint64":"1.0"}"#,
            r#"{"$uint64":"1e3"}"#,
            r#"{"$uint64":" 1"}"#,
            r#"{"$uint64":"1 "}"#,
            r#"{"$uint64":"١"}"#,
            r#"{"$uint64":"18446744073709551616"}"#,
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        assert!(parse("18446744073709551615").is_err());
    }

    #[test]
    fn typed_decimals_preserve_exact_text_at_every_collection_depth() {
        let value = parse_json(
            r#"[{"$decimal":"9007199254740993.000000000000000001"},
                {"amount":{"$decimal":"-0.000000000000000001"}},
                [{"$decimal":"9999999999999999.999999999999999999"}]]"#,
            32,
            64,
        )
        .unwrap();
        assert_eq!(
            argument(&value).unwrap(),
            WireValue::List(vec![
                WireValue::Decimal("9007199254740993.000000000000000001".into()),
                WireValue::Map(vec![(
                    "amount".into(),
                    WireValue::Decimal("-0.000000000000000001".into()),
                )]),
                WireValue::List(vec![WireValue::Decimal(
                    "9999999999999999.999999999999999999".into(),
                )]),
            ])
        );
    }

    #[test]
    fn decimal_tags_require_bounded_text_without_reinterpreting_plain_values() {
        let parse = |text: &str| argument(&parse_json(text, 16, 64).unwrap());
        for text in [
            r#"{"$decimal":1.25}"#,
            r#"{"$decimal":null}"#,
            r#"{"$decimal":true}"#,
            r#"{"$decimal":[]}"#,
            r#"{"$decimal":{}}"#,
        ] {
            assert_eq!(
                parse(text),
                Err("a $decimal holds canonical fixed-scale text".into())
            );
        }
        let oversized = Json::Object(BTreeMap::from([(
            "$decimal".into(),
            Json::String("9".repeat(crate::body::MAX_SCALAR_BYTES + 1)),
        )]));
        assert_eq!(
            argument(&oversized),
            Err("a $decimal exceeds the wire scalar byte limit".into())
        );
        assert_eq!(parse("1.25"), Ok(WireValue::Float(1.25)));
        assert_eq!(parse(r#""1.25""#), Ok(WireValue::Text("1.25".into())));
        assert!(matches!(
            parse(r#"{"$decimal":"1.25","other":0}"#),
            Ok(WireValue::Map(_))
        ));
        // As for binary FGP, decimal canonicality belongs to the native binder.
        // This adapter must not silently pad, round or coerce a supplied scale.
        assert_eq!(
            parse(r#"{"$decimal":"1.25"}"#),
            Ok(WireValue::Decimal("1.25".into()))
        );
    }

    #[test]
    fn decimal_cells_escape_raw_wire_strings_and_preserve_canonical_output() {
        let canonical = "0.000000000000000001";
        assert_eq!(
            cell(&WireValue::Decimal(canonical.into())),
            r#"{"type":"decimal","value":"0.000000000000000001"}"#
        );
        // WireValue is a public transport value, not an admitted native scalar.
        // Even malformed decimal text must remain one safely quoted JSON cell.
        let raw = "\"\\\n\0";
        let encoded = cell(&WireValue::Decimal(raw.into()));
        let Json::Object(fields) = parse_json(&encoded, 8, 64).unwrap() else {
            panic!("one decimal cell object expected");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields.get("type"), Some(&Json::String("decimal".into())));
        assert_eq!(fields.get("value"), Some(&Json::String(raw.into())));
    }

    #[test]
    fn typed_arguments_spell_bytes_and_packed_vectors() {
        let parse = |text: &str| argument(&parse_json(text, 64, 1024).unwrap());
        assert_eq!(
            parse(r#"{"$bytes": "00ff10"}"#),
            Ok(WireValue::Bytes(vec![0, 255, 16]))
        );
        assert_eq!(
            parse(r#"{"$vector": [0.5, -2, 1e-3]}"#),
            Ok(WireValue::Bytes(
                [0.5_f32, -2.0, 1e-3]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect()
            ))
        );
        for refused in [
            r#"{"$bytes": "abc"}"#,
            r#"{"$vector": ["x"]}"#,
            r#"{"$vector": [1e300]}"#,
            r#"{"$other": 1}"#,
        ] {
            assert!(parse(refused).is_err(), "{refused}");
        }
        // Ordinary maps, including ones with a `$` key among others, stay maps.
        assert!(matches!(
            parse(r#"{"$bytes": "00", "x": 1}"#),
            Ok(WireValue::Map(_))
        ));
    }

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
        // The temporal values are the CLI's cells of the same names (fgdb-txbg9).
        assert_eq!(
            cell(&WireValue::Date("-0001-07-21".into())),
            r#"{"type":"date","value":"-0001-07-21"}"#
        );
        assert_eq!(
            cell(&WireValue::LocalTime("21:40:32.142".into())),
            r#"{"type":"localtime","value":"21:40:32.142"}"#
        );
        assert_eq!(
            cell(&WireValue::Time("21:40-01:30".into())),
            r#"{"type":"time","value":"21:40-01:30"}"#
        );
        assert_eq!(
            cell(&WireValue::LocalDateTime("2015-07-21T21:40".into())),
            r#"{"type":"localdatetime","value":"2015-07-21T21:40"}"#
        );
        assert_eq!(
            cell(&WireValue::Duration("PT-1.999S".into())),
            r#"{"type":"duration","value":"PT-1.999S"}"#
        );
    }
}

#[cfg(test)]
mod parse_control_tests {
    use super::*;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::rc::Rc;

    #[test]
    fn controlled_json_preserves_values_and_existing_error_messages() {
        for (input, values, bytes, expected) in [
            ("null", 1, 0, Json::Null),
            ("[]", 1, 0, Json::Array(Vec::new())),
            (r#""é""#, 1, 2, Json::String("é".into())),
            (r#""\uD834\uDD1E""#, 1, 4, Json::String("𝄞".into())),
            ("1.25e+30", 1, 8, Json::Number("1.25e+30".into())),
            (
                r#"{"x":0,"y":false}"#,
                3,
                1,
                Json::Object(BTreeMap::from([
                    ("x".into(), Json::Number("0".into())),
                    ("y".into(), Json::Bool(false)),
                ])),
            ),
        ] {
            assert_eq!(parse_json(input, values, bytes), Ok(expected.clone()));
            assert_eq!(
                parse_json_controlled(input, values, bytes, |_| Ok::<(), Infallible>(())),
                Ok(expected)
            );
        }
        for (input, values, bytes, message) in [
            ("[0]", 1, 10, "JSON value limit exceeded"),
            (r#""é""#, 1, 1, "JSON string limit exceeded"),
            (r#""\uD834\uDD1E""#, 1, 3, "JSON string limit exceeded"),
            ("1.25e+30", 1, 7, "JSON number limit exceeded"),
            (r#"{"x":0,"x":1}"#, 3, 10, "duplicate JSON object field"),
            (r#"{"x":0,"x":}"#, 3, 10, "expected JSON value at 11"),
            ("00", 1, 10, "trailing JSON bytes at 1"),
            ("[true,]", 3, 10, "expected JSON value at 6"),
            (r#""\uD834\u0000""#, 1, 10, "invalid low surrogate"),
            (r#""\uDD1E""#, 1, 10, "invalid Unicode scalar"),
            (r#""\uZZZZ""#, 1, 10, "invalid Unicode escape"),
            (r#""\q""#, 1, 10, "invalid JSON escape"),
            ("-", 1, 10, "expected digit"),
            (" \t\r\n", 1, 10, "expected JSON value at 4"),
        ] {
            assert_eq!(parse_json(input, values, bytes), Err(message.into()));
            assert_eq!(
                parse_json_controlled(input, values, bytes, |_| Ok::<(), Infallible>(())),
                Err(JsonParseError::Invalid(message.into()))
            );
        }
        let too_deep = format!("{}0{}", "[".repeat(32), "]".repeat(32));
        assert_eq!(
            parse_json_controlled(&too_deep, 100, 100, |_| Ok::<(), Infallible>(())),
            Err(JsonParseError::Invalid(
                "JSON nesting limit exceeded".into()
            ))
        );
    }

    #[test]
    fn controlled_json_bounds_source_runs_and_interrupts_every_checkpoint() {
        let run = JSON_PARSE_RUN_BYTES;
        let mut documents = vec![
            format!("{}null", " ".repeat(3 * run + 5)),
            format!("null{}", " \n\t\r".repeat(run)),
            format!("-1.{}e+37", "7".repeat(3 * run + 1)),
            format!("\"{}\"", "x".repeat(1024 * 1024 - 2)),
            format!("\"{}\"", r"\uD834\uDD1E".repeat(run / 3)),
            format!("[{}0]", "0,".repeat(2 * run)),
            format!(r#"{{"{}":0,"tail":true}}"#, "k".repeat(3 * run)),
            format!("null{}!", " ".repeat(3 * run + 3)),
        ];
        // Place each multi-byte scalar and surrogate pair across the first
        // source window at every possible byte alignment around that boundary.
        for padding in 0..12 {
            for text in ["é", "€", "🦀", r"\uD834\uDD1E"] {
                documents.push(format!("\"{}{}tail\"", "a".repeat(run - padding - 1), text));
            }
        }
        for input in documents {
            let expected = parse_json(&input, usize::MAX, input.len());
            let mut offsets = Vec::new();
            let result = parse_json_controlled(&input, usize::MAX, input.len(), |offset| {
                assert!(input.is_char_boundary(offset));
                offsets.push(offset);
                Ok::<(), Infallible>(())
            });
            assert_eq!(
                result,
                expected.map_err(JsonParseError::<Infallible>::Invalid)
            );
            assert_eq!(offsets.first(), Some(&0));
            assert!(offsets.len() >= 2);
            for pair in offsets.windows(2) {
                assert!(pair[1] > pair[0]);
                assert!(pair[1] - pair[0] <= run);
            }
            for stop in 0..offsets.len() {
                let token = Rc::new(Cell::new(usize::MAX));
                let mut observed = Vec::new();
                let interrupted =
                    parse_json_controlled(&input, usize::MAX, input.len(), |offset| {
                        observed.push(offset);
                        if observed.len() == stop + 1 {
                            token.set(offset);
                            Err(Rc::clone(&token))
                        } else {
                            Ok(())
                        }
                    });
                let Err(JsonParseError::Interrupted(error)) = interrupted else {
                    panic!("every visited checkpoint must retain its typed interruption")
                };
                assert!(Rc::ptr_eq(&error, &token));
                assert_eq!(token.get(), offsets[stop]);
                assert_eq!(observed, offsets[..=stop]);
            }
        }
    }

    #[test]
    fn controlled_json_unwinds_inside_a_token_and_next_parse_starts_clean() {
        let input = format!("\"{}\"", "🦀".repeat(JSON_PARSE_RUN_BYTES));
        let stopped = Rc::new(Cell::new(0));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parse_json_controlled(&input, usize::MAX, input.len(), |offset| {
                if offset != 0 {
                    stopped.set(offset);
                    std::panic::resume_unwind(Box::new("inside JSON token"));
                }
                Ok::<(), Infallible>(())
            })
        }));
        assert!(result.is_err());
        assert!(stopped.get() > 0);
        assert!(stopped.get() <= JSON_PARSE_RUN_BYTES);
        assert!(stopped.get() < input.len());
        assert!(matches!(
            parse_json_controlled(&input, usize::MAX, input.len(), |_| Ok::<(), Infallible>(())),
            Ok(Json::String(text)) if text == "🦀".repeat(JSON_PARSE_RUN_BYTES)
        ));
    }
}

#[cfg(test)]
mod controlled_hex_tests {
    use super::*;
    use std::convert::Infallible;
    use std::rc::Rc;

    #[test]
    fn controlled_hex_keeps_values_and_validation_precedence() {
        for byte in 0..=255_u8 {
            for text in [format!("{byte:02x}"), format!("{byte:02X}")] {
                assert_eq!(bytes_from_hex(&text), Ok(vec![byte]));
            }
        }
        for text in ["", "0", "xyz", "gg", "é", "🦀", "0x00", " 0", "00\n0"] {
            let expected = if text.is_empty() {
                Ok(Vec::new())
            } else {
                Err("$bytes must be even-length hexadecimal".to_owned())
            };
            assert_eq!(bytes_from_hex(text), expected, "{text:?}");
        }
        let mut calls = 0;
        assert_eq!(
            bytes_from_hex_controlled("0", |_| {
                calls += 1;
                Ok::<(), Infallible>(())
            }),
            Err(JsonParseError::Invalid(
                "$bytes must be even-length hexadecimal".into()
            ))
        );
        assert_eq!(calls, 0, "odd length is refused before scanning");

        let invalid = "aB".repeat(JSON_PARSE_RUN_BYTES) + "xz";
        let mut runs = Vec::new();
        assert_eq!(
            bytes_from_hex_controlled(&invalid, |bytes| {
                runs.push(bytes);
                Ok::<(), Infallible>(())
            }),
            Err(JsonParseError::Invalid(
                "$bytes must be even-length hexadecimal".into()
            ))
        );
        assert_eq!(
            runs,
            [JSON_PARSE_RUN_BYTES, JSON_PARSE_RUN_BYTES, 2],
            "all callbacks are validation runs; decoding never starts"
        );
    }

    #[test]
    fn every_validation_and_decode_run_preserves_non_send_cancellation() {
        let text = "aB".repeat(JSON_PARSE_RUN_BYTES + 7);
        let mut runs = Vec::new();
        let bytes = bytes_from_hex_controlled(&text, |bytes| {
            runs.push(bytes);
            Ok::<(), Infallible>(())
        })
        .unwrap();
        assert_eq!(bytes, vec![0xab; JSON_PARSE_RUN_BYTES + 7]);
        assert_eq!(
            runs,
            [
                JSON_PARSE_RUN_BYTES,
                JSON_PARSE_RUN_BYTES,
                14,
                JSON_PARSE_RUN_BYTES,
                JSON_PARSE_RUN_BYTES,
                14
            ]
        );
        for cancel in 1..=runs.len() {
            let token = Rc::new(cancel);
            let mut seen = 0;
            let error = bytes_from_hex_controlled(&text, |_| {
                seen += 1;
                if seen == cancel {
                    Err(token.clone())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            let JsonParseError::Interrupted(error) = error else {
                panic!("lost cancellation")
            };
            assert!(Rc::ptr_eq(&error, &token));
            assert_eq!(seen, cancel);
        }
    }
}
