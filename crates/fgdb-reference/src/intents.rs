//! The Appendix B intent layer: deterministic commands, and what they reduce to.
//!
//! plan:2731 is emphatic about what an intent is NOT: "Intents are deterministic
//! commands captured at statement execution; they are **not** committed effects
//! or Ripple deltas." Everything else in this crate consumes *effects*. This
//! module is the step before that — §9.1's "Finalization evaluates all commands
//! and emits only canonical effects".
//!
//! Modelling that step separately is what makes three semantics testable that
//! are invisible when you only ever look at effects:
//!
//! 1. **The mismatch trichotomy.** `CompareAndSet` declares what a failed
//!    precondition MEANS: `NoOp`, `StatementError`, or `TxnAbort`. The same
//!    failed comparison produces three different outcomes, and an
//!    implementation that conflates any two of them is wrong in a way no
//!    effect-level test can see, because the conflated cases emit the same
//!    effects — they differ in what SURVIVES.
//!
//! 2. **Multi-statement error policy.** A `StatementError` kills one statement
//!    and leaves earlier statements' effects intact; a `TxnAbort` kills the
//!    transaction and leaves nothing. Distinguishing them requires the
//!    statement boundary to be real, not a formatting convention.
//!
//! 3. **Read-your-own-writes.** Intents are evaluated in order against state
//!    that already includes the effects of earlier intents *in the same
//!    transaction*. Without this, `CompareAndSet` after a `SetProp` in the same
//!    transaction would compare against a value the transaction had already
//!    replaced — and a transaction that cannot see its own writes is not a
//!    transaction.
//!
//! SUBSET NOTE (doctrine 7). Appendix B lists eighteen intent kinds; nine are
//! here — one of them narrowed to a name that does not overclaim
//! (`EnsureEdgeByTriple`, see its own note) — the ones whose reduction has semantics rather than being a direct
//! transcription. The delete family earns its place on the cascade alone: the
//! retired-edge image is COMPUTED by finalization and checked for equality by the
//! materializer, so it is the sharpest instance in the vocabulary of the rule that
//! an intent declares what it wants and an effect declares what was true. `AdjustCounter`, `SketchUpdate`, escrow, valid-time and schema
//! intents each need machinery that belongs to other beads (registered algebra
//! profiles, sketch profiles, valid-time contracts, the schema catalog), and
//! guessing at them here would be worse than their absence. What is here is a
//! subset of the final vocabulary, not a substitute for it.

use crate::{ApplyError, ConflictKey, ReferenceGraph};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::BTreeSet;

/// What a failed `CompareAndSet` precondition means (Appendix B, verbatim
/// vocabulary: `mismatch: NoOp|StatementError|TxnAbort`).
///
/// A closed union because the caller must choose: there is no defensible
/// default. Silently picking `NoOp` would make a failed guard invisible, and
/// silently picking `TxnAbort` would let one optimistic check destroy unrelated
/// work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MismatchPolicy {
    /// The intent does nothing. The statement continues.
    NoOp,
    /// The statement fails. Its own effects are discarded; earlier statements
    /// keep theirs.
    StatementError,
    /// The whole transaction aborts and produces no effects at all.
    TxnAbort,
}

/// A deterministic command captured at statement execution.
#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    CreateVertex {
        vid: VId,
        labels: Vec<LabelId>,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
    AddEdge {
        eid: EId,
        src: VId,
        etype: RelationId,
        dst: VId,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
    SetProp {
        elem: ElementId,
        name: PropertyKeyId,
        value: CanonicalScalar,
    },
    /// Create the edge only if no edge with this exact `(src, etype, dst)` triple
    /// already exists. IDEMPOTENT by construction: the second evaluation emits
    /// nothing.
    ///
    /// **RENAMED FROM `EnsureEdge`, AND THE RENAME IS THE FIX**
    /// (fgdb-ensure-edge-constraint-counterfeit-xa2x). Appendix B defines
    /// `EnsureEdge {src, etype, dst, constraint_id, props}`, where the named
    /// constraint decides what "already exists" MEANS — which key the uniqueness is
    /// evaluated over, and whether properties participate in it. The variant here
    /// destructured `constraint_id` with `..` and matched on the raw triple, so an
    /// unknown constraint, a wrong-domain constraint and the right constraint all
    /// produced the same answer, and an unrelated existing triple could suppress a
    /// creation no validated key observation supports. Deleting the field changed no
    /// behaviour and left every test green: the counterfeit signature exactly.
    ///
    /// Honest triple-uniqueness is a genuinely useful primitive, so the capability
    /// stays and the CLAIM is what shrinks. Doctrine 7 permits a subset of a final
    /// abstraction and forbids a substitute for it; a variant named `EnsureEdge`
    /// that ignores its constraint is a substitute, while one named
    /// `EnsureEdgeByTriple` that takes no constraint is a subset — it does less and
    /// says so, and cannot be mistaken for the full thing.
    ///
    /// The constraint-keyed form needs canonical constraint state and key
    /// evaluation, which belong to the schema catalog (fgdb-w4-schema-catalog); it
    /// must land with those facts rather than by adding the parameter back.
    ///
    /// `eid` is caller-supplied, as it is for every create in this crate, and
    /// Appendix B's arm allocates identity instead. That is a modelling shortcut the
    /// whole crate shares — not a claim that this is the normative allocation
    /// contract.
    EnsureEdgeByTriple {
        eid: EId,
        src: VId,
        etype: RelationId,
        dst: VId,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
    /// Retire a vertex and everything hanging off it.
    ///
    /// THE CASCADE IS COMPUTED, NEVER SUPPLIED. `DeltaRow::DeleteVertex` carries
    /// `sorted_retired_incident_edges`, and the materializer checks that image
    /// for EQUALITY with the actual incident set — too few leaves a dangling
    /// edge, too many claims a retirement that never happened. Finalization is
    /// the step that knows the answer, so this intent takes no edge list. Letting
    /// a caller pass one would make the cascade an assertion the caller could get
    /// wrong, and the whole point of §9.1's finalization is that it cannot be.
    DeleteVertex { vid: VId },
    /// Retire one edge.
    DeleteEdge { eid: EId },
    /// Remove a property, if it is there.
    ///
    /// Distinct from `SetProp` with a null value: an absent property and a
    /// property holding null are different states, and Appendix B's before/after
    /// images spell the difference as `None` versus `Some(Null)`. Collapsing them
    /// would make removal unexpressible.
    RemoveProp {
        elem: ElementId,
        name: PropertyKeyId,
    },
    /// Create the vertex only if it does not already exist. IDEMPOTENT by
    /// construction, the vertex counterpart of `EnsureEdgeByTriple`.
    EnsureVertex {
        vid: VId,
        labels: Vec<LabelId>,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
    /// Set `name` to `value` only if it currently equals `expected`.
    CompareAndSet {
        elem: ElementId,
        name: PropertyKeyId,
        expected: Option<CanonicalScalar>,
        value: CanonicalScalar,
        mismatch: MismatchPolicy,
    },
}

/// The binder-derived upper bound on one statement's mutation capability.
///
/// The order is the conservative monotone join order used by a transaction's
/// workspace. Once a published statement raises the cumulative value, a later
/// read-only statement cannot lower it and thereby gain the read-close path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonicalMutationPotential {
    #[default]
    ProvenReadOnly,
    MayMutateGraph,
    MayMutateSchema,
    MayInvokeReplicatedProcedure,
}

impl CanonicalMutationPotential {
    /// Conservative monotone join for a predecessor-linked workspace.
    pub fn join(self, next: Self) -> Self {
        self.max(next)
    }

    pub const fn is_proven_read_only(self) -> bool {
        matches!(self, Self::ProvenReadOnly)
    }
}

/// One statement's worth of intents plus its server-derived mutation class.
///
/// Every intent currently modelled by this reference crate is a graph mutation
/// intent, even when evaluation later reduces it to no effect. Consequently
/// [`Statement::new`] is always `MayMutateGraph`; an empty vector passed there
/// remains a mutation-capable semantic no-op. A genuinely read-only statement
/// is constructed with [`Statement::read_only`]. Keeping the field private
/// prevents a caller from attaching mutation intents to a read-only class.
#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    intents: Vec<Intent>,
    mutation_potential: CanonicalMutationPotential,
}

impl Statement {
    pub fn new(intents: Vec<Intent>) -> Self {
        Self {
            intents,
            mutation_potential: CanonicalMutationPotential::MayMutateGraph,
        }
    }

    /// A successfully publishable statement with no mutation intents.
    pub fn read_only() -> Self {
        Self {
            intents: Vec::new(),
            mutation_potential: CanonicalMutationPotential::ProvenReadOnly,
        }
    }

    pub const fn mutation_potential(&self) -> CanonicalMutationPotential {
        self.mutation_potential
    }

    fn intents(&self) -> &[Intent] {
        &self.intents
    }
}

/// Why a statement failed.
#[derive(Clone, Debug, PartialEq)]
pub enum StatementFailure {
    /// A `CompareAndSet` precondition failed under `StatementError`.
    ///
    /// The scalars are boxed because a `CanonicalScalar` carries owned text, and
    /// an enum as wide as its largest payload makes every `Result` carrying it
    /// that wide too.
    Mismatch {
        elem: ElementId,
        name: PropertyKeyId,
        expected: Option<Box<CanonicalScalar>>,
        actual: Option<Box<CanonicalScalar>>,
    },
    /// An intent reduced to an effect the graph refused.
    Rejected(ApplyError),
    /// The transaction has consumed the largest representable source-intent
    /// ordinal, so another intent cannot be assigned a unique birth-order
    /// component without wrapping.
    IntentOrdinalExhausted { current: u64 },
    /// One intent named the same property twice with DIFFERENT values
    /// (fgdb-intent-conflicting-property-order-btxr).
    ///
    /// Refused rather than resolved. The previous implementation sorted by key and
    /// deduplicated, which — because the sort is stable — silently kept whichever
    /// value the caller listed first. That invents a first-write policy Appendix B
    /// never specified, and it means the SAME logical request submitted with its
    /// property list in a different order produces a different effect digest. Since
    /// the digest is the object identity the capsule, the marker and every
    /// downstream cross-check compare, "the same request" would name two different
    /// durable objects.
    ///
    /// `fgdb-delta-types`' canonical form already declares repeated property keys
    /// invalid and plan:2805 requires the net-effect normal form to reject
    /// incompatible terminal values, so refusing is what the surrounding contracts
    /// already say. Identical duplicates are still collapsed: they express one
    /// fact twice rather than two conflicting facts.
    ConflictingPropertyValues {
        property: PropertyKeyId,
        first: Box<CanonicalScalar>,
        second: Box<CanonicalScalar>,
    },
}

impl StatementFailure {
    /// The expected and actual values of a mismatch, or `None` for any other
    /// failure. An accessor so a caller can `.expect(..)` rather than write an
    /// irrefutable-pattern escape hatch.
    #[allow(clippy::type_complexity)]
    pub fn mismatch_values(&self) -> Option<(Option<&CanonicalScalar>, Option<&CanonicalScalar>)> {
        match self {
            Self::Mismatch {
                expected, actual, ..
            } => Some((expected.as_deref(), actual.as_deref())),
            // Exhaustive rather than a wildcard: a new failure variant should stop
            // this crate compiling until someone decides whether it carries a
            // before/after pair, instead of silently reporting that it does not.
            Self::Rejected(_)
            | Self::IntentOrdinalExhausted { .. }
            | Self::ConflictingPropertyValues { .. } => None,
        }
    }

    /// The property and both values of a conflicting-duplicate refusal.
    pub fn conflicting_values(
        &self,
    ) -> Option<(PropertyKeyId, &CanonicalScalar, &CanonicalScalar)> {
        match self {
            Self::ConflictingPropertyValues {
                property,
                first,
                second,
            } => Some((*property, first, second)),
            Self::Mismatch { .. } | Self::Rejected(_) | Self::IntentOrdinalExhausted { .. } => None,
        }
    }
}

/// The committed halves of an [`Outcome`]: its effects and its per-statement
/// failures.
pub type CommittedParts<'a> = (&'a [DeltaRow], &'a [(usize, StatementFailure)]);

/// What evaluating a transaction produced.
///
/// `Committed` carries the effects AND the per-statement failures, because a
/// statement error is not a transaction failure: the plan's multi-statement
/// policy requires the transaction to continue and the caller to learn which
/// statements did not take effect. Returning only effects would make a
/// half-applied transaction indistinguishable from a fully-applied one.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Committed {
        effects: Vec<DeltaRow>,
        statement_failures: Vec<(usize, StatementFailure)>,
    },
    /// A `TxnAbort` mismatch. NO effects — not the effects up to the abort.
    Aborted {
        statement: usize,
        failure: StatementFailure,
    },
}

impl Outcome {
    pub fn effects(&self) -> &[DeltaRow] {
        match self {
            Self::Committed { effects, .. } => effects,
            Self::Aborted { .. } => &[],
        }
    }

    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted { .. })
    }

    /// The committed parts, or `None` if this transaction aborted.
    ///
    /// An accessor rather than making callers match, so a test can say
    /// `.expect("committed")` instead of a `panic!` arm — which keeps the
    /// project's UBS panic-class ratchet where it is without weakening the
    /// assertion.
    pub fn committed_parts(&self) -> Option<CommittedParts<'_>> {
        match self {
            Self::Committed {
                effects,
                statement_failures,
            } => Some((effects, statement_failures)),
            Self::Aborted { .. } => None,
        }
    }

    /// The abort's statement index and failure, or `None` if it committed.
    pub fn aborted_parts(&self) -> Option<(usize, &StatementFailure)> {
        match self {
            Self::Aborted { statement, failure } => Some((*statement, failure)),
            Self::Committed { .. } => None,
        }
    }
}

/// Evaluate a transaction's statements in order against `basis`, reducing
/// intents to canonical effects.
///
/// `basis` is NOT mutated. Evaluation runs against a scratch copy that
/// accumulates effects as it goes — which is what gives read-your-own-writes —
/// and the caller applies the returned effects to the real graph. That split
/// matters: finalization must be able to produce effects without committing
/// them, because the commit protocol needs the capsule built before D1.
pub fn evaluate(basis: &ReferenceGraph, statements: &[Statement]) -> Outcome {
    evaluate_from_intent_ordinal(basis, statements, 0).0
}

/// Evaluate from the last intent ordinal already consumed by this transaction.
///
/// The returned ordinal is the last intent actually visited, including no-ops
/// and statement failures. [`evaluate`] starts a standalone evaluation at zero;
/// [`crate::txn::Transaction`] carries the returned cursor across its repeatable
/// `execute` calls so splitting one statement stream cannot reset birth order.
/// The final component is the logical read set observed while reducing visited
/// intents. It is separate from durable effects because a successful conditional
/// no-op still read the value that made it a no-op.
pub(crate) fn evaluate_from_intent_ordinal(
    basis: &ReferenceGraph,
    statements: &[Statement],
    mut intent_ordinal: u64,
) -> (Outcome, u64, BTreeSet<ConflictKey>) {
    let mut scratch = basis.clone();
    let mut effects: Vec<DeltaRow> = Vec::new();
    let mut statement_failures: Vec<(usize, StatementFailure)> = Vec::new();
    let mut captured_reads = BTreeSet::new();
    // THE CANONICAL INTENT ORDINAL, and it is deliberately not a graph
    // cardinality (fgdb-intent-birth-ordinal-cardinality-spa1).
    //
    // Appendix B and plan:223 define a sequence-neutral BirthOrdinal from
    // intent_ordinal, merge_ordinal and element_id; a later CommitSeq creates
    // OriginBirthOrder. `state.vertex_count() + 1` is none of those facts. It
    // moves when unrelated elements are present or retired, so deleting something
    // untouched could make a later birth field move BACKWARD and alias a
    // different source intent; it kept separate counters for vertex and edge
    // creates, so a mixed statement minted colliding ordinals; and it was bound
    // to mutable population rather than to the published intent order. The
    // comment defending it — "never supplied by a caller, two callers choosing
    // their own would collide" — argued for the wrong property: being derived
    // rather than supplied does not make a derivation sound.
    //
    // Counted over EVERY intent actually visited, in statement/intent order,
    // whatever each one reduces to. The ordinal is therefore a function of the
    // evaluated request prefix, not graph cardinality. A basis-dependent early
    // statement failure can truncate that prefix and shift later ordinals; among
    // the visited source intents, however, each ordinal remains unique and cannot
    // move because unrelated graph elements were added or retired.
    //
    // SUBSET NOTE (doctrine 7): merge_ordinal and permanent element identity are
    // still absent, and are not fabricated from state to stand in for the missing
    // pieces. This is the intent_ordinal component alone, honestly narrow.
    for (index, statement) in statements.iter().enumerate() {
        // Each statement is evaluated on its own scratch so that a
        // StatementError can discard exactly its own effects — a statement is
        // the unit of partial failure, so it has to be the unit of rollback.
        let mut statement_scratch = scratch.clone();
        let mut statement_effects: Vec<DeltaRow> = Vec::new();
        let mut failure: Option<StatementFailure> = None;

        for intent in statement.intents() {
            let Some(next_intent_ordinal) = intent_ordinal.checked_add(1) else {
                return (
                    Outcome::Aborted {
                        statement: index,
                        failure: StatementFailure::IntentOrdinalExhausted {
                            current: intent_ordinal,
                        },
                    },
                    intent_ordinal,
                    captured_reads,
                );
            };
            intent_ordinal = next_intent_ordinal;
            if let Intent::CompareAndSet { elem, .. } = intent {
                captured_reads.insert(ConflictKey::Element(*elem));
            }
            match reduce(&statement_scratch, intent, intent_ordinal) {
                Reduction::Effects(rows) => {
                    for row in rows {
                        if let Err(error) = statement_scratch.apply_row(&row) {
                            failure = Some(StatementFailure::Rejected(error));
                            break;
                        }
                        statement_effects.push(row);
                    }
                    if failure.is_some() {
                        break;
                    }
                }
                Reduction::Nothing => {}
                Reduction::Failed(f) => {
                    failure = Some(f);
                    break;
                }
                Reduction::Abort(f) => {
                    return (
                        Outcome::Aborted {
                            statement: index,
                            failure: f,
                        },
                        intent_ordinal,
                        captured_reads,
                    );
                }
            }
        }

        match failure {
            None => {
                // The statement succeeded: its effects join the transaction and
                // become visible to later statements.
                scratch = statement_scratch;
                effects.extend(statement_effects);
            }
            Some(f) => statement_failures.push((index, f)),
        }
    }

    (
        Outcome::Committed {
            effects,
            statement_failures,
        },
        intent_ordinal,
        captured_reads,
    )
}

/// What one intent reduces to against a given state.
enum Reduction {
    Effects(Vec<DeltaRow>),
    /// The intent legitimately produces nothing — `EnsureEdgeByTriple` on an existing
    /// edge, or a `NoOp` mismatch. Distinct from failure: nothing went wrong.
    Nothing,
    Failed(StatementFailure),
    Abort(StatementFailure),
}

/// Reduce one intent against `state`, at canonical intent ordinal `ordinal`.
///
/// `ordinal` is the intent's position in the visited prefix of this evaluation,
/// counted in statement/intent order. It is independent of graph cardinality;
/// see the basis-dependent early-exit boundary in the birth-ordinal note in
/// [`evaluate`].
fn reduce(state: &ReferenceGraph, intent: &Intent, ordinal: u64) -> Reduction {
    match intent {
        Intent::CreateVertex { vid, labels, props } => {
            Reduction::Effects(vec![DeltaRow::CreateVertex {
                vid: *vid,
                birth_ordinal: ordinal,
                labels: sorted_labels(labels),
                props: match canonical_props(props) {
                    Ok(props) => props,
                    Err(failure) => return Reduction::Failed(failure),
                },
                valid_time: None,
            }])
        }
        Intent::AddEdge {
            eid,
            src,
            etype,
            dst,
            props,
        } => Reduction::Effects(vec![DeltaRow::CreateEdge {
            eid: *eid,
            birth_ordinal: ordinal,
            src: *src,
            relation: *etype,
            dst: *dst,
            canonical_key: None,
            props: match canonical_props(props) {
                Ok(props) => props,
                Err(failure) => return Reduction::Failed(failure),
            },
            valid_time: None,
        }]),
        Intent::SetProp { elem, name, value } => {
            // The before image comes from the STATE, not the caller. That is the
            // difference between an intent and an effect: an effect declares its
            // before image and finalization is what computes it.
            let before = property_of(state, *elem, *name);
            if before.as_ref() == Some(value) {
                // Setting a property to what it already holds emits no effect —
                // a no-op write is not a change, and emitting one would put a
                // row in the delta stream that changes nothing.
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::Property {
                elem: *elem,
                property: *name,
                before,
                after: Some(value.clone()),
            }])
        }
        Intent::EnsureEdgeByTriple {
            eid,
            src,
            etype,
            dst,
            props,
        } => {
            if edge_exists(state, *src, *etype, *dst) {
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::CreateEdge {
                eid: *eid,
                birth_ordinal: ordinal,
                src: *src,
                relation: *etype,
                dst: *dst,
                canonical_key: None,
                props: match canonical_props(props) {
                    Ok(props) => props,
                    Err(failure) => return Reduction::Failed(failure),
                },
                valid_time: None,
            }])
        }
        Intent::DeleteVertex { vid } => {
            let Some(vertex) = state.vertex(*vid) else {
                // Deleting what is not there emits nothing rather than failing.
                // A delete is a statement about the END state, and the end state
                // is already what was asked for — the same reading that makes
                // SetProp-to-the-current-value a no-op.
                return Reduction::Nothing;
            };
            Reduction::Effects(vec![DeltaRow::DeleteVertex {
                vid: *vid,
                // The stable VId names every version. Finalization captures the
                // exact current system-time version so materialization is a
                // compare-and-set rather than an unconditional retirement.
                before_version: vertex.version,
                // COMPUTED from the state being finalized against. `incident_edges`
                // returns them sorted and deduplicated, which is what the
                // materializer's equality check demands — a self-loop appears once,
                // not twice, though it is both an in-edge and an out-edge.
                sorted_retired_incident_edges: state.incident_edges(*vid),
            }])
        }
        Intent::DeleteEdge { eid } => {
            let Some(edge) = state.edge(*eid) else {
                return Reduction::Nothing;
            };
            Reduction::Effects(vec![DeltaRow::DeleteEdge {
                eid: *eid,
                before_version: edge.version,
            }])
        }
        Intent::RemoveProp { elem, name } => {
            let before = property_of(state, *elem, *name);
            if before.is_none() {
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::Property {
                elem: *elem,
                property: *name,
                before,
                after: None,
            }])
        }
        Intent::EnsureVertex { vid, labels, props } => {
            if state.vertex(*vid).is_some() {
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::CreateVertex {
                vid: *vid,
                birth_ordinal: ordinal,
                labels: sorted_labels(labels),
                props: match canonical_props(props) {
                    Ok(props) => props,
                    Err(failure) => return Reduction::Failed(failure),
                },
                valid_time: None,
            }])
        }
        Intent::CompareAndSet {
            elem,
            name,
            expected,
            value,
            mismatch,
        } => {
            let actual = property_of(state, *elem, *name);
            if actual != *expected {
                let failure = StatementFailure::Mismatch {
                    elem: *elem,
                    name: *name,
                    expected: expected.clone().map(Box::new),
                    actual: actual.map(Box::new),
                };
                return match mismatch {
                    MismatchPolicy::NoOp => Reduction::Nothing,
                    MismatchPolicy::StatementError => Reduction::Failed(failure),
                    MismatchPolicy::TxnAbort => Reduction::Abort(failure),
                };
            }
            if actual.as_ref() == Some(value) {
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::Property {
                elem: *elem,
                property: *name,
                before: actual,
                after: Some(value.clone()),
            }])
        }
    }
}

fn property_of(
    state: &ReferenceGraph,
    elem: ElementId,
    name: PropertyKeyId,
) -> Option<CanonicalScalar> {
    match elem {
        ElementId::Vertex(vid) => state.vertex(vid)?.props.get(&name).cloned(),
        ElementId::Edge(eid) => state.edge(eid)?.props.get(&name).cloned(),
    }
}

fn edge_exists(state: &ReferenceGraph, src: VId, etype: RelationId, dst: VId) -> bool {
    state
        .out_edges(src)
        .into_iter()
        .filter_map(|eid| state.edge(eid))
        .any(|edge| edge.relation == etype && edge.dst == dst)
}

/// Labels and properties are canonically ordered on the way OUT of finalization,
/// so the effects an intent reduces to are already in the form the delta
/// encoding requires — the intent layer is where caller-supplied order stops
/// mattering.
fn sorted_labels(labels: &[LabelId]) -> Vec<LabelId> {
    let mut out = labels.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

/// Canonicalize one intent's property list, or refuse it.
///
/// Sorting alone is not canonicalization when the input can be contradictory: a
/// stable sort preserves the caller's relative order among duplicates, so a
/// dedup silently resolves a conflict by submission order. Sorting the complete
/// `(key, value)` pair also gives a conflicting refusal one canonical value
/// order: the failure payload is observable and must not retain caller order
/// after the effects have been discarded. Identical duplicates collapse;
/// conflicting ones are a statement failure naming the property and both values.
fn canonical_props(
    props: &[(PropertyKeyId, CanonicalScalar)],
) -> Result<Vec<(PropertyKeyId, CanonicalScalar)>, StatementFailure> {
    let mut out = props.to_vec();
    out.sort();
    let mut deduped: Vec<(PropertyKeyId, CanonicalScalar)> = Vec::with_capacity(out.len());
    for (key, value) in out {
        match deduped.last() {
            Some((previous, seen)) if *previous == key => {
                if seen != &value {
                    return Err(StatementFailure::ConflictingPropertyValues {
                        property: key,
                        first: Box::new(seen.clone()),
                        second: Box::new(value),
                    });
                }
                // Identical: one fact stated twice.
            }
            _ => deduped.push((key, value)),
        }
    }
    Ok(deduped)
}

#[cfg(test)]
mod overflow_internal_tests {
    use super::{Intent, Outcome, Statement, StatementFailure, evaluate_from_intent_ordinal};
    use crate::ReferenceGraph;
    use fgdb_types::VId;

    #[test]
    fn intent_ordinal_exhaustion_aborts_without_wrapping() {
        let statement = Statement::new(vec![Intent::EnsureVertex {
            vid: VId(1),
            labels: vec![],
            props: vec![],
        }]);

        let (outcome, cursor, captured_reads) =
            evaluate_from_intent_ordinal(&ReferenceGraph::default(), &[statement], u64::MAX);

        assert_eq!(cursor, u64::MAX);
        assert!(captured_reads.is_empty());
        assert!(outcome.effects().is_empty());
        assert_eq!(
            outcome,
            Outcome::Aborted {
                statement: 0,
                failure: StatementFailure::IntentOrdinalExhausted { current: u64::MAX },
            }
        );
    }
}
