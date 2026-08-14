//! `PackedObjectGroup` laws.
//!
//! The load-bearing one is `a_mixed_key_domain_pack_cannot_be_built`: the plan
//! makes homogeneity mandatory because a mixed pack coarsens crypto-erasure
//! granularity across epochs, tenants, or floors — meaning a later "erase this
//! branch's keys" would leave another domain's bytes unreadable, or worse,
//! readable. It must fail at build time, and every axis must fail.

use fgdb_chronicle::identity::IdentifiedObject;
use fgdb_chronicle::pack::{
    DomainAxis, PACK_REALIZATION_KIND, PackBuilder, PackDomain, PackError, PackProtectionProfile,
    WriteKeyDomain,
};
use fgdb_types::ids::DatabaseSecurityNamespaceId;

fn k_oid() -> [u8; 32] {
    core::array::from_fn(|i| (i as u8).wrapping_mul(3).wrapping_add(7))
}

fn dek() -> [u8; 32] {
    core::array::from_fn(|i| (i as u8).wrapping_mul(11).wrapping_add(1))
}

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId(core::array::from_fn(|i| (i as u8) ^ 0x5a))
}

fn domain() -> PackDomain {
    PackDomain {
        namespace: namespace(),
        tenant: 42,
        write_key: WriteKeyDomain::BranchScoped {
            graph: 1,
            branch: 2,
            envelope_epoch: 3,
            write_key_epoch: 4,
        },
        retention_class: 7,
    }
}

fn protection_profile() -> PackProtectionProfile {
    PackProtectionProfile {
        codec_profile: 1,
        data_crypto_profile: 1,
        dek_id: [3u8; 16],
    }
}

/// A small control-plane object, of the kind a commit produces in bursts.
fn member(tag: u8, len: usize) -> IdentifiedObject {
    let payload: Vec<u8> = (0..len)
        .map(|i| ((i as u8).wrapping_add(tag)) % 251)
        .collect();
    IdentifiedObject::new(&k_oid(), namespace(), 0x0002, &[], &payload)
}

fn sealed_pack() -> fgdb_chronicle::PackedObjectGroup {
    let mut builder = PackBuilder::new(domain());
    for (tag, len) in [(1u8, 96usize), (2, 40), (3, 200), (4, 8)] {
        builder
            .add(member(tag, len), domain())
            .expect("homogeneous members must be admitted");
    }
    let total: u64 = builder.len() as u64;
    assert_eq!(total, 4);
    builder
        .seal(&k_oid(), &dek(), protection_profile())
        .expect("a homogeneous pack must seal")
}

/// THE MANDATORY LAW, on every axis. Each disagreement must be refused at
/// build time and must name the axis that disagreed.
#[test]
fn a_mixed_key_domain_pack_cannot_be_built() {
    let base = domain();

    // Different tenant.
    let mut builder = PackBuilder::new(base);
    builder.add(member(1, 32), base).expect("first member");
    let other_tenant = PackDomain { tenant: 99, ..base };
    assert_eq!(
        builder.add(member(2, 32), other_tenant),
        Err(PackError::HeterogeneousDomain {
            axis: DomainAxis::Tenant
        })
    );

    // Different write-key epoch — the same branch at a later key epoch is a
    // DIFFERENT crypto-erasure boundary.
    let rotated = PackDomain {
        write_key: WriteKeyDomain::BranchScoped {
            graph: 1,
            branch: 2,
            envelope_epoch: 3,
            write_key_epoch: 5,
        },
        ..base
    };
    assert_eq!(
        builder.add(member(3, 32), rotated),
        Err(PackError::HeterogeneousDomain {
            axis: DomainAxis::WriteKey
        })
    );

    // Branch-scoped and commit-stream bytes may never share a pack.
    let commit_stream = PackDomain {
        write_key: WriteKeyDomain::CommitStream,
        ..base
    };
    assert_eq!(
        builder.add(member(4, 32), commit_stream),
        Err(PackError::HeterogeneousDomain {
            axis: DomainAxis::WriteKey
        })
    );

    // Different retention/floor class.
    let other_retention = PackDomain {
        retention_class: 9,
        ..base
    };
    assert_eq!(
        builder.add(member(5, 32), other_retention),
        Err(PackError::HeterogeneousDomain {
            axis: DomainAxis::RetentionClass
        })
    );

    // Different security namespace, asserted by the caller AND carried by the
    // object's own keyed identity — both are checked.
    let other_ns = DatabaseSecurityNamespaceId(core::array::from_fn(|i| (i as u8) ^ 0xa5));
    let foreign = PackDomain {
        namespace: other_ns,
        ..base
    };
    assert_eq!(
        builder.add(member(6, 32), foreign),
        Err(PackError::HeterogeneousDomain {
            axis: DomainAxis::Namespace
        })
    );
    let foreign_object = IdentifiedObject::new(&k_oid(), other_ns, 0x0002, &[], b"bytes");
    assert_eq!(
        builder.add(foreign_object, base),
        Err(PackError::HeterogeneousDomain {
            axis: DomainAxis::Namespace
        })
    );

    // The rejections left the pack intact: exactly the one legal member.
    assert_eq!(builder.len(), 1, "a refused member must not be admitted");
}

/// A pack pays ONE pipeline: one AEAD, one CiphertextId, for all members.
#[test]
fn a_pack_pays_one_pipeline_for_every_member() {
    let pack = sealed_pack();
    assert_eq!(pack.member_count(), 4);
    // One protected realization owns the chain — not four.
    let protected = pack.protected();
    assert!(!protected.protected_bytes().is_empty());
    assert_eq!(
        protected.descriptor().object_kind,
        PACK_REALIZATION_KIND,
        "a pack never acquires a logical ObjectKind"
    );
}

/// The public pack profile has no nonce. The implementation derives one from
/// the pack identity and every selectable protection field, so the same pack
/// is reproducible while a different protection transcript cannot reuse it.
#[test]
fn the_pack_nonce_and_descriptor_facts_are_derived() {
    let first = sealed_pack();
    let second = sealed_pack();
    assert_eq!(
        first.protected().descriptor().object_nonce,
        second.protected().descriptor().object_nonce,
        "the identical pack/profile pair has one deterministic nonce"
    );

    let mut changed_profile = protection_profile();
    changed_profile.dek_id[0] ^= 0x01;
    let mut builder = PackBuilder::new(domain());
    for (tag, len) in [(1u8, 96usize), (2, 40), (3, 200), (4, 8)] {
        builder
            .add(member(tag, len), domain())
            .expect("homogeneous members must be admitted");
    }
    let changed = builder
        .seal(&k_oid(), &dek(), changed_profile)
        .expect("changed profile still seals");
    assert_ne!(
        first.protected().descriptor().object_nonce,
        changed.protected().descriptor().object_nonce,
        "a distinct protection transcript must not reuse the nonce"
    );

    let descriptor = first.protected().descriptor();
    assert_eq!(descriptor.object_kind, PACK_REALIZATION_KIND);
    // Each of the four members carries its little-endian `object_kind` in
    // the canonical logical header as well as its payload.
    assert_eq!(descriptor.canonical_plaintext_len, 344 + 4 * 2);
    assert_eq!(descriptor.compressed_len, 344 + 4 * 2);
    assert_eq!(descriptor.object_tag_len, 16);
}

/// Every member keeps its own identity, and extraction PROVES it: the bytes at
/// a locator must recompute that member's `ObjectId`.
#[test]
fn every_member_keeps_and_proves_its_own_identity() {
    let pack = sealed_pack();
    for locator in pack.locators() {
        let bytes = pack
            .extract(locator.object_id, &k_oid(), &dek(), &mut Vec::new())
            .expect("a member must extract and verify");
        assert_eq!(bytes.len() as u64, locator.length);
    }
}

/// Locators partition the pack: contiguous, in order, covering it exactly.
#[test]
fn locators_partition_the_pack_exactly() {
    let pack = sealed_pack();
    let mut expected_offset = 0u64;
    for locator in pack.locators() {
        assert_eq!(
            locator.offset, expected_offset,
            "locators must be contiguous"
        );
        expected_offset += locator.length;
    }
    let unpacked = pack
        .protected()
        .open(&dek(), &mut Vec::new())
        .expect("the pack must open")
        .len() as u64;
    assert_eq!(
        expected_offset, unpacked,
        "locators must cover the pack exactly"
    );
}

/// A member of another pack is not a member of this one, and asking for it
/// fails rather than returning a neighbouring member's bytes.
#[test]
fn a_non_member_is_refused() {
    let pack = sealed_pack();
    let stranger = member(200, 64);
    assert_eq!(
        pack.extract(stranger.object_id(), &k_oid(), &dek(), &mut Vec::new()),
        Err(PackError::NotAMember)
    );
}

/// The wrong DEK cannot open the pack at all — one AEAD protects every member,
/// so member access is gated by the pack's own authentication.
#[test]
fn the_wrong_dek_yields_no_member() {
    let pack = sealed_pack();
    let locator = pack.locators()[0];
    let mut wrong = dek();
    wrong[0] ^= 0xff;
    assert_eq!(
        pack.extract(locator.object_id, &k_oid(), &wrong, &mut Vec::new(),),
        Err(PackError::MemberIdentityMismatch)
    );
}

/// The wrong database identity key cannot make bytes into a member: identity
/// is keyed, so a substituted K_oid fails the recomputation.
#[test]
fn the_wrong_identity_key_fails_verification() {
    let pack = sealed_pack();
    let locator = pack.locators()[0];
    let mut wrong = k_oid();
    wrong[0] ^= 0xff;
    assert_eq!(
        pack.extract(locator.object_id, &wrong, &dek(), &mut Vec::new()),
        Err(PackError::MemberIdentityMismatch)
    );
}

/// RECLAMATION COARSENS TO THE PACK: reclaimable only when every member is.
#[test]
fn a_pack_is_reclaimable_only_when_every_member_is() {
    let pack = sealed_pack();
    let all = pack.locators().to_vec();

    assert!(
        pack.is_reclaimable(|_| true),
        "all members dead means the pack is reclaimable"
    );
    assert!(
        !pack.is_reclaimable(|_| false),
        "all members live means it is not"
    );
    // One live member is enough to hold the whole pack's bytes.
    let live = all[2].object_id;
    assert!(
        !pack.is_reclaimable(|id| id != live),
        "a single live member must hold the pack"
    );
}

/// An empty pack has no domain and no meaning.
#[test]
fn an_empty_pack_cannot_be_sealed() {
    let builder = PackBuilder::new(domain());
    assert!(builder.is_empty());
    assert_eq!(
        builder.seal(&k_oid(), &dek(), protection_profile()).err(),
        Some(PackError::EmptyPack)
    );
}

/// Packing is deterministic: the same members in the same order produce the
/// same realization, so a retry after a crash is idempotent rather than a
/// second differently-packed copy.
#[test]
fn packing_is_deterministic() {
    let first = sealed_pack();
    let second = sealed_pack();
    assert_eq!(
        first.protected().ciphertext_id(),
        second.protected().ciphertext_id()
    );
    assert_eq!(first.locators(), second.locators());
}
