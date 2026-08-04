//! Atomic durable-resource accounting algebra.
//!
//! This module is the in-memory semantic contract that registry-generated
//! durable records wrap. It deliberately does not define durable byte tags or
//! object identities: those remain owned by the generated-format and identity
//! layers. What it does define is the part that must not be left to each
//! caller: checked six-axis charges, canonical quota paths, ancestor
//! aggregation, whole-entry ownership transitions, exact retry idempotence,
//! and fail-closed expiry.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use fgdb_types::{BranchId, DatabaseId, DatabaseSecurityNamespaceId, GraphId, ObjectId};

/// The fixed axes of a semantic durable charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DurableChargeAxis {
    CanonicalDurableBytes,
    RetainedHistoryBytes,
    BranchCount,
    IndexCount,
    ViewCount,
    SubscriptionCount,
}

impl DurableChargeAxis {
    pub const ALL: [Self; 6] = [
        Self::CanonicalDurableBytes,
        Self::RetainedHistoryBytes,
        Self::BranchCount,
        Self::IndexCount,
        Self::ViewCount,
        Self::SubscriptionCount,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::CanonicalDurableBytes => "canonical_durable_bytes",
            Self::RetainedHistoryBytes => "retained_history_bytes",
            Self::BranchCount => "branch_count",
            Self::IndexCount => "index_count",
            Self::ViewCount => "view_count",
            Self::SubscriptionCount => "subscription_count",
        }
    }
}

/// Fixed, nonnegative semantic tenant charge.
///
/// Replica, erasure-code, cache, and placement bytes are intentionally absent:
/// they are operational capacity, not semantic ownership.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurableChargeVector {
    pub canonical_durable_bytes: u64,
    pub retained_history_bytes: u64,
    pub branch_count: u64,
    pub index_count: u64,
    pub view_count: u64,
    pub subscription_count: u64,
}

impl DurableChargeVector {
    pub const ZERO: Self = Self {
        canonical_durable_bytes: 0,
        retained_history_bytes: 0,
        branch_count: 0,
        index_count: 0,
        view_count: 0,
        subscription_count: 0,
    };

    pub const fn axis(self, axis: DurableChargeAxis) -> u64 {
        match axis {
            DurableChargeAxis::CanonicalDurableBytes => self.canonical_durable_bytes,
            DurableChargeAxis::RetainedHistoryBytes => self.retained_history_bytes,
            DurableChargeAxis::BranchCount => self.branch_count,
            DurableChargeAxis::IndexCount => self.index_count,
            DurableChargeAxis::ViewCount => self.view_count,
            DurableChargeAxis::SubscriptionCount => self.subscription_count,
        }
    }

    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    pub fn fits_within(self, ceiling: Self) -> bool {
        DurableChargeAxis::ALL
            .into_iter()
            .all(|axis| self.axis(axis) <= ceiling.axis(axis))
    }

    pub fn checked_add(self, other: Self) -> Result<Self, DurableVectorError> {
        Ok(Self {
            canonical_durable_bytes: add_axis(
                DurableChargeAxis::CanonicalDurableBytes,
                self.canonical_durable_bytes,
                other.canonical_durable_bytes,
            )?,
            retained_history_bytes: add_axis(
                DurableChargeAxis::RetainedHistoryBytes,
                self.retained_history_bytes,
                other.retained_history_bytes,
            )?,
            branch_count: add_axis(
                DurableChargeAxis::BranchCount,
                self.branch_count,
                other.branch_count,
            )?,
            index_count: add_axis(
                DurableChargeAxis::IndexCount,
                self.index_count,
                other.index_count,
            )?,
            view_count: add_axis(
                DurableChargeAxis::ViewCount,
                self.view_count,
                other.view_count,
            )?,
            subscription_count: add_axis(
                DurableChargeAxis::SubscriptionCount,
                self.subscription_count,
                other.subscription_count,
            )?,
        })
    }

    pub fn checked_sub(self, other: Self) -> Result<Self, DurableVectorError> {
        Ok(Self {
            canonical_durable_bytes: sub_axis(
                DurableChargeAxis::CanonicalDurableBytes,
                self.canonical_durable_bytes,
                other.canonical_durable_bytes,
            )?,
            retained_history_bytes: sub_axis(
                DurableChargeAxis::RetainedHistoryBytes,
                self.retained_history_bytes,
                other.retained_history_bytes,
            )?,
            branch_count: sub_axis(
                DurableChargeAxis::BranchCount,
                self.branch_count,
                other.branch_count,
            )?,
            index_count: sub_axis(
                DurableChargeAxis::IndexCount,
                self.index_count,
                other.index_count,
            )?,
            view_count: sub_axis(
                DurableChargeAxis::ViewCount,
                self.view_count,
                other.view_count,
            )?,
            subscription_count: sub_axis(
                DurableChargeAxis::SubscriptionCount,
                self.subscription_count,
                other.subscription_count,
            )?,
        })
    }
}

fn add_axis(axis: DurableChargeAxis, lhs: u64, rhs: u64) -> Result<u64, DurableVectorError> {
    lhs.checked_add(rhs)
        .ok_or(DurableVectorError::Overflow { axis, lhs, rhs })
}

fn sub_axis(axis: DurableChargeAxis, held: u64, released: u64) -> Result<u64, DurableVectorError> {
    held.checked_sub(released)
        .ok_or(DurableVectorError::Underflow {
            axis,
            held,
            released,
        })
}

/// Checked-vector failure with the exact failing axis and operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableVectorError {
    Overflow {
        axis: DurableChargeAxis,
        lhs: u64,
        rhs: u64,
    },
    Underflow {
        axis: DurableChargeAxis,
        held: u64,
        released: u64,
    },
}

impl fmt::Display for DurableVectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Overflow { axis, lhs, rhs } => {
                write!(formatter, "{} overflow: {lhs} + {rhs}", axis.name())
            }
            Self::Underflow {
                axis,
                held,
                released,
            } => write!(
                formatter,
                "{} underflow: held {held}, released {released}",
                axis.name()
            ),
        }
    }
}

impl std::error::Error for DurableVectorError {}

/// Only these roles may author canonical durable accounting state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceAccountingRole {
    Local,
    Meta,
}

/// Capacity lane used by a reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceClass {
    Ordinary,
    RegisteredMaintenance,
}

/// One level in the canonical quota hierarchy.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QuotaSegment {
    Database(DatabaseId),
    Tenant(u128),
    Graph(GraphId),
    Branch(BranchId),
    Feature(u128),
}

impl QuotaSegment {
    const fn rank(&self) -> u8 {
        match self {
            Self::Database(_) => 0,
            Self::Tenant(_) => 1,
            Self::Graph(_) => 2,
            Self::Branch(_) => 3,
            Self::Feature(_) => 4,
        }
    }
}

/// Canonical nonempty root-to-leaf quota path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuotaPath(Box<[QuotaSegment]>);

impl QuotaPath {
    pub const MAX_SEGMENTS: usize = 5;

    pub fn try_new(segments: Vec<QuotaSegment>) -> Result<Self, QuotaPathError> {
        if segments.is_empty() {
            return Err(QuotaPathError::Empty);
        }
        if segments.len() > Self::MAX_SEGMENTS {
            return Err(QuotaPathError::TooDeep {
                depth: segments.len(),
                maximum: Self::MAX_SEGMENTS,
            });
        }
        if segments[0].rank() != 0 {
            return Err(QuotaPathError::MissingDatabaseRoot {
                first_rank: segments[0].rank(),
            });
        }
        for (index, pair) in segments.windows(2).enumerate() {
            let expected = pair[0].rank() + 1;
            let actual = pair[1].rank();
            if actual != expected {
                return Err(QuotaPathError::NonCanonicalLevel {
                    index: index + 1,
                    expected_rank: expected,
                    actual_rank: actual,
                });
            }
        }
        Ok(Self(segments.into_boxed_slice()))
    }

    pub fn segments(&self) -> &[QuotaSegment] {
        &self.0
    }

    pub fn depth(&self) -> usize {
        self.0.len()
    }

    fn ancestors_inclusive(&self) -> Vec<Self> {
        (1..=self.0.len())
            .map(|length| Self(self.0[..length].to_vec().into_boxed_slice()))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaPathError {
    Empty,
    TooDeep {
        depth: usize,
        maximum: usize,
    },
    MissingDatabaseRoot {
        first_rank: u8,
    },
    NonCanonicalLevel {
        index: usize,
        expected_rank: u8,
        actual_rank: u8,
    },
}

impl fmt::Display for QuotaPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid quota path: {self:?}")
    }
}

impl std::error::Error for QuotaPathError {}

/// One quota bucket's exact accounting state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketState {
    hard_limit: DurableChargeVector,
    protected_maintenance_reserve: DurableChargeVector,
    ordinary_reserved: DurableChargeVector,
    maintenance_reserved: DurableChargeVector,
    committed: DurableChargeVector,
}

impl BucketState {
    pub fn try_new(
        hard_limit: DurableChargeVector,
        protected_maintenance_reserve: DurableChargeVector,
        ordinary_reserved: DurableChargeVector,
        maintenance_reserved: DurableChargeVector,
        committed: DurableChargeVector,
    ) -> Result<Self, BucketStateError> {
        let state = Self {
            hard_limit,
            protected_maintenance_reserve,
            ordinary_reserved,
            maintenance_reserved,
            committed,
        };
        state.validate()?;
        Ok(state)
    }

    pub fn empty(
        hard_limit: DurableChargeVector,
        protected_maintenance_reserve: DurableChargeVector,
    ) -> Result<Self, BucketStateError> {
        Self::try_new(
            hard_limit,
            protected_maintenance_reserve,
            DurableChargeVector::ZERO,
            DurableChargeVector::ZERO,
            DurableChargeVector::ZERO,
        )
    }

    pub const fn hard_limit(self) -> DurableChargeVector {
        self.hard_limit
    }

    pub const fn protected_maintenance_reserve(self) -> DurableChargeVector {
        self.protected_maintenance_reserve
    }

    pub const fn ordinary_reserved(self) -> DurableChargeVector {
        self.ordinary_reserved
    }

    pub const fn maintenance_reserved(self) -> DurableChargeVector {
        self.maintenance_reserved
    }

    pub const fn committed(self) -> DurableChargeVector {
        self.committed
    }

    fn accounting_is_empty(self) -> bool {
        self.ordinary_reserved.is_zero()
            && self.maintenance_reserved.is_zero()
            && self.committed.is_zero()
    }

    fn validate(self) -> Result<(), BucketStateError> {
        for axis in DurableChargeAxis::ALL {
            let hard = self.hard_limit.axis(axis);
            let reserve = self.protected_maintenance_reserve.axis(axis);
            if reserve > hard {
                return Err(BucketStateError::ReserveExceedsHardLimit {
                    axis,
                    reserve,
                    hard_limit: hard,
                });
            }
            let ordinary_limit = hard - reserve;
            let committed = self.committed.axis(axis);
            let ordinary_reserved = self.ordinary_reserved.axis(axis);
            let ordinary_use = committed.checked_add(ordinary_reserved).ok_or(
                BucketStateError::UsageOverflow {
                    axis,
                    first: committed,
                    second: ordinary_reserved,
                },
            )?;
            if ordinary_use > ordinary_limit {
                return Err(BucketStateError::OrdinaryCapacityExceeded {
                    axis,
                    usage: ordinary_use,
                    limit: ordinary_limit,
                });
            }
            let maintenance_reserved = self.maintenance_reserved.axis(axis);
            if maintenance_reserved > reserve {
                return Err(BucketStateError::MaintenanceReserveExceeded {
                    axis,
                    usage: maintenance_reserved,
                    reserve,
                });
            }
            let total = ordinary_use.checked_add(maintenance_reserved).ok_or(
                BucketStateError::UsageOverflow {
                    axis,
                    first: ordinary_use,
                    second: maintenance_reserved,
                },
            )?;
            if total > hard {
                return Err(BucketStateError::HardLimitExceeded {
                    axis,
                    usage: total,
                    hard_limit: hard,
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketStateError {
    ReserveExceedsHardLimit {
        axis: DurableChargeAxis,
        reserve: u64,
        hard_limit: u64,
    },
    UsageOverflow {
        axis: DurableChargeAxis,
        first: u64,
        second: u64,
    },
    OrdinaryCapacityExceeded {
        axis: DurableChargeAxis,
        usage: u64,
        limit: u64,
    },
    MaintenanceReserveExceeded {
        axis: DurableChargeAxis,
        usage: u64,
        reserve: u64,
    },
    HardLimitExceeded {
        axis: DurableChargeAxis,
        usage: u64,
        hard_limit: u64,
    },
}

impl fmt::Display for BucketStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid resource bucket: {self:?}")
    }
}

impl std::error::Error for BucketStateError {}

macro_rules! byte_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; 32]);
    };
}

byte_id!(TransitionId);
byte_id!(ReservationId);
byte_id!(ChargeId);
byte_id!(StableSubjectKey);

/// Stable logical owner; never a future content address.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceOwnerKey {
    Attempt {
        posture: u16,
        registration_identity: [u8; 32],
    },
    Statement {
        posture: u16,
        registration_identity: [u8; 32],
        statement_seq: u64,
    },
    ResultDelivery {
        posture: u16,
        result_delivery_id: [u8; 32],
    },
    Subscription {
        cursor_id: [u8; 32],
    },
    DurableObject {
        object_class: u16,
        stable_logical_owner_key: [u8; 32],
    },
    Replay {
        replay_manifest_key: [u8; 32],
    },
    BackupPin {
        backup_id: [u8; 32],
        scope: [u8; 32],
    },
    PreparedClosure {
        prepared_ownership_id: [u8; 32],
    },
    Maintenance {
        job_id: [u8; 32],
        registered_class: u16,
    },
}

/// A hold that makes timeout-based expiry structurally illegal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerHold {
    None,
    PreparedOwnership,
    FinalCertification,
    ResultDelivery,
    Backup,
    RemoteGrant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservationEntry {
    id: ReservationId,
    generation: u64,
    owner: ResourceOwnerKey,
    path: QuotaPath,
    class: ResourceClass,
    vector: DurableChargeVector,
    hold: OwnerHold,
}

impl ReservationEntry {
    pub const fn id(&self) -> ReservationId {
        self.id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn owner(&self) -> &ResourceOwnerKey {
        &self.owner
    }

    pub fn path(&self) -> &QuotaPath {
        &self.path
    }

    pub const fn class(&self) -> ResourceClass {
        self.class
    }

    pub const fn vector(&self) -> DurableChargeVector {
        self.vector
    }

    pub const fn hold(&self) -> OwnerHold {
        self.hold
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargeEntry {
    id: ChargeId,
    generation: u64,
    owner: ResourceOwnerKey,
    path: QuotaPath,
    class: ResourceClass,
    vector: DurableChargeVector,
    stable_subject_key: StableSubjectKey,
}

impl ChargeEntry {
    pub const fn id(&self) -> ChargeId {
        self.id
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn owner(&self) -> &ResourceOwnerKey {
        &self.owner
    }

    pub fn path(&self) -> &QuotaPath {
        &self.path
    }

    pub const fn class(&self) -> ResourceClass {
        self.class
    }

    pub const fn vector(&self) -> DurableChargeVector {
        self.vector
    }

    pub const fn stable_subject_key(&self) -> StableSubjectKey {
        self.stable_subject_key
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLedgerIdentity {
    pub database_security_namespace_id: DatabaseSecurityNamespaceId,
    pub cluster_incarnation: [u8; 16],
    pub role: ResourceAccountingRole,
    pub limit_policy_oid: ObjectId,
    pub limit_policy_epoch: u64,
}

/// The first non-role ledger-identity component that failed to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerIdentityMismatch {
    DatabaseSecurityNamespace {
        expected: DatabaseSecurityNamespaceId,
        actual: DatabaseSecurityNamespaceId,
    },
    ClusterIncarnation {
        expected: [u8; 16],
        actual: [u8; 16],
    },
    LimitPolicy {
        expected: ObjectId,
        actual: ObjectId,
    },
    LimitPolicyEpoch {
        expected: u64,
        actual: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerSubject {
    Reservation(ReservationId),
    Charge(ChargeId),
}

/// Evidence references consumed by expiry validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpiryEvidence {
    pub reservation_id: ReservationId,
    pub reservation_generation: u64,
    pub owner_quiescence_proof_ref: ObjectId,
    pub time_validation_evidence_ref: ObjectId,
    pub time_classification: TimeEvidenceClassification,
    pub confirms_quiescence: bool,
}

/// The only time-evidence classification that authorizes expiry is
/// [`TimeEvidenceClassification::Expired`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeEvidenceClassification {
    NotExpired,
    Expired,
}

/// Exact whole-entry ledger operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerOperation {
    Reserve {
        reservation_id: ReservationId,
        vector: DurableChargeVector,
        hold: OwnerHold,
    },
    Charge {
        reservation_id: ReservationId,
        expected_reservation_generation: u64,
        charge_id: ChargeId,
        vector: DurableChargeVector,
        stable_subject_key: StableSubjectKey,
    },
    Release {
        subject: LedgerSubject,
        expected_generation: u64,
        exact_vector: DurableChargeVector,
    },
    Expire {
        reservation_id: ReservationId,
        expected_generation: u64,
        exact_vector: DurableChargeVector,
        evidence: ExpiryEvidence,
    },
    Transfer {
        subject: LedgerSubject,
        expected_generation: u64,
        target_owner: ResourceOwnerKey,
        target_path: QuotaPath,
        exact_conserved_vector: DurableChargeVector,
    },
    Adjust {
        charge_id: ChargeId,
        expected_generation: u64,
        before_vector: DurableChargeVector,
        after_vector: DurableChargeVector,
        stable_subject_key: StableSubjectKey,
    },
}

/// Authority and basis fields common to every transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionHeader {
    pub expected_ledger_identity: ResourceLedgerIdentity,
    pub transition_id: TransitionId,
    pub idempotency_key_digest: [u8; 32],
    pub expected_ledger_generation: u64,
    pub owner: ResourceOwnerKey,
    pub resource_class: ResourceClass,
    pub quota_path: QuotaPath,
}

/// Sequence-neutral pre-order transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceLedgerTransition {
    pub header: TransitionHeader,
    pub operation: LedgerOperation,
    /// Digest of the complete canonical transition body, supplied by the
    /// separately owned identity pipeline.
    pub body_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppliedOperation {
    Reserved,
    Charged,
    Released,
    Expired,
    Transferred,
    Adjusted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyDisposition {
    Applied,
    Replayed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub ledger_generation: u64,
    pub operation: AppliedOperation,
    pub disposition: ApplyDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AppliedTransition {
    transition: ResourceLedgerTransition,
    outcome: ApplyOutcome,
}

/// Canonical-authority ledger state and its atomic transition engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceLedger {
    identity: ResourceLedgerIdentity,
    generation: u64,
    buckets: BTreeMap<QuotaPath, BucketState>,
    reservations: BTreeMap<ReservationId, ReservationEntry>,
    charges: BTreeMap<ChargeId, ChargeEntry>,
    used_reservation_ids: BTreeSet<ReservationId>,
    used_charge_ids: BTreeSet<ChargeId>,
    applied_transitions: BTreeMap<TransitionId, AppliedTransition>,
}

impl ResourceLedger {
    /// Creates a generation-zero ledger from policy buckets.
    ///
    /// Because this constructor accepts no reservation or charge entries,
    /// every bucket must start with zero accounted usage. Restored ledgers need
    /// a separate constructor that validates the complete bucket/entry
    /// conservation relation rather than manufacturing unowned balances.
    pub fn try_new(
        identity: ResourceLedgerIdentity,
        buckets: impl IntoIterator<Item = (QuotaPath, BucketState)>,
    ) -> Result<Self, LedgerError> {
        let mut bucket_map = BTreeMap::new();
        for (path, state) in buckets {
            state
                .validate()
                .map_err(|cause| LedgerError::BucketRejected {
                    path: path.clone(),
                    cause,
                })?;
            if !state.accounting_is_empty() {
                return Err(LedgerError::NonemptyGenesisBucket { path });
            }
            if bucket_map.insert(path.clone(), state).is_some() {
                return Err(LedgerError::DuplicateBucket { path });
            }
        }
        if bucket_map.is_empty() {
            return Err(LedgerError::NoBuckets);
        }
        for path in bucket_map.keys() {
            for ancestor in path.ancestors_inclusive() {
                if !bucket_map.contains_key(&ancestor) {
                    return Err(LedgerError::MissingAncestorBucket {
                        path: path.clone(),
                        missing: ancestor,
                    });
                }
            }
        }
        Ok(Self {
            identity,
            generation: 0,
            buckets: bucket_map,
            reservations: BTreeMap::new(),
            charges: BTreeMap::new(),
            used_reservation_ids: BTreeSet::new(),
            used_charge_ids: BTreeSet::new(),
            applied_transitions: BTreeMap::new(),
        })
    }

    pub const fn identity(&self) -> ResourceLedgerIdentity {
        self.identity
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn bucket(&self, path: &QuotaPath) -> Option<&BucketState> {
        self.buckets.get(path)
    }

    pub fn reservation(&self, id: ReservationId) -> Option<&ReservationEntry> {
        self.reservations.get(&id)
    }

    pub fn charge(&self, id: ChargeId) -> Option<&ChargeEntry> {
        self.charges.get(&id)
    }

    pub fn apply(
        &mut self,
        transition: &ResourceLedgerTransition,
    ) -> Result<ApplyOutcome, LedgerError> {
        if let Some(applied) = self
            .applied_transitions
            .get(&transition.header.transition_id)
        {
            if applied.transition.body_digest != transition.body_digest {
                return Err(LedgerError::IdempotencyBodyDrift {
                    transition_id: transition.header.transition_id,
                    recorded: applied.transition.body_digest,
                    supplied: transition.body_digest,
                });
            }
            if applied.transition != *transition {
                return Err(LedgerError::IdempotencySemanticDrift {
                    transition_id: transition.header.transition_id,
                });
            }
            return Ok(ApplyOutcome {
                disposition: ApplyDisposition::Replayed,
                ..applied.outcome
            });
        }
        if transition.header.expected_ledger_identity.role != self.identity.role {
            return Err(LedgerError::WrongAuthorityRole {
                expected: self.identity.role,
                actual: transition.header.expected_ledger_identity.role,
            });
        }
        if let Some(mismatch) =
            ledger_identity_mismatch(self.identity, transition.header.expected_ledger_identity)
        {
            return Err(LedgerError::WrongLedgerIdentity { mismatch });
        }
        if transition.header.expected_ledger_generation != self.generation {
            return Err(LedgerError::StaleLedgerGeneration {
                expected: self.generation,
                actual: transition.header.expected_ledger_generation,
            });
        }
        let next_generation = self
            .generation
            .checked_add(1)
            .ok_or(LedgerError::LedgerGenerationOverflow)?;
        let operation = match &transition.operation {
            LedgerOperation::Reserve {
                reservation_id,
                vector,
                hold,
            } => {
                self.apply_reserve(transition, *reservation_id, *vector, *hold)?;
                AppliedOperation::Reserved
            }
            LedgerOperation::Charge {
                reservation_id,
                expected_reservation_generation,
                charge_id,
                vector,
                stable_subject_key,
            } => {
                self.apply_charge(
                    transition,
                    *reservation_id,
                    *expected_reservation_generation,
                    *charge_id,
                    *vector,
                    *stable_subject_key,
                )?;
                AppliedOperation::Charged
            }
            LedgerOperation::Release {
                subject,
                expected_generation,
                exact_vector,
            } => {
                self.apply_release(transition, subject, *expected_generation, *exact_vector)?;
                AppliedOperation::Released
            }
            LedgerOperation::Expire {
                reservation_id,
                expected_generation,
                exact_vector,
                evidence,
            } => {
                self.apply_expire(
                    transition,
                    *reservation_id,
                    *expected_generation,
                    *exact_vector,
                    *evidence,
                )?;
                AppliedOperation::Expired
            }
            LedgerOperation::Transfer {
                subject,
                expected_generation,
                target_owner,
                target_path,
                exact_conserved_vector,
            } => {
                self.apply_transfer(
                    transition,
                    subject,
                    *expected_generation,
                    target_owner,
                    target_path,
                    *exact_conserved_vector,
                )?;
                AppliedOperation::Transferred
            }
            LedgerOperation::Adjust {
                charge_id,
                expected_generation,
                before_vector,
                after_vector,
                stable_subject_key,
            } => {
                self.apply_adjust(
                    transition,
                    *charge_id,
                    *expected_generation,
                    *before_vector,
                    *after_vector,
                    *stable_subject_key,
                )?;
                AppliedOperation::Adjusted
            }
        };
        self.generation = next_generation;
        let outcome = ApplyOutcome {
            ledger_generation: next_generation,
            operation,
            disposition: ApplyDisposition::Applied,
        };
        self.applied_transitions.insert(
            transition.header.transition_id,
            AppliedTransition {
                transition: transition.clone(),
                outcome,
            },
        );
        Ok(outcome)
    }

    fn apply_reserve(
        &mut self,
        transition: &ResourceLedgerTransition,
        reservation_id: ReservationId,
        vector: DurableChargeVector,
        hold: OwnerHold,
    ) -> Result<(), LedgerError> {
        validate_maintenance_owner(transition.header.resource_class, &transition.header.owner)?;
        if vector.is_zero() {
            return Err(LedgerError::ZeroVector);
        }
        if self.used_reservation_ids.contains(&reservation_id) {
            return Err(LedgerError::ReservationIdAlreadyUsed { reservation_id });
        }
        let mut changes = BTreeMap::new();
        add_path_change(
            &mut changes,
            &transition.header.quota_path,
            transition.header.resource_class,
            ChangeDirection::Add,
            vector,
        )?;
        let plan = self.plan_bucket_changes(changes)?;
        self.install_bucket_plan(plan);
        self.reservations.insert(
            reservation_id,
            ReservationEntry {
                id: reservation_id,
                generation: 0,
                owner: transition.header.owner.clone(),
                path: transition.header.quota_path.clone(),
                class: transition.header.resource_class,
                vector,
                hold,
            },
        );
        self.used_reservation_ids.insert(reservation_id);
        Ok(())
    }

    fn apply_charge(
        &mut self,
        transition: &ResourceLedgerTransition,
        reservation_id: ReservationId,
        expected_generation: u64,
        charge_id: ChargeId,
        charged: DurableChargeVector,
        stable_subject_key: StableSubjectKey,
    ) -> Result<(), LedgerError> {
        let reservation = self
            .reservations
            .get(&reservation_id)
            .cloned()
            .ok_or(LedgerError::UnknownReservation { reservation_id })?;
        validate_entry_binding(
            &reservation.owner,
            &reservation.path,
            reservation.class,
            reservation.generation,
            reservation.vector,
            transition,
            (expected_generation, None),
        )?;
        if charged.is_zero() {
            return Err(LedgerError::ZeroVector);
        }
        if let Some(axis) = DurableChargeAxis::ALL
            .into_iter()
            .find(|&axis| charged.axis(axis) > reservation.vector.axis(axis))
        {
            return Err(LedgerError::ChargeExceedsReservation {
                reservation_id,
                axis,
                reserved: reservation.vector.axis(axis),
                charged: charged.axis(axis),
            });
        }
        if self.used_charge_ids.contains(&charge_id) {
            return Err(LedgerError::ChargeIdAlreadyUsed { charge_id });
        }
        let mut changes = BTreeMap::new();
        add_path_change(
            &mut changes,
            &reservation.path,
            reservation.class,
            ChangeDirection::Subtract,
            reservation.vector,
        )?;
        add_committed_path_change(
            &mut changes,
            &reservation.path,
            ChangeDirection::Add,
            charged,
        )?;
        let plan = self.plan_bucket_changes(changes)?;
        self.install_bucket_plan(plan);
        self.reservations.remove(&reservation_id);
        self.charges.insert(
            charge_id,
            ChargeEntry {
                id: charge_id,
                generation: 0,
                owner: reservation.owner,
                path: reservation.path,
                class: reservation.class,
                vector: charged,
                stable_subject_key,
            },
        );
        self.used_charge_ids.insert(charge_id);
        Ok(())
    }

    fn apply_release(
        &mut self,
        transition: &ResourceLedgerTransition,
        subject: &LedgerSubject,
        expected_generation: u64,
        exact_vector: DurableChargeVector,
    ) -> Result<(), LedgerError> {
        let snapshot = self.subject_snapshot(subject)?;
        snapshot.validate_binding(transition, expected_generation, exact_vector)?;
        let mut changes = BTreeMap::new();
        snapshot.add_change(&mut changes, ChangeDirection::Subtract)?;
        let plan = self.plan_bucket_changes(changes)?;
        self.install_bucket_plan(plan);
        self.remove_subject(subject);
        Ok(())
    }

    fn apply_expire(
        &mut self,
        transition: &ResourceLedgerTransition,
        reservation_id: ReservationId,
        expected_generation: u64,
        exact_vector: DurableChargeVector,
        evidence: ExpiryEvidence,
    ) -> Result<(), LedgerError> {
        let reservation = self
            .reservations
            .get(&reservation_id)
            .cloned()
            .ok_or(LedgerError::UnknownReservation { reservation_id })?;
        validate_entry_binding(
            &reservation.owner,
            &reservation.path,
            reservation.class,
            reservation.generation,
            reservation.vector,
            transition,
            (expected_generation, Some(exact_vector)),
        )?;
        if evidence.reservation_id != reservation_id {
            return Err(LedgerError::ExpiryEvidenceSubjectMismatch {
                expected: reservation_id,
                actual: evidence.reservation_id,
            });
        }
        if evidence.reservation_generation != expected_generation {
            return Err(LedgerError::ExpiryEvidenceGenerationMismatch {
                expected: expected_generation,
                actual: evidence.reservation_generation,
            });
        }
        if evidence.time_classification != TimeEvidenceClassification::Expired {
            return Err(LedgerError::ExpiryTimeNotExpired { reservation_id });
        }
        if !evidence.confirms_quiescence {
            return Err(LedgerError::ExpiryNotQuiescent { reservation_id });
        }
        if reservation.hold != OwnerHold::None {
            return Err(LedgerError::ProtectedOwnerCannotExpire {
                reservation_id,
                hold: reservation.hold,
            });
        }
        let mut changes = BTreeMap::new();
        add_path_change(
            &mut changes,
            &reservation.path,
            reservation.class,
            ChangeDirection::Subtract,
            reservation.vector,
        )?;
        let plan = self.plan_bucket_changes(changes)?;
        self.install_bucket_plan(plan);
        self.reservations.remove(&reservation_id);
        Ok(())
    }

    fn apply_transfer(
        &mut self,
        transition: &ResourceLedgerTransition,
        subject: &LedgerSubject,
        expected_generation: u64,
        target_owner: &ResourceOwnerKey,
        target_path: &QuotaPath,
        exact_vector: DurableChargeVector,
    ) -> Result<(), LedgerError> {
        let snapshot = self.subject_snapshot(subject)?;
        snapshot.validate_binding(transition, expected_generation, exact_vector)?;
        validate_maintenance_owner(snapshot.class, target_owner)?;
        if snapshot.owner == *target_owner && snapshot.path == *target_path {
            return Err(LedgerError::NoopTransfer);
        }
        let next_entry_generation = snapshot
            .generation
            .checked_add(1)
            .ok_or(LedgerError::EntryGenerationOverflow)?;
        let mut changes = BTreeMap::new();
        snapshot.add_change(&mut changes, ChangeDirection::Subtract)?;
        snapshot.add_change_at(&mut changes, target_path, ChangeDirection::Add)?;
        let plan = self.plan_bucket_changes(changes)?;
        match subject {
            LedgerSubject::Reservation(id) => {
                let entry =
                    self.reservations
                        .get_mut(id)
                        .ok_or(LedgerError::UnknownReservation {
                            reservation_id: *id,
                        })?;
                entry.generation = next_entry_generation;
                entry.owner = target_owner.clone();
                entry.path = target_path.clone();
            }
            LedgerSubject::Charge(id) => {
                let entry = self
                    .charges
                    .get_mut(id)
                    .ok_or(LedgerError::UnknownCharge { charge_id: *id })?;
                entry.generation = next_entry_generation;
                entry.owner = target_owner.clone();
                entry.path = target_path.clone();
            }
        }
        self.install_bucket_plan(plan);
        Ok(())
    }

    fn apply_adjust(
        &mut self,
        transition: &ResourceLedgerTransition,
        charge_id: ChargeId,
        expected_generation: u64,
        before_vector: DurableChargeVector,
        after_vector: DurableChargeVector,
        stable_subject_key: StableSubjectKey,
    ) -> Result<(), LedgerError> {
        let charge = self
            .charges
            .get(&charge_id)
            .cloned()
            .ok_or(LedgerError::UnknownCharge { charge_id })?;
        validate_entry_binding(
            &charge.owner,
            &charge.path,
            charge.class,
            charge.generation,
            charge.vector,
            transition,
            (expected_generation, Some(before_vector)),
        )?;
        if stable_subject_key != charge.stable_subject_key {
            return Err(LedgerError::StableSubjectKeyMismatch {
                charge_id,
                expected: charge.stable_subject_key,
                actual: stable_subject_key,
            });
        }
        if after_vector.is_zero() {
            return Err(LedgerError::WholeChargeMustUseRelease { charge_id });
        }
        if before_vector == after_vector {
            return Err(LedgerError::NoopAdjust { charge_id });
        }
        let next_entry_generation = charge
            .generation
            .checked_add(1)
            .ok_or(LedgerError::EntryGenerationOverflow)?;
        let mut changes = BTreeMap::new();
        add_committed_path_change(
            &mut changes,
            &charge.path,
            ChangeDirection::Subtract,
            before_vector,
        )?;
        add_committed_path_change(
            &mut changes,
            &charge.path,
            ChangeDirection::Add,
            after_vector,
        )?;
        let plan = self.plan_bucket_changes(changes)?;
        let entry = self
            .charges
            .get_mut(&charge_id)
            .ok_or(LedgerError::UnknownCharge { charge_id })?;
        entry.generation = next_entry_generation;
        entry.vector = after_vector;
        self.install_bucket_plan(plan);
        Ok(())
    }

    fn subject_snapshot(&self, subject: &LedgerSubject) -> Result<SubjectSnapshot, LedgerError> {
        match *subject {
            LedgerSubject::Reservation(id) => {
                let entry = self
                    .reservations
                    .get(&id)
                    .ok_or(LedgerError::UnknownReservation { reservation_id: id })?;
                Ok(SubjectSnapshot {
                    owner: entry.owner.clone(),
                    path: entry.path.clone(),
                    class: entry.class,
                    generation: entry.generation,
                    vector: entry.vector,
                    component: SubjectComponent::Reservation,
                })
            }
            LedgerSubject::Charge(id) => {
                let entry = self
                    .charges
                    .get(&id)
                    .ok_or(LedgerError::UnknownCharge { charge_id: id })?;
                Ok(SubjectSnapshot {
                    owner: entry.owner.clone(),
                    path: entry.path.clone(),
                    class: entry.class,
                    generation: entry.generation,
                    vector: entry.vector,
                    component: SubjectComponent::Committed,
                })
            }
        }
    }

    fn remove_subject(&mut self, subject: &LedgerSubject) {
        match *subject {
            LedgerSubject::Reservation(id) => {
                self.reservations.remove(&id);
            }
            LedgerSubject::Charge(id) => {
                self.charges.remove(&id);
            }
        }
    }

    fn plan_bucket_changes(
        &self,
        changes: BTreeMap<QuotaPath, BucketChange>,
    ) -> Result<Vec<(QuotaPath, BucketState)>, LedgerError> {
        let mut plan = Vec::new();
        plan.try_reserve_exact(changes.len()).map_err(|_| {
            LedgerError::BucketPlanAllocationFailed {
                requested_buckets: changes.len(),
            }
        })?;
        for (path, change) in changes {
            let mut state = *self
                .buckets
                .get(&path)
                .ok_or_else(|| LedgerError::UnknownBucket { path: path.clone() })?;
            state.ordinary_reserved = state.ordinary_reserved.checked_sub(change.ordinary_sub)?;
            state.ordinary_reserved = state.ordinary_reserved.checked_add(change.ordinary_add)?;
            state.maintenance_reserved = state
                .maintenance_reserved
                .checked_sub(change.maintenance_sub)?;
            state.maintenance_reserved = state
                .maintenance_reserved
                .checked_add(change.maintenance_add)?;
            state.committed = state.committed.checked_sub(change.committed_sub)?;
            state.committed = state.committed.checked_add(change.committed_add)?;
            state
                .validate()
                .map_err(|cause| LedgerError::BucketRejected {
                    path: path.clone(),
                    cause,
                })?;
            plan.push((path, state));
        }
        Ok(plan)
    }

    fn install_bucket_plan(&mut self, plan: Vec<(QuotaPath, BucketState)>) {
        for (path, state) in plan {
            self.buckets.insert(path, state);
        }
    }
}

fn validate_maintenance_owner(
    class: ResourceClass,
    owner: &ResourceOwnerKey,
) -> Result<(), LedgerError> {
    if class == ResourceClass::RegisteredMaintenance
        && !matches!(owner, ResourceOwnerKey::Maintenance { .. })
    {
        return Err(LedgerError::MaintenanceReserveRequiresRegisteredOwner);
    }
    Ok(())
}

fn ledger_identity_mismatch(
    expected: ResourceLedgerIdentity,
    actual: ResourceLedgerIdentity,
) -> Option<LedgerIdentityMismatch> {
    if expected.database_security_namespace_id != actual.database_security_namespace_id {
        return Some(LedgerIdentityMismatch::DatabaseSecurityNamespace {
            expected: expected.database_security_namespace_id,
            actual: actual.database_security_namespace_id,
        });
    }
    if expected.cluster_incarnation != actual.cluster_incarnation {
        return Some(LedgerIdentityMismatch::ClusterIncarnation {
            expected: expected.cluster_incarnation,
            actual: actual.cluster_incarnation,
        });
    }
    if expected.limit_policy_oid != actual.limit_policy_oid {
        return Some(LedgerIdentityMismatch::LimitPolicy {
            expected: expected.limit_policy_oid,
            actual: actual.limit_policy_oid,
        });
    }
    if expected.limit_policy_epoch != actual.limit_policy_epoch {
        return Some(LedgerIdentityMismatch::LimitPolicyEpoch {
            expected: expected.limit_policy_epoch,
            actual: actual.limit_policy_epoch,
        });
    }
    None
}

#[derive(Clone, Debug)]
struct SubjectSnapshot {
    owner: ResourceOwnerKey,
    path: QuotaPath,
    class: ResourceClass,
    generation: u64,
    vector: DurableChargeVector,
    component: SubjectComponent,
}

impl SubjectSnapshot {
    fn validate_binding(
        &self,
        transition: &ResourceLedgerTransition,
        expected_generation: u64,
        exact_vector: DurableChargeVector,
    ) -> Result<(), LedgerError> {
        validate_entry_binding(
            &self.owner,
            &self.path,
            self.class,
            self.generation,
            self.vector,
            transition,
            (expected_generation, Some(exact_vector)),
        )
    }

    fn add_change(
        &self,
        changes: &mut BTreeMap<QuotaPath, BucketChange>,
        direction: ChangeDirection,
    ) -> Result<(), LedgerError> {
        self.add_change_at(changes, &self.path, direction)
    }

    fn add_change_at(
        &self,
        changes: &mut BTreeMap<QuotaPath, BucketChange>,
        path: &QuotaPath,
        direction: ChangeDirection,
    ) -> Result<(), LedgerError> {
        match self.component {
            SubjectComponent::Reservation => {
                add_path_change(changes, path, self.class, direction, self.vector)
            }
            SubjectComponent::Committed => {
                add_committed_path_change(changes, path, direction, self.vector)
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum SubjectComponent {
    Reservation,
    Committed,
}

fn validate_entry_binding(
    owner: &ResourceOwnerKey,
    path: &QuotaPath,
    class: ResourceClass,
    generation: u64,
    vector: DurableChargeVector,
    transition: &ResourceLedgerTransition,
    expected: (u64, Option<DurableChargeVector>),
) -> Result<(), LedgerError> {
    let (expected_generation, exact_vector) = expected;
    if owner != &transition.header.owner {
        return Err(LedgerError::OwnerMismatch);
    }
    if path != &transition.header.quota_path {
        return Err(LedgerError::QuotaPathMismatch {
            expected: path.clone(),
            actual: transition.header.quota_path.clone(),
        });
    }
    if class != transition.header.resource_class {
        return Err(LedgerError::ResourceClassMismatch {
            expected: class,
            actual: transition.header.resource_class,
        });
    }
    if generation != expected_generation {
        return Err(LedgerError::StaleEntryGeneration {
            expected: generation,
            actual: expected_generation,
        });
    }
    if let Some(exact) = exact_vector
        && vector != exact
    {
        return Err(LedgerError::ExactVectorMismatch {
            expected: vector,
            actual: exact,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct BucketChange {
    ordinary_sub: DurableChargeVector,
    ordinary_add: DurableChargeVector,
    maintenance_sub: DurableChargeVector,
    maintenance_add: DurableChargeVector,
    committed_sub: DurableChargeVector,
    committed_add: DurableChargeVector,
}

#[derive(Clone, Copy, Debug)]
enum ChangeDirection {
    Add,
    Subtract,
}

fn add_path_change(
    changes: &mut BTreeMap<QuotaPath, BucketChange>,
    path: &QuotaPath,
    class: ResourceClass,
    direction: ChangeDirection,
    vector: DurableChargeVector,
) -> Result<(), LedgerError> {
    for ancestor in path.ancestors_inclusive() {
        let change = changes.entry(ancestor).or_default();
        let target = match (class, direction) {
            (ResourceClass::Ordinary, ChangeDirection::Add) => &mut change.ordinary_add,
            (ResourceClass::Ordinary, ChangeDirection::Subtract) => &mut change.ordinary_sub,
            (ResourceClass::RegisteredMaintenance, ChangeDirection::Add) => {
                &mut change.maintenance_add
            }
            (ResourceClass::RegisteredMaintenance, ChangeDirection::Subtract) => {
                &mut change.maintenance_sub
            }
        };
        *target = target.checked_add(vector)?;
    }
    Ok(())
}

fn add_committed_path_change(
    changes: &mut BTreeMap<QuotaPath, BucketChange>,
    path: &QuotaPath,
    direction: ChangeDirection,
    vector: DurableChargeVector,
) -> Result<(), LedgerError> {
    for ancestor in path.ancestors_inclusive() {
        let change = changes.entry(ancestor).or_default();
        let target = match direction {
            ChangeDirection::Add => &mut change.committed_add,
            ChangeDirection::Subtract => &mut change.committed_sub,
        };
        *target = target.checked_add(vector)?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LedgerError {
    NoBuckets,
    DuplicateBucket {
        path: QuotaPath,
    },
    MissingAncestorBucket {
        path: QuotaPath,
        missing: QuotaPath,
    },
    UnknownBucket {
        path: QuotaPath,
    },
    BucketRejected {
        path: QuotaPath,
        cause: BucketStateError,
    },
    BucketPlanAllocationFailed {
        requested_buckets: usize,
    },
    NonemptyGenesisBucket {
        path: QuotaPath,
    },
    Vector(DurableVectorError),
    WrongAuthorityRole {
        expected: ResourceAccountingRole,
        actual: ResourceAccountingRole,
    },
    WrongLedgerIdentity {
        mismatch: LedgerIdentityMismatch,
    },
    StaleLedgerGeneration {
        expected: u64,
        actual: u64,
    },
    LedgerGenerationOverflow,
    EntryGenerationOverflow,
    IdempotencyBodyDrift {
        transition_id: TransitionId,
        recorded: [u8; 32],
        supplied: [u8; 32],
    },
    IdempotencySemanticDrift {
        transition_id: TransitionId,
    },
    ZeroVector,
    ReservationIdAlreadyUsed {
        reservation_id: ReservationId,
    },
    ChargeIdAlreadyUsed {
        charge_id: ChargeId,
    },
    MaintenanceReserveRequiresRegisteredOwner,
    UnknownReservation {
        reservation_id: ReservationId,
    },
    UnknownCharge {
        charge_id: ChargeId,
    },
    OwnerMismatch,
    QuotaPathMismatch {
        expected: QuotaPath,
        actual: QuotaPath,
    },
    ResourceClassMismatch {
        expected: ResourceClass,
        actual: ResourceClass,
    },
    StaleEntryGeneration {
        expected: u64,
        actual: u64,
    },
    ExactVectorMismatch {
        expected: DurableChargeVector,
        actual: DurableChargeVector,
    },
    ChargeExceedsReservation {
        reservation_id: ReservationId,
        axis: DurableChargeAxis,
        reserved: u64,
        charged: u64,
    },
    ExpiryEvidenceSubjectMismatch {
        expected: ReservationId,
        actual: ReservationId,
    },
    ExpiryEvidenceGenerationMismatch {
        expected: u64,
        actual: u64,
    },
    ExpiryTimeNotExpired {
        reservation_id: ReservationId,
    },
    ExpiryNotQuiescent {
        reservation_id: ReservationId,
    },
    ProtectedOwnerCannotExpire {
        reservation_id: ReservationId,
        hold: OwnerHold,
    },
    NoopTransfer,
    NoopAdjust {
        charge_id: ChargeId,
    },
    StableSubjectKeyMismatch {
        charge_id: ChargeId,
        expected: StableSubjectKey,
        actual: StableSubjectKey,
    },
    WholeChargeMustUseRelease {
        charge_id: ChargeId,
    },
}

impl From<DurableVectorError> for LedgerError {
    fn from(value: DurableVectorError) -> Self {
        Self::Vector(value)
    }
}

impl fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "resource-ledger transition rejected: {self:?}")
    }
}

impl std::error::Error for LedgerError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(bytes: u64) -> DurableChargeVector {
        DurableChargeVector {
            canonical_durable_bytes: bytes,
            retained_history_bytes: bytes / 2,
            branch_count: u64::from(bytes != 0),
            index_count: 0,
            view_count: 0,
            subscription_count: 0,
        }
    }

    fn capacity(value: u64) -> DurableChargeVector {
        DurableChargeVector {
            canonical_durable_bytes: value,
            retained_history_bytes: value,
            branch_count: value,
            index_count: value,
            view_count: value,
            subscription_count: value,
        }
    }

    fn db_path() -> QuotaPath {
        QuotaPath::try_new(vec![QuotaSegment::Database(DatabaseId([1; 16]))])
            .expect("database path is canonical")
    }

    fn tenant_path() -> QuotaPath {
        QuotaPath::try_new(vec![
            QuotaSegment::Database(DatabaseId([1; 16])),
            QuotaSegment::Tenant(7),
        ])
        .expect("tenant path is canonical")
    }

    fn graph_path() -> QuotaPath {
        QuotaPath::try_new(vec![
            QuotaSegment::Database(DatabaseId([1; 16])),
            QuotaSegment::Tenant(7),
            QuotaSegment::Graph(GraphId(11)),
        ])
        .expect("graph path is canonical")
    }

    fn branch_path(branch: u128) -> QuotaPath {
        QuotaPath::try_new(vec![
            QuotaSegment::Database(DatabaseId([1; 16])),
            QuotaSegment::Tenant(7),
            QuotaSegment::Graph(GraphId(11)),
            QuotaSegment::Branch(BranchId(branch)),
        ])
        .expect("branch path is canonical")
    }

    fn owner(seed: u8) -> ResourceOwnerKey {
        ResourceOwnerKey::DurableObject {
            object_class: 3,
            stable_logical_owner_key: [seed; 32],
        }
    }

    fn maintenance_owner(seed: u8) -> ResourceOwnerKey {
        ResourceOwnerKey::Maintenance {
            job_id: [seed; 32],
            registered_class: u16::from(seed),
        }
    }

    fn identity() -> ResourceLedgerIdentity {
        ResourceLedgerIdentity {
            database_security_namespace_id: DatabaseSecurityNamespaceId([2; 32]),
            cluster_incarnation: [3; 16],
            role: ResourceAccountingRole::Local,
            limit_policy_oid: ObjectId([4; 32]),
            limit_policy_epoch: 5,
        }
    }

    fn ledger_with_limit(limit: u64, reserve: u64) -> ResourceLedger {
        let state = BucketState::empty(capacity(limit), capacity(reserve))
            .expect("fixture limits are internally consistent");
        ResourceLedger::try_new(
            identity(),
            [
                (db_path(), state),
                (tenant_path(), state),
                (graph_path(), state),
                (branch_path(13), state),
                (branch_path(17), state),
            ],
        )
        .expect("fixture contains every ancestor")
    }

    fn transition(
        ledger: &ResourceLedger,
        seed: u8,
        owner: ResourceOwnerKey,
        path: QuotaPath,
        class: ResourceClass,
        operation: LedgerOperation,
    ) -> ResourceLedgerTransition {
        ResourceLedgerTransition {
            header: TransitionHeader {
                expected_ledger_identity: ledger.identity(),
                transition_id: TransitionId([seed; 32]),
                idempotency_key_digest: [seed.wrapping_add(1); 32],
                expected_ledger_generation: ledger.generation(),
                owner,
                resource_class: class,
                quota_path: path,
            },
            operation,
            body_digest: [seed.wrapping_add(2); 32],
        }
    }

    #[test]
    fn quota_paths_require_the_exact_root_to_leaf_shape() {
        assert_eq!(QuotaPath::try_new(Vec::new()), Err(QuotaPathError::Empty));
        assert!(matches!(
            QuotaPath::try_new(vec![QuotaSegment::Tenant(1)]),
            Err(QuotaPathError::MissingDatabaseRoot { .. })
        ));
        assert!(matches!(
            QuotaPath::try_new(vec![
                QuotaSegment::Database(DatabaseId([0; 16])),
                QuotaSegment::Graph(GraphId(1)),
            ]),
            Err(QuotaPathError::NonCanonicalLevel { .. })
        ));
    }

    #[test]
    fn reserve_updates_each_ancestor_once_and_replay_is_idempotent() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([10; 32]);
        let tx = transition(
            &ledger,
            20,
            owner(1),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(25),
                hold: OwnerHold::None,
            },
        );
        let applied = ledger.apply(&tx).expect("reservation fits");
        assert_eq!(applied.disposition, ApplyDisposition::Applied);
        for path in [db_path(), tenant_path(), graph_path(), branch_path(13)] {
            assert_eq!(
                ledger
                    .bucket(&path)
                    .expect("ancestor bucket exists")
                    .ordinary_reserved(),
                vector(25)
            );
        }
        assert_eq!(
            ledger
                .bucket(&branch_path(17))
                .expect("unrelated bucket exists")
                .ordinary_reserved(),
            DurableChargeVector::ZERO
        );
        let replay = ledger.apply(&tx).expect("exact retry replays");
        assert_eq!(replay.disposition, ApplyDisposition::Replayed);
        assert_eq!(replay.ledger_generation, 1);

        let mut drift = tx.clone();
        drift.body_digest[0] ^= 1;
        assert!(matches!(
            ledger.apply(&drift),
            Err(LedgerError::IdempotencyBodyDrift { .. })
        ));

        let mut semantic_drift = tx.clone();
        semantic_drift.header.idempotency_key_digest[0] ^= 1;
        assert_eq!(
            ledger.apply(&semantic_drift),
            Err(LedgerError::IdempotencySemanticDrift {
                transition_id: tx.header.transition_id,
            })
        );
        assert_eq!(ledger.generation(), 1);
    }

    #[test]
    fn protected_reserve_is_unavailable_to_ordinary_but_available_to_maintenance() {
        let mut ordinary = ledger_with_limit(100, 20);
        let ordinary_tx = transition(
            &ordinary,
            30,
            owner(2),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: ReservationId([31; 32]),
                vector: vector(81),
                hold: OwnerHold::None,
            },
        );
        assert!(matches!(
            ordinary.apply(&ordinary_tx),
            Err(LedgerError::BucketRejected {
                cause: BucketStateError::OrdinaryCapacityExceeded { .. },
                ..
            })
        ));
        assert_eq!(ordinary.generation(), 0);

        let mut maintenance = ledger_with_limit(100, 20);
        let maintenance_tx = transition(
            &maintenance,
            32,
            maintenance_owner(3),
            branch_path(13),
            ResourceClass::RegisteredMaintenance,
            LedgerOperation::Reserve {
                reservation_id: ReservationId([33; 32]),
                vector: vector(20),
                hold: OwnerHold::None,
            },
        );
        maintenance
            .apply(&maintenance_tx)
            .expect("registered maintenance may consume its reserve");
        assert_eq!(
            maintenance
                .bucket(&branch_path(13))
                .expect("bucket exists")
                .maintenance_reserved(),
            vector(20)
        );
    }

    #[test]
    fn maintenance_reserve_requires_a_registered_owner_across_transfer() {
        let mut ledger = ledger_with_limit(100, 20);
        let before = ledger.clone();
        let invalid_reserve = transition(
            &ledger,
            34,
            owner(3),
            branch_path(13),
            ResourceClass::RegisteredMaintenance,
            LedgerOperation::Reserve {
                reservation_id: ReservationId([35; 32]),
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        assert_eq!(
            ledger.apply(&invalid_reserve),
            Err(LedgerError::MaintenanceReserveRequiresRegisteredOwner)
        );
        assert_eq!(ledger, before);

        let reservation_id = ReservationId([36; 32]);
        let valid_reserve = transition(
            &ledger,
            37,
            maintenance_owner(4),
            branch_path(13),
            ResourceClass::RegisteredMaintenance,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        ledger
            .apply(&valid_reserve)
            .expect("registered maintenance owner may use its lane");
        let before_transfer = ledger.clone();
        let invalid_transfer = transition(
            &ledger,
            38,
            maintenance_owner(4),
            branch_path(13),
            ResourceClass::RegisteredMaintenance,
            LedgerOperation::Transfer {
                subject: LedgerSubject::Reservation(reservation_id),
                expected_generation: 0,
                target_owner: owner(5),
                target_path: branch_path(17),
                exact_conserved_vector: vector(10),
            },
        );
        assert_eq!(
            ledger.apply(&invalid_transfer),
            Err(LedgerError::MaintenanceReserveRequiresRegisteredOwner)
        );
        assert_eq!(ledger, before_transfer);
    }

    #[test]
    fn charge_consumes_the_whole_reservation_and_releases_the_remainder() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([40; 32]);
        let charge_id = ChargeId([41; 32]);
        let reserve = transition(
            &ledger,
            42,
            owner(4),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(50),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let before_overcharge = ledger.clone();
        let overcharge = transition(
            &ledger,
            45,
            owner(4),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(60),
                stable_subject_key: StableSubjectKey([46; 32]),
            },
        );
        assert_eq!(
            ledger.apply(&overcharge),
            Err(LedgerError::ChargeExceedsReservation {
                reservation_id,
                axis: DurableChargeAxis::CanonicalDurableBytes,
                reserved: 50,
                charged: 60,
            })
        );
        assert_eq!(ledger, before_overcharge);

        let charge = transition(
            &ledger,
            43,
            owner(4),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(30),
                stable_subject_key: StableSubjectKey([44; 32]),
            },
        );
        ledger.apply(&charge).expect("charge succeeds");
        assert!(ledger.reservation(reservation_id).is_none());
        assert_eq!(
            ledger.charge(charge_id).expect("charge exists").vector(),
            vector(30)
        );
        let branch = ledger.bucket(&branch_path(13)).expect("bucket exists");
        assert_eq!(branch.ordinary_reserved(), DurableChargeVector::ZERO);
        assert_eq!(branch.committed(), vector(30));
    }

    #[test]
    fn shared_ancestor_transfer_is_netted_and_destination_failure_is_atomic() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([50; 32]);
        let reserve = transition(
            &ledger,
            51,
            owner(5),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(60),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let transfer = transition(
            &ledger,
            52,
            owner(5),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Transfer {
                subject: LedgerSubject::Reservation(reservation_id),
                expected_generation: 0,
                target_owner: owner(6),
                target_path: branch_path(17),
                exact_conserved_vector: vector(60),
            },
        );
        ledger.apply(&transfer).expect("transfer succeeds");
        assert_eq!(
            ledger
                .bucket(&graph_path())
                .expect("shared ancestor exists")
                .ordinary_reserved(),
            vector(60)
        );
        assert_eq!(
            ledger
                .bucket(&branch_path(13))
                .expect("source exists")
                .ordinary_reserved(),
            DurableChargeVector::ZERO
        );
        assert_eq!(
            ledger
                .bucket(&branch_path(17))
                .expect("target exists")
                .ordinary_reserved(),
            vector(60)
        );
        let entry = ledger
            .reservation(reservation_id)
            .expect("reservation moved");
        assert_eq!(entry.generation(), 1);
        assert_eq!(entry.owner(), &owner(6));

        let ancestor_state =
            BucketState::empty(capacity(100), capacity(10)).expect("ancestor limits are valid");
        let constrained_target =
            BucketState::empty(capacity(50), capacity(10)).expect("target limits are valid");
        let mut constrained = ResourceLedger::try_new(
            identity(),
            [
                (db_path(), ancestor_state),
                (tenant_path(), ancestor_state),
                (graph_path(), ancestor_state),
                (branch_path(13), ancestor_state),
                (branch_path(17), constrained_target),
            ],
        )
        .expect("constrained fixture contains every ancestor");
        let fill = transition(
            &constrained,
            53,
            owner(7),
            branch_path(17),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: ReservationId([54; 32]),
                vector: vector(40),
                hold: OwnerHold::None,
            },
        );
        constrained.apply(&fill).expect("target fill succeeds");
        let source = transition(
            &constrained,
            55,
            owner(8),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: ReservationId([56; 32]),
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        constrained.apply(&source).expect("source reserve succeeds");
        let generation_before = constrained.generation();
        let failed = transition(
            &constrained,
            57,
            owner(8),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Transfer {
                subject: LedgerSubject::Reservation(ReservationId([56; 32])),
                expected_generation: 0,
                target_owner: owner(9),
                target_path: branch_path(17),
                exact_conserved_vector: vector(10),
            },
        );
        assert!(matches!(
            constrained.apply(&failed),
            Err(LedgerError::BucketRejected { .. })
        ));
        assert_eq!(constrained.generation(), generation_before);
        assert_eq!(
            constrained
                .reservation(ReservationId([56; 32]))
                .expect("failed transfer leaves source intact")
                .path(),
            &branch_path(13)
        );
    }

    #[test]
    fn protected_owners_cannot_expire_and_release_remains_explicit() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([60; 32]);
        let reserve = transition(
            &ledger,
            61,
            owner(10),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(20),
                hold: OwnerHold::PreparedOwnership,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let expire = transition(
            &ledger,
            62,
            owner(10),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Expire {
                reservation_id,
                expected_generation: 0,
                exact_vector: vector(20),
                evidence: ExpiryEvidence {
                    reservation_id,
                    reservation_generation: 0,
                    owner_quiescence_proof_ref: ObjectId([63; 32]),
                    time_validation_evidence_ref: ObjectId([64; 32]),
                    time_classification: TimeEvidenceClassification::Expired,
                    confirms_quiescence: true,
                },
            },
        );
        assert_eq!(
            ledger.apply(&expire),
            Err(LedgerError::ProtectedOwnerCannotExpire {
                reservation_id,
                hold: OwnerHold::PreparedOwnership,
            })
        );
        let release = transition(
            &ledger,
            65,
            owner(10),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Release {
                subject: LedgerSubject::Reservation(reservation_id),
                expected_generation: 0,
                exact_vector: vector(20),
            },
        );
        ledger
            .apply(&release)
            .expect("explicit authorized release succeeds");
        assert!(ledger.reservation(reservation_id).is_none());
    }

    #[test]
    fn expiry_requires_exact_positive_evidence_and_is_atomic() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([66; 32]);
        let reserve = transition(
            &ledger,
            67,
            owner(13),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(12),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let generation_before = ledger.generation();
        let rejected = transition(
            &ledger,
            68,
            owner(13),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Expire {
                reservation_id,
                expected_generation: 0,
                exact_vector: vector(12),
                evidence: ExpiryEvidence {
                    reservation_id,
                    reservation_generation: 0,
                    owner_quiescence_proof_ref: ObjectId([69; 32]),
                    time_validation_evidence_ref: ObjectId([70; 32]),
                    time_classification: TimeEvidenceClassification::Expired,
                    confirms_quiescence: false,
                },
            },
        );
        assert_eq!(
            ledger.apply(&rejected),
            Err(LedgerError::ExpiryNotQuiescent { reservation_id })
        );
        assert_eq!(ledger.generation(), generation_before);
        assert!(ledger.reservation(reservation_id).is_some());

        let expire = transition(
            &ledger,
            71,
            owner(13),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Expire {
                reservation_id,
                expected_generation: 0,
                exact_vector: vector(12),
                evidence: ExpiryEvidence {
                    reservation_id,
                    reservation_generation: 0,
                    owner_quiescence_proof_ref: ObjectId([72; 32]),
                    time_validation_evidence_ref: ObjectId([73; 32]),
                    time_classification: TimeEvidenceClassification::Expired,
                    confirms_quiescence: true,
                },
            },
        );
        let outcome = ledger.apply(&expire).expect("supported expiry succeeds");
        assert_eq!(outcome.operation, AppliedOperation::Expired);
        assert!(ledger.reservation(reservation_id).is_none());
        assert_eq!(
            ledger
                .bucket(&branch_path(13))
                .expect("bucket exists")
                .ordinary_reserved(),
            DurableChargeVector::ZERO
        );
    }

    #[test]
    fn expiry_evidence_is_bound_to_the_current_entry_generation_and_time_class() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([74; 32]);
        let reserve = transition(
            &ledger,
            75,
            owner(16),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(15),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let transfer = transition(
            &ledger,
            76,
            owner(16),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Transfer {
                subject: LedgerSubject::Reservation(reservation_id),
                expected_generation: 0,
                target_owner: owner(17),
                target_path: branch_path(13),
                exact_conserved_vector: vector(15),
            },
        );
        ledger.apply(&transfer).expect("owner transfer succeeds");
        let before_expiry = ledger.clone();
        let stale_evidence = transition(
            &ledger,
            77,
            owner(17),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Expire {
                reservation_id,
                expected_generation: 1,
                exact_vector: vector(15),
                evidence: ExpiryEvidence {
                    reservation_id,
                    reservation_generation: 0,
                    owner_quiescence_proof_ref: ObjectId([78; 32]),
                    time_validation_evidence_ref: ObjectId([79; 32]),
                    time_classification: TimeEvidenceClassification::Expired,
                    confirms_quiescence: true,
                },
            },
        );
        assert_eq!(
            ledger.apply(&stale_evidence),
            Err(LedgerError::ExpiryEvidenceGenerationMismatch {
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(ledger, before_expiry);

        let not_expired = transition(
            &ledger,
            80,
            owner(17),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Expire {
                reservation_id,
                expected_generation: 1,
                exact_vector: vector(15),
                evidence: ExpiryEvidence {
                    reservation_id,
                    reservation_generation: 1,
                    owner_quiescence_proof_ref: ObjectId([81; 32]),
                    time_validation_evidence_ref: ObjectId([82; 32]),
                    time_classification: TimeEvidenceClassification::NotExpired,
                    confirms_quiescence: true,
                },
            },
        );
        assert_eq!(
            ledger.apply(&not_expired),
            Err(LedgerError::ExpiryTimeNotExpired { reservation_id })
        );
        assert_eq!(ledger, before_expiry);
    }

    #[test]
    fn committed_charge_transfers_whole_then_releases_once() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([82; 32]);
        let charge_id = ChargeId([83; 32]);
        let reserve = transition(
            &ledger,
            84,
            owner(14),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(30),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let charge = transition(
            &ledger,
            85,
            owner(14),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(25),
                stable_subject_key: StableSubjectKey([86; 32]),
            },
        );
        ledger.apply(&charge).expect("charge succeeds");
        let transfer = transition(
            &ledger,
            87,
            owner(14),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Transfer {
                subject: LedgerSubject::Charge(charge_id),
                expected_generation: 0,
                target_owner: owner(15),
                target_path: branch_path(17),
                exact_conserved_vector: vector(25),
            },
        );
        ledger.apply(&transfer).expect("charge transfer succeeds");
        assert_eq!(
            ledger
                .bucket(&graph_path())
                .expect("shared ancestor exists")
                .committed(),
            vector(25)
        );
        assert_eq!(
            ledger
                .bucket(&branch_path(13))
                .expect("source exists")
                .committed(),
            DurableChargeVector::ZERO
        );
        assert_eq!(
            ledger
                .bucket(&branch_path(17))
                .expect("target exists")
                .committed(),
            vector(25)
        );
        let release = transition(
            &ledger,
            88,
            owner(15),
            branch_path(17),
            ResourceClass::Ordinary,
            LedgerOperation::Release {
                subject: LedgerSubject::Charge(charge_id),
                expected_generation: 1,
                exact_vector: vector(25),
            },
        );
        ledger.apply(&release).expect("charge release succeeds");
        assert!(ledger.charge(charge_id).is_none());
        assert_eq!(
            ledger
                .bucket(&graph_path())
                .expect("shared ancestor exists")
                .committed(),
            DurableChargeVector::ZERO
        );
        let duplicate_release = transition(
            &ledger,
            89,
            owner(15),
            branch_path(17),
            ResourceClass::Ordinary,
            LedgerOperation::Release {
                subject: LedgerSubject::Charge(charge_id),
                expected_generation: 1,
                exact_vector: vector(25),
            },
        );
        assert_eq!(
            ledger.apply(&duplicate_release),
            Err(LedgerError::UnknownCharge { charge_id })
        );
    }

    #[test]
    fn adjustment_requires_exact_subject_and_preserves_atomicity() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([70; 32]);
        let charge_id = ChargeId([71; 32]);
        let subject_key = StableSubjectKey([72; 32]);
        let reserve = transition(
            &ledger,
            73,
            owner(11),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(50),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let charge = transition(
            &ledger,
            74,
            owner(11),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(40),
                stable_subject_key: subject_key,
            },
        );
        ledger.apply(&charge).expect("charge succeeds");
        let before_noop = ledger.clone();
        let noop = transition(
            &ledger,
            77,
            owner(11),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Adjust {
                charge_id,
                expected_generation: 0,
                before_vector: vector(40),
                after_vector: vector(40),
                stable_subject_key: subject_key,
            },
        );
        assert_eq!(
            ledger.apply(&noop),
            Err(LedgerError::NoopAdjust { charge_id })
        );
        assert_eq!(ledger, before_noop);
        let wrong = transition(
            &ledger,
            75,
            owner(11),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Adjust {
                charge_id,
                expected_generation: 0,
                before_vector: vector(39),
                after_vector: vector(20),
                stable_subject_key: subject_key,
            },
        );
        assert!(matches!(
            ledger.apply(&wrong),
            Err(LedgerError::ExactVectorMismatch { .. })
        ));
        assert_eq!(
            ledger.charge(charge_id).expect("charge remains").vector(),
            vector(40)
        );
        let adjust = transition(
            &ledger,
            76,
            owner(11),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Adjust {
                charge_id,
                expected_generation: 0,
                before_vector: vector(40),
                after_vector: vector(20),
                stable_subject_key: subject_key,
            },
        );
        ledger.apply(&adjust).expect("adjustment succeeds");
        assert_eq!(
            ledger.charge(charge_id).expect("charge remains").vector(),
            vector(20)
        );
        assert_eq!(
            ledger
                .bucket(&branch_path(13))
                .expect("bucket exists")
                .committed(),
            vector(20)
        );

        let before_capacity_failure = ledger.clone();
        let over_capacity = transition(
            &ledger,
            78,
            owner(11),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Adjust {
                charge_id,
                expected_generation: 1,
                before_vector: vector(20),
                after_vector: vector(91),
                stable_subject_key: subject_key,
            },
        );
        assert!(matches!(
            ledger.apply(&over_capacity),
            Err(LedgerError::BucketRejected {
                cause: BucketStateError::OrdinaryCapacityExceeded { .. },
                ..
            })
        ));
        assert_eq!(ledger, before_capacity_failure);
    }

    #[test]
    fn retired_entry_ids_cannot_be_reused() {
        let mut ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([90; 32]);
        let reserve = transition(
            &ledger,
            91,
            owner(18),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve).expect("reserve succeeds");
        let release = transition(
            &ledger,
            92,
            owner(18),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Release {
                subject: LedgerSubject::Reservation(reservation_id),
                expected_generation: 0,
                exact_vector: vector(10),
            },
        );
        ledger.apply(&release).expect("release succeeds");
        let before_reservation_reuse = ledger.clone();
        let reservation_reuse = transition(
            &ledger,
            93,
            owner(18),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        assert_eq!(
            ledger.apply(&reservation_reuse),
            Err(LedgerError::ReservationIdAlreadyUsed { reservation_id })
        );
        assert_eq!(ledger, before_reservation_reuse);

        let second_reservation = ReservationId([94; 32]);
        let charge_id = ChargeId([95; 32]);
        let reserve_for_charge = transition(
            &ledger,
            96,
            owner(19),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: second_reservation,
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        ledger
            .apply(&reserve_for_charge)
            .expect("second reserve succeeds");
        let charge = transition(
            &ledger,
            97,
            owner(19),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id: second_reservation,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(10),
                stable_subject_key: StableSubjectKey([98; 32]),
            },
        );
        ledger.apply(&charge).expect("charge succeeds");
        let release_charge = transition(
            &ledger,
            99,
            owner(19),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Release {
                subject: LedgerSubject::Charge(charge_id),
                expected_generation: 0,
                exact_vector: vector(10),
            },
        );
        ledger
            .apply(&release_charge)
            .expect("charge release succeeds");

        let third_reservation = ReservationId([100; 32]);
        let reserve_for_reuse = transition(
            &ledger,
            101,
            owner(20),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: third_reservation,
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        ledger
            .apply(&reserve_for_reuse)
            .expect("third reserve succeeds");
        let before_charge_reuse = ledger.clone();
        let charge_reuse = transition(
            &ledger,
            102,
            owner(20),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id: third_reservation,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(10),
                stable_subject_key: StableSubjectKey([103; 32]),
            },
        );
        assert_eq!(
            ledger.apply(&charge_reuse),
            Err(LedgerError::ChargeIdAlreadyUsed { charge_id })
        );
        assert_eq!(ledger, before_charge_reuse);
    }

    #[test]
    fn charge_validates_its_source_before_disclosing_destination_collision() {
        let mut ledger = ledger_with_limit(100, 10);
        let first_reservation = ReservationId([104; 32]);
        let charge_id = ChargeId([105; 32]);
        let reserve_first = transition(
            &ledger,
            106,
            owner(21),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: first_reservation,
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve_first).expect("reserve succeeds");
        let first_charge = transition(
            &ledger,
            107,
            owner(21),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id: first_reservation,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(10),
                stable_subject_key: StableSubjectKey([108; 32]),
            },
        );
        ledger.apply(&first_charge).expect("charge succeeds");

        let second_reservation = ReservationId([109; 32]);
        let reserve_second = transition(
            &ledger,
            110,
            owner(22),
            branch_path(17),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: second_reservation,
                vector: vector(10),
                hold: OwnerHold::None,
            },
        );
        ledger.apply(&reserve_second).expect("reserve succeeds");
        let before = ledger.clone();
        let collision_with_wrong_source = transition(
            &ledger,
            111,
            owner(23),
            branch_path(17),
            ResourceClass::Ordinary,
            LedgerOperation::Charge {
                reservation_id: second_reservation,
                expected_reservation_generation: 0,
                charge_id,
                vector: vector(10),
                stable_subject_key: StableSubjectKey([112; 32]),
            },
        );
        assert_eq!(
            ledger.apply(&collision_with_wrong_source),
            Err(LedgerError::OwnerMismatch)
        );
        assert_eq!(ledger, before);
    }

    #[test]
    fn generation_overflow_and_nonempty_genesis_fail_without_mutation() {
        let nonempty = BucketState::try_new(
            capacity(100),
            capacity(10),
            vector(1),
            DurableChargeVector::ZERO,
            DurableChargeVector::ZERO,
        )
        .expect("nonempty bucket is capacity-valid");
        assert_eq!(
            ResourceLedger::try_new(identity(), [(db_path(), nonempty)]),
            Err(LedgerError::NonemptyGenesisBucket { path: db_path() })
        );

        let mut ledger = ledger_with_limit(100, 10);
        ledger.generation = u64::MAX;
        let before = ledger.clone();
        let reserve = transition(
            &ledger,
            113,
            owner(24),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: ReservationId([114; 32]),
                vector: vector(1),
                hold: OwnerHold::None,
            },
        );
        assert_eq!(
            ledger.apply(&reserve),
            Err(LedgerError::LedgerGenerationOverflow)
        );
        assert_eq!(ledger, before);

        let mut entry_ledger = ledger_with_limit(100, 10);
        let reservation_id = ReservationId([115; 32]);
        let reserve_entry = transition(
            &entry_ledger,
            116,
            owner(25),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id,
                vector: vector(1),
                hold: OwnerHold::None,
            },
        );
        entry_ledger
            .apply(&reserve_entry)
            .expect("reserve succeeds");
        entry_ledger
            .reservations
            .get_mut(&reservation_id)
            .expect("reservation exists")
            .generation = u64::MAX;
        let before_entry_overflow = entry_ledger.clone();
        let transfer = transition(
            &entry_ledger,
            117,
            owner(25),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Transfer {
                subject: LedgerSubject::Reservation(reservation_id),
                expected_generation: u64::MAX,
                target_owner: owner(26),
                target_path: branch_path(17),
                exact_conserved_vector: vector(1),
            },
        );
        assert_eq!(
            entry_ledger.apply(&transfer),
            Err(LedgerError::EntryGenerationOverflow)
        );
        assert_eq!(entry_ledger, before_entry_overflow);
    }

    #[test]
    fn role_generation_vector_and_constructor_failures_are_typed() {
        assert!(matches!(
            BucketState::empty(capacity(10), capacity(11)),
            Err(BucketStateError::ReserveExceedsHardLimit { .. })
        ));
        assert!(matches!(
            vector(u64::MAX).checked_add(vector(1)),
            Err(DurableVectorError::Overflow {
                axis: DurableChargeAxis::CanonicalDurableBytes,
                ..
            })
        ));
        let mut ledger = ledger_with_limit(100, 10);
        let mut tx = transition(
            &ledger,
            80,
            owner(12),
            branch_path(13),
            ResourceClass::Ordinary,
            LedgerOperation::Reserve {
                reservation_id: ReservationId([81; 32]),
                vector: vector(1),
                hold: OwnerHold::None,
            },
        );
        tx.header.expected_ledger_identity.role = ResourceAccountingRole::Meta;
        assert!(matches!(
            ledger.apply(&tx),
            Err(LedgerError::WrongAuthorityRole { .. })
        ));
        tx.header.expected_ledger_identity = ledger.identity();
        tx.header.expected_ledger_identity.limit_policy_epoch += 1;
        assert!(matches!(
            ledger.apply(&tx),
            Err(LedgerError::WrongLedgerIdentity { .. })
        ));
        tx.header.expected_ledger_identity = ledger.identity();
        tx.header.expected_ledger_generation = 1;
        assert!(matches!(
            ledger.apply(&tx),
            Err(LedgerError::StaleLedgerGeneration { .. })
        ));
    }
}
