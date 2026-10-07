//! PackStream v1, the value encoding under every Bolt message.
//!
//! The decoder is strict and bounded: every length is checked against the
//! bytes actually present before anything is allocated, nesting is capped,
//! strings must be UTF-8, map keys must be strings and appear once, and a
//! value must consume its input exactly. The encoder always chooses the
//! smallest representation, so equal values encode to equal bytes.

/// Deepest list/map/structure nesting a decoded value may have.
pub const MAX_DEPTH: usize = 64;

/// One PackStream value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<Value>),
    /// Entries in wire order; keys are unique.
    Map(Vec<(String, Value)>),
    /// A tagged structure (a message, a node, a date-time).
    Struct {
        tag: u8,
        fields: Vec<Value>,
    },
}

impl Value {
    #[must_use]
    pub fn string(text: impl Into<String>) -> Self {
        Self::String(text.into())
    }

    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// The entries of a map value.
    #[must_use]
    pub fn as_map(&self) -> Option<&[(String, Value)]> {
        match self {
            Self::Map(entries) => Some(entries),
            _ => None,
        }
    }
}

/// The value under `key` in map entries.
#[must_use]
pub fn get<'a>(entries: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find_map(|(name, value)| (name == key).then_some(value))
}

/// Why bytes are not one canonical-enough PackStream value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended inside a value.
    Truncated,
    /// A marker byte PackStream v1 does not define.
    UnknownMarker(u8),
    /// A string that is not UTF-8.
    InvalidUtf8,
    /// A map key that is not a string.
    NonStringKey,
    /// The same key twice in one map.
    DuplicateKey,
    /// Nesting beyond [`MAX_DEPTH`].
    TooDeep,
    /// Bytes after a complete value.
    TrailingBytes,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("PackStream value is truncated"),
            Self::UnknownMarker(marker) => write!(f, "unknown PackStream marker 0x{marker:02X}"),
            Self::InvalidUtf8 => f.write_str("PackStream string is not UTF-8"),
            Self::NonStringKey => f.write_str("PackStream map key is not a string"),
            Self::DuplicateKey => f.write_str("PackStream map repeats a key"),
            Self::TooDeep => f.write_str("PackStream value nests too deeply"),
            Self::TrailingBytes => f.write_str("bytes follow a complete PackStream value"),
        }
    }
}
impl core::error::Error for DecodeError {}

/// Append the encoding of `value`.
pub fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0xC0),
        Value::Bool(false) => out.push(0xC2),
        Value::Bool(true) => out.push(0xC3),
        Value::Int(value) => encode_int(*value, out),
        Value::Float(value) => {
            out.push(0xC1);
            out.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        Value::Bytes(bytes) => {
            sized(out, bytes.len(), None, [0xCC, 0xCD, 0xCE]);
            out.extend_from_slice(bytes);
        }
        Value::String(text) => {
            sized(out, text.len(), Some(0x80), [0xD0, 0xD1, 0xD2]);
            out.extend_from_slice(text.as_bytes());
        }
        Value::List(items) => {
            sized(out, items.len(), Some(0x90), [0xD4, 0xD5, 0xD6]);
            for item in items {
                encode(item, out);
            }
        }
        Value::Map(entries) => {
            sized(out, entries.len(), Some(0xA0), [0xD8, 0xD9, 0xDA]);
            for (key, value) in entries {
                sized(out, key.len(), Some(0x80), [0xD0, 0xD1, 0xD2]);
                out.extend_from_slice(key.as_bytes());
                encode(value, out);
            }
        }
        Value::Struct { tag, fields } => {
            // Bolt structures have at most 15 fields: a tiny header only.
            debug_assert!(fields.len() < 16);
            out.push(0xB0 | (fields.len() as u8 & 0x0F));
            out.push(*tag);
            for field in fields {
                encode(field, out);
            }
        }
    }
}

fn encode_int(value: i64, out: &mut Vec<u8>) {
    if (-16..=127).contains(&value) {
        out.push(value as i8 as u8);
    } else if let Ok(small) = i8::try_from(value) {
        out.push(0xC8);
        out.push(small as u8);
    } else if let Ok(small) = i16::try_from(value) {
        out.push(0xC9);
        out.extend_from_slice(&small.to_be_bytes());
    } else if let Ok(small) = i32::try_from(value) {
        out.push(0xCA);
        out.extend_from_slice(&small.to_be_bytes());
    } else {
        out.push(0xCB);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

/// A length header: tiny form when `tiny` is given and the length is under
/// 16, else the 8/16/32-bit marker. Lengths beyond u32 cannot be framed by
/// a Bolt message at all, so they saturate rather than wrap.
fn sized(out: &mut Vec<u8>, len: usize, tiny: Option<u8>, markers: [u8; 3]) {
    match tiny {
        Some(base) if len < 16 => out.push(base | len as u8),
        _ if len <= usize::from(u8::MAX) => {
            out.push(markers[0]);
            out.push(len as u8);
        }
        _ if len <= usize::from(u16::MAX) => {
            out.push(markers[1]);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            out.push(markers[2]);
            out.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_be_bytes());
        }
    }
}

/// Decode exactly one value occupying all of `bytes`.
pub fn decode(bytes: &[u8]) -> Result<Value, DecodeError> {
    let mut reader = Reader { bytes, at: 0 };
    let value = reader.value(0)?;
    if reader.at != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(value)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.at.checked_add(len).ok_or(DecodeError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(DecodeError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn length(&mut self, width: usize) -> Result<usize, DecodeError> {
        let raw = self.take(width)?;
        Ok(raw
            .iter()
            .fold(0_usize, |acc, &b| (acc << 8) | usize::from(b)))
    }

    fn text(&mut self, len: usize) -> Result<String, DecodeError> {
        let raw = self.take(len)?;
        core::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| DecodeError::InvalidUtf8)
    }

    /// Every element takes at least one byte, so no count may exceed the
    /// bytes left: a forged huge count fails before it allocates.
    fn count(&self, len: usize) -> Result<usize, DecodeError> {
        if len > self.bytes.len() - self.at {
            return Err(DecodeError::Truncated);
        }
        Ok(len)
    }

    fn items(&mut self, len: usize, depth: usize) -> Result<Vec<Value>, DecodeError> {
        let len = self.count(len)?;
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            items.push(self.value(depth + 1)?);
        }
        Ok(items)
    }

    fn entries(&mut self, len: usize, depth: usize) -> Result<Vec<(String, Value)>, DecodeError> {
        let len = self.count(len)?;
        let mut entries: Vec<(String, Value)> = Vec::with_capacity(len);
        for _ in 0..len {
            let Value::String(key) = self.value(depth + 1)? else {
                return Err(DecodeError::NonStringKey);
            };
            if entries.iter().any(|(seen, _)| *seen == key) {
                return Err(DecodeError::DuplicateKey);
            }
            let value = self.value(depth + 1)?;
            entries.push((key, value));
        }
        Ok(entries)
    }

    fn value(&mut self, depth: usize) -> Result<Value, DecodeError> {
        if depth > MAX_DEPTH {
            return Err(DecodeError::TooDeep);
        }
        let marker = self.byte()?;
        Ok(match marker {
            0x00..=0x7F => Value::Int(i64::from(marker)),
            0xF0..=0xFF => Value::Int(i64::from(marker as i8)),
            0x80..=0x8F => Value::String(self.text(usize::from(marker & 0x0F))?),
            0x90..=0x9F => Value::List(self.items(usize::from(marker & 0x0F), depth)?),
            0xA0..=0xAF => Value::Map(self.entries(usize::from(marker & 0x0F), depth)?),
            0xB0..=0xBF => {
                let tag = self.byte()?;
                Value::Struct {
                    tag,
                    fields: self.items(usize::from(marker & 0x0F), depth)?,
                }
            }
            0xC0 => Value::Null,
            0xC1 => {
                let raw: [u8; 8] = self.take(8)?.try_into().expect("eight bytes");
                Value::Float(f64::from_bits(u64::from_be_bytes(raw)))
            }
            0xC2 => Value::Bool(false),
            0xC3 => Value::Bool(true),
            0xC8 => Value::Int(i64::from(self.byte()? as i8)),
            0xC9 => {
                let raw: [u8; 2] = self.take(2)?.try_into().expect("two bytes");
                Value::Int(i64::from(i16::from_be_bytes(raw)))
            }
            0xCA => {
                let raw: [u8; 4] = self.take(4)?.try_into().expect("four bytes");
                Value::Int(i64::from(i32::from_be_bytes(raw)))
            }
            0xCB => {
                let raw: [u8; 8] = self.take(8)?.try_into().expect("eight bytes");
                Value::Int(i64::from_be_bytes(raw))
            }
            0xCC..=0xCE => {
                let len = self.length(1 << (marker - 0xCC))?;
                Value::Bytes(self.take(len)?.to_vec())
            }
            0xD0..=0xD2 => {
                let len = self.length(1 << (marker - 0xD0))?;
                Value::String(self.text(len)?)
            }
            0xD4..=0xD6 => {
                let len = self.length(1 << (marker - 0xD4))?;
                Value::List(self.items(len, depth)?)
            }
            0xD8..=0xDA => {
                let len = self.length(1 << (marker - 0xD8))?;
                Value::Map(self.entries(len, depth)?)
            }
            other => return Err(DecodeError::UnknownMarker(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(value: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        encode(value, &mut out);
        out
    }

    #[test]
    fn integers_take_their_smallest_form_and_round_trip() {
        for (value, encoded) in [
            (0, vec![0x00]),
            (127, vec![0x7F]),
            (-16, vec![0xF0]),
            (-17, vec![0xC8, 0xEF]),
            (-128, vec![0xC8, 0x80]),
            (128, vec![0xC9, 0x00, 0x80]),
            (40_000, vec![0xCA, 0x00, 0x00, 0x9C, 0x40]),
            (
                i64::MIN,
                vec![0xCB, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            ),
        ] {
            assert_eq!(bytes(&Value::Int(value)), encoded, "{value}");
            assert_eq!(decode(&encoded), Ok(Value::Int(value)));
        }
        // A wider form than necessary still decodes to the same integer.
        assert_eq!(decode(&[0xCB, 0, 0, 0, 0, 0, 0, 0, 1]), Ok(Value::Int(1)));
    }

    #[test]
    fn composite_values_round_trip_at_every_size_class() {
        let long = "x".repeat(300);
        let value = Value::Map(vec![
            ("a".into(), Value::Null),
            ("b".into(), Value::Bool(true)),
            ("c".into(), Value::Float(-1.5)),
            ("d".into(), Value::Bytes(vec![1, 2, 3])),
            ("e".into(), Value::string(long.clone())),
            (
                "f".into(),
                Value::List((0..20).map(Value::Int).collect::<Vec<_>>()),
            ),
            (
                "g".into(),
                Value::Struct {
                    tag: 0x4E,
                    fields: vec![Value::Int(1), Value::List(vec![]), Value::Map(vec![])],
                },
            ),
        ]);
        let encoded = bytes(&value);
        assert_eq!(decode(&encoded), Ok(value));
        assert_eq!(&bytes(&Value::string("hi"))[..], [0x82, b'h', b'i']);
        assert_eq!(bytes(&Value::string(long))[..3], [0xD1, 0x01, 0x2C]);
    }

    #[test]
    fn malformed_input_refuses_without_allocating_forged_lengths() {
        assert_eq!(decode(&[]), Err(DecodeError::Truncated));
        assert_eq!(
            decode(&[0xD6, 0xFF, 0xFF, 0xFF, 0xFF]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(decode(&[0x82, 0xFF, 0xFE]), Err(DecodeError::InvalidUtf8));
        assert_eq!(decode(&[0xA1, 0x01, 0x01]), Err(DecodeError::NonStringKey));
        assert_eq!(
            decode(&[0xA2, 0x81, b'k', 0x01, 0x81, b'k', 0x02]),
            Err(DecodeError::DuplicateKey)
        );
        assert_eq!(decode(&[0xC0, 0xC0]), Err(DecodeError::TrailingBytes));
        assert_eq!(decode(&[0xE0]), Err(DecodeError::UnknownMarker(0xE0)));
        let deep: Vec<u8> = core::iter::repeat_n(0x91, MAX_DEPTH + 1)
            .chain([0xC0])
            .collect();
        assert_eq!(decode(&deep), Err(DecodeError::TooDeep));
    }
}
