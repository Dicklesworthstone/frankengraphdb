use super::*;
use crate::AdjacencyEntry;
use crate::edge_props::{BlockProps, EdgePropertyRow};
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
fn post_seal_overlay_binds_both_sources_to_one_admitted_envelope() {
    run(0x5ea1_2026, |cx| {
        use super::super::SealedDeltaOverlay;

        let limits = SealedLimits::default();
        let (partition, origins, _) = source(false);
        let unbound = AdjRunSegment::from_sealed(&cx, &partition, &origins, limits).unwrap();
        let root = PartitionRootVersion(ObjectId([0xa1; 32]));
        let foreign_root = PartitionRootVersion(ObjectId([0xb2; 32]));
        let publication = CommitSeq(7);
        let verified = CommitSeq(4);
        let as_of = CommitSeq(5);
        let added = AdjacencyEntry {
            src: VId(1),
            relation: RELATION,
            dst: VId(2),
            eid: EId(77),
            created_at: as_of,
            retired_at: None,
        };
        let suffix = crate::store::PostSealBlocks {
            source_root: root,
            publication,
            relation: RELATION,
            after_seq: verified,
            blocks: vec![vec![added]],
            props: vec![None],
        };

        // Matching public relation/sequence arguments alone are not evidence.
        let refused = |base| {
            assert!(matches!(
                SealedDeltaOverlay::build(&cx, base, root, verified, as_of, &suffix, limits,),
                Err(SealedError::ImageMismatch)
            ));
        };
        refused(unbound.clone());
        for (other_root, other_publication, other_verified) in [
            (foreign_root, publication, verified),
            (root, CommitSeq(6), verified),
            (root, publication, CommitSeq(5)),
        ] {
            let mut wrong = unbound.clone();
            wrong.bind_envelope(other_root, other_publication, other_verified);
            refused(wrong);
        }

        let mut base = unbound;
        base.bind_envelope(root, publication, verified);
        // A carried run retains its older source root; the admitted envelope,
        // rather than that original root, is what must agree with the suffix.
        assert_ne!(base.header().scope.source_root, root);
        let overlay =
            SealedDeltaOverlay::build(&cx, base, root, verified, as_of, &suffix, limits).unwrap();
        let materialized = overlay.materialize_partition(&cx, limits).unwrap();
        let mut expected = vec![(added, Vec::new())];
        expected.extend(collect(&cx, &partition, VId(1), verified.0));
        assert_eq!(collect(&cx, &materialized, VId(1), as_of.0), expected);
    });
}

#[test]
fn post_seal_overlay_rejects_impossible_proof_horizons_before_materialization() {
    run(0x5ea1_2027, |cx| {
        use super::super::SealedDeltaOverlay;

        let limits = SealedLimits::default();
        let (partition, origins, _) = source(false);
        let original = AdjRunSegment::from_sealed(&cx, &partition, &origins, limits).unwrap();
        let root = PartitionRootVersion(ObjectId([0xa1; 32]));
        for (publication, verified, as_of) in [(7, 3, 5), (4, 4, 5), (4, 5, 6)] {
            let publication = CommitSeq(publication);
            let verified = CommitSeq(verified);
            let mut base = original.clone();
            base.bind_envelope(root, publication, verified);
            let suffix = crate::store::PostSealBlocks {
                source_root: root,
                publication,
                relation: RELATION,
                after_seq: verified,
                blocks: Vec::new(),
                props: Vec::new(),
            };
            assert!(matches!(
                SealedDeltaOverlay::build(
                    &cx,
                    base,
                    root,
                    verified,
                    CommitSeq(as_of),
                    &suffix,
                    limits,
                ),
                Err(SealedError::ImageMismatch)
            ));
        }
    });
}

fn overlay_fixture(
    cx: &QueryCx,
    rows: Rows,
) -> (AdjRunSegment, crate::store::PostSealBlocks) {
    let (partition, origins, _) = source(false);
    let mut base =
        AdjRunSegment::from_sealed(cx, &partition, &origins, SealedLimits::default()).unwrap();
    let source_root = PartitionRootVersion(ObjectId([0xc3; 32]));
    let publication = CommitSeq(8);
    let after_seq = CommitSeq(4);
    base.bind_envelope(source_root, publication, after_seq);
    let (blocks, props) = rows
        .into_iter()
        .map(|(entry, row)| {
            let props = (!row.is_empty()).then(|| BlockProps {
                locators: vec![1],
                rows: vec![row],
            });
            (vec![entry], props)
        })
        .unzip();
    (
        base,
        crate::store::PostSealBlocks {
            source_root,
            publication,
            relation: RELATION,
            after_seq,
            blocks,
            props,
        },
    )
}

fn overlay_entry(id: u128, src: u128, dst: u128, created: u64) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: RELATION,
        dst: VId(dst),
        eid: EId(id),
        created_at: CommitSeq(created),
        retired_at: None,
    }
}

fn overlay_at_five(
    cx: &QueryCx,
    base: &AdjRunSegment,
    suffix: &crate::store::PostSealBlocks,
    limits: SealedLimits,
) -> Result<super::super::SealedDeltaOverlay, SealedError> {
    super::super::SealedDeltaOverlay::build(
        cx,
        base.clone(),
        suffix.source_root,
        suffix.after_seq,
        CommitSeq(5),
        suffix,
        limits,
    )
}

#[test]
fn post_seal_overlay_bounds_distinct_statements_but_coalesces_restatements() {
    run(0x5ea1_2030, |cx| {
        // The base has fourteen retained incidences. Fifteen distinct suffix
        // statements exceed the SAME explicit allowance before any row copy.
        let rows: Rows = (0..15)
            .map(|at| (overlay_entry(100 + at, 1, 2, 5), Vec::new()))
            .collect();
        let (base, suffix) = overlay_fixture(&cx, rows.clone());
        let limits = SealedLimits {
            max_incidences: 14,
            ..SealedLimits::default()
        };
        assert!(matches!(
            overlay_at_five(&cx, &base, &suffix, limits),
            Err(SealedError::Limit {
                resource: "overlay statements",
                requested: 15,
                limit: 14,
            })
        ));
        let exact = overlay_at_five(
            &cx,
            &base,
            &suffix,
            SealedLimits {
                max_incidences: 15,
                ..limits
            },
        )
        .unwrap();
        assert_eq!(exact.visible_delta_count(), 15);

        // Repeated physical statements have one logical winner; bounding raw
        // block rows instead would falsely refuse this otherwise tiny suffix.
        let (base, suffix) = overlay_fixture(&cx, vec![rows[0].clone(); 128]);
        let repeated = overlay_at_five(&cx, &base, &suffix, limits).unwrap();
        assert_eq!(repeated.visible_delta_count(), 1);
        let mut cursor = repeated.outgoing_cursor(&cx, VId(1)).unwrap();
        assert_eq!(cursor.next(&cx).unwrap().unwrap().entry, rows[0].0);
    });
}

#[test]
fn post_seal_overlay_bounds_both_faces_and_future_retained_descriptors() {
    run(0x5ea1_2031, |cx| {
        for (different_sources, created) in [(true, 5), (false, 5), (true, 7)] {
            let rows = (0..3)
                .map(|at| {
                    let (src, dst) = if different_sources {
                        (10 + at, 2)
                    } else {
                        (1, 10 + at)
                    };
                    (overlay_entry(100 + at, src, dst, created), Vec::new())
                })
                .collect();
            let (base, suffix) = overlay_fixture(&cx, rows);
            let limits = SealedLimits {
                max_rows: 2,
                ..SealedLimits::default()
            };
            assert!(matches!(
                overlay_at_five(&cx, &base, &suffix, limits),
                Err(SealedError::Limit {
                    resource: "overlay descriptors",
                    requested: 3,
                    limit: 2,
                })
            ));
            let exact = overlay_at_five(
                &cx,
                &base,
                &suffix,
                SealedLimits {
                    max_rows: 3,
                    ..limits
                },
            )
            .unwrap();
            assert_eq!(exact.visible_delta_count(), if created == 5 { 3 } else { 0 });
        }
    });
}

#[test]
fn post_seal_overlay_admits_future_properties_and_shares_visible_payloads() {
    run(0x5ea1_2032, |cx| {
        let property = vec![(PropertyKeyId(1), CanonicalScalar::Int(42).into())];
        let rows = vec![
            (overlay_entry(100, 1, 2, 5), property.clone()),
            (overlay_entry(101, 1, 3, 7), property.clone()),
        ];
        let (base, suffix) = overlay_fixture(&cx, rows);
        // Two rows: each is u32 count + u64 key + u32 length + 9-byte Int.
        let limits = SealedLimits {
            max_property_bytes: 49,
            ..SealedLimits::default()
        };
        assert!(matches!(
            overlay_at_five(&cx, &base, &suffix, limits),
            Err(SealedError::Limit {
                resource: "overlay property bytes",
                requested: 50,
                limit: 49,
            })
        ));
        let exact = overlay_at_five(
            &cx,
            &base,
            &suffix,
            SealedLimits {
                max_property_bytes: 50,
                ..limits
            },
        )
        .unwrap();
        assert_eq!(exact.visible_delta_count(), 1);
        let mut outgoing = exact.outgoing_cursor(&cx, VId(1)).unwrap();
        let out = outgoing.next(&cx).unwrap().unwrap();
        let mut incoming = exact.incoming_cursor(&cx, VId(2)).unwrap();
        let inbound = incoming.next(&cx).unwrap().unwrap();
        assert_eq!(out.entry.eid, EId(100));
        assert_eq!(inbound.entry, out.entry);
        assert_eq!(out.properties, property.as_slice());
        assert!(core::ptr::eq(out.properties, inbound.properties));
        assert!(incoming.next(&cx).unwrap().is_none());

        let (base, suffix) =
            overlay_fixture(&cx, vec![(overlay_entry(100, 1, 2, 5), property); 128]);
        let repeated = overlay_at_five(
            &cx,
            &base,
            &suffix,
            SealedLimits {
                max_property_bytes: 25,
                ..limits
            },
        )
        .unwrap();
        assert_eq!(repeated.visible_delta_count(), 1);
    });
}

#[test]
fn post_seal_materialization_streams_families_with_cumulative_output_admission() {
    run(0x5ea1_2033, |cx| {
        let limits = SealedLimits::default();
        let (_, _, original) = source(false);
        let property = |value| vec![(PropertyKeyId(1), CanonicalScalar::Int(value).into())];
        let mut later_retirement = original
            .iter()
            .find(|row| row.0.eid == EId(11))
            .unwrap()
            .0;
        later_retirement.retired_at = Some(CommitSeq(7));
        let mut current_retirement = original
            .iter()
            .find(|row| row.0.eid == EId(12))
            .unwrap()
            .0;
        current_retirement.retired_at = Some(CommitSeq(5));
        let mut added = overlay_entry(100, 1, 3, 5);
        added.retired_at = Some(CommitSeq(6));
        let future = overlay_entry(101, 8, 9, 7);
        let suffix_rows = vec![
            (later_retirement, property(11)),
            (current_retirement, property(12)),
            (added, property(100)),
            (future, property(101)),
        ];
        let (base, suffix) = overlay_fixture(&cx, suffix_rows);
        let overlay = overlay_at_five(&cx, &base, &suffix, limits).unwrap();

        // Independently assemble retained logical history at floor five,
        // including a base retirement after the cut and a future delta birth.
        let mut expected_rows: Rows = original
            .into_iter()
            .filter(|(entry, _)| entry.retired_at.is_none_or(|retired| retired > CommitSeq(5)))
            .filter(|(entry, _)| entry.eid != EId(12))
            .map(|(mut entry, row)| {
                if entry.eid == EId(11) {
                    entry.retired_at = Some(CommitSeq(7));
                }
                (entry, row)
            })
            .chain([(added, property(100)), (future, property(101))])
            .collect();
        expected_rows.sort_by_key(|(entry, _)| {
            (
                entry.src,
                entry.relation,
                entry.dst,
                entry.eid,
                entry.created_at,
            )
        });
        let expected_count = expected_rows.len();
        let expected_properties: usize = expected_rows
            .iter()
            .map(|(_, row)| crate::edge_props::admitted_row_bytes(row).unwrap() as usize)
            .sum();
        let image =
            super::super::image::build_retained(expected_rows.clone(), limits, &mut || Ok(()))
                .unwrap();
        let scope = SealedScope {
            source_root: suffix.source_root,
            graph: base.header().scope.graph,
            branch: base.header().scope.branch,
            partition: base.header().scope.partition,
            floor: CommitSeq(5),
            publication: suffix.publication,
        };
        let expected =
            SealedPartition::finish_scoped(scope, Some(RELATION), image, limits, &mut || Ok(()))
                .unwrap();

        for (tight, resource) in [
            (
                SealedLimits {
                    max_incidences: expected_count - 1,
                    ..limits
                },
                "incidences",
            ),
            (
                SealedLimits {
                    max_rows: 2,
                    ..limits
                },
                "descriptors",
            ),
            (
                SealedLimits {
                    max_property_bytes: expected_properties - 1,
                    ..limits
                },
                "property bytes",
            ),
        ] {
            assert!(matches!(
                overlay.materialize_partition(&cx, tight),
                Err(SealedError::Limit { resource: actual, .. }) if actual == resource
            ));
        }

        // Every failed private build leaves the same overlay reusable.
        let exact = SealedLimits {
            max_incidences: expected_count,
            max_property_bytes: expected_properties,
            max_image_bytes: expected.stats().encoded_bytes,
            ..limits
        };
        let materialized = overlay.materialize_partition(&cx, exact).unwrap();
        assert_eq!(
            materialized.encode(&cx, limits).unwrap(),
            expected.encode(&cx, limits).unwrap()
        );
        assert_eq!(materialized.anchor(), expected.anchor());
        for sequence in 5..=8 {
            for src in [VId(1), VId(8), VId((1_u128 << 127) + 19)] {
                let expected: Rows = expected_rows
                    .iter()
                    .filter(|(entry, _)| {
                        entry.src == src && entry.visible_at(CommitSeq(sequence))
                    })
                    .cloned()
                    .collect();
                assert_eq!(collect(&cx, &materialized, src, sequence), expected);
            }
        }
        let mut incoming = overlay.incoming_cursor(&cx, VId(3)).unwrap();
        let observed = incoming.next(&cx).unwrap().unwrap();
        assert_eq!(observed.entry, added);
        assert_eq!(observed.properties, property(100).as_slice());
        assert!(incoming.next(&cx).unwrap().is_none());
    });
}

#[test]
fn post_seal_overlay_cancellation_is_terminal_and_private_builds_are_reusable() {
    run(0x5ea1_2034, |cx| {
        use fgdb_types::context::SimulationCheckpointProbe;

        let limits = SealedLimits::default();
        let (_, _, original) = source(false);
        // Retire the complete first base descriptor, then materialize a
        // delta-only source. Its row copies must observe cancellation even
        // though there is no base cursor to provide their checkpoints.
        let suffix_rows: Rows = original
            .into_iter()
            .filter(|(entry, _)| entry.src == VId(1) && entry.visible_at(CommitSeq(4)))
            .map(|(mut entry, row)| {
                entry.retired_at = Some(CommitSeq(5));
                (entry, row)
            })
            .chain((0..20).map(|at| {
                (
                    overlay_entry(200 + at, 8, 20 + at, 5),
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(at as i64).into())],
                )
            }))
            .collect();
        let (base, suffix) = overlay_fixture(&cx, suffix_rows);
        let build_probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&build_probe));
        let overlay = overlay_at_five(&observed, &base, &suffix, limits).unwrap();
        assert!(build_probe.calls() > 0);
        for stop in 1..=build_probe.calls() {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let stopped = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                overlay_at_five(&stopped, &base, &suffix, limits),
                Err(SealedError::Interrupted(_))
            ));
            assert_eq!(probe.calls(), stop);
        }

        let image_probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&image_probe));
        let expected = overlay.materialize_partition(&observed, limits).unwrap();
        assert!(collect(&cx, &expected, VId(1), 5).is_empty());
        assert_eq!(collect(&cx, &expected, VId(8), 5).len(), 20);
        assert!(image_probe.calls() > 20);
        for stop in 1..=image_probe.calls() {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let stopped = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                overlay.materialize_partition(&stopped, limits),
                Err(SealedError::Interrupted(_))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let retried = overlay.materialize_partition(&cx, limits).unwrap();
        assert_eq!(
            retried.encode(&cx, limits).unwrap(),
            expected.encode(&cx, limits).unwrap()
        );
    });
}

#[derive(Default)]
struct DecodeAdmission {
    encoded: Vec<usize>,
    profiles: Vec<SealedLimits>,
    live_owners: Arc<std::sync::atomic::AtomicUsize>,
    reject_encoded: bool,
    reject_decoded: bool,
}

struct DecodeOwner(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for DecodeOwner {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl AdjRunReadAdmission for DecodeAdmission {
    type Error = &'static str;
    type Owner = DecodeOwner;

    fn encoded(&mut self, bytes: usize) -> Result<(), Self::Error> {
        self.encoded.push(bytes);
        if self.reject_encoded {
            Err("encoded admission")
        } else {
            Ok(())
        }
    }

    fn decoded(&mut self, limits: SealedLimits) -> Result<DecodeOwner, Self::Error> {
        self.profiles.push(limits);
        if self.reject_decoded {
            return Err("decoded admission");
        }
        self.live_owners
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(DecodeOwner(Arc::clone(&self.live_owners)))
    }
}

fn admitted(
    cx: &QueryCx,
    segment: &AdjRunSegment,
    bytes: &[u8],
    limits: SealedLimits,
    admission: &mut DecodeAdmission,
) -> Result<Result<(AdjRunSegment, DecodeOwner), &'static str>, AdjRunError> {
    read_adj_run_admitted(
        cx,
        adj_run_id(&KEY, NAMESPACE, bytes),
        segment.header.scope,
        RELATION,
        &KEY,
        NAMESPACE,
        bytes,
        limits,
        None,
        admission,
    )
}

#[test]
fn authenticated_run_admission_uses_actual_shape_under_generous_ceilings() {
    run(0xf6_7210, |cx| {
        let (partition, origins, rows) = source(false);
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        for dual_face in [false, true] {
            let bytes = if dual_face {
                segment.encode_dual_face(&cx, SealedLimits::default())
            } else {
                segment.encode(&cx, SealedLimits::default())
            }
            .unwrap();
            let mut previous = None;
            for limits in [
                SealedLimits::default(),
                SealedLimits {
                    max_rows: 14,
                    max_incidences: 14,
                    max_image_bytes: bytes.len(),
                    max_property_bytes: bytes.len(),
                },
            ] {
                let mut admission = DecodeAdmission::default();
                let (reopened, owner) = admitted(&cx, &segment, &bytes, limits, &mut admission)
                    .unwrap()
                    .unwrap();
                assert_eq!(admission.encoded, [bytes.len()]);
                assert_eq!(admission.profiles.len(), 1);
                let actual = admission.profiles[0];
                assert_eq!(actual.max_rows, 14);
                assert_eq!(actual.max_incidences, 14);
                assert_eq!(actual.max_image_bytes, bytes.len());
                assert!(actual.max_property_bytes < bytes.len());
                if let Some(previous) = previous {
                    assert_eq!(actual, previous, "unused ceilings must not alter demand");
                }
                previous = Some(actual);
                assert_eq!(reopened.header(), segment.header());
                for seq in 0..=4 {
                    let expected: Rows = rows
                        .iter()
                        .filter(|(entry, _)| {
                            entry.src == VId(1)
                                && entry.created_at.0 <= seq
                                && entry.retired_at.is_none_or(|end| seq < end.0)
                        })
                        .cloned()
                        .collect();
                    assert_eq!(collect(&cx, reopened.partition(), VId(1), seq), expected);
                }
                assert_eq!(
                    admission
                        .live_owners
                        .load(std::sync::atomic::Ordering::SeqCst),
                    1
                );
                drop(reopened);
                drop(owner);
                assert_eq!(
                    admission
                        .live_owners
                        .load(std::sync::atomic::Ordering::SeqCst),
                    0
                );
            }
        }
    });
}

#[test]
fn forged_run_framing_refuses_before_decoder_admission_and_failed_images_release_the_owner() {
    run(0xf6_7211, |cx| {
        let (partition, origins, _) = source(false);
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        let bytes = segment.encode(&cx, SealedLimits::default()).unwrap();
        // Literal offsets in the existing FGRN-v1 scalar frame: incidence
        // count, origin count, and embedded image length. Each candidate has
        // a valid checksum and object ID, so these exercise shape admission.
        for (offset, value) in [(109, u32::MAX), (113, 15), (245, u32::MAX)] {
            let mut forged = bytes.clone();
            forged[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            resign(&cx, &mut forged);
            let mut admission = DecodeAdmission::default();
            assert!(
                admitted(
                    &cx,
                    &segment,
                    &forged,
                    SealedLimits::default(),
                    &mut admission
                )
                .is_err()
            );
            assert_eq!(admission.encoded, [forged.len()]);
            assert!(admission.profiles.is_empty());
            assert_eq!(
                admission
                    .live_owners
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
        }

        // A valid outer frame cannot use a forged inner count to exceed the
        // authenticated outer population. Re-sign both integrity layers so
        // the independent inner decoder must enforce the narrowed ceiling.
        let mut forged = bytes.clone();
        let image_len = u32::from_le_bytes(forged[245..249].try_into().unwrap()) as usize;
        forged[272..276].copy_from_slice(&15_u32.to_le_bytes());
        let mut fingerprint = super::super::fingerprint_hasher();
        fingerprint.update(&forged[262..262 + image_len]);
        forged[213..245].copy_from_slice(&fingerprint.finalize().0);
        resign(&cx, &mut forged);
        let mut admission = DecodeAdmission::default();
        assert!(matches!(
            admitted(
                &cx,
                &segment,
                &forged,
                SealedLimits::default(),
                &mut admission
            ),
            Err(AdjRunError::Sealed(SealedError::Limit {
                resource: "incidences",
                requested: 15,
                limit: 14,
            }))
        ));
        assert_eq!(admission.profiles.len(), 1);
        assert_eq!(
            admission
                .live_owners
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );

        let mut admission = DecodeAdmission::default();
        let limits = SealedLimits {
            max_property_bytes: 0,
            ..SealedLimits::default()
        };
        assert!(matches!(
            admitted(&cx, &segment, &bytes, limits, &mut admission),
            Err(AdjRunError::Sealed(SealedError::Limit {
                resource: "property bytes",
                ..
            }))
        ));
        assert_eq!(
            admission
                .live_owners
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    });
}

#[test]
fn run_owner_refusals_precede_decode_and_keep_their_original_error() {
    run(0xf6_7212, |cx| {
        let (partition, origins, _) = source(false);
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        let mut bytes = segment.encode(&cx, SealedLimits::default()).unwrap();
        bytes[262] ^= 1; // Invalid embedded image, still a well-framed run.
        resign(&cx, &mut bytes);
        for stage in [0, 1] {
            let mut admission = DecodeAdmission {
                reject_encoded: stage == 0,
                reject_decoded: stage == 1,
                ..DecodeAdmission::default()
            };
            assert!(matches!(
                admitted(&cx, &segment, &bytes, SealedLimits::default(), &mut admission),
                Ok(Err(error)) if error == if stage == 0 { "encoded admission" } else { "decoded admission" }
            ));
            assert_eq!(admission.profiles.len(), stage);
            assert_eq!(
                admission
                    .live_owners
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
        }
    });
}

#[test]
fn actual_run_admission_keeps_many_incoming_descriptors_for_one_forward_row() {
    run(0xf6_7213, |cx| {
        let (template, _, _) = source(false);
        let mut rows = Vec::new();
        let mut origins = BTreeMap::new();
        for id in 10..22 {
            let eid = EId(id);
            rows.push((
                AdjacencyEntry {
                    src: VId(1),
                    relation: RELATION,
                    dst: VId(id),
                    eid,
                    created_at: CommitSeq(1),
                    retired_at: None,
                },
                Vec::new(),
            ));
            origins.insert(eid, OriginBirthOrder::new(CommitSeq(1), id as u64, 0, eid));
        }
        let limits = SealedLimits::default();
        let image = super::super::image::build_retained(rows, limits, &mut || Ok(())).unwrap();
        let partition = SealedPartition::finish_scoped(
            template.anchor.scope,
            Some(RELATION),
            image,
            limits,
            &mut || Ok(()),
        )
        .unwrap();
        let segment = AdjRunSegment::from_sealed(&cx, &partition, &origins, limits).unwrap();
        let bytes = segment.encode_dual_face(&cx, limits).unwrap();
        let mut admission = DecodeAdmission::default();
        let (reopened, owner) = admitted(&cx, &segment, &bytes, limits, &mut admission)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.header().row_count, 1);
        assert_eq!(admission.profiles[0].max_rows, 12);
        let incoming = reopened
            .partition()
            .incoming_index(&cx, super::super::IncomingIndexLimits::default())
            .unwrap();
        for id in 10..22 {
            let mut cursor = incoming.row(&cx, VId(id), RELATION, CommitSeq(1)).unwrap();
            let edge = cursor.next(&cx).unwrap().unwrap();
            assert_eq!(
                (edge.entry.eid, edge.entry.src, edge.entry.dst),
                (EId(id), VId(1), VId(id))
            );
            assert!(cursor.next(&cx).unwrap().is_none());
        }
        drop(incoming);
        drop(reopened);
        drop(owner);
        assert_eq!(
            admission
                .live_owners
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    });
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

#[test]
fn fgrn_v2_dual_face_persists_incoming_ef_csr_with_zero_transpose_scratch_and_tamper_refusal() {
    run(0xf6_7219, |cx| {
        let (partition, origins, _) = source(false);
        let segment =
            AdjRunSegment::from_sealed(&cx, &partition, &origins, SealedLimits::default()).unwrap();
        let v1_bytes = segment.encode(&cx, SealedLimits::default()).unwrap();
        let v2_bytes = segment
            .encode_dual_face(&cx, SealedLimits::default())
            .unwrap();
        assert_eq!(&v1_bytes[4..6], &FORMAT.to_le_bytes());
        assert_eq!(&v2_bytes[4..6], &FORMAT_V2.to_le_bytes());
        assert!(v2_bytes.len() > v1_bytes.len());

        // V1 backwards compatibility remains intact.
        let reloaded_v1 = read(&cx, &segment, &v1_bytes).unwrap();
        assert!(!reloaded_v1.partition().has_durable_incoming());

        // V2 carries durable incoming Elias-Fano CSR index with zero transpose scratch.
        let reloaded_v2 = read(&cx, &segment, &v2_bytes).unwrap();
        assert!(reloaded_v2.partition().has_durable_incoming());
        let inc_a = reloaded_v2
            .partition()
            .incoming_index(&cx, super::super::IncomingIndexLimits::default())
            .unwrap();
        let inc_b = reloaded_v2
            .partition()
            .incoming_index(&cx, super::super::IncomingIndexLimits::default())
            .unwrap();
        assert!(inc_a.shares_index_with(&inc_b));
        assert_eq!(
            inc_a.stats().charged_workspace_bytes,
            inc_a.stats().charged_resident_bytes,
            "durable FGRN-v2 incoming reuse must allocate zero IncidenceRef transpose scratch",
        );

        // Incoming cursor on V2 matches freshly transposed V1 incoming cursor bit-for-bit.
        let fresh_inc = reloaded_v1
            .partition()
            .incoming_index(&cx, super::super::IncomingIndexLimits::default())
            .unwrap();
        let high = VId((1_u128 << 127) + 19);
        for cut in [1, 2, 3, 4] {
            for dst in [VId(1), VId(u128::MAX), high] {
                let mut cur_v2 = inc_a.row(&cx, dst, RELATION, CommitSeq(cut)).unwrap();
                let mut cur_v1 = fresh_inc.row(&cx, dst, RELATION, CommitSeq(cut)).unwrap();
                loop {
                    let a = cur_v2.next(&cx).unwrap();
                    let b = cur_v1.next(&cx).unwrap();
                    match (a, b) {
                        (Some(ea), Some(eb)) => {
                            assert_eq!(ea.entry, eb.entry);
                            assert_eq!(ea.properties, eb.properties);
                        }
                        (None, None) => break,
                        other => panic!("mismatch at cut={cut} dst={dst:?}: {other:?}"),
                    }
                }
            }
        }

        // Corrupting the trailing E_in section (even after resigning frame checksum)
        // must fail closed during durable incoming index admission.
        let mut tampered = v2_bytes.clone();
        let ein_tail = tampered.len() - CHECKSUM_BYTES - 1;
        tampered[ein_tail] ^= 0xff;
        resign(&cx, &mut tampered);
        assert!(
            read(&cx, &segment, &tampered).is_err(),
            "tampered FGRN-v2 E_in payload must be refused",
        );
    });
}
