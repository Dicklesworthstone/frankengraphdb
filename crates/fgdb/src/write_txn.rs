use crate::{Database, ReadError, WriteBatch, WriteError};
use asupersync::fs::Vfs;
use fgdb_delta_types::RelationId;
use fgdb_types::{
    Acquired, CommitCx, CommitSeq, ObligationAcquireError, ObligationId, PurposeObligation, TxnCx,
};

/// Failure to prepare or finish the bounded one-batch write transaction.
#[derive(Debug)]
pub enum WriteTxnError {
    NoPreparedWrite,
    Finished,
    RelationMismatch {
        expected: RelationId,
        found: RelationId,
    },
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
            Self::NoPreparedWrite => formatter.write_str("write transaction has no batch"),
            Self::Finished => formatter.write_str("write transaction is already finished"),
            Self::RelationMismatch { expected, found } => write!(
                formatter,
                "write transaction relation mismatch: expected {expected:?}, found {found:?}"
            ),
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
            Self::NoPreparedWrite
            | Self::Finished
            | Self::RelationMismatch { .. }
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

/// Write batches staged against a snapshot pinned by a [`TxnCx`].
///
/// This is deliberately not SSI: it combines same-relation batches in call
/// order, prepares them once, and delegates the commit verdict to
/// [`Database::commit_prepared`].
pub struct WriteTxn {
    basis: CommitSeq,
    staged: Vec<WriteBatch>,
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
            staged: Vec::new(),
            pin: Some(pin),
        })
    }

    /// The snapshot frontier retained for this transaction.
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    /// Stage a same-relation batch against this transaction's pinned snapshot.
    pub fn write<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let live = database.frontier()?;
        if live != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }

        if let Some(expected) = self.staged.first().map(|staged| staged.relation)
            && batch.relation != expected
        {
            return Err(WriteTxnError::RelationMismatch {
                expected,
                found: batch.relation,
            });
        }
        self.staged.push(batch);
        Ok(())
    }

    /// Commit the prepared batch exactly as derived, then release the pin.
    pub async fn commit<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.commit_with_crash(database, cx, None).await
    }

    /// Commit through the production crash-point path, then release the pin
    /// regardless of whether the prepared write committed or was refused.
    pub async fn commit_with_crash<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
    ) -> Result<CommitSeq, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        let mut staged = core::mem::take(&mut self.staged).into_iter();
        let Some(mut combined) = staged.next() else {
            self.release_pin();
            return Err(WriteTxnError::NoPreparedWrite);
        };
        for mut batch in staged {
            debug_assert_eq!(batch.relation, combined.relation);
            combined.rows.append(&mut batch.rows);
        }

        let live = match database.frontier() {
            Ok(live) => live,
            Err(source) => {
                self.release_pin();
                return Err(WriteTxnError::Read(source));
            }
        };
        if live != self.basis {
            self.release_pin();
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }
        let prepared = match database.prepare_write(combined) {
            Ok(prepared) => prepared,
            Err(source) => {
                self.release_pin();
                return Err(WriteTxnError::Write(source));
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);

        let result = database
            .commit_prepared_with_crash(cx, prepared, crash_at)
            .await
            .map_err(WriteTxnError::Write);
        self.release_pin();
        result
    }

    /// End the transaction without publishing its prepared batch.
    pub fn abort(mut self) {
        self.staged.clear();
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
            .field("staged_batches", &self.staged.len())
            .field("pin_obligation", &self.pin.as_ref().map(PurposeObligation::id))
            .finish()
    }
}

impl Drop for WriteTxn {
    fn drop(&mut self) {
        self.release_pin();
    }
}

#[cfg(test)]
mod tests {
    use super::WriteTxnError;
    use crate::{Database, DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};

    #[test]
    fn write_refuses_an_advanced_snapshot_without_preparing() {
        let ((), report) = run_async_under_lab(0x7a_10, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let baseline = txn_cx.outstanding_obligations();
            let directory = std::env::temp_dir().join(format!(
                "fgdb-write-txn-snapshot-advanced-{}",
                std::process::id()
            ));
            let keys = DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            );
            let mut database = Database::create(&commit, &directory, keys)
                .await
                .expect("database creates");
            let mut transaction = database.begin(&txn_cx).expect("transaction begins");
            let pinned = transaction.basis();

            let mut advancing = WriteBatch::new(RelationId(1));
            advancing.create_vertex(VId(1), Vec::new(), Vec::new());
            let live = database
                .write(&commit, advancing)
                .await
                .expect("autocommit advances the live frontier");

            let mut stale = WriteBatch::new(RelationId(1));
            stale.create_vertex(VId(2), Vec::new(), Vec::new());
            let error = transaction
                .write(&mut database, stale)
                .expect_err("a stale transaction cannot prepare against the live fold");
            assert!(matches!(
                error,
                WriteTxnError::SnapshotAdvanced {
                    pinned: error_pinned,
                    live: error_live,
                } if error_pinned == pinned && error_live == live
            ));
            assert!(
                transaction.staged.is_empty(),
                "snapshot refusal must not retain a staged batch"
            );
            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline + 1,
                "snapshot refusal keeps the pin live until explicit abort"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), baseline);
        });

        assert!(report.lab_test_passed(), "lab run failed: {report:?}");
    }
}
