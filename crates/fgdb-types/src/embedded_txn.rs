//! In-process completion facts for the bounded embedded transaction surface.
//!
//! These are ordinary values, not durable transaction records, wire formats,
//! outcome capabilities, or proof of full SSI. A read close validates the
//! embedded engine's recorded observations without allocating a commit sequence.

use crate::CommitSeq;

/// Successful completion distinguishes a published write from a read close.
/// A staged write remains a write even when its canonical effects are empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddedTxnCompletion {
    WriteCommitted {
        commit_seq: CommitSeq,
    },
    /// No prepared write remained. Reads still refer to snapshot_seq; the
    /// engine checked their recorded dependencies through validated_through.
    /// This does not mean that the result was read at the newer sequence.
    ReadClosed {
        snapshot_seq: CommitSeq,
        validated_through: CommitSeq,
    },
}

impl EmbeddedTxnCompletion {
    /// None is a successful read close, never an invented marker sequence.
    #[must_use]
    pub const fn commit_seq(self) -> Option<CommitSeq> {
        match self {
            Self::WriteCommitted { commit_seq } => Some(commit_seq),
            Self::ReadClosed { .. } => None,
        }
    }
}

/// Retained local state of an embedded transaction handle. Only Active admits
/// work. Terminal state remains inspectable after its completion future is
/// dropped, while the snapshot obligation and private workspace are released.
/// Unknown must not be interpreted as aborted or retried as a new write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbeddedTxnState {
    Active,
    Completed(EmbeddedTxnCompletion),
    Aborted,
    /// The write entered the cancellable commit path, but its marker outcome
    /// was not established. The database's existing recovery fence remains.
    CommitOutcomeUnknown {
        published_frontier: CommitSeq,
    },
    /// Chronicle committed, but publication of the derived state did not
    /// complete. The database carries the detailed recovery stage and cause.
    CommittedNeedsRecovery {
        commit_seq: CommitSeq,
    },
}

impl EmbeddedTxnState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Active)
    }
}
