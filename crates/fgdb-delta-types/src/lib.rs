//! The G0/W2 logical delta **schema** (Appendix B, ~plan line 2807).
//!
//! Scope discipline: this crate is the *type* layer only. The differential
//! machinery — deriving templates from `NetEffectNormalForm`, applying
//! batches, indexes, frontier streams — is bead `fgdb-w2-delta-batches`.
//! What lives here is the normative shape:
//!
//! - the nine exact typed delta-row families (create / delete / property /
//!   valid-time / counter / escrow / sketch / schema / constraint),
//! - the **sequence-neutral** [`LogicalDeltaTemplate`], keyed by
//!   graph/branch/relation/schema/`IntentSemanticsOid` and carrying no
//!   commit order anywhere in its type,
//! - the law that **only the committed `MarkerRef` turns the template into
//!   an ordered [`LogicalDeltaBatch`]** — expressed as a type distinction:
//!   the only constructor of a batch consumes a [`CommittedMarker`], and a
//!   bare [`MarkerRef`](fgdb_types::MarkerRef) (an identity, per the
//!   Appendix A reference law) is not accepted.
//!
//! Field notes: identifier widths for labels/property keys/relations are
//! catalog-interned ordinals whose durable pinning belongs to
//! `fgdb-w4-schema-catalog`; valid-time *semantics* (contracts, selectors)
//! belong to `fgdb-w4-valid-time` — the rows here carry the assignments as
//! opaque-but-ordered period bounds. Neither crate exists yet; these types
//! are the subset both will consume, never a substitute for them.

#![forbid(unsafe_code)]

mod zweight;

use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, MarkerRef, ObjectId, VId};

pub use fgdb_bigint::{ArithmeticOperation as ZWeightOperation, LimbLimit};
pub use zweight::{ZWeight, ZWeightError};

/// Catalog-interned label ordinal (durable pinning: `fgdb-w4-schema-catalog`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LabelId(pub u64);

/// Catalog-interned property-key ordinal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PropertyKeyId(pub u64);

/// Catalog-interned relation (edge-type) ordinal.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RelationId(pub u64);

/// Schema epoch under which a template's rows were produced.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SchemaEpoch(pub u64);

/// Registered unique operation key (ACI merge dedupes on these before
/// checked summation — Appendix B "set-union of unique operation keys").
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct OperationKey(pub [u8; 32]);

/// Escrow domain identity (§4.7 custody ledger).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EscrowDomainId(pub u128);

/// A graph element: vertex or edge.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ElementId {
    Vertex(VId),
    Edge(EId),
}

/// Valid-time assignment bounds as they appear in delta rows. Microseconds
/// since the profile epoch; `end: None` is an open period. Semantics
/// (contracts, overlap laws, selectors) are `fgdb-w4-valid-time`'s.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ValidTimePeriod {
    pub start_micros: i64,
    pub end_micros: Option<i64>,
}

/// The nine exact typed delta-row families. Every arm carries explicit
/// before/after images where the normal form requires them; nothing here is
/// conditional and nothing carries commit order (that is the batch's job).
#[derive(Clone, PartialEq, Debug)]
pub enum DeltaRow {
    /// Create-family: a vertex with its permanent identity and birth ordinal.
    CreateVertex {
        vid: VId,
        birth_ordinal: u64,
        labels: Vec<LabelId>,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
        valid_time: Option<ValidTimePeriod>,
    },
    /// Create-family: an edge with endpoints, relation, optional canonical key.
    CreateEdge {
        eid: EId,
        birth_ordinal: u64,
        src: VId,
        relation: RelationId,
        dst: VId,
        canonical_key: Option<CanonicalScalar>,
        props: Vec<(PropertyKeyId, CanonicalScalar)>,
        valid_time: Option<ValidTimePeriod>,
    },
    /// Delete-family: vertex retirement with its complete cascade
    /// before-image (sorted retired incident edges).
    DeleteVertex {
        vid: VId,
        before_version: ObjectId,
        sorted_retired_incident_edges: Vec<EId>,
    },
    /// Delete-family: edge retirement.
    DeleteEdge { eid: EId, before_version: ObjectId },
    /// Property-family: label membership flip with before/after.
    LabelMembership {
        vid: VId,
        label: LabelId,
        before: bool,
        after: bool,
    },
    /// Property-family: property transition with full before/after images
    /// (`None` = absent).
    Property {
        elem: ElementId,
        property: PropertyKeyId,
        before: Option<CanonicalScalar>,
        after: Option<CanonicalScalar>,
    },
    /// Valid-time-family: period transition under a named contract.
    ValidTime {
        elem: ElementId,
        contract_id: ObjectId,
        before: Option<ValidTimePeriod>,
        after: Option<ValidTimePeriod>,
    },
    /// Counter-family: checked adjustment identified by a registered algebra
    /// profile and deduped by operation key. Overflow policy is always
    /// Reject (Appendix B) — there is no saturating arm to encode.
    Counter {
        operation_key: OperationKey,
        elem: ElementId,
        property: PropertyKeyId,
        algebra_profile: ObjectId,
        delta: i128,
        before: i128,
        after: i128,
    },
    /// Escrow-family: custody-ledger adjustment (§4.7).
    Escrow {
        domain_id: EscrowDomainId,
        epoch: u64,
        operation_key: OperationKey,
        subject: ElementId,
        subject_property: Option<PropertyKeyId>,
        delta: i128,
        before_value: i128,
        after_value: i128,
    },
    /// Sketch-family: profile-governed state transition; the before image is
    /// a digest and the after image a content address.
    Sketch {
        operation_key: OperationKey,
        sketch_profile_oid: ObjectId,
        before_state_digest: [u8; 32],
        after_state_oid: ObjectId,
    },
    /// Schema-family: a coordinate schema transition, referenced by its
    /// canonical transition object.
    Schema {
        transition_oid: ObjectId,
        before_epoch: SchemaEpoch,
        after_epoch: SchemaEpoch,
    },
    /// Constraint-family: constraint-state root transition with explicit
    /// before/after roots on both the schema and constraint axes.
    Constraint {
        before_schema_root: ObjectId,
        after_schema_root: ObjectId,
        before_constraint_root: ObjectId,
        after_constraint_root: ObjectId,
    },
}

impl DeltaRow {
    /// The named family (bead vocabulary) this row belongs to.
    pub fn family(&self) -> DeltaFamily {
        match self {
            DeltaRow::CreateVertex { .. } | DeltaRow::CreateEdge { .. } => DeltaFamily::Create,
            DeltaRow::DeleteVertex { .. } | DeltaRow::DeleteEdge { .. } => DeltaFamily::Delete,
            DeltaRow::LabelMembership { .. } | DeltaRow::Property { .. } => DeltaFamily::Property,
            DeltaRow::ValidTime { .. } => DeltaFamily::ValidTime,
            DeltaRow::Counter { .. } => DeltaFamily::Counter,
            DeltaRow::Escrow { .. } => DeltaFamily::Escrow,
            DeltaRow::Sketch { .. } => DeltaFamily::Sketch,
            DeltaRow::Schema { .. } => DeltaFamily::Schema,
            DeltaRow::Constraint { .. } => DeltaFamily::Constraint,
        }
    }
}

/// The nine families named by the plan ("Create/delete/property/valid-time/
/// counter/escrow/sketch/schema/constraint deltas are exact typed rows").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DeltaFamily {
    Create,
    Delete,
    Property,
    ValidTime,
    Counter,
    Escrow,
    Sketch,
    Schema,
    Constraint,
}

/// The template key: graph / branch / relation / schema epoch /
/// `IntentSemanticsOid`. Everything a template's rows mean is pinned by this
/// key; nothing about *when* is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DeltaTemplateKey {
    pub graph: GraphId,
    pub branch: BranchId,
    pub relation: RelationId,
    pub schema_epoch: SchemaEpoch,
    pub intent_semantics_oid: ObjectId,
}

/// Sequence-neutral logical delta template. By construction this type has no
/// commit sequence, no marker, and no ordering field of any kind — it cannot
/// express "when", only "what".
#[derive(Clone, PartialEq, Debug)]
pub struct LogicalDeltaTemplate {
    key: DeltaTemplateKey,
    rows: Vec<DeltaRow>,
}

impl LogicalDeltaTemplate {
    pub fn new(key: DeltaTemplateKey, rows: Vec<DeltaRow>) -> Self {
        LogicalDeltaTemplate { key, rows }
    }

    pub fn key(&self) -> &DeltaTemplateKey {
        &self.key
    }

    pub fn rows(&self) -> &[DeltaRow] {
        &self.rows
    }
}

/// A marker attested as **committed** (both fsyncs of the two-fsync protocol
/// complete). This is a distinct type from the bare identity
/// [`MarkerRef`](fgdb_types::MarkerRef) so the delta layer can demand
/// committedness in signatures. Production construction sites live in the
/// W2 commit pipeline (`fgdb-w2-commit-protocol`), after the marker fsync;
/// once `Cx` capabilities land, attestation will additionally require the
/// commit capability, making misuse unrepresentable rather than reviewable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CommittedMarker(MarkerRef);

impl CommittedMarker {
    /// Attests that `marker` is durably committed. See the type docs for who
    /// may call this.
    pub const fn attest(marker: MarkerRef) -> Self {
        CommittedMarker(marker)
    }

    pub const fn marker(&self) -> MarkerRef {
        self.0
    }
}

/// An **ordered** logical delta batch: a template joined to the committed
/// marker that gives it its place in history. The only constructor consumes
/// a [`CommittedMarker`]; there is deliberately no way to build one from a
/// bare `MarkerRef`.
#[derive(Clone, PartialEq, Debug)]
pub struct LogicalDeltaBatch {
    template: LogicalDeltaTemplate,
    marker: CommittedMarker,
}

impl LogicalDeltaBatch {
    /// The one door: order a sequence-neutral template by its committed
    /// marker.
    pub fn order(template: LogicalDeltaTemplate, marker: CommittedMarker) -> Self {
        LogicalDeltaBatch { template, marker }
    }

    pub fn template(&self) -> &LogicalDeltaTemplate {
        &self.template
    }

    pub fn marker(&self) -> CommittedMarker {
        self.marker
    }

    /// The batch's position in history (from its committed marker).
    pub fn commit_seq(&self) -> fgdb_types::CommitSeq {
        self.marker.0.commit_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::CommitSeq;

    fn key() -> DeltaTemplateKey {
        DeltaTemplateKey {
            graph: GraphId(1),
            branch: BranchId(1),
            relation: RelationId(3),
            schema_epoch: SchemaEpoch(2),
            intent_semantics_oid: ObjectId([0x11; 32]),
        }
    }

    fn one_row_of_each_family() -> Vec<DeltaRow> {
        vec![
            DeltaRow::CreateVertex {
                vid: VId(1),
                birth_ordinal: 1,
                labels: vec![LabelId(1)],
                props: vec![(PropertyKeyId(1), CanonicalScalar::Int(1815))],
                valid_time: None,
            },
            DeltaRow::CreateEdge {
                eid: EId(1),
                birth_ordinal: 2,
                src: VId(1),
                relation: RelationId(3),
                dst: VId(2),
                canonical_key: None,
                props: vec![],
                valid_time: Some(ValidTimePeriod {
                    start_micros: 0,
                    end_micros: None,
                }),
            },
            DeltaRow::DeleteVertex {
                vid: VId(9),
                before_version: ObjectId([2; 32]),
                sorted_retired_incident_edges: vec![EId(4), EId(7)],
            },
            DeltaRow::DeleteEdge {
                eid: EId(4),
                before_version: ObjectId([3; 32]),
            },
            DeltaRow::LabelMembership {
                vid: VId(1),
                label: LabelId(2),
                before: false,
                after: true,
            },
            DeltaRow::Property {
                elem: ElementId::Vertex(VId(1)),
                property: PropertyKeyId(2),
                before: None,
                after: Some(
                    CanonicalScalar::ucs_basic_text("Ada")
                        .expect("small UCS_BASIC fixture is canonical"),
                ),
            },
            DeltaRow::ValidTime {
                elem: ElementId::Edge(EId(1)),
                contract_id: ObjectId([4; 32]),
                before: None,
                after: Some(ValidTimePeriod {
                    start_micros: 10,
                    end_micros: Some(20),
                }),
            },
            DeltaRow::Counter {
                operation_key: OperationKey([5; 32]),
                elem: ElementId::Vertex(VId(1)),
                property: PropertyKeyId(3),
                algebra_profile: ObjectId([6; 32]),
                delta: 5,
                before: 10,
                after: 15,
            },
            DeltaRow::Escrow {
                domain_id: EscrowDomainId(1),
                epoch: 1,
                operation_key: OperationKey([7; 32]),
                subject: ElementId::Vertex(VId(1)),
                subject_property: None,
                delta: -3,
                before_value: 10,
                after_value: 7,
            },
            DeltaRow::Sketch {
                operation_key: OperationKey([8; 32]),
                sketch_profile_oid: ObjectId([9; 32]),
                before_state_digest: [0; 32],
                after_state_oid: ObjectId([10; 32]),
            },
            DeltaRow::Schema {
                transition_oid: ObjectId([11; 32]),
                before_epoch: SchemaEpoch(2),
                after_epoch: SchemaEpoch(3),
            },
            DeltaRow::Constraint {
                before_schema_root: ObjectId([12; 32]),
                after_schema_root: ObjectId([13; 32]),
                before_constraint_root: ObjectId([14; 32]),
                after_constraint_root: ObjectId([15; 32]),
            },
        ]
    }

    #[test]
    fn every_arm_maps_into_the_nine_families() {
        let rows = one_row_of_each_family();
        let mut seen: Vec<DeltaFamily> = rows.iter().map(|r| r.family()).collect();
        seen.dedup();
        assert_eq!(
            seen,
            [
                DeltaFamily::Create,
                DeltaFamily::Delete,
                DeltaFamily::Property,
                DeltaFamily::ValidTime,
                DeltaFamily::Counter,
                DeltaFamily::Escrow,
                DeltaFamily::Sketch,
                DeltaFamily::Schema,
                DeltaFamily::Constraint,
            ],
            "the twelve arms cover exactly the nine plan families, in order"
        );
    }

    #[test]
    fn template_is_sequence_neutral_and_batch_is_ordered() {
        let template = LogicalDeltaTemplate::new(key(), one_row_of_each_family());
        // The template type carries no order: its full public surface is the
        // key and rows. (The absence of any marker/seq accessor is the
        // compile-time half of this test.)
        assert_eq!(template.key(), &key());
        assert_eq!(template.rows().len(), 12);

        let marker = MarkerRef {
            marker_oid: ObjectId([0xAA; 32]),
            commit_seq: CommitSeq(41999),
        };
        let batch = LogicalDeltaBatch::order(template.clone(), CommittedMarker::attest(marker));
        assert_eq!(batch.commit_seq(), CommitSeq(41999));
        assert_eq!(batch.template(), &template);
        assert_eq!(batch.marker().marker(), marker);
    }

    #[test]
    fn identical_templates_under_different_markers_are_different_batches() {
        let t = LogicalDeltaTemplate::new(key(), one_row_of_each_family());
        let m1 = CommittedMarker::attest(MarkerRef {
            marker_oid: ObjectId([1; 32]),
            commit_seq: CommitSeq(1),
        });
        let m2 = CommittedMarker::attest(MarkerRef {
            marker_oid: ObjectId([2; 32]),
            commit_seq: CommitSeq(2),
        });
        let b1 = LogicalDeltaBatch::order(t.clone(), m1);
        let b2 = LogicalDeltaBatch::order(t, m2);
        assert_ne!(b1, b2, "order comes from the marker, not the rows");
        assert_eq!(b1.template(), b2.template());
    }
}
