//! Real-token authorization over borrowed native field images. This tests the
//! Warden validator, not database execution, storage constraints or publication.
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::{CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, VId};
use fgdb_warden::{
    Authority, EdgeWriteImage, Error, ExecutionPermit, Grant, LimitDimension, QueryLimits,
    Restriction, Rights, Scope, Usage, VertexWriteFields, VertexWriteImage, WriteAccess,
    WriteEndpoint,
};

const NOW: u64 = 100;
const BRANCH: &str = "main";
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const NONE: VertexWriteFields<'static> = VertexWriteFields {
    labels: &[],
    properties: &[],
};

fn issuer() -> Authority {
    Authority::new(
        AuthKey::from_seed(901),
        DatabaseSecurityNamespaceId([17; 32]),
        "graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        1000,
        QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 0,
        },
    );
    grant.rights = Rights::Write;
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([P]);
    grant.relations = Scope::only([R]);
    grant
}
fn vertex<'a>(
    labels: &'a [LabelId],
    properties: &'a [(PropertyKeyId, CanonicalScalar)],
) -> VertexWriteImage<'a> {
    VertexWriteImage {
        id: VId(u128::MAX),
        labels,
        properties,
    }
}
fn edge<'a>(
    source_labels: &'a [LabelId],
    destination_labels: &'a [LabelId],
    properties: &'a [(PropertyKeyId, CanonicalScalar)],
) -> EdgeWriteImage<'a> {
    EdgeWriteImage {
        id: EId(u128::MAX),
        relation: R,
        source: WriteEndpoint {
            id: VId(1),
            labels: source_labels,
        },
        destination: WriteEndpoint {
            id: VId(u128::MAX),
            labels: destination_labels,
        },
        properties,
    }
}
fn with_permit<T>(
    grant: &Grant,
    run: impl FnOnce(&mut ExecutionPermit<'_, WriteAccess>) -> T,
) -> T {
    let issuer = issuer();
    let token = issuer.issue_at(grant, NOW).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, NOW).unwrap();
    let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
    run(&mut permit)
}

#[test]
fn create_update_delete_vertices_and_edges_share_one_permit() {
    let one = [(P, CanonicalScalar::Int(1))];
    let two = [(P, CanonicalScalar::Int(2))];
    let a = vertex(&[L], &one);
    let b = vertex(&[L], &two);
    let fields = VertexWriteFields {
        labels: &[L],
        properties: &[P],
    };
    with_permit(&grant(), |permit| {
        permit.check_vertex_write_at(NOW, None, Some(a), fields).unwrap();
        permit.check_vertex_write_at(NOW, Some(a), Some(b), fields).unwrap();
        permit.check_vertex_write_at(NOW, Some(b), None, fields).unwrap();
        let a = edge(&[L], &[L], &one);
        let b = edge(&[L], &[L], &two);
        permit.check_edge_write_at(NOW, None, Some(a), &[P]).unwrap();
        permit.check_edge_write_at(NOW, Some(a), Some(b), &[P]).unwrap();
        permit.check_edge_write_at(NOW, Some(b), None, &[P]).unwrap();
        assert_eq!(permit.usage().nodes, 12);
        assert_eq!(permit.usage().rows, 0);
        assert!(permit.usage().work > 12);
    });
    assert_eq!(one[0].1, CanonicalScalar::Int(1));
    assert_eq!(two[0].1, CanonicalScalar::Int(2));
}

#[test]
fn unchanged_hidden_fields_survive_but_cannot_be_written_or_deleted() {
    let old = [(P, CanonicalScalar::Int(1)), (SECRET, CanonicalScalar::Int(9))];
    let new = [(P, CanonicalScalar::Int(2)), (SECRET, CanonicalScalar::Int(9))];
    let before = vertex(&[L, HIDDEN], &old);
    let after = vertex(&[L, HIDDEN], &new);
    let permitted = VertexWriteFields { labels: &[], properties: &[P] };
    with_permit(&grant(), |permit| {
        permit.check_vertex_write_at(NOW, Some(before), Some(after), permitted).unwrap();
    });
    for fields in [
        VertexWriteFields { labels: &[], properties: &[SECRET] },
        VertexWriteFields { labels: &[HIDDEN], properties: &[] },
    ] {
        with_permit(&grant(), |permit| {
            // The two images are IDENTICAL: forbidden no-op writes still fail.
            assert_eq!(permit.check_vertex_write_at(NOW, Some(before), Some(before), fields), Err(Error::ScopeDenied));
            assert_eq!(permit.checkpoint_at(NOW), Err(Error::ExecutionStopped));
        });
    }
    with_permit(&grant(), |permit| {
        assert_eq!(permit.check_vertex_write_at(NOW, Some(before), None,
            VertexWriteFields { labels: &[L, HIDDEN], properties: &[P, SECRET] }), Err(Error::ScopeDenied));
    });
    with_permit(&grant(), |permit| {
        // Omitting the hidden fields cannot turn deletion into authorization.
        assert_eq!(permit.check_vertex_write_at(NOW, Some(before), None,
            VertexWriteFields { labels: &[L], properties: &[P] }), Err(Error::InvalidWriteImage));
    });
    let old_edge = edge(&[L], &[L], &old);
    with_permit(&grant(), |permit| {
        permit.check_edge_write_at(NOW, Some(old_edge), Some(edge(&[L], &[L], &new)), &[P]).unwrap();
        assert_eq!(permit.check_edge_write_at(NOW, Some(old_edge), Some(old_edge), &[SECRET]), Err(Error::ScopeDenied));
    });
}

#[test]
fn changed_or_absent_fields_cannot_escape_touched_field_admission() {
    let one = [(P, CanonicalScalar::Int(1))];
    let two = [(P, CanonicalScalar::Int(2))];
    let hidden = [(SECRET, CanonicalScalar::Null)];
    for (before, after) in [
        (Some(vertex(&[L], &one)), Some(vertex(&[L], &two))),
        (Some(vertex(&[L], &[])), Some(vertex(&[L], &one))),
        (Some(vertex(&[L], &one)), Some(vertex(&[L], &[]))),
        (Some(vertex(&[L], &[])), Some(vertex(&[L], &hidden))),
        (None, Some(vertex(&[L], &[]))),
        (Some(vertex(&[L], &[])), None),
    ] {
        with_permit(&grant(), |permit| {
            assert_eq!(permit.check_vertex_write_at(NOW, before, after, NONE), Err(Error::InvalidWriteImage));
        });
    }
    with_permit(&grant(), |permit| {
        let empty = vertex(&[L], &[]);
        // REMOVE missing.secret still names a forbidden property.
        assert_eq!(permit.check_vertex_write_at(NOW, Some(empty), Some(empty),
            VertexWriteFields { labels: &[], properties: &[SECRET] }), Err(Error::ScopeDenied));
    });
}

#[test]
fn original_before_and_after_scope_are_both_mandatory() {
    let mut grant = grant();
    grant.labels = Scope::only([L, HIDDEN]);
    let authority = issuer();
    let token = authority.issue_at(&grant, NOW).unwrap()
        .attenuate(Restriction::Labels(Scope::only([HIDDEN, LabelId(3)]))).unwrap();
    let verified = authority.verify_at(&token, BRANCH, NOW).unwrap();
    // Separate labels satisfy separate conjuncts. Their intersection is empty
    // in this row, but the ORIGINAL vertex is still inside both any-of scopes.
    let labels = [L, LabelId(3)];
    let original = vertex(&labels, &[]);
    let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
    permit.check_vertex_write_at(NOW, Some(original), Some(original), NONE).unwrap();
    for (before, after) in [
        (Some(vertex(&[L], &[])), Some(original)),
        (Some(original), Some(vertex(&[L], &[]))),
    ] {
        let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
        assert_eq!(permit.check_vertex_write_at(NOW, before, after, NONE), Err(Error::ScopeDenied));
    }
    let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
    assert_eq!(permit.check_vertex_write_at(NOW, Some(original), Some(original),
        VertexWriteFields { labels: &[L], properties: &[] }), Err(Error::ScopeDenied));
}

#[test]
fn edge_relation_and_both_endpoint_states_are_checked() {
    let good = edge(&[L], &[L], &[]);
    let bad_source = edge(&[HIDDEN], &[L], &[]);
    let bad_destination = edge(&[L], &[HIDDEN], &[]);
    for hidden in [bad_source, bad_destination] {
        for (before, after) in [
            (None, Some(hidden)),
            (Some(hidden), None),
            (Some(hidden), Some(good)),
            (Some(good), Some(hidden)),
        ] {
            with_permit(&grant(), |permit| {
                assert_eq!(permit.check_edge_write_at(NOW, before, after, &[]), Err(Error::ScopeDenied));
            });
        }
    }
    let hidden = EdgeWriteImage { relation: RelationId(2), ..good };
    for (before, after) in [(None, Some(hidden)), (Some(hidden), None)] {
        with_permit(&grant(), |permit| {
            assert_eq!(permit.check_edge_write_at(NOW, before, after, &[]), Err(Error::ScopeDenied));
            assert_eq!(permit.usage().nodes, 0);
        });
    }
}

#[test]
fn identities_and_topology_cannot_be_rewritten_under_an_update_check() {
    let good = edge(&[L], &[L], &[]);
    for changed in [
        EdgeWriteImage { id: EId(0), ..good },
        EdgeWriteImage { relation: RelationId(2), ..good },
        EdgeWriteImage { source: WriteEndpoint { id: VId(2), labels: &[L] }, ..good },
        EdgeWriteImage { destination: WriteEndpoint { id: VId(2), labels: &[L] }, ..good },
    ] {
        with_permit(&grant(), |permit| {
            assert_eq!(permit.check_edge_write_at(NOW, Some(good), Some(changed), &[]), Err(Error::InvalidWriteImage));
        });
    }
    with_permit(&grant(), |permit| {
        let before = vertex(&[L], &[]);
        let after = VertexWriteImage { id: VId(0), ..before };
        assert_eq!(permit.check_vertex_write_at(NOW, Some(before), Some(after), NONE), Err(Error::InvalidWriteImage));
    });
}

#[test]
fn inconsistent_self_loops_and_noncanonical_images_are_rejected_not_repaired() {
    let good = edge(&[L], &[L], &[]);
    let self_loop = EdgeWriteImage { destination: good.source, ..good };
    with_permit(&grant(), |permit| {
        permit.check_edge_write_at(NOW, None, Some(self_loop), &[]).unwrap();
        assert_eq!(permit.usage().nodes, 2);
    });
    let inconsistent = EdgeWriteImage {
        destination: WriteEndpoint { id: good.source.id, labels: &[L, HIDDEN] },
        ..self_loop
    };
    with_permit(&grant(), |permit| {
        assert_eq!(permit.check_edge_write_at(NOW, None, Some(inconsistent), &[]), Err(Error::InvalidWriteImage));
    });
    let duplicate = [(P, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(2))];
    let reversed = [(SECRET, CanonicalScalar::Int(1)), (P, CanonicalScalar::Int(2))];
    for image in [
        vertex(&[L, L], &[]),
        vertex(&[HIDDEN, L], &[]),
        vertex(&[L], &duplicate),
        vertex(&[L], &reversed),
    ] {
        with_permit(&grant(), |permit| {
            assert_eq!(permit.check_vertex_write_at(NOW, Some(image), Some(image), NONE), Err(Error::InvalidWriteImage));
        });
    }
    with_permit(&grant(), |permit| {
        assert_eq!(permit.check_vertex_write_at(NOW, None, None, NONE), Err(Error::InvalidWriteImage));
    });
    with_permit(&grant(), |permit| {
        assert_eq!(permit.check_edge_write_at(NOW, None, None, &[]), Err(Error::InvalidWriteImage));
    });
}

#[test]
fn malformed_touched_fields_stop_before_target_admission() {
    let mut grant = grant();
    grant.labels = Scope::All;
    grant.properties = Scope::All;
    for fields in [
        VertexWriteFields { labels: &[L, L], properties: &[] },
        VertexWriteFields { labels: &[HIDDEN, L], properties: &[] },
        VertexWriteFields { labels: &[], properties: &[P, P] },
        VertexWriteFields { labels: &[], properties: &[SECRET, P] },
    ] {
        with_permit(&grant, |permit| {
            assert_eq!(permit.check_vertex_write_at(NOW, None, Some(vertex(&[L], &[])), fields), Err(Error::InvalidWriteImage));
            assert_eq!(permit.usage().nodes, 0);
            assert_eq!(permit.checkpoint_at(NOW), Err(Error::ExecutionStopped));
        });
    }
}

#[test]
fn untouched_scalars_use_exact_canonical_equality_not_encoding_length() {
    let values = [
        CanonicalScalar::Null,
        CanonicalScalar::Bool(false),
        CanonicalScalar::Int(0),
        CanonicalScalar::Int(1),
        CanonicalScalar::Float(CanonicalF64::new(-0.0)),
        CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
        CanonicalScalar::ucs_basic_text("secret-a").unwrap(),
        CanonicalScalar::ucs_basic_text("secret-b").unwrap(),
        CanonicalScalar::bytes(vec![0, 1, 2]).unwrap(),
        CanonicalScalar::bytes(vec![0, 1, 3]).unwrap(),
    ];
    for old in &values {
        for new in &values {
            let a = [(SECRET, old.clone())];
            let b = [(SECRET, new.clone())];
            with_permit(&grant(), |permit| {
                let result = permit.check_vertex_write_at(NOW,
                    Some(vertex(&[L], &a)), Some(vertex(&[L], &b)), NONE);
                // Independent canonical encoding is the test oracle.
                assert_eq!(result.is_ok(), old.encode().unwrap() == new.encode().unwrap());
            });
        }
    }
}

#[test]
fn exact_limits_succeed_one_below_fails_and_checks_do_not_refresh_allowances() {
    let properties = [(SECRET, CanonicalScalar::ucs_basic_text(&"s".repeat(4096)).unwrap())];
    let image = vertex(&[L, HIDDEN], &properties);
    let measured = with_permit(&grant(), |permit| {
        permit.check_vertex_write_at(NOW, Some(image), Some(image), NONE).unwrap();
        permit.usage()
    });
    assert_eq!(measured.nodes, 2);
    assert_eq!(measured.rows, 0);
    assert!(measured.work > 64);
    let mut exact = grant();
    exact.limits.max_nodes = measured.nodes;
    exact.limits.max_work = measured.work;
    with_permit(&exact, |permit| {
        permit.check_vertex_write_at(NOW, Some(image), Some(image), NONE).unwrap();
        assert_eq!(permit.usage(), measured);
        assert_eq!(permit.check_vertex_write_at(NOW, Some(image), Some(image), NONE),
            Err(Error::LimitExceeded(LimitDimension::Nodes)));
        assert_eq!(permit.usage(), measured);
    });
    for dimension in [LimitDimension::Nodes, LimitDimension::Work] {
        let mut small = exact.clone();
        match dimension {
            LimitDimension::Nodes => small.limits.max_nodes -= 1,
            LimitDimension::Work => small.limits.max_work -= 1,
            LimitDimension::Rows => unreachable!(),
        }
        with_permit(&small, |permit| {
            assert_eq!(permit.check_vertex_write_at(NOW, Some(image), Some(image), NONE),
                Err(Error::LimitExceeded(dimension)));
            let stopped = permit.usage();
            assert_eq!(permit.checkpoint_at(NOW), Err(Error::ExecutionStopped));
            assert_eq!(permit.usage(), stopped);
        });
    }
}

#[test]
fn retirement_expiry_and_backwards_time_fence_write_checks() {
    let authority = issuer();
    let token = authority.issue_at(&grant(), NOW).unwrap();
    let verified = authority.verify_at(&token, BRANCH, NOW).unwrap();
    let image = vertex(&[L], &[]);
    let fields = VertexWriteFields { labels: &[L], properties: &[] };
    let mut retired = verified.begin_write_at(BRANCH, NOW).unwrap();
    let mut expired = verified.begin_write_at(BRANCH, NOW).unwrap();
    let mut backwards = verified.begin_write_at(BRANCH, NOW + 1).unwrap();
    assert_eq!(expired.check_vertex_write_at(1000, None, Some(image), fields), Err(Error::Expired));
    assert_eq!(backwards.check_vertex_write_at(NOW, None, Some(image), fields), Err(Error::ClockWentBackwards));
    authority.retire();
    assert_eq!(retired.check_edge_write_at(NOW, None, Some(edge(&[L], &[L], &[])), &[]), Err(Error::AuthorityRetired));
    for permit in [&mut retired, &mut expired, &mut backwards] {
        assert_eq!(permit.usage(), Usage::default());
        assert_eq!(permit.checkpoint_at(NOW), Err(Error::ExecutionStopped));
    }
}

#[test]
fn write_only_is_sufficient_read_only_is_not_and_debug_is_redacted() {
    let authority = issuer();
    let mut read = grant();
    read.rights = Rights::Read;
    let token = authority.issue_at(&read, NOW).unwrap();
    let verified = authority.verify_at(&token, BRANCH, NOW).unwrap();
    assert!(matches!(verified.begin_write_at(BRANCH, NOW), Err(Error::PermissionDenied)));
    let secret = [(SECRET, CanonicalScalar::ucs_basic_text("do-not-print").unwrap())];
    assert_eq!(format!("{:?}", vertex(&[L, HIDDEN], &secret)), "VertexWriteImage([REDACTED])");
    let edge = edge(&[L], &[L], &secret);
    assert_eq!(format!("{edge:?}"), "EdgeWriteImage([REDACTED])");
    assert_eq!(format!("{:?}", edge.source), "WriteEndpoint([REDACTED])");
    assert_eq!(format!("{NONE:?}"), "VertexWriteFields([REDACTED])");
}

#[test]
fn all_small_property_transitions_match_independent_field_authority_oracle() {
    let authority = issuer();
    for allowed in 0_u8..4 {
        let mut grant = grant();
        grant.properties = Scope::only((0..2).filter(|bit| allowed & (1 << bit) != 0)
            .map(|bit| PropertyKeyId(bit + 1)));
        let token = authority.issue_at(&grant, NOW).unwrap();
        let verified = authority.verify_at(&token, BRANCH, NOW).unwrap();
        // A property has four states: absent, null, zero, one.
        let state = |bits: u8| -> Vec<(PropertyKeyId, CanonicalScalar)> {
            (0..2).filter_map(|bit| {
                let value = match (bits >> (2 * bit)) & 3 {
                    0 => return None,
                    1 => CanonicalScalar::Null,
                    2 => CanonicalScalar::Int(0),
                    _ => CanonicalScalar::Int(1),
                };
                Some((PropertyKeyId(bit + 1), value))
            }).collect()
        };
        for old in 0..16 {
            for new in 0..16 {
                let a = state(old);
                let b = state(new);
                for touched in 0_u8..4 {
                    let keys: Vec<_> = (0..2).filter(|bit| touched & (1 << bit) != 0)
                        .map(|bit| PropertyKeyId(bit + 1)).collect();
                    let expected = touched & !allowed == 0 && (0..2).all(|bit| {
                        ((old >> (2 * bit)) & 3) == ((new >> (2 * bit)) & 3)
                            || touched & (1 << bit) != 0
                    });
                    let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
                    let actual = permit.check_vertex_write_at(NOW,
                        Some(vertex(&[L], &a)), Some(vertex(&[L], &b)),
                        VertexWriteFields { labels: &[], properties: &keys });
                    assert_eq!(actual.is_ok(), expected,
                        "allow={allowed} before={old} after={new} touched={touched}");
                }
            }
        }
    }
}

#[test]
fn all_small_label_transitions_preserve_conjunction_and_field_masks() {
    let authority = issuer();
    let labels = |bits: u8| -> Vec<LabelId> {
        (0..3).filter(|bit| bits & (1 << bit) != 0)
            .map(|bit| LabelId(bit + 1)).collect()
    };
    let mut admitted = 0;
    let mut refused = 0;
    for first in 0_u8..8 {
        for second in 0_u8..8 {
            let mut grant = grant();
            grant.labels = Scope::only(labels(first));
            let token = authority.issue_at(&grant, NOW).unwrap()
                .attenuate(Restriction::Labels(Scope::only(labels(second)))).unwrap();
            let verified = authority.verify_at(&token, BRANCH, NOW).unwrap();
            for old in 0_u8..8 {
                for new in 0_u8..8 {
                    let a = labels(old);
                    let b = labels(new);
                    for touched in 0_u8..8 {
                        let changed = old ^ new;
                        let expected = old & first != 0 && old & second != 0
                            && new & first != 0 && new & second != 0
                            && touched & !(first & second) == 0
                            && changed & !touched == 0;
                        let fields = labels(touched);
                        let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
                        let result = permit.check_vertex_write_at(NOW,
                            Some(vertex(&a, &[])), Some(vertex(&b, &[])),
                            VertexWriteFields { labels: &fields, properties: &[] });
                        assert_eq!(result.is_ok(), expected,
                            "first={first} second={second} old={old} new={new} touched={touched}");
                        if expected { admitted += 1; } else { refused += 1; }
                    }
                }
            }
        }
    }
    assert!(admitted > 0 && refused > 0);
    assert_eq!(admitted + refused, 32_768);
}

#[test]
fn explicit_property_denials_survive_later_wildcards_for_both_row_kinds() {
    let authority = issuer();
    let mut grant = grant();
    grant.properties = Scope::All;
    let token = authority.issue_at(&grant, NOW).unwrap()
        .attenuate(Restriction::DenyProperties([SECRET].into_iter().collect())).unwrap()
        .attenuate(Restriction::Properties(Scope::All)).unwrap();
    let verified = authority.verify_at(&token, BRANCH, NOW).unwrap();
    let props = [(SECRET, CanonicalScalar::Int(5))];
    let image = vertex(&[L], &props);
    let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
    permit.check_vertex_write_at(NOW, Some(image), Some(image), NONE).unwrap();
    assert_eq!(permit.check_vertex_write_at(NOW, Some(image), Some(image),
        VertexWriteFields { labels: &[], properties: &[SECRET] }), Err(Error::ScopeDenied));
    let mut permit = verified.begin_write_at(BRANCH, NOW).unwrap();
    let image = edge(&[L], &[L], &props);
    permit.check_edge_write_at(NOW, Some(image), Some(image), &[]).unwrap();
    assert_eq!(permit.check_edge_write_at(NOW, Some(image), Some(image), &[SECRET]), Err(Error::ScopeDenied));
}
