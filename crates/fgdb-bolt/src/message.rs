//! Bolt connection framing: version negotiation, chunked message transport,
//! and the typed request and response messages of protocol version 5.0.

use crate::packstream::{self, DecodeError, Value, get};

/// The four bytes a Bolt client sends before its version proposals.
pub const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// A negotiated protocol version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u8,
    pub minor: u8,
}

impl Version {
    /// The four response bytes that accept this version.
    #[must_use]
    pub fn response(self) -> [u8; 4] {
        [0, 0, self.minor, self.major]
    }
}

/// The versions this crate implements: 5.0 only. Bolt 5.0 carries
/// authentication in HELLO, UTC-based date-times, and element identities.
pub const SUPPORTED: [Version; 1] = [Version { major: 5, minor: 0 }];

/// Choose from the client's four proposals: the first proposal (in the
/// client's preference order) whose range covers a supported version, and
/// within it the highest such version. `None` means no common version; the
/// server answers four zero bytes and closes.
#[must_use]
pub fn negotiate(proposals: &[u8; 16]) -> Option<Version> {
    for proposal in proposals.as_chunks::<4>().0 {
        let (range, minor, major) = (proposal[1], proposal[2], proposal[3]);
        // A manifest-style proposal (major 0xFF) asks for a negotiation this
        // server does not offer; plain version proposals follow it.
        if major == 0xFF {
            continue;
        }
        let lowest = minor.saturating_sub(range);
        if let Some(version) = SUPPORTED
            .iter()
            .filter(|v| v.major == major && (lowest..=minor).contains(&v.minor))
            .max()
        {
            return Some(*version);
        }
    }
    None
}

/// Largest payload of one chunk.
pub const MAX_CHUNK: usize = 0xFFFF;

/// Append `message` as chunks of at most [`MAX_CHUNK`] bytes and the
/// zero-length chunk that ends it.
pub fn frame(message: &[u8], out: &mut Vec<u8>) {
    for chunk in message.chunks(MAX_CHUNK) {
        out.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&[0, 0]);
}

/// Reassembles messages from received bytes. A zero chunk with no data
/// before it is a keep-alive (NOOP) and yields nothing.
#[derive(Debug)]
pub struct Dechunker {
    pending: Vec<u8>,
    message: Vec<u8>,
    max_message: usize,
}

/// A message larger than the bound the server accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageTooLarge {
    pub limit: usize,
}

impl Dechunker {
    #[must_use]
    pub fn new(max_message: usize) -> Self {
        Self {
            pending: Vec::new(),
            message: Vec::new(),
            max_message,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The next complete message, if the received bytes hold one.
    pub fn next_message(&mut self) -> Result<Option<Vec<u8>>, MessageTooLarge> {
        let mut at = 0;
        let result = loop {
            let Some(header) = self.pending.get(at..at + 2) else {
                break Ok(None);
            };
            let len = usize::from(u16::from_be_bytes([header[0], header[1]]));
            if len == 0 {
                at += 2;
                if self.message.is_empty() {
                    continue;
                }
                break Ok(Some(core::mem::take(&mut self.message)));
            }
            let Some(chunk) = self.pending.get(at + 2..at + 2 + len) else {
                break Ok(None);
            };
            if self.message.len() + len > self.max_message {
                break Err(MessageTooLarge {
                    limit: self.max_message,
                });
            }
            self.message.extend_from_slice(chunk);
            at += 2 + len;
        };
        self.pending.drain(..at);
        result
    }
}

pub type Map = Vec<(String, Value)>;

/// A client request of protocol 5.0.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Hello {
        extra: Map,
    },
    Goodbye,
    Reset,
    Run {
        query: String,
        parameters: Map,
        extra: Map,
    },
    Begin {
        extra: Map,
    },
    Commit,
    Rollback,
    Discard {
        n: i64,
        qid: i64,
    },
    Pull {
        n: i64,
        qid: i64,
    },
    Route {
        routing: Map,
        bookmarks: Vec<String>,
        extra: Map,
    },
}

impl Request {
    /// The message name, for logs and refusals.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "HELLO",
            Self::Goodbye => "GOODBYE",
            Self::Reset => "RESET",
            Self::Run { .. } => "RUN",
            Self::Begin { .. } => "BEGIN",
            Self::Commit => "COMMIT",
            Self::Rollback => "ROLLBACK",
            Self::Discard { .. } => "DISCARD",
            Self::Pull { .. } => "PULL",
            Self::Route { .. } => "ROUTE",
        }
    }
}

/// Why a message is not a request this protocol version defines.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestError {
    Decode(DecodeError),
    /// Not a structure.
    NotAMessage,
    /// A structure tag this version has no request for.
    UnknownTag(u8),
    /// A known request with the wrong fields.
    Malformed(&'static str),
}

impl core::fmt::Display for RequestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Decode(error) => error.fmt(f),
            Self::NotAMessage => f.write_str("a Bolt message must be a structure"),
            Self::UnknownTag(tag) => write!(f, "unsupported Bolt message 0x{tag:02X}"),
            Self::Malformed(what) => write!(f, "malformed {what} message"),
        }
    }
}
impl core::error::Error for RequestError {}

fn map(value: Value, what: &'static str) -> Result<Map, RequestError> {
    match value {
        Value::Map(entries) => Ok(entries),
        _ => Err(RequestError::Malformed(what)),
    }
}

/// `n` and `qid` of PULL/DISCARD: -1 means all rows and the last query.
fn stream_window(extra: &Map) -> (i64, i64) {
    let n = get(extra, "n").and_then(Value::as_int).unwrap_or(-1);
    let qid = get(extra, "qid").and_then(Value::as_int).unwrap_or(-1);
    (n, qid)
}

/// Decode one dechunked message.
pub fn decode_request(message: &[u8]) -> Result<Request, RequestError> {
    let Value::Struct { tag, fields } =
        packstream::decode(message).map_err(RequestError::Decode)?
    else {
        return Err(RequestError::NotAMessage);
    };
    let mut fields = fields.into_iter();
    let mut next = || fields.next();
    let request = match tag {
        0x01 => Request::Hello {
            extra: map(next().ok_or(RequestError::Malformed("HELLO"))?, "HELLO")?,
        },
        0x02 => Request::Goodbye,
        0x0F => Request::Reset,
        0x10 => {
            let Some(Value::String(query)) = next() else {
                return Err(RequestError::Malformed("RUN"));
            };
            let parameters = map(next().unwrap_or(Value::Map(Vec::new())), "RUN")?;
            let extra = map(next().unwrap_or(Value::Map(Vec::new())), "RUN")?;
            Request::Run {
                query,
                parameters,
                extra,
            }
        }
        0x11 => Request::Begin {
            extra: map(next().unwrap_or(Value::Map(Vec::new())), "BEGIN")?,
        },
        0x12 => Request::Commit,
        0x13 => Request::Rollback,
        0x2F | 0x3F => {
            let extra = map(next().unwrap_or(Value::Map(Vec::new())), "PULL")?;
            let (n, qid) = stream_window(&extra);
            if n == 0 || n < -1 {
                return Err(RequestError::Malformed("PULL"));
            }
            if tag == 0x2F {
                Request::Discard { n, qid }
            } else {
                Request::Pull { n, qid }
            }
        }
        0x66 => {
            let routing = map(next().unwrap_or(Value::Map(Vec::new())), "ROUTE")?;
            let bookmarks = match next() {
                Some(Value::List(items)) => items
                    .into_iter()
                    .map(|item| match item {
                        Value::String(text) => Ok(text),
                        _ => Err(RequestError::Malformed("ROUTE")),
                    })
                    .collect::<Result<_, _>>()?,
                None | Some(Value::Null) => Vec::new(),
                Some(_) => return Err(RequestError::Malformed("ROUTE")),
            };
            let extra = match next() {
                Some(Value::Map(entries)) => entries,
                // 4.3's form named the database directly.
                Some(Value::String(db)) => vec![("db".to_owned(), Value::String(db))],
                None | Some(Value::Null) => Vec::new(),
                Some(_) => return Err(RequestError::Malformed("ROUTE")),
            };
            Request::Route {
                routing,
                bookmarks,
                extra,
            }
        }
        other => return Err(RequestError::UnknownTag(other)),
    };
    Ok(request)
}

/// A server response.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Success(Map),
    Record(Vec<Value>),
    Ignored,
    Failure { code: String, message: String },
}

impl Response {
    /// The response as one framed message.
    pub fn frame(&self, out: &mut Vec<u8>) {
        let value = match self {
            Self::Success(metadata) => Value::Struct {
                tag: 0x70,
                fields: vec![Value::Map(metadata.clone())],
            },
            Self::Record(values) => Value::Struct {
                tag: 0x71,
                fields: vec![Value::List(values.clone())],
            },
            Self::Ignored => Value::Struct {
                tag: 0x7E,
                fields: Vec::new(),
            },
            Self::Failure { code, message } => Value::Struct {
                tag: 0x7F,
                fields: vec![Value::Map(vec![
                    ("code".to_owned(), Value::string(code.clone())),
                    ("message".to_owned(), Value::string(message.clone())),
                ])],
            },
        };
        let mut message = Vec::new();
        packstream::encode(&value, &mut message);
        frame(&message, out);
    }
}

/// Bolt 5.0 graph and temporal structures.
pub mod structure {
    use super::Value;

    /// `Node{id, labels, properties, element_id}` (tag `N`).
    #[must_use]
    pub fn node(
        id: i64,
        labels: Vec<String>,
        properties: Vec<(String, Value)>,
        element_id: String,
    ) -> Value {
        Value::Struct {
            tag: 0x4E,
            fields: vec![
                Value::Int(id),
                Value::List(labels.into_iter().map(Value::String).collect()),
                Value::Map(properties),
                Value::String(element_id),
            ],
        }
    }

    /// `Relationship{id, start, end, type, properties, element_id,
    /// start_element_id, end_element_id}` (tag `R`). The element identity
    /// array is ordered relationship, start node, end node.
    #[must_use]
    pub fn relationship(
        id: i64,
        start: i64,
        end: i64,
        kind: String,
        properties: Vec<(String, Value)>,
        element_ids: [String; 3],
    ) -> Value {
        let [element_id, start_element_id, end_element_id] = element_ids;
        Value::Struct {
            tag: 0x52,
            fields: vec![
                Value::Int(id),
                Value::Int(start),
                Value::Int(end),
                Value::String(kind),
                Value::Map(properties),
                Value::String(element_id),
                Value::String(start_element_id),
                Value::String(end_element_id),
            ],
        }
    }

    /// A path's `UnboundRelationship{id, type, properties, element_id}`
    /// (tag `r`); its endpoints are supplied by the path's traversal indices.
    #[must_use]
    pub fn unbound_relationship(
        id: i64,
        kind: String,
        properties: Vec<(String, Value)>,
        element_id: String,
    ) -> Value {
        Value::Struct {
            tag: 0x72,
            fields: vec![
                Value::Int(id),
                Value::String(kind),
                Value::Map(properties),
                Value::String(element_id),
            ],
        }
    }

    /// `Path{nodes, relationships, indices}` (tag `P`). The first node is
    /// the start; indices alternate signed one-based relationship positions
    /// and nonnegative zero-based node positions.
    #[must_use]
    pub fn path(nodes: Vec<Value>, relationships: Vec<Value>, indices: Vec<i64>) -> Value {
        Value::Struct {
            tag: 0x50,
            fields: vec![
                Value::List(nodes),
                Value::List(relationships),
                Value::List(indices.into_iter().map(Value::Int).collect()),
            ],
        }
    }

    /// `DateTime{seconds, nanoseconds, tz_offset_seconds}` (tag `I`):
    /// seconds and nanoseconds since the Unix epoch in UTC.
    #[must_use]
    pub fn date_time(seconds: i64, nanoseconds: i64, offset_seconds: i64) -> Value {
        Value::Struct {
            tag: 0x49,
            fields: vec![
                Value::Int(seconds),
                Value::Int(nanoseconds),
                Value::Int(offset_seconds),
            ],
        }
    }

    /// `DateTimeZoneId{seconds, nanoseconds, tz_id}` (tag `i`).
    #[must_use]
    pub fn date_time_zone(seconds: i64, nanoseconds: i64, zone: String) -> Value {
        Value::Struct {
            tag: 0x69,
            fields: vec![
                Value::Int(seconds),
                Value::Int(nanoseconds),
                Value::String(zone),
            ],
        }
    }

    pub const DATE_TIME: u8 = 0x49;
    pub const DATE_TIME_ZONE_ID: u8 = 0x69;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bolt_5_graph_structures_match_wire_goldens_and_decode_a_reverse_path() {
        // Bolt 5 adds element identities to bound and unbound relationships;
        // path indices still encode traversal direction independently of them.
        let properties = vec![("p".into(), Value::Int(7))];
        let relationship = structure::relationship(
            9,
            1,
            2,
            "R".into(),
            properties.clone(),
            ["I".into(), "A".into(), "B".into()],
        );
        let unbound = structure::unbound_relationship(9, "R".into(), properties, "I".into());
        let path = structure::path(
            vec![
                structure::node(2, vec![], vec![], "B".into()),
                structure::node(1, vec![], vec![], "A".into()),
            ],
            vec![unbound.clone()],
            vec![-1, 1],
        );
        let goldens: [(&Value, &[u8]); 3] = [
            (
                &relationship,
                b"\xB8\x52\x09\x01\x02\x81R\xA1\x81p\x07\x81I\x81A\x81B",
            ),
            (&unbound, b"\xB4\x72\x09\x81R\xA1\x81p\x07\x81I"),
            (
                &path,
                b"\xB3\x50\x92\xB4\x4E\x02\x90\xA0\x81B\xB4\x4E\x01\x90\xA0\x81A\x91\xB4\x72\x09\x81R\xA1\x81p\x07\x81I\x92\xFF\x01",
            ),
        ];
        for (value, golden) in goldens {
            let mut bytes = Vec::new();
            packstream::encode(value, &mut bytes);
            assert_eq!(bytes, golden);
            assert_eq!(&packstream::decode(golden).unwrap(), value);
        }

        fn fields(value: &Value, expected_tag: u8) -> &[Value] {
            let Value::Struct { tag, fields } = value else {
                panic!("expected graph structure");
            };
            assert_eq!(*tag, expected_tag);
            fields
        }

        // A driver can reconstruct the same directed relationship from the
        // unbound path entry even though this path walks it from B to A.
        let decoded = packstream::decode(goldens[2].1).unwrap();
        let [
            Value::List(nodes),
            Value::List(relationships),
            Value::List(indices),
        ] = fields(&decoded, 0x50)
        else {
            panic!("expected three path lists");
        };
        let [Value::Int(relationship_index), Value::Int(node_index)] = indices.as_slice() else {
            panic!("expected one path step");
        };
        assert_eq!(*relationship_index, -1);
        let unbound_index = usize::try_from(relationship_index.unsigned_abs() - 1).unwrap();
        let next_index = usize::try_from(*node_index).unwrap();
        let previous = fields(&nodes[0], 0x4E);
        let next = fields(&nodes[next_index], 0x4E);
        let unbound = fields(&relationships[unbound_index], 0x72);
        let bound = fields(&relationship, 0x52);
        let (source, target) = if *relationship_index > 0 {
            (previous, next)
        } else {
            (next, previous)
        };
        assert_eq!((&unbound[0], &unbound[3]), (&bound[0], &bound[5]));
        assert_eq!((&source[0], &target[0]), (&bound[1], &bound[2]));
        assert_eq!((&source[3], &target[3]), (&bound[6], &bound[7]));
    }

    #[test]
    fn negotiation_picks_5_0_from_a_range_and_skips_manifest_proposals() {
        // The Python driver 6.x proposal set.
        let proposals = *b"\x00\x00\x01\xff\x00\x08\x08\x05\x00\x02\x04\x04\x00\x00\x00\x03";
        assert_eq!(negotiate(&proposals), Some(Version { major: 5, minor: 0 }));
        assert_eq!(Version { major: 5, minor: 0 }.response(), [0, 0, 0, 5]);
        let only_old = *b"\x00\x00\x04\x04\x00\x00\x00\x03\x00\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(negotiate(&only_old), None);
        let exact = *b"\x00\x00\x00\x05\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(negotiate(&exact), Some(Version { major: 5, minor: 0 }));
    }

    #[test]
    fn messages_survive_chunking_split_reads_and_keep_alives() {
        let payload: Vec<u8> = (0..200_000_u32).map(|i| i as u8).collect();
        let mut wire = vec![0, 0]; // a leading keep-alive
        frame(&payload, &mut wire);
        frame(b"second", &mut wire);
        let mut dechunker = Dechunker::new(1 << 20);
        let mut received = Vec::new();
        for byte in wire.chunks(7) {
            dechunker.push(byte);
            while let Some(message) = dechunker.next_message().unwrap() {
                received.push(message);
            }
        }
        assert_eq!(received, vec![payload, b"second".to_vec()]);
        let mut small = Dechunker::new(4);
        let mut big = Vec::new();
        frame(b"too long", &mut big);
        small.push(&big);
        assert_eq!(small.next_message(), Err(MessageTooLarge { limit: 4 }));
    }

    #[test]
    fn requests_decode_and_responses_encode() {
        let run = Value::Struct {
            tag: 0x10,
            fields: vec![
                Value::string("RETURN 1"),
                Value::Map(vec![("x".into(), Value::Int(1))]),
                Value::Map(vec![("db".into(), Value::string("social"))]),
            ],
        };
        let mut bytes = Vec::new();
        packstream::encode(&run, &mut bytes);
        assert_eq!(
            decode_request(&bytes),
            Ok(Request::Run {
                query: "RETURN 1".into(),
                parameters: vec![("x".into(), Value::Int(1))],
                extra: vec![("db".into(), Value::string("social"))],
            })
        );
        let pull = Value::Struct {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Int(1000))])],
        };
        bytes.clear();
        packstream::encode(&pull, &mut bytes);
        assert_eq!(
            decode_request(&bytes),
            Ok(Request::Pull { n: 1000, qid: -1 })
        );
        bytes.clear();
        packstream::encode(
            &Value::Struct {
                tag: 0x55,
                fields: vec![],
            },
            &mut bytes,
        );
        assert_eq!(decode_request(&bytes), Err(RequestError::UnknownTag(0x55)));

        let mut out = Vec::new();
        Response::Failure {
            code: "Neo.ClientError.Statement.SyntaxError".into(),
            message: "no".into(),
        }
        .frame(&mut out);
        let mut dechunker = Dechunker::new(1 << 16);
        dechunker.push(&out);
        let message = dechunker.next_message().unwrap().unwrap();
        let Value::Struct { tag, fields } = packstream::decode(&message).unwrap() else {
            panic!("a structure");
        };
        assert_eq!(tag, 0x7F);
        assert_eq!(
            get(fields[0].as_map().unwrap(), "code").and_then(Value::as_str),
            Some("Neo.ClientError.Statement.SyntaxError")
        );
    }
}
