use crate::{Database, PreparedWrite, ReadError, WriteBatch, WriteError};
use asupersync::fs::Vfs;
use fgdb_types::{
    Acquired, CommitCx, CommitSeq, ObligationAcquireError, ObligationId, PurposeObligation, TxnCx,
};

/// Failure to prepare or finish the bounded one-batch write transaction.
#[derive(Debug)]
pub enum WriteTxnError {
    AlreadyPrepared,
    NoPreparedWrite,
    Finished,
    SnapshotAdvanced {
        pinned: CommitSeq,
        live: CommitSeq,
    },
    Read(ReadError),
    Write(WriteError),
}

impl core::fmt::Display for WriteTxnError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AlreadyPrepared => formatter.write_str("write transaction already has a batch"),
            Self::NoPreparedWrite => formatter.write_str("write transaction has no batch"),
            Self::Finished => formatter.write_str("write transaction is already finished"),
            Self::SnapshotAdvanced { pinned, live } => write!(
                formatter,
                "write transaction pinned {pinned:?}, but the live snapshot advanced to {live:?}"
            ),
            Self::Read(source) => write!(formatter, "could not read the pinned snapshot: {source}"),
            Self::Write(source) => write!(formatter, "write transaction failed: {source}"),
        }
    }
}

impl core::error::Error for WriteTxnError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(source) => Some(source),
            Self::Write(source) => Some(source),
            Self::AlreadyPrepared
            | Self::NoPreparedWrite
            | Self::Finished
            | Self::SnapshotAdvanced { .. } => None,
        }
    }
}

impl From<ReadError> for WriteTxnError {
    fn from(source: ReadError) -> Self {
        Self::Read(source)
    }
}

impl From<WriteError> for WriteTxnError {
    fn from(source: WriteError) -> Self {
        Self::Write(source)
    }
}

/// One write batch derived from a snapshot pinned by a [`TxnCx`].
///
/// This is deliberately not SSI: it retains one prepared batch and delegates
/// the commit verdict to [`Database::commit_prepared`].
pub struct WriteTxn {
    basis: CommitSeq,
    prepared: Option<PreparedWrite>,
    pin: Option<PurposeObligation<Acquired>>,
}

impl WriteTxn {
    pub(crate) fn begin(
        basis: CommitSeq,
        txn: &TxnCx,
        obligation_id: ObligationId,
    ) -> Result<Self, ObligationAcquireError> {
        let pin = txn.pin_snapshot(obligation_id)?;
        Ok(Self {
            basis,
            prepared: None,
            pin: Some(pin),
        })
    }

    /// The snapshot frontier retained for this transaction.
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    /// Prepare this transaction's sole batch against its pinned live snapshot.
    pub fn write<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        if self.prepared.is_some() {
            return Err(WriteTxnError::AlreadyPrepared);
        }

        let live = database.frontier()?;
        if live != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }

        let prepared = database.prepare_write(batch)?;
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(prepared);
        Ok(())
    }

    /// Commit the prepared batch exactly as derived, then release the pin.
    pub async fn commit<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        let Some(prepared) = self.prepared.take() else {
            self.release_pin();
            return Err(WriteTxnError::NoPreparedWrite);
        };

        let result = database
            .commit_prepared(cx, prepared)
            .await
            .map_err(WriteTxnError::Write);
        self.release_pin();
        result
    }

    /// End the transaction without publishing its prepared batch.
    pub fn abort(mut self) {
        self.prepared = None;
        self.release_pin();
    }

    fn release_pin(&mut self) {
        if let Some(pin) = self.pin.take() {
            let _receipt = pin.abort();
        }
    }
}

impl core::fmt::Debug for WriteTxn {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("WriteTxn")
            .field("basis", &self.basis)
            .field("has_prepared_write", &self.prepared.is_some())
            .field("pin_obligation", &self.pin.as_ref().map(PurposeObligation::id))
            .finish()
    }
}

impl Drop for WriteTxn {
    fn drop(&mut self) {
        self.release_pin();
    }
}
