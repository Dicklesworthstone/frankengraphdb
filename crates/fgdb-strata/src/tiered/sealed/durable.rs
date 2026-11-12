//! Registered FGRN adjacency runs over the existing canonical sealed codec.
//!
//! Profile 1 is one forward relation in one admitted source lineage. Source
//! identities remain stable VIds; the image's destination ordinals belong to
//! its embedded, digest-bound dictionary only. Retained half-open intervals
//! are meaningful under the exact source root and [floor, publication] cut.
//! No external ordinal map or branch import is accepted by this profile.
//!
//! The stable-EId origin column comes from authenticated Chronicle creation
//! effects. Content-version starts are checked against it, never substituted
//! for it. Identity slots use the same registered prefix/FOR column framing
//! as FGSI; the three origin-order scalars use canonical unsigned LEB128.
//! The stable digest covers decoded identities, origin orders, visibility and
//! values independently of the image codec. A checksum covers the complete
//! frame, and the ordinary keyed object identity binds it to its root ref.
//!
//! Only the store's authenticated root attachment may deserialize this image
//! into a source anchor. A standalone FGRN byte string is not graph authority.

use super::image::{RowScan, identity_limits, reserved};
use super::{
    OriginBirthOrder, SealedAnchor, SealedError, SealedLimits, SealedPartition, SealedScope,
    check_limit,
};
use crate::PartitionRootVersion;
use fgdb_codec::identity::{IdentityColumn, IdentityColumnDescriptor};
use fgdb_codec::varint::{decode_u64_prefix, encode_u64};
use fgdb_delta_types::RelationId;
use fgdb_types::{
    BranchId, CanonicalPropertyValueError, CanonicalScalarResolver, CommitSeq,
    DatabaseSecurityNamespaceId, EId, GraphId, LogicalObjectKind, LogicalObjectKindCode, ObjectId,
    QueryCx,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const ADJ_RUN_OBJECT_KIND: u16 = 0x058d;
pub const MAX_ADJ_RUN_BYTES: usize = 256 * 1024 * 1024;
const MAGIC: &[u8; 4] = b"FGRN";
const FORMAT: u16 = 1;
pub const FORMAT_V2: u16 = 2;
const PROFILE: u16 = 1;
const CHECKSUM_BYTES: usize = 32;
const CHECKSUM_DOMAIN: &[u8] = b"fgdb.strata.adj-run-checksum.v1";
const STABLE_DOMAIN: &[u8] = b"fgdb.strata.adj-run-stable-digest.v1";
const SOURCE_MAP_DOMAIN: &[u8] = b"fgdb.strata.adj-run-source-map.v1";
const DESTINATION_MAP_DOMAIN: &[u8] = b"fgdb.strata.adj-run-destination-map.v1";

/// Content identity of one registered immutable adjacency run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct AdjRunSegmentVersion(pub ObjectId);

impl LogicalObjectKind for AdjRunSegmentVersion {
    const OBJECT_KIND: LogicalObjectKindCode = LogicalObjectKindCode::AdjRunSegment;
}

/// Authenticated run metadata. Map IDs bind the exact embedded projection;
/// they are not permission to use an ordinal from any other run or lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjRunHeader {
    pub scope: SealedScope,
    pub relation: RelationId,
    pub row_count: usize,
    pub incidence_count: usize,
    pub origin_count: usize,
    pub source_map_id: [u8; 32],
    pub destination_map_id: [u8; 32],
    pub canonical_stable_id_digest: [u8; 32],
}

#[derive(Debug)]
pub enum AdjRunError {
    Sealed(SealedError),
    PropertyValue(CanonicalPropertyValueError),
    NotARun,
    UnsupportedFormat(u16),
    UnsupportedProfile(u16),
    UnsupportedDirection(u8),
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    ChecksumMismatch,
    ScopeMismatch,
    RelationRequired,
    MissingOrigin(EId),
    InvalidOrigin(EId),
    OriginSetMismatch,
    MapMismatch,
    StableDigestMismatch,
    NonCanonical,
}

impl From<SealedError> for AdjRunError {
    fn from(error: SealedError) -> Self {
        Self::Sealed(error)
    }
}

impl core::fmt::Display for AdjRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sealed(error) => error.fmt(f),
            Self::PropertyValue(error) => error.fmt(f),
            other => write!(f, "adjacency run: {other:?}"),
        }
    }
}

impl std::error::Error for AdjRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sealed(error) => Some(error),
            Self::PropertyValue(error) => Some(error),
            _ => None,
        }
    }
}

/// A fully admitted compressed run with immutable origins. Cloning shares the
/// image and sidecar. Only `from_sealed` or an authenticated store reopen may
/// construct one; raw headers never manufacture a `SealedAnchor`.
#[derive(Clone, Debug)]
pub struct AdjRunSegment {
    header: AdjRunHeader,
    partition: SealedPartition,
    origins: Arc<[(EId, OriginBirthOrder<EId>)]>,
    origin_identity: Arc<IdentityColumn<EId>>,
}

fn checkpoint(cx: &QueryCx) -> Result<(), SealedError> {
    cx.checkpoint().map_err(SealedError::Interrupted)
}

fn hash_chunks(cx: &QueryCx, domain: &[u8], bytes: &[u8]) -> Result<[u8; 32], AdjRunError> {
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(domain);
    for chunk in bytes.chunks(4096) {
        checkpoint(cx)?;
        hash.update(chunk);
    }
    Ok(hash.finalize().0)
}

fn map_hasher(domain: &[u8], scope: SealedScope, relation: RelationId) -> fgdb_crypto::Hasher {
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(domain);
    hash.update(&scope.source_root.0.0);
    hash.update(&scope.graph.0.to_be_bytes());
    hash.update(&scope.branch.0.to_be_bytes());
    hash.update(&scope.partition.to_be_bytes());
    hash.update(&scope.floor.0.to_be_bytes());
    hash.update(&scope.publication.0.to_be_bytes());
    hash.update(&relation.0.to_be_bytes());
    hash
}

fn maps(
    cx: &QueryCx,
    partition: &SealedPartition,
    relation: RelationId,
) -> Result<([u8; 32], [u8; 32]), AdjRunError> {
    let scope = partition.anchor.scope;
    let mut sources = map_hasher(SOURCE_MAP_DOMAIN, scope, relation);
    sources.update(&(partition.image.rows.len() as u64).to_be_bytes());
    for row in &partition.image.rows {
        checkpoint(cx)?;
        sources.update(&row.key.src.0.to_be_bytes());
    }
    let mut destinations = map_hasher(DESTINATION_MAP_DOMAIN, scope, relation);
    destinations.update(&(partition.image.dictionary.len() as u64).to_be_bytes());
    for at in 0..partition.image.dictionary.len() {
        if at % 128 == 0 {
            checkpoint(cx)?;
        }
        let id = partition
            .image
            .dictionary
            .get(at)
            .ok_or(AdjRunError::NonCanonical)?;
        destinations.update(&id.0.to_be_bytes());
    }
    Ok((sources.finalize().0, destinations.finalize().0))
}

fn origin_in(origins: &[(EId, OriginBirthOrder<EId>)], eid: EId) -> Option<OriginBirthOrder<EId>> {
    origins
        .binary_search_by_key(&eid, |(id, _)| *id)
        .ok()
        .map(|at| origins[at].1)
}

fn stable_digest(
    cx: &QueryCx,
    partition: &SealedPartition,
    relation: RelationId,
    origins: &[(EId, OriginBirthOrder<EId>)],
) -> Result<[u8; 32], AdjRunError> {
    let scope = partition.anchor.scope;
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(STABLE_DOMAIN);
    hash.update(&scope.graph.0.to_be_bytes());
    hash.update(&scope.branch.0.to_be_bytes());
    hash.update(&scope.partition.to_be_bytes());
    hash.update(&relation.0.to_be_bytes());
    hash.update(&scope.floor.0.to_be_bytes());
    hash.update(&scope.publication.0.to_be_bytes());
    hash.update(&(partition.image.incidences as u64).to_be_bytes());
    for row in &partition.image.rows {
        checkpoint(cx)?;
        let mut cursor = RowScan::new(&partition.image, row, 0);
        while let Some((entry, locator)) = cursor.next() {
            if cursor.position.is_multiple_of(128) {
                checkpoint(cx)?;
            }
            let origin =
                origin_in(origins, entry.eid).ok_or(AdjRunError::MissingOrigin(entry.eid))?;
            if origin.element_id() != entry.eid
                || origin.commit_seq() == CommitSeq::ORIGIN
                || origin.commit_seq() > entry.created_at
            {
                return Err(AdjRunError::InvalidOrigin(entry.eid));
            }
            hash.update(&entry.src.0.to_be_bytes());
            hash.update(&entry.relation.0.to_be_bytes());
            hash.update(&entry.dst.0.to_be_bytes());
            hash.update(&entry.eid.0.to_be_bytes());
            hash.update(&entry.created_at.0.to_be_bytes());
            hash.update(&entry.retired_at.map_or(0, |seq| seq.0).to_be_bytes());
            hash.update(&origin.canonical_be_key());
            let properties = if locator == 0 {
                &[][..]
            } else {
                partition
                    .image
                    .properties
                    .get(locator as usize - 1)
                    .ok_or(AdjRunError::NonCanonical)?
                    .as_slice()
            };
            hash.update(&(properties.len() as u64).to_be_bytes());
            for (key, value) in properties {
                checkpoint(cx)?;
                let bytes = value.encode().map_err(AdjRunError::PropertyValue)?;
                hash.update(&key.0.to_be_bytes());
                hash.update(&(bytes.len() as u64).to_be_bytes());
                for chunk in bytes.chunks(4096) {
                    checkpoint(cx)?;
                    hash.update(chunk);
                }
            }
        }
    }
    Ok(hash.finalize().0)
}

impl AdjRunSegment {
    /// Preserve an already admitted image and its actual immutable origins.
    /// `origins` may cover more relations; only identities retained by this
    /// image are selected. Every selected identity must have exactly one
    /// origin preceding all of its retained content versions.
    pub fn from_sealed(
        cx: &QueryCx,
        partition: &SealedPartition,
        origins: &BTreeMap<EId, OriginBirthOrder<EId>>,
        limits: SealedLimits,
    ) -> Result<Self, AdjRunError> {
        checkpoint(cx)?;
        let relation = partition
            .anchor
            .relation
            .ok_or(AdjRunError::RelationRequired)?;
        partition
            .image
            .validate(partition.anchor.scope, limits, &mut || checkpoint(cx))?;
        let ids = partition.stable_edge_ids(cx, limits.max_incidences)?;
        let mut selected = reserved(ids.len())?;
        for eid in ids {
            checkpoint(cx)?;
            let origin = origins
                .get(&eid)
                .copied()
                .ok_or(AdjRunError::MissingOrigin(eid))?;
            if origin.element_id() != eid {
                return Err(AdjRunError::InvalidOrigin(eid));
            }
            selected.push((eid, origin));
        }
        let (source_map_id, destination_map_id) = maps(cx, partition, relation)?;
        let canonical_stable_id_digest = stable_digest(cx, partition, relation, &selected)?;
        let mut identities = reserved(selected.len())?;
        identities.extend(selected.iter().map(|(eid, _)| *eid));
        let origin_identity =
            IdentityColumn::try_new(&identities, identity_limits(identities.len(), limits))
                .map_err(SealedError::Identity)?;
        let mut partition = partition.clone();
        if partition.durable_incoming().is_none() {
            let incoming = partition.incoming_index(
                cx,
                super::IncomingIndexLimits {
                    max_rows: limits.max_rows,
                    max_incidences: limits.max_incidences,
                    max_workspace_bytes: usize::MAX / 2,
                },
            )?;
            partition.set_durable_incoming(incoming.index_arc());
        }
        checkpoint(cx)?;
        Ok(Self {
            header: AdjRunHeader {
                scope: partition.anchor.scope,
                relation,
                row_count: partition.image.rows.len(),
                incidence_count: partition.image.incidences,
                origin_count: selected.len(),
                source_map_id,
                destination_map_id,
                canonical_stable_id_digest,
            },
            partition,
            origins: selected.into(),
            origin_identity: Arc::new(origin_identity),
        })
    }

    pub const fn header(&self) -> AdjRunHeader {
        self.header
    }

    pub fn partition(&self) -> &SealedPartition {
        &self.partition
    }

    pub fn into_partition(self) -> SealedPartition {
        self.partition
    }

    pub(crate) fn bind_envelope(
        &mut self,
        envelope_root: PartitionRootVersion,
        envelope_publication: CommitSeq,
        verified_through: CommitSeq,
    ) {
        self.partition.anchor.admitted_envelope =
            Some((envelope_root, envelope_publication, verified_through));
    }

    pub fn origin(&self, eid: EId) -> Option<OriginBirthOrder<EId>> {
        origin_in(&self.origins, eid)
    }

    /// Encode canonical FGRN-v1 bytes. The byte ceiling includes the envelope,
    /// embedded FGSI image, compressed identity column, origins and checksum.
    /// A complete encoded run is in memory; this is not a mapped-page writer.
    pub fn encode(&self, cx: &QueryCx, limits: SealedLimits) -> Result<Vec<u8>, AdjRunError> {
        self.encode_versioned(cx, limits, false)
    }

    /// Encode canonical FGRN-v2 dual-face (`E_out` + `E_in` Elias-Fano CSR)
    /// bytes so cold reload and `SealedGraphView` (`Reversed` / `Undirected`)
    /// serve incoming traversals directly without `O(|E|)` transpose rebuild.
    pub fn encode_dual_face(
        &self,
        cx: &QueryCx,
        limits: SealedLimits,
    ) -> Result<Vec<u8>, AdjRunError> {
        self.encode_versioned(cx, limits, true)
    }

    fn encode_versioned(
        &self,
        cx: &QueryCx,
        limits: SealedLimits,
        dual_face: bool,
    ) -> Result<Vec<u8>, AdjRunError> {
        checkpoint(cx)?;
        let max_bytes = limits.max_image_bytes.min(MAX_ADJ_RUN_BYTES);
        let image = self.partition.encode(cx, limits)?;
        let mut scalars = Output::new(max_bytes);
        for (_, origin) in self.origins.iter() {
            checkpoint(cx)?;
            for scalar in [
                origin.commit_seq().0,
                origin.intent_ordinal(),
                origin.merge_ordinal(),
            ] {
                scalars.put(encode_u64(scalar).as_bytes())?;
            }
        }
        let incoming_bytes = if dual_face {
            let owned;
            let index_ref = match self.partition.durable_incoming() {
                Some(idx) => idx,
                None => {
                    owned = self
                        .partition
                        .incoming_index(
                            cx,
                            super::IncomingIndexLimits {
                                max_rows: limits.max_rows,
                                max_incidences: limits.max_incidences,
                                max_workspace_bytes: usize::MAX / 2,
                            },
                        )?
                        .index_arc();
                    &owned
                }
            };
            Some(super::incoming::encode_durable_index(
                index_ref,
                self.header.relation,
                &self.partition.image,
                max_bytes,
                &mut || checkpoint(cx),
            )?)
        } else {
            None
        };
        let identity = &self.origin_identity;
        let descriptor = identity.descriptor();
        let mut out = Output::new(max_bytes);
        out.put(MAGIC)?;
        let fmt = if dual_face { FORMAT_V2 } else { FORMAT };
        out.put(&fmt.to_le_bytes())?;
        out.put(&PROFILE.to_le_bytes())?;
        let header = self.header;
        out.put(&header.scope.source_root.0.0)?;
        out.put(&header.scope.graph.0.to_le_bytes())?;
        out.put(&header.scope.branch.0.to_le_bytes())?;
        out.put(&header.scope.partition.to_le_bytes())?;
        out.put(&header.relation.0.to_le_bytes())?;
        out.put(&[crate::Direction::Outbound as u8])?;
        out.put(&header.scope.floor.0.to_le_bytes())?;
        out.put(&header.scope.publication.0.to_le_bytes())?;
        out.count(header.row_count)?;
        out.count(header.incidence_count)?;
        out.count(header.origin_count)?;
        out.put(&header.source_map_id)?;
        out.put(&header.destination_map_id)?;
        out.put(&header.canonical_stable_id_digest)?;
        out.put(&self.partition.anchor.fingerprint)?;
        out.count(image.len())?;
        out.put(&[crate::representation_tag(descriptor.representation())])?;
        out.count(descriptor.prefixes())?;
        out.count(identity.encoded_payload_len())?;
        out.count(scalars.bytes.len())?;
        for chunk in image.chunks(4096) {
            checkpoint(cx)?;
            out.put(chunk)?;
        }
        identity
            .try_write_scalar_payload(limits.max_image_bytes, |chunk| {
                checkpoint(cx)?;
                out.put(chunk)
            })
            .map_err(AdjRunError::from)?;
        for chunk in scalars.bytes.chunks(4096) {
            checkpoint(cx)?;
            out.put(chunk)?;
        }
        if let Some(incoming_bytes) = incoming_bytes {
            out.count(incoming_bytes.len())?;
            for chunk in incoming_bytes.chunks(4096) {
                checkpoint(cx)?;
                out.put(chunk)?;
            }
        }
        let checksum = hash_chunks(cx, CHECKSUM_DOMAIN, &out.bytes)?;
        out.put(&checksum)?;
        checkpoint(cx)?;
        Ok(out.bytes)
    }
}

/// Same namespace-keyed identity derivation as the existing Strata objects,
/// with AdjRunSegment's distinct registered kind.
pub fn adj_run_id(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
) -> ObjectId {
    ObjectId(
        fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &ADJ_RUN_OBJECT_KIND.to_le_bytes(),
            bytes,
        )
        .0,
    )
}

struct Output {
    bytes: Vec<u8>,
    limit: usize,
}

impl Output {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), SealedError> {
        let size = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("durable run bytes", size, self.limit)?;
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| SealedError::AllocationFailed)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn count(&mut self, count: usize) -> Result<(), SealedError> {
        self.put(
            &u32::try_from(count)
                .map_err(|_| SealedError::SizeOverflow)?
                .to_le_bytes(),
        )
    }
}

struct Input<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], SealedError> {
        let end = self
            .at
            .checked_add(count)
            .ok_or(SealedError::SizeOverflow)?;
        let bytes = self.bytes.get(self.at..end).ok_or(SealedError::Truncated)?;
        self.at = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], SealedError> {
        self.take(N)?.try_into().map_err(|_| SealedError::Truncated)
    }

    fn byte(&mut self) -> Result<u8, SealedError> {
        Ok(self.take(1)?[0])
    }

    fn count(&mut self) -> Result<usize, SealedError> {
        Ok(u32::from_le_bytes(self.array()?) as usize)
    }

    fn u64(&mut self) -> Result<u64, SealedError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn varint(&mut self) -> Result<u64, AdjRunError> {
        let (value, count) =
            decode_u64_prefix(&self.bytes[self.at..]).map_err(|_| AdjRunError::NonCanonical)?;
        self.take(count)?;
        Ok(value)
    }
}

/// Store-only admission: the selected root supplies the expected lineage,
/// relation and content identity. Verify integrity before interpreting counts
/// or minting an anchor; decode then independently checks every column.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_adj_run(
    cx: &QueryCx,
    expected: ObjectId,
    expected_scope: SealedScope,
    relation: RelationId,
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    limits: SealedLimits,
    resolver: Option<&dyn CanonicalScalarResolver>,
) -> Result<AdjRunSegment, AdjRunError> {
    checkpoint(cx)?;
    check_limit(
        "durable run bytes",
        bytes.len(),
        limits.max_image_bytes.min(MAX_ADJ_RUN_BYTES),
    )?;
    let actual = adj_run_id(k_oid, namespace, bytes);
    if actual != expected {
        return Err(AdjRunError::IdentityMismatch { expected, actual });
    }
    decode(cx, expected_scope, relation, bytes, limits, resolver)
}

fn decode(
    cx: &QueryCx,
    expected_scope: SealedScope,
    relation: RelationId,
    bytes: &[u8],
    limits: SealedLimits,
    resolver: Option<&dyn CanonicalScalarResolver>,
) -> Result<AdjRunSegment, AdjRunError> {
    let end = bytes
        .len()
        .checked_sub(CHECKSUM_BYTES)
        .ok_or(SealedError::Truncated)?;
    let (body, checksum) = bytes.split_at(end);
    if hash_chunks(cx, CHECKSUM_DOMAIN, body)? != checksum {
        return Err(AdjRunError::ChecksumMismatch);
    }
    let mut input = Input { bytes: body, at: 0 };
    if input.take(4)? != MAGIC {
        return Err(AdjRunError::NotARun);
    }
    let version = u16::from_le_bytes(input.array()?);
    if version != FORMAT && version != FORMAT_V2 {
        return Err(AdjRunError::UnsupportedFormat(version));
    }
    let profile = u16::from_le_bytes(input.array()?);
    if profile != PROFILE {
        return Err(AdjRunError::UnsupportedProfile(profile));
    }
    let source_root = PartitionRootVersion(ObjectId(input.array()?));
    let graph = GraphId(u128::from_le_bytes(input.array()?));
    let branch = BranchId(u128::from_le_bytes(input.array()?));
    let partition_id = input.u64()?;
    let stored_relation = RelationId(input.u64()?);
    let direction = input.byte()?;
    if direction != crate::Direction::Outbound as u8 {
        return Err(AdjRunError::UnsupportedDirection(direction));
    }
    let scope = SealedScope {
        source_root,
        graph,
        branch,
        partition: partition_id,
        floor: CommitSeq(input.u64()?),
        publication: CommitSeq(input.u64()?),
    };
    if scope != expected_scope || relation != stored_relation {
        return Err(AdjRunError::ScopeMismatch);
    }
    let row_count = input.count()?;
    let incidence_count = input.count()?;
    let origin_count = input.count()?;
    check_limit("descriptors", row_count, limits.max_rows)?;
    check_limit("incidences", incidence_count, limits.max_incidences)?;
    check_limit("origins", origin_count, limits.max_incidences)?;
    if row_count > incidence_count || origin_count > incidence_count {
        return Err(AdjRunError::NonCanonical);
    }
    let source_map_id = input.array()?;
    let destination_map_id = input.array()?;
    let canonical_stable_id_digest = input.array()?;
    let image_fingerprint = input.array()?;
    let image_len = input.count()?;
    let representation =
        crate::representation_from_tag(input.byte()?).ok_or(AdjRunError::NonCanonical)?;
    let prefixes = input.count()?;
    let identity_len = input.count()?;
    let scalar_len = input.count()?;
    let image_bytes = input.take(image_len)?;
    let identity_bytes = input.take(identity_len)?;
    let scalar_bytes = input.take(scalar_len)?;
    let incoming_bytes = if version == FORMAT_V2 {
        let len = input.count()?;
        Some(input.take(len)?)
    } else {
        None
    };
    if input.at != input.bytes.len() {
        return Err(SealedError::TrailingBytes.into());
    }
    let anchor = SealedAnchor {
        scope,
        relation: Some(relation),
        fingerprint: image_fingerprint,
        encoded_bytes: image_len,
        admitted_envelope: None,
    };
    let mut partition = SealedPartition::reload(cx, anchor, image_bytes, limits, resolver)?;
    if partition.image.rows.len() != row_count || partition.image.incidences != incidence_count {
        return Err(AdjRunError::NonCanonical);
    }
    if let Some(incoming_bytes) = incoming_bytes {
        let incoming = super::incoming::decode_durable_index(
            incoming_bytes,
            relation,
            &partition.image,
            limits,
            &mut || checkpoint(cx),
        )?;
        partition.set_durable_incoming(incoming);
    }
    let ids = IdentityColumn::<EId>::try_from_scalar_payload(
        identity_bytes,
        IdentityColumnDescriptor::new(representation, origin_count, prefixes),
        identity_limits(origin_count, limits),
    )
    .map_err(SealedError::Identity)?;
    let mut scalar_input = Input {
        bytes: scalar_bytes,
        at: 0,
    };
    let mut origins = reserved(origin_count)?;
    let mut previous = None;
    for at in 0..origin_count {
        checkpoint(cx)?;
        let eid = ids.get(at).ok_or(AdjRunError::NonCanonical)?;
        if previous.is_some_and(|previous| previous >= eid) {
            return Err(AdjRunError::NonCanonical);
        }
        previous = Some(eid);
        let origin = OriginBirthOrder::new(
            CommitSeq(scalar_input.varint()?),
            scalar_input.varint()?,
            scalar_input.varint()?,
            eid,
        );
        origins.push((eid, origin));
    }
    if scalar_input.at != scalar_input.bytes.len() {
        return Err(SealedError::TrailingBytes.into());
    }
    let expected_ids = partition.stable_edge_ids(cx, limits.max_incidences)?;
    if !expected_ids
        .iter()
        .copied()
        .eq(origins.iter().map(|(eid, _)| *eid))
    {
        return Err(AdjRunError::OriginSetMismatch);
    }
    let actual_maps = maps(cx, &partition, relation)?;
    if actual_maps != (source_map_id, destination_map_id) {
        return Err(AdjRunError::MapMismatch);
    }
    if stable_digest(cx, &partition, relation, &origins)? != canonical_stable_id_digest {
        return Err(AdjRunError::StableDigestMismatch);
    }
    checkpoint(cx)?;
    Ok(AdjRunSegment {
        header: AdjRunHeader {
            scope,
            relation,
            row_count,
            incidence_count,
            origin_count,
            source_map_id,
            destination_map_id,
            canonical_stable_id_digest,
        },
        partition,
        origins: origins.into(),
        origin_identity: Arc::new(ids),
    })
}

#[cfg(test)]
mod tests;
