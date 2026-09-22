use super::*;

fn basis() -> PayloadBasis {
    PayloadBasis { domain: Domain([1; 32]), configuration: [2; 32],
        predicate_digest: [3; 32], base_closure_digest: [4; 32] }
}
fn member(index: u8) -> MemberId { MemberId((1_u128 << 96) + u128::from(index)) }
fn fixture(count: u8, faults: usize) -> AvailabilityInput {
    let members: Vec<_> = (1..=count).map(member).collect();
    let mut input = AvailabilityInput {
        policy: AvailabilityPolicy { basis: basis(), storage_sets: StorageSets::Stable(members),
            locations: (1..=count).map(|id| StorageLocation { member: member(id),
                placement_id: [id; 32], failure_domains: vec![FailureDomain(u128::from(id))] }).collect(),
            tolerated_domain_failures: faults },
        requirements: vec![EncodingRequirement { object_id: ObjectId([10; 32]),
            encoding_id: [11; 32], source_symbols: vec![3] }],
        receipts: Vec::new(),
    };
    input.receipts = (1..=count).map(|id| ReceiptCoverage { receipt_id: [id; 32], basis: basis(),
        storage_member: member(id), prepared_ownership_id: [id + 20; 32],
        coverage: vec![span(id, 0, 3)] }).collect();
    input
}
fn span(placement: u8, first: u32, end: u32) -> SourceCoverage {
    SourceCoverage { object_id: ObjectId([10; 32]), encoding_id: [11; 32],
        placement_id: [placement; 32], source_block: 0, first_esi: first, end_esi: end }
}
fn check(input: &AvailabilityInput) -> Result<SystematicAssessment<'_>, AvailabilityError<()>> {
    assess_systematic(input, AvailabilityLimits::default(), &mut || Ok(()))
}
fn gap(input: &AvailabilityInput) -> CoverageGap {
    match check(input) {
        Err(AvailabilityError::Unrecoverable(gap)) => gap,
        result => panic!("expected exact coverage gap, got {result:?}"),
    }
}

#[test]
fn quorum_one_still_requires_the_complete_inventory() {
    let mut input = fixture(1, 0);
    assert_eq!(check(&input).unwrap().checked_failure_cases(), 1);
    input.receipts.clear();
    assert_eq!(gap(&input).first_missing_esi, 0);
}

#[test]
fn replicas_in_independent_domains_survive_each_declared_loss() {
    for f in 0..3 {
        let input = fixture(3, f);
        assert_eq!(check(&input).unwrap().checked_failure_cases(), cut_count(3, f) as u64);
    }
    let mut input = fixture(3, 1);
    // Three members but one common physical failure domain.
    for location in &mut input.policy.locations {
        location.failure_domains.push(FailureDomain(99));
    }
    let failure = gap(&input);
    assert_eq!(failure.failed_domains, [FailureDomain(99)]);
    assert_eq!(failure.first_missing_esi, 0);
}

#[test]
fn overlapping_rack_and_zone_failures_remove_every_affected_placement() {
    let mut input = fixture(3, 1);
    input.policy.locations[0].failure_domains.push(FailureDomain(4));
    input.policy.locations[1].failure_domains.push(FailureDomain(4));
    input.receipts[2].coverage = vec![span(3, 1, 3)];
    // Losing either host alone is fine. Losing their shared rack loses ESI 0.
    assert_eq!(gap(&input).failed_domains, [FailureDomain(4)]);
}

#[test]
fn striped_source_ranges_union_without_double_counting_overlap() {
    let mut input = fixture(3, 0);
    input.receipts[0].coverage = vec![span(1, 0, 1)];
    input.receipts[1].coverage = vec![span(2, 1, 2)];
    input.receipts[2].coverage = vec![span(3, 2, 3)];
    assert!(check(&input).is_ok());
    input.receipts[1].coverage = vec![span(2, 0, 1); 16];
    // Many equations/receipts of the SAME source coordinate do not cover ESI 1.
    assert_eq!(gap(&input).first_missing_esi, 1);
}

#[test]
fn joint_storage_quorums_never_pool_their_payloads() {
    let mut input = fixture(3, 0);
    input.policy.storage_sets = StorageSets::Joint {
        old: vec![member(1), member(2)], new: vec![member(2), member(3)],
    };
    input.receipts[0].coverage = vec![span(1, 0, 3)];
    input.receipts[1].coverage.clear();
    input.receipts[2].coverage = vec![span(3, 1, 3)];
    assert_eq!(gap(&input).side, StorageSide::New);
    input.receipts[2].coverage.push(span(3, 0, 1));
    assert_eq!(check(&input).unwrap().checked_failure_cases(), 2);
    input.policy.tolerated_domain_failures = 1;
    assert!(matches!(check(&input), Err(AvailabilityError::Unrecoverable(_))));
    input.receipts[1].coverage = vec![span(2, 0, 3)];
    assert_eq!(check(&input).unwrap().checked_failure_cases(), 6);
}

#[test]
fn every_encoding_and_source_block_is_checked_separately() {
    let mut input = fixture(1, 0);
    input.requirements[0].source_symbols.push(2);
    assert_eq!(gap(&input).source_block, 1);
    let mut block = span(1, 0, 2);
    block.source_block = 1;
    input.receipts[0].coverage.push(block);
    assert!(check(&input).is_ok());
    input.requirements.push(EncodingRequirement { object_id: ObjectId([10; 32]),
        encoding_id: [12; 32], source_symbols: vec![3] });
    assert_eq!(gap(&input).encoding_id, [12; 32]);
    let mut different = span(1, 0, 3);
    different.encoding_id = [12; 32];
    input.receipts[0].coverage.push(different);
    assert!(check(&input).is_ok());
    input.receipts[0].coverage[0].end_esi = 2;
    assert_eq!(gap(&input).encoding_id, [11; 32]);
    assert_eq!(gap(&input).first_missing_esi, 2);
}

#[test]
fn repair_symbols_are_not_mislabeled_as_independent_source_symbols() {
    let mut input = fixture(1, 0);
    input.receipts[0].coverage = vec![span(1, 3, 6)];
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::InvalidCoverage);
    input.receipts[0].coverage = vec![span(1, 0, 0)];
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::InvalidCoverage);
    input.receipts[0].coverage = vec![span(1, 0, 4)];
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::InvalidCoverage);
}

#[test]
fn foreign_domains_configurations_predicates_and_closures_reject() {
    let original = fixture(1, 0);
    for field in 0..4 {
        let mut input = original.clone();
        match field {
            0 => input.receipts[0].basis.domain = Domain([99; 32]),
            1 => input.receipts[0].basis.configuration = [99; 32],
            2 => input.receipts[0].basis.predicate_digest = [99; 32],
            _ => input.receipts[0].basis.base_closure_digest = [99; 32],
        }
        assert_eq!(check(&input).unwrap_err(), AvailabilityError::WrongBasis);
    }
}

#[test]
fn receipt_and_placement_identity_cannot_be_reassigned() {
    let original = fixture(2, 0);
    let mut input = original.clone();
    input.receipts[0].storage_member = member(99);
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::UnknownMember);
    input = original.clone();
    input.receipts[0].coverage[0].placement_id = [2; 32];
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::UnknownPlacement);
    input = original.clone();
    input.receipts[0].coverage[0].object_id = ObjectId([99; 32]);
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::UnknownEncoding);
    input = original.clone();
    input.receipts[1].receipt_id = input.receipts[0].receipt_id;
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::DuplicateReceipt);
    input = original;
    input.receipts[1].storage_member = input.receipts[0].storage_member;
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::DuplicateReceipt);
}

#[test]
fn malformed_inventory_and_ambiguous_policy_fail_closed() {
    let original = fixture(2, 0);
    for case in 0..8 {
        let mut input = original.clone();
        match case {
            0 => input.policy.locations[1].placement_id = input.policy.locations[0].placement_id,
            1 => input.policy.locations[0].failure_domains.clear(),
            2 => input.policy.locations[0].failure_domains.push(FailureDomain(1)),
            3 => input.policy.storage_sets = StorageSets::Stable(vec![member(1), member(1)]),
            4 => input.policy.storage_sets = StorageSets::Stable(vec![member(2), member(1)]),
            5 => input.policy.storage_sets = StorageSets::Stable(Vec::new()),
            6 => input.policy.locations[0].member = member(99),
            _ => input.policy.tolerated_domain_failures = 2,
        }
        assert_eq!(check(&input).unwrap_err(), AvailabilityError::InvalidPolicy);
    }
    for count in [0, MAX_SYSTEMATIC_SYMBOLS + 1] {
        let mut input = original.clone();
        input.requirements[0].source_symbols = vec![count];
        assert_eq!(check(&input).unwrap_err(), AvailabilityError::InvalidInventory);
    }
    let mut input = original.clone();
    input.requirements.push(input.requirements[0].clone());
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::InvalidInventory);
    input.requirements[1].object_id = ObjectId([99; 32]);
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::InvalidInventory);
}

#[test]
fn large_source_coordinates_do_not_expand_to_per_symbol_storage_or_work() {
    let mut input = fixture(1, 0);
    let work = check(&input).unwrap().work();
    input.requirements[0].source_symbols[0] = MAX_SYSTEMATIC_SYMBOLS;
    input.receipts[0].coverage[0].end_esi = MAX_SYSTEMATIC_SYMBOLS;
    assert_eq!(check(&input).unwrap().work(), work);
}

#[test]
fn failure_case_budget_precedes_exponential_enumeration() {
    let mut input = fixture(20, 10);
    assert_eq!(check(&input).unwrap_err(), AvailabilityError::FailureCaseBudget);
    input.policy.tolerated_domain_failures = 1;
    let mut limits = AvailabilityLimits::default();
    limits.max_failure_cases = 20;
    assert!(assess_systematic(&input, limits, &mut || Ok::<(), ()>(())).is_ok());
    limits.max_failure_cases = 19;
    assert_eq!(assess_systematic(&input, limits, &mut || Ok::<(), ()>(())).unwrap_err(),
        AvailabilityError::FailureCaseBudget);
}

#[test]
fn input_work_and_cancellation_budgets_do_not_emit_partial_proofs() {
    let mut input = fixture(3, 1);
    for receipt in &mut input.receipts {
        receipt.coverage = vec![receipt.coverage[0].clone(); 256];
    }
    let saved = input.clone();
    let mut callbacks = 0;
    let work = assess_systematic(&input, AvailabilityLimits::default(), &mut || {
        callbacks += 1; Ok::<(), usize>(())
    }).unwrap().work();
    for stop in 1..=callbacks {
        let mut call = 0;
        let result = assess_systematic(&input, AvailabilityLimits::default(), &mut || {
            call += 1;
            if call == stop { Err(stop) } else { Ok(()) }
        });
        assert_eq!(result.unwrap_err(), AvailabilityError::Interrupted(stop));
        assert_eq!(input, saved);
    }
    let mut limits = AvailabilityLimits { max_work: work, ..AvailabilityLimits::default() };
    assert!(assess_systematic(&input, limits, &mut || Ok::<(), ()>(())).is_ok());
    limits.max_work -= 1;
    assert_eq!(assess_systematic(&input, limits, &mut || Ok::<(), ()>(())).unwrap_err(),
        AvailabilityError::WorkBudget);
    limits = AvailabilityLimits { max_spans: 767, ..AvailabilityLimits::default() };
    assert_eq!(assess_systematic(&input, limits, &mut || Ok::<(), ()>(())).unwrap_err(),
        AvailabilityError::InputBudget);
}

#[test]
fn cut_enumeration_matches_an_independent_bitmask_oracle() {
    for n in 0..=12 {
        for k in 0..=n {
            let actual: BTreeSet<_> = Cuts::new(n, k).collect();
            let expected: BTreeSet<_> = (0..(1_u64 << n)).filter(|mask| mask.count_ones() as usize == k).collect();
            assert_eq!(actual, expected);
            assert_eq!(actual.len() as u128, cut_count(n, k));
        }
    }
    assert_eq!(Cuts::new(64, 1).last(), Some(1_u64 << 63));
}

#[test]
fn exhaustive_small_placements_match_per_symbol_failure_recomputation() {
    // 512 coverage matrices x three loss bounds x stable/joint storage forms.
    // This oracle enumerates EVERY <=f failure set and EVERY source coordinate,
    // independently of production's maximal-cut interval-union algorithm.
    for bits in 0..512_u32 {
        for f in 0..3 {
            for joint in [false, true] {
                let mut input = fixture(3, f);
                if joint {
                    input.policy.storage_sets = StorageSets::Joint {
                        old: vec![member(1), member(2)], new: vec![member(2), member(3)],
                    };
                }
                for donor in 0..3 {
                    input.receipts[donor].coverage = (0..3_u32)
                        .filter(|esi| bits & (1 << (donor as u32 * 3 + esi)) != 0)
                        .map(|esi| span(donor as u8 + 1, esi, esi + 1)).collect();
                }
                let masks: &[u32] = if joint { &[0b011, 0b110] } else { &[0b111] };
                let expected = masks.iter().all(|members| (0..8_u32)
                    .filter(|failed| failed.count_ones() as usize <= f)
                    .all(|failed| (0..3_u32).all(|esi| (0..3_u32).any(|donor| {
                        members & (1 << donor) != 0 && failed & (1 << donor) == 0
                            && bits & (1 << (donor * 3 + esi)) != 0
                    }))));
                assert_eq!(check(&input).is_ok(), expected, "bits={bits}, f={f}, joint={joint}");
            }
        }
    }
}
