//! Bounded optimistic append windows over the existing Raft messages.
//!
//! Issuance is not acknowledgement. A successful later append proves its exact
//! prefix; a rejection invalidates every speculative request for that peer and
//! returns to one-at-a-time probing. Snapshot offers occupy the whole peer lane.
//! Retransmissions retain request identities and cannot become fresh read probes.

use super::{Error, InFlight, MemberId, Message, Output, Raft, Role};

const MAX_APPEND_WINDOW: usize = 64;

impl InFlight {
    pub(super) fn last(&self) -> u64 {
        match self {
            Self::Append { last, .. } => *last,
            Self::Snapshot { snapshot, .. } => snapshot.index(),
        }
    }
}

impl<C: Clone + Eq> Raft<C> {
    /// Set the per-follower append window while this member is a follower.
    /// This is volatile transport admission, not membership or durable state.
    /// Recovery starts at one; the host explicitly reapplies its chosen bound.
    /// Changing it never discards requests from a live leader or pending write.
    ///
    /// At most `maximum` append RPCs per peer are retained, each containing at
    /// most `Limits::max_append_entries` commands. Command byte/CPU limits and
    /// downstream output-queue admission remain the host's responsibility.
    pub fn configure_append_pipeline(&mut self, maximum: usize) -> Result<(), Error> {
        self.available()?;
        if maximum == 0 || maximum > MAX_APPEND_WINDOW {
            return Err(Error::InvalidLimits);
        }
        if self.role != Role::Follower || !self.progress.is_empty() {
            return Err(Error::PipelineConfigurationBusy);
        }
        self.append_window = maximum;
        Ok(())
    }

    pub fn append_pipeline_window(&self) -> usize {
        self.append_window
    }

    /// Whether the live kernel would accept this exact current-term append
    /// response. This is a local history query, NOT peer authentication or read
    /// authority. The caller must still run the complete envelope through step
    /// and its publication gate before using a successful response as evidence.
    /// Retired, compacted and rejection-invalidated requests return false.
    pub fn pending_append_reply(
        &self,
        from: MemberId,
        term: u64,
        request: u64,
    ) -> Result<bool, Error> {
        self.available()?;
        Ok(self.role == Role::Leader
            && term == self.state.term
            && self.pending_append(from, request).is_some())
    }

    pub(super) fn pending_append(&self, peer: MemberId, request: u64) -> Option<&InFlight> {
        self.progress.get(&peer)?.in_flight.iter().find(|flight| {
            matches!(flight, InFlight::Append { request: issued, .. } if *issued == request)
        })
    }

    pub(super) fn send_append(
        &mut self,
        peer: MemberId,
        output: &mut Output<C>,
        retry: bool,
    ) -> Result<(), Error> {
        let Some(progress) = self.progress.get(&peer) else {
            return Ok(());
        };
        if retry {
            // Metadata only, bounded by MAX_APPEND_WINDOW. Commands are borrowed
            // from the retained log when constructing each released envelope.
            let retries: Vec<_> = progress.in_flight.iter().cloned().collect();
            for flight in &retries {
                self.emit_flight(peer, flight, output)?;
            }
        }
        loop {
            let progress = self
                .progress
                .get(&peer)
                .ok_or(Error::InvalidRecoveryState)?;
            let capacity = if progress.probing {
                1
            } else {
                self.append_window
            };
            if progress.in_flight.len() >= capacity
                || matches!(progress.in_flight.front(), Some(InFlight::Snapshot { .. }))
                || (progress.next > self.last_index() && !progress.in_flight.is_empty())
            {
                return Ok(());
            }
            let next = progress.next;
            self.request = self.request.checked_add(1).ok_or(Error::CounterExhausted)?;
            let flight = if next <= self.state.base_index() {
                InFlight::Snapshot {
                    request: self.request,
                    snapshot: self
                        .state
                        .snapshot
                        .clone()
                        .ok_or(Error::InvalidRecoveryState)?,
                }
            } else {
                let prev = next - 1;
                let last = self
                    .last_index()
                    .min(prev.saturating_add(self.limits.max_append_entries as u64));
                InFlight::Append {
                    request: self.request,
                    prev,
                    last,
                }
            };
            self.emit_flight(peer, &flight, output)?;
            let progress = self
                .progress
                .get_mut(&peer)
                .ok_or(Error::InvalidRecoveryState)?;
            if let InFlight::Append { last, .. } = &flight {
                // This is only the next UNSENT position. Neither matched nor
                // commit_index advances until an exact acknowledgement arrives.
                progress.next = *last + 1;
            }
            progress.in_flight.push_back(flight);
            // Do not append empty heartbeats behind a data batch. A quiescent
            // peer gets one fresh heartbeat when its preceding window drains.
        }
    }

    fn emit_flight(
        &mut self,
        peer: MemberId,
        flight: &InFlight,
        output: &mut Output<C>,
    ) -> Result<(), Error> {
        let message = match flight {
            InFlight::Append {
                request,
                prev,
                last,
            } => {
                let prev_term = self.term_at(*prev).ok_or(Error::InvalidRecoveryState)?;
                let base = self.state.base_index();
                let start = prev.checked_sub(base).ok_or(Error::InvalidRecoveryState)?;
                let end = last.checked_sub(base).ok_or(Error::InvalidRecoveryState)?;
                let entries = self
                    .state
                    .entries
                    .get(start as usize..end as usize)
                    .ok_or(Error::InvalidRecoveryState)?
                    .to_vec();
                Message::Append {
                    term: self.state.term,
                    request: *request,
                    prev_index: *prev,
                    prev_term,
                    entries,
                    leader_commit: self.state.commit_index,
                }
            }
            InFlight::Snapshot { request, snapshot } => Message::InstallSnapshot {
                term: self.state.term,
                request: *request,
                snapshot: snapshot.clone(),
            },
        };
        if matches!(flight, InFlight::Append { .. }) {
            if let Some(progress) = self.progress.get_mut(&peer) {
                progress.sent_commit = self.state.commit_index;
            }
        }
        self.emit(peer, message, output);
        Ok(())
    }
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
