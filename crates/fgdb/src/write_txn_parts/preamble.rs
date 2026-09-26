use crate::{
    BoundPlan, Database, EdgeRecord, GqlError, PendingRow, PreparedWrite, ReadError, RelationBind,
    VertexRow, WriteBatch, WriteError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, LabelId, RelationId};
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{
    Acquired, CanonicalScalar, CommitCx, CommitSeq, EId, EmbeddedTxnCompletion, EmbeddedTxnState,
    ObligationAcquireError, ObligationId, PurposeObligation, TxnCx, VId,
};

/// Per-open-handle reservations, never persisted independently of Chronicle.
/// The durable floor is refreshed from committed delta rows before allocation.
/// Aborted work does not rewind reservations within this writer lifetime.
#[derive(Default)]
pub(crate) struct IdentityAllocation {
    frontier: CommitSeq,
    vertex: u128,
    edge: u128,
}

/// The error of a governed WriteTxn GQL entry point: the statement family's
/// error `E` (over [`WriteTxnError`]) or the runtime's interruption. Named once
/// so each signature states only the family it can fail with.
pub(crate) type TxnGqlError<E> = fgdb_gql::GqlQueryError<E, Box<asupersync::error::Error>>;

/// A returning write's outcome: its statistics plus the vertex and edge
/// identities it affected, in the statement's canonical order.
pub(crate) type WithAffectedIds<S> = (S, Vec<VId>, Vec<EId>);

/// Failure to prepare an atomic write or stage/finish a bounded transaction.
#[derive(Debug)]
pub enum WriteTxnError {
    /// Capability admission, target/field scope, or a live execution fence.
    Authorization(fgdb_warden::Error),
    /// A scoped mutation refused before publication. Native preparation/source
    /// diagnostics can contain protected identities or CAS values and do not
    /// escape this boundary. Never used for an admitted commit's outcome.
    AuthorizedMutationRefused,
    NoPreparedWrite,
    Finished,
    /// The supplied database is not the opened handle that began this txn.
    /// Refusal preserves the transaction for use with its actual owner.
    WrongDatabase,
    /// No live savepoint has the requested name. Names are transaction-local.
    UnknownSavepoint,
    /// The bounded embedded workspace has reached its savepoint-count limit.
    SavepointLimit {
        limit: usize,
    },
    RelationMismatch {
        expected: RelationId,
        found: RelationId,
    },
    SnapshotAdvanced {
        pinned: CommitSeq,
        live: CommitSeq,
    },
    /// Independent relation groups cannot silently consume each other's writes.
    AtomicRelationConflict {
        first: RelationId,
        second: RelationId,
        element: ElementId,
    },
    /// The compound command cannot assign distinct u64 intent-visit ordinals.
    AtomicOrdinalOverflow,
    /// Ordered preparation would exceed its admitted evaluator-input rows.
    /// Counts include repeated vertex instructions, not just net effects.
    /// This is a row-replication bound, not a byte or storage-work quota.
    OrderedWriteBudgetExceeded {
        limit: u64,
        required: u128,
    },
    /// Re-staging would give two created elements the same birth ordinal once
    /// the prefix's observed births are retained. Retention never renumbers,
    /// so this refuses instead of publishing ambiguous births.
    BirthOrdinalCollision,
    /// A new delta family needs an explicit compound-write independence law.
    UnsupportedAtomicMutation,
    /// The requested 128-bit identity domain has no remaining successor.
    IdentityExhausted,
    /// Cancellation before acceptance. Completion makes the transaction
    /// terminal; snapshot refresh instead preserves its entire active workspace.
    /// Neither interrupted operation publishes a write from this attempt.
    Interrupted(Box<asupersync::error::Error>),
    Read(ReadError),
    Gql(GqlError),
    Write(WriteError),
}

impl core::fmt::Display for WriteTxnError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Authorization(source) => {
                write!(formatter, "write authorization refused: {source}")
            }
            Self::AuthorizedMutationRefused => formatter.write_str("authorized mutation refused"),
            Self::NoPreparedWrite => formatter.write_str("write transaction has no batch"),
            Self::Finished => formatter.write_str("write transaction is already finished"),
            Self::WrongDatabase => {
                formatter.write_str("write transaction belongs to a different database handle")
            }
            Self::UnknownSavepoint => formatter.write_str("unknown transaction savepoint"),
            Self::SavepointLimit { limit } => {
                write!(formatter, "transaction savepoint limit {limit} reached")
            }
            Self::RelationMismatch { expected, found } => write!(
                formatter,
                "write transaction relation mismatch: expected {expected:?}, found {found:?}"
            ),
            Self::SnapshotAdvanced { pinned, live } => write!(
                formatter,
                "write transaction pinned {pinned:?}, but the live snapshot advanced to {live:?}"
            ),
            Self::AtomicRelationConflict {
                first,
                second,
                element,
            } => write!(
                formatter,
                "atomic relation groups {first:?} and {second:?} are not independent at {element:?}"
            ),
            Self::AtomicOrdinalOverflow => {
                formatter.write_str("atomic write intent ordinal overflow")
            }
            Self::OrderedWriteBudgetExceeded { limit, required } => write!(
                formatter,
                "ordered write requires {required} evaluator-input rows, exceeding limit {limit}"
            ),
            Self::BirthOrdinalCollision => formatter
                .write_str("retained birth ordinals would collide with newly assigned births"),
            Self::IdentityExhausted => formatter.write_str("element identity domain exhausted"),
            Self::UnsupportedAtomicMutation => formatter
                .write_str("atomic write contains a mutation without a defined independence law"),
            Self::Interrupted(source) => {
                write!(formatter, "transaction operation interrupted: {source}")
            }
            Self::Read(source) => write!(formatter, "could not read the pinned snapshot: {source}"),
            Self::Gql(source) => write!(formatter, "transaction GQL failed: {source}"),
            Self::Write(source) => write!(formatter, "write transaction failed: {source}"),
        }
    }
}

impl core::error::Error for WriteTxnError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Authorization(source) => Some(source),
            Self::Interrupted(source) => Some(source.as_ref()),
            Self::Read(source) => Some(source),
            Self::Gql(source) => Some(source),
            Self::Write(source) => Some(source),
            Self::AuthorizedMutationRefused
            | Self::NoPreparedWrite
            | Self::Finished
            | Self::WrongDatabase
            | Self::UnknownSavepoint
            | Self::SavepointLimit { .. }
            | Self::SnapshotAdvanced { .. }
            | Self::AtomicRelationConflict { .. }
            | Self::RelationMismatch { .. }
            | Self::AtomicOrdinalOverflow
            | Self::OrderedWriteBudgetExceeded { .. }
            | Self::BirthOrdinalCollision
            | Self::IdentityExhausted
            | Self::UnsupportedAtomicMutation => None,
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
/// This is deliberately not SSI. Same-relation prefixes retain call order;
/// `write_atomic` explicitly admits independent relation groups at the same
/// pinned basis. Each successful staging refreshes one canonical template.
/// Commit validates observations and conservative scan witnesses before
/// delegating the whole template to the existing publication coordinator.
/// The basis stays pinned unless `refresh_snapshot` explicitly validates and
/// advances it; ordinary reads and writes never refresh it implicitly.
pub struct WriteTxn {
    handle_owner: std::sync::Arc<()>,
    basis: CommitSeq,
    staged: Vec<WriteBatch>,
    prepared: Option<PreparedWrite>,
    savepoints: Vec<EmbeddedSavepoint>,
    /// Enabled only while a mixed write program owns its rollback workspace.
    program_multi_relation: bool,
    read_set: std::cell::RefCell<std::collections::BTreeSet<ElementId>>,
    match_expansions: std::cell::RefCell<std::collections::BTreeSet<(VId, RelationId)>>,
    scanned_vertex_labels: std::cell::RefCell<std::collections::BTreeSet<LabelId>>,
    /// A scan depends on absent rows too, even if it returned nothing or its
    /// result was discarded by a budget check. Raw scans remain table-wide;
    /// labelled node scans separately retain their required label. Neither
    /// witness models arbitrary property predicates or full predicate SSI.
    scanned_vertices: std::cell::Cell<bool>,
    scanned_edges: std::cell::Cell<bool>,
    state: EmbeddedTxnState,
    pin: Option<PurposeObligation<Acquired>>,
}
