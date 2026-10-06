//! Typed FGP v1 operation bodies for the frames `fgdbd` serves.
//!
//! The framing layer treats a payload as opaque bytes. This module gives the
//! frames of the served subset an exact, canonical body encoding so that a
//! server and its clients agree byte for byte without any serialization
//! framework. Encoding is total; decoding is strict:
//!
//! - every integer is fixed-width big-endian, and every length is a `u32`;
//! - text is length-prefixed UTF-8, validated before it is returned;
//! - every closed union has an explicit tag, and an unknown tag is refused;
//! - list, map, nesting and byte sizes are bounded before allocation;
//! - trailing bytes, a truncated body at any prefix, and a noncanonical
//!   spelling (an unsorted or duplicated map key, a non-boolean byte) are
//!   refused, so one value has exactly one encoding.
//!
//! Debug output of a credential is redacted. Nothing here authenticates,
//! authorizes or releases output; those decisions remain with the owning
//! services, exactly as for the frame codec.

use crate::{Posture, ReadyBinding, SessionBinding};
use core::fmt;

/// Largest statement text one EXECUTE may carry.
pub const MAX_STATEMENT_BYTES: usize = 1 << 20;
/// Largest database name, column name, parameter name or map key.
pub const MAX_NAME_BYTES: usize = 1024;
/// Largest scalar text, decimal or byte value.
pub const MAX_SCALAR_BYTES: usize = 1 << 20;
/// Largest credential an AUTH may carry.
pub const MAX_CREDENTIAL_BYTES: usize = 32 * 1024;
/// Largest human-readable ERROR message.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_PARAMETERS: usize = 1024;
pub const MAX_COLUMNS: usize = 4096;
pub const MAX_ROWS_PER_CHUNK: usize = 1 << 20;
/// Lists and maps nest at most this deep.
pub const MAX_VALUE_DEPTH: usize = 64;
/// Total value nodes in one decoded body.
pub const MAX_VALUE_NODES: usize = 1 << 22;

/// Structural, data-independent body failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyError {
    Truncated,
    TrailingBytes,
    UnknownTag,
    InvalidUtf8,
    TooLarge,
    TooDeep,
    Noncanonical,
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "truncated FGP body",
            Self::TrailingBytes => "trailing bytes after FGP body",
            Self::UnknownTag => "unknown FGP body tag",
            Self::InvalidUtf8 => "FGP body text is not UTF-8",
            Self::TooLarge => "FGP body exceeds a bounded limit",
            Self::TooDeep => "FGP value nests too deeply",
            Self::Noncanonical => "noncanonical FGP body encoding",
        })
    }
}
impl core::error::Error for BodyError {}

// ---------------------------------------------------------------------------
// Primitive codec
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u128(&mut self, v: u128) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn i128(&mut self, v: i128) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn array<const N: usize>(&mut self, v: &[u8; N]) {
        self.0.extend_from_slice(v);
    }
    fn len(&mut self, n: usize) {
        // Every encoder bounds its collections far below u32::MAX first.
        self.u32(u32::try_from(n).expect("bounded FGP length fits u32"));
    }
    fn bytes(&mut self, v: &[u8]) {
        self.len(v.len());
        self.0.extend_from_slice(v);
    }
    fn text(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

struct In<'a> {
    bytes: &'a [u8],
    at: usize,
    nodes: usize,
}

impl<'a> In<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            nodes: 0,
        }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], BodyError> {
        let end = self.at.checked_add(n).ok_or(BodyError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(BodyError::Truncated)?;
        self.at = end;
        Ok(slice)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], BodyError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, BodyError> {
        Ok(self.take(1)?[0])
    }
    fn bool(&mut self) -> Result<bool, BodyError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(BodyError::Noncanonical),
        }
    }
    fn u16(&mut self) -> Result<u16, BodyError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, BodyError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, BodyError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32, BodyError> {
        Ok(i32::from_be_bytes(self.array()?))
    }
    fn i64(&mut self) -> Result<i64, BodyError> {
        Ok(i64::from_be_bytes(self.array()?))
    }
    fn u128(&mut self) -> Result<u128, BodyError> {
        Ok(u128::from_be_bytes(self.array()?))
    }
    fn i128(&mut self) -> Result<i128, BodyError> {
        Ok(i128::from_be_bytes(self.array()?))
    }
    /// A declared count, refused before any allocation when it exceeds the
    /// caller's bound or could not possibly fit in the remaining bytes.
    fn count(&mut self, limit: usize, min_item_bytes: usize) -> Result<usize, BodyError> {
        let n = usize::try_from(self.u32()?).map_err(|_| BodyError::TooLarge)?;
        if n > limit {
            return Err(BodyError::TooLarge);
        }
        let remaining = self.bytes.len() - self.at;
        if n.saturating_mul(min_item_bytes.max(1)) > remaining {
            return Err(BodyError::Truncated);
        }
        Ok(n)
    }
    fn bytes(&mut self, limit: usize) -> Result<&'a [u8], BodyError> {
        let n = usize::try_from(self.u32()?).map_err(|_| BodyError::TooLarge)?;
        if n > limit {
            return Err(BodyError::TooLarge);
        }
        self.take(n)
    }
    fn text(&mut self, limit: usize) -> Result<String, BodyError> {
        let raw = self.bytes(limit)?;
        core::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| BodyError::InvalidUtf8)
    }
    fn node(&mut self) -> Result<(), BodyError> {
        self.nodes += 1;
        if self.nodes > MAX_VALUE_NODES {
            return Err(BodyError::TooLarge);
        }
        Ok(())
    }
    fn finish(self) -> Result<(), BodyError> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(BodyError::TrailingBytes)
        }
    }
}

fn check_len(n: usize, limit: usize) -> Result<(), BodyError> {
    if n > limit {
        Err(BodyError::TooLarge)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// The zone of a zoned timestamp: its tz identifier and the content address
/// of the exact tz database that resolved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireZone {
    pub identifier: String,
    pub tzdb_oid: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireTimestamp {
    pub instant_utc_nanos: i128,
    pub utc_offset_seconds: i32,
    pub zone: Option<WireZone>,
}

/// One result cell or statement argument. The union mirrors the engine's
/// value lattice (and the CLI robot contract's cell types) exactly: nothing
/// is widened, rounded or stringified in transit, except that an exact
/// decimal travels as its canonical decimal text.
#[derive(Clone, Debug, PartialEq)]
pub enum WireValue {
    Null,
    Bool(bool),
    Int(i64),
    /// IEEE-754 binary64; the bits are carried exactly.
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Timestamp(WireTimestamp),
    Vertex(u128),
    Edge(u128),
    /// A path: its start vertex, then one (edge, vertex) pair per step.
    Path {
        start: u128,
        steps: Vec<(u128, u128)>,
    },
    Vertices(Vec<u128>),
    Edges(Vec<u128>),
    List(Vec<WireValue>),
    /// Keys strictly ascending by UTF-8 bytes, as the engine stores a map.
    Map(Vec<(String, WireValue)>),
    /// An aggregate count.
    Count(u64),
    /// A wide aggregate integer (an exact `sum`).
    WideInt(i128),
    /// An exact average: reduced `numerator / denominator`, denominator > 0.
    Average {
        numerator: i128,
        denominator: u64,
    },
}

mod tag {
    pub const NULL: u8 = 0;
    pub const BOOL: u8 = 1;
    pub const INT: u8 = 2;
    pub const FLOAT: u8 = 3;
    pub const DECIMAL: u8 = 4;
    pub const TEXT: u8 = 5;
    pub const BYTES: u8 = 6;
    pub const TIMESTAMP: u8 = 7;
    pub const VERTEX: u8 = 8;
    pub const EDGE: u8 = 9;
    pub const PATH: u8 = 10;
    pub const VERTICES: u8 = 11;
    pub const EDGES: u8 = 12;
    pub const LIST: u8 = 13;
    pub const MAP: u8 = 14;
    pub const COUNT: u8 = 15;
    pub const WIDE_INT: u8 = 16;
    pub const AVERAGE: u8 = 17;
}

impl WireValue {
    fn check(&self, depth: usize) -> Result<(), BodyError> {
        if depth > MAX_VALUE_DEPTH {
            return Err(BodyError::TooDeep);
        }
        match self {
            Self::Decimal(v) | Self::Text(v) => check_len(v.len(), MAX_SCALAR_BYTES),
            Self::Bytes(v) => check_len(v.len(), MAX_SCALAR_BYTES),
            Self::Timestamp(t) => t
                .zone
                .as_ref()
                .map_or(Ok(()), |z| check_len(z.identifier.len(), MAX_NAME_BYTES)),
            Self::Path { steps, .. } => check_len(steps.len(), MAX_VALUE_NODES),
            Self::Vertices(v) | Self::Edges(v) => check_len(v.len(), MAX_VALUE_NODES),
            Self::List(items) => {
                check_len(items.len(), MAX_VALUE_NODES)?;
                items.iter().try_for_each(|item| item.check(depth + 1))
            }
            Self::Map(entries) => {
                check_len(entries.len(), MAX_VALUE_NODES)?;
                for pair in entries.windows(2) {
                    if pair[0].0.as_bytes() >= pair[1].0.as_bytes() {
                        return Err(BodyError::Noncanonical);
                    }
                }
                entries.iter().try_for_each(|(key, value)| {
                    check_len(key.len(), MAX_NAME_BYTES)?;
                    value.check(depth + 1)
                })
            }
            Self::Average { denominator, .. } if *denominator == 0 => Err(BodyError::Noncanonical),
            _ => Ok(()),
        }
    }

    fn put(&self, out: &mut Out) {
        match self {
            Self::Null => out.u8(tag::NULL),
            Self::Bool(v) => {
                out.u8(tag::BOOL);
                out.u8(u8::from(*v));
            }
            Self::Int(v) => {
                out.u8(tag::INT);
                out.i64(*v);
            }
            Self::Float(v) => {
                out.u8(tag::FLOAT);
                out.u64(v.to_bits());
            }
            Self::Decimal(v) => {
                out.u8(tag::DECIMAL);
                out.text(v);
            }
            Self::Text(v) => {
                out.u8(tag::TEXT);
                out.text(v);
            }
            Self::Bytes(v) => {
                out.u8(tag::BYTES);
                out.bytes(v);
            }
            Self::Timestamp(t) => {
                out.u8(tag::TIMESTAMP);
                out.i128(t.instant_utc_nanos);
                out.i32(t.utc_offset_seconds);
                match &t.zone {
                    None => out.u8(0),
                    Some(zone) => {
                        out.u8(1);
                        out.text(&zone.identifier);
                        out.array(&zone.tzdb_oid);
                    }
                }
            }
            Self::Vertex(v) => {
                out.u8(tag::VERTEX);
                out.u128(*v);
            }
            Self::Edge(v) => {
                out.u8(tag::EDGE);
                out.u128(*v);
            }
            Self::Path { start, steps } => {
                out.u8(tag::PATH);
                out.u128(*start);
                out.len(steps.len());
                for (edge, vertex) in steps {
                    out.u128(*edge);
                    out.u128(*vertex);
                }
            }
            Self::Vertices(ids) | Self::Edges(ids) => {
                out.u8(if matches!(self, Self::Vertices(_)) {
                    tag::VERTICES
                } else {
                    tag::EDGES
                });
                out.len(ids.len());
                for id in ids {
                    out.u128(*id);
                }
            }
            Self::List(items) => {
                out.u8(tag::LIST);
                out.len(items.len());
                for item in items {
                    item.put(out);
                }
            }
            Self::Map(entries) => {
                out.u8(tag::MAP);
                out.len(entries.len());
                for (key, value) in entries {
                    out.text(key);
                    value.put(out);
                }
            }
            Self::Count(v) => {
                out.u8(tag::COUNT);
                out.u64(*v);
            }
            Self::WideInt(v) => {
                out.u8(tag::WIDE_INT);
                out.i128(*v);
            }
            Self::Average {
                numerator,
                denominator,
            } => {
                out.u8(tag::AVERAGE);
                out.i128(*numerator);
                out.u64(*denominator);
            }
        }
    }

    fn get(input: &mut In<'_>, depth: usize) -> Result<Self, BodyError> {
        if depth > MAX_VALUE_DEPTH {
            return Err(BodyError::TooDeep);
        }
        input.node()?;
        Ok(match input.u8()? {
            tag::NULL => Self::Null,
            tag::BOOL => Self::Bool(input.bool()?),
            tag::INT => Self::Int(input.i64()?),
            tag::FLOAT => Self::Float(f64::from_bits(input.u64()?)),
            tag::DECIMAL => Self::Decimal(input.text(MAX_SCALAR_BYTES)?),
            tag::TEXT => Self::Text(input.text(MAX_SCALAR_BYTES)?),
            tag::BYTES => Self::Bytes(input.bytes(MAX_SCALAR_BYTES)?.to_vec()),
            tag::TIMESTAMP => {
                let instant_utc_nanos = input.i128()?;
                let utc_offset_seconds = input.i32()?;
                let zone = if input.bool()? {
                    Some(WireZone {
                        identifier: input.text(MAX_NAME_BYTES)?,
                        tzdb_oid: input.array()?,
                    })
                } else {
                    None
                };
                Self::Timestamp(WireTimestamp {
                    instant_utc_nanos,
                    utc_offset_seconds,
                    zone,
                })
            }
            tag::VERTEX => Self::Vertex(input.u128()?),
            tag::EDGE => Self::Edge(input.u128()?),
            tag::PATH => {
                let start = input.u128()?;
                let n = input.count(MAX_VALUE_NODES, 32)?;
                let mut steps = Vec::with_capacity(n);
                for _ in 0..n {
                    input.node()?;
                    steps.push((input.u128()?, input.u128()?));
                }
                Self::Path { start, steps }
            }
            t @ (tag::VERTICES | tag::EDGES) => {
                let n = input.count(MAX_VALUE_NODES, 16)?;
                let mut ids = Vec::with_capacity(n);
                for _ in 0..n {
                    input.node()?;
                    ids.push(input.u128()?);
                }
                if t == tag::VERTICES {
                    Self::Vertices(ids)
                } else {
                    Self::Edges(ids)
                }
            }
            tag::LIST => {
                let n = input.count(MAX_VALUE_NODES, 1)?;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(Self::get(input, depth + 1)?);
                }
                Self::List(items)
            }
            tag::MAP => {
                let n = input.count(MAX_VALUE_NODES, 5)?;
                let mut entries: Vec<(String, WireValue)> = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = input.text(MAX_NAME_BYTES)?;
                    if entries
                        .last()
                        .is_some_and(|(previous, _)| previous.as_bytes() >= key.as_bytes())
                    {
                        return Err(BodyError::Noncanonical);
                    }
                    let value = Self::get(input, depth + 1)?;
                    entries.push((key, value));
                }
                Self::Map(entries)
            }
            tag::COUNT => Self::Count(input.u64()?),
            tag::WIDE_INT => Self::WideInt(input.i128()?),
            tag::AVERAGE => {
                let numerator = input.i128()?;
                let denominator = input.u64()?;
                if denominator == 0 {
                    return Err(BodyError::Noncanonical);
                }
                Self::Average {
                    numerator,
                    denominator,
                }
            }
            _ => return Err(BodyError::UnknownTag),
        })
    }
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// Every typed body encodes to exactly one byte string and decodes only from
/// that string.
pub trait Body: Sized {
    fn encode(&self) -> Result<Vec<u8>, BodyError>;
    fn decode(bytes: &[u8]) -> Result<Self, BodyError>;
}

macro_rules! body {
    ($ty:ty, |$s:ident, $out:ident| $enc:block, |$input:ident| $dec:block) => {
        impl Body for $ty {
            fn encode(&self) -> Result<Vec<u8>, BodyError> {
                let $s = self;
                let mut $out = Out::default();
                $enc
                Ok($out.0)
            }
            fn decode(bytes: &[u8]) -> Result<Self, BodyError> {
                let mut $input = In::new(bytes);
                let value = $dec;
                $input.finish()?;
                Ok(value)
            }
        }
    };
}

/// HELLO: protocol mechanics only. No database selector, namespace or limit
/// of any particular database appears here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub min_version: u16,
    pub max_version: u16,
    pub client_nonce: [u8; 32],
    pub max_frame_len: u32,
}

body!(
    Hello,
    |s, out| {
        out.u16(s.min_version);
        out.u16(s.max_version);
        out.array(&s.client_nonce);
        out.u32(s.max_frame_len);
    },
    |input| {
        Hello {
            min_version: input.u16()?,
            max_version: input.u16()?,
            client_nonce: input.array()?,
            max_frame_len: input.u32()?,
        }
    }
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelloAck {
    pub version: u16,
    pub server_nonce: [u8; 32],
    /// The largest frame the server accepts. The server in turn never sends a
    /// frame larger than the client's HELLO limit.
    pub max_frame_len: u32,
    /// The flow credit every result stream starts with. The client replenishes
    /// it with WINDOW_UPDATE; the server never sends beyond it.
    pub initial_window_bytes: u64,
    pub initial_window_rows: u64,
}

body!(
    HelloAck,
    |s, out| {
        out.u16(s.version);
        out.array(&s.server_nonce);
        out.u32(s.max_frame_len);
        out.u64(s.initial_window_bytes);
        out.u64(s.initial_window_rows);
    },
    |input| {
        HelloAck {
            version: input.u16()?,
            server_nonce: input.array()?,
            max_frame_len: input.u32()?,
            initial_window_bytes: input.u64()?,
            initial_window_rows: input.u64()?,
        }
    }
);

/// The authentication mechanisms. A Warden capability token is presented as
/// opaque bearer bytes; the server's Authority alone decides what it means.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    WardenCapability(Vec<u8>),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WardenCapability(_) => f.write_str("WardenCapability(<redacted>)"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Auth {
    pub credential: Credential,
}

body!(
    Auth,
    |s, out| {
        match &s.credential {
            Credential::WardenCapability(token) => {
                check_len(token.len(), MAX_CREDENTIAL_BYTES)?;
                out.u8(1);
                out.bytes(token);
            }
        }
    },
    |input| {
        match input.u8()? {
            1 => Auth {
                credential: Credential::WardenCapability(
                    input.bytes(MAX_CREDENTIAL_BYTES)?.to_vec(),
                ),
            },
            _ => return Err(BodyError::UnknownTag),
        }
    }
);

/// AUTH_OK hands the client the transcript binding its session headers carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthOk {
    pub session: SessionBinding,
}

body!(
    AuthOk,
    |s, out| {
        out.array(&s.session.transcript);
        out.u64(s.session.auth_generation);
    },
    |input| {
        AuthOk {
            session: SessionBinding {
                transcript: input.array()?,
                auth_generation: input.u64()?,
            },
        }
    }
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectDatabase {
    pub name: String,
}

body!(
    SelectDatabase,
    |s, out| {
        check_len(s.name.len(), MAX_NAME_BYTES)?;
        out.text(&s.name);
    },
    |input| {
        SelectDatabase {
            name: input.text(MAX_NAME_BYTES)?,
        }
    }
);

/// READY: the selected database's public binding plus the frontier the
/// selection observed. The client copies these fields into every Ready header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ready {
    pub namespace: [u8; 32],
    pub incarnation: [u8; 32],
    pub service_epoch: u64,
    pub posture: Posture,
    pub authority_commitment: [u8; 32],
    pub frontier: u64,
}

impl Ready {
    #[must_use]
    pub const fn binding(&self, session: SessionBinding) -> ReadyBinding {
        ReadyBinding {
            session,
            namespace: self.namespace,
            incarnation: self.incarnation,
            service_epoch: self.service_epoch,
            posture: self.posture,
            authority_commitment: self.authority_commitment,
        }
    }
}

body!(
    Ready,
    |s, out| {
        out.array(&s.namespace);
        out.array(&s.incarnation);
        out.u64(s.service_epoch);
        out.u8(match s.posture {
            Posture::Local => 0,
            Posture::Sharded => 1,
        });
        out.array(&s.authority_commitment);
        out.u64(s.frontier);
    },
    |input| {
        Ready {
            namespace: input.array()?,
            incarnation: input.array()?,
            service_epoch: input.u64()?,
            posture: match input.u8()? {
                0 => Posture::Local,
                1 => Posture::Sharded,
                _ => return Err(BodyError::UnknownTag),
            },
            authority_commitment: input.array()?,
            frontier: input.u64()?,
        }
    }
);

/// What an EXECUTE asks for. A read can never commit: the server runs it
/// through a read-only authorized session, and a write statement presented
/// as a read is refused rather than reinterpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecuteMode {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Execute {
    pub mode: ExecuteMode,
    pub statement: String,
    /// Named arguments, names strictly ascending and unique.
    pub parameters: Vec<(String, WireValue)>,
}

body!(
    Execute,
    |s, out| {
        check_len(s.statement.len(), MAX_STATEMENT_BYTES)?;
        check_len(s.parameters.len(), MAX_PARAMETERS)?;
        for pair in s.parameters.windows(2) {
            if pair[0].0.as_bytes() >= pair[1].0.as_bytes() {
                return Err(BodyError::Noncanonical);
            }
        }
        out.u8(match s.mode {
            ExecuteMode::Read => 0,
            ExecuteMode::Write => 1,
        });
        out.text(&s.statement);
        out.len(s.parameters.len());
        for (name, value) in &s.parameters {
            check_len(name.len(), MAX_NAME_BYTES)?;
            value.check(0)?;
            out.text(name);
            value.put(&mut out);
        }
    },
    |input| {
        let mode = match input.u8()? {
            0 => ExecuteMode::Read,
            1 => ExecuteMode::Write,
            _ => return Err(BodyError::UnknownTag),
        };
        let statement = input.text(MAX_STATEMENT_BYTES)?;
        let n = input.count(MAX_PARAMETERS, 5)?;
        let mut parameters: Vec<(String, WireValue)> = Vec::with_capacity(n);
        for _ in 0..n {
            let name = input.text(MAX_NAME_BYTES)?;
            if parameters
                .last()
                .is_some_and(|(previous, _)| previous.as_bytes() >= name.as_bytes())
            {
                return Err(BodyError::Noncanonical);
            }
            let value = WireValue::get(&mut input, 0)?;
            parameters.push((name, value));
        }
        Execute {
            mode,
            statement,
            parameters,
        }
    }
);

/// One chunk of a session-owned result stream. The first chunk of a stream
/// carries the column names; later chunks carry `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultChunk {
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<WireValue>>,
}

impl ResultChunk {
    /// The encoded size of one row, as a chunker needs to fill a frame.
    pub fn row_len(row: &[WireValue]) -> Result<usize, BodyError> {
        let mut out = Out::default();
        out.len(row.len());
        for value in row {
            value.check(0)?;
            value.put(&mut out);
        }
        Ok(out.0.len())
    }
}

body!(
    ResultChunk,
    |s, out| {
        check_len(s.rows.len(), MAX_ROWS_PER_CHUNK)?;
        match &s.columns {
            None => out.u8(0),
            Some(columns) => {
                check_len(columns.len(), MAX_COLUMNS)?;
                out.u8(1);
                out.len(columns.len());
                for column in columns {
                    check_len(column.len(), MAX_NAME_BYTES)?;
                    out.text(column);
                }
            }
        }
        out.len(s.rows.len());
        for row in &s.rows {
            check_len(row.len(), MAX_COLUMNS)?;
            out.len(row.len());
            for value in row {
                value.check(0)?;
                value.put(&mut out);
            }
        }
    },
    |input| {
        let columns = if input.bool()? {
            let n = input.count(MAX_COLUMNS, 4)?;
            let mut columns = Vec::with_capacity(n);
            for _ in 0..n {
                columns.push(input.text(MAX_NAME_BYTES)?);
            }
            Some(columns)
        } else {
            None
        };
        let n = input.count(MAX_ROWS_PER_CHUNK, 4)?;
        let mut rows = Vec::with_capacity(n);
        for _ in 0..n {
            let width = input.count(MAX_COLUMNS, 1)?;
            let mut row = Vec::with_capacity(width);
            for _ in 0..width {
                row.push(WireValue::get(&mut input, 0)?);
            }
            rows.push(row);
        }
        ResultChunk { columns, rows }
    }
);

/// The terminal outcome of one statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A read answered at exactly this committed sequence.
    Rows { seq: u64 },
    /// A write committed at this sequence.
    WriteCommitted { seq: u64, statements: u64 },
    /// A write program that changed nothing closed as a read at this sequence.
    ReadClosed { seq: u64, statements: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultEnd {
    pub outcome: Outcome,
    /// Rows delivered on the stream before this END.
    pub rows: u64,
}

body!(
    ResultEnd,
    |s, out| {
        match s.outcome {
            Outcome::Rows { seq } => {
                out.u8(0);
                out.u64(seq);
            }
            Outcome::WriteCommitted { seq, statements } => {
                out.u8(1);
                out.u64(seq);
                out.u64(statements);
            }
            Outcome::ReadClosed { seq, statements } => {
                out.u8(2);
                out.u64(seq);
                out.u64(statements);
            }
        }
        out.u64(s.rows);
    },
    |input| {
        let outcome = match input.u8()? {
            0 => Outcome::Rows { seq: input.u64()? },
            1 => Outcome::WriteCommitted {
                seq: input.u64()?,
                statements: input.u64()?,
            },
            2 => Outcome::ReadClosed {
                seq: input.u64()?,
                statements: input.u64()?,
            },
            _ => return Err(BodyError::UnknownTag),
        };
        ResultEnd {
            outcome,
            rows: input.u64()?,
        }
    }
);

/// Closed, public error classes. A class never encodes whether a hidden
/// database, element or credential exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// The frame sequence or a body violated this protocol.
    Protocol,
    /// No common protocol version.
    UnsupportedVersion,
    /// The credential was not accepted.
    Unauthenticated,
    /// The database does not exist or this principal may not select it.
    NotFoundOrUnauthorized,
    /// The statement was refused before execution (syntax, binding, kind).
    Statement,
    /// The capability does not permit this operation or scope.
    PermissionDenied,
    /// An execution budget or signed limit was exceeded.
    Budget,
    /// A write conflicted with a concurrent commit; nothing was committed.
    Conflict,
    /// Execution failed; nothing was committed.
    Execution,
    /// A write's commit outcome is unknown; read the frontier to learn it.
    OutcomeUnknown,
    /// The connection already has a statement in flight.
    Busy,
    /// The server is draining and admits no new work.
    Draining,
    /// The client cancelled the statement's result stream.
    Cancelled,
}

impl ErrorCode {
    const fn tag(self) -> u16 {
        match self {
            Self::Protocol => 1,
            Self::UnsupportedVersion => 2,
            Self::Unauthenticated => 3,
            Self::NotFoundOrUnauthorized => 4,
            Self::Statement => 5,
            Self::PermissionDenied => 6,
            Self::Budget => 7,
            Self::Conflict => 8,
            Self::Execution => 9,
            Self::OutcomeUnknown => 10,
            Self::Busy => 11,
            Self::Draining => 12,
            Self::Cancelled => 13,
        }
    }
    fn from_tag(tag: u16) -> Result<Self, BodyError> {
        Ok(match tag {
            1 => Self::Protocol,
            2 => Self::UnsupportedVersion,
            3 => Self::Unauthenticated,
            4 => Self::NotFoundOrUnauthorized,
            5 => Self::Statement,
            6 => Self::PermissionDenied,
            7 => Self::Budget,
            8 => Self::Conflict,
            9 => Self::Execution,
            10 => Self::OutcomeUnknown,
            11 => Self::Busy,
            12 => Self::Draining,
            13 => Self::Cancelled,
            _ => return Err(BodyError::UnknownTag),
        })
    }
    /// The stable lowercase name a robot client prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Unauthenticated => "unauthenticated",
            Self::NotFoundOrUnauthorized => "not_found_or_unauthorized",
            Self::Statement => "statement",
            Self::PermissionDenied => "permission_denied",
            Self::Budget => "budget",
            Self::Conflict => "conflict",
            Self::Execution => "execution",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Busy => "busy",
            Self::Draining => "draining",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: ErrorCode,
    /// Structural diagnostics only: never query text, values or hidden facts.
    pub message: String,
}

body!(
    ErrorBody,
    |s, out| {
        out.u16(s.code.tag());
        let mut message = s.message.as_str();
        if message.len() > MAX_MESSAGE_BYTES {
            let mut end = MAX_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message = &message[..end];
        }
        out.text(message);
    },
    |input| {
        ErrorBody {
            code: ErrorCode::from_tag(input.u16()?)?,
            message: input.text(MAX_MESSAGE_BYTES)?,
        }
    }
);

/// WINDOW_UPDATE: one sequential flow-credit grant for the addressed stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowUpdate {
    pub sequence: u64,
    pub bytes: u64,
    pub rows: u64,
}

body!(
    WindowUpdate,
    |s, out| {
        out.u64(s.sequence);
        out.u64(s.bytes);
        out.u64(s.rows);
    },
    |input| {
        WindowUpdate {
            sequence: input.u64()?,
            bytes: input.u64()?,
            rows: input.u64()?,
        }
    }
);

/// PING and PONG carry an opaque echo value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ping {
    pub nonce: u64,
}

body!(
    Ping,
    |s, out| {
        out.u64(s.nonce);
    },
    |input| {
        Ping {
            nonce: input.u64()?,
        }
    }
);

/// DRAIN, GOODBYE and QUERY_CANCEL carry no body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Empty;

body!(
    Empty,
    |_s, out| {
        let _ = &mut out;
    },
    |input| {
        let _ = &mut input;
        Empty
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_value() -> WireValue {
        WireValue::List(vec![
            WireValue::Null,
            WireValue::Bool(true),
            WireValue::Int(-7),
            WireValue::Float(-0.0),
            WireValue::Decimal("12.50".into()),
            WireValue::Text("héllo".into()),
            WireValue::Bytes(vec![0, 255]),
            WireValue::Timestamp(WireTimestamp {
                instant_utc_nanos: -5,
                utc_offset_seconds: 3600,
                zone: Some(WireZone {
                    identifier: "Europe/Paris".into(),
                    tzdb_oid: [9; 32],
                }),
            }),
            WireValue::Vertex(u128::MAX),
            WireValue::Edge(3),
            WireValue::Path {
                start: 1,
                steps: vec![(10, 2), (11, 3)],
            },
            WireValue::Vertices(vec![1, 2]),
            WireValue::Edges(vec![]),
            WireValue::Map(vec![
                ("a".into(), WireValue::Int(1)),
                ("b".into(), WireValue::List(vec![])),
            ]),
            WireValue::Count(u64::MAX),
            WireValue::WideInt(i128::MIN),
            WireValue::Average {
                numerator: -3,
                denominator: 2,
            },
        ])
    }

    fn execute() -> Execute {
        Execute {
            mode: ExecuteMode::Write,
            statement: "MATCH (n) RETURN n".into(),
            parameters: vec![("a".into(), sample_value()), ("b".into(), WireValue::Null)],
        }
    }

    fn every_prefix_refuses<B: Body + fmt::Debug>(bytes: &[u8]) {
        for end in 0..bytes.len() {
            assert!(
                B::decode(&bytes[..end]).is_err(),
                "prefix {end} of {} decoded",
                bytes.len()
            );
        }
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert_eq!(
            B::decode(&longer).unwrap_err(),
            BodyError::TrailingBytes,
            "a trailing byte must be refused"
        );
    }

    #[test]
    fn every_body_round_trips_and_refuses_prefixes_and_trailers() {
        let bytes = execute().encode().unwrap();
        assert_eq!(Execute::decode(&bytes).unwrap(), execute());
        every_prefix_refuses::<Execute>(&bytes);

        let chunk = ResultChunk {
            columns: Some(vec!["x".into(), "y".into()]),
            rows: vec![vec![sample_value(), WireValue::Int(1)]],
        };
        let bytes = chunk.encode().unwrap();
        assert_eq!(ResultChunk::decode(&bytes).unwrap(), chunk);
        every_prefix_refuses::<ResultChunk>(&bytes);

        let hello = Hello {
            min_version: 1,
            max_version: 3,
            client_nonce: [4; 32],
            max_frame_len: 1 << 20,
        };
        let bytes = hello.encode().unwrap();
        assert_eq!(Hello::decode(&bytes).unwrap(), hello);
        every_prefix_refuses::<Hello>(&bytes);

        let ready = Ready {
            namespace: [1; 32],
            incarnation: [2; 32],
            service_epoch: 3,
            posture: Posture::Local,
            authority_commitment: [4; 32],
            frontier: 5,
        };
        let bytes = ready.encode().unwrap();
        assert_eq!(Ready::decode(&bytes).unwrap(), ready);
        every_prefix_refuses::<Ready>(&bytes);

        for outcome in [
            Outcome::Rows { seq: 9 },
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1,
            },
            Outcome::ReadClosed {
                seq: 2,
                statements: 3,
            },
        ] {
            let end = ResultEnd { outcome, rows: 4 };
            let bytes = end.encode().unwrap();
            assert_eq!(ResultEnd::decode(&bytes).unwrap(), end);
            every_prefix_refuses::<ResultEnd>(&bytes);
        }

        let error = ErrorBody {
            code: ErrorCode::Conflict,
            message: "retry".into(),
        };
        let bytes = error.encode().unwrap();
        assert_eq!(ErrorBody::decode(&bytes).unwrap(), error);
        every_prefix_refuses::<ErrorBody>(&bytes);

        let auth = Auth {
            credential: Credential::WardenCapability(vec![7; 40]),
        };
        let bytes = auth.encode().unwrap();
        assert_eq!(Auth::decode(&bytes).unwrap(), auth);
        every_prefix_refuses::<Auth>(&bytes);
        assert!(!format!("{auth:?}").contains('7'));

        assert_eq!(Empty.encode().unwrap(), Vec::<u8>::new());
        assert_eq!(Empty::decode(&[0]).unwrap_err(), BodyError::TrailingBytes);
    }

    #[test]
    fn noncanonical_spellings_are_refused() {
        // An unsorted map.
        let mut out = Out::default();
        WireValue::Map(vec![
            ("b".into(), WireValue::Null),
            ("a".into(), WireValue::Null),
        ])
        .put(&mut out);
        assert_eq!(
            WireValue::get(&mut In::new(&out.0), 0).unwrap_err(),
            BodyError::Noncanonical
        );
        // A boolean byte that is neither 0 nor 1.
        assert_eq!(
            WireValue::get(&mut In::new(&[tag::BOOL, 2]), 0).unwrap_err(),
            BodyError::Noncanonical
        );
        // Duplicate parameter names refuse on both sides.
        let mut duplicate = execute();
        duplicate.parameters = vec![("a".into(), WireValue::Null), ("a".into(), WireValue::Null)];
        assert_eq!(duplicate.encode().unwrap_err(), BodyError::Noncanonical);
        // An unknown value tag.
        assert_eq!(
            WireValue::get(&mut In::new(&[99]), 0).unwrap_err(),
            BodyError::UnknownTag
        );
        // A zero-denominator average.
        let mut out = Out::default();
        out.u8(tag::AVERAGE);
        out.i128(1);
        out.u64(0);
        assert_eq!(
            WireValue::get(&mut In::new(&out.0), 0).unwrap_err(),
            BodyError::Noncanonical
        );
    }

    #[test]
    fn hostile_counts_and_depth_refuse_before_allocation() {
        // A list declaring u32::MAX items in a five-byte body.
        let mut out = Out::default();
        out.u8(tag::LIST);
        out.u32(u32::MAX);
        assert!(WireValue::get(&mut In::new(&out.0), 0).is_err());
        // A list declaring more items than bytes remain.
        let mut out = Out::default();
        out.u8(tag::LIST);
        out.u32(1000);
        out.u8(tag::NULL);
        assert_eq!(
            WireValue::get(&mut In::new(&out.0), 0).unwrap_err(),
            BodyError::Truncated
        );
        // Nesting one level beyond the bound.
        let mut value = WireValue::Null;
        for _ in 0..=MAX_VALUE_DEPTH {
            value = WireValue::List(vec![value]);
        }
        let mut out = Out::default();
        value.put(&mut out);
        assert_eq!(
            WireValue::get(&mut In::new(&out.0), 0).unwrap_err(),
            BodyError::TooDeep
        );
        let deep = Execute {
            mode: ExecuteMode::Read,
            statement: String::new(),
            parameters: vec![("p".into(), value)],
        };
        assert_eq!(deep.encode().unwrap_err(), BodyError::TooDeep);
    }

    #[test]
    fn float_bits_travel_exactly() {
        for bits in [
            0u64,
            1 << 63,
            f64::NAN.to_bits(),
            f64::INFINITY.to_bits(),
            1,
        ] {
            let value = WireValue::Float(f64::from_bits(bits));
            let mut out = Out::default();
            value.put(&mut out);
            let WireValue::Float(back) = WireValue::get(&mut In::new(&out.0), 0).unwrap() else {
                panic!("float decodes as float");
            };
            assert_eq!(back.to_bits(), bits);
        }
    }
}
