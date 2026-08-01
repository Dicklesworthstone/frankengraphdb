//! `fgdb-reference` — the executable semantics oracle (plan §15).
//!
//! A deliberately simple, single-threaded, obviously-correct implementation of
//! the logical semantics over canonical maps. It is compiled for tests,
//! fuzzing and model-checking only; it is never shipped and never optimized.
//! Its whole reason to exist is that **"what should this return" becomes a
//! program rather than a debate**, and the plan is explicit that it arrives
//! *before* the first optimized line rather than after.
//!
//! WHAT IT DOES TODAY: materializes a committed delta stream into graph state.
//! That makes it the first place in this codebase where a commit produces a
//! *graph* instead of bytes — Chronicle can already make a mutation durable and
//! recover it, and this turns the recovered rows back into vertices, edges,
//! labels and properties.
//!
//! **BEFORE-IMAGES ARE CHECKED, NOT TRUSTED.** Every `DeltaRow` that mutates
//! existing state carries an explicit before image (Appendix B: "explicit
//! before/after semantics ... full cascade before-images"). Applying one to a
//! state whose actual before differs is a *refusal*, never an overwrite. That
//! single decision is what makes the delta stream self-verifying: an apply
//! order bug, a dropped row, a duplicated row, or a template built from the
//! wrong basis all surface here as a typed disagreement naming the row, rather
//! than as state that is quietly wrong and agrees with nothing.
//!
//! An oracle that repaired what it was given could not detect any of that — it
//! would make every stream look applicable, which is the same as checking
//! nothing. So the rule is: if the row and the state disagree, the ROW is
//! reported and the state does not move.
//!
//! Canonical maps throughout (`BTreeMap`/`BTreeSet`), so iteration order is a
//! function of the keys alone and two runs cannot differ (doctrine 4).

#![forbid(unsafe_code)]

pub mod intents;
pub mod ssi;
pub mod txn;

use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, ElementId, EscrowDomainId, LabelId, LogicalDeltaTemplate,
    OperationKey, PropertyKeyId, RelationId, SchemaEpoch, ValidTimePeriod,
};
use fgdb_types::{
    BranchId, CanonicalScalar, CommitSeq, CommitSeqExhausted as CommitSeqExhaustion, DatabaseId,
    EId, GraphId, LogicalCommandSeq, ObjectId, VId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Domain separation for the stream-prefix digest. A bare hash of the bytes would
/// collide with any other transcript that happens to hash the same material.
const PREFIX_DIGEST_DOMAIN: &[u8] = b"fgdb.reference.stream-prefix.v2";
const LINEAGE_DIGEST_DOMAIN: &[u8] = b"fgdb.reference.snapshot-lineage.v1";
const ELEMENT_VERSION_DOMAIN: &[u8] = b"fgdb.reference.element-version.v1";

/// A materialized vertex.
#[derive(Clone, Debug, PartialEq)]
pub struct Vertex {
    /// Identity of this exact system-time version.
    ///
    /// The reference model derives it from the complete canonical effect chain,
    /// not from the stable vertex ID or an ambient commit sequence. Replaying
    /// the same effects therefore reproduces it, while any intervening mutation
    /// creates a distinct successor even when the visible payload later returns
    /// to an earlier value.
    pub version: ObjectId,
    pub birth_ordinal: u64,
    pub labels: BTreeSet<LabelId>,
    pub props: BTreeMap<PropertyKeyId, CanonicalScalar>,
    pub valid_time: Option<ValidTimePeriod>,
}

/// A materialized edge.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    /// Identity of this exact system-time version. See [`Vertex::version`].
    pub version: ObjectId,
    pub birth_ordinal: u64,
    pub src: VId,
    pub relation: RelationId,
    pub dst: VId,
    pub canonical_key: Option<CanonicalScalar>,
    pub props: BTreeMap<PropertyKeyId, CanonicalScalar>,
    pub valid_time: Option<ValidTimePeriod>,
}

/// One hop of a path: the edge traversed and the vertex it arrives at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathStep {
    pub edge: EId,
    pub to: VId,
}

/// A path: where it started and every hop since.
///
/// Stores the START plus hops rather than a vertex list, because a vertex list
/// cannot distinguish two parallel edges between the same pair — and telling
/// those apart is exactly what separates `Trail` from `Simple`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub start: VId,
    pub steps: Vec<PathStep>,
}

impl Path {
    /// The vertex this path currently ends at.
    pub fn end(&self) -> VId {
        self.steps.last().map_or(self.start, |step| step.to)
    }

    /// Every vertex visited, in order, including repeats.
    pub fn vertices(&self) -> Vec<VId> {
        let mut out = vec![self.start];
        out.extend(self.steps.iter().map(|step| step.to));
        out
    }

    /// Every edge traversed, in order.
    pub fn edge_ids(&self) -> Vec<EId> {
        self.steps.iter().map(|step| step.edge).collect()
    }

    pub fn hop_count(&self) -> usize {
        self.steps.len()
    }
}

/// GQL path modes (ISO/IEC 39075; plan:657 names the four).
///
/// They form a CONTAINMENT CHAIN — `Acyclic` ⊆ `Simple` ⊆ `Trail` ⊆ `Walk` —
/// which is why they are one closed union rather than four unrelated flags. The
/// nesting is a law worth testing: an implementation that confuses two adjacent
/// modes breaks it, and one that collapses two modes passes every test that only
/// checks each mode alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathMode {
    /// No restriction: vertices and edges may repeat.
    Walk,
    /// No repeated EDGES. A vertex may recur, reached by a different edge.
    Trail,
    /// No repeated VERTICES except that the first may equal the last, so a
    /// closed walk is admissible. This is the ONLY difference from `Acyclic`.
    Simple,
    /// No repeated vertices at all — not even a closed walk.
    Acyclic,
}

impl PathMode {
    /// May `current` be extended by `edge` arriving at `next`?
    ///
    /// `target` is needed because `Simple` admits returning to the start only
    /// when that closes the path: a mid-path return to the start is a repeated
    /// vertex like any other.
    fn admits(self, current: &Path, edge: EId, next: VId, target: VId) -> bool {
        match self {
            Self::Walk => true,
            Self::Trail => !current.steps.iter().any(|step| step.edge == edge),
            Self::Simple => {
                if current.steps.iter().any(|step| step.to == current.start) {
                    // Already closed. NOTHING may extend a closed walk: a
                    // further return to the start repeats it past the one
                    // closure `Simple` buys, and a hop anywhere else leaves
                    // a walk whose start recurs mid-path. (fgdb-alyw: the
                    // pre-fix check admitted both, so [10,10] could
                    // re-bounce 10->1->10->1... and [1,2,1] could escape
                    // to fresh vertices.)
                    return false;
                }
                if next == current.start {
                    // Closing the walk is allowed only if this is the answer.
                    return next == target;
                }
                !current.vertices().contains(&next)
            }
            Self::Acyclic => !current.vertices().contains(&next),
        }
    }
}

/// Why a row could not be applied.
///
/// Every arm names the row's subject and the exact disagreement, because the
/// point of the oracle is to say *what* is wrong. "Apply failed" would tell a
/// caller only that one of several dozen laws broke.
#[derive(Clone, Debug, PartialEq)]
pub enum ApplyError {
    /// A create names an identity that already exists. Identities are never
    /// recycled (§6.2), so this is always a defect rather than an update.
    VertexAlreadyExists {
        vid: VId,
    },
    EdgeAlreadyExists {
        eid: EId,
    },
    /// A create names an identity that existed earlier but is no longer live.
    ///
    /// Distinct from `*AlreadyExists`: the visible element is gone, but its
    /// allocation slot remains permanently spent (plan §4.5). Keeping this a
    /// typed refusal prevents a caller from mistaking retirement for permission
    /// to mint a different element under the same stable identity.
    VertexIdentitySpent {
        vid: VId,
    },
    EdgeIdentitySpent {
        eid: EId,
    },
    /// A row names an element that is not there.
    NoSuchVertex {
        vid: VId,
    },
    NoSuchEdge {
        eid: EId,
    },
    /// A delete effect was finalized against a different element version.
    ///
    /// The stable VId/EId is deliberately insufficient: it names every version
    /// of the element. Deletion is a compare-and-set against the exact current
    /// version and must leave the graph untouched on disagreement.
    ElementVersionMismatch {
        elem: ElementId,
        declared: ObjectId,
        actual: ObjectId,
    },
    /// A row could not supply canonical bytes for the version-identity chain.
    ///
    /// This is checked before mutation so malformed input cannot move state and
    /// then fail while deriving the successor version.
    VersionIdentityEncoding(fgdb_delta_types::CanonicalError),
    /// An edge names an endpoint that does not exist. Referential integrity is
    /// checked here because a graph with a dangling endpoint is not a graph.
    DanglingEndpoint {
        eid: EId,
        endpoint: VId,
    },
    /// A before image disagrees with the materialized state.
    LabelBeforeMismatch {
        vid: VId,
        label: LabelId,
        declared: bool,
        actual: bool,
    },
    /// Boxed because a `CanonicalScalar` carries owned text, and an error enum
    /// as wide as its largest payload makes every `Result` in the crate that
    /// wide too.
    PropertyBeforeMismatch {
        elem: ElementId,
        property: PropertyKeyId,
        declared: Option<Box<CanonicalScalar>>,
        actual: Option<Box<CanonicalScalar>>,
    },
    ValidTimeBeforeMismatch {
        elem: ElementId,
        declared: Option<ValidTimePeriod>,
        actual: Option<ValidTimePeriod>,
    },
    /// A valid-time row whose after-period ends before it starts. The
    /// before-image law checks agreement with the past; this one checks the
    /// period itself — an inverted interval is not a weaker claim, it is not
    /// a period at all (fgdb-nrub).
    InvertedValidTimePeriod {
        elem: ElementId,
        declared: ValidTimePeriod,
    },
    CounterBeforeMismatch {
        elem: ElementId,
        property: PropertyKeyId,
        declared: i128,
        actual: i128,
    },
    EscrowBeforeMismatch {
        domain: EscrowDomainId,
        declared: i128,
        actual: i128,
    },
    SketchBeforeMismatch {
        profile: ObjectId,
        declared: [u8; 32],
        actual: [u8; 32],
    },
    SchemaEpochMismatch {
        declared: SchemaEpoch,
        actual: SchemaEpoch,
    },
    /// A coordinate entry was validated under a schema epoch other than the
    /// one present before this template began.
    SchemaBindingMismatch {
        graph: GraphId,
        branch: BranchId,
        relation: RelationId,
        declared: SchemaEpoch,
        actual: SchemaEpoch,
    },
    /// The commit-time constraint-root binding drifted after the snapshot
    /// was taken: effects evaluated under the old root may not be restamped
    /// onto the new one (fgdb-hdgw).
    ConstraintBindingMismatch {
        declared_constraint_root: ObjectId,
        actual_constraint_root: ObjectId,
    },
    /// The entry-level schema-transition reference does not exactly describe
    /// the schema row carried by that entry.
    SchemaTransitionMismatch {
        graph: GraphId,
        branch: BranchId,
        relation: RelationId,
        declared: Option<ObjectId>,
        schema_rows: Vec<ObjectId>,
    },
    ConstraintRootMismatch {
        declared_schema_root: ObjectId,
        actual_schema_root: ObjectId,
    },
    /// A counter or escrow row's arithmetic does not close: `after` must equal
    /// `before + delta` exactly. Checked rather than assumed, because a row
    /// that disagrees with itself would otherwise install a value no arithmetic
    /// produced.
    ArithmeticDoesNotClose {
        before: i128,
        delta: i128,
        declared_after: i128,
    },
    /// A vertex deletion's cascade before-image is not exactly the set of
    /// incident edges. Both directions are errors: a missing edge would leave a
    /// dangling edge behind, and an extra one claims a retirement that did not
    /// happen.
    CascadeImageMismatch {
        vid: VId,
        declared: Vec<EId>,
        actual: Vec<EId>,
    },
    /// An operation key already used by a DIFFERENT row. Idempotence means a
    /// repeated row is a no-op; it does not mean a key may name two effects.
    OperationKeyReused {
        key: OperationKey,
    },
    /// A template was offered at a sequence the coordinate has already passed.
    /// History is append-only: re-applying a sequence would either duplicate
    /// effects or silently rewrite what that sequence meant, and the commit
    /// stream this materializes is gap-free and monotone by construction.
    SequenceNotAdvancing {
        graph: GraphId,
        branch: BranchId,
        applied_through: CommitSeq,
        offered: CommitSeq,
    },
    /// The template was offered at a sequence that is not the stream's next one.
    ///
    /// PER-COORDINATE MONOTONICITY IS NOT ENOUGH, and the comment above used to
    /// claim the stream was "gap-free and monotone by construction" while nothing
    /// checked it (fgdb-reference-global-commit-frontier-pjqu). Comparing only
    /// against each touched coordinate lets a fresh coordinate accept ANY
    /// sequence, because it has no `applied_through` to be compared with: seq 2 to
    /// A then seq 1 to an untouched B was admitted, as was a gap, as was zero.
    ///
    /// EXACT-NEXT rather than merely increasing, because that is what the durable
    /// layer does: Chronicle's `MarkerChain` starts at 1 and demands the exact
    /// successor, and `LocalDeltaBatchIndex` keeps one global frontier that rejects
    /// gaps and duplicates alike. A gapped history cannot come from the commit
    /// stream, so an oracle that admits one can no longer reject — which is the
    /// specific way an oracle stops being worth having.
    SequenceNotNext {
        expected: CommitSeq,
        offered: CommitSeq,
    },
    /// The persisted global frontier is the largest representable sequence.
    /// No further semantic commit can be assigned without wrapping to the
    /// reserved origin, so every later apply is permanently refused.
    CommitSeqExhausted(CommitSeqExhaustion),
    /// The persisted semantic-command frontier is the largest representable
    /// position. No later transaction or control command can advance it, so the
    /// condition is permanent rather than a comparative ordering violation.
    LogicalCommandSeqExhausted {
        frontier: LogicalCommandSeq,
    },
    /// The transaction commit did not advance the independent semantic-command
    /// position.
    ///
    /// This is deliberately NOT an exact-next law. Control commands occupy
    /// logical-command positions without consuming a [`CommitSeq`], so two
    /// successive transaction commits may have a gap in this domain. Chronicle's
    /// `MarkerChain::validate` enforces the same strictly-increasing law.
    LogicalCommandSequenceNotAdvancing {
        previous: LogicalCommandSeq,
        offered: LogicalCommandSeq,
    },
    /// The template's canonical bytes could not be produced, so the stream-prefix
    /// digest cannot be computed.
    ///
    /// The current public constructors validate before returning a
    /// `LogicalDeltaTemplate`, so this is a defensive boundary rather than a
    /// claimed reachable path. `canonical_bytes` is nevertheless fallible, and
    /// applying durable input must preserve its typed cause instead of turning a
    /// future constructor or format change into a panic.
    TemplateNotCanonical(fgdb_delta_types::CanonicalError),
    /// The stream frontier had no recorded prefix digest.
    ///
    /// Private state construction makes this an internal contradiction today, but
    /// folding an all-`0xff` sentinel into the next digest would turn corruption
    /// into a plausible new history. The oracle fails closed and names the missing
    /// sequence instead.
    PrefixDigestMissing {
        seq: CommitSeq,
    },
    /// A template with no coordinate entries.
    ///
    /// Refused rather than treated as a successful no-op. An empty template
    /// applies "successfully" while recording nothing, so it consumes no sequence
    /// and leaves no trace — a commit that happened for no reason, which is
    /// exactly what the write-path laws in `fgdb-sim` forbid one layer up ("not an
    /// empty capsule, not a marker with no effects — nothing at all"). Accepting
    /// it here would let the oracle bless a stream the engine must never produce.
    EmptyTemplate,
}

impl core::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VertexAlreadyExists { vid } => write!(f, "vertex {vid:?} already exists"),
            Self::EdgeAlreadyExists { eid } => write!(f, "edge {eid:?} already exists"),
            Self::VertexIdentitySpent { vid } => {
                write!(f, "vertex identity {vid:?} is permanently spent")
            }
            Self::EdgeIdentitySpent { eid } => {
                write!(f, "edge identity {eid:?} is permanently spent")
            }
            Self::NoSuchVertex { vid } => write!(f, "no such vertex {vid:?}"),
            Self::NoSuchEdge { eid } => write!(f, "no such edge {eid:?}"),
            Self::ElementVersionMismatch { elem, .. } => {
                write!(f, "delete before-version disagrees with current {elem:?}")
            }
            Self::VersionIdentityEncoding(cause) => {
                write!(f, "element version identity could not be encoded: {cause}")
            }
            Self::DanglingEndpoint { eid, endpoint } => {
                write!(f, "edge {eid:?} names missing endpoint {endpoint:?}")
            }
            Self::LabelBeforeMismatch {
                vid,
                label,
                declared,
                actual,
            } => write!(
                f,
                "label {label:?} on {vid:?}: row declares before={declared}, state has {actual}"
            ),
            Self::PropertyBeforeMismatch { elem, property, .. } => write!(
                f,
                "property {property:?} on {elem:?}: before image disagrees with state"
            ),
            Self::ValidTimeBeforeMismatch { elem, .. } => {
                write!(
                    f,
                    "valid time on {elem:?}: before image disagrees with state"
                )
            }
            Self::InvertedValidTimePeriod { elem, declared } => {
                write!(
                    f,
                    "valid time on {elem:?}: after period {declared:?} ends before it starts"
                )
            }
            Self::CounterBeforeMismatch {
                elem,
                property,
                declared,
                actual,
            } => write!(
                f,
                "counter {property:?} on {elem:?}: row declares before={declared}, state has {actual}"
            ),
            Self::EscrowBeforeMismatch {
                domain,
                declared,
                actual,
            } => write!(
                f,
                "escrow domain {domain:?}: row declares before={declared}, state has {actual}"
            ),
            Self::SketchBeforeMismatch { profile, .. } => {
                write!(f, "sketch {profile:?}: before digest disagrees with state")
            }
            Self::SchemaEpochMismatch { declared, actual } => write!(
                f,
                "schema epoch: row declares before={declared:?}, state has {actual:?}"
            ),
            Self::SchemaBindingMismatch {
                graph,
                branch,
                relation,
                declared,
                actual,
            } => write!(
                f,
                "schema binding for ({graph:?}, {branch:?}, {relation:?}) declares \
                 {declared:?}, pre-template state has {actual:?}"
            ),
            Self::ConstraintBindingMismatch {
                declared_constraint_root,
                actual_constraint_root,
            } => write!(
                f,
                "constraint-root binding declares {declared_constraint_root:?}, \
                 pre-commit state has {actual_constraint_root:?}"
            ),
            Self::SchemaTransitionMismatch {
                graph,
                branch,
                relation,
                ..
            } => write!(
                f,
                "schema transition for ({graph:?}, {branch:?}, {relation:?}) \
                 disagrees with its schema rows"
            ),
            Self::ConstraintRootMismatch { .. } => {
                write!(f, "constraint transition: before root disagrees with state")
            }
            Self::ArithmeticDoesNotClose {
                before,
                delta,
                declared_after,
            } => write!(
                f,
                "row does not close: {before} + {delta} != {declared_after}"
            ),
            Self::CascadeImageMismatch { vid, .. } => write!(
                f,
                "deletion of {vid:?} declares a cascade image that is not its incident edge set"
            ),
            Self::SequenceNotAdvancing {
                graph,
                branch,
                applied_through,
                offered,
            } => write!(
                f,
                "({graph:?}, {branch:?}) has applied through {applied_through:?}; {offered:?} does not advance"
            ),
            Self::SequenceNotNext { expected, offered } => write!(
                f,
                "the stream's next commit is {expected:?}; {offered:?} is not it"
            ),
            Self::CommitSeqExhausted(cause) => write!(f, "{cause}"),
            Self::LogicalCommandSeqExhausted { frontier } => write!(
                f,
                "logical command sequence space is exhausted at {frontier:?}"
            ),
            Self::LogicalCommandSequenceNotAdvancing { previous, offered } => write!(
                f,
                "logical command position {offered:?} does not advance {previous:?}"
            ),
            Self::TemplateNotCanonical(cause) => write!(
                f,
                "the template's canonical bytes could not be produced, so its \
                 stream-prefix digest is unknowable: {cause}"
            ),
            Self::PrefixDigestMissing { seq } => {
                write!(f, "the stream-prefix digest at {seq:?} is missing")
            }
            Self::EmptyTemplate => {
                write!(f, "a template with no coordinate entries is not a commit")
            }
            Self::OperationKeyReused { key } => {
                write!(f, "operation key {key:?} already names a different effect")
            }
        }
    }
}

impl core::error::Error for ApplyError {}

impl From<CommitSeqExhaustion> for ApplyError {
    fn from(cause: CommitSeqExhaustion) -> Self {
        Self::CommitSeqExhausted(cause)
    }
}

/// The materialized state of one coordinate (a graph/branch pair).
///
/// Canonical maps throughout, so iteration order is a function of the keys and
/// two runs over the same rows cannot differ.
#[derive(Clone, Debug, PartialEq)]
pub struct ReferenceGraph {
    vertices: BTreeMap<VId, Vertex>,
    edges: BTreeMap<EId, Edge>,
    /// Every vertex identity ever admitted on this materialized lineage.
    ///
    /// Live identities are present here and in `vertices`; retirement removes
    /// only the visible row. The set is deliberately part of cloned/equal state:
    /// an empty genesis graph and a graph made visibly empty by deletion admit
    /// different future histories, so they are not the same state-machine state.
    spent_vertex_ids: BTreeSet<VId>,
    /// Edge-kind counterpart of `spent_vertex_ids`.
    spent_edge_ids: BTreeSet<EId>,
    /// Counter values, which are their own state rather than ordinary
    /// properties: their rows carry a checked delta and a merge algebra.
    counters: BTreeMap<(ElementId, PropertyKeyId), i128>,
    /// Escrow ledger balances, keyed by domain. Not graph state — a coordinate
    /// effect may not mutate it and a global effect may not mutate the graph
    /// (Appendix B), so they are deliberately separate maps.
    escrow: BTreeMap<EscrowDomainId, i128>,
    /// Sketch state digests by profile.
    sketches: BTreeMap<ObjectId, [u8; 32]>,
    /// Rows already folded in, by operation key, so counter/escrow/sketch
    /// families are idempotent under replay — the plan's "set-union of unique
    /// operation keys followed by deterministic checked summation".
    ///
    /// The ROW is kept, not merely the key. Remembering only the key would make
    /// a *different* row bearing a reused key a silent no-op, which is strictly
    /// worse than double-counting: the effect vanishes and nothing reports it.
    /// Keeping the row makes a reuse detectable, so idempotence covers replay
    /// without also swallowing a collision.
    operation_keys: BTreeMap<OperationKey, DeltaRow>,
    schema_epoch: SchemaEpoch,
    schema_root: ObjectId,
    constraint_root: ObjectId,
}

impl Default for ReferenceGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceGraph {
    /// The empty coordinate. Genesis roots are all-zero rather than derived,
    /// so the first Schema/Constraint row's before image has something exact to
    /// name — an "unset" sentinel would make the first transition unverifiable.
    pub fn new() -> Self {
        Self {
            vertices: BTreeMap::new(),
            edges: BTreeMap::new(),
            spent_vertex_ids: BTreeSet::new(),
            spent_edge_ids: BTreeSet::new(),
            counters: BTreeMap::new(),
            escrow: BTreeMap::new(),
            sketches: BTreeMap::new(),
            operation_keys: BTreeMap::new(),
            schema_epoch: SchemaEpoch(0),
            schema_root: ObjectId([0u8; 32]),
            constraint_root: ObjectId([0u8; 32]),
        }
    }

    // ---- queries ---------------------------------------------------------

    pub fn vertex(&self, vid: VId) -> Option<&Vertex> {
        self.vertices.get(&vid)
    }

    pub fn edge(&self, eid: EId) -> Option<&Edge> {
        self.edges.get(&eid)
    }

    /// Exact current system-time version identity of an element.
    pub fn element_version(&self, elem: ElementId) -> Option<ObjectId> {
        match elem {
            ElementId::Vertex(vid) => self.vertices.get(&vid).map(|vertex| vertex.version),
            ElementId::Edge(eid) => self.edges.get(&eid).map(|edge| edge.version),
        }
    }

    pub fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn schema_epoch(&self) -> SchemaEpoch {
        self.schema_epoch
    }

    pub fn schema_root(&self) -> ObjectId {
        self.schema_root
    }

    pub fn constraint_root(&self) -> ObjectId {
        self.constraint_root
    }

    pub fn counter(&self, elem: ElementId, property: PropertyKeyId) -> Option<i128> {
        self.counters.get(&(elem, property)).copied()
    }

    pub fn escrow_balance(&self, domain: EscrowDomainId) -> i128 {
        self.escrow.get(&domain).copied().unwrap_or(0)
    }

    /// Outgoing edges of a vertex, in canonical edge-id order.
    pub fn out_edges(&self, vid: VId) -> Vec<EId> {
        self.edges
            .iter()
            .filter(|(_, edge)| edge.src == vid)
            .map(|(eid, _)| *eid)
            .collect()
    }

    /// Incoming edges of a vertex, in canonical edge-id order.
    pub fn in_edges(&self, vid: VId) -> Vec<EId> {
        self.edges
            .iter()
            .filter(|(_, edge)| edge.dst == vid)
            .map(|(eid, _)| *eid)
            .collect()
    }

    /// Every edge touching a vertex, in canonical order. This is the set a
    /// vertex deletion's cascade before-image must equal exactly.
    pub fn incident_edges(&self, vid: VId) -> Vec<EId> {
        self.edges
            .iter()
            .filter(|(_, edge)| edge.src == vid || edge.dst == vid)
            .map(|(eid, _)| *eid)
            .collect()
    }

    // ---- path modes (GQL / ISO 39075, plan:657) ---------------------------

    /// Every path from `from` to `to` over `relation`, up to `max_hops`,
    /// admissible under `mode`, in canonical order.
    ///
    /// Deliberately an exhaustive walk: §15 defines this crate as obviously
    /// correct rather than fast, and "enumerate then filter" is the definition
    /// of each mode rather than an encoding of it. A real planner must produce
    /// the same SET.
    ///
    /// `max_hops` is required, not optional. Under `Walk` a cycle admits
    /// infinitely many paths, so an unbounded call would not terminate — and a
    /// silent internal default would make the result depend on a number the
    /// caller never chose.
    pub fn paths(
        &self,
        from: VId,
        to: VId,
        relation: RelationId,
        mode: PathMode,
        max_hops: usize,
    ) -> Vec<Path> {
        let mut found = Vec::new();
        if !self.vertices.contains_key(&from) || !self.vertices.contains_key(&to) {
            return found;
        }
        let mut current = Path {
            start: from,
            steps: Vec::new(),
        };
        self.extend_path(&mut current, to, relation, mode, max_hops, &mut found);
        // Canonical order: shorter paths first, then lexicographically by the
        // edges traversed. Two runs must agree, and "the order the search
        // happened to find them" is not a specification.
        found.sort_by(|a, b| {
            a.steps
                .len()
                .cmp(&b.steps.len())
                .then_with(|| a.edge_ids().cmp(&b.edge_ids()))
        });
        found
    }

    fn extend_path(
        &self,
        current: &mut Path,
        target: VId,
        relation: RelationId,
        mode: PathMode,
        max_hops: usize,
        found: &mut Vec<Path>,
    ) {
        if current.end() == target && !current.steps.is_empty() {
            found.push(current.clone());
            // Do NOT return: a longer path may also reach the target, and under
            // Walk or Trail that longer path is a distinct answer.
        }
        if current.steps.len() >= max_hops {
            return;
        }
        let tip = current.end();
        for (eid, edge) in &self.edges {
            if edge.relation != relation || edge.src != tip {
                continue;
            }
            if !mode.admits(current, *eid, edge.dst, target) {
                continue;
            }
            current.steps.push(PathStep {
                edge: *eid,
                to: edge.dst,
            });
            self.extend_path(current, target, relation, mode, max_hops, found);
            current.steps.pop();
        }
    }

    // ---- temporal selectors (§15: the oracle implements temporal selectors) --

    /// Is this period live at `micros`?
    ///
    /// **Half-open, `[start, end)`.** Stated because the choice is observable
    /// and the alternative is worse: with a closed upper bound, two adjacent
    /// periods `[0,10]` and `[10,20]` would BOTH be live at 10, so a value
    /// replaced at an instant would have two simultaneous versions. Half-open
    /// makes replacement seamless — the old period ends exactly where the new
    /// one begins and no instant is covered twice.
    ///
    /// `None` means unbounded, not absent: a period with no end is live
    /// forever after its start.
    pub fn period_covers(period: ValidTimePeriod, micros: i64) -> bool {
        micros >= period.start_micros && period.end_micros.is_none_or(|end| micros < end)
    }

    /// Is `elem`'s own period live at `micros`?
    ///
    /// An element with NO period is live at every instant. That is the right
    /// reading of absence here: valid time is an optional assertion about when a
    /// fact holds, and a fact with no such assertion is not thereby time-limited
    /// to nothing.
    fn own_period_live_at(&self, elem: ElementId, micros: i64) -> bool {
        let period = match elem {
            ElementId::Vertex(vid) => self.vertices.get(&vid).map(|v| v.valid_time),
            ElementId::Edge(eid) => self.edges.get(&eid).map(|e| e.valid_time),
        };
        match period {
            None => false, // the element does not exist at all
            Some(None) => true,
            Some(Some(period)) => Self::period_covers(period, micros),
        }
    }

    /// Is this vertex visible at `micros`?
    pub fn vertex_live_at(&self, vid: VId, micros: i64) -> bool {
        self.own_period_live_at(ElementId::Vertex(vid), micros)
    }

    /// Is this edge visible at `micros`?
    ///
    /// **TEMPORAL REFERENTIAL INTEGRITY.** An edge is visible only when its own
    /// period is live AND both endpoints are live. Filtering edges by their own
    /// period alone is the obvious implementation and it is wrong: it produces a
    /// historical view containing an edge to a vertex that does not exist at
    /// that instant — precisely the dangling edge the non-temporal view refuses
    /// to accept at apply time. A time-travel query that can return a graph the
    /// database would never have accepted is not answering "what did this look
    /// like then".
    pub fn edge_live_at(&self, eid: EId, micros: i64) -> bool {
        let Some(edge) = self.edges.get(&eid) else {
            return false;
        };
        self.own_period_live_at(ElementId::Edge(eid), micros)
            && self.vertex_live_at(edge.src, micros)
            && self.vertex_live_at(edge.dst, micros)
    }

    /// Vertices visible at `micros`, in canonical order.
    pub fn vertices_as_of(&self, micros: i64) -> Vec<VId> {
        self.vertices
            .keys()
            .copied()
            .filter(|vid| self.vertex_live_at(*vid, micros))
            .collect()
    }

    /// Edges visible at `micros`, in canonical order.
    pub fn edges_as_of(&self, micros: i64) -> Vec<EId> {
        self.edges
            .keys()
            .copied()
            .filter(|eid| self.edge_live_at(*eid, micros))
            .collect()
    }

    /// Neighbours over one relation as of `micros`.
    ///
    /// A neighbour requires a live edge, which by the rule above already
    /// requires both endpoints live — so this cannot report a neighbour that
    /// does not exist at that instant.
    pub fn neighbours_as_of(&self, vid: VId, relation: RelationId, micros: i64) -> Vec<VId> {
        if !self.vertex_live_at(vid, micros) {
            return Vec::new();
        }
        let mut out: BTreeSet<VId> = BTreeSet::new();
        for (eid, edge) in &self.edges {
            if edge.relation == relation && edge.src == vid && self.edge_live_at(*eid, micros) {
                out.insert(edge.dst);
            }
        }
        out.into_iter().collect()
    }

    /// Neighbours reachable over one relation, deduplicated and canonically
    /// ordered — the smallest query that is recognisably a graph query.
    pub fn neighbours(&self, vid: VId, relation: RelationId) -> Vec<VId> {
        let mut out: BTreeSet<VId> = BTreeSet::new();
        for edge in self.edges.values() {
            if edge.relation == relation && edge.src == vid {
                out.insert(edge.dst);
            }
        }
        out.into_iter().collect()
    }

    // ---- apply -----------------------------------------------------------

    /// Extend one element's version chain with one canonical effect.
    ///
    /// The predecessor tag distinguishes creation from a successor whose prior
    /// digest happens to be all-zero. The row length makes the transcript
    /// self-delimiting, and the row's canonical bytes include its family and
    /// stable element identity. No branch population, wall clock, or commit
    /// sequence enters this derivation.
    fn successor_version(
        previous: Option<ObjectId>,
        row: &DeltaRow,
    ) -> Result<ObjectId, ApplyError> {
        let canonical = row
            .canonical_bytes()
            .map_err(ApplyError::VersionIdentityEncoding)?;
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(ELEMENT_VERSION_DOMAIN);
        match previous {
            None => {
                hasher.update(&[0]);
            }
            Some(version) => {
                hasher.update(&[1]);
                hasher.update(&version.0);
            }
        }
        hasher.update(&(canonical.len() as u64).to_le_bytes());
        hasher.update(&canonical);
        Ok(ObjectId(hasher.finalize().0))
    }

    /// Install a version already derived and validated for an existing element.
    fn set_element_version(
        &mut self,
        elem: ElementId,
        version: ObjectId,
    ) -> Result<(), ApplyError> {
        match elem {
            ElementId::Vertex(vid) => {
                self.vertices
                    .get_mut(&vid)
                    .ok_or(ApplyError::NoSuchVertex { vid })?
                    .version = version;
            }
            ElementId::Edge(eid) => {
                self.edges
                    .get_mut(&eid)
                    .ok_or(ApplyError::NoSuchEdge { eid })?
                    .version = version;
            }
        }
        Ok(())
    }

    /// Apply one row, or refuse and leave the state untouched.
    ///
    /// Every mutation is computed and every check passed before anything is
    /// written, so refusal of THIS ROW is total: a caller that retries after
    /// fixing the row sees exactly the state it had before that row. This does
    /// not make a sequence of separate [`Self::apply_row`] calls atomic.
    pub fn apply_row(&mut self, row: &DeltaRow) -> Result<(), ApplyError> {
        match row {
            DeltaRow::CreateVertex {
                vid,
                birth_ordinal,
                labels,
                props,
                valid_time,
            } => {
                if self.vertices.contains_key(vid) {
                    return Err(ApplyError::VertexAlreadyExists { vid: *vid });
                }
                if self.spent_vertex_ids.contains(vid) {
                    return Err(ApplyError::VertexIdentitySpent { vid: *vid });
                }
                let version = Self::successor_version(None, row)?;
                self.vertices.insert(
                    *vid,
                    Vertex {
                        version,
                        birth_ordinal: *birth_ordinal,
                        labels: labels.iter().copied().collect(),
                        props: props.iter().cloned().collect(),
                        valid_time: *valid_time,
                    },
                );
                let was_fresh = self.spent_vertex_ids.insert(*vid);
                debug_assert!(was_fresh, "spent-set admission was checked above");
            }
            DeltaRow::CreateEdge {
                eid,
                birth_ordinal,
                src,
                relation,
                dst,
                canonical_key,
                props,
                valid_time,
            } => {
                if self.edges.contains_key(eid) {
                    return Err(ApplyError::EdgeAlreadyExists { eid: *eid });
                }
                if self.spent_edge_ids.contains(eid) {
                    return Err(ApplyError::EdgeIdentitySpent { eid: *eid });
                }
                // Referential integrity BEFORE insertion: a graph holding an
                // edge to a vertex that does not exist is not a graph, and
                // discovering it later gives no way to say which row was wrong.
                for endpoint in [*src, *dst] {
                    if !self.vertices.contains_key(&endpoint) {
                        return Err(ApplyError::DanglingEndpoint {
                            eid: *eid,
                            endpoint,
                        });
                    }
                }
                let version = Self::successor_version(None, row)?;
                self.edges.insert(
                    *eid,
                    Edge {
                        version,
                        birth_ordinal: *birth_ordinal,
                        src: *src,
                        relation: *relation,
                        dst: *dst,
                        canonical_key: canonical_key.clone(),
                        props: props.iter().cloned().collect(),
                        valid_time: *valid_time,
                    },
                );
                let was_fresh = self.spent_edge_ids.insert(*eid);
                debug_assert!(was_fresh, "spent-set admission was checked above");
            }
            DeltaRow::DeleteVertex {
                vid,
                before_version,
                sorted_retired_incident_edges,
            } => {
                let actual_version = self
                    .vertices
                    .get(vid)
                    .ok_or(ApplyError::NoSuchVertex { vid: *vid })?
                    .version;
                if actual_version != *before_version {
                    return Err(ApplyError::ElementVersionMismatch {
                        elem: ElementId::Vertex(*vid),
                        declared: *before_version,
                        actual: actual_version,
                    });
                }
                // THE CASCADE LAW. The declared image must be EXACTLY the
                // incident set. Too few would leave a dangling edge; too many
                // claims a retirement that never happened. Checking equality
                // rather than containment is what makes the before-image
                // load-bearing instead of decorative.
                let actual = self.incident_edges(*vid);
                let declared: Vec<EId> = sorted_retired_incident_edges.clone();
                if declared != actual {
                    return Err(ApplyError::CascadeImageMismatch {
                        vid: *vid,
                        declared,
                        actual,
                    });
                }
                for eid in &actual {
                    self.edges.remove(eid);
                }
                self.vertices.remove(vid);
                // Counter rows are element-keyed (the conflict layer emits
                // ConflictKey::Element for them), so a deleted element's
                // counters are dead state, not element-orthogonal history:
                // identities never recycle (§6.2), which makes the residue
                // permanently unreadable-but-present. Reap it with the
                // element (fgdb-nrub).
                self.counters.retain(|(elem, _), _| match elem {
                    ElementId::Vertex(v) => v != vid,
                    ElementId::Edge(e) => !actual.contains(e),
                });
            }
            DeltaRow::DeleteEdge {
                eid,
                before_version,
            } => {
                let actual_version = self
                    .edges
                    .get(eid)
                    .ok_or(ApplyError::NoSuchEdge { eid: *eid })?
                    .version;
                if actual_version != *before_version {
                    return Err(ApplyError::ElementVersionMismatch {
                        elem: ElementId::Edge(*eid),
                        declared: *before_version,
                        actual: actual_version,
                    });
                }
                self.edges.remove(eid);
                // Same law as DeleteVertex: element-keyed counters die with
                // the element (fgdb-nrub).
                self.counters
                    .retain(|(elem, _), _| *elem != ElementId::Edge(*eid));
            }
            DeltaRow::LabelMembership {
                vid,
                label,
                before,
                after,
            } => {
                let previous = self
                    .vertices
                    .get(vid)
                    .ok_or(ApplyError::NoSuchVertex { vid: *vid })?
                    .version;
                let version = Self::successor_version(Some(previous), row)?;
                let vertex = self
                    .vertices
                    .get_mut(vid)
                    .ok_or(ApplyError::NoSuchVertex { vid: *vid })?;
                let actual = vertex.labels.contains(label);
                if actual != *before {
                    return Err(ApplyError::LabelBeforeMismatch {
                        vid: *vid,
                        label: *label,
                        declared: *before,
                        actual,
                    });
                }
                if *after {
                    vertex.labels.insert(*label);
                } else {
                    vertex.labels.remove(label);
                }
                vertex.version = version;
            }
            DeltaRow::Property {
                elem,
                property,
                before,
                after,
            } => {
                let previous = self.element_version(*elem).ok_or(match elem {
                    ElementId::Vertex(vid) => ApplyError::NoSuchVertex { vid: *vid },
                    ElementId::Edge(eid) => ApplyError::NoSuchEdge { eid: *eid },
                })?;
                let version = Self::successor_version(Some(previous), row)?;
                match elem {
                    ElementId::Vertex(vid) => {
                        let vertex = self
                            .vertices
                            .get_mut(vid)
                            .ok_or(ApplyError::NoSuchVertex { vid: *vid })?;
                        let actual = vertex.props.get(property).cloned();
                        if actual != *before {
                            return Err(ApplyError::PropertyBeforeMismatch {
                                elem: *elem,
                                property: *property,
                                declared: before.clone().map(Box::new),
                                actual: actual.map(Box::new),
                            });
                        }
                        match after {
                            Some(value) => {
                                vertex.props.insert(*property, value.clone());
                            }
                            None => {
                                vertex.props.remove(property);
                            }
                        }
                        vertex.version = version;
                    }
                    ElementId::Edge(eid) => {
                        let edge = self
                            .edges
                            .get_mut(eid)
                            .ok_or(ApplyError::NoSuchEdge { eid: *eid })?;
                        let actual = edge.props.get(property).cloned();
                        if actual != *before {
                            return Err(ApplyError::PropertyBeforeMismatch {
                                elem: *elem,
                                property: *property,
                                declared: before.clone().map(Box::new),
                                actual: actual.map(Box::new),
                            });
                        }
                        match after {
                            Some(value) => {
                                edge.props.insert(*property, value.clone());
                            }
                            None => {
                                edge.props.remove(property);
                            }
                        }
                        edge.version = version;
                    }
                }
            }
            DeltaRow::ValidTime {
                elem,
                before,
                after,
                ..
            } => {
                // The after-period must BE a period before anything agrees
                // to transition into it: start past end is inverted, and an
                // inverted interval is dead state from the moment it lands
                // (fgdb-nrub). Open periods (end = None) are always valid.
                if let Some(period) = after
                    && let Some(end) = period.end_micros
                    && period.start_micros > end
                {
                    return Err(ApplyError::InvertedValidTimePeriod {
                        elem: *elem,
                        declared: *period,
                    });
                }
                match elem {
                    ElementId::Vertex(vid) => {
                        let vertex = self
                            .vertices
                            .get_mut(vid)
                            .ok_or(ApplyError::NoSuchVertex { vid: *vid })?;
                        let version = Self::successor_version(Some(vertex.version), row)?;
                        let actual = vertex.valid_time;
                        if actual != *before {
                            return Err(ApplyError::ValidTimeBeforeMismatch {
                                elem: *elem,
                                declared: *before,
                                actual,
                            });
                        }
                        vertex.valid_time = *after;
                        vertex.version = version;
                    }
                    ElementId::Edge(eid) => {
                        let edge = self
                            .edges
                            .get_mut(eid)
                            .ok_or(ApplyError::NoSuchEdge { eid: *eid })?;
                        let version = Self::successor_version(Some(edge.version), row)?;
                        let actual = edge.valid_time;
                        if actual != *before {
                            return Err(ApplyError::ValidTimeBeforeMismatch {
                                elem: *elem,
                                declared: *before,
                                actual,
                            });
                        }
                        edge.valid_time = *after;
                        edge.version = version;
                    }
                }
            }
            DeltaRow::Counter {
                operation_key,
                elem,
                property,
                delta,
                before,
                after,
                ..
            } => {
                // An already-seen operation key is a replay of a row already
                // folded in. Idempotence is the plan's rule for these families
                // ("set-union of unique operation keys"), and it is why raw
                // addition alone would be wrong.
                if self.already_applied(operation_key, row)? {
                    return Ok(());
                }
                self.require_element(*elem)?;
                let previous = self.element_version(*elem).ok_or(match elem {
                    ElementId::Vertex(vid) => ApplyError::NoSuchVertex { vid: *vid },
                    ElementId::Edge(eid) => ApplyError::NoSuchEdge { eid: *eid },
                })?;
                let version = Self::successor_version(Some(previous), row)?;
                Self::require_closing(*before, *delta, *after)?;
                let actual = self.counter(*elem, *property).unwrap_or(0);
                if actual != *before {
                    return Err(ApplyError::CounterBeforeMismatch {
                        elem: *elem,
                        property: *property,
                        declared: *before,
                        actual,
                    });
                }
                self.set_element_version(*elem, version)?;
                self.counters.insert((*elem, *property), *after);
                self.operation_keys.insert(*operation_key, row.clone());
            }
            DeltaRow::Escrow {
                domain_id,
                operation_key,
                subject,
                delta,
                before_value,
                after_value,
                ..
            } => {
                if self.already_applied(operation_key, row)? {
                    return Ok(());
                }
                // The conflict keys already claim this row "is checked against
                // the subject, so a concurrent write to either [domain or
                // subject] invalidates it" — and the anomaly test asserts that
                // claim. Make it TRUE in the materializer, by the Counter
                // arm's own precedent: subject existence plus version-chain
                // participation (fgdb-wodn).
                self.require_element(*subject)?;
                let previous = self.element_version(*subject).ok_or(match subject {
                    ElementId::Vertex(vid) => ApplyError::NoSuchVertex { vid: *vid },
                    ElementId::Edge(eid) => ApplyError::NoSuchEdge { eid: *eid },
                })?;
                let version = Self::successor_version(Some(previous), row)?;
                Self::require_closing(*before_value, *delta, *after_value)?;
                let actual = self.escrow_balance(*domain_id);
                if actual != *before_value {
                    return Err(ApplyError::EscrowBeforeMismatch {
                        domain: *domain_id,
                        declared: *before_value,
                        actual,
                    });
                }
                self.set_element_version(*subject, version)?;
                self.escrow.insert(*domain_id, *after_value);
                self.operation_keys.insert(*operation_key, row.clone());
            }
            DeltaRow::Sketch {
                operation_key,
                sketch_profile_oid,
                before_state_digest,
                after_state_oid,
            } => {
                if self.already_applied(operation_key, row)? {
                    return Ok(());
                }
                let actual = self
                    .sketches
                    .get(sketch_profile_oid)
                    .copied()
                    .unwrap_or([0u8; 32]);
                // ubs:ignore — canonical sketch state is a public before-image, not authentication material.
                if actual != *before_state_digest {
                    return Err(ApplyError::SketchBeforeMismatch {
                        profile: *sketch_profile_oid,
                        declared: *before_state_digest,
                        actual,
                    });
                }
                self.sketches.insert(*sketch_profile_oid, after_state_oid.0);
                self.operation_keys.insert(*operation_key, row.clone());
            }
            DeltaRow::Schema {
                before_epoch,
                after_epoch,
                ..
            } => {
                if self.schema_epoch != *before_epoch {
                    return Err(ApplyError::SchemaEpochMismatch {
                        declared: *before_epoch,
                        actual: self.schema_epoch,
                    });
                }
                self.schema_epoch = *after_epoch;
            }
            DeltaRow::Constraint {
                before_schema_root,
                after_schema_root,
                before_constraint_root,
                after_constraint_root,
            } => {
                if self.schema_root != *before_schema_root
                    || self.constraint_root != *before_constraint_root
                {
                    return Err(ApplyError::ConstraintRootMismatch {
                        declared_schema_root: *before_schema_root,
                        actual_schema_root: self.schema_root,
                    });
                }
                self.schema_root = *after_schema_root;
                self.constraint_root = *after_constraint_root;
            }
        }
        Ok(())
    }

    /// Apply every row of one coordinate entry, in the order given.
    ///
    /// The rows arrive canonically ordered from the template, so this is a
    /// total function of the entry rather than of how a caller iterated it.
    /// Rows are not staged here: if a later row is refused, earlier rows remain
    /// applied. [`ReferenceDatabase::apply_template`] obtains entry/template
    /// atomicity by applying to a clone and installing it only after all rows
    /// succeed; a direct caller needing the same boundary must stage likewise.
    pub fn apply_entry(&mut self, entry: &CoordinateEntry) -> Result<(), ApplyError> {
        for row in &entry.rows {
            self.apply_row(row)?;
        }
        Ok(())
    }

    /// Has this exact row already been folded in under this key?
    ///
    /// `Ok(true)` means replay — skip it. `Ok(false)` means new. An error means
    /// the key is reused by a different row, which is a defect rather than a
    /// replay and must not be silently skipped.
    fn already_applied(&self, key: &OperationKey, row: &DeltaRow) -> Result<bool, ApplyError> {
        match self.operation_keys.get(key) {
            None => Ok(false),
            Some(applied) if applied == row => Ok(true),
            Some(_) => Err(ApplyError::OperationKeyReused { key: *key }),
        }
    }

    fn require_closing(before: i128, delta: i128, after: i128) -> Result<(), ApplyError> {
        // Checked, because overflow policy is Reject (Appendix B) and there is
        // no saturating arm to encode.
        let closes = before
            .checked_add(delta)
            .map(|sum| sum == after)
            .unwrap_or(false);
        if closes {
            Ok(())
        } else {
            Err(ApplyError::ArithmeticDoesNotClose {
                before,
                delta,
                declared_after: after,
            })
        }
    }

    fn require_element(&self, elem: ElementId) -> Result<(), ApplyError> {
        match elem {
            ElementId::Vertex(vid) if !self.vertices.contains_key(&vid) => {
                Err(ApplyError::NoSuchVertex { vid })
            }
            ElementId::Edge(eid) if !self.edges.contains_key(&eid) => {
                Err(ApplyError::NoSuchEdge { eid })
            }
            _ => Ok(()),
        }
    }
}

/// How a branch came to exist (plan:2000).
///
/// A CLOSED union whose `Genesis` arm carries no parent, so a genesis branch
/// cannot name one even by mistake. That part of the plan's Genesis/Fork
/// distinction is structural rather than a validation someone must remember
/// to run.
///
/// SUBSET NOTE (doctrine 7): the plan's `Fork` arm additionally carries
/// `parent_head: StrongMarkerRef` and `boundary_reservation_identity`. Neither
/// is spellable honestly yet: this oracle has no committed marker-head model and
/// the reservation belongs to W4's certification machinery. The logical-command
/// boundary is modeled because committed applications now carry that independent
/// sequence and historical selection uses it. A placeholder head or reservation
/// would still be counterfeit evidence, so neither appears here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchOrigin {
    Genesis,
    Fork {
        parent_branch: BranchId,
        /// The selected stream-wide logical-command position.
        ///
        /// It is LOAD-BEARING as well as recorded: [`ReferenceDatabase::read`]
        /// caps each ancestor's contribution at its boundary, so this value is
        /// what keeps a parent's post-fork commits out of a child's historical
        /// reads. Deleting it would not merely drop a label — it would make every
        /// cross-fork read wrong.
        ///
        /// It is the database's logical-command frontier for a [`fork_branch`]
        /// and an earlier observed logical position for a [`fork_branch_at`].
        /// Transaction-only [`CommitSeq`] values are deliberately not accepted:
        /// control commands may occupy positions between commits, and plan:2000
        /// names exactly `fork_boundary_logical_command_seq`.
        ///
        /// [`fork_branch`]: ReferenceDatabase::fork_branch
        /// [`fork_branch_at`]: ReferenceDatabase::fork_branch_at
        fork_boundary: LogicalCommandSeq,
    },
}

/// Why a branch operation was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum BranchError {
    /// The parent named by a fork does not exist. Forking from nothing would
    /// produce a branch whose history has no origin.
    NoSuchParent { graph: GraphId, parent: BranchId },
    /// The target branch already exists. A branch is created once; permitting a
    /// second fork onto a live branch would silently replace its history.
    BranchExists { graph: GraphId, branch: BranchId },
    /// A branch may not fork from itself.
    SelfFork { branch: BranchId },
    /// The requested boundary is above the logical-command stream frontier — a
    /// fork from the database's future.
    ///
    /// Refused rather than clamped to the frontier. Clamping would make a fork
    /// silently mean something other than what it said, and the difference is
    /// invisible afterwards: the child's recorded boundary would be the frontier
    /// and nothing would show that a different one was asked for.
    BoundaryBeyondLogicalFrontier {
        graph: GraphId,
        parent: BranchId,
        logical_frontier: LogicalCommandSeq,
        requested: LogicalCommandSeq,
    },
    /// The boundary is neither zero nor a logical-command position observed in
    /// the reference stream.
    ///
    /// A numeric value below the frontier is not evidence that a command existed
    /// there. The full engine verifies a `BranchEpochBoundaryReservation`; this
    /// narrower oracle fails closed over the command positions it can actually
    /// witness.
    BoundaryNotObserved {
        graph: GraphId,
        parent: BranchId,
        requested: LogicalCommandSeq,
    },
    /// The parent's state at the boundary could not be materialized.
    ///
    /// An internal contradiction, since the boundary was checked against the
    /// parent's frontier first — surfaced with its cause rather than collapsed
    /// into one of the arms above, because "the boundary was wrong" and "the
    /// recorded stream does not re-apply" call for opposite responses.
    BoundaryNotMaterializable {
        graph: GraphId,
        parent: BranchId,
        boundary: LogicalCommandSeq,
        cause: Box<SnapshotError>,
    },
}

impl core::fmt::Display for BranchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchParent { graph, parent } => {
                write!(f, "no branch {parent:?} in graph {graph:?} to fork from")
            }
            Self::BranchExists { graph, branch } => {
                write!(f, "branch {branch:?} in graph {graph:?} already exists")
            }
            Self::SelfFork { branch } => write!(f, "branch {branch:?} cannot fork from itself"),
            Self::BoundaryBeyondLogicalFrontier {
                graph,
                parent,
                logical_frontier,
                requested,
            } => write!(
                f,
                "the logical-command stream is at {logical_frontier:?}; parent \
                 {parent:?} in graph {graph:?} cannot fork at {requested:?}"
            ),
            Self::BoundaryNotObserved {
                graph,
                parent,
                requested,
            } => write!(
                f,
                "parent {parent:?} in graph {graph:?} cannot fork at unobserved \
                 logical-command position {requested:?}"
            ),
            Self::BoundaryNotMaterializable {
                graph,
                parent,
                boundary,
                cause,
            } => write!(
                f,
                "parent {parent:?} in graph {graph:?} did not materialize at \
                 {boundary:?}: {cause}"
            ),
        }
    }
}

impl core::error::Error for BranchError {}

/// Authority that identifies the database lineage allowed to spend a snapshot.
///
/// A standalone database holds an `Arc` in every snapshot, preventing
/// allocator-address reuse after the issuer is dropped; equality is allocation
/// identity, not the `()` value inside it. Durable replay instead supplies the
/// plan's persisted [`DatabaseId`], so two independently materialized views of one
/// database retain the same authority across recovery.
#[derive(Clone)]
enum DatabaseAuthority {
    /// One standalone in-memory database lineage.
    Ephemeral(Arc<()>),
    /// A durable identity supplied by the recovery harness.
    Durable(DatabaseId),
}

impl DatabaseAuthority {
    fn fresh() -> Self {
        Self::Ephemeral(Arc::new(()))
    }

    fn durable(database_id: DatabaseId) -> Self {
        Self::Durable(database_id)
    }
}

impl core::fmt::Debug for DatabaseAuthority {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Ephemeral(_) => f.write_str("EphemeralDatabaseAuthority"),
            Self::Durable(_) => f.write_str("DurableDatabaseAuthority"),
        }
    }
}

impl PartialEq for DatabaseAuthority {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ephemeral(left), Self::Ephemeral(right)) => Arc::ptr_eq(left, right),
            (Self::Durable(left), Self::Durable(right)) => left == right,
            (Self::Ephemeral(_), Self::Durable(_)) | (Self::Durable(_), Self::Ephemeral(_)) => {
                false
            }
        }
    }
}

impl Eq for DatabaseAuthority {}

/// The materialized database: one [`ReferenceGraph`] per `(graph, branch)`.
///
/// Keyed by coordinate because a template may carry entries for several, and
/// applying them to one shared map would silently merge two branches — the
/// error a single-coordinate materializer cannot even represent.
#[derive(Clone, Debug, PartialEq)]
pub struct ReferenceDatabase {
    /// Opaque authority standing in for the plan's durable `database_id`.
    ///
    /// A `Clone` deliberately shares this authority: it is another materialized
    /// view of the same database lineage, and the history/head bases below decide
    /// whether a particular snapshot is still valid there. A separately
    /// constructed [`new`](Self::new) database gets a fresh authority even when
    /// its current bytes happen to match; durable recovery must opt into the same
    /// persisted ID through [`with_database_id`](Self::with_database_id). Content
    /// equality alone is not authority to spend a transaction capability.
    authority: DatabaseAuthority,
    coordinates: BTreeMap<(GraphId, BranchId), ReferenceGraph>,
    /// How each branch came to exist. Separate from the state map because a
    /// branch's origin is metadata about history, not part of the graph.
    origins: BTreeMap<(GraphId, BranchId), BranchOrigin>,
    /// The highest transaction prefix each coordinate can observe.
    ///
    /// For a genesis branch this advances when the branch itself is written. A
    /// fork starts at the greatest global commit prefix at or below its logical
    /// boundary, even when the last commit in that prefix touched another
    /// coordinate; the child's graph is unchanged, but its system-time cut is
    /// not an earlier global history.
    applied_through: BTreeMap<(GraphId, BranchId), CommitSeq>,
    /// The stream's frontier: the highest commit sequence this database has
    /// applied, across every coordinate.
    ///
    /// BESIDE the per-coordinate map, never instead of it. Intervening commits can
    /// touch other coordinates, so a coordinate's own frontier is genuinely below
    /// this one — and the fork boundary derivation and the conflict window both
    /// need the per-coordinate value. Two frontiers answering two different
    /// questions: "what has this branch seen" and "where is the stream".
    replay_frontier: CommitSeq,
    /// Highest semantic command position carried by the committed stream.
    ///
    /// Unlike `replay_frontier`, this is strictly increasing rather than
    /// gap-free: control commands consume logical positions without consuming a
    /// transaction commit sequence. Keeping the domains in distinct Rust types
    /// prevents a historical fork from silently selecting the wrong one.
    logical_command_frontier: LogicalCommandSeq,
    /// Logical-command position carried by each transaction commit.
    ///
    /// The mapping is append-only and order-preserving. It lets a snapshot in
    /// the transaction-only [`CommitSeq`] domain acquire the corresponding
    /// logical cut, and lets a fork at a logical position derive the greatest
    /// committed prefix at or below that boundary.
    stream_positions: BTreeMap<CommitSeq, LogicalCommandSeq>,
    /// The digest of the stream through each applied sequence.
    ///
    /// **THIS IS THE HISTORY COMPONENT OF SNAPSHOT PROVENANCE**
    /// (fgdb-reference-snapshot-provenance-9bvm). A `Snapshot` used to carry only
    /// `(graph, branch, high)`, so it was freely transferable: a snapshot minted
    /// against one database could be read against ANOTHER, which silently answered
    /// with its own state. With equal frontiers and divergent histories nothing
    /// distinguished them at all.
    ///
    /// Keyed by sequence rather than being one running digest, and that is the
    /// whole trick: the digest of the stream THROUGH `high` never changes as the
    /// database advances, so a snapshot stays valid across later commits (which is
    /// a law) while still being refused by a database whose history through `high`
    /// differs. A single current digest would invalidate every snapshot on the
    /// next commit.
    prefix_digests: BTreeMap<CommitSeq, [u8; 32]>,
    /// What each coordinate applied, at which sequence, oldest first.
    ///
    /// **THIS IS THE HISTORY MODEL, and its shape is the whole point.** B1's
    /// claim is that MVCC versions, time-travel, replication and branches are
    /// *the same mechanism* — an append-only commit stream — rather than four
    /// features that happen to coexist. An oracle that kept version chains
    /// beside the materialized state would be asserting that claim with a
    /// second, independent representation that could disagree with the stream.
    /// So the oracle keeps the stream, and every historical read is defined as
    /// the fold of it: **the state as of sequence S is what you get by applying
    /// every record through S, and nothing else.** That is a definition, not an
    /// algorithm, which is what §15 asks this crate to be.
    ///
    /// A forked branch's vector holds **only its own** commits, never a copy of
    /// its parent's. Reads follow the branch-parent link backwards and cap each
    /// ancestor at its fork boundary, so the child's pre-fork history *is* the
    /// parent's — the git semantics, and the one place this crate matches
    /// plan:451's "reads select the branch head and follow explicit
    /// branch-parent links" instead of copying.
    history: BTreeMap<(GraphId, BranchId), Vec<CommitRecord>>,
}

/// What a transaction reports when commit certification refuses it.
///
/// **A CONFLICT DOMAIN IS COARSER THAN A ROW AND FINER THAN A BRANCH,** and
/// which is which is a semantic decision per family rather than a mechanical one.
/// Most variants are ordinary first-committer-wins write domains: two rows
/// collide when they name the same key. Two are intentionally trace-only or
/// mode-bearing instead. [`Adjacency`](Self::Adjacency) is an SSI predicate
/// domain and never an SI write/write key; [`EndpointExistence`](Self::EndpointExistence)
/// has shared/exclusive access that cannot be represented by set intersection.
/// That makes omissions the interesting part, so both collectors have no
/// wildcard arm — a new row family is a compile error rather than a silent hole
/// that lets two conflicting transactions both commit.
///
/// [`DeltaRow::conflict_keys`]: ReferenceDatabase::conflict_keys_since
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKey {
    /// One vertex or edge version. Creating an edge names the EDGE and not its
    /// endpoints: under snapshot isolation two transactions adding different
    /// edges to one vertex do not conflict, which is correct SI behaviour and
    /// also precisely how phantoms slip through it. Detecting those is SSI's
    /// job, over adjacency, and it is not this rule.
    Element(ElementId),
    /// The outgoing-neighbour predicate for one vertex and relation.
    ///
    /// This is a logical SIREAD domain, not an ordinary write/write domain.
    /// [`crate::txn::Transaction::read_neighbours`] records it even when no edge
    /// exists, while the transaction trace records final edge creates/deletes
    /// that may change it. It is deliberately absent from
    /// [`CertificationSummary`]: two concurrent writers adding distinct edges
    /// to one adjacency are independent under SI, while either one forms an rw
    /// antidependency with a reader of this predicate.
    Adjacency { vertex: VId, relation: RelationId },
    /// One escrow domain.
    ///
    /// WORTH RECORDING: escrow exists in the plan so that concurrent
    /// reservations against one balance need NOT conflict. The row shape here
    /// cannot deliver that — `Escrow` carries `before_value`/`after_value` and
    /// the materializer checks the closure — so in this oracle an escrow write
    /// is an absolute checked transition like any other, and calling it
    /// commutative would be a claim the code does not support. Emitting the key
    /// is the honest reading of the row we actually have.
    Escrow(EscrowDomainId),
    /// One sketch profile's state, named by its profile object.
    Sketch(ObjectId),
    /// The coordinate's existence.
    ///
    /// Named by a transaction that claims to be the FIRST write to a branch, and
    /// by the window of any coordinate that came into existence after a
    /// transaction's basis. No row emits it — it is a claim about the coordinate
    /// rather than about its contents, which is exactly why element keys cannot
    /// catch two racing genesis transactions whose effects happen to be disjoint.
    CoordinateExistence,
    /// One vertex's continued existence as an edge endpoint.
    ///
    /// Reported by the asymmetric constraint-certification rule: creating an
    /// edge takes shared access to both endpoints, while deleting a vertex takes
    /// exclusive access to this domain. This key is deliberately NOT emitted by
    /// [`collect_conflict_keys`]. Putting it in an edge's ordinary write set
    /// would make two unrelated edge creations at one hub conflict.
    EndpointExistence(VId),
    /// The coordinate's schema and constraint roots, which the `Schema` and
    /// `Constraint` families both move. One key rather than two, because a
    /// transaction that moves the schema root and one that moves the constraint
    /// root are not independent: `Constraint` carries before/after images of
    /// BOTH axes, so either write invalidates the other's before-image.
    CoordinateRoots,
}

/// The SI commit-time domains that cannot be represented by one ordinary write
/// set.
///
/// Creating an edge reads the continued existence of both endpoints (shared);
/// deleting a vertex invalidates that fact (exclusive). Shared/shared is legal,
/// while either shared/exclusive ordering is a conflict. Keeping the modes in
/// separate sets is the part that prevents hub vertices from becoming
/// serialization points. SSI adjacency predicates are also intentionally absent
/// here: they live only in [`crate::ssi::TxnTrace`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CertificationSummary {
    writes: BTreeSet<ConflictKey>,
    endpoint_shared: BTreeSet<VId>,
    endpoint_exclusive: BTreeSet<VId>,
}

impl CertificationSummary {
    pub(crate) fn from_transaction(rows: &[DeltaRow], claims_genesis: bool) -> Self {
        let mut summary = Self::default();
        if claims_genesis {
            summary.writes.insert(ConflictKey::CoordinateExistence);
        }
        for row in rows {
            summary.collect_row(row);
        }
        summary
    }

    fn collect_row(&mut self, row: &DeltaRow) {
        collect_conflict_keys(row, &mut self.writes);
        match row {
            DeltaRow::CreateEdge { src, dst, .. } => {
                self.endpoint_shared.extend([*src, *dst]);
            }
            DeltaRow::DeleteVertex { vid, .. } => {
                self.endpoint_exclusive.insert(*vid);
            }
            DeltaRow::CreateVertex { .. }
            | DeltaRow::DeleteEdge { .. }
            | DeltaRow::LabelMembership { .. }
            | DeltaRow::Property { .. }
            | DeltaRow::ValidTime { .. }
            | DeltaRow::Counter { .. }
            | DeltaRow::Escrow { .. }
            | DeltaRow::Sketch { .. }
            | DeltaRow::Schema { .. }
            | DeltaRow::Constraint { .. } => {}
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.writes.is_empty()
            && self.endpoint_shared.is_empty()
            && self.endpoint_exclusive.is_empty()
    }

    /// Conflicts between this candidate and effects committed since its basis.
    pub(crate) fn conflicts_with(&self, committed: &Self) -> Vec<ConflictKey> {
        let mut conflicts: BTreeSet<ConflictKey> = self
            .writes
            .intersection(&committed.writes)
            .copied()
            .collect();

        // Shared/shared is intentionally absent. Exclusive/exclusive is already
        // the ordinary DeleteVertex write/write collision on the vertex element;
        // adding this reporting key too would make one cause appear twice.
        for vid in self
            .endpoint_shared
            .intersection(&committed.endpoint_exclusive)
            .chain(
                self.endpoint_exclusive
                    .intersection(&committed.endpoint_shared),
            )
        {
            conflicts.insert(ConflictKey::EndpointExistence(*vid));
        }
        conflicts.into_iter().collect()
    }
}

/// Add everything `row` ordinarily writes to `keys`.
///
/// TOTAL OVER THE ROW FAMILIES BY CONSTRUCTION: no wildcard arm, so a new
/// `DeltaRow` variant stops this crate compiling instead of quietly writing
/// nothing. Asymmetric constraint accesses are collected separately by
/// [`CertificationSummary`]; putting a shared dependency in this set would turn
/// it into an exclusive write. SSI adjacency predicates are collected from the
/// pre-effect workspace by [`crate::txn::Transaction`] and likewise stay out of
/// this SI set. A conflict rule that silently omits a family does not report a
/// missing conflict — it reports "no conflict", and both transactions commit.
pub fn collect_conflict_keys(row: &DeltaRow, keys: &mut BTreeSet<ConflictKey>) {
    match row {
        DeltaRow::CreateVertex { vid, .. } => {
            keys.insert(ConflictKey::Element(ElementId::Vertex(*vid)));
        }
        DeltaRow::CreateEdge { eid, .. } | DeltaRow::DeleteEdge { eid, .. } => {
            keys.insert(ConflictKey::Element(ElementId::Edge(*eid)));
        }
        DeltaRow::DeleteVertex {
            vid,
            sorted_retired_incident_edges,
            ..
        } => {
            // The cascade writes the edges too, so a concurrent write to any
            // retired edge is a conflict with this deletion.
            keys.insert(ConflictKey::Element(ElementId::Vertex(*vid)));
            keys.extend(
                sorted_retired_incident_edges
                    .iter()
                    .map(|eid| ConflictKey::Element(ElementId::Edge(*eid))),
            );
        }
        DeltaRow::LabelMembership { vid, .. } => {
            keys.insert(ConflictKey::Element(ElementId::Vertex(*vid)));
        }
        DeltaRow::Property { elem, .. }
        | DeltaRow::ValidTime { elem, .. }
        | DeltaRow::Counter { elem, .. } => {
            keys.insert(ConflictKey::Element(*elem));
        }
        DeltaRow::Escrow {
            domain_id, subject, ..
        } => {
            // Both: the row moves the domain balance AND is checked against the
            // subject, so a concurrent write to either invalidates it.
            keys.insert(ConflictKey::Escrow(*domain_id));
            keys.insert(ConflictKey::Element(*subject));
        }
        DeltaRow::Sketch {
            sketch_profile_oid, ..
        } => {
            keys.insert(ConflictKey::Sketch(*sketch_profile_oid));
        }
        DeltaRow::Schema { .. } | DeltaRow::Constraint { .. } => {
            keys.insert(ConflictKey::CoordinateRoots);
        }
    }
}

/// A `(graph, branch)` pair: one materialization coordinate.
type Coordinate = (GraphId, BranchId);

/// The two independent stream positions through which an ancestor may
/// contribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HistoryCut {
    commit_high: CommitSeq,
    logical_command_high: LogicalCommandSeq,
}

/// One ancestor of a lineage walk and its effective two-axis cut.
type CappedAncestor = (Coordinate, HistoryCut);

/// One coordinate's contribution at one commit sequence.
///
/// The whole entry is retained rather than a summary of it: replay must be the
/// same operation as the original apply, or the fold is a re-implementation of
/// the materializer and can drift from it.
#[derive(Clone, Debug, PartialEq)]
struct CommitRecord {
    commit_seq: CommitSeq,
    logical_command_seq: LogicalCommandSeq,
    entry: CoordinateEntry,
}

/// A read capability pinned to one coordinate at one commit sequence.
///
/// **THE SI ORACLE'S INVARIANT, and exactly how far the structural part reaches.**
/// §15 asks for an oracle asserting that no read sees a sequence above the
/// snapshot. Two mechanisms deliver it, and they are not the same strength:
///
/// * WITHIN one database it is structural. A `Snapshot` is minted only by
///   [`snapshot`](ReferenceDatabase::snapshot),
///   [`snapshot_at`](ReferenceDatabase::snapshot_at) or
///   [`genesis_snapshot`](ReferenceDatabase::genesis_snapshot), each of which
///   refuses a `high` above the coordinate's frontier, and there is no constructor
///   and no mutator — so "a read above the snapshot" is a state the type cannot
///   reach rather than a condition to detect.
/// * ACROSS database values it is CHECKED, not structural, and the earlier version of
///   this comment overclaimed by not saying so. A value with no provenance is
///   freely transferable, so nothing stopped a snapshot from one database being
///   read against another. It now carries an opaque issuing authority plus
///   history and head bases, and both
///   [`read`](ReferenceDatabase::read) and `Transaction::commit` revalidate it.
///
/// The distinction matters because the two failures look nothing alike: the first
/// would be a logic error inside one history, the second silently answers a
/// question about database A using database B's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotKind {
    /// A snapshot of a branch that existed when the capability was minted.
    ExistingHead,
    /// A capability to create a branch that was absent when it was minted.
    ///
    /// Kept distinct from `ExistingHead` at sequence zero: both see empty rows,
    /// but only this arm may legitimately reach coordinate-creation conflict
    /// certification when another handle of the same authority wins the race.
    GenesisClaim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    graph: GraphId,
    branch: BranchId,
    high: CommitSeq,
    kind: SnapshotKind,
    /// The database authority that minted this capability.
    ///
    /// A true [`ReferenceDatabase::clone`] shares it; a separately constructed
    /// same-state database does not. This is what closes the empty/genesis case,
    /// where there are no history bytes to distinguish two authorities.
    authority: DatabaseAuthority,
    /// The digest of the minting database's stream through `high`.
    ///
    /// Without it a snapshot was a bare `(graph, branch, high)` triple and
    /// therefore FREELY TRANSFERABLE: reading it against a different database
    /// silently answered with that database's state, and two databases with equal
    /// frontiers but divergent histories were indistinguishable
    /// (fgdb-reference-snapshot-provenance-9bvm).
    basis: [u8; 32],
    /// The exact branch ancestry that selects a head from that stream.
    lineage_basis: [u8; 32],
}

impl Snapshot {
    pub fn graph(&self) -> GraphId {
        self.graph
    }

    pub fn branch(&self) -> BranchId {
        self.branch
    }

    /// The highest commit sequence this snapshot can observe.
    pub fn high(&self) -> CommitSeq {
        self.high
    }
}

/// Why a snapshot could not be minted or read.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotError {
    /// No such coordinate. Distinct from an empty graph: "this branch does not
    /// exist" and "this branch has no vertices" are different answers, and
    /// collapsing them would let a typo read as a legitimately empty database.
    NoSuchCoordinate { graph: GraphId, branch: BranchId },
    /// The requested sequence is above what this coordinate has applied — a
    /// read of the future. Refused rather than clamped to the frontier, because
    /// a clamped answer is indistinguishable from a correct one: every "as of a
    /// later sequence" assertion would pass while measuring the present.
    BeyondFrontier {
        graph: GraphId,
        branch: BranchId,
        applied_through: CommitSeq,
        requested: CommitSeq,
    },
    /// A genesis snapshot was asked for on a coordinate that already exists.
    /// "Create this branch" and "start a transaction on this branch" are
    /// different intentions, so the permissive reading is refused.
    CoordinateAlreadyExists { graph: GraphId, branch: BranchId },
    /// The database says it applied `seq` but has no stream-prefix digest there.
    ///
    /// Private state construction makes this an internal contradiction today.
    /// Surfaced as a typed refusal because silently substituting an "unknown"
    /// sentinel would mint a capability with counterfeit provenance.
    PrefixDigestMissing { seq: CommitSeq },
    /// A committed prefix has no corresponding logical-command position.
    ///
    /// Successful application installs both axes atomically. Without this
    /// mapping a commit-domain snapshot cannot be projected into the logical
    /// domain used by branch ancestry, so replay fails closed.
    LogicalCommandPositionMissing { commit_seq: CommitSeq },
    /// The snapshot was minted against a different history.
    ///
    /// The issuing database authority, stream prefix, or selected branch lineage
    /// does not match this database at `high`. Refused rather than answered,
    /// because answering means silently substituting one database's state for
    /// another's — the reported failure was not a crash but a WRONG ANSWER, which
    /// is the worse of the two.
    ForeignSnapshot {
        graph: GraphId,
        branch: BranchId,
        high: CommitSeq,
    },
    /// A fork's parent is not in the database.
    ///
    /// Unreachable while branches are never removed, and kept deliberately: the
    /// lineage walk must be total, and the tempting total answer — treat the
    /// missing ancestor as contributing nothing — would silently return a graph
    /// missing all of its inherited state, which is a wrong answer rather than a
    /// refused one. If branch deletion ever lands, this is the arm it must hit.
    BrokenLineage {
        graph: GraphId,
        branch: BranchId,
        parent: BranchId,
    },
    /// A recorded commit failed to re-apply during replay. An internal
    /// contradiction — the stream that built the live state must rebuild it —
    /// surfaced as a typed error naming the record rather than a panic.
    HistoryNotApplicable {
        graph: GraphId,
        branch: BranchId,
        seq: CommitSeq,
        /// Boxed: an `ApplyError` is wide, this arm is the coldest one here, and
        /// every read in the crate returns this type by value.
        cause: Box<ApplyError>,
    },
}

impl core::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchCoordinate { graph, branch } => {
                write!(f, "no coordinate {graph:?}/{branch:?}")
            }
            Self::BeyondFrontier {
                graph,
                branch,
                applied_through,
                requested,
            } => write!(
                f,
                "coordinate {graph:?}/{branch:?} has applied through {applied_through:?}, \
                 cannot read as of {requested:?}"
            ),
            Self::CoordinateAlreadyExists { graph, branch } => {
                write!(f, "coordinate {graph:?}/{branch:?} already exists")
            }
            Self::PrefixDigestMissing { seq } => {
                write!(f, "the stream-prefix digest at {seq:?} is missing")
            }
            Self::LogicalCommandPositionMissing { commit_seq } => write!(
                f,
                "commit position {commit_seq:?} has no logical-command position"
            ),
            Self::ForeignSnapshot {
                graph,
                branch,
                high,
            } => write!(
                f,
                "the snapshot of {graph:?}/{branch:?} at {high:?} belongs to a \
                 different database authority, history, or branch head"
            ),
            Self::BrokenLineage {
                graph,
                branch,
                parent,
            } => write!(
                f,
                "coordinate {graph:?}/{branch:?} forked from missing parent {parent:?}"
            ),
            Self::HistoryNotApplicable {
                graph,
                branch,
                seq,
                cause,
            } => write!(
                f,
                "recorded commit {seq:?} of {graph:?}/{branch:?} did not re-apply: {cause}"
            ),
        }
    }
}

impl core::error::Error for SnapshotError {}

impl Default for ReferenceDatabase {
    fn default() -> Self {
        Self {
            authority: DatabaseAuthority::fresh(),
            coordinates: BTreeMap::new(),
            origins: BTreeMap::new(),
            applied_through: BTreeMap::new(),
            // Zero is "nothing applied": Chronicle's chain starts at 1, so the
            // first legal commit is its successor. Written out rather than derived
            // from a Default impl on CommitSeq, so the starting point of the
            // stream is stated where it matters.
            replay_frontier: CommitSeq(0),
            logical_command_frontier: LogicalCommandSeq(0),
            stream_positions: BTreeMap::new(),
            prefix_digests: BTreeMap::new(),
            history: BTreeMap::new(),
        }
    }
}

impl ReferenceDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstruct one durable database authority.
    ///
    /// Every replay/open of the same database must receive its persisted
    /// [`DatabaseId`]; independently authoritative databases must not share one.
    /// Unlike [`new`](Self::new), this constructor intentionally makes separately
    /// materialized values authority-compatible. Stream-prefix and exact-head
    /// bases still reject divergent reconstructions.
    pub fn with_database_id(database_id: DatabaseId) -> Self {
        Self {
            authority: DatabaseAuthority::durable(database_id),
            ..Self::default()
        }
    }

    /// The digest of the stream through `seq`.
    ///
    /// Zero is the empty stream, and every database's empty stream has the same
    /// content basis. Snapshot authority remains separate: identical empty content
    /// does not authorize one database to spend another's genesis capability.
    ///
    /// An unknown nonzero sequence is `None`, not an in-band sentinel that could
    /// collide with a real digest.
    pub fn prefix_digest(&self, seq: CommitSeq) -> Option<[u8; 32]> {
        if seq.0 == 0 {
            Some([0u8; 32])
        } else {
            self.prefix_digests.get(&seq).copied()
        }
    }

    /// The stream's frontier — the highest sequence applied across every
    /// coordinate. The next legal commit is its successor.
    pub fn replay_frontier(&self) -> CommitSeq {
        self.replay_frontier
    }

    /// The highest semantic command position observed in the committed stream.
    pub fn logical_command_frontier(&self) -> LogicalCommandSeq {
        self.logical_command_frontier
    }

    pub fn graph(&self, graph: GraphId, branch: BranchId) -> Option<&ReferenceGraph> {
        self.coordinates.get(&(graph, branch))
    }

    /// Refuse a create that would recycle an identity spent on another branch.
    ///
    /// `ReferenceGraph` enforces the rule within one materialized lineage. This
    /// database-level check closes the other half of plan §4.5: branches share
    /// the graph allocator rather than forking its namespace. The union of the
    /// coordinate spent sets is therefore graph-wide allocation history. That
    /// also covers a historical fork taken before the original create: its
    /// child materialization does not contain the identity, but a sibling or
    /// parent coordinate still proves that the slot was spent.
    ///
    /// A live identity on the target coordinate is left to `apply_row`, which
    /// reports the more precise `*AlreadyExists` error. Rows earlier in this
    /// same entry are likewise handled by sequential application; this guard is
    /// specifically the cross-coordinate boundary.
    ///
    /// Callers run this against the immutable pre-template database, not the
    /// candidate. One atomic template may project one newly allocated identity
    /// into several branch coordinates at the same birth; that is one first use,
    /// not recycling. Once the template commits, every later template sees the
    /// identity in at least one coordinate spent set and cannot mint it again.
    fn preflight_graph_identity_reuse(&self, entry: &CoordinateEntry) -> Result<(), ApplyError> {
        let local = self.coordinates.get(&(entry.graph, entry.branch));
        for row in &entry.rows {
            match row {
                DeltaRow::CreateVertex { vid, .. } => {
                    let locally_live = local.is_some_and(|graph| graph.vertices.contains_key(vid));
                    let spent_in_graph = self.coordinates.iter().any(|((graph, _), state)| {
                        *graph == entry.graph && state.spent_vertex_ids.contains(vid)
                    });
                    if !locally_live && spent_in_graph {
                        return Err(ApplyError::VertexIdentitySpent { vid: *vid });
                    }
                }
                DeltaRow::CreateEdge { eid, .. } => {
                    let locally_live = local.is_some_and(|graph| graph.edges.contains_key(eid));
                    let spent_in_graph = self.coordinates.iter().any(|((graph, _), state)| {
                        *graph == entry.graph && state.spent_edge_ids.contains(eid)
                    });
                    if !locally_live && spent_in_graph {
                        return Err(ApplyError::EdgeIdentitySpent { eid: *eid });
                    }
                }
                DeltaRow::DeleteVertex { .. }
                | DeltaRow::DeleteEdge { .. }
                | DeltaRow::LabelMembership { .. }
                | DeltaRow::Property { .. }
                | DeltaRow::ValidTime { .. }
                | DeltaRow::Counter { .. }
                | DeltaRow::Escrow { .. }
                | DeltaRow::Sketch { .. }
                | DeltaRow::Schema { .. }
                | DeltaRow::Constraint { .. } => {}
            }
        }
        Ok(())
    }

    /// The highest transaction prefix this coordinate can observe.
    ///
    /// This is the *bound* on visibility, not visibility itself: it is the
    /// largest `high` [`snapshot_at`](Self::snapshot_at) will mint. For a fork it
    /// is the global commit prefix paired with the branch's logical-command
    /// boundary, not necessarily the last commit that changed the parent.
    /// Reading the graph as it stood at an earlier sequence goes through
    /// [`read`](Self::read).
    pub fn applied_through(&self, graph: GraphId, branch: BranchId) -> Option<CommitSeq> {
        self.applied_through.get(&(graph, branch)).copied()
    }

    /// Everything **visible to this coordinate** that was committed after `since`
    /// wrote, as conflict keys.
    ///
    /// This is the raw material of first-committer-wins: a transaction that read
    /// at `since` conflicts exactly when its own write set meets this.
    ///
    /// **IT WALKS THE LINEAGE, and the first version did not — that was a real
    /// defect (fgdb-reference-historical-fork-conflict-lineage-re6w), found by
    /// another pane reading 4f860e9.** Consulting only the coordinate's own
    /// records is correct for a transaction reading at the frontier, and wrong for
    /// one reading BELOW a fork boundary: the inherited records in
    /// `(since, boundary]` are visible to the child and invisible to its own
    /// history, so the check reported "disjoint" and the stale before-image then
    /// failed at apply time as `TxnError::Apply` — a concurrency outcome wearing
    /// the label of an internal contradiction.
    ///
    /// Each ancestor is still capped at its own fork boundary, so a parent's
    /// commits AFTER the fork are not conflicts for the child: they are not
    /// visible to it at all. For a transaction reading at the frontier the
    /// per-ancestor window `(since, cap]` is empty, so nothing about the
    /// non-historical case changes.
    ///
    /// Returns an error rather than an empty set when the lineage cannot be
    /// walked. An empty set means "no conflicts", which is the answer that lets a
    /// commit through — exactly the wrong default for a question that could not be
    /// answered.
    pub fn conflict_keys_since(
        &self,
        graph: GraphId,
        branch: BranchId,
        since: CommitSeq,
    ) -> Result<BTreeSet<ConflictKey>, SnapshotError> {
        Ok(self.certification_since(graph, branch, since)?.writes)
    }

    /// Full write plus asymmetric-constraint summary visible after `since`.
    ///
    /// This walks the same capped lineage as [`conflict_keys_since`](Self::conflict_keys_since).
    /// Constraint certification that ignored inherited commits would recreate
    /// the historical-child apply-error window under a different key family.
    pub(crate) fn certification_since(
        &self,
        graph: GraphId,
        branch: BranchId,
        since: CommitSeq,
    ) -> Result<CertificationSummary, SnapshotError> {
        let mut summary = CertificationSummary::default();
        let frontier = self
            .applied_through(graph, branch)
            .ok_or(SnapshotError::NoSuchCoordinate { graph, branch })?;

        // COORDINATE CREATION IS ITSELF A CONFLICT DOMAIN
        // (fgdb-reference-genesis-transaction-race-dfk3). Two transactions each
        // claiming to be the first write to a branch both used an empty basis, so
        // whichever loses computed every before-image against a state that never
        // existed. Their effects can be disjoint, so no element key catches it.
        let born_by_commit = self
            .history
            .get(&(graph, branch))
            .and_then(|records| records.first())
            .is_some_and(|first| first.commit_seq.0 > since.0);

        // ...AND A FORK IS THE OTHER WAY A COORDINATE COMES INTO EXISTENCE
        // (fgdb-1xqd). `fork_branch_at` appends nothing to the stream and writes
        // no history record — the child "owns an empty record vector" by design,
        // which is what makes a fork O(1) in the dimension time-travel reads. So
        // the own-history test above is structurally blind to it: a genesis
        // claimant would certify cleanly against a branch that had been forked
        // into existence underneath it, and commit a template evaluated on the
        // empty graph into a coordinate that already had a parent's state.
        //
        // `origins` is the register that knows both ways, so ask it. Genesis is
        // recorded there too — but only on FIRST WRITE, inside the candidate,
        // which is strictly after this runs. So at certification time an entry
        // here means "someone else brought this coordinate into being", which is
        // exactly the claim a genesis transaction is asserting is false.
        //
        // THAT ORDERING IS LOAD-BEARING AND IT IS NOT LOCAL TO THIS FUNCTION.
        // Move the `origins.entry(key).or_insert(BranchOrigin::Genesis)` in
        // `apply_template` to anywhere before certification and this test starts
        // matching a transaction's own pending genesis, refusing every genesis
        // commit in the system. What catches that is
        // `an_uncontested_genesis_transaction_still_commits` in
        // tests/transaction_anomalies.rs — verified 2026-08-01 to still pass with
        // this clause forced off, so it constrains the ordering rather than
        // riding on this fix.
        //
        // A non-genesis transaction on a freshly forked branch also matches (it
        // has no own record at or before its basis) and that is harmless: the key
        // only bites when it is in BOTH sets, and only a genesis claimant carries
        // CoordinateExistence in its own.
        let born_by_fork = self.origins.contains_key(&(graph, branch))
            && !self
                .history
                .get(&(graph, branch))
                .is_some_and(|records| records.iter().any(|r| r.commit_seq.0 <= since.0));

        if born_by_commit || born_by_fork {
            summary.writes.insert(ConflictKey::CoordinateExistence);
        }

        for (key, cap) in self.lineage(graph, branch, frontier)? {
            let Some(records) = self.history.get(&key) else {
                continue;
            };
            for record in records {
                if record.commit_seq.0 <= since.0 {
                    continue;
                }
                if record.commit_seq.0 > cap.commit_high.0
                    || record.logical_command_seq.0 > cap.logical_command_high.0
                {
                    break;
                }
                for row in &record.entry.rows {
                    summary.collect_row(row);
                }
            }
        }
        Ok(summary)
    }

    /// How many commits this coordinate recorded **itself**.
    ///
    /// A forked child reports 0 until it is written to, however large its
    /// inherited state: its history is its parent's, reached by link. Exposed so
    /// that "the fork shared history rather than copying it" is an assertion a
    /// test can make about the mechanism, not only about the answers.
    pub fn recorded_commits(&self, graph: GraphId, branch: BranchId) -> usize {
        self.history.get(&(graph, branch)).map_or(0, |records| {
            // History has one record per RELATION entry. Several entries
            // may share one atomic commit sequence, so record count is not
            // commit count.
            records
                .iter()
                .map(|record| record.commit_seq)
                .collect::<BTreeSet<_>>()
                .len()
        })
    }

    /// Mint a snapshot at this coordinate's current frontier.
    ///
    /// The read-your-own-writes position: everything committed so far, nothing
    /// after.
    pub fn snapshot(&self, graph: GraphId, branch: BranchId) -> Result<Snapshot, SnapshotError> {
        let high = self
            .applied_through(graph, branch)
            .ok_or(SnapshotError::NoSuchCoordinate { graph, branch })?;
        let basis = self
            .prefix_digest(high)
            .ok_or(SnapshotError::PrefixDigestMissing { seq: high })?;
        self.logical_position_for_commit(high)?;
        Ok(Snapshot {
            graph,
            branch,
            high,
            kind: SnapshotKind::ExistingHead,
            authority: self.authority.clone(),
            basis,
            lineage_basis: self.lineage_digest(graph, branch)?,
        })
    }

    /// Mint the empty snapshot of a coordinate that **does not exist yet**.
    ///
    /// FOUND BY WRITING THE TRANSACTION LAWS: there was no way to express the
    /// first transaction on a branch. `snapshot` refuses an absent coordinate —
    /// correctly, since reading a branch nobody created is a caller error — but
    /// writing is precisely how a branch comes to exist, so refusing there closed
    /// the only door in. `apply_template` already creates a coordinate on first
    /// write and records it as `Genesis`; this is that same rule reachable from
    /// the transaction path.
    ///
    /// It REFUSES a coordinate that already exists, rather than quietly handing
    /// back sequence zero. The two cases want opposite handling — "create this
    /// branch" and "start a transaction on this branch" are different intentions
    /// — and a permissive `begin` that silently fell back to genesis would let a
    /// typo'd branch name read as a legitimately new branch.
    pub fn genesis_snapshot(
        &self,
        graph: GraphId,
        branch: BranchId,
    ) -> Result<Snapshot, SnapshotError> {
        if self.origins.contains_key(&(graph, branch)) {
            return Err(SnapshotError::CoordinateAlreadyExists { graph, branch });
        }
        Ok(Snapshot {
            graph,
            branch,
            high: CommitSeq(0),
            kind: SnapshotKind::GenesisClaim,
            authority: self.authority.clone(),
            // The empty stream and empty visible lineage have common content
            // bases. `authority` above still distinguishes independent databases.
            basis: [0u8; 32],
            lineage_basis: [0u8; 32],
        })
    }

    /// Mint a snapshot as of an earlier sequence — `FOR SYSTEM_TIME AS OF`.
    ///
    /// Refuses a sequence above the frontier. `CommitSeq(0)` is legal and names
    /// the state before this coordinate's first commit.
    pub fn snapshot_at(
        &self,
        graph: GraphId,
        branch: BranchId,
        high: CommitSeq,
    ) -> Result<Snapshot, SnapshotError> {
        let frontier = self
            .applied_through(graph, branch)
            .ok_or(SnapshotError::NoSuchCoordinate { graph, branch })?;
        if high.0 > frontier.0 {
            return Err(SnapshotError::BeyondFrontier {
                graph,
                branch,
                applied_through: frontier,
                requested: high,
            });
        }
        let basis = self
            .prefix_digest(high)
            .ok_or(SnapshotError::PrefixDigestMissing { seq: high })?;
        self.logical_position_for_commit(high)?;
        Ok(Snapshot {
            graph,
            branch,
            high,
            kind: SnapshotKind::ExistingHead,
            authority: self.authority.clone(),
            basis,
            lineage_basis: self.lineage_digest(graph, branch)?,
        })
    }

    /// Materialize what a snapshot observes.
    ///
    /// Returns an owned [`ReferenceGraph`], so every existing read — property
    /// lookups, adjacency, path modes, valid-time selectors — applies to a
    /// historical state with no second implementation. That is why the result is
    /// a graph and not a bundle of specialized accessors: `AS OF <sequence>`
    /// (system time) and `AS OF <instant>` (valid time) are independent
    /// dimensions, and composing them must not require a bitemporal method per
    /// read.
    ///
    /// A snapshot at the frontier reconstructs the live graph exactly. That
    /// equality is the load-bearing internal check on this whole mechanism: it
    /// says the recorded stream and the materialized state are two views of one
    /// fact rather than two facts that must be kept in step.
    pub fn read(&self, snapshot: &Snapshot) -> Result<ReferenceGraph, SnapshotError> {
        self.check_provenance(snapshot)?;
        // A genesis snapshot names a coordinate with no history. Sequence zero of
        // a branch that does not exist yet is the same empty graph as sequence
        // zero of one that does, so this answers rather than refusing — which
        // keeps every minted snapshot readable instead of leaving one shape of
        // Snapshot that `read` rejects.
        if snapshot.high.0 == 0
            && !self
                .origins
                .contains_key(&(snapshot.graph, snapshot.branch))
        {
            return Ok(ReferenceGraph::new());
        }
        let logical_command_high = self.logical_position_for_commit(snapshot.high)?;
        self.materialize_at_cut(
            snapshot.graph,
            snapshot.branch,
            HistoryCut {
                commit_high: snapshot.high,
                logical_command_high,
            },
        )
    }

    /// Fold one branch lineage through an exact two-axis history cut.
    fn materialize_at_cut(
        &self,
        graph_id: GraphId,
        branch_id: BranchId,
        cut: HistoryCut,
    ) -> Result<ReferenceGraph, SnapshotError> {
        let mut graph = ReferenceGraph::new();
        for (key, cap) in self.lineage_at_cut(graph_id, branch_id, cut)? {
            let Some(records) = self.history.get(&key) else {
                continue;
            };
            for record in records {
                // Both axes ascend together across committed records. A record
                // above either cap means every later record is above it too.
                if record.commit_seq.0 > cap.commit_high.0
                    || record.logical_command_seq.0 > cap.logical_command_high.0
                {
                    break;
                }
                graph.apply_entry(&record.entry).map_err(|cause| {
                    SnapshotError::HistoryNotApplicable {
                        graph: key.0,
                        branch: key.1,
                        seq: record.commit_seq,
                        cause: Box::new(cause),
                    }
                })?;
            }
        }
        Ok(graph)
    }

    /// Refuse a snapshot outside this database authority or selected history/head.
    ///
    /// Authority closes the empty-stream case and keeps an independently rebuilt
    /// same-state database from spending this capability. Within one authority
    /// (including a true clone), the stream-prefix and exact-lineage bases answer
    /// the remaining question: *does this value still select the history and head
    /// that minted it?* Everything else is refused, including equal-frontier
    /// divergent histories and forks whose lineage changed without a stream append.
    pub fn check_provenance(&self, snapshot: &Snapshot) -> Result<(), SnapshotError> {
        // ubs:ignore — non-secret stream-history content identity, not authentication material.
        let prefix_matches = self.prefix_digest(snapshot.high) == Some(snapshot.basis);
        if self.authority != snapshot.authority || !prefix_matches {
            return Err(SnapshotError::ForeignSnapshot {
                graph: snapshot.graph,
                branch: snapshot.branch,
                high: snapshot.high,
            });
        }
        let lineage_matches = match snapshot.kind {
            SnapshotKind::ExistingHead => self
                .lineage_digest(snapshot.graph, snapshot.branch)
                .is_ok_and(|basis| basis == snapshot.lineage_basis),
            SnapshotKind::GenesisClaim => {
                snapshot.high.0 == 0 && snapshot.lineage_basis == [0u8; 32]
            }
        };
        if !lineage_matches {
            return Err(SnapshotError::ForeignSnapshot {
                graph: snapshot.graph,
                branch: snapshot.branch,
                high: snapshot.high,
            });
        }
        Ok(())
    }

    /// Digest the exact ancestry that identifies a selected branch head.
    ///
    /// The stream prefix alone is insufficient because `fork_branch_at` changes
    /// lineage without appending a template. Hashing only the effective read caps
    /// is also insufficient: at sequence zero every cap is zero, and at a sequence
    /// below two different fork boundaries both reads may currently fold the same
    /// rows. A snapshot is still a capability for one selected head, so the raw
    /// parent and fork boundary are part of the identity.
    fn lineage_digest(&self, graph: GraphId, branch: BranchId) -> Result<[u8; 32], SnapshotError> {
        let mut chain = Vec::new();
        let mut key = (graph, branch);
        loop {
            let origin =
                self.origins
                    .get(&key)
                    .copied()
                    .ok_or(SnapshotError::NoSuchCoordinate {
                        graph: key.0,
                        branch: key.1,
                    })?;
            chain.push((key, origin));
            match origin {
                BranchOrigin::Genesis => break,
                BranchOrigin::Fork { parent_branch, .. } => {
                    key = (key.0, parent_branch);
                }
            }
        }
        chain.reverse();

        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(LINEAGE_DIGEST_DOMAIN);
        for ((ancestor_graph, ancestor_branch), origin) in chain {
            hasher.update(&ancestor_graph.0.to_le_bytes());
            hasher.update(&ancestor_branch.0.to_le_bytes());
            match origin {
                BranchOrigin::Genesis => {
                    hasher.update(&[0]);
                }
                BranchOrigin::Fork {
                    parent_branch,
                    fork_boundary,
                } => {
                    hasher.update(&[1]);
                    hasher.update(&parent_branch.0.to_le_bytes());
                    hasher.update(&fork_boundary.0.to_le_bytes());
                }
            }
        }
        Ok(hasher.finalize().0)
    }

    /// The logical-command position carried by a transaction commit.
    fn logical_position_for_commit(
        &self,
        commit_seq: CommitSeq,
    ) -> Result<LogicalCommandSeq, SnapshotError> {
        if commit_seq.0 == 0 {
            return Ok(LogicalCommandSeq(0));
        }
        self.stream_positions
            .get(&commit_seq)
            .copied()
            .ok_or(SnapshotError::LogicalCommandPositionMissing { commit_seq })
    }

    /// Greatest transaction commit whose logical position is at or below
    /// `boundary`.
    ///
    /// Values are strictly increasing in commit order, so the reverse scan stops
    /// at the first match. The reference oracle deliberately favors the obvious
    /// definition over a second index that could disagree with this one.
    fn commit_frontier_at_logical(&self, boundary: LogicalCommandSeq) -> CommitSeq {
        self.stream_positions
            .iter()
            .rev()
            .find_map(|(commit_seq, logical_command_seq)| {
                (logical_command_seq.0 <= boundary.0).then_some(*commit_seq)
            })
            .unwrap_or(CommitSeq(0))
    }

    fn logical_position_is_observed(&self, boundary: LogicalCommandSeq) -> bool {
        boundary.0 == 0
            || self
                .stream_positions
                .values()
                .any(|position| position.0 == boundary.0)
    }

    /// The ancestor chain from genesis to `branch`, each capped in both
    /// transaction-commit and logical-command domains.
    ///
    /// Each ancestor's cap is the minimum of the requested `high` and every fork
    /// boundary below it. Taking the minimum is what makes a child's pre-fork
    /// read equal its parent's read at the same sequence *without* letting the
    /// parent's post-fork commits leak in — the two facts a branch model has to
    /// deliver at once, and the reason the cap is per-ancestor rather than one
    /// global filter.
    ///
    /// Iterative, not recursive: the depth is the fork depth, which is
    /// user-controlled.
    fn lineage(
        &self,
        graph: GraphId,
        branch: BranchId,
        high: CommitSeq,
    ) -> Result<Vec<CappedAncestor>, SnapshotError> {
        self.lineage_at_cut(
            graph,
            branch,
            HistoryCut {
                commit_high: high,
                logical_command_high: self.logical_position_for_commit(high)?,
            },
        )
    }

    fn lineage_at_cut(
        &self,
        graph: GraphId,
        branch: BranchId,
        cut: HistoryCut,
    ) -> Result<Vec<CappedAncestor>, SnapshotError> {
        let mut chain: Vec<CappedAncestor> = Vec::new();
        let mut key = (graph, branch);
        let mut cap = cut;
        loop {
            chain.push((key, cap));
            match self.origins.get(&key) {
                None => {
                    return Err(SnapshotError::NoSuchCoordinate {
                        graph: key.0,
                        branch: key.1,
                    });
                }
                Some(BranchOrigin::Genesis) => break,
                Some(BranchOrigin::Fork {
                    parent_branch,
                    fork_boundary,
                }) => {
                    let parent_key = (key.0, *parent_branch);
                    if !self.origins.contains_key(&parent_key) {
                        return Err(SnapshotError::BrokenLineage {
                            graph: key.0,
                            branch: key.1,
                            parent: *parent_branch,
                        });
                    }
                    if fork_boundary.0 < cap.logical_command_high.0 {
                        cap.logical_command_high = *fork_boundary;
                    }
                    let boundary_commit = self.commit_frontier_at_logical(*fork_boundary);
                    if boundary_commit.0 < cap.commit_high.0 {
                        cap.commit_high = boundary_commit;
                    }
                    key = parent_key;
                }
            }
        }
        chain.reverse();
        Ok(chain)
    }

    /// How this branch came to exist, if it exists.
    ///
    /// A coordinate that received writes without an explicit fork is `Genesis`:
    /// it has no parent, which is the honest reading of a branch nobody forked.
    pub fn branch_origin(&self, graph: GraphId, branch: BranchId) -> Option<BranchOrigin> {
        self.origins.get(&(graph, branch)).copied()
    }

    /// Fork `child` from `parent` at its current materialized state.
    ///
    /// **THE SEMANTICS, which is what the real engine must match:** the child
    /// begins as exactly the parent's state when this method is called, and
    /// from then on the two diverge with no leakage in either direction. That
    /// is B1's git-style branching and B6's branch-per-agent isolation, and it
    /// is what the laws in `tests/branch_fork.rs` pin.
    ///
    /// Defined as [`fork_branch_at`](Self::fork_branch_at) at the current
    /// logical-command frontier, so there is one fork mechanism rather than two
    /// that must agree.
    /// The boundary stays DERIVED here — supplying it is what fgdb-vyb0 got
    /// wrong — and the historical form is a separate method precisely so that
    /// "fork here" cannot be spelled by accidentally passing a stale sequence.
    ///
    /// **THE COMPLEXITY, which the real engine must NOT match.** The child's
    /// state is materialized in full, which is O(n). plan:451 is explicit that
    /// branch creation "adds only metadata and key wraps, so its data-copy
    /// complexity is O(1)" and that "reads select the branch head and follow
    /// explicit branch-parent links atop structurally shared objects". This
    /// produces identical OBSERVABLE behaviour by a mechanism the engine may not
    /// use. That is legitimate here and only here: §15 defines this crate as
    /// deliberately simple, single-threaded and never optimized, existing so that
    /// "what should this return" is a program. It would be a doctrine-7
    /// substitute anywhere else, and the same plan paragraph warns specifically
    /// against falsely claiming O(1), so nothing in this crate should ever be
    /// cited as evidence about fork cost.
    pub fn fork_branch(
        &mut self,
        graph: GraphId,
        parent: BranchId,
        child: BranchId,
    ) -> Result<(), BranchError> {
        // Read before the shape checks so a missing parent is still reported as
        // NoSuchParent rather than as a boundary problem.
        self.applied_through
            .get(&(graph, parent))
            .copied()
            .ok_or(BranchError::NoSuchParent { graph, parent })?;
        self.fork_branch_at(graph, parent, child, self.logical_command_frontier)
    }

    /// Fork `child` from `parent` **as the parent stood at `boundary`** — a
    /// branch rooted at a point in history, not only at the present.
    ///
    /// This is what plan:2000's `fork_boundary_logical_command_seq` asks for, and
    /// it was genuinely unlandable until [`read`](Self::read) existed: with no
    /// history to select in, a caller-supplied boundary could only be stored,
    /// which made it an unauthenticated label rather than oracle evidence
    /// (fgdb-vyb0). It is landable now for exactly one reason — the boundary is
    /// *used*: the child's state is the parent's fold at that logical position,
    /// and the boundary must be an observed command position no later than the
    /// stream frontier. Delete the parameter and the child may inherit a
    /// different graph.
    ///
    /// `LogicalCommandSeq(0)` is a legal boundary and produces an empty child: a
    /// branch taken from before the parent's first command. Refusing zero would
    /// be arbitrary — the parent's state at zero is a state the parent genuinely
    /// had.
    pub fn fork_branch_at(
        &mut self,
        graph: GraphId,
        parent: BranchId,
        child: BranchId,
        boundary: LogicalCommandSeq,
    ) -> Result<(), BranchError> {
        if parent == child {
            return Err(BranchError::SelfFork { branch: child });
        }
        if self.coordinates.contains_key(&(graph, child))
            || self.origins.contains_key(&(graph, child))
        {
            return Err(BranchError::BranchExists {
                graph,
                branch: child,
            });
        }
        self.applied_through
            .get(&(graph, parent))
            .copied()
            .ok_or(BranchError::NoSuchParent { graph, parent })?;
        if boundary.0 > self.logical_command_frontier.0 {
            return Err(BranchError::BoundaryBeyondLogicalFrontier {
                graph,
                parent,
                logical_frontier: self.logical_command_frontier,
                requested: boundary,
            });
        }
        if !self.logical_position_is_observed(boundary) {
            return Err(BranchError::BoundaryNotObserved {
                graph,
                parent,
                requested: boundary,
            });
        }

        // The child's state is the parent's HISTORY folded to the boundary, not
        // a copy of the parent's present. At the frontier the two are equal —
        // that is the faithfulness law — so routing the current-state fork
        // through here costs nothing and removes the second implementation that
        // could drift from this one.
        let commit_high = self.commit_frontier_at_logical(boundary);
        let inherited = self
            .materialize_at_cut(
                graph,
                parent,
                HistoryCut {
                    commit_high,
                    logical_command_high: boundary,
                },
            )
            .map_err(|cause| BranchError::BoundaryNotMaterializable {
                graph,
                parent,
                boundary,
                cause: Box::new(cause),
            })?;

        // The child's state is materialized; its HISTORY is not copied. The child
        // owns an empty record vector and reaches everything before the boundary
        // through the parent link, so a fork costs one metadata row in the
        // dimension that time-travel reads — the mechanism plan:451 describes,
        // even though the state materialization beside it is not.
        self.coordinates.insert((graph, child), inherited);
        self.applied_through.insert((graph, child), commit_high);
        self.origins.insert(
            (graph, child),
            BranchOrigin::Fork {
                parent_branch: parent,
                fork_boundary: boundary,
            },
        );
        Ok(())
    }

    pub fn coordinate_count(&self) -> usize {
        self.coordinates.len()
    }

    /// Validate every entry against the immutable state on which the whole
    /// template was prepared.
    ///
    /// This cannot happen while applying entries to the candidate: two entries
    /// may name the same graph/branch under different relations, and an earlier
    /// schema row would then rewrite the basis checked for a later entry. The
    /// plan instead binds every entry to the one pre-template schema state.
    fn preflight_template_schema(&self, template: &LogicalDeltaTemplate) -> Result<(), ApplyError> {
        for entry in template.coordinate_entries() {
            let actual = self
                .coordinates
                .get(&(entry.graph, entry.branch))
                .map_or(SchemaEpoch(0), ReferenceGraph::schema_epoch);
            if entry.schema_epoch != actual {
                return Err(ApplyError::SchemaBindingMismatch {
                    graph: entry.graph,
                    branch: entry.branch,
                    relation: entry.relation,
                    declared: entry.schema_epoch,
                    actual,
                });
            }

            let mut schema_rows = Vec::new();
            for row in &entry.rows {
                if let DeltaRow::Schema {
                    transition_oid,
                    before_epoch,
                    ..
                } = row
                {
                    schema_rows.push(*transition_oid);
                    if *before_epoch != entry.schema_epoch {
                        return Err(ApplyError::SchemaEpochMismatch {
                            declared: *before_epoch,
                            actual: entry.schema_epoch,
                        });
                    }
                }
            }

            let transition_matches = match (entry.schema_transition, schema_rows.as_slice()) {
                (None, []) => true,
                (Some(declared), [row]) => declared == *row,
                _ => false,
            };
            if !transition_matches {
                return Err(ApplyError::SchemaTransitionMismatch {
                    graph: entry.graph,
                    branch: entry.branch,
                    relation: entry.relation,
                    declared: entry.schema_transition,
                    schema_rows,
                });
            }
        }
        Ok(())
    }

    /// Apply a whole template — every coordinate entry it carries.
    ///
    /// ALL OR NOTHING. The template is validated against a *clone* and only
    /// swapped in once every entry applied, so a template that is applicable at
    /// entry 1 and not at entry 3 leaves the database exactly as it was. A
    /// partially-applied commit would put the database in a state no commit
    /// stream describes, which is the one outcome an oracle must never produce.
    ///
    /// `commit_seq` is gap-free and exact-next. `logical_command_seq` is the
    /// independent semantic-command position and need only advance, because
    /// control commands may occupy positions between two transaction commits.
    pub fn apply_template(
        &mut self,
        template: &LogicalDeltaTemplate,
        commit_seq: CommitSeq,
        logical_command_seq: LogicalCommandSeq,
    ) -> Result<(), ApplyError> {
        let expected = self.replay_frontier.checked_successor()?;
        if template.coordinate_entries().is_empty() {
            return Err(ApplyError::EmptyTemplate);
        }
        if commit_seq != expected {
            return Err(ApplyError::SequenceNotNext {
                expected,
                offered: commit_seq,
            });
        }
        if self.logical_command_frontier.0 == u64::MAX {
            return Err(ApplyError::LogicalCommandSeqExhausted {
                frontier: self.logical_command_frontier,
            });
        }
        if logical_command_seq.0 <= self.logical_command_frontier.0 {
            return Err(ApplyError::LogicalCommandSequenceNotAdvancing {
                previous: self.logical_command_frontier,
                offered: logical_command_seq,
            });
        }
        self.preflight_template_schema(template)?;
        let mut candidate = self.clone();
        candidate.replay_frontier = commit_seq;
        candidate.logical_command_frontier = logical_command_seq;
        candidate
            .stream_positions
            .insert(commit_seq, logical_command_seq);
        // Folded over the PREVIOUS prefix and this template's canonical bytes, so
        // the value at each sequence is a function of the whole stream up to it —
        // two databases agree here exactly when their histories agree.
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(PREFIX_DIGEST_DOMAIN);
        let previous =
            self.prefix_digest(self.replay_frontier)
                .ok_or(ApplyError::PrefixDigestMissing {
                    seq: self.replay_frontier,
                })?;
        hasher.update(&previous);
        hasher.update(&commit_seq.0.to_le_bytes());
        hasher.update(&logical_command_seq.0.to_le_bytes());
        let canonical = template
            .canonical_bytes()
            .map_err(ApplyError::TemplateNotCanonical)?;
        hasher.update(&canonical);
        candidate
            .prefix_digests
            .insert(commit_seq, hasher.finalize().0);
        for entry in template.coordinate_entries() {
            let key = (entry.graph, entry.branch);
            // The sequence must ADVANCE from the pre-template state for every
            // coordinate this template touches. Read from `self`, not the
            // candidate: one atomic template may carry several relation entries
            // for the same graph/branch, and those entries share one sequence.
            if let Some(applied) = self.applied_through.get(&key).copied()
                && commit_seq.0 <= applied.0
            {
                return Err(ApplyError::SequenceNotAdvancing {
                    graph: entry.graph,
                    branch: entry.branch,
                    applied_through: applied,
                    offered: commit_seq,
                });
            }
            // A coordinate that receives writes without having been forked is
            // Genesis. Recorded on first write rather than inferred later,
            // because "no origin" and "genesis origin" are different claims and
            // only one of them is true here.
            candidate
                .origins
                .entry(key)
                .or_insert(BranchOrigin::Genesis);
            self.preflight_graph_identity_reuse(entry)?;
            candidate
                .coordinates
                .entry(key)
                .or_default()
                .apply_entry(entry)?;
            candidate.applied_through.insert(key, commit_seq);
            // Recorded only on the path that actually applied it, inside the
            // all-or-nothing candidate: a refused template must leave no trace
            // in the stream, or a later historical read would fold in effects
            // that were never committed.
            candidate
                .history
                .entry(key)
                .or_default()
                .push(CommitRecord {
                    commit_seq,
                    logical_command_seq,
                    entry: entry.clone(),
                });
        }
        *self = candidate;
        Ok(())
    }
}

#[cfg(test)]
mod provenance_internal_tests {
    use super::{ApplyError, ReferenceDatabase, SnapshotError};
    use fgdb_delta_types::{CoordinateEntry, LogicalDeltaTemplate, RelationId, SchemaEpoch};
    use fgdb_types::{
        BranchId, CommitSeq, CommitSeqExhausted, GraphId, LogicalCommandSeq, ObjectId,
    };

    fn rowless_template() -> LogicalDeltaTemplate {
        LogicalDeltaTemplate::build(
            ObjectId([0x11; 32]),
            [0x22; 32],
            vec![CoordinateEntry {
                graph: GraphId(1),
                branch: BranchId(1),
                relation: RelationId(1),
                schema_epoch: SchemaEpoch(0),
                schema_transition: None,
                rows: vec![],
            }],
        )
        .expect("template builds")
    }

    #[test]
    fn exhaustion_accepts_max_once_then_permanently_refuses_without_mutation() {
        let penultimate = CommitSeq(u64::MAX - 1);
        let maximum = CommitSeq(u64::MAX);
        let mut db = ReferenceDatabase::new();
        // Seed the persisted stream boundary directly: iterating a real oracle
        // through 2^64 commits is impossible in a unit test, while the prefix
        // digest is the exact predecessor evidence `apply_template` consumes.
        db.replay_frontier = penultimate;
        db.prefix_digests.insert(penultimate, [0xAA; 32]);

        db.apply_template(&rowless_template(), maximum, LogicalCommandSeq(1))
            .expect("the final representable sequence is assignable");
        assert_eq!(db.replay_frontier, maximum);

        let settled = db.clone();
        let exhausted = ApplyError::CommitSeqExhausted(CommitSeqExhausted { frontier: maximum });
        for logical_command_seq in [LogicalCommandSeq(2), LogicalCommandSeq(3)] {
            assert_eq!(
                db.apply_template(&rowless_template(), maximum, logical_command_seq),
                Err(exhausted.clone())
            );
            assert_eq!(db, settled, "an exhausted refusal changes no state");
        }
    }

    #[test]
    fn logical_command_exhaustion_is_permanent_and_named_exactly() {
        let maximum = LogicalCommandSeq(u64::MAX);
        let mut db = ReferenceDatabase::new();
        db.apply_template(&rowless_template(), CommitSeq(1), maximum)
            .expect("the final representable logical position is assignable");

        let settled = db.clone();
        let exhausted = ApplyError::LogicalCommandSeqExhausted { frontier: maximum };
        for offered in [LogicalCommandSeq(0), maximum] {
            assert_eq!(
                db.apply_template(&rowless_template(), CommitSeq(2), offered),
                Err(exhausted.clone())
            );
            assert_eq!(db, settled, "an exhausted refusal changes no state");
        }
    }

    /// The prefix map is private, so only an internal test can construct this
    /// contradiction. The refusal must happen before candidate state is installed.
    #[test]
    fn a_missing_frontier_digest_fails_closed_without_moving_state() {
        let mut db = ReferenceDatabase::new();
        db.replay_frontier = CommitSeq(1);
        let settled = db.clone();

        assert_eq!(
            db.apply_template(&rowless_template(), CommitSeq(2), LogicalCommandSeq(2)),
            Err(ApplyError::PrefixDigestMissing { seq: CommitSeq(1) })
        );
        assert_eq!(db, settled);
    }

    #[test]
    fn a_snapshot_cannot_mint_an_unknown_history_basis() {
        let mut db = ReferenceDatabase::new();
        db.apply_template(&rowless_template(), CommitSeq(1), LogicalCommandSeq(1))
            .expect("initial template applies");
        db.prefix_digests.remove(&CommitSeq(1));

        assert_eq!(
            db.snapshot(GraphId(1), BranchId(1)),
            Err(SnapshotError::PrefixDigestMissing { seq: CommitSeq(1) })
        );
    }

    #[test]
    fn a_snapshot_cannot_mint_without_its_logical_command_position() {
        let mut db = ReferenceDatabase::new();
        db.apply_template(&rowless_template(), CommitSeq(1), LogicalCommandSeq(10))
            .expect("initial template applies");
        db.stream_positions.remove(&CommitSeq(1));

        assert_eq!(
            db.snapshot(GraphId(1), BranchId(1)),
            Err(SnapshotError::LogicalCommandPositionMissing {
                commit_seq: CommitSeq(1),
            })
        );
    }
}
