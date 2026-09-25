use crate::{Binding, FrameKind, Header, ProtocolError, ReadyBinding, SessionBinding, StreamId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// An identity for one queued send, scoped to its allocating connection.
/// Dropping a ticket does not discharge the outstanding obligation.
#[derive(Debug)]
pub struct SendTicket {
    owner: Arc<()>,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendTerminus {
    Sent,
    CancelledBeforeWrite,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    TransportEstablished,
    VersionNegotiated,
    Authenticated,
    Ready,
    Draining,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildKind {
    Query,
    Transaction,
    Subscription,
    ArtifactOutput,
    ProtectedError,
}

/// These are reports from the owning lifecycle, NOT constructors for durable
/// evidence. The composition layer must obtain the corresponding rooted
/// handoff/terminal evidence before reporting one. There is deliberately no
/// "socket closed", "timed out", "dropped", or "all bytes written" arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildTerminus {
    EphemeralCancelledBeforeRegistration,
    TransactionOwnershipDetached,
    SemanticTerminalDurable,
    AdmittedRecoveryRooted,
    ResultDurablyDetached,
    ArtifactOutputDurablyDetached,
    SubscriptionDurablyDetached,
    ProtectedErrorDurablyOwned,
}

#[derive(Clone, Copy, Debug)]
struct Child {
    kind: ChildKind,
    generation: u64,
}

/// Transport state and ephemeral ownership only. This type owns NO database,
/// durable cursor, result-retention record, release receipt, or authentication
/// secret. State transitions requiring security decisions accept the binding
/// returned by that service; this module never manufactures one from a name.
#[derive(Debug)]
pub struct Connection {
    phase: Phase,
    binding: Binding,
    maximum_children: usize,
    maximum_sends: usize,
    next_child_generation: u64,
    children: BTreeMap<StreamId, Child>,
    send_owner: Arc<()>,
    next_send_sequence: u64,
    sends: BTreeSet<u64>,
    drain_cutoff: Option<u64>,
}

impl Connection {
    pub fn new(maximum_children: usize, maximum_sends: usize) -> Result<Self, ProtocolError> {
        if maximum_children == 0 || maximum_sends == 0 {
            return Err(ProtocolError::InvalidLimit);
        }
        Ok(Self {
            phase: Phase::TransportEstablished,
            binding: Binding::Transport,
            maximum_children,
            maximum_sends,
            next_child_generation: 0,
            children: BTreeMap::new(),
            send_owner: Arc::new(()),
            next_send_sequence: 0,
            sends: BTreeSet::new(),
            drain_cutoff: None,
        })
    }
    pub const fn phase(&self) -> Phase {
        self.phase
    }
    pub const fn binding(&self) -> Binding {
        self.binding
    }
    pub fn children_in_flight(&self) -> usize {
        self.children.len()
    }
    pub fn sends_in_flight(&self) -> usize {
        self.sends.len()
    }
    pub const fn drain_cutoff(&self) -> Option<u64> {
        self.drain_cutoff
    }

    /// Run this as Decoder's header validator. Equality is checked before any
    /// database/body parsing. The service must ALSO check live authority,
    /// revocation and current root/epoch before dispatch or protected output.
    pub fn validate_client_header(&self, header: &Header) -> Result<(), ProtocolError> {
        if header.binding != self.binding {
            return Err(ProtocolError::InvalidBinding);
        }
        if header.request_id == 0 {
            return Err(ProtocolError::InvalidRequest);
        }
        let legal = match header.kind {
            FrameKind::Hello => self.phase == Phase::TransportEstablished,
            FrameKind::Auth => self.phase == Phase::VersionNegotiated,
            // ubs:ignore -- connection-phase enum state, not secret material.
            FrameKind::SelectDatabase => self.phase == Phase::Authenticated,
            FrameKind::AuthRefresh => matches!(self.phase, Phase::Authenticated | Phase::Ready),
            FrameKind::Prepare | FrameKind::Execute => self.phase == Phase::Ready,
            FrameKind::QueryCancel
            | FrameKind::ResultAck
            | FrameKind::ResultRelease
            | FrameKind::WindowUpdate => matches!(self.phase, Phase::Ready | Phase::Draining),
            FrameKind::Ping => matches!(self.phase, Phase::Authenticated | Phase::Ready),
            FrameKind::Drain => matches!(
                self.phase,
                Phase::Authenticated | Phase::Ready | Phase::Draining
            ),
            // A client cannot impersonate a server reply, including ERROR.
            _ => false,
        };
        if !legal {
            return Err(ProtocolError::InvalidState);
        }
        let needs_stream = matches!(
            header.kind,
            FrameKind::QueryCancel
                | FrameKind::ResultAck
                | FrameKind::ResultRelease
                | FrameKind::WindowUpdate
        );
        if needs_stream == header.stream_id.is_control() {
            return Err(ProtocolError::InvalidStream);
        }
        if matches!(
            header.kind,
            FrameKind::QueryCancel | FrameKind::WindowUpdate
        ) && !self.children.contains_key(&header.stream_id)
        {
            return Err(ProtocolError::InvalidStream);
        }
        // ACK/release may address an independently retained owner, not a local
        // child. Only the durable owner service can validate those selectors.
        Ok(())
    }

    /// Invoke only after validating HELLO's version/mechanics and freezing the
    /// HELLO_ACK transcript. No database selector or database limit is accepted.
    pub fn negotiated(&mut self) -> Result<(), ProtocolError> {
        if self.phase != Phase::TransportEstablished {
            return Err(ProtocolError::InvalidState);
        }
        self.phase = Phase::VersionNegotiated;
        Ok(())
    }

    /// Invoke only with the transcript-bound session from protected-transport
    /// authentication. Neither the codec nor HELLO produces this value.
    pub fn authenticated(&mut self, session: SessionBinding) -> Result<(), ProtocolError> {
        if self.phase != Phase::VersionNegotiated {
            return Err(ProtocolError::InvalidState);
        }
        self.binding = Binding::Session(session);
        self.phase = Phase::Authenticated;
        Ok(())
    }

    /// Selection is one-shot for this Ready binding. Operational-root checks,
    /// uniform nonexistent/unauthorized refusal, and promotion validation are
    /// required at the caller. In-band database or posture switching is absent.
    pub fn selected(&mut self, ready: ReadyBinding) -> Result<(), ProtocolError> {
        if self.phase != Phase::Authenticated {
            return Err(ProtocolError::InvalidState);
        }
        // Checks the host-built ReadyBinding against this connection's own session.
        // ubs:ignore -- SessionBinding is a transcript binding, not a bearer credential.
        if self.binding != Binding::Session(ready.session) {
            return Err(ProtocolError::InvalidBinding);
        }
        self.binding = Binding::Ready(ready);
        self.phase = Phase::Ready;
        Ok(())
    }

    /// The authority service must prove non-widening BEFORE this transition.
    /// This advances only auth_context_generation and fences old headers. It
    /// cannot change transcript, database, incarnation, posture, or service.
    pub fn authority_narrowed(&mut self) -> Result<SessionBinding, ProtocolError> {
        if !matches!(self.phase, Phase::Authenticated | Phase::Ready) {
            return Err(ProtocolError::InvalidState);
        }
        let mut session = self
            .binding
            .session()
            .ok_or(ProtocolError::InvalidBinding)?;
        session.auth_generation = session
            .auth_generation
            .checked_add(1)
            .ok_or(ProtocolError::GenerationExhausted)?;
        self.binding = match self.binding {
            Binding::Session(_) => Binding::Session(session),
            Binding::Ready(mut ready) => {
                ready.session = session;
                Binding::Ready(ready)
            }
            Binding::Transport => return Err(ProtocolError::InvalidBinding),
        };
        Ok(session)
    }

    pub fn admit_child(
        &mut self,
        stream_id: StreamId,
        kind: ChildKind,
    ) -> Result<u64, ProtocolError> {
        if self.phase != Phase::Ready {
            return Err(ProtocolError::InvalidState);
        }
        if stream_id.is_control() || self.children.contains_key(&stream_id) {
            return Err(ProtocolError::InvalidStream);
        }
        if self.children.len() >= self.maximum_children {
            return Err(ProtocolError::StreamLimit);
        }
        let generation = self
            .next_child_generation
            .checked_add(1)
            .ok_or(ProtocolError::GenerationExhausted)?;
        self.children.insert(stream_id, Child { kind, generation });
        self.next_child_generation = generation;
        Ok(generation)
    }

    /// Finish exactly the admitted child generation, not a later reuse of its
    /// stream id. Wrong-kind/wrong-generation reports never remove an owner.
    pub fn child_terminal(
        &mut self,
        stream_id: StreamId,
        generation: u64,
        terminus: ChildTerminus,
    ) -> Result<(), ProtocolError> {
        let child = self
            .children
            .get(&stream_id)
            .ok_or(ProtocolError::InvalidStream)?;
        if child.generation != generation {
            return Err(ProtocolError::InvalidStream);
        }
        let legal = match terminus {
            ChildTerminus::EphemeralCancelledBeforeRegistration => true,
            ChildTerminus::TransactionOwnershipDetached => child.kind == ChildKind::Transaction,
            ChildTerminus::SemanticTerminalDurable | ChildTerminus::AdmittedRecoveryRooted => {
                matches!(child.kind, ChildKind::Query | ChildKind::Transaction)
            }
            ChildTerminus::ResultDurablyDetached => child.kind == ChildKind::Query,
            ChildTerminus::ArtifactOutputDurablyDetached => child.kind == ChildKind::ArtifactOutput,
            ChildTerminus::SubscriptionDurablyDetached => child.kind == ChildKind::Subscription,
            ChildTerminus::ProtectedErrorDurablyOwned => true,
        };
        if !legal {
            return Err(ProtocolError::InvalidState);
        }
        self.children.remove(&stream_id);
        Ok(())
    }

    /// A queued transport obligation must be counted before its task is exposed.
    /// No new sends can be queued after the transport has closed.
    pub fn queue_send(&mut self) -> Result<SendTicket, ProtocolError> {
        if self.phase == Phase::Closed {
            return Err(ProtocolError::InvalidState);
        }
        if self.sends.len() == self.maximum_sends {
            return Err(ProtocolError::SendLimit);
        }
        let sequence = self
            .next_send_sequence
            .checked_add(1)
            .ok_or(ProtocolError::GenerationExhausted)?;
        self.sends.insert(sequence);
        self.next_send_sequence = sequence;
        Ok(SendTicket {
            owner: Arc::clone(&self.send_owner),
            sequence,
        })
    }
    /// Report only Sent, CancelledBeforeWrite, or Failed. This discharges a
    /// transport obligation and has no authority to ACK/release a result.
    pub fn send_terminal(
        &mut self,
        ticket: &SendTicket,
        _terminus: SendTerminus,
    ) -> Result<(), ProtocolError> {
        if !Arc::ptr_eq(&self.send_owner, &ticket.owner) || !self.sends.remove(&ticket.sequence) {
            return Err(ProtocolError::InvalidState);
        }
        Ok(())
    }
    pub fn begin_drain(&mut self) -> Result<(), ProtocolError> {
        if self.phase == Phase::Closed {
            return Err(ProtocolError::InvalidState);
        }
        if self.phase != Phase::Draining {
            self.drain_cutoff = Some(self.next_child_generation);
            self.phase = Phase::Draining;
        }
        Ok(())
    }
    /// Success authorizes only GOODBYE, not durable owner retirement. Detached
    /// results/cursors do not appear in this map and cannot block shutdown.
    pub fn complete_drain(&mut self) -> Result<(), ProtocolError> {
        if self.phase != Phase::Draining {
            return Err(ProtocolError::InvalidState);
        }
        if !self.children.is_empty() || !self.sends.is_empty() {
            return Err(ProtocolError::DrainIncomplete);
        }
        self.phase = Phase::Closed;
        Ok(())
    }
}
