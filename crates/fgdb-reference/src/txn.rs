//! Snapshot-isolated transactions, and the anomaly oracle §15 asks for.
//!
//! Three pieces already existed and could not be composed: [`Snapshot`] says
//! what a reader may see, [`crate::intents`] says what a statement means, and
//! [`ReferenceDatabase::apply_template`] makes effects durable. What was missing
//! is the thing between them — a transaction that reads at a *fixed* sequence,
//! accumulates writes in a workspace, and then has to answer whether it may
//! still commit given what landed while it was thinking.
//!
//! **WHY THIS IS AN ORACLE AND NOT AN IMPLEMENTATION.** §15 wants transaction
//! anomaly oracles: programs that say which histories a given isolation level
//! admits, so a real transaction manager can be differentially tested against
//! them instead of against an argument. The point of writing SI here is
//! therefore as much to pin what it FAILS to prevent as what it prevents.
//! `write_skew_is_admitted_under_snapshot_isolation` is the load-bearing law in
//! the test file, not an embarrassment in it: doctrine 7 forbids "snapshot
//! isolation quietly labeled ACID", and an executable demonstration of the gap
//! is what keeps it un-quiet. When SSI lands, that same history must be
//! REFUSED, and the two laws together are the specification of what SSI buys.
//!
//! **FIRST COMMITTER WINS**, not first writer, not last writer. Two
//! transactions from the same snapshot writing the same element: whichever
//! commits first succeeds and the other is refused. Refusing both would lose
//! work no rule requires losing; allowing both is lost update. The rule is
//! evaluated at commit time against the branch's recorded history, which is why
//! it needed the history model to exist.

use crate::intents::{Outcome, Statement, StatementFailure, evaluate};
use crate::{
    ApplyError, ConflictKey, ReferenceDatabase, ReferenceGraph, Snapshot, SnapshotError,
    collect_conflict_keys,
};
use fgdb_delta_types::{
    CanonicalError, CoordinateEntry, DeltaRow, LogicalDeltaTemplate, RelationId, SchemaEpoch,
};
use fgdb_types::{BranchId, CommitSeq, GraphId, ObjectId};
use std::collections::BTreeSet;

/// A transaction reading at a fixed sequence, writing into a private workspace.
///
/// Not `Clone`: two handles to one transaction would let a caller commit the
/// same work twice under different sequences, and the type refusing to be
/// duplicated is cheaper than a rule saying not to.
#[derive(Debug)]
pub struct Transaction {
    graph: GraphId,
    branch: BranchId,
    snapshot: Snapshot,
    /// The snapshot state plus this transaction's own effects so far. This is
    /// what read-your-own-writes MEANS: statements evaluate against the
    /// workspace, never against the basis or the live database.
    workspace: ReferenceGraph,
    effects: Vec<DeltaRow>,
    statement_failures: usize,
    /// Which statement aborted, if one did. Once set, the transaction is
    /// finished: further `execute` calls are refused rather than ignored, since
    /// a caller that keeps issuing statements after an abort has misunderstood
    /// something and silence would confirm the misunderstanding.
    aborted_at: Option<usize>,
}

/// What happened when a transaction tried to commit.
#[derive(Clone, Debug, PartialEq)]
pub enum TxnOutcome {
    /// Durable. `commit_seq` is where it landed.
    Committed {
        commit_seq: CommitSeq,
        effects: usize,
        statement_failures: usize,
    },
    /// Refused: something this transaction wrote was also written after its
    /// snapshot. The keys are reported because "you conflicted" is not
    /// actionable and "you conflicted on this vertex" is.
    Conflicted { conflicts: Vec<ConflictKey> },
    /// A statement aborted the transaction. Nothing durable, and NOT a
    /// conflict: the transaction did exactly what its guard told it to.
    Aborted { statement: usize },
    /// No effects to commit — every statement was a no-op or failed. Nothing
    /// durable, no sequence consumed, and distinct from `Aborted`, because a
    /// caller deciding whether to retry needs to tell "nothing to do" from
    /// "refused".
    NothingToCommit { statement_failures: usize },
}

/// Why a transaction operation could not be carried out at all.
///
/// Distinct from [`TxnOutcome`]: a conflict or an abort is a transaction
/// behaving correctly, while these are the caller or the database being wrong.
/// Collapsing the two would make "you lost a race" and "your effects are
/// malformed" the same event.
#[derive(Clone, Debug, PartialEq)]
pub enum TxnError {
    /// The coordinate could not be snapshotted.
    Snapshot(SnapshotError),
    /// Statements were issued after the transaction aborted.
    AlreadyAborted { statement: usize },
    /// The effects did not form a canonical template.
    Canonical(CanonicalError),
    /// The effects did not apply to the committed state, even though no
    /// conflict was detected. An internal contradiction: the conflict rule is
    /// supposed to be exactly the condition under which they would not.
    Apply(Box<ApplyError>),
}

impl core::fmt::Display for TxnError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Snapshot(error) => write!(f, "snapshot: {error}"),
            Self::AlreadyAborted { statement } => {
                write!(f, "transaction aborted at statement {statement}")
            }
            Self::Canonical(error) => write!(f, "effects are not canonical: {error:?}"),
            Self::Apply(error) => write!(f, "effects did not apply: {error}"),
        }
    }
}

impl core::error::Error for TxnError {}

impl Transaction {
    /// Begin at the coordinate's current frontier.
    pub fn begin(
        db: &ReferenceDatabase,
        graph: GraphId,
        branch: BranchId,
    ) -> Result<Self, TxnError> {
        let snapshot = db.snapshot(graph, branch).map_err(TxnError::Snapshot)?;
        Self::begin_at(db, snapshot)
    }

    /// Begin the FIRST transaction on a branch that does not exist yet.
    ///
    /// Refuses if the coordinate already exists, so this cannot be used as a
    /// permissive fallback for a mistyped branch name — see
    /// [`ReferenceDatabase::genesis_snapshot`].
    pub fn begin_genesis(
        db: &ReferenceDatabase,
        graph: GraphId,
        branch: BranchId,
    ) -> Result<Self, TxnError> {
        let snapshot = db
            .genesis_snapshot(graph, branch)
            .map_err(TxnError::Snapshot)?;
        Self::begin_at(db, snapshot)
    }

    /// Begin at an explicit snapshot — including a historical one, which is what
    /// makes a repeatable read over old state expressible rather than only a
    /// read of the present.
    pub fn begin_at(db: &ReferenceDatabase, snapshot: Snapshot) -> Result<Self, TxnError> {
        let workspace = db.read(&snapshot).map_err(TxnError::Snapshot)?;
        Ok(Self {
            graph: snapshot.graph(),
            branch: snapshot.branch(),
            snapshot,
            workspace,
            effects: Vec::new(),
            statement_failures: 0,
            aborted_at: None,
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// What this transaction sees: its snapshot plus its own writes.
    pub fn workspace(&self) -> &ReferenceGraph {
        &self.workspace
    }

    pub fn effects(&self) -> &[DeltaRow] {
        &self.effects
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted_at.is_some()
    }

    /// Every conflict key this transaction has written so far.
    pub fn write_set(&self) -> BTreeSet<ConflictKey> {
        let mut keys = BTreeSet::new();
        for row in &self.effects {
            collect_conflict_keys(row, &mut keys);
        }
        keys
    }

    /// Evaluate `statements` against the workspace, folding their effects in.
    ///
    /// Called more than once to model a transaction that issues statements over
    /// time — which is the only way a concurrent commit can land *between* two
    /// of its statements, and therefore the only way the anomaly laws can be
    /// written at all. Each call sees the workspace the previous call left.
    pub fn execute(&mut self, statements: &[Statement]) -> Result<(), TxnError> {
        if let Some(statement) = self.aborted_at {
            return Err(TxnError::AlreadyAborted { statement });
        }
        match evaluate(&self.workspace, statements) {
            Outcome::Aborted { statement, .. } => {
                self.aborted_at = Some(statement);
                // The workspace is deliberately left as it was. An aborted
                // transaction has no state worth inspecting, and clearing it
                // would make `workspace()` say the snapshot was empty.
                Ok(())
            }
            Outcome::Committed {
                effects,
                statement_failures,
            } => {
                for row in &effects {
                    self.workspace
                        .apply_row(row)
                        .map_err(|error| TxnError::Apply(Box::new(error)))?;
                }
                self.statement_failures += statement_failures.len();
                self.effects.extend(effects);
                Ok(())
            }
        }
    }

    /// The statement failures accumulated so far.
    pub fn statement_failures(&self) -> usize {
        self.statement_failures
    }

    /// Try to make this transaction durable at `commit_seq`.
    ///
    /// ABORT IS DECIDED BEFORE CONFLICT, and that order is observable: a
    /// transaction that aborted never had a claim to make, so reporting it as a
    /// conflict would blame a concurrent writer for a guard this transaction
    /// chose. There is a law for it, because an aborted transaction whose writes
    /// *would* have conflicted is exactly the case where the two answers differ.
    ///
    /// The conflict/emptiness order, by contrast, is NOT observable, and saying
    /// otherwise would be a claim with nothing behind it: a transaction with no
    /// effects has an empty write set and therefore cannot conflict, so the two
    /// conditions are mutually exclusive and either order gives the same answer.
    ///
    /// Consumes the transaction. A handle that survived its own commit could be
    /// committed again at a second sequence, duplicating the effects.
    pub fn commit(
        self,
        db: &mut ReferenceDatabase,
        relation: RelationId,
        intent_semantics: ObjectId,
        commit_seq: CommitSeq,
    ) -> Result<TxnOutcome, TxnError> {
        if let Some(statement) = self.aborted_at {
            return Ok(TxnOutcome::Aborted { statement });
        }

        let mine = self.write_set();
        if !mine.is_empty() {
            let theirs = db.conflict_keys_since(self.graph, self.branch, self.snapshot.high());
            let conflicts: Vec<ConflictKey> = mine.intersection(&theirs).copied().collect();
            if !conflicts.is_empty() {
                return Ok(TxnOutcome::Conflicted { conflicts });
            }
        }

        if self.effects.is_empty() {
            return Ok(TxnOutcome::NothingToCommit {
                statement_failures: self.statement_failures,
            });
        }

        let effects = self.effects.len();
        let template = LogicalDeltaTemplate::build(
            intent_semantics,
            [0u8; 32],
            vec![CoordinateEntry {
                graph: self.graph,
                branch: self.branch,
                relation,
                schema_epoch: SchemaEpoch(0),
                schema_transition: None,
                rows: self.effects,
            }],
        )
        .map_err(TxnError::Canonical)?;
        db.apply_template(&template, commit_seq)
            .map_err(|error| TxnError::Apply(Box::new(error)))?;

        Ok(TxnOutcome::Committed {
            commit_seq,
            effects,
            statement_failures: self.statement_failures,
        })
    }
}

impl TxnOutcome {
    /// `(commit_seq, effects, statement_failures)` if this committed.
    ///
    /// An accessor so tests assert via `expect` rather than a `panic!` arm on a
    /// non-matching variant — a `panic!` in a test moves the workspace's UBS
    /// panic class.
    pub fn committed_parts(&self) -> Option<(CommitSeq, usize, usize)> {
        match self {
            Self::Committed {
                commit_seq,
                effects,
                statement_failures,
            } => Some((*commit_seq, *effects, *statement_failures)),
            _ => None,
        }
    }

    pub fn conflicts(&self) -> Option<&[ConflictKey]> {
        match self {
            Self::Conflicted { conflicts } => Some(conflicts),
            _ => None,
        }
    }

    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }
}

/// A statement failure paired with the statement it came from, for callers that
/// want to inspect what survived a partially-failed transaction.
pub type IndexedFailure = (usize, StatementFailure);
