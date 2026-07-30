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
//! here — the ones whose reduction has semantics rather than being a direct
//! transcription. The delete family earns its place on the cascade alone: the
//! retired-edge image is COMPUTED by finalization and checked for equality by the
//! materializer, so it is the sharpest instance in the vocabulary of the rule that
//! an intent declares what it wants and an effect declares what was true. `AdjustCounter`, `SketchUpdate`, escrow, valid-time and schema
//! intents each need machinery that belongs to other beads (registered algebra
//! profiles, sketch profiles, valid-time contracts, the schema catalog), and
//! guessing at them here would be worse than their absence. What is here is a
//! subset of the final vocabulary, not a substitute for it.

use crate::{ApplyError, ReferenceGraph};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, ObjectId, VId};

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
    /// Create the edge only if no edge with this `(src, etype, dst)` already
    /// exists. IDEMPOTENT by construction: the second evaluation emits nothing.
    EnsureEdge {
        eid: EId,
        src: VId,
        etype: RelationId,
        dst: VId,
        constraint_id: ObjectId,
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
    /// construction, the vertex counterpart of `EnsureEdge`.
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

/// One statement's worth of intents.
#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    pub intents: Vec<Intent>,
}

impl Statement {
    pub fn new(intents: Vec<Intent>) -> Self {
        Self { intents }
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
            Self::Rejected(_) => None,
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
    let mut scratch = basis.clone();
    let mut effects: Vec<DeltaRow> = Vec::new();
    let mut statement_failures: Vec<(usize, StatementFailure)> = Vec::new();

    for (index, statement) in statements.iter().enumerate() {
        // Each statement is evaluated on its own scratch so that a
        // StatementError can discard exactly its own effects — a statement is
        // the unit of partial failure, so it has to be the unit of rollback.
        let mut statement_scratch = scratch.clone();
        let mut statement_effects: Vec<DeltaRow> = Vec::new();
        let mut failure: Option<StatementFailure> = None;

        for intent in &statement.intents {
            match reduce(&statement_scratch, intent) {
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
                    return Outcome::Aborted {
                        statement: index,
                        failure: f,
                    };
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

    Outcome::Committed {
        effects,
        statement_failures,
    }
}

/// What one intent reduces to against a given state.
enum Reduction {
    Effects(Vec<DeltaRow>),
    /// The intent legitimately produces nothing — `EnsureEdge` on an existing
    /// edge, or a `NoOp` mismatch. Distinct from failure: nothing went wrong.
    Nothing,
    Failed(StatementFailure),
    Abort(StatementFailure),
}

fn reduce(state: &ReferenceGraph, intent: &Intent) -> Reduction {
    match intent {
        Intent::CreateVertex { vid, labels, props } => {
            Reduction::Effects(vec![DeltaRow::CreateVertex {
                vid: *vid,
                // Birth ordinal is derived from the state the intent is
                // finalized against, never supplied by a caller: two callers
                // choosing their own would collide.
                birth_ordinal: state.vertex_count() as u64 + 1,
                labels: sorted_labels(labels),
                props: sorted_props(props),
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
            birth_ordinal: state.edge_count() as u64 + 1,
            src: *src,
            relation: *etype,
            dst: *dst,
            canonical_key: None,
            props: sorted_props(props),
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
        Intent::EnsureEdge {
            eid,
            src,
            etype,
            dst,
            props,
            ..
        } => {
            if edge_exists(state, *src, *etype, *dst) {
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::CreateEdge {
                eid: *eid,
                birth_ordinal: state.edge_count() as u64 + 1,
                src: *src,
                relation: *etype,
                dst: *dst,
                canonical_key: None,
                props: sorted_props(props),
                valid_time: None,
            }])
        }
        Intent::DeleteVertex { vid } => {
            if state.vertex(*vid).is_none() {
                // Deleting what is not there emits nothing rather than failing.
                // A delete is a statement about the END state, and the end state
                // is already what was asked for — the same reading that makes
                // SetProp-to-the-current-value a no-op.
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::DeleteVertex {
                vid: *vid,
                before_version: ObjectId([0u8; 32]),
                // COMPUTED from the state being finalized against. `incident_edges`
                // returns them sorted and deduplicated, which is what the
                // materializer's equality check demands — a self-loop appears once,
                // not twice, though it is both an in-edge and an out-edge.
                sorted_retired_incident_edges: state.incident_edges(*vid),
            }])
        }
        Intent::DeleteEdge { eid } => {
            if state.edge(*eid).is_none() {
                return Reduction::Nothing;
            }
            Reduction::Effects(vec![DeltaRow::DeleteEdge {
                eid: *eid,
                before_version: ObjectId([0u8; 32]),
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
                birth_ordinal: state.vertex_count() as u64 + 1,
                labels: sorted_labels(labels),
                props: sorted_props(props),
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

fn sorted_props(
    props: &[(PropertyKeyId, CanonicalScalar)],
) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    let mut out = props.to_vec();
    out.sort_by_key(|(key, _)| *key);
    out.dedup_by_key(|(key, _)| *key);
    out
}
