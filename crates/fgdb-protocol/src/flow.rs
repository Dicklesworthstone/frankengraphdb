use crate::ProtocolError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendCost {
    pub bytes: u64,
    pub rows: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreditUpdate {
    pub sequence: u64,
    pub bytes: u64,
    pub rows: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowStatus {
    Applied,
    Replayed,
}

/// The byte window charges complete encoded frame bytes, including its header.
/// Row credit is charged independently; zero-row control frames still cost
/// bytes. Control-plane admission must reserve its own bounded window so data
/// backpressure cannot starve cancellation/drain traffic.
#[derive(Debug)]
pub struct FlowWindow {
    available: SendCost,
    maximum: SendCost,
    last_update: Option<CreditUpdate>,
    sent: SendCost,
    failed_after_write: u64,
}

impl FlowWindow {
    pub fn new(initial: SendCost, maximum: SendCost) -> Result<Self, ProtocolError> {
        if maximum.bytes == 0 || initial.bytes > maximum.bytes || initial.rows > maximum.rows {
            return Err(ProtocolError::InvalidLimit);
        }
        Ok(Self {
            available: initial,
            maximum,
            last_update: None,
            sent: SendCost { bytes: 0, rows: 0 },
            failed_after_write: 0,
        })
    }
    pub const fn available(&self) -> SendCost {
        self.available
    }
    pub const fn sent(&self) -> SendCost {
        self.sent
    }
    pub const fn failed_after_write(&self) -> u64 {
        self.failed_after_write
    }
    /// The immediate duplicate must byte-match; it never grants credit twice.
    /// Out-of-order updates and overflow leave BOTH counters unchanged.
    pub fn grant(&mut self, update: CreditUpdate) -> Result<WindowStatus, ProtocolError> {
        if let Some(previous) = self.last_update {
            if update == previous {
                return Ok(WindowStatus::Replayed);
            }
            if update.sequence != previous.sequence.checked_add(1)
                .ok_or(ProtocolError::InvalidCreditUpdate)?
            {
                return Err(ProtocolError::InvalidCreditUpdate);
            }
        } else if update.sequence != 1 {
            return Err(ProtocolError::InvalidCreditUpdate);
        }
        let bytes = self.available.bytes.checked_add(update.bytes)
            .ok_or(ProtocolError::CreditOverflow)?;
        let rows = self.available.rows.checked_add(update.rows)
            .ok_or(ProtocolError::CreditOverflow)?;
        if bytes > self.maximum.bytes || rows > self.maximum.rows {
            return Err(ProtocolError::CreditOverflow);
        }
        self.available = SendCost { bytes, rows };
        self.last_update = Some(update);
        Ok(WindowStatus::Applied)
    }
    pub fn reserve(&mut self, cost: SendCost) -> Result<Reservation<'_>, ProtocolError> {
        if cost.bytes == 0 || cost.bytes > self.available.bytes || cost.rows > self.available.rows {
            return Err(ProtocolError::CreditExceeded);
        }
        // Preflight the cumulative counters now, so recording a successful
        // physical write cannot subsequently fail due to arithmetic overflow.
        self.sent.bytes.checked_add(cost.bytes).ok_or(ProtocolError::CreditOverflow)?;
        self.sent.rows.checked_add(cost.rows).ok_or(ProtocolError::CreditOverflow)?;
        self.failed_after_write.checked_add(1).ok_or(ProtocolError::CreditOverflow)?;
        self.available.bytes -= cost.bytes;
        self.available.rows -= cost.rows;
        Ok(Reservation { window: self, cost, state: SendState::Queued })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendState {
    Queued,
    Writing,
    Sent,
    CancelledBeforeWrite,
    Failed,
}

/// Linear accounting for ONE frame. The mutable window borrow serializes
/// accounting, not I/O across the server: each stream has its own window.
/// Dropping before write refunds; dropping after write-start records failure
/// and does not refund uncertain bytes. No state here retires durable output.
#[derive(Debug)]
pub struct Reservation<'a> {
    window: &'a mut FlowWindow,
    cost: SendCost,
    state: SendState,
}

impl Reservation<'_> {
    pub const fn state(&self) -> SendState {
        self.state
    }
    /// Call only after obtaining a fresh public-frame send guard, immediately
    /// before the first physical write. Queue-time authorization is insufficient.
    pub fn begin_write(&mut self) -> Result<(), ProtocolError> {
        if self.state != SendState::Queued {
            return Err(ProtocolError::InvalidState);
        }
        self.state = SendState::Writing;
        Ok(())
    }
    pub fn sent(mut self) -> Result<(), ProtocolError> {
        if self.state != SendState::Writing {
            return Err(ProtocolError::InvalidState);
        }
        self.window.sent.bytes += self.cost.bytes;
        self.window.sent.rows += self.cost.rows;
        self.state = SendState::Sent;
        Ok(())
    }
    pub fn cancel_before_write(mut self) -> Result<(), ProtocolError> {
        if self.state != SendState::Queued {
            return Err(ProtocolError::InvalidState);
        }
        self.refund();
        self.state = SendState::CancelledBeforeWrite;
        Ok(())
    }
    pub fn failed(mut self) -> Result<(), ProtocolError> {
        if self.state != SendState::Writing {
            return Err(ProtocolError::InvalidState);
        }
        self.window.failed_after_write += 1;
        self.state = SendState::Failed;
        Ok(())
    }
    fn refund(&mut self) {
        // A reservation holds the only mutable borrow; grants cannot race this
        // refund, which exactly reverses the checked subtraction in reserve().
        self.window.available.bytes += self.cost.bytes;
        self.window.available.rows += self.cost.rows;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        match self.state {
            SendState::Queued => {
                self.refund();
                self.state = SendState::CancelledBeforeWrite;
            }
            SendState::Writing => {
                self.window.failed_after_write += 1;
                self.state = SendState::Failed;
            }
            SendState::Sent | SendState::CancelledBeforeWrite | SendState::Failed => {}
        }
    }
}
