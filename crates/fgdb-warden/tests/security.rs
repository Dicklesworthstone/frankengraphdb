//! Adversarial tests for the implemented first-party boundary, not a claim
//! that existing fgdb storage/execution paths enforce FG-INV-20 end to end.

use asupersync::cx::macaroon::{CaveatPredicate, MacaroonToken};
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_warden::{
    Authority, CapabilityToken, Error, Grant, LimitDimension, MAX_CAVEATS, MAX_NAME_BYTES,
    MAX_SCOPE_ORDINALS, MAX_TOKEN_BYTES, QueryLimits, Restriction, Rights, Scope,
};
use std::cell::Cell;
use std::collections::BTreeSet;

const START: u64 = 100;
const END: u64 = 1000;
const BRANCH: &str = "agent-17";

fn key() -> AuthKey {
    // Deterministic TEST key, never a production fallback.
    AuthKey::from_seed(0x71ac_203d_981b_4fe2)
}

fn authority() -> Authority {
    Authority::new(
        key(),
        DatabaseSecurityNamespaceId([7; 32]),
        "graph",
        SchemaEpoch(3),
        9,
    )
    .expect("fixture authority")
}

fn grant() -> Grant {
    let mut result = Grant::read_only(
        BRANCH,
        END,
        QueryLimits {
            max_nodes: 100,
            max_work: 1000,
            max_rows: 10,
        },
    );
    result.labels = Scope::only([LabelId(1), LabelId(2)]);
    result.relations = Scope::only([RelationId(10), RelationId(20)]);
    result.properties = Scope::only([PropertyKeyId(100), PropertyKeyId(200)]);
    result
}

fn raw(token: &CapabilityToken) -> MacaroonToken {
    MacaroonToken::from_binary(&token.encode()).expect("foundation roundtrip")
}

fn append_raw(token: &CapabilityToken, predicate: CaveatPredicate) -> CapabilityToken {
    CapabilityToken::decode(&raw(token).add_caveat(predicate).to_binary()).expect("bounded token")
}

#[test]
fn issue_roundtrip_and_verify() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let bytes = token.encode();
    let decoded = CapabilityToken::decode(&bytes).unwrap();
    assert_eq!(decoded.encode(), bytes);
    let verified = authority.verify_at(&decoded, BRANCH, START).unwrap();
    let predicates = verified.predicates();
    assert!(predicates.allows_vertex(&[LabelId(2)]));
    assert!(!predicates.allows_vertex(&[LabelId(999)]));
    assert!(predicates.allows_relation(RelationId(10)));
    assert!(!predicates.allows_relation(RelationId(11)));
    assert_eq!(predicates.limits(), grant().limits);
}

#[test]
fn least_privilege_constructor_denies_all_objects() {
    let authority = authority();
    let grant = Grant::read_only(BRANCH, END, grant().limits);
    let token = authority.issue_at(&grant, START).unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    assert!(!verified.predicates().allows_vertex(&[]));
    assert!(!verified.predicates().allows_vertex(&[LabelId(1)]));
    assert!(!verified.predicates().allows_relation(RelationId(10)));
    assert!(!verified.predicates().allows_property(PropertyKeyId(100)));
    assert!(verified.begin_read_at(BRANCH, START).is_ok());
    assert!(matches!(
        verified.begin_write_at(BRANCH, START),
        Err(Error::PermissionDenied)
    ));
}

#[test]
fn keyless_attenuation_keeps_parent_and_cannot_restore_wildcards() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let parent_bytes = token.encode();
    let child = token
        .attenuate(Restriction::Relations(Scope::only([RelationId(10)])))
        .unwrap();
    let child = child.attenuate(Restriction::Relations(Scope::All)).unwrap();
    let verified = authority.verify_at(&child, BRANCH, START).unwrap();
    assert!(verified.predicates().allows_relation(RelationId(10)));
    assert!(!verified.predicates().allows_relation(RelationId(20)));
    assert_eq!(token.encode(), parent_bytes);
    assert!(
        authority
            .verify_at(&token, BRANCH, START)
            .unwrap()
            .predicates()
            .allows_relation(RelationId(20))
    );
}

#[test]
fn multilabel_attenuation_preserves_conjunction_not_set_intersection() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::Labels(Scope::only([LabelId(2), LabelId(3)])))
        .unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    let p = verified.predicates();
    assert!(p.allows_vertex(&[LabelId(1), LabelId(3)]));
    assert!(p.allows_vertex(&[LabelId(2)]));
    assert!(!p.allows_vertex(&[LabelId(1)]));
    assert!(!p.allows_vertex(&[LabelId(3)]));
    assert!(!p.allows_vertex(&[]));
    assert!(!p.allows_label(LabelId(1)));
    assert!(p.allows_label(LabelId(2)));
    assert!(!p.allows_label(LabelId(3)));
}

#[test]
fn finite_label_caveats_match_independent_boolean_oracle_exhaustively() {
    let authority = authority();
    let ids = |bits: u8| {
        (0..3_u8)
            .filter(move |i| bits & (1_u8 << *i) != 0)
            .map(|i| LabelId(u64::from(i)))
            .collect::<Vec<_>>()
    };
    for parent_bits in 0..8_u8 {
        for child_bits in 0..8_u8 {
            let mut grant = grant();
            grant.labels = Scope::only(ids(parent_bits));
            let token = authority
                .issue_at(&grant, START)
                .unwrap()
                .attenuate(Restriction::Labels(Scope::only(ids(child_bits))))
                .unwrap();
            let verified = authority.verify_at(&token, BRANCH, START).unwrap();
            for vertex_bits in 0..8_u8 {
                let expected = parent_bits & vertex_bits != 0 && child_bits & vertex_bits != 0;
                assert_eq!(
                    verified.predicates().allows_vertex(&ids(vertex_bits)),
                    expected,
                    "parent={parent_bits} child={child_bits} vertex={vertex_bits}"
                );
            }
        }
    }
}

#[test]
fn empty_scope_is_not_unlimited() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::Labels(Scope::only([])))
        .unwrap()
        .attenuate(Restriction::Labels(Scope::All))
        .unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    assert!(!verified.predicates().allows_vertex(&[]));
    assert!(
        !verified
            .predicates()
            .allows_vertex(&[LabelId(1), LabelId(2)])
    );
}

#[test]
fn allowed_properties_intersect_and_denials_accumulate() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::Properties(Scope::only([PropertyKeyId(100)])))
        .unwrap()
        .attenuate(Restriction::DenyProperties(BTreeSet::from([
            PropertyKeyId(100),
        ])))
        .unwrap()
        .attenuate(Restriction::Properties(Scope::All))
        .unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    assert!(!verified.predicates().allows_property(PropertyKeyId(100)));
    assert!(!verified.predicates().allows_property(PropertyKeyId(200)));
    assert!(!verified.predicates().allows_property(PropertyKeyId(300)));
}

#[test]
fn write_right_cannot_be_added_to_read_only_parent() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::Rights(Rights::ReadWrite))
        .unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    assert_eq!(verified.predicates().rights(), Rights::Read);
    assert!(matches!(
        verified.begin_write_at(BRANCH, START),
        Err(Error::PermissionDenied)
    ));
}

#[test]
fn write_capability_can_be_attenuated_to_read_or_no_rights() {
    let authority = authority();
    let mut grant = grant();
    grant.rights = Rights::ReadWrite;
    let parent = authority.issue_at(&grant, START).unwrap();
    assert!(
        authority
            .verify_at(&parent, BRANCH, START)
            .unwrap()
            .begin_write_at(BRANCH, START)
            .is_ok()
    );
    let read = parent.attenuate(Restriction::Rights(Rights::Read)).unwrap();
    assert!(matches!(
        authority
            .verify_at(&read, BRANCH, START)
            .unwrap()
            .begin_write_at(BRANCH, START),
        Err(Error::PermissionDenied)
    ));
    let none = read.attenuate(Restriction::Rights(Rights::None)).unwrap();
    assert!(matches!(
        authority
            .verify_at(&none, BRANCH, START)
            .unwrap()
            .begin_read_at(BRANCH, START),
        Err(Error::PermissionDenied)
    ));
}

#[test]
fn branch_binding_checks_admission_and_reuse() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    assert!(matches!(
        authority.verify_at(&token, "main", START),
        Err(Error::ScopeDenied)
    ));
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    assert!(matches!(
        verified.begin_read_at("main", START),
        Err(Error::ScopeDenied)
    ));
    let contradictory = token
        .attenuate(Restriction::Branch("main".to_owned()))
        .unwrap();
    assert!(matches!(
        authority.verify_at(&contradictory, "main", START),
        Err(Error::ScopeDenied)
    ));
    assert!(matches!(
        authority.verify_at(&contradictory, BRANCH, START),
        Err(Error::ScopeDenied)
    ));
}

#[test]
fn expiration_is_exclusive_and_start_is_inclusive() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    assert!(matches!(
        authority.verify_at(&token, BRANCH, START - 1),
        Err(Error::NotYetValid)
    ));
    assert!(authority.verify_at(&token, BRANCH, START).is_ok());
    assert!(authority.verify_at(&token, BRANCH, END - 1).is_ok());
    assert!(matches!(
        authority.verify_at(&token, BRANCH, END),
        Err(Error::Expired)
    ));
}

#[test]
fn time_attenuation_takes_maximum_start_and_minimum_expiry() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::NotBefore(200))
        .unwrap()
        .attenuate(Restriction::NotBefore(0))
        .unwrap()
        .attenuate(Restriction::ExpiresBefore(500))
        .unwrap()
        .attenuate(Restriction::ExpiresBefore(u64::MAX))
        .unwrap();
    assert!(matches!(
        authority.verify_at(&token, BRANCH, 199),
        Err(Error::NotYetValid)
    ));
    assert!(authority.verify_at(&token, BRANCH, 200).is_ok());
    assert!(authority.verify_at(&token, BRANCH, 499).is_ok());
    assert!(matches!(
        authority.verify_at(&token, BRANCH, 500),
        Err(Error::Expired)
    ));
}

#[test]
fn expiry_is_rechecked_during_execution_and_cannot_be_caught_and_ignored() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, START).unwrap();
    assert_eq!(permit.charge_work_at(END - 1, 1), Ok(()));
    assert_eq!(permit.charge_work_at(END, 1), Err(Error::Expired));
    assert_eq!(
        permit.charge_work_at(START, 0),
        Err(Error::ExecutionStopped)
    );
}

#[test]
fn budgets_intersect_and_zero_really_disallows_work() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::MaxNodes(1))
        .unwrap()
        .attenuate(Restriction::MaxNodes(u64::MAX))
        .unwrap()
        .attenuate(Restriction::MaxRows(0))
        .unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    assert_eq!(verified.predicates().limits().max_nodes, 1);
    let mut permit = verified.begin_read_at(BRANCH, START).unwrap();
    assert_eq!(permit.charge_nodes_at(START, 1), Ok(()));
    assert_eq!(
        permit.charge_nodes_at(START, 1),
        Err(Error::LimitExceeded(LimitDimension::Nodes))
    );
    assert_eq!(
        permit.charge_work_at(START, 0),
        Err(Error::ExecutionStopped)
    );
    let mut permit = verified.begin_read_at(BRANCH, START).unwrap();
    assert_eq!(
        permit.charge_rows_at(START, 1),
        Err(Error::LimitExceeded(LimitDimension::Rows))
    );
}

#[test]
fn counter_overflow_fails_before_exposing_work() {
    let authority = authority();
    let mut grant = grant();
    grant.limits.max_work = u64::MAX;
    let token = authority.issue_at(&grant, START).unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, START).unwrap();
    assert_eq!(permit.charge_work_at(START, u64::MAX), Ok(()));
    assert_eq!(
        permit.charge_work_at(START, 1),
        Err(Error::LimitExceeded(LimitDimension::Work))
    );
    assert_eq!(permit.usage().work, u64::MAX);
}

#[test]
fn hidden_descriptor_is_never_opened() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, START).unwrap();
    let called = Cell::new(false);
    assert_eq!(
        permit.with_relation_at(START, RelationId(999), || {
            called.set(true);
            123
        }),
        Ok(None)
    );
    assert!(!called.get());
    assert_eq!(permit.usage().work, 0);
    assert_eq!(
        permit.with_relation_at(START, RelationId(10), || 123),
        Ok(Some(123))
    );
    assert_eq!(permit.usage().work, 1);
}

#[test]
fn insufficient_budget_prevents_even_allowed_descriptor_open() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::MaxWork(0))
        .unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    let mut permit = verified.begin_read_at(BRANCH, START).unwrap();
    let called = Cell::new(false);
    assert_eq!(
        permit.with_relation_at(START, RelationId(10), || called.set(true)),
        Err(Error::LimitExceeded(LimitDimension::Work))
    );
    assert!(!called.get());
}

#[test]
fn edges_and_degree_require_both_visible_endpoints() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    let p = verified.predicates();
    assert!(p.allows_edge(RelationId(10), &[LabelId(1)], &[LabelId(2)]));
    assert!(!p.allows_edge(RelationId(999), &[LabelId(1)], &[LabelId(2)]));
    assert!(!p.allows_edge(RelationId(10), &[LabelId(999)], &[LabelId(2)]));
    assert!(!p.allows_edge(RelationId(10), &[LabelId(1)], &[LabelId(999)]));
    let degree = |extra_hidden_edges: usize| {
        let visible = [(RelationId(10), LabelId(2))];
        visible
            .into_iter()
            .chain(std::iter::repeat_n(
                (RelationId(10), LabelId(999)),
                extra_hidden_edges,
            ))
            .filter(|(relation, label)| p.allows_edge(*relation, &[LabelId(1)], &[*label]))
            .count()
    };
    assert_eq!(degree(0), 1);
    assert_eq!(degree(1000), 1);
}

#[test]
fn signature_tampering_fails() {
    let authority = authority();
    let mut bytes = authority.issue_at(&grant(), START).unwrap().encode();
    *bytes.last_mut().unwrap() ^= 1;
    let token = CapabilityToken::decode(&bytes).unwrap();
    assert!(matches!(
        authority.verify_at(&token, BRANCH, START),
        Err(Error::Unauthenticated)
    ));
}

#[test]
fn every_byte_mutation_is_either_rejected_or_has_identical_authorized_semantics() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let bytes = token.encode();
    let expected = authority.verify_at(&token, BRANCH, START).unwrap();
    for index in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[index] ^= 1;
        if let Ok(candidate) = CapabilityToken::decode(&changed) {
            if let Ok(verified) = authority.verify_at(&candidate, BRANCH, START) {
                // Location is an unauthenticated hint; changing it is harmless
                // only because it has no influence on Warden authorization.
                assert_eq!(verified.predicates(), expected.predicates(), "byte {index}");
            }
        }
    }
}

#[test]
fn database_graph_catalog_and_policy_are_signed_and_noninterchangeable() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let cases = [
        (
            DatabaseSecurityNamespaceId([8; 32]),
            "graph",
            SchemaEpoch(3),
            9,
        ),
        (
            DatabaseSecurityNamespaceId([7; 32]),
            "other",
            SchemaEpoch(3),
            9,
        ),
        (
            DatabaseSecurityNamespaceId([7; 32]),
            "graph",
            SchemaEpoch(4),
            9,
        ),
        (
            DatabaseSecurityNamespaceId([7; 32]),
            "graph",
            SchemaEpoch(3),
            10,
        ),
    ];
    for (namespace, graph, schema, epoch) in cases {
        let other = Authority::new(key(), namespace, graph, schema, epoch).unwrap();
        assert!(matches!(
            other.verify_at(&token, BRANCH, START),
            Err(Error::WrongAuthority)
        ));
    }
}

#[test]
fn wrong_key_with_identical_authority_identity_fails() {
    let token = authority().issue_at(&grant(), START).unwrap();
    let other = Authority::new(
        AuthKey::from_seed(31),
        DatabaseSecurityNamespaceId([7; 32]),
        "graph",
        SchemaEpoch(3),
        9,
    )
    .unwrap();
    assert!(matches!(
        other.verify_at(&token, BRANCH, START),
        Err(Error::Unauthenticated)
    ));
}

#[test]
fn preverified_capability_cannot_be_transplanted_between_issuer_instances() {
    let expected = authority();
    let other = authority();
    let token = other.issue_at(&grant(), START).unwrap();
    let foreign = other.verify_at(&token, BRANCH, START).unwrap();
    assert_eq!(
        expected.recheck_at(&foreign, BRANCH, START),
        Err(Error::WrongAuthority)
    );
    let local = expected.verify_at(&token, BRANCH, START).unwrap();
    assert_eq!(expected.recheck_at(&local, BRANCH, START), Ok(()));
    assert_eq!(
        expected.recheck_at(&local, BRANCH, END),
        Err(Error::Expired)
    );
}

#[test]
fn removing_a_caveat_without_recomputing_the_signature_fails() {
    let authority = authority();
    let token = authority
        .issue_at(&grant(), START)
        .unwrap()
        .attenuate(Restriction::MaxWork(1))
        .unwrap();
    let mut bytes = token.encode();
    let read_len =
        |bytes: &[u8], at: usize| usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
    let mut pos = 1;
    pos += 2 + read_len(&bytes, pos);
    pos += 2 + read_len(&bytes, pos);
    let count_at = pos;
    let count = read_len(&bytes, pos);
    pos += 2;
    let mut last_start = pos;
    for _ in 0..count {
        last_start = pos;
        assert_eq!(bytes[pos], 0);
        pos += 3 + read_len(&bytes, pos + 1);
    }
    // Keep the child's signature, but physically remove the last caveat.
    drop(bytes.drain(last_start..pos));
    bytes[count_at..count_at + 2].copy_from_slice(&u16::try_from(count - 1).unwrap().to_le_bytes());
    let stripped = CapabilityToken::decode(&bytes).unwrap();
    assert!(matches!(
        authority.verify_at(&stripped, BRANCH, START),
        Err(Error::Unauthenticated)
    ));
}

#[test]
fn valid_signature_without_root_restrictions_is_not_authorization() {
    let authority = authority();
    let issued = authority.issue_at(&grant(), START).unwrap();
    let raw = raw(&issued);
    let bare = MacaroonToken::mint(&key(), raw.identifier(), raw.location());
    let bare = CapabilityToken::decode(&bare.to_binary()).unwrap();
    assert!(matches!(
        authority.verify_at(&bare, BRANCH, START),
        Err(Error::MissingRestriction)
    ));
}

#[test]
fn unknown_signed_caveat_is_never_ignored() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let unknown = append_raw(
        &token,
        CaveatPredicate::Custom("fgdb/warden/v2/admin".to_owned(), "true".to_owned()),
    );
    assert!(matches!(
        authority.verify_at(&unknown, BRANCH, START),
        Err(Error::UnsupportedCaveat)
    ));
}

#[test]
fn unsupported_foundation_caveats_fail_closed_at_ingress() {
    let token = authority().issue_at(&grant(), START).unwrap();
    for predicate in [
        CaveatPredicate::MaxUses(1),
        CaveatPredicate::RegionScope(1),
        CaveatPredicate::TaskScope(1),
        CaveatPredicate::ResourceScope("*".to_owned()),
        CaveatPredicate::RateLimit {
            max_count: 1,
            window_secs: 1,
        },
    ] {
        let bytes = raw(&token).add_caveat(predicate).to_binary();
        assert!(matches!(
            CapabilityToken::decode(&bytes),
            Err(Error::UnsupportedCaveat)
        ));
    }
    let third_party = raw(&token).add_third_party_caveat("issuer", "discharge", &key());
    assert!(matches!(
        CapabilityToken::decode(&third_party.to_binary()),
        Err(Error::UnsupportedCaveat)
    ));
}

#[test]
fn noncanonical_numeric_and_set_spellings_are_rejected() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    for (key, value) in [
        ("fgdb/warden/v1/max-work", "01"),
        ("fgdb/warden/v1/max-work", "+1"),
        ("fgdb/warden/v1/max-work", "18446744073709551616"),
        ("fgdb/warden/v1/rights", "4"),
        ("fgdb/warden/v1/relations", ""),
        ("fgdb/warden/v1/relations", "000000000000000A"),
        (
            "fgdb/warden/v1/relations",
            "000000000000000a,000000000000000a",
        ),
        (
            "fgdb/warden/v1/relations",
            "0000000000000014,000000000000000a",
        ),
        ("fgdb/warden/v1/deny-properties", "*"),
    ] {
        let candidate = append_raw(
            &token,
            CaveatPredicate::Custom(key.to_owned(), value.to_owned()),
        );
        assert!(
            matches!(
                authority.verify_at(&candidate, BRANCH, START),
                Err(Error::Malformed)
            ),
            "key={key} value={value}"
        );
    }
}

#[test]
fn trailing_and_truncated_wire_data_fail() {
    let token = authority().issue_at(&grant(), START).unwrap();
    let bytes = token.encode();
    for end in 0..bytes.len() {
        assert!(
            CapabilityToken::decode(&bytes[..end]).is_err(),
            "prefix length {end}"
        );
    }
    let mut trailing = bytes;
    trailing.push(0);
    assert!(CapabilityToken::decode(&trailing).is_err());
}

#[test]
fn unused_bytes_inside_predicate_packet_are_rejected() {
    let token = authority().issue_at(&grant(), START).unwrap();
    let mut bytes = token.encode();
    let read_len =
        |bytes: &[u8], at: usize| usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
    let mut pos = 1;
    pos += 2 + read_len(&bytes, pos); // identifier
    pos += 2 + read_len(&bytes, pos); // location
    pos += 2; // caveat count
    assert_eq!(bytes[pos], 0); // first-party packet
    let length_at = pos + 1;
    let length = read_len(&bytes, length_at);
    bytes.insert(length_at + 2 + length, 0);
    let enlarged = u16::try_from(length + 1).unwrap().to_le_bytes();
    bytes[length_at..length_at + 2].copy_from_slice(&enlarged);
    // Prove this witness reaches the foundation's lenient packet decoder.
    let decoded = MacaroonToken::from_binary(&bytes).unwrap();
    assert!(decoded.verify_signature(&key()));
    assert!(matches!(
        CapabilityToken::decode(&bytes),
        Err(Error::Malformed)
    ));
}

#[test]
fn input_size_and_caveat_count_are_bounded() {
    assert!(matches!(
        CapabilityToken::decode(&vec![0; MAX_TOKEN_BYTES + 1]),
        Err(Error::TooLarge)
    ));
    let authority = authority();
    let mut token = authority.issue_at(&grant(), START).unwrap();
    for _ in raw(&token).caveat_count()..MAX_CAVEATS {
        token = token.attenuate(Restriction::MaxWork(1000)).unwrap();
    }
    assert!(authority.verify_at(&token, BRANCH, START).is_ok());
    assert!(matches!(
        token.attenuate(Restriction::MaxWork(1000)),
        Err(Error::TooLarge)
    ));
}

#[test]
fn issuer_and_attenuator_reject_oversized_fields_without_serializing_them() {
    let authority = authority();
    let mut oversized = grant();
    oversized.branch = "x".repeat(MAX_NAME_BYTES + 1);
    assert!(matches!(
        authority.issue_at(&oversized, START),
        Err(Error::TooLarge)
    ));
    oversized = grant();
    oversized.labels = Scope::only((0..=MAX_SCOPE_ORDINALS).map(|i| LabelId(i as u64)));
    assert!(matches!(
        authority.issue_at(&oversized, START),
        Err(Error::TooLarge)
    ));
    let token = authority.issue_at(&grant(), START).unwrap();
    assert!(matches!(
        token.attenuate(Restriction::Branch("x".repeat(MAX_NAME_BYTES + 1))),
        Err(Error::TooLarge)
    ));
}

#[test]
fn debug_never_contains_bearer_or_authority_material() {
    let authority = authority();
    let token = authority.issue_at(&grant(), START).unwrap();
    let verified = authority.verify_at(&token, BRANCH, START).unwrap();
    assert_eq!(format!("{authority:?}"), "Authority(<redacted>)");
    assert_eq!(format!("{token:?}"), "CapabilityToken(<redacted>)");
    assert_eq!(format!("{verified:?}"), "VerifiedCapability(<redacted>)");
}
