use super::*;
use crate::edge_props::{BlockProps, EdgePropertyRow};
use fgdb_types::{EId, ObjectId};

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
                locators: (1..=block.len() as u32).collect(),
                rows: block
                    .iter()
                    .map(|edge| {
                        vec![(
                            PropertyKeyId(1),
                            CanonicalScalar::Int((edge.eid.0 - (1_u128 << 96)) as i64),
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
                        (edge, vec![(PropertyKeyId(1), CanonicalScalar::Int(value))])
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
            .map(|eid| vec![(PropertyKeyId(1), CanonicalScalar::Int(eid))])
            .collect(),
    };
    let change = vec![entry(1, 7, 1, 1, Some(5)), entry(1, 7, 1, 5, None)];
    let changed_props = BlockProps {
        locators: vec![1, 2],
        rows: vec![
            vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(99))],
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
        vec![(PropertyKeyId(1), CanonicalScalar::Int(1))]
    );
    assert_eq!(
        collect(&partition, 1, 5, None)[0].1,
        vec![(PropertyKeyId(1), CanonicalScalar::Int(99))]
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
                .map(|i| vec![(PropertyKeyId(1), CanonicalScalar::Int(chunk * 120 + i))])
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
            &vec![(PropertyKeyId(1), CanonicalScalar::Int(index as i64))]
        );
    }
}

#[test]
fn empty_image_is_canonical_and_resource_limits_remain_explicit() {
    let empty = seal(&[], &[], 0);
    let encoded = bytes(&empty);
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
