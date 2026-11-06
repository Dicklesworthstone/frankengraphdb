use super::*;
use crate::AdjacencyEntry;
use crate::edge_props::EdgePropertyRow;
use fgdb_codec::identity::IdentityRepresentation;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, PurposeContexts, VId};

const KEY: [u8; 32] = [0x72; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x84; 32]);
const RELATION: RelationId = RelationId(9);

fn run(seed: u64, test: impl FnOnce(QueryCx) + Send + 'static) {
    let ((), report) = asupersync::lab::run_async_under_lab(seed, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root).query());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type Rows = Vec<(AdjacencyEntry, EdgePropertyRow)>;

fn source(raw: bool) -> (SealedPartition, BTreeMap<EId, OriginBirthOrder<EId>>, Rows) {
    let mut rows = Vec::new();
    let mut origins = BTreeMap::new();
    for id in 10..22 {
        let eid = EId(id);
        rows.push((
            AdjacencyEntry {
                src: VId(1),
                relation: RELATION,
                dst: VId(u128::MAX),
                eid,
                created_at: CommitSeq(1),
                retired_at: (id == 10).then_some(CommitSeq(3)),
            },
            vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64).into())],
        ));
        origins.insert(eid, OriginBirthOrder::new(CommitSeq(1), id as u64, 0, eid));
    }
    rows.push((
        AdjacencyEntry {
            src: VId(1),
            relation: RELATION,
            dst: VId(u128::MAX),
            eid: EId(10),
            created_at: CommitSeq(3),
            retired_at: None,
        },
        vec![(PropertyKeyId(1), CanonicalScalar::Int(99).into())],
    ));
    let high = VId((1_u128 << 127) + 19);
    let eid = EId((1_u128 << 120) + 7);
    rows.push((
        AdjacencyEntry {
            src: high,
            relation: RELATION,
            dst: high,
            eid,
            created_at: CommitSeq(2),
            retired_at: None,
        },
        Vec::new(),
    ));
    origins.insert(eid, OriginBirthOrder::new(CommitSeq(2), 6, 0, eid));
    rows.sort_by_key(|(entry, _)| {
        (
            entry.src,
            entry.relation,
            entry.dst,
            entry.eid,
            entry.created_at,
        )
    });
    let limits = SealedLimits::default();
    let mut image =
        super::super::image::build_retained(rows.clone(), limits, &mut || Ok(())).unwrap();
    if raw {
        for list in &mut image.runs {
            let values: Vec<u8> = (0..list.edge_ids.len())
                .flat_map(|at| list.edge_ids.get(at).unwrap().0.to_be_bytes())
                .collect();
            list.edge_ids = IdentityColumn::try_from_scalar_payload(
                &values,
                IdentityColumnDescriptor::new(
                    IdentityRepresentation::Raw128,
                    list.edge_ids.len(),
                    0,
                ),
                identity_limits(list.edge_ids.len(), limits),
            )
            .unwrap();
        }
    }
    let scope = SealedScope {
        source_root: PartitionRootVersion(ObjectId([0x91; 32])),
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        floor: CommitSeq::ORIGIN,
        publication: CommitSeq(4),
    };
    let partition =
        SealedPartition::finish_scoped(scope, Some(RELATION), image, limits, &mut || Ok(()))
            .unwrap();
    (partition, origins, rows)
}

fn read(cx: &QueryCx, segment: &AdjRunSegment, bytes: &[u8]) -> Result<AdjRunSegment, AdjRunError> {
    read_adj_run(
        cx,
        adj_run_id(&KEY, NAMESPACE, bytes),
        segment.header.scope,
        RELATION,
        &KEY,
        NAMESPACE,
        bytes,
        SealedLimits::default(),
        None,
    )
}

fn resign(cx: &QueryCx, bytes: &mut [u8]) {
    let end = bytes.len() - CHECKSUM_BYTES;
    let checksum = hash_chunks(cx, CHECKSUM_DOMAIN, &bytes[..end]).unwrap();
    bytes[end..].copy_from_slice(&checksum);
}

fn collect(cx: &QueryCx, source: &SealedPartition, src: VId, seq: u64) -> Rows {
    let mut cursor = source.row(cx, src, RELATION, CommitSeq(seq)).unwrap();
    let mut rows = Vec::new();
    while let Some(edge) = cursor.next(cx).unwrap() {
        rows.push((edge.entry, edge.properties.to_vec()));
    }
    rows
}

#[test]
fn empty_scoped_run_matches_the_literal_v1_golden_and_proves_only_its_relation() {
    run(0xf6_7200, |cx| {
        let limits = SealedLimits::default();
        let image =
            super::super::image::build_retained(Vec::new(), limits, &mut || Ok(())).unwrap();
        let partition = SealedPartition::finish_scoped(
            SealedScope {
                source_root: PartitionRootVersion(ObjectId([0x91; 32])),
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 0,
                floor: CommitSeq::ORIGIN,
                publication: CommitSeq(4),
            },
            Some(RELATION),
            image,
            limits,
            &mut || Ok(()),
        )
        .unwrap();
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &BTreeMap::new(), limits).unwrap();
        let bytes = segment.encode(&cx, limits).unwrap();
        // Independently assembled from the documented scalar layout: empty
        // raw identity columns, EF offsets [0], no retained entries/origins.
        // The production encoders did not generate this 345-byte vector.
        let expected = concat!(
            "4647524e01000100919191919191919191919191919191919191919191919191",
            "9191919191919191010000000000000000000000000000000200000000000000",
            "0000000000000000000000000000000009000000000000000000000000000000",
            "00040000000000000000000000000000000000000034d73792c03f26586dda94",
            "a167cdf2ac2eb82cd47d865d6494b8ed63dc7260f0abbd820ec1fa4178309c31",
            "55b72340ccc7fe3c7587c28846dafd2d923988b7ed8bfc8a37de2d612129cb4c",
            "2be59c1c2e390018d93ef02d95ffd9b55b7642bb3e899bbc1a4540cc6e7263f9",
            "dc3e0a242f04a3aaeee9f5c7c1fa61c15482c2d12c3300000000000000000000",
            "0000000000004647534901000000000000000000000000000000000000000000",
            "0000000000000000000000000008000000010000000000000097096d180f2ef6",
            "fcd843773aa8c02817d81d43ee1bdb15dfdfacce8753e908b9",
        );
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        assert_eq!(hex(&bytes), expected);
        assert_eq!(
            hex(&adj_run_id(&KEY, NAMESPACE, &bytes).0),
            "ace8b60fdb3cc4f910bfe162d662ccaa006e729e7b57ba6d6480dcbaf6a1fa34"
        );
        let reopened = read(&cx, &segment, &bytes).unwrap();
        assert_eq!(reopened.header.row_count, 0);
        assert_eq!(reopened.header.incidence_count, 0);
        assert_eq!(reopened.header.origin_count, 0);
        assert!(collect(&cx, reopened.partition(), VId(99), 4).is_empty());
        assert!(matches!(
            reopened
                .partition()
                .row(&cx, VId(99), RelationId(10), CommitSeq(4)),
            Err(SealedError::RelationOutsideAnchor { .. })
        ));
    });
}

#[test]
fn durable_run_roundtrip_preserves_parallel_edges_versions_properties_and_true_births() {
    run(0xf6_7201, |cx| {
        let (partition, origins, rows) = source(false);
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        assert_eq!(
            (
                segment.header.row_count,
                segment.header.incidence_count,
                segment.header.origin_count
            ),
            (2, 14, 13)
        );
        let bytes = segment.encode(&cx, SealedLimits::default()).unwrap();
        let reopened = read(&cx, &segment, &bytes).unwrap();
        assert_eq!(reopened.header(), segment.header());
        assert_eq!(
            reopened.encode(&cx, SealedLimits::default()).unwrap(),
            bytes
        );
        for seq in 0..=4 {
            for src in [VId(1), VId((1_u128 << 127) + 19), VId(20)] {
                let expected: Rows = rows
                    .iter()
                    .filter(|(entry, _)| {
                        entry.src == src
                            && entry.created_at.0 <= seq
                            && entry.retired_at.is_none_or(|end| seq < end.0)
                    })
                    .cloned()
                    .collect();
                assert_eq!(collect(&cx, reopened.partition(), src, seq), expected);
            }
        }
        assert_eq!(
            reopened.origin(EId(10)),
            Some(OriginBirthOrder::new(CommitSeq(1), 10, 0, EId(10)))
        );
        assert_eq!(collect(&cx, reopened.partition(), VId(1), 4).len(), 12);
        assert!(reopened.origin(EId(999)).is_none());
        assert!(matches!(
            partition.stable_edge_ids(&cx, 12),
            Err(SealedError::Limit { .. })
        ));
    });
}

#[test]
fn registered_identity_and_lineage_precede_any_forged_image_admission() {
    run(0xf6_7202, |cx| {
        let (partition, origins, _) = source(false);
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        let mut bytes = segment.encode(&cx, SealedLimits::default()).unwrap();
        let expected = adj_run_id(&KEY, NAMESPACE, &bytes);
        bytes[262] ^= 1;
        assert!(matches!(
            read_adj_run(
                &cx,
                expected,
                segment.header.scope,
                RELATION,
                &KEY,
                NAMESPACE,
                &bytes,
                SealedLimits::default(),
                None
            ),
            Err(AdjRunError::IdentityMismatch { .. })
        ));
        assert!(matches!(
            read(&cx, &segment, &bytes),
            Err(AdjRunError::ChecksumMismatch)
        ));
        bytes[262] ^= 1;
        let mut other = segment.header.scope;
        other.branch = BranchId(99);
        assert!(matches!(
            read_adj_run(
                &cx,
                expected,
                other,
                RELATION,
                &KEY,
                NAMESPACE,
                &bytes,
                SealedLimits::default(),
                None
            ),
            Err(AdjRunError::ScopeMismatch)
        ));
        assert!(matches!(
            read_adj_run(
                &cx,
                expected,
                segment.header.scope,
                RelationId(99),
                &KEY,
                NAMESPACE,
                &bytes,
                SealedLimits::default(),
                None
            ),
            Err(AdjRunError::ScopeMismatch)
        ));
        assert!(matches!(
            read_adj_run(
                &cx,
                expected,
                segment.header.scope,
                RELATION,
                &KEY,
                DatabaseSecurityNamespaceId([0x85; 32]),
                &bytes,
                SealedLimits::default(),
                None
            ),
            Err(AdjRunError::IdentityMismatch { .. })
        ));
        let mut changed_map = bytes.clone();
        changed_map[149] ^= 1;
        resign(&cx, &mut changed_map);
        assert!(matches!(
            read(&cx, &segment, &changed_map),
            Err(AdjRunError::MapMismatch)
        ));
        bytes[181] ^= 1;
        resign(&cx, &mut bytes);
        assert!(matches!(
            read(&cx, &segment, &bytes),
            Err(AdjRunError::StableDigestMismatch)
        ));
    });
}

#[test]
fn origins_must_cover_retained_identities_and_precede_every_content_version() {
    run(0xf6_7203, |cx| {
        let (partition, origins, _) = source(false);
        for invalid in [
            None,
            Some(OriginBirthOrder::new(CommitSeq(0), 10, 0, EId(10))),
            Some(OriginBirthOrder::new(CommitSeq(3), 10, 0, EId(10))),
            Some(OriginBirthOrder::new(CommitSeq(1), 10, 0, EId(99))),
        ] {
            let mut offered = origins.clone();
            match invalid {
                Some(origin) => {
                    offered.insert(EId(10), origin);
                }
                None => {
                    offered.remove(&EId(10));
                }
            }
            assert!(matches!(
                AdjRunSegment::from_sealed(&cx, &partition, &offered, SealedLimits::default()),
                Err(AdjRunError::MissingOrigin(EId(10)) | AdjRunError::InvalidOrigin(EId(10)))
            ));
        }
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        let bytes = segment.encode(&cx, SealedLimits::default()).unwrap();
        let image_len = u32::from_le_bytes(bytes[245..249].try_into().unwrap()) as usize;
        let ids_len = u32::from_le_bytes(bytes[254..258].try_into().unwrap()) as usize;
        let first_origin = 262 + image_len + ids_len;
        let mut zero_birth = bytes.clone();
        zero_birth[first_origin] = 0;
        resign(&cx, &mut zero_birth);
        assert!(matches!(
            read(&cx, &segment, &zero_birth),
            Err(AdjRunError::InvalidOrigin(EId(10)))
        ));
        let mut nonminimal = bytes;
        nonminimal[first_origin] = 0x80;
        nonminimal[first_origin + 1] = 0;
        resign(&cx, &mut nonminimal);
        assert!(matches!(
            read(&cx, &segment, &nonminimal),
            Err(AdjRunError::NonCanonical)
        ));
    });
}

#[test]
fn physical_identity_codec_substitution_keeps_the_stable_logical_digest() {
    run(0xf6_7204, |cx| {
        let (compressed, origins, _) = source(false);
        let (raw, _, _) = source(true);
        let a = AdjRunSegment::from_sealed(&cx, &compressed, &origins, SealedLimits::default())
            .unwrap();
        let b = AdjRunSegment::from_sealed(&cx, &raw, &origins, SealedLimits::default()).unwrap();
        assert_eq!(
            a.header.canonical_stable_id_digest,
            b.header.canonical_stable_id_digest
        );
        let left = a.encode(&cx, SealedLimits::default()).unwrap();
        let right = b.encode(&cx, SealedLimits::default()).unwrap();
        assert_ne!(left, right);
        assert_ne!(
            adj_run_id(&KEY, NAMESPACE, &left),
            adj_run_id(&KEY, NAMESPACE, &right)
        );
        let a = read(&cx, &a, &left).unwrap();
        let b = read(&cx, &b, &right).unwrap();
        for seq in 0..=4 {
            assert_eq!(
                collect(&cx, a.partition(), VId(1), seq),
                collect(&cx, b.partition(), VId(1), seq)
            );
        }
    });
}

#[test]
fn durable_run_refuses_unknown_profiles_oversized_counts_and_short_byte_budgets() {
    run(0xf6_7205, |cx| {
        let (partition, origins, _) = source(false);
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        let bytes = segment.encode(&cx, SealedLimits::default()).unwrap();
        let mut profile = bytes.clone();
        profile[6..8].copy_from_slice(&2_u16.to_le_bytes());
        resign(&cx, &mut profile);
        assert!(matches!(
            read(&cx, &segment, &profile),
            Err(AdjRunError::UnsupportedProfile(2))
        ));
        let mut count = bytes.clone();
        count[109..113].copy_from_slice(&u32::MAX.to_le_bytes());
        resign(&cx, &mut count);
        assert!(matches!(
            read(&cx, &segment, &count),
            Err(AdjRunError::Sealed(SealedError::Limit { .. }))
        ));
        let mut limits = SealedLimits {
            max_image_bytes: bytes.len(),
            ..SealedLimits::default()
        };
        assert_eq!(segment.encode(&cx, limits).unwrap(), bytes);
        limits.max_image_bytes -= 1;
        assert!(matches!(
            segment.encode(&cx, limits),
            Err(AdjRunError::Sealed(SealedError::Limit { .. }))
        ));
        assert!(matches!(
            read_adj_run(
                &cx,
                adj_run_id(&KEY, NAMESPACE, &bytes),
                segment.header.scope,
                RELATION,
                &KEY,
                NAMESPACE,
                &bytes,
                limits,
                None
            ),
            Err(AdjRunError::Sealed(SealedError::Limit { .. }))
        ));
    });
}
