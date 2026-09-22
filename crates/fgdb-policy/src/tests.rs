use super::*;

fn predicate(key: u64, comparison: Comparison, value: i64) -> PropertyPredicate {
    PropertyPredicate { key: PropertyKeyId(key), comparison, value: CanonicalScalar::Int(value) }
}

#[test]
fn all_caveat_arms_round_trip_and_reject_every_truncated_prefix() {
    let caveats = [
        ReadCaveat::Graphs(vec![GraphId(1), GraphId(u128::MAX)]),
        ReadCaveat::Branches(vec![BranchId(2)]),
        ReadCaveat::Labels(vec![LabelId(3)]),
        ReadCaveat::EdgeTypes(vec![RelationId(4)]),
        ReadCaveat::Properties(vec![]),
        ReadCaveat::Vertices(vec![VId(0), VId(5)]),
        ReadCaveat::HasLabel(LabelId(6)),
        ReadCaveat::VertexProperty(predicate(7, Comparison::GreaterOrEqual, i64::MIN)),
        ReadCaveat::EdgeProperty(predicate(8, Comparison::NotEqual, i64::MAX)),
        ReadCaveat::TimeWindow { not_before: 10, expires_at: u128::MAX },
        ReadCaveat::Snapshots { first: CommitSeq(0), last: CommitSeq(u64::MAX) },
        ReadCaveat::Limits(ReadLimits { rows: 0, work: 100, scratch: 200 }),
    ];
    for caveat in caveats {
        let bytes = caveat.to_bytes().unwrap();
        assert_eq!(ReadCaveat::from_bytes(&bytes).unwrap(), caveat);
        for end in 0..bytes.len() {
            assert!(ReadCaveat::from_bytes(&bytes[..end]).is_err(), "accepted prefix {end}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(ReadCaveat::from_bytes(&trailing).is_err());
        let mut version = bytes;
        version[0] = 255;
        assert_eq!(ReadCaveat::from_bytes(&version), Err(PolicyError::Unsupported));
    }
}

#[test]
fn canonical_vector_and_malformed_sets() {
    assert_eq!(ReadCaveat::Properties(vec![]).to_bytes().unwrap(), vec![1, 5, 0, 0]);
    assert_eq!(ReadCaveat::Labels(vec![LabelId(1), LabelId(1)]).to_bytes(), Err(PolicyError::NonCanonical));
    assert_eq!(ReadCaveat::Labels(vec![LabelId(2), LabelId(1)]).to_bytes(), Err(PolicyError::NonCanonical));
    assert_eq!(ReadCaveat::from_bytes(&[1, 3, 255, 255]), Err(PolicyError::Limit));
    assert_eq!(ReadCaveat::from_bytes(&[1, 255]), Err(PolicyError::Unsupported));
}

#[test]
fn exhaustive_small_scope_attenuation_never_widens() {
    for parent_bits in 0u8..16 {
        for child_bits in 0u8..16 {
            let ids = |bits: u8| (0..4).filter(|bit| bits & (1 << bit) != 0).map(VId).collect();
            let parent = ReadPolicy::compile(&[ReadCaveat::Vertices(ids(parent_bits))]).unwrap();
            let child = parent.attenuate(&[ReadCaveat::Vertices(ids(child_bits))]).unwrap();
            for id in 0..5 {
                let expected = parent.vertex_scope().contains(&VId(id)) && child_bits & (1 << id) != 0;
                assert_eq!(child.allows_vertex(VId(id), &[], &[]), expected);
            }
        }
    }
}

#[test]
fn predicates_conjoin_and_cannot_be_overwritten_by_delegation() {
    let parent = ReadPolicy::compile(&[ReadCaveat::VertexProperty(predicate(1, Comparison::Greater, 10))]).unwrap();
    let child = parent.attenuate(&[ReadCaveat::VertexProperty(predicate(1, Comparison::Less, 20))]).unwrap();
    for value in -1..30 {
        let properties = [(PropertyKeyId(1), CanonicalScalar::Int(value))];
        assert_eq!(child.allows_vertex(VId(1), &[], &properties), value > 10 && value < 20);
    }
    assert!(!child.allows_vertex(VId(1), &[], &[]));
    assert!(!child.allows_vertex(VId(1), &[], &[(PropertyKeyId(1), CanonicalScalar::Bool(true))]));
    assert!(!child.allows_vertex(VId(1), &[], &[(PropertyKeyId(1), CanonicalScalar::Null)]));
}

#[test]
fn hidden_secondary_labels_and_empty_scopes_fail_closed() {
    let policy = ReadPolicy::compile(&[ReadCaveat::Labels(vec![LabelId(1)])]).unwrap();
    assert!(policy.allows_vertex(VId(1), &[LabelId(1)], &[]));
    assert!(!policy.allows_vertex(VId(1), &[LabelId(1), LabelId(2)], &[]));
    assert!(!policy.allows_vertex(VId(1), &[], &[]));
    let deny = policy.attenuate(&[ReadCaveat::Labels(vec![])]).unwrap();
    assert!(!deny.allows_vertex(VId(1), &[LabelId(1)], &[]));
}

#[test]
fn authorization_precedes_property_disclosure() {
    let policy = ReadPolicy::compile(&[
        ReadCaveat::VertexProperty(predicate(1, Comparison::Equal, 7)),
        ReadCaveat::Properties(vec![PropertyKeyId(2)]),
    ]).unwrap();
    let source = [(PropertyKeyId(1), CanonicalScalar::Int(7)), (PropertyKeyId(2), CanonicalScalar::Int(8))];
    assert!(policy.allows_vertex(VId(1), &[], &source));
    assert_eq!(policy.project_properties(&source), vec![(PropertyKeyId(2), CanonicalScalar::Int(8))]);
}

#[test]
fn windows_and_ceilings_intersect_conservatively() {
    let policy = ReadPolicy::compile(&[
        ReadCaveat::TimeWindow { not_before: 10, expires_at: 20 },
        ReadCaveat::Snapshots { first: CommitSeq(5), last: CommitSeq(8) },
        ReadCaveat::Graphs(vec![GraphId(1)]),
        ReadCaveat::Branches(vec![BranchId(2)]),
        ReadCaveat::Limits(ReadLimits { rows: 10, work: 100, scratch: 50 }),
    ]).unwrap();
    let child = policy.attenuate(&[
        ReadCaveat::TimeWindow { not_before: 0, expires_at: 100 },
        ReadCaveat::Limits(ReadLimits { rows: 20, work: 5, scratch: 80 }),
    ]).unwrap();
    assert!(child.allows_time_interval(10, 19));
    assert!(!child.allows_time_interval(9, 10));
    assert!(!child.allows_time_interval(19, 20));
    assert!(!child.allows_time_interval(11, 10));
    assert_eq!(child.limits(), ReadLimits { rows: 10, work: 5, scratch: 50 });
    assert!(child.allows_snapshot(GraphId(1), BranchId(2), CommitSeq(5)));
    assert!(!child.allows_snapshot(GraphId(1), BranchId(3), CommitSeq(5)));
    assert!(!child.allows_snapshot(GraphId(1), BranchId(2), CommitSeq(9)));
    let empty = child.attenuate(&[ReadCaveat::TimeWindow { not_before: 30, expires_at: 40 }]).unwrap();
    assert!(!empty.allows_time_interval(35, 35));
}

#[test]
fn resource_bounds_and_debug_redaction() {
    let too_many = vec![ReadCaveat::HasLabel(LabelId(1)); MAX_READ_CAVEATS + 1];
    assert_eq!(ReadPolicy::compile(&too_many), Err(PolicyError::Limit));
    let max = ReadPolicy::compile(&too_many[..MAX_READ_CAVEATS]).unwrap();
    assert_eq!(max.attenuate(&[ReadCaveat::HasLabel(LabelId(2))]), Err(PolicyError::Limit));
    let value = ReadCaveat::VertexProperty(predicate(123456789, Comparison::Equal, 987654321));
    assert!(!format!("{value:?}").contains("123456789"));
    assert!(!format!("{value:?}").contains("987654321"));
}
