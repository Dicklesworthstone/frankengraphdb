use crate::{
    BoundPlan, Database, EdgeRecord, GqlError, PendingRow, PreparedWrite, ReadError, RelationBind,
    VertexRow, WriteBatch, WriteError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, RelationId};
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{
    Acquired, CanonicalScalar, CommitCx, CommitSeq, EId, ObligationAcquireError, ObligationId,
    PurposeObligation, TxnCx, VId,
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
    Gql(GqlError),
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
            Self::Gql(source) => write!(formatter, "transaction GQL failed: {source}"),
            Self::Write(source) => write!(formatter, "write transaction failed: {source}"),
        }
    }
}

impl core::error::Error for WriteTxnError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(source) => Some(source),
            Self::Gql(source) => Some(source),
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

impl From<GqlError> for WriteTxnError {
    fn from(source: GqlError) -> Self {
        Self::Gql(source)
    }
}

/// Write batches staged against a snapshot pinned by a [`TxnCx`].
///
/// This is deliberately not SSI: after each write it combines same-relation
/// batches in call order and refreshes one prepared template against the
/// pinned basis. Commit delegates that retained template's verdict to
/// [`Database::commit_prepared`].
pub struct WriteTxn {
    basis: CommitSeq,
    staged: Vec<WriteBatch>,
    prepared: Option<PreparedWrite>,
    read_set: std::cell::RefCell<std::collections::BTreeSet<ElementId>>,
    match_expansions: std::cell::RefCell<std::collections::BTreeSet<(VId, RelationId)>>,
    pin: Option<PurposeObligation<Acquired>>,
}

