//! `PackedObjectGroup`: many small logical objects, one physical pipeline.
//!
//! A commit produces a burst of small control-plane objects — capsule, effect
//! set, delta template, authorization decision, resource transitions, outcome
//! preparation. Paying a separate compress/AEAD/FEC/placement pipeline for
//! each is most of the metadata cost of a small transaction. A pack pays that
//! pipeline **once** for the whole group (plan L342).
//!
//! WHAT DOES NOT CHANGE, and this is the point: every member keeps its own
//! keyed `ObjectId`, computed and verified over its own canonical plaintext.
//! `StrongRef` targets logical identities, never placements, so references,
//! the generated reachability/GC walkers, and content addressing are all
//! unaffected by whether an object happened to be packed. **A pack never
//! acquires an `ObjectKind`** — it is a physical realization, not a logical
//! object.
//!
//! THE ONE MANDATORY LAW: a pack is **homogeneous in key domain**. Every
//! member must share one write-key domain, one tenant/authority domain (one
//! `DatabaseSecurityNamespaceId`), and one retention/floor class, because the
//! pack's DEK wrap binds a single source epoch and creation boundary. A mixed
//! pack would coarsen crypto-erasure granularity across epochs, tenants, or
//! floors — so it **fails closed at build time**, not at review time.

use crate::identity::{CipherDescriptor, IdentifiedObject, ProtectedObject};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

/// Which key domain an object's bytes are written under (plan §12.5).
///
/// Branch-scoped payloads are wrapped under the branch envelope and write-key
/// epoch; commit-mechanism objects (markers, capsules, command records) are
/// wrapped under the database-scoped commit-stream domain. They are different
/// crypto-erasure boundaries, so they may never share a pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKeyDomain {
    /// Branch-scoped: erasure granularity is the branch's key epoch.
    BranchScoped {
        graph: u64,
        branch: u64,
        envelope_epoch: u32,
        write_key_epoch: u32,
    },
    /// The database-scoped commit-stream key domain.
    CommitStream,
}

/// The complete domain a pack is homogeneous in. All three axes bind the
/// pack's DEK wrap, so all three must agree across members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackDomain {
    pub namespace: DatabaseSecurityNamespaceId,
    pub tenant: u64,
    pub write_key: WriteKeyDomain,
    /// The retention/floor class this pack's bytes are reclaimed under.
    pub retention_class: u16,
}

/// Caller-selected protection policy for one pack realization.
///
/// There is deliberately no nonce field here. The pack derives its nonce from
/// its keyed identity and this complete profile, so a caller cannot reuse one
/// nonce across different pack plaintexts or protection descriptors. The
/// object kind, plaintext lengths, and AEAD tag length are likewise facts of
/// the realization and are constructed by [`PackBuilder::seal`], not asserted
/// by its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackProtectionProfile {
    pub codec_profile: u16,
    pub data_crypto_profile: u16,
    pub dek_id: [u8; 16],
}

/// Why a pack could not be built or read. Every variant is fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    /// THE HOMOGENEITY LAW FIRED: a member's key domain, tenant, namespace, or
    /// retention class differs from the pack's. Building it anyway would
    /// coarsen crypto-erasure granularity, so the build refuses.
    HeterogeneousDomain {
        /// Which axis disagreed, for a diagnosable failure.
        axis: DomainAxis,
    },
    /// A pack with no members has no domain and no meaning.
    EmptyPack,
    /// The requested object is not a member of this pack.
    NotAMember,
    /// The member's bytes did not recompute its `ObjectId` — the pack's
    /// contents are not what its locators claim.
    MemberIdentityMismatch,
    /// A locator points outside the unpacked bytes.
    LocatorOutOfRange,
}

/// Which axis of the pack domain a rejected member disagreed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainAxis {
    Namespace,
    Tenant,
    WriteKey,
    RetentionClass,
}

impl core::fmt::Display for PackError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::HeterogeneousDomain { axis } => {
                write!(f, "member disagrees with the pack domain on {axis:?}")
            }
            Self::EmptyPack => f.write_str("a pack must have at least one member"),
            Self::NotAMember => f.write_str("object is not a member of this pack"),
            Self::MemberIdentityMismatch => {
                f.write_str("member bytes do not recompute the member ObjectId")
            }
            Self::LocatorOutOfRange => f.write_str("member locator points outside the pack"),
        }
    }
}

impl core::error::Error for PackError {}

/// Where one member's canonical plaintext lives inside the unpacked bytes.
///
/// This is the subobject locator the object-location index stores: a member
/// is addressed by `(pack encoding, offset, length)` while still being
/// *named* by its logical `ObjectId`. The locator itself is pack-side
/// metadata and is NOT under the pack's AEAD — what makes an edited locator
/// fail closed is extract-time identity re-derivation (the bytes it points
/// at must recompute the requested `ObjectId`), so tampering degrades
/// availability, never returns wrong bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubobjectLocator {
    pub object_id: ObjectId,
    pub offset: u64,
    pub length: u64,
}

/// A pack under construction. Members are admitted one at a time and the
/// domain law is checked on admission, so an illegal pack cannot reach a
/// sealed state at all.
#[derive(Debug, Clone)]
pub struct PackBuilder {
    domain: PackDomain,
    members: Vec<IdentifiedObject>,
}

impl PackBuilder {
    /// Start a pack in one declared domain. Every member must match it.
    pub fn new(domain: PackDomain) -> Self {
        Self {
            domain,
            members: Vec::new(),
        }
    }

    /// Admit one member, or refuse with the exact axis that disagreed.
    ///
    /// `member_domain` is the domain the caller asserts for these bytes; the
    /// namespace is cross-checked against the object's own keyed identity
    /// namespace so a caller cannot assert a domain the object does not have.
    pub fn add(
        &mut self,
        member: IdentifiedObject,
        member_domain: PackDomain,
    ) -> Result<(), PackError> {
        if member.namespace() != self.domain.namespace
            || member_domain.namespace != self.domain.namespace
        {
            return Err(PackError::HeterogeneousDomain {
                axis: DomainAxis::Namespace,
            });
        }
        if member_domain.tenant != self.domain.tenant {
            return Err(PackError::HeterogeneousDomain {
                axis: DomainAxis::Tenant,
            });
        }
        if member_domain.write_key != self.domain.write_key {
            return Err(PackError::HeterogeneousDomain {
                axis: DomainAxis::WriteKey,
            });
        }
        if member_domain.retention_class != self.domain.retention_class {
            return Err(PackError::HeterogeneousDomain {
                axis: DomainAxis::RetentionClass,
            });
        }
        self.members.push(member);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Seal the pack: concatenate members' canonical plaintext in admission
    /// order, record their locators, and pay ONE AEAD for the whole group.
    ///
    /// Framing is length-delimited by the locators. The locators themselves
    /// are pack-side metadata OUTSIDE the AEAD — what the AEAD covers is the
    /// packed plaintext. An edited locator cannot hand back the wrong
    /// object: `extract` recomputes the member's `ObjectId` from the bytes
    /// the locator names, so tampering fails closed as an availability
    /// loss, never as wrong bytes.
    pub fn seal(
        self,
        k_oid: &[u8; 32],
        dek: &[u8; 32],
        profile: PackProtectionProfile,
    ) -> Result<PackedObjectGroup, PackError> {
        if self.members.is_empty() {
            return Err(PackError::EmptyPack);
        }
        let mut packed_plaintext = Vec::new();
        let mut locators = Vec::with_capacity(self.members.len());
        for member in &self.members {
            let bytes = member.canonical_plaintext();
            locators.push(SubobjectLocator {
                object_id: member.object_id(),
                offset: packed_plaintext.len() as u64,
                length: bytes.len() as u64,
            });
            packed_plaintext.extend_from_slice(bytes);
        }

        // The pack is a PHYSICAL realization, not a logical object. It is
        // given an identity here only so the one AEAD has a stable AAD to bind
        // — object_kind 0x0000 is permanently invalid in every registry, so
        // this identity can never be mistaken for a logical-object row, and no
        // member's ObjectId is affected by being packed.
        let pack_object = IdentifiedObject::new(
            k_oid,
            self.domain.namespace,
            PACK_REALIZATION_KIND,
            PACK_HEADER_DOMAIN,
            &packed_plaintext,
        );
        let packed_len = packed_plaintext.len() as u64;
        let descriptor = CipherDescriptor {
            object_kind: PACK_REALIZATION_KIND,
            canonical_plaintext_len: packed_len,
            codec_profile: profile.codec_profile,
            compressed_len: packed_len,
            data_crypto_profile: profile.data_crypto_profile,
            dek_id: profile.dek_id,
            object_nonce: derive_pack_nonce(pack_object.object_id(), profile),
            // XChaCha20-Poly1305's fixed authenticator width. A caller cannot
            // choose a shorter split and thereby weaken CiphertextId binding.
            object_tag_len: 16,
        };
        let protected = pack_object.protect(dek, descriptor, &packed_plaintext);

        Ok(PackedObjectGroup {
            domain: self.domain,
            locators,
            protected,
        })
    }
}

/// The header domain string of a pack realization. Distinct from every
/// logical-object domain so a pack can never be mistaken for one.
pub const PACK_HEADER_DOMAIN: &[u8] = b"fgdb:packed-object-group:v1";

/// Domain separation for deterministic pack-nonce derivation.
const PACK_NONCE_DOMAIN: &[u8] = b"fgdb:packed-object-group-nonce:v1";

/// Derive the AEAD nonce from everything that can distinguish two pack
/// encryptions under one DEK. Identical identity/profile pairs encrypt the
/// identical plaintext under identical AAD; changing either the pack bytes or
/// any caller-selectable protection field necessarily changes the nonce.
fn derive_pack_nonce(object_id: ObjectId, profile: PackProtectionProfile) -> [u8; 24] {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(PACK_NONCE_DOMAIN);
    hasher.update(&object_id.0);
    hasher.update(&profile.codec_profile.to_be_bytes());
    hasher.update(&profile.data_crypto_profile.to_be_bytes());
    hasher.update(&profile.dek_id);
    let digest = hasher.finalize();
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&digest.0[..24]);
    nonce
}

/// A pack never acquires an `ObjectKind`. `0x0000` is permanently invalid in
/// every registry code space (plan L290), so a pack realization cannot collide
/// with any logical kind now or later.
pub const PACK_REALIZATION_KIND: u16 = 0x0000;

/// A sealed pack: one physical realization carrying many logical objects.
#[derive(Debug, Clone)]
pub struct PackedObjectGroup {
    domain: PackDomain,
    locators: Vec<SubobjectLocator>,
    protected: ProtectedObject,
}

impl PackedObjectGroup {
    pub fn domain(&self) -> PackDomain {
        self.domain
    }

    /// The member locators, in pack order. Pack-side metadata outside the
    /// AEAD; extract-time identity re-derivation is their integrity guard.
    pub fn locators(&self) -> &[SubobjectLocator] {
        &self.locators
    }

    pub fn member_count(&self) -> usize {
        self.locators.len()
    }

    /// The pack's single protected realization — the one object the FEC and
    /// placement layers see. **The pack, not the member, owns this chain.**
    pub fn protected(&self) -> &ProtectedObject {
        &self.protected
    }

    /// Find a member's locator by its logical identity.
    pub fn locate(&self, object_id: ObjectId) -> Option<SubobjectLocator> {
        self.locators
            .iter()
            .copied()
            .find(|locator| locator.object_id == object_id)
    }

    /// Extract one member's canonical plaintext, proving on the way out that
    /// the bytes at its locator really are that object.
    ///
    /// `k_oid` is the database identity key: the member's `ObjectId` is
    /// recomputed from the extracted bytes and compared, so a pack whose
    /// locators were edited — or whose members were reordered — cannot hand
    /// back the wrong object under the right name.
    pub fn extract(
        &self,
        object_id: ObjectId,
        k_oid: &[u8; 32],
        dek: &[u8; 32],
    ) -> Result<Vec<u8>, PackError> {
        let locator = self.locate(object_id).ok_or(PackError::NotAMember)?;
        let unpacked = self
            .protected
            .open(dek)
            .map_err(|_| PackError::MemberIdentityMismatch)?;

        let start = usize::try_from(locator.offset).map_err(|_| PackError::LocatorOutOfRange)?;
        let length = usize::try_from(locator.length).map_err(|_| PackError::LocatorOutOfRange)?;
        let end = start
            .checked_add(length)
            .ok_or(PackError::LocatorOutOfRange)?;
        if end > unpacked.len() {
            return Err(PackError::LocatorOutOfRange);
        }
        let member_bytes = unpacked[start..end].to_vec();

        // The member keeps its own identity and it is VERIFIED here, not
        // assumed — the same full-verification discipline a collision bucket
        // pays before any deduplication or substitution.
        //
        // The header/payload split does not have to be known: the §5.1
        // transcript concatenates them, so hashing the whole canonical
        // plaintext as the payload reproduces the identical byte stream.
        let recomputed =
            fgdb_crypto::logical_object_id(k_oid, &self.domain.namespace.0, &[], &member_bytes);
        if ObjectId(recomputed.0) != object_id {
            return Err(PackError::MemberIdentityMismatch);
        }
        Ok(member_bytes)
    }

    /// RECLAMATION COARSENS TO THE PACK (plan L342): a pack's bytes may be
    /// reclaimed only when EVERY member is reclaimable. Compaction owns the
    /// rewrite of partially dead packs; this predicate is what stops a
    /// reclaim path from freeing bytes a live member still needs.
    pub fn is_reclaimable(&self, member_is_reclaimable: impl Fn(ObjectId) -> bool) -> bool {
        self.locators
            .iter()
            .all(|locator| member_is_reclaimable(locator.object_id))
    }
}
