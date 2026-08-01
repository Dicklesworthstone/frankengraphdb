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

use crate::intents::{
    CanonicalMutationPotential, Outcome, Statement, StatementFailure, evaluate_from_intent_ordinal,
};
use crate::{
    ApplyError, CertificationSummary, ConflictKey, ReferenceDatabase, ReferenceGraph, Snapshot,
    SnapshotError, Vertex, collect_conflict_keys,
};
use fgdb_delta_types::{
    CanonicalError, CoordinateEntry, DeltaRow, ElementId, LogicalDeltaTemplate, PropertyKeyId,
    RelationId, SchemaEpoch,
};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, GraphId, LogicalCommandSeq, ObjectId, VId};
use std::collections::BTreeSet;

/// A transaction reading at a fixed sequence, writing into a private workspace.
///
/// Not `Clone`: two handles to one transaction would let a caller commit the
/// same work twice under different sequences, and the type refusing to be
/// duplicated is cheaper than a rule saying not to.
///
/// The per-coordinate schema binding captured at begin: epoch plus both
/// roots. Plan:205/213 makes the binding part of snapshot identity —
/// effects are evaluated under it, so a commit may stamp only it; a
/// post-snapshot move is an asymmetric invalidation of every holder of the
/// old binding (fgdb-hdgw).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SchemaBindingAtBegin {
    epoch: SchemaEpoch,
    schema_root: ObjectId,
    constraint_root: ObjectId,
}

#[derive(Debug)]
pub struct Transaction {
    graph: GraphId,
    branch: BranchId,
    snapshot: Snapshot,
    /// The snapshot state plus this transaction's own effects so far. This is
    /// what read-your-own-writes MEANS: statements evaluate against the
    /// workspace, never against the basis or the live database.
    workspace: ReferenceGraph,
    /// The schema binding captured at begin (fgdb-hdgw): effects are
    /// evaluated under it and a commit may stamp only it. `None` on a
    /// genesis claim.
    schema_binding_at_begin: Option<SchemaBindingAtBegin>,
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
    /// Predicate domains changed by final effects, for the SSI trace only.
    ///
    /// Deliberately separate from [`CertificationSummary`]: two edge creators
    /// may write one adjacency without conflicting under SI. The domain becomes
    /// a dependency only when another transaction READ it.
    ssi_predicate_writes: BTreeSet<ConflictKey>,
    /// The latest successfully published statement generation. Generation zero
    /// is installed at begin and every successful publication advances exactly
    /// once from its predecessor.
    workspace_generation: WorkspaceGeneration,
    /// Monotone join of every successfully published statement's server-derived
    /// mutation class. Terminal-path selection reads this value, never the
    /// surviving effect count.
    cumulative_mutation_potential: CanonicalMutationPotential,
    /// Append-only reference statement lifecycle. Every accepted registration
    /// appends `Opened` before evaluation, and every call returning `Ok` appends
    /// exactly one terminal event for it. A typed publication error after
    /// evaluation may leave the final statement open; no result is visible from
    /// such an event.
    statement_events: Vec<StatementLifecycleEvent>,
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

/// Identity of one predecessor-linked transaction workspace generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceGeneration(u64);

impl WorkspaceGeneration {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    fn successor(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// One statement's terminal disposition after its mandatory Open state.
#[derive(Clone, Debug, PartialEq)]
pub enum StatementTerminal {
    /// The statement published successfully and installed exactly one successor
    /// workspace generation. This is the only terminal that publishes results.
    Published {
        predecessor: WorkspaceGeneration,
        generation: WorkspaceGeneration,
        cumulative_mutation_potential: CanonicalMutationPotential,
    },
    /// Evaluation rejected the statement. It published neither effects nor
    /// results and did not advance the workspace generation.
    Failed { failure: StatementFailure },
    /// The registered Open statement was abandoned before evaluation reached a
    /// semantic verdict. Distinct from `Failed` so retry policy need not guess.
    Abandoned,
}

/// One append-only statement lifecycle transition.
#[derive(Clone, Debug, PartialEq)]
pub enum StatementLifecycleEvent {
    /// Appended before semantic evaluation begins.
    Opened {
        statement: usize,
        predecessor: WorkspaceGeneration,
        mutation_potential: CanonicalMutationPotential,
    },
    /// Appended only after evaluation reaches exactly one terminal.
    Terminal {
        statement: usize,
        terminal: StatementTerminal,
    },
}

impl StatementLifecycleEvent {
    pub const fn statement(&self) -> usize {
        match self {
            Self::Opened { statement, .. } | Self::Terminal { statement, .. } => *statement,
        }
    }

    pub const fn terminal(&self) -> Option<&StatementTerminal> {
        match self {
            Self::Opened { .. } => None,
            Self::Terminal { terminal, .. } => Some(terminal),
        }
    }

    /// Statement result publication is exactly the `Published` terminal.
    pub const fn results_visible(&self) -> bool {
        matches!(
            self,
            Self::Terminal {
                terminal: StatementTerminal::Published { .. },
                ..
            }
        )
    }
}

/// What happened when a transaction tried to commit.
#[derive(Clone, Debug, PartialEq)]
pub enum TxnOutcome {
    /// The write terminal path. `commit_seq` is where it landed, including a
    /// mutation-capable statement whose final effect set is empty.
    WriteCommitted {
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
    /// The cumulative statement class remained `ProvenReadOnly`. No graph
    /// marker is written and no commit sequence is consumed.
    ReadClosed { statement_failures: usize },
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
    /// A statement named a stale, reused, or future workspace generation.
    WorkspaceGenerationMismatch {
        expected: WorkspaceGeneration,
        offered: WorkspaceGeneration,
    },
    /// The generation counter cannot represent the next successful
    /// publication. Refuse before publishing effects or results.
    WorkspaceGenerationExhausted { current: WorkspaceGeneration },
    /// The next statement index has no representable successor. Refuse before
    /// registering that Open event, because the cursor could not then advance
    /// without wrapping or reusing the same index.
    StatementIndexExhausted { current: usize },
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
            Self::WorkspaceGenerationMismatch { expected, offered } => write!(
                f,
                "workspace generation mismatch: expected {}, offered {}",
                expected.value(),
                offered.value()
            ),
            Self::WorkspaceGenerationExhausted { current } => write!(
                f,
                "workspace generation {} has no representable successor",
                current.value()
            ),
            Self::StatementIndexExhausted { current } => {
                write!(f, "statement index space is exhausted at {current}")
            }
            Self::Canonical(error) => write!(f, "effects are not canonical: {error:?}"),
            Self::Apply(error) => write!(f, "effects did not apply: {error}"),
        }
    }
}

impl core::error::Error for TxnError {}

/// Predicate domains one final effect may change.
///
/// Deletes need the PRE-EFFECT workspace: their durable row names the retired
/// edge identity but not the removed edge's source/relation. Collecting after
/// apply would therefore lose exactly the information the phantom witness needs.
/// Missing edges are not guessed here; the immediately following materializer
/// check rejects the malformed delete, and its transaction never gains a
/// committed trace.
fn ssi_predicate_writes_for(row: &DeltaRow, workspace: &ReferenceGraph) -> BTreeSet<ConflictKey> {
    let mut keys = BTreeSet::new();

    match row {
        DeltaRow::CreateEdge { src, relation, .. } => {
            keys.insert(ConflictKey::Adjacency {
                vertex: *src,
                relation: *relation,
            });
        }
        DeltaRow::DeleteEdge { eid, .. } => {
            if let Some(edge) = workspace.edge(*eid) {
                keys.insert(ConflictKey::Adjacency {
                    vertex: edge.src,
                    relation: edge.relation,
                });
            }
        }
        DeltaRow::DeleteVertex {
            sorted_retired_incident_edges,
            ..
        } => {
            for eid in sorted_retired_incident_edges {
                if let Some(edge) = workspace.edge(*eid) {
                    keys.insert(ConflictKey::Adjacency {
                        vertex: edge.src,
                        relation: edge.relation,
                    });
                }
            }
        }
        DeltaRow::CreateVertex { .. }
        | DeltaRow::LabelMembership { .. }
        | DeltaRow::Property { .. }
        | DeltaRow::ValidTime { .. }
        | DeltaRow::Counter { .. }
        | DeltaRow::Escrow { .. }
        | DeltaRow::Sketch { .. }
        | DeltaRow::Schema { .. }
        | DeltaRow::Constraint { .. } => {}
    }
    keys
}

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
        // The binding these effects will be evaluated under. It must come
        // from the materialized workspace, not the live coordinate: an
        // explicit historical snapshot retains its historical binding even
        // when `begin_at` is called after the live head has moved (fgdb-hdgw).
        // `None` is reserved for a genuine genesis claim, where no coordinate
        // existed for the snapshot to bind.
        let schema_binding_at_begin = (!claims_genesis).then_some(SchemaBindingAtBegin {
            epoch: workspace.schema_epoch(),
            schema_root: workspace.schema_root(),
            constraint_root: workspace.constraint_root(),
        });
        Ok(Self {
            graph: snapshot.graph(),
            branch: snapshot.branch(),
            snapshot,
            workspace,
            claims_genesis,
            schema_binding_at_begin,
            effects: Vec::new(),
            last_intent_ordinal: 0,
            read_set: BTreeSet::new(),
            ssi_predicate_writes: BTreeSet::new(),
            workspace_generation: WorkspaceGeneration::ZERO,
            cumulative_mutation_potential: CanonicalMutationPotential::ProvenReadOnly,
            statement_events: Vec::new(),
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

    /// Read the outgoing neighbours of a vertex over one relation.
    ///
    /// Three dependencies are distinct and all load-bearing: the source vertex,
    /// every matching edge that supplied a positive result, and the logical
    /// adjacency predicate — including its observed absences. Incoming edges and
    /// edges of other relations are not traversed and therefore are not element
    /// reads. The relation-qualified predicate is the reference counterpart of
    /// §7.3's production adjacency-range witness.
    pub fn read_neighbours(&mut self, vid: VId, relation: RelationId) -> Vec<VId> {
        self.read_set
            .insert(ConflictKey::Element(ElementId::Vertex(vid)));
        self.read_set.insert(ConflictKey::Adjacency {
            vertex: vid,
            relation,
        });
        for eid in self.workspace.out_edges(vid) {
            if self
                .workspace
                .edge(eid)
                .is_some_and(|edge| edge.relation == relation)
            {
                self.read_set
                    .insert(ConflictKey::Element(ElementId::Edge(eid)));
            }
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
        let mut writes = self.write_set();
        writes.extend(self.ssi_predicate_writes.iter().copied());
        crate::ssi::TxnTrace {
            id,
            snapshot_high: self.snapshot.high(),
            commit_seq: None,
            reads: self.read_set.clone(),
            writes,
        }
    }

    pub fn effects(&self) -> &[DeltaRow] {
        &self.effects
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted_at.is_some()
    }

    /// Latest successfully published workspace generation.
    pub const fn workspace_generation(&self) -> WorkspaceGeneration {
        self.workspace_generation
    }

    /// Monotone mutation class of all successfully published statements.
    pub const fn cumulative_mutation_potential(&self) -> CanonicalMutationPotential {
        self.cumulative_mutation_potential
    }

    /// Append-only Open and terminal statement events in registration order.
    pub fn statement_events(&self) -> &[StatementLifecycleEvent] {
        &self.statement_events
    }

    /// Every ordinary first-committer-wins key this transaction has written.
    ///
    /// Shared endpoint-existence dependencies and SSI adjacency predicates
    /// deliberately do not appear here. The former participate in commit
    /// certification with access modes; the latter become meaningful only
    /// against a predicate READ and are merged into [`trace`](Self::trace)
    /// separately. Keeping both out is what lets two writers add distinct edges
    /// at one hub without a write/write conflict.
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

    /// Consume the next transaction-global statement index without wrapping.
    ///
    /// The cursor advances before `Opened` is appended. Consequently an
    /// exhaustion refusal creates no lifecycle trace, while any later refusal
    /// cannot cause a retry to reuse an index that has already become visible.
    fn take_statement_index(&mut self) -> Result<usize, TxnError> {
        let current = self.next_statement_index;
        let next = current
            .checked_add(1)
            .ok_or(TxnError::StatementIndexExhausted { current })?;
        self.next_statement_index = next;
        Ok(current)
    }

    /// Evaluate `statements` against the workspace, folding their effects in.
    ///
    /// Called more than once to model a transaction that issues statements over
    /// time — which is the only way a concurrent commit can land *between* two
    /// of its statements, and therefore the only way the anomaly laws can be
    /// written at all. Each call sees the workspace the previous call left.
    pub fn execute(&mut self, statements: &[Statement]) -> Result<(), TxnError> {
        self.execute_at_generation(self.workspace_generation, statements)
    }

    /// Evaluate statements whose registration names `expected_predecessor`.
    ///
    /// A stale/reused generation and a future/gapped generation are the same
    /// typed refusal. The check runs before any statement becomes Open, so a
    /// refused registration leaves no lifecycle or workspace trace.
    pub fn execute_at_generation(
        &mut self,
        expected_predecessor: WorkspaceGeneration,
        statements: &[Statement],
    ) -> Result<(), TxnError> {
        if let Some(statement) = self.aborted_at {
            return Err(TxnError::AlreadyAborted { statement });
        }
        if expected_predecessor != self.workspace_generation {
            return Err(TxnError::WorkspaceGenerationMismatch {
                expected: self.workspace_generation,
                offered: expected_predecessor,
            });
        }

        for statement in statements {
            let statement_index = self.take_statement_index()?;
            let opened_at = self.workspace_generation;
            let mutation_potential = statement.mutation_potential();
            self.statement_events.push(StatementLifecycleEvent::Opened {
                statement: statement_index,
                predecessor: opened_at,
                mutation_potential,
            });
            let (outcome, last_intent_ordinal) = evaluate_from_intent_ordinal(
                &self.workspace,
                core::slice::from_ref(statement),
                self.last_intent_ordinal,
            );
            self.last_intent_ordinal = last_intent_ordinal;
            match outcome {
                Outcome::Aborted { failure, .. } => {
                    self.statement_events
                        .push(StatementLifecycleEvent::Terminal {
                            statement: statement_index,
                            terminal: StatementTerminal::Failed { failure },
                        });
                    self.aborted_at = Some(statement_index);
                    // Earlier statement publications retain their workspace
                    // generations, but an aborted transaction has no durable
                    // graph outcome and later statements are not opened.
                    return Ok(());
                }
                Outcome::Committed {
                    effects,
                    statement_failures,
                } => {
                    let failure_count = statement_failures.len();
                    if let Some((_, failure)) = statement_failures.into_iter().next() {
                        self.statement_failures += failure_count;
                        self.statement_events
                            .push(StatementLifecycleEvent::Terminal {
                                statement: statement_index,
                                terminal: StatementTerminal::Failed { failure },
                            });
                        continue;
                    }

                    let generation = opened_at
                        .successor()
                        .ok_or(TxnError::WorkspaceGenerationExhausted { current: opened_at })?;
                    let mut workspace = self.workspace.clone();
                    let mut predicate_writes = BTreeSet::new();
                    for row in &effects {
                        predicate_writes.extend(ssi_predicate_writes_for(row, &workspace));
                        workspace
                            .apply_row(row)
                            .map_err(|error| TxnError::Apply(Box::new(error)))?;
                    }
                    self.workspace = workspace;
                    self.ssi_predicate_writes.extend(predicate_writes);
                    self.effects.extend(effects);
                    self.cumulative_mutation_potential =
                        self.cumulative_mutation_potential.join(mutation_potential);
                    self.workspace_generation = generation;
                    self.statement_events
                        .push(StatementLifecycleEvent::Terminal {
                            statement: statement_index,
                            terminal: StatementTerminal::Published {
                                predecessor: opened_at,
                                generation,
                                cumulative_mutation_potential: self.cumulative_mutation_potential,
                            },
                        });
                }
            }
        }
        Ok(())
    }

    /// Terminally abandon one Open statement without evaluating or publishing
    /// it. Abandonment consumes a statement index but neither an intent ordinal
    /// nor a workspace generation.
    pub fn abandon_statement(&mut self, statement: &Statement) -> Result<(), TxnError> {
        self.abandon_statement_at_generation(self.workspace_generation, statement)
    }

    /// Generation-checked form of [`Self::abandon_statement`].
    pub fn abandon_statement_at_generation(
        &mut self,
        expected_predecessor: WorkspaceGeneration,
        statement: &Statement,
    ) -> Result<(), TxnError> {
        if let Some(statement) = self.aborted_at {
            return Err(TxnError::AlreadyAborted { statement });
        }
        if expected_predecessor != self.workspace_generation {
            return Err(TxnError::WorkspaceGenerationMismatch {
                expected: self.workspace_generation,
                offered: expected_predecessor,
            });
        }
        let statement_index = self.take_statement_index()?;
        self.statement_events.push(StatementLifecycleEvent::Opened {
            statement: statement_index,
            predecessor: self.workspace_generation,
            mutation_potential: statement.mutation_potential(),
        });
        self.statement_events
            .push(StatementLifecycleEvent::Terminal {
                statement: statement_index,
                terminal: StatementTerminal::Abandoned,
            });
        Ok(())
    }

    /// The statement failures accumulated so far.
    pub fn statement_failures(&self) -> usize {
        self.statement_failures
    }

    /// Try to make this transaction durable at `commit_seq` and its independent
    /// semantic `logical_command_seq`.
    ///
    /// ABORT IS DECIDED BEFORE CONFLICT, and that order is observable: a
    /// transaction that aborted never had a claim to make, so reporting it as a
    /// conflict would blame a concurrent writer for a guard this transaction
    /// chose. There is a law for it, because an aborted transaction whose writes
    /// *would* have conflicted is exactly the case where the two answers differ.
    ///
    /// Read-close is selected from the cumulative binder class, not by counting
    /// effects. A successfully published mutation-capable statement therefore
    /// follows write certification and consumes its offered commit sequence even
    /// when every effect normalized away.
    ///
    /// Consumes the transaction. A handle that survived its own commit could be
    /// committed again at a second sequence, duplicating the effects.
    pub fn commit(
        self,
        db: &mut ReferenceDatabase,
        relation: RelationId,
        intent_semantics: ObjectId,
        commit_seq: CommitSeq,
        logical_command_seq: LogicalCommandSeq,
    ) -> Result<TxnOutcome, TxnError> {
        // THE WRITE-SIDE HALF of the provenance hole: `begin_at` validates against
        // the database it began on, and this used to accept an arbitrary `&mut
        // ReferenceDatabase`. If the before-images happened to match, a capability
        // minted from database A committed its effects into B
        // (fgdb-reference-snapshot-provenance-9bvm). Checked BEFORE the abort arm,
        // because a transaction pointed at the wrong database has not earned a
        // verdict about its own guards.
        db.check_provenance(&self.snapshot)
            .map_err(TxnError::Snapshot)?;

        if let Some(statement) = self.aborted_at {
            return Ok(TxnOutcome::Aborted { statement });
        }

        if self.cumulative_mutation_potential.is_proven_read_only() {
            return Ok(TxnOutcome::ReadClosed {
                statement_failures: self.statement_failures,
            });
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

        let effects = self.effects.len();
        // The snapshot's schema binding is part of its identity (plan:205/213):
        // these effects were evaluated under the binding captured at begin, so
        // the commit may stamp ONLY that binding. A Schema/Constraint move
        // since is an asymmetric invalidation of every holder of the old
        // binding — refuse exactly, never restamp old effects onto the new
        // binding (fgdb-hdgw; li8d's live-epoch read was exactly that
        // failure). Disjoint writers under an UNCHANGED binding are untouched:
        // no dependency enters any write set.
        let schema_epoch = match self.schema_binding_at_begin {
            Some(at_begin) => {
                if let Some(coordinate) = db.graph(self.graph, self.branch) {
                    if coordinate.schema_epoch() != at_begin.epoch {
                        return Err(TxnError::Apply(Box::new(
                            ApplyError::SchemaBindingMismatch {
                                graph: self.graph,
                                branch: self.branch,
                                relation,
                                declared: at_begin.epoch,
                                actual: coordinate.schema_epoch(),
                            },
                        )));
                    }
                    if coordinate.schema_root() != at_begin.schema_root {
                        return Err(TxnError::Apply(Box::new(
                            ApplyError::ConstraintRootMismatch {
                                declared_schema_root: at_begin.schema_root,
                                actual_schema_root: coordinate.schema_root(),
                            },
                        )));
                    }
                    if coordinate.constraint_root() != at_begin.constraint_root {
                        return Err(TxnError::Apply(Box::new(
                            ApplyError::ConstraintBindingMismatch {
                                declared_constraint_root: at_begin.constraint_root,
                                actual_constraint_root: coordinate.constraint_root(),
                            },
                        )));
                    }
                }
                at_begin.epoch
            }
            // The genesis claim: the coordinate did not exist at begin. The
            // first-committer-wins machinery refuses a loser before apply, so
            // the surviving first commit establishes the initial epoch.
            None => SchemaEpoch(0),
        };
        let template = LogicalDeltaTemplate::build(
            intent_semantics,
            [0u8; 32],
            vec![CoordinateEntry {
                graph: self.graph,
                branch: self.branch,
                relation,
                schema_epoch,
                schema_transition: None,
                rows: self.effects,
            }],
        )
        .map_err(TxnError::Canonical)?;
        db.apply_template(&template, commit_seq, logical_command_seq)
            .map_err(|error| TxnError::Apply(Box::new(error)))?;

        Ok(TxnOutcome::WriteCommitted {
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
            Self::WriteCommitted {
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
        matches!(self, Self::WriteCommitted { .. })
    }
}

/// A statement failure paired with the statement it came from, for callers that
/// want to inspect what survived a partially-failed transaction.
pub type IndexedFailure = (usize, StatementFailure);

#[cfg(test)]
mod overflow_internal_tests {
    use super::{StatementTerminal, Transaction, TxnError};
    use crate::ReferenceDatabase;
    use crate::intents::{Intent, Statement, StatementFailure};
    use fgdb_types::{BranchId, GraphId, VId};

    fn transaction() -> Transaction {
        Transaction::begin_genesis(&ReferenceDatabase::new(), GraphId(1), BranchId(1))
            .expect("genesis transaction begins")
    }

    #[test]
    fn statement_index_exhaustion_registers_no_lifecycle_event() {
        let expected = Err(TxnError::StatementIndexExhausted {
            current: usize::MAX,
        });

        let mut executing = transaction();
        executing.next_statement_index = usize::MAX - 1;
        assert_eq!(executing.execute(&[Statement::read_only()]), Ok(()));
        assert_eq!(executing.next_statement_index, usize::MAX);
        let settled_events = executing.statement_events().to_vec();
        assert_eq!(executing.execute(&[Statement::read_only()]), expected);
        assert_eq!(executing.statement_events(), settled_events);

        let mut abandoning = transaction();
        abandoning.next_statement_index = usize::MAX;
        assert_eq!(
            abandoning.abandon_statement(&Statement::read_only()),
            Err(TxnError::StatementIndexExhausted {
                current: usize::MAX,
            })
        );
        assert!(abandoning.statement_events().is_empty());
    }

    #[test]
    fn intent_ordinal_exhaustion_aborts_without_publication() {
        let mut transaction = transaction();
        transaction.last_intent_ordinal = u64::MAX;
        let statement = Statement::new(vec![Intent::EnsureVertex {
            vid: VId(1),
            labels: vec![],
            props: vec![],
        }]);

        assert_eq!(transaction.execute(&[statement]), Ok(()));
        assert!(transaction.is_aborted());
        assert!(transaction.effects().is_empty());
        assert_eq!(transaction.last_intent_ordinal, u64::MAX);
        assert!(matches!(
            transaction
                .statement_events()
                .last()
                .and_then(|event| event.terminal()),
            Some(StatementTerminal::Failed {
                failure: StatementFailure::IntentOrdinalExhausted { current }
            }) if *current == u64::MAX
        ));
    }
}
