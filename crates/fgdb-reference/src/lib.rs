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
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, ObjectId, VId};
use std::collections::{BTreeMap, BTreeSet};

/// A materialized vertex.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Vertex {
    pub birth_ordinal: u64,
    pub labels: BTreeSet<LabelId>,
    pub props: BTreeMap<PropertyKeyId, CanonicalScalar>,
    pub valid_time: Option<ValidTimePeriod>,
}

/// A materialized edge.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
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
    /// A row names an element that is not there.
    NoSuchVertex {
        vid: VId,
    },
    NoSuchEdge {
        eid: EId,
    },
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
}

impl core::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::VertexAlreadyExists { vid } => write!(f, "vertex {vid:?} already exists"),
            Self::EdgeAlreadyExists { eid } => write!(f, "edge {eid:?} already exists"),
            Self::NoSuchVertex { vid } => write!(f, "no such vertex {vid:?}"),
            Self::NoSuchEdge { eid } => write!(f, "no such edge {eid:?}"),
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
            Self::OperationKeyReused { key } => {
                write!(f, "operation key {key:?} already names a different effect")
            }
        }
    }
}

impl core::error::Error for ApplyError {}

/// The materialized state of one coordinate (a graph/branch pair).
///
/// Canonical maps throughout, so iteration order is a function of the keys and
/// two runs over the same rows cannot differ.
#[derive(Clone, Debug, PartialEq)]
pub struct ReferenceGraph {
    vertices: BTreeMap<VId, Vertex>,
    edges: BTreeMap<EId, Edge>,
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

    pub fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn schema_epoch(&self) -> SchemaEpoch {
        self.schema_epoch
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

    /// Apply one row, or refuse and leave the state untouched.
    ///
    /// Every mutation is computed and every check passed before anything is
    /// written, so a refusal is total: a caller that retries after fixing the
    /// row sees exactly the state it had.
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
                self.vertices.insert(
                    *vid,
                    Vertex {
                        birth_ordinal: *birth_ordinal,
                        labels: labels.iter().copied().collect(),
                        props: props.iter().cloned().collect(),
                        valid_time: *valid_time,
                    },
                );
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
                self.edges.insert(
                    *eid,
                    Edge {
                        birth_ordinal: *birth_ordinal,
                        src: *src,
                        relation: *relation,
                        dst: *dst,
                        canonical_key: canonical_key.clone(),
                        props: props.iter().cloned().collect(),
                        valid_time: *valid_time,
                    },
                );
            }
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } => {
                if !self.vertices.contains_key(vid) {
                    return Err(ApplyError::NoSuchVertex { vid: *vid });
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
            }
            DeltaRow::DeleteEdge { eid, .. } => {
                if self.edges.remove(eid).is_none() {
                    return Err(ApplyError::NoSuchEdge { eid: *eid });
                }
            }
            DeltaRow::LabelMembership {
                vid,
                label,
                before,
                after,
            } => {
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
            }
            DeltaRow::Property {
                elem,
                property,
                before,
                after,
            } => {
                let props = self.props_mut(*elem)?;
                let actual = props.get(property).cloned();
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
                        props.insert(*property, value.clone());
                    }
                    None => {
                        props.remove(property);
                    }
                }
            }
            DeltaRow::ValidTime {
                elem,
                before,
                after,
                ..
            } => {
                let actual = self.valid_time_of(*elem)?;
                if actual != *before {
                    return Err(ApplyError::ValidTimeBeforeMismatch {
                        elem: *elem,
                        declared: *before,
                        actual,
                    });
                }
                self.set_valid_time(*elem, *after)?;
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
                self.counters.insert((*elem, *property), *after);
                self.operation_keys.insert(*operation_key, row.clone());
            }
            DeltaRow::Escrow {
                domain_id,
                operation_key,
                delta,
                before_value,
                after_value,
                ..
            } => {
                if self.already_applied(operation_key, row)? {
                    return Ok(());
                }
                Self::require_closing(*before_value, *delta, *after_value)?;
                let actual = self.escrow_balance(*domain_id);
                if actual != *before_value {
                    return Err(ApplyError::EscrowBeforeMismatch {
                        domain: *domain_id,
                        declared: *before_value,
                        actual,
                    });
                }
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

    fn props_mut(
        &mut self,
        elem: ElementId,
    ) -> Result<&mut BTreeMap<PropertyKeyId, CanonicalScalar>, ApplyError> {
        match elem {
            ElementId::Vertex(vid) => self
                .vertices
                .get_mut(&vid)
                .map(|v| &mut v.props)
                .ok_or(ApplyError::NoSuchVertex { vid }),
            ElementId::Edge(eid) => self
                .edges
                .get_mut(&eid)
                .map(|e| &mut e.props)
                .ok_or(ApplyError::NoSuchEdge { eid }),
        }
    }

    fn valid_time_of(&self, elem: ElementId) -> Result<Option<ValidTimePeriod>, ApplyError> {
        match elem {
            ElementId::Vertex(vid) => self
                .vertices
                .get(&vid)
                .map(|v| v.valid_time)
                .ok_or(ApplyError::NoSuchVertex { vid }),
            ElementId::Edge(eid) => self
                .edges
                .get(&eid)
                .map(|e| e.valid_time)
                .ok_or(ApplyError::NoSuchEdge { eid }),
        }
    }

    fn set_valid_time(
        &mut self,
        elem: ElementId,
        period: Option<ValidTimePeriod>,
    ) -> Result<(), ApplyError> {
        match elem {
            ElementId::Vertex(vid) => {
                self.vertices
                    .get_mut(&vid)
                    .ok_or(ApplyError::NoSuchVertex { vid })?
                    .valid_time = period;
            }
            ElementId::Edge(eid) => {
                self.edges
                    .get_mut(&eid)
                    .ok_or(ApplyError::NoSuchEdge { eid })?
                    .valid_time = period;
            }
        }
        Ok(())
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
/// `parent_head: StrongMarkerRef`, `fork_boundary_logical_command_seq`, and
/// `boundary_reservation_identity`. None is spellable honestly in this
/// sequence-neutral materializer: it tracks neither committed parent heads nor
/// logical-command history, and the reservation belongs to W4's certification
/// machinery. This slice therefore models only a fork of the parent's current
/// materialized state.
///
/// FORKING FROM AN EARLIER SEQUENCE IS NOW IMPLEMENTABLE and is not yet
/// implemented. The blocker used to be real — there was no history to select a
/// boundary in, so a caller-supplied sequence would have been an unauthenticated
/// label rather than oracle evidence. [`ReferenceDatabase::read`] removes it: a
/// boundary can now be checked against the parent's frontier and *used*, by
/// materializing the parent at that sequence. That is a capability this type
/// should grow, and until it does, the restriction is a missing feature rather
/// than an impossibility — recorded here so the distinction does not decay back
/// into "cannot".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchOrigin {
    Genesis,
    Fork {
        parent_branch: BranchId,
        /// The parent's `applied_through` at the moment of the fork.
        ///
        /// DERIVED, never supplied. An earlier version of this type took a
        /// boundary from the caller and stored it, which was a counterfeit: the
        /// materializer could not select or verify a historical boundary, so the
        /// value constrained nothing while the signature advertised historical
        /// forking (fgdb-vyb0). This is the honest version — it records the
        /// sequence the parent had actually applied, which is a fact the
        /// database owns and can check.
        ///
        /// It is LOAD-BEARING as well as recorded: [`ReferenceDatabase::read`]
        /// caps each ancestor's contribution at its boundary, so this value is
        /// what keeps a parent's post-fork commits out of a child's historical
        /// reads. Deleting it would not merely drop a label — it would make every
        /// cross-fork read wrong.
        ///
        /// It is the parent's frontier for a [`fork_branch`] and an earlier
        /// sequence for a [`fork_branch_at`]; either way it is the sequence the
        /// child's inherited state was materialized at, which is what makes it
        /// checkable. plan:2000 calls this
        /// `fork_boundary_logical_command_seq`; commit sequences are the
        /// analogue this materializer has.
        ///
        /// [`fork_branch`]: ReferenceDatabase::fork_branch
        /// [`fork_branch_at`]: ReferenceDatabase::fork_branch_at
        fork_boundary: CommitSeq,
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
    /// The requested boundary is above what the parent has applied — a fork from
    /// the parent's future.
    ///
    /// Refused rather than clamped to the frontier. Clamping would make a fork
    /// silently mean something other than what it said, and the difference is
    /// invisible afterwards: the child's recorded boundary would be the frontier
    /// and nothing would show that a different one was asked for.
    BoundaryBeyondParentFrontier {
        graph: GraphId,
        parent: BranchId,
        applied_through: CommitSeq,
        requested: CommitSeq,
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
        boundary: CommitSeq,
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
            Self::BoundaryBeyondParentFrontier {
                graph,
                parent,
                applied_through,
                requested,
            } => write!(
                f,
                "parent {parent:?} in graph {graph:?} has applied through \
                 {applied_through:?}, cannot fork at {requested:?}"
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

/// The materialized database: one [`ReferenceGraph`] per `(graph, branch)`.
///
/// Keyed by coordinate because a template may carry entries for several, and
/// applying them to one shared map would silently merge two branches — the
/// error a single-coordinate materializer cannot even represent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReferenceDatabase {
    coordinates: BTreeMap<(GraphId, BranchId), ReferenceGraph>,
    /// How each branch came to exist. Separate from the state map because a
    /// branch's origin is metadata about history, not part of the graph.
    origins: BTreeMap<(GraphId, BranchId), BranchOrigin>,
    /// The highest commit sequence each coordinate has applied.
    applied_through: BTreeMap<(GraphId, BranchId), CommitSeq>,
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
/// collide when they name the same key. [`EndpointExistence`](Self::EndpointExistence)
/// is different because its shared/exclusive access mode cannot be represented
/// by set intersection; it is produced only by the constraint-certification
/// summary below. That makes omissions the interesting part, so both collectors
/// have no wildcard arm — a new row family is a compile error rather than a
/// silent hole that lets two conflicting transactions both commit.
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

/// The commit-time domains that cannot be represented by one ordinary write set.
///
/// Creating an edge reads the continued existence of both endpoints (shared);
/// deleting a vertex invalidates that fact (exclusive). Shared/shared is legal,
/// while either shared/exclusive ordering is a conflict. Keeping the modes in
/// separate sets is the part that prevents hub vertices from becoming
/// serialization points.
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
/// it into an exclusive write. A conflict rule that silently omits a family does
/// not report a missing conflict — it reports "no conflict", and both
/// transactions commit.
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

/// One ancestor of a lineage walk, with the highest sequence it may contribute
/// through — its own fork boundary, or the requested `high` if that is lower.
type CappedAncestor = (Coordinate, CommitSeq);

/// One coordinate's contribution at one commit sequence.
///
/// The whole entry is retained rather than a summary of it: replay must be the
/// same operation as the original apply, or the fold is a re-implementation of
/// the materializer and can drift from it.
#[derive(Clone, Debug, PartialEq)]
struct CommitRecord {
    seq: CommitSeq,
    entry: CoordinateEntry,
}

/// A read capability pinned to one coordinate at one commit sequence.
///
/// **THE SI ORACLE'S INVARIANT IS STRUCTURAL HERE, NOT CHECKED.** §15 asks for
/// an oracle asserting that no read sees a sequence above the snapshot. A
/// `Snapshot` is minted only by [`ReferenceDatabase::snapshot`] or
/// [`ReferenceDatabase::snapshot_at`], both of which refuse a `high` above the
/// coordinate's frontier, and [`ReferenceDatabase::read`] folds only records at
/// or below it. There is no constructor and no mutator, so "a read above the
/// snapshot" is not a condition to detect — it is a state the type cannot
/// reach. The alternative, passing a bare sequence to every read, makes the
/// invariant a discipline that every future call site has to remember.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    graph: GraphId,
    branch: BranchId,
    high: CommitSeq,
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

impl ReferenceDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn graph(&self, graph: GraphId, branch: BranchId) -> Option<&ReferenceGraph> {
        self.coordinates.get(&(graph, branch))
    }

    /// The highest commit sequence this coordinate has applied — its frontier.
    ///
    /// This is the *bound* on visibility, not visibility itself: it is the
    /// largest `high` [`snapshot_at`](Self::snapshot_at) will mint, and the
    /// sequence a fork boundary honestly records. Reading the graph as it stood
    /// at an earlier sequence goes through [`read`](Self::read).
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
        if self
            .history
            .get(&(graph, branch))
            .and_then(|records| records.first())
            .is_some_and(|first| first.seq.0 > since.0)
        {
            summary.writes.insert(ConflictKey::CoordinateExistence);
        }

        for (key, cap) in self.lineage(graph, branch, frontier)? {
            let Some(records) = self.history.get(&key) else {
                continue;
            };
            for record in records {
                if record.seq.0 <= since.0 {
                    continue;
                }
                if record.seq.0 > cap.0 {
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
        self.history
            .get(&(graph, branch))
            .map_or(0, |records| records.len())
    }

    /// Mint a snapshot at this coordinate's current frontier.
    ///
    /// The read-your-own-writes position: everything committed so far, nothing
    /// after.
    pub fn snapshot(&self, graph: GraphId, branch: BranchId) -> Result<Snapshot, SnapshotError> {
        let high = self
            .applied_through(graph, branch)
            .ok_or(SnapshotError::NoSuchCoordinate { graph, branch })?;
        Ok(Snapshot {
            graph,
            branch,
            high,
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
        Ok(Snapshot {
            graph,
            branch,
            high,
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
        let mut graph = ReferenceGraph::new();
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
            return Ok(graph);
        }
        for (key, cap) in self.lineage(snapshot.graph, snapshot.branch, snapshot.high)? {
            let Some(records) = self.history.get(&key) else {
                continue;
            };
            for record in records {
                // Ascending by construction: `apply_template` refuses a
                // sequence that does not advance the coordinate, so a record
                // above the cap means every later one is too.
                if record.seq.0 > cap.0 {
                    break;
                }
                graph.apply_entry(&record.entry).map_err(|cause| {
                    SnapshotError::HistoryNotApplicable {
                        graph: key.0,
                        branch: key.1,
                        seq: record.seq,
                        cause: Box::new(cause),
                    }
                })?;
            }
        }
        Ok(graph)
    }

    /// The ancestor chain from genesis to `branch`, each capped at the sequence
    /// it may contribute through.
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
        let mut chain: Vec<CappedAncestor> = Vec::new();
        let mut key = (graph, branch);
        let mut cap = high;
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
                    if fork_boundary.0 < cap.0 {
                        cap = *fork_boundary;
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
    /// Defined as [`fork_branch_at`](Self::fork_branch_at) at the parent's
    /// frontier, so there is one fork mechanism rather than two that must agree.
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
        let frontier = self
            .applied_through
            .get(&(graph, parent))
            .copied()
            .ok_or(BranchError::NoSuchParent { graph, parent })?;
        self.fork_branch_at(graph, parent, child, frontier)
    }

    /// Fork `child` from `parent` **as the parent stood at `boundary`** — a
    /// branch rooted at a point in history, not only at the present.
    ///
    /// This is what plan:2000's `fork_boundary_logical_command_seq` asks for, and
    /// it was genuinely unlandable until [`read`](Self::read) existed: with no
    /// history to select in, a caller-supplied boundary could only be stored,
    /// which made it an unauthenticated label rather than oracle evidence
    /// (fgdb-vyb0). It is landable now for exactly one reason — the boundary is
    /// *used*: the child's state is the parent's fold at that sequence, and the
    /// boundary is checked against the parent's frontier first. Delete the
    /// parameter and the child inherits a different graph.
    ///
    /// `CommitSeq(0)` is a legal boundary and produces an empty child: a branch
    /// taken from before the parent's first commit. Refusing zero would be
    /// arbitrary — the parent's state at zero is a state the parent genuinely
    /// had.
    pub fn fork_branch_at(
        &mut self,
        graph: GraphId,
        parent: BranchId,
        child: BranchId,
        boundary: CommitSeq,
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
        let frontier = self
            .applied_through
            .get(&(graph, parent))
            .copied()
            .ok_or(BranchError::NoSuchParent { graph, parent })?;
        if boundary.0 > frontier.0 {
            return Err(BranchError::BoundaryBeyondParentFrontier {
                graph,
                parent,
                applied_through: frontier,
                requested: boundary,
            });
        }

        // The child's state is the parent's HISTORY folded to the boundary, not
        // a copy of the parent's present. At the frontier the two are equal —
        // that is the faithfulness law — so routing the current-state fork
        // through here costs nothing and removes the second implementation that
        // could drift from this one.
        let snapshot = self.snapshot_at(graph, parent, boundary).map_err(|cause| {
            BranchError::BoundaryNotMaterializable {
                graph,
                parent,
                boundary,
                cause: Box::new(cause),
            }
        })?;
        let inherited =
            self.read(&snapshot)
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
        self.applied_through.insert((graph, child), boundary);
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

    /// Apply a whole template — every coordinate entry it carries.
    ///
    /// ALL OR NOTHING. The template is validated against a *clone* and only
    /// swapped in once every entry applied, so a template that is applicable at
    /// entry 1 and not at entry 3 leaves the database exactly as it was. A
    /// partially-applied commit would put the database in a state no commit
    /// stream describes, which is the one outcome an oracle must never produce.
    pub fn apply_template(
        &mut self,
        template: &LogicalDeltaTemplate,
        commit_seq: CommitSeq,
    ) -> Result<(), ApplyError> {
        let mut candidate = self.clone();
        for entry in template.coordinate_entries() {
            let key = (entry.graph, entry.branch);
            // The sequence must ADVANCE for every coordinate this template
            // touches. Checked inside the all-or-nothing candidate, so a
            // template that advances one coordinate and not another applies to
            // neither.
            if let Some(applied) = candidate.applied_through.get(&key).copied()
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
                    seq: commit_seq,
                    entry: entry.clone(),
                });
        }
        *self = candidate;
        Ok(())
    }
}
