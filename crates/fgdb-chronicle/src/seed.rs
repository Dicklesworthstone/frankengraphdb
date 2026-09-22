//! Replica-seeding publication gate for an exact authenticated snapshot closure.
//!
//! This is the owned transition layer, not a new snapshot format or file store.
//! The canonical snapshot/closure verifier supplies the immutable inventory and
//! exact cut. ATP supplies VerifiedObject values. The runtime durably publishes
//! each object's protected closure, then the exact destination root, using the
//! existing Chronicle barriers and an exclusive destination writer fence.
//! Verification, durable object ownership, root installation and serving/voting
//! authority are deliberately four different events. This module supplies the
//! middle two gates; it never grants configuration membership or read access.
//! `ReplicaSeed::begin_pull` joins manifest-bound bonded ATP recovery directly
//! to these publication gates without creating a second durability discipline.

use crate::identity::{CryptoVerificationSink, EncodedObject};
use crate::store::RootPublicationEvidence;
use crate::symbolize::RecoveryTarget;
use crate::transfer::{BondedPull, DonorId, PullError, PullLimits, PullRequest, SymbolAdmission, VerifiedObject};
use fgdb_crypto::Digest;
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Coordinates extracted from the authenticated snapshot and destination
/// publication plan. These are NOT a competing durable or wire record schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedAnchor {
    pub namespace: DatabaseSecurityNamespaceId,
    pub consensus_domain: [u8; 32],
    pub configuration: [u8; 32],
    pub snapshot_manifest: ObjectId,
    pub state_root: ObjectId,
    pub retention_floor: ObjectId,
    pub publication_root: ObjectId,
    pub publication_generation: u64,
    pub raft_index: u64,
    pub raft_term: u64,
    pub logical_command_seq: u64,
    pub commit_seq: u64,
}

/// Expected logical object facts from the authenticated transitive closure.
/// Encoding/placement are intentionally absent: replicas may recode the same
/// logical object without changing its meaning or its complete ObjectId.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedObjectSpec {
    pub object_id: ObjectId,
    pub object_kind: u16,
    pub compressed_len: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct SeedLimits {
    pub max_objects: usize,
    pub max_object_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for SeedLimits {
    fn default() -> Self {
        Self {
            max_objects: 1_000_000,
            max_object_bytes: 64 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeedError {
    InvalidLimits,
    InvalidAnchor,
    DuplicateObject,
    MissingRoot,
    ObjectCountBudget,
    ObjectSizeBudget,
    TotalSizeBudget,
    WrongNamespace,
    UnexpectedObject,
    KindMismatch,
    LengthMismatch,
    AlreadyPublished,
    AwaitingObjectPublication,
    MissingObjects { remaining: usize },
    InstallPending,
    StalePublication,
    RootEvidenceMismatch,
    GenerationExhausted,
    RecoveryRequired,
    Closed,
}

impl core::fmt::Display for SeedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis replica seed: {self:?}")
    }
}

impl core::error::Error for SeedError {}

#[derive(Clone, Debug)]
pub struct SeedPlan {
    anchor: SeedAnchor,
    inventory: BTreeMap<[u8; 32], SeedObjectSpec>,
    total_bytes: u64,
}

impl SeedPlan {
    /// The caller MUST validate canonical root formats, reference reachability,
    /// namespace/role/configuration binding, the exact snapshot cut, retention
    /// floors and destination generation before calling this constructor.
    /// A donor's claimed list or a self-consistent hash is not that authority.
    /// This function checks resource/structural constraints and freezes that
    /// verifier output; it does not pretend to decode the snapshot formats.
    pub fn from_authenticated_inventory(
        anchor: SeedAnchor,
        objects: impl IntoIterator<Item = SeedObjectSpec>,
        limits: SeedLimits,
    ) -> Result<Self, SeedError> {
        if limits.max_objects == 0
            || limits.max_object_bytes == 0
            || limits.max_total_bytes == 0
        {
            return Err(SeedError::InvalidLimits);
        }
        if anchor.publication_generation == 0
            || (anchor.raft_index == 0) != (anchor.raft_term == 0)
        {
            return Err(SeedError::InvalidAnchor);
        }
        let mut inventory = BTreeMap::new();
        let mut total_bytes = 0_u64;
        for object in objects {
            if inventory.len() >= limits.max_objects {
                return Err(SeedError::ObjectCountBudget);
            }
            if object.compressed_len > limits.max_object_bytes {
                return Err(SeedError::ObjectSizeBudget);
            }
            total_bytes = total_bytes
                .checked_add(object.compressed_len)
                .filter(|bytes| *bytes <= limits.max_total_bytes)
                .ok_or(SeedError::TotalSizeBudget)?;
            if inventory.insert(object.object_id.0, object).is_some() {
                return Err(SeedError::DuplicateObject);
            }
        }
        for root in [
            anchor.snapshot_manifest,
            anchor.state_root,
            anchor.retention_floor,
            anchor.publication_root,
        ] {
            if !inventory.contains_key(&root.0) {
                return Err(SeedError::MissingRoot);
            }
        }
        Ok(Self { anchor, inventory, total_bytes })
    }

    pub fn anchor(&self) -> &SeedAnchor {
        &self.anchor
    }

    pub fn objects(&self) -> impl Iterator<Item = &SeedObjectSpec> {
        self.inventory.values()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationKind {
    Object,
    Install,
}

/// Acknowledgement scoped to one session, generation and publication kind.
/// It is intentionally not serializable or an authorization certificate.
#[derive(Clone, Debug)]
pub struct SeedPublicationId {
    session: Arc<()>,
    serial: u64,
    kind: PublicationKind,
}

impl PartialEq for SeedPublicationId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session)
            && self.serial == other.serial
            && self.kind == other.kind
    }
}

impl Eq for SeedPublicationId {}

pub struct ObjectPublication<'a> {
    id: SeedPublicationId,
    object: &'a VerifiedObject,
}

impl ObjectPublication<'_> {
    pub fn id(&self) -> SeedPublicationId {
        self.id.clone()
    }

    /// Publish the complete protected/encoded object closure, not plaintext in
    /// a side file. The existing Chronicle pipeline owns its durable format.
    pub fn object(&self) -> &VerifiedObject {
        self.object
    }
}

pub struct SeedInstallation<'a> {
    id: SeedPublicationId,
    plan: &'a SeedPlan,
    encodings: &'a BTreeMap<[u8; 32], Digest>,
}

impl SeedInstallation<'_> {
    pub fn id(&self) -> SeedPublicationId {
        self.id.clone()
    }

    pub fn plan(&self) -> &SeedPlan {
        self.plan
    }

    /// Verified incoming encodings, not a replacement for durable placement
    /// descriptors. Storage must publish valid local placement/ownership roots.
    pub fn incoming_encodings(&self) -> impl Iterator<Item = (ObjectId, Digest)> + '_ {
        self.encodings.iter().map(|(oid, encoding)| (ObjectId(*oid), *encoding))
    }
}

/// The exact locally installed seed. This is not a Raft membership change,
/// payload-availability certificate, learner promotion or read authorization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedCompletion {
    anchor: SeedAnchor,
    object_count: usize,
    bytes: u64,
}

impl SeedCompletion {
    pub fn anchor(&self) -> &SeedAnchor {
        &self.anchor
    }

    pub fn object_count(&self) -> usize {
        self.object_count
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

pub struct ReplicaSeed {
    plan: SeedPlan,
    session: Arc<()>,
    serial: u64,
    published: BTreeMap<[u8; 32], Digest>,
    pending_object: Option<(SeedPublicationId, VerifiedObject)>,
    installing: Option<SeedPublicationId>,
    completion: Option<SeedCompletion>,
    poisoned: bool,
}

impl ReplicaSeed {
    pub fn new(plan: SeedPlan) -> Self {
        Self {
            plan,
            session: Arc::new(()),
            serial: 0,
            published: BTreeMap::new(),
            pending_object: None,
            installing: None,
            completion: None,
            poisoned: false,
        }
    }

    pub fn plan(&self) -> &SeedPlan {
        &self.plan
    }

    pub fn missing_objects(&self) -> impl Iterator<Item = &SeedObjectSpec> {
        self.plan.inventory.iter().filter_map(|(oid, spec)| {
            (!self.published.contains_key(oid)).then_some(spec)
        })
    }

    pub fn published_count(&self) -> usize {
        self.published.len()
    }

    pub fn completion(&self) -> Option<&SeedCompletion> {
        self.completion.as_ref()
    }

    fn open(&self) -> Result<(), SeedError> {
        if self.poisoned {
            Err(SeedError::RecoveryRequired)
        } else if self.completion.is_some() {
            Err(SeedError::Closed)
        } else {
            Ok(())
        }
    }

    fn next_id(&mut self, kind: PublicationKind) -> Result<SeedPublicationId, SeedError> {
        let serial = self.serial.checked_add(1).ok_or(SeedError::GenerationExhausted)?;
        self.serial = serial;
        Ok(SeedPublicationId { session: Arc::clone(&self.session), serial, kind })
    }

    /// Admit one inventory-bound bonded pull before emitting any ATP requests.
    /// The exclusive borrow bounds this seed to one active object pull or object
    /// publication. Donors run concurrently within that pull; their failures do
    /// not discard authenticated symbols supplied by surviving streams.
    ///
    /// Descriptor/target/key material must come from the authenticated closure.
    /// The transport must recheck donor authorization against this plan's exact
    /// configuration and current writer fences; donor IDs are not certificates.
    /// Hold the returned owner across cancellable I/O futures to resume progress.
    pub fn begin_pull<'seed, 'data>(
        &'seed mut self,
        encoding: &'data EncodedObject,
        target: RecoveryTarget<'data>,
        dek: &'data [u8; 32],
        donors: &[DonorId],
        limits: PullLimits,
    ) -> Result<SeedObjectPull<'seed, 'data>, SeedPullError> {
        self.open()?;
        if self.installing.is_some() {
            return Err(SeedError::InstallPending.into());
        }
        if self.pending_object.is_some() {
            return Err(SeedError::AwaitingObjectPublication.into());
        }
        if target.namespace != self.plan.anchor.namespace {
            return Err(SeedError::WrongNamespace.into());
        }
        let oid = encoding.object_id();
        if target.object_id != oid {
            return Err(PullError::InvalidTarget.into());
        }
        let spec = self.plan.inventory.get(&oid.0).ok_or(SeedError::UnexpectedObject)?;
        if self.published.contains_key(&oid.0) {
            return Err(SeedError::AlreadyPublished.into());
        }
        if encoding.cipher_descriptor().object_kind != spec.object_kind {
            return Err(SeedError::KindMismatch.into());
        }
        if encoding.cipher_descriptor().compressed_len != spec.compressed_len {
            return Err(SeedError::LengthMismatch.into());
        }
        let pull = BondedPull::new(encoding, target, dek, donors, limits)?;
        Ok(SeedObjectPull { seed: self, pull: Some(pull) })
    }

    /// Stage one cryptographically verified object without marking it durable.
    /// Only one object is retained at a time, bounded by the admitted inventory.
    pub fn stage(&mut self, object: VerifiedObject) -> Result<ObjectPublication<'_>, SeedError> {
        self.open()?;
        if self.installing.is_some() {
            return Err(SeedError::InstallPending);
        }
        if self.pending_object.is_some() {
            return Err(SeedError::AwaitingObjectPublication);
        }
        if object.namespace() != self.plan.anchor.namespace {
            return Err(SeedError::WrongNamespace);
        }
        let oid = object.object_id();
        let spec = self.plan.inventory.get(&oid.0).ok_or(SeedError::UnexpectedObject)?;
        if self.published.contains_key(&oid.0) {
            return Err(SeedError::AlreadyPublished);
        }
        if object.encoding().cipher_descriptor().object_kind != spec.object_kind {
            return Err(SeedError::KindMismatch);
        }
        if u64::try_from(object.plaintext().len()).ok() != Some(spec.compressed_len) {
            return Err(SeedError::LengthMismatch);
        }
        let id = self.next_id(PublicationKind::Object)?;
        self.pending_object = Some((id, object));
        self.pending_publication()
    }

    /// Reacquire the same pending view after cancellation. Dropping a view does
    /// not acknowledge it and cannot advance missing/published object counts.
    pub fn pending_publication(&self) -> Result<ObjectPublication<'_>, SeedError> {
        self.open()?;
        let (id, object) = self.pending_object.as_ref().ok_or(SeedError::StalePublication)?;
        Ok(ObjectPublication { id: id.clone(), object })
    }

    /// Call only after all required object/placement/ownership publication
    /// barriers complete. This method is not evidence that storage was called.
    pub fn object_published(&mut self, id: SeedPublicationId) -> Result<ObjectId, SeedError> {
        self.open()?;
        let (expected, object) = self.pending_object.as_ref().ok_or(SeedError::StalePublication)?;
        if expected != &id {
            return Err(SeedError::StalePublication);
        }
        let oid = object.object_id();
        let encoding_id = object.encoding().encoding_id();
        self.published.insert(oid.0, encoding_id);
        self.pending_object = None;
        Ok(oid)
    }

    pub fn begin_install(&mut self) -> Result<SeedInstallation<'_>, SeedError> {
        self.open()?;
        if self.pending_object.is_some() {
            return Err(SeedError::AwaitingObjectPublication);
        }
        let remaining = self.plan.inventory.len() - self.published.len();
        if remaining != 0 {
            return Err(SeedError::MissingObjects { remaining });
        }
        if self.installing.is_none() {
            let id = self.next_id(PublicationKind::Install)?;
            self.installing = Some(id);
        }
        let id = self.installing.as_ref().ok_or(SeedError::StalePublication)?.clone();
        Ok(SeedInstallation { id, plan: &self.plan, encodings: &self.published })
    }

    /// Complete only for the exact post-sync root reread evidence. The caller
    /// must obtain it from RootStore under the destination's writer fence; the
    /// evidence type is Chronicle's current unsigned local subset, not a signed
    /// distributed receipt. A mismatch leaves this installation pending.
    pub fn finish_install(
        &mut self,
        id: SeedPublicationId,
        evidence: &RootPublicationEvidence,
    ) -> Result<&SeedCompletion, SeedError> {
        self.open()?;
        if self.installing.as_ref() != Some(&id) {
            return Err(SeedError::StalePublication);
        }
        if evidence.written_index > 1
            || evidence.slot_generation != self.plan.anchor.publication_generation
            || evidence.root_manifest_oid != self.plan.anchor.publication_root.0
        {
            return Err(SeedError::RootEvidenceMismatch);
        }
        self.completion = Some(SeedCompletion {
            anchor: self.plan.anchor.clone(),
            object_count: self.published.len(),
            bytes: self.plan.total_bytes,
        });
        self.installing = None;
        self.completion.as_ref().ok_or(SeedError::StalePublication)
    }

    /// Unknown/failed publication is not rollback. Reopen the destination,
    /// authenticate its durable root and construct a new seeding session.
    pub fn publication_failed(&mut self) {
        if self.completion.is_none() {
            self.poisoned = true;
        }
    }
}

/// Preserve the distinction between closure/publication failures and transport,
/// authentication, decoding or per-object budget failures.
#[derive(Debug)]
pub enum SeedPullError {
    Seed(SeedError),
    Pull(PullError),
}

impl From<SeedError> for SeedPullError {
    fn from(error: SeedError) -> Self {
        Self::Seed(error)
    }
}

impl From<PullError> for SeedPullError {
    fn from(error: PullError) -> Self {
        Self::Pull(error)
    }
}

impl core::fmt::Display for SeedPullError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Seed(error) => core::fmt::Display::fmt(error, f),
            Self::Pull(error) => core::fmt::Display::fmt(error, f),
        }
    }
}

impl core::error::Error for SeedPullError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Seed(error) => Some(error),
            Self::Pull(error) => Some(error),
        }
    }
}

/// One admitted object transfer joined to its seed's publication gate.
///
/// Keep this owner while driving its bounded requests over authenticated ATP.
/// Once try_stage returns a publication, its bytes are owned by ReplicaSeed,
/// not by this handle. Dropping a publication view or this handle cannot mark
/// them durable or discard that pending publication. Decoder/wire buffers are
/// released immediately at staging, before storage begins publishing the object.
pub struct SeedObjectPull<'seed, 'data> {
    seed: &'seed mut ReplicaSeed,
    pull: Option<BondedPull<'data>>,
}

impl core::fmt::Debug for SeedObjectPull<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SeedObjectPull")
            .field("pull", &self.pull)
            .field("awaiting_publication", &self.seed.pending_object.is_some())
            .finish()
    }
}

impl<'seed, 'data> SeedObjectPull<'seed, 'data> {
    fn pulling(&mut self) -> Result<&mut BondedPull<'data>, SeedPullError> {
        self.seed.open()?;
        if self.seed.pending_object.is_some() {
            return Err(SeedError::AwaitingObjectPublication.into());
        }
        self.pull.as_mut().ok_or_else(|| PullError::Closed.into())
    }

    pub fn pending_count(&self) -> usize {
        self.pull.as_ref().map_or(0, BondedPull::pending_count)
    }

    pub fn schedule(&mut self, maximum: usize) -> Result<Vec<PullRequest>, SeedPullError> {
        Ok(self.pulling()?.schedule(maximum)?)
    }

    pub fn accept(
        &mut self,
        donor: DonorId,
        bytes: &[u8],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<SymbolAdmission, SeedPullError> {
        Ok(self.pulling()?.accept(donor, bytes, verification)?)
    }

    pub fn expire(&mut self, request: PullRequest) -> Result<(), SeedPullError> {
        Ok(self.pulling()?.expire(request)?)
    }

    pub fn donor_failed(&mut self, donor: DonorId) -> Result<(), SeedPullError> {
        Ok(self.pulling()?.donor_failed(donor)?)
    }

    /// The caller must freshly validate this donor's fenced authorization;
    /// restoring availability does not itself grant authority to serve bytes.
    pub fn donor_available(&mut self, donor: DonorId) -> Result<(), SeedPullError> {
        Ok(self.pulling()?.donor_available(donor)?)
    }

    /// Recover through the real symbol-MAC/FEC/AEAD/ObjectId verifier, then stage
    /// exactly once. Repeated calls after staging return the same pending view,
    /// without decoding again or advancing the durable inventory.
    pub fn try_stage(
        &mut self,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Option<ObjectPublication<'_>>, SeedPullError> {
        self.seed.open()?;
        if self.seed.pending_object.is_some() {
            return Ok(Some(self.seed.pending_publication()?));
        }
        let Some(object) = self.pulling()?.try_recover(verification)? else {
            return Ok(None);
        };
        self.pull = None;
        Ok(Some(self.seed.stage(object)?))
    }

    /// Consume this pull after the runtime completes every required object
    /// ownership/placement publication barrier. A stale ID leaves the seed's
    /// pending publication intact for recovery through pending_publication.
    pub fn object_published(self, id: SeedPublicationId) -> Result<ObjectId, SeedPullError> {
        Ok(self.seed.object_published(id)?)
    }

    /// An unknown storage-publication outcome poisons the entire seed, not just
    /// the transport attempt. Reopen the durable destination under its fence.
    pub fn publication_failed(self) {
        self.seed.publication_failed();
    }
}
