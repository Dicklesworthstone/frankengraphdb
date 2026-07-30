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
//!
//! **CONSTRAINT CERTIFICATION IS A SEPARATE DOMAIN.** Edge creation depends on
//! both endpoints continuing to exist, while vertex deletion invalidates that
//! fact. That dependency is shared/exclusive rather than an ordinary write key:
//! two edge creations at one hub may both commit, but either ordering of edge
//! creation versus endpoint deletion must refuse the loser before durable apply.
//! This is endpoint-specific referential integrity, not a claim that this SI
//! oracle protects general adjacency or neighbour phantoms.

use crate::intents::{Outcome, Statement, StatementFailure, evaluate_from_intent_ordinal};
use crate::{
    ApplyError, CertificationSummary, ConflictKey, ReferenceDatabase, ReferenceGraph, Snapshot,
    SnapshotError, Vertex, collect_conflict_keys,
};
use fgdb_delta_types::{
    CanonicalError, CoordinateEntry, DeltaRow, ElementId, LogicalDeltaTemplate, PropertyKeyId,
    RelationId, SchemaEpoch,
};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, GraphId, ObjectId, VId};
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
    /// The last canonical source-intent ordinal consumed across every
    /// `execute` call. This belongs to the transaction, not to one evaluator
    /// invocation: resetting it would give two published creates the same
    /// sequence-neutral birth order.
    last_intent_ordinal: u64,
    /// What this transaction depended on. Separate from the write set because an
    /// rw-antidependency needs both, and a transaction that reads x and writes y
    /// is exactly the shape write-write conflict detection cannot see.
    read_set: BTreeSet<ConflictKey>,
    statement_failures: usize,
    /// Transaction-global index of the next statement submitted through
    /// `execute`. Evaluator outcomes use slice-local indexes, so every boundary
    /// must translate through this cursor before an abort becomes observable.
    next_statement_index: usize,
    /// Did this transaction claim to be the FIRST write to the branch?
    ///
    /// Carried to commit rather than checked only at begin
    /// (fgdb-reference-genesis-transaction-race-dfk3): two transactions can both
    /// find the coordinate absent, and the one that commits second computed every
    /// before-image against a state that no longer exists. The claim has to be
    /// certified where the decision is made.
    claims_genesis: bool,
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
    /// Refused: either an ordinary write collided or an asymmetric constraint
    /// dependency was invalidated after this transaction's snapshot. The keys
    /// are reported because "you conflicted" is not actionable and "your edge
    /// endpoint was deleted" is.
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
    /// The effects did not apply to the committed state even though no conflict
    /// was detected.
    ///
    /// AN EARLIER VERSION OF THIS COMMENT CALLED THAT AN INTERNAL CONTRADICTION,
    /// and it was reachable: a historical child transaction whose inherited
    /// lineage had moved landed here rather than being reported as a conflict
    /// (fgdb-reference-historical-fork-conflict-lineage-re6w). The conflict rule
    /// now covers that window, so this arm is expected to be unreachable — but
    /// "expected" is the honest word. It is a typed refusal that names the row
    /// rather than a claim that the rule is complete, and a new reachable path to
    /// it is a defect in the rule rather than in this arm.
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
        // A basis on a coordinate with no origin is a genesis claim, however the
        // snapshot was minted.
        let claims_genesis = db
            .branch_origin(snapshot.graph(), snapshot.branch())
            .is_none();
        Ok(Self {
            graph: snapshot.graph(),
            branch: snapshot.branch(),
            snapshot,
            workspace,
            claims_genesis,
            effects: Vec::new(),
            last_intent_ordinal: 0,
            read_set: BTreeSet::new(),
            statement_failures: 0,
            next_statement_index: 0,
            aborted_at: None,
        })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// What this transaction sees: its snapshot plus its own writes.
    ///
    /// **AN UNTRACKED READ.** Nothing read through here enters the read set, so
    /// it is invisible to [`crate::ssi`]. That is correct for a test making an
    /// assertion about state and wrong for anything modelling what a transaction
    /// *depended on* — use [`read_vertex`](Self::read_vertex) and its siblings
    /// for that. The distinction is named rather than removed because assertions
    /// genuinely should not create dependencies: a test that inspected the
    /// workspace would otherwise change the history it is checking.
    pub fn workspace(&self) -> &ReferenceGraph {
        &self.workspace
    }

    /// Every conflict key this transaction has READ through a tracked read.
    ///
    /// Reads are tracked because rw-antidependencies are what separates
    /// serializable from snapshot isolation: write-write conflicts alone cannot
    /// see write skew, since the two writers touch different elements. The read
    /// set is the other half of that edge.
    pub fn read_set(&self) -> &BTreeSet<ConflictKey> {
        &self.read_set
    }

    /// Read a vertex, recording the dependency.
    ///
    /// Returns an owned clone rather than a borrow: this crate is never
    /// optimized, and a `&mut self` read that also handed out a reference would
    /// make every tracked read exclusive for its whole lifetime.
    pub fn read_vertex(&mut self, vid: VId) -> Option<Vertex> {
        self.read_set
            .insert(ConflictKey::Element(ElementId::Vertex(vid)));
        self.workspace.vertex(vid).cloned()
    }

    /// Read one property of one element, recording the dependency.
    pub fn read_property(
        &mut self,
        elem: ElementId,
        property: PropertyKeyId,
    ) -> Option<CanonicalScalar> {
        self.read_set.insert(ConflictKey::Element(elem));
        match elem {
            ElementId::Vertex(vid) => self.workspace.vertex(vid)?.props.get(&property).cloned(),
            ElementId::Edge(eid) => self.workspace.edge(eid)?.props.get(&property).cloned(),
        }
    }

    /// Read the neighbours of a vertex over one relation, recording the vertex
    /// and every edge traversed.
    ///
    /// SCOPED, and the limit matters: recording the edges that EXIST cannot
    /// express a dependency on an edge that does not. A concurrent transaction
    /// inserting a new neighbour therefore forms no rw edge here, so this read
    /// set catches write skew over existing elements and NOT predicate phantoms.
    /// Full phantom detection needs predicate or index-range tracking, which
    /// belongs with Strata's adjacency structures; claiming it here would be a
    /// substitute for a mechanism this crate does not have.
    pub fn read_neighbours(&mut self, vid: VId, relation: RelationId) -> Vec<VId> {
        self.read_set
            .insert(ConflictKey::Element(ElementId::Vertex(vid)));
        for eid in self.workspace.incident_edges(vid) {
            self.read_set
                .insert(ConflictKey::Element(ElementId::Edge(eid)));
        }
        self.workspace.neighbours(vid, relation)
    }

    /// Assemble this transaction's trace for the SSI oracle.
    ///
    /// Taken BEFORE `commit`, which consumes the transaction; the sequence it
    /// landed at comes from the outcome via
    /// [`TxnTrace::committed_at`](crate::ssi::TxnTrace::committed_at). A trace
    /// that is never marked committed models a transaction that did not commit,
    /// and forms no edges.
    pub fn trace(&self, id: usize) -> crate::ssi::TxnTrace {
        crate::ssi::TxnTrace {
            id,
            snapshot_high: self.snapshot.high(),
            commit_seq: None,
            reads: self.read_set.clone(),
            writes: self.write_set(),
        }
    }

    pub fn effects(&self) -> &[DeltaRow] {
        &self.effects
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted_at.is_some()
    }

    /// Every ordinary first-committer-wins key this transaction has written.
    ///
    /// Shared endpoint-existence dependencies deliberately do not appear here:
    /// they participate in commit certification with access modes, while this
    /// set is also the ordinary SI write set consumed by the SSI trace.
    pub fn write_set(&self) -> BTreeSet<ConflictKey> {
        let mut keys = BTreeSet::new();
        if self.claims_genesis {
            // The claim is part of what this transaction wrote: it asserted the
            // branch did not exist, and that assertion has to be able to lose.
            keys.insert(ConflictKey::CoordinateExistence);
        }
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
        let statement_base = self.next_statement_index;
        let (outcome, last_intent_ordinal) =
            evaluate_from_intent_ordinal(&self.workspace, statements, self.last_intent_ordinal);
        match outcome {
            Outcome::Aborted { statement, .. } => {
                self.last_intent_ordinal = last_intent_ordinal;
                let statement = statement_base + statement;
                self.next_statement_index = statement + 1;
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
                self.last_intent_ordinal = last_intent_ordinal;
                self.next_statement_index = statement_base + statements.len();
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

        let mine = CertificationSummary::from_transaction(&self.effects, self.claims_genesis);
        if !mine.is_empty() {
            // A coordinate that does not exist yet has nothing to conflict with,
            // and asking is a NoSuchCoordinate refusal rather than an empty set —
            // so the genesis case is answered here instead of failing open.
            let theirs = match db.certification_since(self.graph, self.branch, self.snapshot.high())
            {
                Ok(summary) => summary,
                Err(SnapshotError::NoSuchCoordinate { .. }) if self.claims_genesis => {
                    CertificationSummary::default()
                }
                Err(error) => return Err(TxnError::Snapshot(error)),
            };
            let conflicts = mine.conflicts_with(&theirs);
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
