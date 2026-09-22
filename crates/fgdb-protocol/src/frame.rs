use crate::ProtocolError;

pub const PROTOCOL_VERSION: u16 = 1;
pub const TRANSPORT_HEADER_LEN: usize = 34;
const SESSION_HEADER_LEN: usize = TRANSPORT_HEADER_LEN + 32 + 8;
pub const MAX_HEADER_LEN: usize = SESSION_HEADER_LEN + 32 + 32 + 8 + 1 + 32;

/// The first concrete FGP byte profile uses big-endian integers, an inclusive
/// u32 frame length, and the low two flag bits for the header class. All other
/// bits are reserved and rejected. See docs/FABRIC_PROTOCOL.md for exact bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum FrameKind {
    Hello = 0x0001,
    HelloAck = 0x0002,
    Auth = 0x0003,
    AuthOk = 0x0004,
    SelectDatabase = 0x0005,
    Ready = 0x0006,
    AuthRefresh = 0x0007,
    AuthRefreshed = 0x0008,
    Drain = 0x0009,
    Goodbye = 0x000a,
    Error = 0x000b,
    Prepare = 0x0010,
    Prepared = 0x0011,
    Execute = 0x0012,
    QueryCancel = 0x0013,
    ResultChunk = 0x0014,
    ResultEnd = 0x0015,
    ResultAck = 0x0016,
    ResultRelease = 0x0017,
    SnapshotResultChunk = 0x0018,
    SnapshotResultEnd = 0x0019,
    WindowUpdate = 0x001a,
    Ping = 0x0020,
    Pong = 0x0021,
}

impl TryFrom<u16> for FrameKind {
    type Error = ProtocolError;
    fn try_from(value: u16) -> Result<Self, ProtocolError> {
        Ok(match value {
            0x0001 => Self::Hello,
            0x0002 => Self::HelloAck,
            0x0003 => Self::Auth,
            0x0004 => Self::AuthOk,
            0x0005 => Self::SelectDatabase,
            0x0006 => Self::Ready,
            0x0007 => Self::AuthRefresh,
            0x0008 => Self::AuthRefreshed,
            0x0009 => Self::Drain,
            0x000a => Self::Goodbye,
            0x000b => Self::Error,
            0x0010 => Self::Prepare,
            0x0011 => Self::Prepared,
            0x0012 => Self::Execute,
            0x0013 => Self::QueryCancel,
            0x0014 => Self::ResultChunk,
            0x0015 => Self::ResultEnd,
            0x0016 => Self::ResultAck,
            0x0017 => Self::ResultRelease,
            0x0018 => Self::SnapshotResultChunk,
            0x0019 => Self::SnapshotResultEnd,
            0x001a => Self::WindowUpdate,
            0x0020 => Self::Ping,
            0x0021 => Self::Pong,
            _ => return Err(ProtocolError::UnknownFrameKind),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Posture {
    Local,
    Sharded,
}

/// Session-scoped, server-minted 128-bit random stream handle. Zero denotes
/// connection control and can never identify a child. The composition layer
/// obtains the nonce from its capability-scoped RNG, not a counter or row id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(pub [u8; 16]);

impl StreamId {
    pub const CONTROL: Self = Self([0; 16]);
    pub fn is_control(self) -> bool {
        self == Self::CONTROL
    }
}

/// An authenticated transcript binding, not a bearer credential. Only the
/// authentication service may supply this after verifying protected transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    pub transcript: [u8; 32],
    pub auth_generation: u64,
}

/// A public connection header, never a substitute for rereading the selected
/// database's current Operational root and authorization before dispatch/send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadyBinding {
    pub session: SessionBinding,
    pub namespace: [u8; 32],
    pub incarnation: [u8; 32],
    pub service_epoch: u64,
    pub posture: Posture,
    pub authority_commitment: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    Transport,
    Session(SessionBinding),
    Ready(ReadyBinding),
}

impl Binding {
    pub const fn header_len(self) -> usize {
        match self {
            Self::Transport => TRANSPORT_HEADER_LEN,
            Self::Session(_) => SESSION_HEADER_LEN,
            Self::Ready(_) => MAX_HEADER_LEN,
        }
    }
    const fn flags(self) -> u16 {
        match self {
            Self::Transport => 0,
            Self::Session(_) => 1,
            Self::Ready(_) => 2,
        }
    }
    pub const fn session(self) -> Option<SessionBinding> {
        match self {
            Self::Transport => None,
            Self::Session(session) => Some(session),
            Self::Ready(ready) => Some(ready.session),
        }
    }
}

/// The byte budget is immutable after validation. It applies equally to encode
/// and decode. The uncompressed-only profile cannot admit a decompression bomb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLimits {
    max_frame_len: usize,
}

impl FrameLimits {
    pub fn new(max_frame_len: usize) -> Result<Self, ProtocolError> {
        if max_frame_len < MAX_HEADER_LEN || u32::try_from(max_frame_len).is_err() {
            return Err(ProtocolError::InvalidLimit);
        }
        Ok(Self { max_frame_len })
    }
    pub const fn max_frame_len(self) -> usize {
        self.max_frame_len
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub(crate) kind: FrameKind,
    pub(crate) request_id: u64,
    pub(crate) stream_id: StreamId,
    pub(crate) binding: Binding,
    frame_len: usize,
}

impl Header {
    pub const fn kind(self) -> FrameKind { self.kind }
    pub const fn request_id(self) -> u64 { self.request_id }
    pub const fn stream_id(self) -> StreamId { self.stream_id }
    pub const fn binding(self) -> Binding { self.binding }
    pub const fn frame_len(self) -> usize {
        self.frame_len
    }
    pub const fn payload_len(self) -> usize {
        self.frame_len - self.binding.header_len()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    header: Header,
    payload: Vec<u8>,
}

impl Frame {
    pub fn new(
        kind: FrameKind,
        request_id: u64,
        stream_id: StreamId,
        binding: Binding,
        payload: Vec<u8>,
        limits: FrameLimits,
    ) -> Result<Self, ProtocolError> {
        let frame_len = binding
            .header_len()
            .checked_add(payload.len())
            .ok_or(ProtocolError::FrameTooLarge)?;
        if frame_len > limits.max_frame_len {
            return Err(ProtocolError::FrameTooLarge);
        }
        Ok(Self {
            header: Header { kind, request_id, stream_id, binding, frame_len },
            payload,
        })
    }
    pub const fn header(&self) -> &Header {
        &self.header
    }
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
    pub fn encode(&self, limits: FrameLimits) -> Result<Vec<u8>, ProtocolError> {
        if self.header.frame_len > limits.max_frame_len {
            return Err(ProtocolError::FrameTooLarge);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(self.header.frame_len)
            .map_err(|_| ProtocolError::AllocationFailed)?;
        let len = u32::try_from(self.header.frame_len)
            .map_err(|_| ProtocolError::FrameTooLarge)?;
        output.extend_from_slice(&len.to_be_bytes());
        output.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        output.extend_from_slice(&(self.header.kind as u16).to_be_bytes());
        output.extend_from_slice(&self.header.binding.flags().to_be_bytes());
        output.extend_from_slice(&self.header.request_id.to_be_bytes());
        output.extend_from_slice(&self.header.stream_id.0);
        if let Some(session) = self.header.binding.session() {
            output.extend_from_slice(&session.transcript);
            output.extend_from_slice(&session.auth_generation.to_be_bytes());
        }
        if let Binding::Ready(ready) = self.header.binding {
            output.extend_from_slice(&ready.namespace);
            output.extend_from_slice(&ready.incarnation);
            output.extend_from_slice(&ready.service_epoch.to_be_bytes());
            output.push(match ready.posture { Posture::Local => 0, Posture::Sharded => 1 });
            output.extend_from_slice(&ready.authority_commitment);
        }
        output.extend_from_slice(&self.payload);
        Ok(output)
    }
}

impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frame")
            .field("header", &self.header)
            .field("payload_len", &self.payload.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeProgress {
    /// Bytes consumed from THIS input slice. Leave the unconsumed suffix for
    /// the next call, after processing this frame's state transition.
    pub consumed: usize,
    pub frame: Option<Frame>,
}

/// Incremental, single-frame decoder. No read-ahead across a state transition:
/// even a coalesced AUTH/SELECT packet must process AUTH before decoding SELECT.
/// A terminal error poisons the decoder; resynchronizing on untrusted lengths
/// could otherwise reinterpret payload bytes as an authenticated header.
pub struct Decoder {
    limits: FrameLimits,
    fixed: [u8; MAX_HEADER_LEN],
    fixed_used: usize,
    fixed_target: usize,
    declared_len: Option<usize>,
    header: Option<Header>,
    payload: Vec<u8>,
    payload_used: usize,
    poisoned: bool,
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decoder")
            .field("limits", &self.limits)
            .field("header_bytes", &self.fixed_used)
            .field("payload_bytes", &self.payload_used)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl Decoder {
    pub fn new(limits: FrameLimits) -> Self {
        Self {
            limits,
            fixed: [0; MAX_HEADER_LEN],
            fixed_used: 0,
            fixed_target: 4,
            declared_len: None,
            header: None,
            payload: Vec::new(),
            payload_used: 0,
            poisoned: false,
        }
    }
    pub fn buffered_payload_bytes(&self) -> usize {
        self.payload.len()
    }
    pub fn decode(
        &mut self,
        input: &[u8],
        validate_header: impl FnOnce(&Header) -> Result<(), ProtocolError>,
    ) -> Result<DecodeProgress, ProtocolError> {
        if self.poisoned {
            return Err(ProtocolError::DecoderPoisoned);
        }
        let result = self.decode_inner(input, validate_header);
        if result.is_err() {
            self.poisoned = true;
            self.payload = Vec::new();
        }
        result
    }
    fn decode_inner(
        &mut self,
        input: &[u8],
        validate_header: impl FnOnce(&Header) -> Result<(), ProtocolError>,
    ) -> Result<DecodeProgress, ProtocolError> {
        let mut consumed = 0;
        let mut validate_header = Some(validate_header);
        loop {
            if self.header.is_none() {
                let count = (self.fixed_target - self.fixed_used).min(input.len() - consumed);
                self.fixed[self.fixed_used..self.fixed_used + count]
                    .copy_from_slice(&input[consumed..consumed + count]);
                self.fixed_used += count;
                consumed += count;
                if self.fixed_used < self.fixed_target {
                    return Ok(DecodeProgress { consumed, frame: None });
                }
                if self.declared_len.is_none() {
                    let len = usize::try_from(u32::from_be_bytes(self.array::<4>(0)))
                        .map_err(|_| ProtocolError::FrameTooLarge)?;
                    if len < TRANSPORT_HEADER_LEN {
                        return Err(ProtocolError::InvalidLength);
                    }
                    if len > self.limits.max_frame_len {
                        return Err(ProtocolError::FrameTooLarge);
                    }
                    self.declared_len = Some(len);
                    self.fixed_target = TRANSPORT_HEADER_LEN;
                    continue;
                }
                if self.fixed_target == TRANSPORT_HEADER_LEN {
                    if u16::from_be_bytes(self.array::<2>(4)) != PROTOCOL_VERSION {
                        return Err(ProtocolError::UnsupportedVersion);
                    }
                    FrameKind::try_from(u16::from_be_bytes(self.array::<2>(6)))?;
                    let flags = u16::from_be_bytes(self.array::<2>(8));
                    let target = match flags {
                        0 => TRANSPORT_HEADER_LEN,
                        1 => SESSION_HEADER_LEN,
                        2 => MAX_HEADER_LEN,
                        _ => return Err(ProtocolError::UnsupportedFlags),
                    };
                    if self.declared_len.is_none_or(|len| len < target) {
                        return Err(ProtocolError::InvalidLength);
                    }
                    self.fixed_target = target;
                    if target != TRANSPORT_HEADER_LEN {
                        continue;
                    }
                }
                let header = self.parse_header()?;
                // Exactly once, after the entire fixed binding and BEFORE any
                // body reservation/copy, including for an empty payload.
                if let Some(validate) = validate_header.take() {
                    validate(&header)?;
                } else {
                    return Err(ProtocolError::InvalidState);
                }
                let len = header.payload_len();
                self.payload
                    .try_reserve_exact(len)
                    .map_err(|_| ProtocolError::AllocationFailed)?;
                self.payload.resize(len, 0);
                self.header = Some(header);
            }
            let count = (self.payload.len() - self.payload_used).min(input.len() - consumed);
            self.payload[self.payload_used..self.payload_used + count]
                .copy_from_slice(&input[consumed..consumed + count]);
            self.payload_used += count;
            consumed += count;
            if self.payload_used != self.payload.len() {
                return Ok(DecodeProgress { consumed, frame: None });
            }
            let header = self.header.take().ok_or(ProtocolError::InvalidState)?;
            let payload = core::mem::take(&mut self.payload);
            self.fixed_used = 0;
            self.fixed_target = 4;
            self.declared_len = None;
            self.payload_used = 0;
            return Ok(DecodeProgress { consumed, frame: Some(Frame { header, payload }) });
        }
    }
    fn array<const N: usize>(&self, start: usize) -> [u8; N] {
        let mut out = [0; N];
        out.copy_from_slice(&self.fixed[start..start + N]);
        out
    }
    fn parse_header(&self) -> Result<Header, ProtocolError> {
        let flags = u16::from_be_bytes(self.array::<2>(8));
        let binding = if flags == 0 {
            Binding::Transport
        } else {
            let session = SessionBinding {
                transcript: self.array::<32>(34),
                auth_generation: u64::from_be_bytes(self.array::<8>(66)),
            };
            if flags == 1 {
                Binding::Session(session)
            } else {
                Binding::Ready(ReadyBinding {
                    session,
                    namespace: self.array::<32>(74),
                    incarnation: self.array::<32>(106),
                    service_epoch: u64::from_be_bytes(self.array::<8>(138)),
                    posture: match self.fixed[146] {
                        0 => Posture::Local,
                        1 => Posture::Sharded,
                        _ => return Err(ProtocolError::InvalidPosture),
                    },
                    authority_commitment: self.array::<32>(147),
                })
            }
        };
        Ok(Header {
            kind: FrameKind::try_from(u16::from_be_bytes(self.array::<2>(6)))?,
            request_id: u64::from_be_bytes(self.array::<8>(10)),
            stream_id: StreamId(self.array::<16>(18)),
            binding,
            frame_len: self.declared_len.ok_or(ProtocolError::InvalidLength)?,
        })
    }
    /// A clean EOF is possible only between frames. Neither a missing END nor
    /// a partially received frame is successful result completion.
    pub fn finish_eof(&mut self) -> Result<(), ProtocolError> {
        if self.poisoned {
            return Err(ProtocolError::DecoderPoisoned);
        }
        if self.fixed_used != 0 || self.header.is_some() {
            self.poisoned = true;
            self.payload = Vec::new();
            return Err(ProtocolError::TruncatedFrame);
        }
        Ok(())
    }
}
