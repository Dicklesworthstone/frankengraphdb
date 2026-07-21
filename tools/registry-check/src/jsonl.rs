//! Deterministic JSONL event emission (bead logging contract).
//!
//! Events carry no timestamps and no randomness: the same registries produce
//! byte-identical event streams (determinism-by-default applies to the
//! constitutional tooling too). Field order is the insertion order of the
//! emitting site, so streams diff cleanly.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<JsonValue>),
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn render_value(v: &JsonValue, out: &mut String) {
    match v {
        JsonValue::Str(s) => {
            out.push('"');
            out.push_str(&escape(s));
            out.push('"');
        }
        JsonValue::Int(i) => {
            let _ = write!(out, "{i}");
        }
        JsonValue::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        JsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_value(item, out);
            }
            out.push(']');
        }
    }
}

/// Render one event as a single JSON line (no trailing newline).
pub fn event(fields: &[(&str, JsonValue)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&escape(key));
        out.push_str("\":");
        render_value(value, &mut out);
    }
    out.push('}');
    out
}

pub fn s(text: impl Into<String>) -> JsonValue {
    JsonValue::Str(text.into())
}

pub fn n(v: i64) -> JsonValue {
    JsonValue::Int(v)
}

pub fn b(v: bool) -> JsonValue {
    JsonValue::Bool(v)
}

pub fn arr(items: impl IntoIterator<Item = String>) -> JsonValue {
    JsonValue::Array(items.into_iter().map(JsonValue::Str).collect())
}
