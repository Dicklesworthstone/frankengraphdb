use super::*;
use crate::edge_props::{BlockProps, EdgePropertyRow};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, ObjectId};

fn scope(floor: u64) -> SealedScope {
    SealedScope {
        source_root: PartitionRootVersion(ObjectId([7; 32])),
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 3,
        floor: CommitSeq(floor),
        publication: CommitSeq(10),
    }
}

fn entry(src: u128, dst: u128, eid: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: RelationId(1),
        dst: VId(dst),
        eid: EId(eid),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn seal(
    blocks: &[Vec<AdjacencyEntry>],
    properties: &[Option<BlockProps>],
    floor: u64,
) -> SealedPartition {
    let compacted =
        crate::compact::compact_with_props(blocks, properties, CommitSeq(floor)).unwrap();
    let image = image::build(compacted, SealedLimits::default(), &mut || Ok(())).unwrap();
    SealedPartition::finish(scope(floor), image, SealedLimits::default(), &mut || Ok(())).unwrap()
}

fn collect(
    partition: &SealedPartition,
    src: u128,
    as_of: u64,
    lower: Option<u128>,
) -> Vec<(AdjacencyEntry, EdgePropertyRow)> {
    partition.anchor.authorize(CommitSeq(as_of)).unwrap();
    let row = partition.image.find_row(VId(src), RelationId(1));
    let position = match (row, lower) {
        (Some(row), Some(dst)) => row.lower_bound(&partition.image, VId(dst)),
        _ => 0,
    };
    let mut cursor = SealedCursor::new(&partition.image, row, position, CommitSeq(as_of));
    let mut entries = Vec::new();
    while let Some(edge) = cursor.next_inner(&mut || Ok(())).unwrap() {
        entries.push((edge.entry, edge.properties.to_vec()));
    }
    entries
}

fn bytes(partition: &SealedPartition) -> Vec<u8> {
    wire::encode(&partition.image, SealedLimits::default(), &mut || Ok(())).unwrap()
}

#[test]
fn real_compaction_to_mixed_tiers_preserves_every_retained_snapshot() {
    let large: Vec<_> = (0..40)
        .map(|i| {
            entry(
                1,
                i / 3,
                i + 1,
                1 + (i % 3) as u64,
                if i % 2 == 0 { Some(8) } else { None },
            )
        })
        .collect();
    let small: Vec<_> = (0..4).map(|i| entry(2, i, 100 + i, 1, None)).collect();
    let source = vec![large.clone(), small.clone()];
    let partition = seal(&source, &[None, None], 3);
    assert_eq!(
        partition.storage_kind(VId(1), RelationId(1)),
        Some(RowStorageKind::SealedCsr)
    );
    assert_eq!(
        partition.storage_kind(VId(2), RelationId(1)),
        Some(RowStorageKind::Inline)
    );
    assert_eq!(partition.stats().incidences, 44);
    for as_of in 3..=10 {
        for (src, entries) in [(1, &large), (2, &small)] {
            let expected: Vec<_> = entries
                .iter()
                .copied()
                .filter(|entry| entry.visible_at(CommitSeq(as_of)))
                .collect();
            let actual: Vec<_> = collect(&partition, src, as_of, None)
                .into_iter()
                .map(|(entry, _)| entry)
                .collect();
            assert_eq!(actual, expected);
        }
    }
    assert!(partition.anchor.authorize(CommitSeq(2)).is_err());
    assert!(partition.anchor.authorize(CommitSeq(11)).is_err());
}

#[test]
fn sequential_csr_scans_preserve_seek_history_properties_and_unit_fuel() {
    let source = (1_u128 << 80) + 1;
    let target_base = 1_u128 << 100;
    let entries: Vec<_> = (0..257_u128)
        .map(|i| {
            entry(
                source,
                target_base + i / 5,
                (1_u128 << 96) + i,
                1 + (i % 3) as u64,
                (i % 2 == 0).then_some(8),
            )
        })
        .collect();
    let blocks: Vec<_> = entries.chunks(120).map(<[_]>::to_vec).collect();
    let properties: Vec<_> = blocks
        .iter()
        .map(|block| {
            Some(BlockProps {
                locators: (1..=u8::try_from(block.len()).unwrap()).collect(),
                rows: block
                    .iter()
                    .map(|edge| {
                        vec![(
                            PropertyKeyId(1),
                            CanonicalScalar::Int((edge.eid.0 - (1_u128 << 96)) as i64).into(),
                        )]
                    })
                    .collect(),
            })
        })
        .collect();
    let partition = seal(&blocks, &properties, 1);
    let encoded = bytes(&partition);
    let reloaded = SealedPartition::reload_inner(
        partition.anchor(),
        &encoded,
        SealedLimits::default(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(bytes(&reloaded), encoded);
    for partition in [&partition, &reloaded] {
        let row = partition
            .image
            .find_row(VId(source), RelationId(1))
            .unwrap();
        assert_eq!(row.kind(), RowStorageKind::SealedCsr);
        // Each independent random-access decode is the incumbent path. Seek
        // inside duplicate destinations, EF words, and visibility-span runs.
        for start in 0..=row.len() {
            let mut scan = RowScan::new(&partition.image, row, start);
            for at in start..row.len() {
                assert_eq!(scan.next(), row.incidence(&partition.image, at));
            }
            assert_eq!(scan.next(), None);
            assert_eq!(scan.next(), None);
        }
        for cut in 1..=10 {
            for lower in [target_base, target_base + 12, target_base + 51, u128::MAX] {
                let expected: Vec<_> = entries
                    .iter()
                    .copied()
                    .filter(|edge| edge.dst.0 >= lower && edge.visible_at(CommitSeq(cut)))
                    .map(|edge| {
                        let value = (edge.eid.0 - (1_u128 << 96)) as i64;
                        (
                            edge,
                            vec![(PropertyKeyId(1), CanonicalScalar::Int(value).into())],
                        )
                    })
                    .collect();
                let start = row.lower_bound(&partition.image, VId(lower));
                let mut cursor =
                    SealedCursor::new(&partition.image, Some(row), start, CommitSeq(cut));
                let mut actual = Vec::new();
                loop {
                    let position = cursor.scan.as_ref().unwrap().position;
                    let paused = cursor
                        .next_budgeted_inner(&mut SealedScanBudget::new(0), &mut || {
                            Ok::<_, SealedError>(())
                        })
                        .unwrap();
                    assert_eq!(cursor.scan.as_ref().unwrap().position, position);
                    if matches!(paused, SealedScanStep::End) {
                        break;
                    }
                    assert!(matches!(paused, SealedScanStep::Yield));
                    match cursor
                        .next_budgeted_inner(&mut SealedScanBudget::new(1), &mut || {
                            Ok::<_, SealedError>(())
                        })
                        .unwrap()
                    {
                        SealedScanStep::Item(edge) => {
                            actual.push((edge.entry, edge.properties.to_vec()));
                        }
                        SealedScanStep::Yield => {}
                        SealedScanStep::End => break,
                    }
                    assert!(cursor.scan.as_ref().unwrap().position <= position + 1);
                }
                assert_eq!(actual, expected, "cut={cut}, lower={lower}");
            }
        }
    }
}

#[test]
fn tombstone_precedence_and_content_version_properties_use_the_existing_compactor() {
    let initial: Vec<_> = (1..=9).map(|eid| entry(1, 7, eid, 1, None)).collect();
    let initial_props = BlockProps {
        locators: (1..=9).collect(),
        rows: (1..=9)
            .map(|eid| {
                vec![(
                    PropertyKeyId(1),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(eid)),
                )]
            })
            .collect(),
    };
    let change = vec![entry(1, 7, 1, 1, Some(5)), entry(1, 7, 1, 5, None)];
    let changed_props = BlockProps {
        locators: vec![1, 2],
        rows: vec![
            vec![(
                PropertyKeyId(1),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(1)),
            )],
            vec![(
                PropertyKeyId(1),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(99)),
            )],
        ],
    };
    let partition = seal(
        &[initial, change],
        &[Some(initial_props), Some(changed_props)],
        1,
    );
    assert_eq!(partition.stats().incidences, 10);
    assert_eq!(
        collect(&partition, 1, 4, None)[0].1,
        vec![(
            PropertyKeyId(1),
            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(1))
        )]
    );
    assert_eq!(
        collect(&partition, 1, 5, None)[0].1,
        vec![(
            PropertyKeyId(1),
            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(99))
        )]
    );
    for cut in [4, 5, 10] {
        assert_eq!(collect(&partition, 1, cut, None).len(), 9);
    }
    let serialized = bytes(&partition);
    let reloaded = SealedPartition::reload_inner(
        partition.anchor,
        &serialized,
        SealedLimits::default(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    for cut in 1..=10 {
        assert_eq!(
            collect(&partition, 1, cut, None),
            collect(&reloaded, 1, cut, None)
        );
    }
}

#[test]
fn floor_drops_can_demote_a_row_without_erasing_observable_history() {
    let entries: Vec<_> = (1..=9)
        .map(|eid| entry(1, eid, eid, 1, (eid == 1).then_some(5)))
        .collect();
    let before = seal(core::slice::from_ref(&entries), &[None], 4);
    let after = seal(&[entries], &[None], 5);
    assert_eq!(
        before.storage_kind(VId(1), RelationId(1)),
        Some(RowStorageKind::SealedCsr)
    );
    assert_eq!(
        after.storage_kind(VId(1), RelationId(1)),
        Some(RowStorageKind::Inline)
    );
    for cut in 5..=10 {
        assert_eq!(
            collect(&before, 1, cut, None),
            collect(&after, 1, cut, None)
        );
    }
    assert_eq!(collect(&before, 1, 4, None).len(), 9);
    assert!(after.anchor.authorize(CommitSeq(4)).is_err());
}

#[test]
fn lower_bound_keeps_all_parallel_incidence_ranges_on_both_tiers() {
    let large: Vec<_> = (0..30).map(|i| entry(1, i / 3, i + 1, 1, None)).collect();
    let small: Vec<_> = (0..6).map(|i| entry(2, i / 3, i + 100, 1, None)).collect();
    let partition = seal(&[large, small], &[None, None], 1);
    for src in [1, 2] {
        let all = collect(&partition, src, 5, None);
        for lower in 0..=11 {
            let expected: Vec<_> = all
                .iter()
                .filter(|(entry, _)| entry.dst >= VId(lower))
                .cloned()
                .collect();
            assert_eq!(collect(&partition, src, 5, Some(lower)), expected);
        }
    }
}

#[test]
fn canonical_bytes_round_trip_and_corruption_never_mints_source_authority() {
    let partition = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    let encoded = bytes(&partition);
    let reloaded = SealedPartition::reload_inner(
        partition.anchor,
        &encoded,
        SealedLimits::default(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(bytes(&reloaded), encoded);
    assert_eq!(reloaded.anchor, partition.anchor);
    for at in 0..encoded.len() {
        let mut damaged = encoded.clone();
        damaged[at] ^= 1;
        assert!(matches!(
            SealedPartition::reload_inner(
                partition.anchor,
                &damaged,
                SealedLimits::default(),
                None,
                &mut || Ok(())
            ),
            Err(SealedError::ImageMismatch)
        ));
        assert!(
            wire::decode(
                &encoded[..at],
                SealedLimits::default(),
                None,
                &mut || Ok(())
            )
            .is_err()
        );
    }
    let mut extra = encoded.clone();
    extra.push(0);
    assert!(matches!(
        wire::decode(&extra, SealedLimits::default(), None, &mut || Ok(())),
        Err(SealedError::TrailingBytes)
    ));
    let mut forged = encoded;
    forged[6..10].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        wire::decode(&forged, SealedLimits::default(), None, &mut || Ok(())),
        Err(SealedError::Limit { .. })
    ));
}

#[test]
fn cloning_shares_payloads_but_retaining_an_anchor_does_not_pin_them() {
    let partition = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    let cloned = partition.clone();
    assert!(partition.shares_image_with(&cloned));
    let weak = Arc::downgrade(&partition.image);
    let anchor = partition.anchor();
    let encoded = bytes(&partition);
    drop(partition);
    assert!(weak.upgrade().is_some());
    drop(cloned);
    assert!(weak.upgrade().is_none());
    let reloaded =
        SealedPartition::reload_inner(anchor, &encoded, SealedLimits::default(), None, &mut || {
            Ok(())
        })
        .unwrap();
    assert_eq!(collect(&reloaded, 1, 5, None).len(), 1);
}

#[test]
fn property_chunks_cross_the_existing_255_row_boundary_without_locator_loss() {
    let mut blocks = Vec::new();
    let mut props = Vec::new();
    for chunk in 0..5 {
        blocks.push(
            (0..120)
                .map(|i| {
                    entry(
                        1,
                        (chunk * 120 + i) as u128,
                        (chunk * 120 + i + 1) as u128,
                        1,
                        None,
                    )
                })
                .collect(),
        );
        props.push(Some(BlockProps {
            locators: (1..=120).collect(),
            rows: (0..120)
                .map(|i| {
                    vec![(
                        PropertyKeyId(1),
                        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(chunk * 120 + i)),
                    )]
                })
                .collect(),
        }));
    }
    let partition = seal(&blocks, &props, 1);
    assert_eq!(partition.stats().property_rows, 600);
    let encoded = bytes(&partition);
    let reloaded = SealedPartition::reload_inner(
        partition.anchor,
        &encoded,
        SealedLimits::default(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    let rows = collect(&reloaded, 1, 10, None);
    assert_eq!(rows.len(), 600);
    for (index, (_, properties)) in rows.iter().enumerate() {
        assert_eq!(
            properties,
            &vec![(
                PropertyKeyId(1),
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(index as i64))
            )]
        );
    }
}

#[test]
fn empty_image_is_canonical_and_resource_limits_remain_explicit() {
    let empty = seal(&[], &[], 0);
    let encoded = bytes(&empty);
    // FGSI v1 empty frame: four zero counts, raw empty dictionary, and the
    // single CSR offset 0 encoded as one unary-high word. Streaming changes
    // neither this existing framing nor the fingerprint domain.
    let golden = [
        b'F', b'G', b'S', b'I', 1, 0, // magic/version
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // counts
        0, 0, 0, 0, 0, 0, 0, 0, 0, // raw dictionary tag/count/length
        0, 0, 0, 0, 0, 0, 0, 0, // offset maximum
        8, 0, 0, 0, // offset payload length
        1, 0, 0, 0, 0, 0, 0, 0, // unary-high payload
    ];
    assert_eq!(encoded, golden);
    let reloaded = SealedPartition::reload_inner(
        empty.anchor,
        &encoded,
        SealedLimits::default(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(reloaded.stats().incidences, 0);
    assert!(collect(&reloaded, 9, 0, None).is_empty());
    assert!(matches!(
        wire::encode(
            &empty.image,
            SealedLimits {
                max_image_bytes: encoded.len() - 1,
                ..SealedLimits::default()
            },
            &mut || Ok(())
        ),
        Err(SealedError::Limit { .. })
    ));
    let one = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    assert!(matches!(
        wire::decode(
            &bytes(&one),
            SealedLimits {
                max_incidences: 0,
                ..SealedLimits::default()
            },
            None,
            &mut || Ok(())
        ),
        Err(SealedError::Limit { .. })
    ));
}

#[test]
fn interrupted_decode_and_scan_do_not_report_a_successful_prefix() {
    let partition = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    assert!(
        wire::decode(
            &bytes(&partition),
            SealedLimits::default(),
            None,
            &mut || Err(SealedError::InvalidFloor)
        )
        .is_err()
    );
    let mut cursor = SealedCursor::new(
        &partition.image,
        partition.image.find_row(VId(1), RelationId(1)),
        0,
        CommitSeq(5),
    );
    assert!(
        cursor
            .next_inner(&mut || Err(SealedError::InvalidFloor))
            .is_err()
    );
    assert!(cursor.finished);
    assert_eq!(cursor.scan.as_ref().unwrap().position, 0);
}

#[test]
fn streamed_image_fingerprint_is_exact_bounded_and_interruptible() {
    let entries: Vec<_> = (0..4097)
        .map(|index| entry(1, index / 3, index + 1, 1, (index % 2 == 0).then_some(8)))
        .collect();
    let mut blocks: Vec<_> = entries.chunks(120).map(<[_]>::to_vec).collect();
    blocks.push(vec![entry(2, 3, 5000, 1, None)]);
    let properties = vec![None; blocks.len()];
    let partition = seal(&blocks, &properties, 1);
    let encoded = bytes(&partition);
    assert!(encoded.len() > 4096 * 3);
    assert_eq!(partition.incidence_count(), entries.len() + 1);

    // Compare streaming construction with a direct, single-update BLAKE3
    // over the complete canonical frame and the unchanged domain separator.
    let mut expected = fgdb_crypto::Hasher::new();
    expected.update(b"fgdb.strata.sealed-adjacency-image.v1");
    expected.update(&encoded);
    let expected = expected.finalize().0;
    assert_eq!(partition.anchor.fingerprint, expected);
    assert_eq!(partition.anchor.encoded_bytes, encoded.len());
    let mut polls = 0;
    let mut emitted = 0;
    let mut streaming = fingerprint_hasher();
    let length = wire::encode_to(
        &partition.image,
        SealedLimits::default(),
        &mut || {
            polls += 1;
            Ok(())
        },
        |part| {
            assert!(!part.is_empty() && part.len() <= 4096);
            streaming.update(part);
            emitted += part.len();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(length, encoded.len());
    assert_eq!(emitted, encoded.len());
    assert_eq!(streaming.finalize().0, expected);
    assert!(polls >= encoded.len().div_ceil(4096));

    for stop in 1..=polls {
        let mut reached = 0;
        assert!(matches!(
            wire::fingerprint(&partition.image, SealedLimits::default(), &mut || {
                reached += 1;
                if reached == stop {
                    Err(SealedError::InvalidFloor)
                } else {
                    Ok(())
                }
            },),
            Err(SealedError::InvalidFloor)
        ));
        assert_eq!(reached, stop);
    }
    assert_eq!(
        wire::fingerprint(&partition.image, SealedLimits::default(), &mut || Ok(())).unwrap(),
        (expected, encoded.len())
    );

    let exact = SealedLimits {
        max_image_bytes: encoded.len(),
        ..SealedLimits::default()
    };
    assert_eq!(
        wire::fingerprint(&partition.image, exact, &mut || Ok(())).unwrap(),
        (expected, encoded.len())
    );
    for (limits, resource) in [
        (
            SealedLimits {
                max_image_bytes: encoded.len() - 1,
                ..exact
            },
            "image bytes",
        ),
        (
            SealedLimits {
                max_rows: 1,
                ..exact
            },
            "descriptors",
        ),
        (
            SealedLimits {
                max_incidences: entries.len(),
                ..exact
            },
            "incidences",
        ),
    ] {
        assert!(
            matches!(wire::fingerprint(&partition.image, limits, &mut || Ok(())),
            Err(SealedError::Limit { resource: actual, .. }) if actual == resource)
        );
    }
}

#[test]
fn reload_hash_checks_live_control_before_decoding_any_untrusted_frame() {
    let entries: Vec<_> = (0..4097).map(|i| entry(1, i, i + 1, 1, None)).collect();
    let blocks: Vec<_> = entries.chunks(120).map(<[_]>::to_vec).collect();
    let partition = seal(&blocks, &vec![None; blocks.len()], 1);
    let mut damaged = bytes(&partition);
    assert!(damaged.len() > 4096 * 2);
    damaged[..4].copy_from_slice(b"BAD!");
    let hash_polls = damaged.len().div_ceil(4096);
    for stop in 1..=hash_polls + 1 {
        let mut reached = 0;
        assert!(matches!(
            SealedPartition::reload_inner(
                partition.anchor(),
                &damaged,
                SealedLimits::default(),
                None,
                &mut || {
                    reached += 1;
                    if reached == stop {
                        Err(SealedError::InvalidFloor)
                    } else {
                        Ok(())
                    }
                },
            ),
            Err(SealedError::InvalidFloor)
        ));
        assert_eq!(reached, stop);
    }
    assert!(matches!(
        SealedPartition::reload_inner(
            partition.anchor(),
            &damaged,
            SealedLimits::default(),
            None,
            &mut || Ok(()),
        ),
        Err(SealedError::ImageMismatch)
    ));
}

/// Manual in-memory physical-access comparison. Both paths run against the
/// same admitted image in one process; construction, validation, QueryCx,
/// property copying and durable I/O are outside the timed region. These local
/// timings are not a Section 17 performance gate or a durable database rate.
#[test]
#[ignore = "manual release-mode positional-versus-sequential CSR microbenchmark"]
fn benchmark_sealed_csr_sequential_scan_against_positional() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    const ROUNDS: usize = 9;
    const INCIDENCES_PER_ROUND: usize = 1 << 18;

    fn fingerprint(value: (AdjacencyEntry, u32)) -> u128 {
        // Force the complete incidence and locator to be decoded on both
        // paths, even though the timing checksum needs only these fields.
        let (edge, locator) = black_box(value);
        edge.eid
            .0
            .wrapping_add(edge.dst.0)
            .wrapping_add(u128::from(locator))
    }

    fn measure(scans: usize, expected: u128, mut scan: impl FnMut() -> u128) -> Duration {
        let start = Instant::now();
        let mut observed = 0_u128;
        for _ in 0..scans {
            observed = observed.wrapping_add(black_box(scan()));
        }
        let elapsed = start.elapsed();
        assert_eq!(observed, expected.wrapping_mul(scans as u128));
        elapsed
    }

    println!(
        "sealed_csr_scan_microbenchmark: same-process incumbent; \
         construction, QueryCx, property copies and durable I/O excluded; \
         empirical_gate_activated=false"
    );
    for count in [16_usize, 256, 4096, 65536] {
        for varied_spans in [false, true] {
            let entries: Vec<_> = (0..count)
                .map(|at| {
                    let (created, retired) = if varied_spans {
                        (
                            1 + ((at / 7) % 3) as u64,
                            (at / 11).is_multiple_of(2).then_some(8),
                        )
                    } else {
                        (1, None)
                    };
                    entry(
                        (1_u128 << 80) + 1,
                        (1_u128 << 100) + (at / 3) as u128,
                        (1_u128 << 96) + at as u128,
                        created,
                        retired,
                    )
                })
                .collect();
            let blocks: Vec<_> = entries.chunks(120).map(<[_]>::to_vec).collect();
            let properties = vec![None; blocks.len()];
            let partition = seal(&blocks, &properties, 1);
            let row = partition
                .image
                .find_row(VId((1_u128 << 80) + 1), RelationId(1))
                .unwrap();
            assert_eq!(row.kind(), RowStorageKind::SealedCsr);
            assert_eq!(row.len(), count);

            // Exact source-model checks precede timing; the timed paths still
            // verify a checksum after every round. Neither copies properties.
            let expected: Vec<_> = entries.iter().copied().map(|edge| (edge, 0)).collect();
            let incumbent: Vec<_> = (0..count)
                .map(|at| row.incidence(&partition.image, at).unwrap())
                .collect();
            let mut cursor = RowScan::new(&partition.image, row, 0);
            let sequential: Vec<_> = std::iter::from_fn(|| cursor.next()).collect();
            assert_eq!(incumbent, expected);
            assert_eq!(sequential, expected);
            let expected_sum = expected
                .iter()
                .copied()
                .fold(0_u128, |sum, value| sum.wrapping_add(fingerprint(value)));

            let positional_scan = || {
                let image = black_box(&partition.image);
                let row = black_box(row);
                let mut sum = 0_u128;
                for at in 0..row.len() {
                    sum = sum.wrapping_add(fingerprint(row.incidence(image, at).unwrap()));
                }
                sum
            };
            let sequential_scan = || {
                let image = black_box(&partition.image);
                let row = black_box(row);
                let mut cursor = RowScan::new(image, row, 0);
                let mut sum = 0_u128;
                while let Some(value) = cursor.next() {
                    sum = sum.wrapping_add(fingerprint(value));
                }
                sum
            };
            assert_eq!(positional_scan(), expected_sum);
            assert_eq!(sequential_scan(), expected_sum);

            let scans = INCIDENCES_PER_ROUND.div_ceil(count);
            let mut positional_times = [Duration::ZERO; ROUNDS];
            let mut sequential_times = [Duration::ZERO; ROUNDS];
            for round in 0..ROUNDS {
                // Reverse order on alternating rounds so either path can be
                // first under the same cache and process conditions.
                if round.is_multiple_of(2) {
                    positional_times[round] = measure(scans, expected_sum, positional_scan);
                    sequential_times[round] = measure(scans, expected_sum, sequential_scan);
                } else {
                    sequential_times[round] = measure(scans, expected_sum, sequential_scan);
                    positional_times[round] = measure(scans, expected_sum, positional_scan);
                }
            }
            positional_times.sort_unstable();
            sequential_times.sort_unstable();
            let incidences = (scans * count) as f64;
            let ns_per_incidence = |time: Duration| time.as_nanos() as f64 / incidences;
            let positional_median = ns_per_incidence(positional_times[ROUNDS / 2]);
            let sequential_median = ns_per_incidence(sequential_times[ROUNDS / 2]);
            println!(
                "degree={count} visibility_spans={} rounds={ROUNDS} scans_per_round={scans} \
                 positional_ns_per_incidence={positional_median:.3} \
                 positional_min={:.3} positional_max={:.3} \
                 sequential_ns_per_incidence={sequential_median:.3} \
                 sequential_min={:.3} sequential_max={:.3} speedup={:.3}",
                if varied_spans { "varied" } else { "repeating" },
                ns_per_incidence(positional_times[0]),
                ns_per_incidence(positional_times[ROUNDS - 1]),
                ns_per_incidence(sequential_times[0]),
                ns_per_incidence(sequential_times[ROUNDS - 1]),
                positional_median / sequential_median,
            );
        }
    }
}

/// The outgoing pull from f7ea1cfb, before visibility-span decode elision.
/// Keep its raw sequential decode and checkpoint/fuel order as an incumbent.
impl<'a> SealedCursor<'a> {
    fn next_decoding_history<E>(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, E> {
        if self.finished {
            return Ok(SealedScanStep::End);
        }
        if let Err(error) = checkpoint() {
            self.finished = true;
            return Err(error);
        }
        let Some(scan) = &mut self.scan else {
            self.finished = true;
            return Ok(SealedScanStep::End);
        };
        while scan.position < scan.len() {
            if !budget.spend() {
                return Ok(SealedScanStep::Yield);
            }
            if scan.position.is_multiple_of(64)
                && let Err(error) = checkpoint()
            {
                self.finished = true;
                return Err(error);
            }
            let (entry, locator) = scan
                .next()
                .expect("sealed admission validated every incidence");
            if entry.visible_at(self.as_of) {
                let properties = if locator == 0 {
                    &[][..]
                } else {
                    self.image.properties[locator as usize - 1].as_slice()
                };
                return Ok(SealedScanStep::Item(SealedEdge { entry, properties }));
            }
        }
        self.finished = true;
        Ok(SealedScanStep::End)
    }
}

#[derive(Clone, Copy, Debug)]
enum HistoryShape {
    AllVisible,
    AllHidden,
    Alternating,
    LongGaps,
}

fn history_fixture(count: usize, shape: HistoryShape) -> (SealedPartition, Vec<AdjacencyEntry>) {
    let entries: Vec<_> = (0..count)
        .map(|at| {
            let live = match shape {
                HistoryShape::AllVisible => true,
                HistoryShape::AllHidden => false,
                HistoryShape::Alternating => !at.is_multiple_of(2),
                HistoryShape::LongGaps => {
                    at + 1 == count || [63, 64, 65, 4095, 4096, 4097].contains(&at)
                }
            };
            entry(
                (1_u128 << 80) + 7,
                (1_u128 << 100) + (at / 3) as u128,
                (1_u128 << 96) + at as u128,
                if live { 3 } else { 1 },
                if live { None } else { Some(3) },
            )
        })
        .collect();
    let blocks: Vec<_> = entries.chunks(120).map(<[_]>::to_vec).collect();
    let properties: Vec<_> = blocks
        .iter()
        .map(|block| {
            Some(BlockProps {
                locators: (1..=u8::try_from(block.len()).unwrap()).collect(),
                rows: block
                    .iter()
                    .map(|edge| {
                        vec![(
                            PropertyKeyId(1),
                            CanonicalScalar::Int((edge.eid.0 - (1_u128 << 96)) as i64).into(),
                        )]
                    })
                    .collect(),
            })
        })
        .collect();
    (seal(&blocks, &properties, 1), entries)
}

/// Compare every externally observable pull, not just the final visible rows.
/// The independent entry vector also checks the incumbent's answers.
fn compare_history_pulls(
    partition: &SealedPartition,
    entries: &[AdjacencyEntry],
    start: usize,
    cut: u64,
    allowances: &[usize],
    stop: Option<usize>,
) -> usize {
    partition.anchor.authorize(CommitSeq(cut)).unwrap();
    let row = partition
        .image
        .find_row(entries[0].src, entries[0].relation)
        .unwrap();
    let mut old = SealedCursor::new(&partition.image, Some(row), start, CommitSeq(cut));
    let mut new = SealedCursor::new(&partition.image, Some(row), start, CommitSeq(cut));
    let expected: Vec<_> = entries[start..]
        .iter()
        .copied()
        .filter(|entry| entry.visible_at(CommitSeq(cut)))
        .collect();
    let (mut old_calls, mut new_calls, mut seen, mut pulls) = (0, 0, 0, 0);
    let (mut modeled_position, mut visible_prefix) = (start, 0);
    loop {
        let allowance = allowances[pulls % allowances.len()];
        pulls += 1;
        assert!(
            pulls <= 3 * entries.len() + 64,
            "a resumable scan must make progress"
        );
        let mut old_fuel = SealedScanBudget::new(allowance);
        let mut new_fuel = SealedScanBudget::new(allowance);
        let old_step = old.next_decoding_history(&mut old_fuel, &mut || {
            old_calls += 1;
            if stop == Some(old_calls) {
                Err(old_calls)
            } else {
                Ok(())
            }
        });
        let new_step = new.next_budgeted_inner(&mut new_fuel, &mut || {
            new_calls += 1;
            if stop == Some(new_calls) {
                Err(new_calls)
            } else {
                Ok(())
            }
        });
        assert_eq!(new_calls, old_calls);
        assert_eq!(new_fuel.remaining(), old_fuel.remaining());
        assert_eq!(new.finished, old.finished);
        let old_scan = old.scan.as_ref().unwrap();
        let new_scan = new.scan.as_ref().unwrap();
        assert_eq!(new_scan.position, old_scan.position);
        assert_eq!(old_scan.decoded_incidences(), old_scan.position - start);
        visible_prefix += entries[modeled_position..new_scan.position]
            .iter()
            .filter(|entry| entry.visible_at(CommitSeq(cut)))
            .count();
        modeled_position = new_scan.position;
        assert_eq!(
            new_scan.decoded_incidences(),
            if row.kind() == RowStorageKind::Inline {
                new_scan.position - start
            } else {
                visible_prefix
            },
        );
        match (old_step, new_step) {
            (Ok(SealedScanStep::Item(old_edge)), Ok(SealedScanStep::Item(new_edge))) => {
                assert_eq!(new_edge.entry, old_edge.entry);
                assert_eq!(new_edge.entry, expected[seen]);
                assert!(std::ptr::eq(new_edge.properties, old_edge.properties));
                assert_eq!(
                    new_edge.properties,
                    &[(
                        PropertyKeyId(1),
                        CanonicalScalar::Int((new_edge.entry.eid.0 - (1_u128 << 96)) as i64).into(),
                    )],
                );
                seen += 1;
            }
            (Ok(SealedScanStep::Yield), Ok(SealedScanStep::Yield)) => {}
            (Ok(SealedScanStep::End), Ok(SealedScanStep::End)) => {
                assert!(stop.is_none(), "the requested refusal must be reached");
                assert_eq!(seen, expected.len());
                break;
            }
            (Err(old_error), Err(new_error)) => {
                assert_eq!(Some(new_error), stop);
                assert_eq!(new_error, old_error);
                break;
            }
            other => panic!("different scan steps: {other:?}"),
        }
    }
    let mut zero = SealedScanBudget::new(0);
    assert!(matches!(
        new.next_budgeted_inner::<usize>(&mut zero, &mut || panic!("terminal cursor polled")),
        Ok(SealedScanStep::End)
    ));
    assert!(matches!(
        old.next_decoding_history::<usize>(&mut zero, &mut || panic!("terminal incumbent polled")),
        Ok(SealedScanStep::End)
    ));
    new_calls
}

#[test]
fn hidden_history_preserves_seek_fuel_properties_and_every_refusal() {
    let allowances: [&[usize]; 3] = [&[0, 1], &[63, 64, 65], &[0, 1, 0, 63, 64, 65, 257]];
    for count in [1, 8, 9, 65, 257] {
        for shape in [
            HistoryShape::AllVisible,
            HistoryShape::AllHidden,
            HistoryShape::Alternating,
            HistoryShape::LongGaps,
        ] {
            let (partition, entries) = history_fixture(count, shape);
            for cut in [1, 2, 3, 10] {
                let mut starts = vec![0, 1, 63, 64, 65, count - 1, count];
                starts.retain(|&start| start <= count);
                starts.sort_unstable();
                starts.dedup();
                for start in starts {
                    for allowance in allowances {
                        compare_history_pulls(&partition, &entries, start, cut, allowance, None);
                    }
                }
                // Sweep every typed refusal, including initial polls, zero
                // fuel, boundary polls and calls after a visible incidence.
                let allowance = allowances[2];
                let total = compare_history_pulls(&partition, &entries, 0, cut, allowance, None);
                for stop in 1..=total {
                    compare_history_pulls(&partition, &entries, 0, cut, allowance, Some(stop));
                }
            }
        }
    }
}

#[test]
fn large_retained_history_elides_decodes_and_keeps_short_gaps_sequential() {
    for shape in [
        HistoryShape::AllHidden,
        HistoryShape::Alternating,
        HistoryShape::LongGaps,
    ] {
        let (partition, entries) = history_fixture(8193, shape);
        let encoded = bytes(&partition);
        let reloaded = SealedPartition::reload_inner(
            partition.anchor(),
            &encoded,
            SealedLimits::default(),
            None,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(bytes(&reloaded), encoded);
        for partition in [&partition, &reloaded] {
            for start in [0, 1, 63, 64, 65, 4095, 4096, 4097, 8192, 8193] {
                compare_history_pulls(
                    partition,
                    &entries,
                    start,
                    3,
                    &[0, 1, 63, 64, 65, 4096],
                    None,
                );
            }
            let row = partition
                .image
                .find_row(entries[0].src, entries[0].relation)
                .unwrap();
            let mut cursor = SealedCursor::new(&partition.image, Some(row), 0, CommitSeq(3));
            while cursor.next_inner(&mut || Ok(())).unwrap().is_some() {}
            let scan = cursor.scan.as_ref().unwrap();
            let visible = entries
                .iter()
                .filter(|entry| entry.visible_at(CommitSeq(3)))
                .count();
            assert_eq!(scan.decoded_incidences(), visible);
            match shape {
                HistoryShape::AllHidden => {
                    assert_eq!(scan.decoded_incidences(), 0);
                    assert_eq!(scan.resumed_seeks(), 0);
                }
                HistoryShape::Alternating => assert_eq!(scan.resumed_seeks(), 0),
                HistoryShape::LongGaps => {
                    assert_eq!(visible, 7);
                    assert!(scan.resumed_seeks() <= 3);
                    let total =
                        compare_history_pulls(partition, &entries, 0, 3, &[usize::MAX], None);
                    for stop in 1..=total {
                        compare_history_pulls(partition, &entries, 0, 3, &[usize::MAX], Some(stop));
                    }
                }
                HistoryShape::AllVisible => unreachable!(),
            }
        }
    }
}

/// Same-process comparison of the incumbent outgoing pull and visibility-span
/// skipping. Timed regions exclude construction, wire admission, QueryCx,
/// property copying and durable I/O. This is not a Section 17 gate.
#[test]
#[ignore = "manual release-mode hidden-history decode-elision microbenchmark"]
fn benchmark_sealed_history_decode_elision_against_unfiltered() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    for count in [4096, 65536] {
        for shape in [
            HistoryShape::AllVisible,
            HistoryShape::AllHidden,
            HistoryShape::Alternating,
            HistoryShape::LongGaps,
        ] {
            let (partition, entries) = history_fixture(count, shape);
            let row = partition
                .image
                .find_row(entries[0].src, entries[0].relation)
                .unwrap();
            compare_history_pulls(&partition, &entries, 0, 3, &[usize::MAX], None);
            let scan = |filtered| {
                let mut cursor = SealedCursor::new(
                    black_box(&partition.image),
                    black_box(Some(row)),
                    0,
                    CommitSeq(3),
                );
                let mut fuel = SealedScanBudget::new(usize::MAX);
                let mut polls = 0;
                let mut sum = 0_u128;
                loop {
                    let mut check = || {
                        polls += 1;
                        Ok::<_, usize>(())
                    };
                    let step = if filtered {
                        cursor.next_budgeted_inner(&mut fuel, &mut check)
                    } else {
                        cursor.next_decoding_history(&mut fuel, &mut check)
                    }
                    .unwrap();
                    match step {
                        SealedScanStep::Item(edge) => {
                            let edge = black_box(edge);
                            sum = sum
                                .wrapping_add(edge.entry.eid.0)
                                .wrapping_add(edge.entry.dst.0);
                            black_box(edge.properties);
                        }
                        SealedScanStep::Yield => panic!("unbounded fixture fuel exhausted"),
                        SealedScanStep::End => break,
                    }
                }
                (
                    sum,
                    polls,
                    usize::MAX - fuel.remaining(),
                    cursor.scan.as_ref().unwrap().decoded_incidences(),
                )
            };
            let expected_old = scan(false);
            let expected_new = scan(true);
            assert_eq!(
                (expected_old.0, expected_old.1, expected_old.2),
                (expected_new.0, expected_new.1, expected_new.2)
            );
            assert_eq!(expected_old.2, count);
            assert_eq!(expected_old.3, count);
            let repeats = (1_usize << 18).div_ceil(count);
            let measure = |filtered, expected| {
                let began = Instant::now();
                for _ in 0..repeats {
                    assert_eq!(black_box(scan(filtered)), expected);
                }
                began.elapsed()
            };
            let mut old = [Duration::ZERO; 9];
            let mut new = [Duration::ZERO; 9];
            for round in 0_usize..9 {
                if round.is_multiple_of(2) {
                    old[round] = measure(false, expected_old);
                    new[round] = measure(true, expected_new);
                } else {
                    new[round] = measure(true, expected_new);
                    old[round] = measure(false, expected_old);
                }
            }
            old.sort_unstable();
            new.sort_unstable();
            let ns = |duration: Duration| duration.as_nanos() as f64 / (count * repeats) as f64;
            println!(
                "history={shape:?} incidences={count} rounds=9 full_decodes_old={} full_decodes_new={} raw_fuel={} polls={} old_ns={:.3} new_ns={:.3} speedup={:.3} empirical_gate_activated=false",
                expected_old.3,
                expected_new.3,
                expected_new.2,
                expected_new.1,
                ns(old[4]),
                ns(new[4]),
                ns(old[4]) / ns(new[4]),
            );
        }
    }
}
