//! Fabric's transport-independent FGP mechanisms (plan Appendix D).
//!
//! This crate implements framing, binding/order validation, send accounting,
//! and drain bookkeeping. It does not authenticate a principal, authorize a
//! database, manufacture Operational-root evidence, or release durable output.
//! Those decisions belong to the owning security/durability services. A valid
//! frame is not an authorization permit. In particular, completing a write or
//! dropping a connection is never a result ACK.
//!
//! The codec validates the fixed header, including its session/Ready binding,
//! before reserving payload memory. The same codec and state machine are used
//! by native sockets and surface adapters. Compression and FEC are unavailable
//! in this byte profile; unknown tags and flag bits fail closed.

#![forbid(unsafe_code)]

mod connection;
mod flow;
mod frame;

/// Asupersync-backed, cancellation-resumable stream I/O. The pure codec and
/// state machines remain usable without enabling a runtime dependency.
#[cfg(feature = "transport")]
pub mod transport;

pub use connection::{ChildKind, ChildTerminus, Connection, Phase, SendTerminus, SendTicket};
pub use flow::{CreditUpdate, FlowWindow, Reservation, SendCost, SendState, WindowStatus};
pub use frame::{
    Binding, DecodeProgress, Decoder, Frame, FrameKind, FrameLimits, Header, MAX_HEADER_LEN,
    PROTOCOL_VERSION, Posture, ReadyBinding, SessionBinding, StreamId, TRANSPORT_HEADER_LEN,
};

/// Stable, data-independent failures. None contains payloads or hidden lookup
/// results. Adapters must not turn an engine's Debug output into a wire error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    InvalidLimit,
    FrameTooLarge,
    InvalidLength,
    UnsupportedVersion,
    UnknownFrameKind,
    UnsupportedFlags,
    InvalidBinding,
    InvalidPosture,
    InvalidState,
    InvalidRequest,
    InvalidStream,
    StreamLimit,
    SendLimit,
    GenerationExhausted,
    CreditExceeded,
    InvalidCreditUpdate,
    CreditOverflow,
    AllocationFailed,
    TruncatedFrame,
    DecoderPoisoned,
    DrainIncomplete,
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InvalidLimit => "invalid protocol limit",
            Self::FrameTooLarge => "frame exceeds negotiated limit",
            Self::InvalidLength => "invalid frame length",
            Self::UnsupportedVersion => "unsupported protocol version",
            Self::UnknownFrameKind => "unknown frame kind",
            Self::UnsupportedFlags => "unsupported frame flags",
            Self::InvalidBinding => "invalid connection binding",
            Self::InvalidPosture => "invalid database posture",
            Self::InvalidState => "frame is not legal in the current connection state",
            Self::InvalidRequest => "invalid request identifier",
            Self::InvalidStream => "invalid stream identifier",
            Self::StreamLimit => "connection stream limit reached",
            Self::SendLimit => "connection send limit reached",
            Self::GenerationExhausted => "connection generation exhausted",
            Self::CreditExceeded => "insufficient flow credit",
            Self::InvalidCreditUpdate => "invalid flow-credit update",
            Self::CreditOverflow => "flow-credit limit exceeded",
            Self::AllocationFailed => "frame memory reservation refused",
            Self::TruncatedFrame => "connection ended inside a frame",
            Self::DecoderPoisoned => "decoder cannot be reused after a protocol failure",
            Self::DrainIncomplete => "connection drain still has outstanding obligations",
        })
    }
}

impl core::error::Error for ProtocolError {}
