//! Signed write-budget observations depend on authorized fields, not preserved
//! hidden sidecars. These are real Warden tokens and public write checks; no
//! database, publication, allocator, timing or malformed-image isolation claim.

use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, VId};
use fgdb_warden::{
    Authority, CapabilityToken, EdgeWriteImage, Error, ExecutionPermit, Grant, LimitDimension,
    QueryLimits, Restriction, Rights, Scope, Usage, VertexWriteFields, VertexWriteImage,
    WriteAccess, WriteEndpoint,
};

const NOW: u64 = 100;
const L: LabelId = LabelId(10);
const P: PropertyKeyId = PropertyKeyId(10);
const Q: PropertyKeyId = PropertyKeyId(30);
const R: RelationId = RelationId(1);
const SECRET: PropertyKeyId = PropertyKeyId(11);
const HIDDEN_LABELS: [LabelId; 6] = [
    LabelId(1),
    LabelId(9),
    LabelId(11),
    LabelId(29),
    LabelId(30),
    LabelId(99),
];
const HIDDEN_KEYS: [PropertyKeyId; 6] = [
    PropertyKeyId(1),
    PropertyKeyId(9),
    PropertyKeyId(11),
    PropertyKeyId(29),
    PropertyKeyId(31),
    PropertyKeyId(99),
];

fn issuer() -> Authority {
    Authority::new(
        AuthKey::from_seed(917),
        DatabaseSecurityNamespaceId([19; 32]),
        "graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap()
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: u64::MAX,
            max_work: u64::MAX,
            max_rows: u64::MAX,
        },
    );
    grant.rights = Rights::Write;
    grant.labels = Scope::only([L, LabelId(30)]);
    grant.relations = Scope::only([R]);
    grant.properties = Scope::only([P, Q]);
    grant
}

fn token(issuer: &Authority) -> CapabilityToken {
    // Original vertex membership is CNF, but label visibility is the
    // intersection. Label 30 is hidden despite belonging to the root clause.
    issuer
        .issue_at(&grant(), NOW)
        .unwrap()
        .attenuate(Restriction::Labels(Scope::only([L, LabelId(20)])))
        .unwrap()
}

#[derive(Clone)]
struct Image {
    labels: Vec<LabelId>,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
}

impl Image {
    fn new(mask: usize, hidden_bytes: usize, value: i64) -> Self {
        let mut labels = vec![L];
        let mut properties = vec![
            (P, CanonicalScalar::Int(value)),
            (Q, CanonicalScalar::bytes(vec![7; 70]).unwrap()),
        ];
        for (index, label) in HIDDEN_LABELS.into_iter().enumerate() {
            if mask & (1 << index) != 0 {
                labels.push(label);
                properties.push((
                    HIDDEN_KEYS[index],
                    CanonicalScalar::bytes(vec![index as u8; hidden_bytes]).unwrap(),
                ));
            }
        }
        labels.sort_unstable();
        properties.sort_by_key(|(key, _)| *key);
        Self { labels, properties }
    }

    fn vertex(&self) -> VertexWriteImage<'_> {
        VertexWriteImage {
            id: VId(u128::MAX),
            labels: &self.labels,
            properties: &self.properties,
        }
    }

    fn edge(&self, self_loop: bool) -> EdgeWriteImage<'_> {
        EdgeWriteImage {
            id: EId(u128::MAX),
            relation: R,
            source: WriteEndpoint {
                id: VId(1),
                labels: &self.labels,
            },
            destination: WriteEndpoint {
                id: if self_loop { VId(1) } else { VId(u128::MAX) },
                labels: &self.labels,
            },
            properties: &self.properties,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Vertex,
    Edge,
    SelfLoop,
}
const KINDS: [Kind; 3] = [Kind::Vertex, Kind::Edge, Kind::SelfLoop];
type Trace = Vec<(Result<(), Error>, Usage)>;

fn check(
    permit: &mut ExecutionPermit<'_, WriteAccess>,
    kind: Kind,
    before: &Image,
    after: &Image,
    touched: &[PropertyKeyId],
) -> Result<(), Error> {
    match kind {
        Kind::Vertex => permit.check_vertex_write_at(
            NOW,
            Some(before.vertex()),
            Some(after.vertex()),
            VertexWriteFields {
                labels: &[],
                properties: touched,
            },
        ),
        Kind::Edge | Kind::SelfLoop => permit.check_edge_write_at(
            NOW,
            Some(before.edge(matches!(kind, Kind::SelfLoop))),
            Some(after.edge(matches!(kind, Kind::SelfLoop))),
            touched,
        ),
    }
}

fn trace(
    issuer: &Authority,
    token: &CapabilityToken,
    kind: Kind,
    before: &Image,
    after: &Image,
) -> Trace {
    let verified = issuer.verify_at(token, "main", NOW).unwrap();
    let mut permit = verified.begin_write_at("main", NOW).unwrap();
    let mut observations = Vec::new();
    // Compare the entire public trace, not just final usage. A refusal at any
    // step stays terminal and must not be erased by the later no-op/reversion.
    for (before, after, touched) in [
        (before, after, &[P][..]),
        (after, after, &[P][..]),
        (after, before, &[P][..]),
        (before, before, &[][..]),
    ] {
        let result = check(&mut permit, kind, before, after, touched);
        observations.push((result, permit.usage()));
    }
    let delivery = permit.charge_rows_at(NOW, 1);
    observations.push((delivery, permit.usage()));
    let final_check = permit.checkpoint_at(NOW);
    observations.push((final_check, permit.usage()));
    observations
}

#[test]
fn hidden_cardinality_and_interleaving_do_not_change_any_public_write_checkpoint() {
    let issuer = issuer();
    let token = token(&issuer);
    let before = Image::new(0, 0, 1);
    let after = Image::new(0, 0, 2);
    for kind in KINDS {
        let expected = trace(&issuer, &token, kind, &before, &after);
        assert!(expected.iter().all(|(result, _)| result.is_ok()));
        for mask in 0..64 {
            let full_before = Image::new(mask, 257, 1);
            let full_after = Image::new(mask, 257, 2);
            assert_eq!(
                trace(&issuer, &token, kind, &full_before, &full_after),
                expected,
                "kind={kind:?}, mask={mask}"
            );
        }
    }
}

#[test]
fn hidden_payload_size_does_not_change_work_or_node_usage() {
    let issuer = issuer();
    let token = token(&issuer);
    for kind in KINDS {
        let expected = trace(
            &issuer,
            &token,
            kind,
            &Image::new(0, 0, 1),
            &Image::new(0, 0, 2),
        );
        for bytes in [0, 1, 63, 64, 65, 511, 4096, 8192] {
            assert_eq!(
                trace(
                    &issuer,
                    &token,
                    kind,
                    &Image::new(63, bytes, 1),
                    &Image::new(63, bytes, 2),
                ),
                expected,
                "kind={kind:?}, hidden_bytes={bytes}"
            );
        }
    }
}

#[test]
fn every_signed_work_node_and_row_boundary_has_the_same_refusal_trace() {
    let issuer = issuer();
    let token = token(&issuer);
    let (a, b) = (Image::new(0, 0, 1), Image::new(0, 0, 2));
    let (full_a, full_b) = (Image::new(63, 4097, 1), Image::new(63, 4097, 2));
    for kind in KINDS {
        let complete = trace(&issuer, &token, kind, &a, &b);
        let usage = complete.last().unwrap().1;
        for restriction in (0..=usage.work + 1)
            .map(Restriction::MaxWork)
            .chain((0..=usage.nodes + 1).map(Restriction::MaxNodes))
            .chain((0..=2).map(Restriction::MaxRows))
        {
            let attenuated = token.attenuate(restriction).unwrap();
            let expected = trace(&issuer, &attenuated, kind, &a, &b);
            assert_eq!(
                trace(&issuer, &attenuated, kind, &full_a, &full_b),
                expected
            );
            let mut stopped = false;
            for (result, _) in expected {
                if stopped {
                    assert_eq!(result, Err(Error::ExecutionStopped));
                }
                stopped |= result.is_err();
            }
        }
        let exact = token
            .attenuate(Restriction::MaxWork(usage.work))
            .unwrap()
            .attenuate(Restriction::MaxNodes(usage.nodes))
            .unwrap()
            .attenuate(Restriction::MaxRows(usage.rows))
            .unwrap();
        assert_eq!(trace(&issuer, &exact, kind, &full_a, &full_b), complete);
    }
}

fn minimum_work(
    issuer: &Authority,
    token: &CapabilityToken,
    kind: Kind,
    before: &Image,
    after: &Image,
) -> u64 {
    let complete = trace(issuer, token, kind, before, after);
    assert!(complete.iter().all(|(result, _)| result.is_ok()));
    let mut low = 0;
    let mut high = complete.last().unwrap().1.work;
    while low < high {
        let middle = low + (high - low) / 2;
        let narrowed = token.attenuate(Restriction::MaxWork(middle)).unwrap();
        if trace(issuer, &narrowed, kind, before, after)
            .iter()
            .all(|(result, _)| result.is_ok())
        {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

#[test]
fn binary_search_attacker_cannot_measure_preserved_secrets() {
    let issuer = issuer();
    let token = token(&issuer);
    for kind in KINDS {
        let empty = minimum_work(
            &issuer,
            &token,
            kind,
            &Image::new(0, 0, 1),
            &Image::new(0, 0, 2),
        );
        for mask in [1, 21, 63] {
            for bytes in [1, 8192] {
                assert_eq!(
                    minimum_work(
                        &issuer,
                        &token,
                        kind,
                        &Image::new(mask, bytes, 1),
                        &Image::new(mask, bytes, 2),
                    ),
                    empty
                );
            }
        }
    }
}

#[test]
fn deny_properties_and_deny_all_property_scope_share_the_same_accounting_law() {
    let issuer = issuer();
    let (a, b) = (Image::new(0, 0, 1), Image::new(0, 0, 2));
    let (full_a, full_b) = (Image::new(63, 4096, 1), Image::new(63, 4096, 2));
    let mut allowed = grant();
    allowed.properties = Scope::All;
    let token = issuer
        .issue_at(&allowed, NOW)
        .unwrap()
        .attenuate(Restriction::Labels(Scope::only([L])))
        .unwrap()
        .attenuate(Restriction::DenyProperties(
            HIDDEN_KEYS.into_iter().collect(),
        ))
        .unwrap();
    for kind in KINDS {
        assert_eq!(
            trace(&issuer, &token, kind, &full_a, &full_b),
            trace(&issuer, &token, kind, &a, &b)
        );
    }
    let mut no_properties = grant();
    no_properties.properties = Scope::only([]);
    let token = issuer.issue_at(&no_properties, NOW).unwrap();
    let verified = issuer.verify_at(&token, "main", NOW).unwrap();
    let mut left = verified.begin_write_at("main", NOW).unwrap();
    let mut right = verified.begin_write_at("main", NOW).unwrap();
    let empty = Image {
        labels: vec![L],
        properties: vec![],
    };
    let mut full = full_a.clone();
    full.labels = vec![L];
    for kind in KINDS {
        assert_eq!(check(&mut left, kind, &empty, &empty, &[]), Ok(()));
        assert_eq!(check(&mut right, kind, &full, &full, &[]), Ok(()));
        assert_eq!(left.usage(), right.usage());
    }
}

#[test]
fn hidden_fields_remain_exactly_validated_and_cannot_be_changed_or_deleted() {
    let issuer = issuer();
    let token = token(&issuer);
    let original = Image::new(63, 257, 1);
    let mut changed = original.clone();
    changed
        .properties
        .iter_mut()
        // ubs:ignore -- SECRET is a hidden property KEY in this fixture, not secret material.
        .find(|(key, _)| *key == SECRET)
        .unwrap()
        .1 = CanonicalScalar::Int(9);
    let mut removed = original.clone();
    // ubs:ignore -- SECRET is a hidden property KEY in this fixture, not secret material.
    removed.properties.retain(|(key, _)| *key != SECRET);
    let mut duplicate = original.clone();
    let at = duplicate
        .properties
        .iter()
        // ubs:ignore -- SECRET is a hidden property KEY in this fixture, not secret material.
        .position(|(key, _)| *key == SECRET)
        .unwrap();
    duplicate
        .properties
        .insert(at, duplicate.properties[at].clone());
    for kind in KINDS {
        for invalid in [&changed, &removed, &duplicate] {
            let verified = issuer.verify_at(&token, "main", NOW).unwrap();
            let mut permit = verified.begin_write_at("main", NOW).unwrap();
            assert_eq!(
                check(&mut permit, kind, &original, invalid, &[]),
                Err(Error::InvalidWriteImage)
            );
            assert_eq!(permit.checkpoint_at(NOW), Err(Error::ExecutionStopped));
        }
        let verified = issuer.verify_at(&token, "main", NOW).unwrap();
        let mut permit = verified.begin_write_at("main", NOW).unwrap();
        assert_eq!(
            check(&mut permit, kind, &original, &original, &[SECRET]),
            Err(Error::ScopeDenied)
        );
    }
    // Hidden labels still have canonical ordering and exact preservation.
    for labels in [vec![LabelId(1), L], vec![LabelId(1), LabelId(1), L]] {
        let invalid = Image {
            labels,
            properties: original.properties.clone(),
        };
        let verified = issuer.verify_at(&token, "main", NOW).unwrap();
        let mut permit = verified.begin_write_at("main", NOW).unwrap();
        assert_eq!(
            check(&mut permit, Kind::Vertex, &original, &invalid, &[]),
            Err(Error::InvalidWriteImage)
        );
    }
    // A self-loop with inconsistent original endpoint labels is still invalid,
    // even when the disagreement concerns a label that is hidden to the holder.
    let verified = issuer.verify_at(&token, "main", NOW).unwrap();
    let mut permit = verified.begin_write_at("main", NOW).unwrap();
    let mut malformed = original.edge(true);
    malformed.destination.labels = &[L];
    assert_eq!(
        permit.check_edge_write_at(NOW, Some(malformed), Some(malformed), &[]),
        Err(Error::InvalidWriteImage)
    );
}

#[test]
fn visible_payloads_still_spend_real_work_and_zero_budgets_still_refuse() {
    let issuer = issuer();
    let mut full = grant();
    full.labels = Scope::All;
    full.properties = Scope::All;
    let token = issuer.issue_at(&full, NOW).unwrap();
    for kind in KINDS {
        let base = trace(
            &issuer,
            &token,
            kind,
            &Image::new(0, 0, 1),
            &Image::new(0, 0, 2),
        );
        let large = trace(
            &issuer,
            &token,
            kind,
            &Image::new(63, 8192, 1),
            &Image::new(63, 8192, 2),
        );
        assert!(large.iter().all(|(result, _)| result.is_ok()));
        assert!(large.last().unwrap().1.work > base.last().unwrap().1.work);
        let narrowed = token
            .attenuate(Restriction::MaxWork(base.last().unwrap().1.work))
            .unwrap();
        let failed = trace(
            &issuer,
            &narrowed,
            kind,
            &Image::new(63, 8192, 1),
            &Image::new(63, 8192, 2),
        );
        assert!(
            failed
                .iter()
                .any(|(result, _)| { *result == Err(Error::LimitExceeded(LimitDimension::Work)) })
        );
        let zero = token.attenuate(Restriction::MaxNodes(0)).unwrap();
        assert_eq!(
            trace(
                &issuer,
                &zero,
                kind,
                &Image::new(0, 0, 1),
                &Image::new(0, 0, 2),
            )[0]
            .0,
            Err(Error::LimitExceeded(LimitDimension::Nodes))
        );
    }
}

#[test]
fn expiry_retirement_and_clock_rollback_remain_terminal_with_hidden_fields() {
    for mode in 0..3 {
        let issuer = issuer();
        let token = token(&issuer);
        let verified = issuer.verify_at(&token, "main", NOW).unwrap();
        let mut permit = verified.begin_write_at("main", NOW).unwrap();
        let image = Image::new(63, 4096, 1);
        assert_eq!(
            check(&mut permit, Kind::Vertex, &image, &image, &[]),
            Ok(())
        );
        let usage = permit.usage();
        let (at, expected) = match mode {
            0 => (1000, Error::Expired),
            1 => {
                issuer.retire();
                (NOW, Error::AuthorityRetired)
            }
            _ => (NOW - 1, Error::ClockWentBackwards),
        };
        assert_eq!(
            permit.check_vertex_write_at(
                at,
                Some(image.vertex()),
                Some(image.vertex()),
                VertexWriteFields {
                    labels: &[],
                    properties: &[],
                },
            ),
            Err(expected)
        );
        assert_eq!(permit.usage(), usage);
        assert_eq!(permit.checkpoint_at(NOW), Err(Error::ExecutionStopped));
    }
}
