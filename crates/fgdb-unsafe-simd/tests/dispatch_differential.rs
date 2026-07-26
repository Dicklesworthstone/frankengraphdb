//! Scalar-fallback differential harness across the whole dispatch matrix
//! (bead `fgdb-w1-unsafe-islands-eqrq`; plan §8.7).
//!
//! A ledger row for a vector kernel claims two things: that the kernel is
//! sound, and that it is **bit-identical** to the scalar profile that defines
//! it. The first is argued in the `// SAFETY:` note; this is where the second
//! is measured, on every path the build actually contains rather than on
//! whichever one the host happens to select.
//!
//! Evidence is emitted as NDJSON — one line per (kernel, path) with the case
//! count and a rolling digest of the input and output streams, so two runs on
//! different hosts can be compared without keeping the cases themselves. Run
//! with `--nocapture` to retain it:
//!
//! ```text
//! cargo test -p fgdb-unsafe-simd --test dispatch_differential -- --nocapture
//! ```
//!
//! The digests are the point of the format. `bit_identical` alone would be a
//! bare green bar; an output digest that two paths must share is a value a
//! reader can diff, and an input digest pins which cases produced it.

use fgdb_unsafe_simd::{
    COMPILED_PATHS, CONTROL_GROUP_WIDTH, DELETED_CONTROL, DispatchPath, EMPTY_CONTROL, GroupMasks,
    classify_scalar, classify_via,
};

/// FNV-1a 64, in-house because the dependency universe is closed and a digest
/// used only to compare two streams needs no more than that.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fold(digest: u64, byte: u8) -> u64 {
    (digest ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
}

fn fold_bytes(mut digest: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        digest = fold(digest, byte);
    }
    digest
}

fn fold_masks(digest: u64, masks: GroupMasks) -> u64 {
    let mut digest = fold_bytes(digest, &masks.matching.to_le_bytes());
    digest = fold_bytes(digest, &masks.empty.to_le_bytes());
    fold_bytes(digest, &masks.deleted.to_le_bytes())
}

/// The deterministic case stream. No clock, no entropy: a seeded LCG plus
/// exhaustive families, so every run on every host sees the same inputs in the
/// same order and the digests below are comparable.
fn cases() -> Vec<([u8; CONTROL_GROUP_WIDTH], u8)> {
    let mut cases = Vec::new();

    // Family 1 — every control byte as a uniform group, against the tag that
    // matches it, a tag that cannot, and both reserved controls.
    for control in u8::MIN..=u8::MAX {
        let lanes = [control; CONTROL_GROUP_WIDTH];
        for tag in [control & 0x7f, 0x00, 0x7f, EMPTY_CONTROL & 0x7f] {
            cases.push((lanes, tag));
        }
    }

    // Family 2 — one distinguished lane against a uniform background, which is
    // what catches a permuted or off-by-one lane-to-bit map.
    for lane in 0..CONTROL_GROUP_WIDTH {
        for &control in &[0x2a_u8, EMPTY_CONTROL, DELETED_CONTROL] {
            let mut lanes = [0x11_u8; CONTROL_GROUP_WIDTH];
            lanes[lane] = control;
            cases.push((lanes, 0x2a));
        }
    }

    // Family 3 — mixed groups drawn from a seeded LCG, including the reserved
    // controls at realistic density, which is where matching/empty/deleted
    // interact.
    let mut state = 0x243f_6a88_85a3_08d3_u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 32) as u32
    };
    for _ in 0..4_096 {
        let lanes = core::array::from_fn(|_| match next() % 8 {
            0 => EMPTY_CONTROL,
            1 => DELETED_CONTROL,
            other => (other * 17) as u8 & 0x7f,
        });
        cases.push((lanes, (next() & 0x7f) as u8));
    }

    cases
}

/// Every case count is asserted, because a harness that silently ran zero
/// cases reports exactly what a passing one reports.
const EXPECTED_CASES: usize = 256 * 4 + CONTROL_GROUP_WIDTH * 3 + 4_096;

#[test]
fn every_compiled_path_is_bit_identical_to_the_scalar_profile() {
    let cases = cases();
    assert_eq!(
        cases.len(),
        EXPECTED_CASES,
        "the case stream changed; the digests below are not comparable to earlier runs"
    );

    let mut input_digest = FNV_OFFSET;
    for (lanes, tag) in &cases {
        input_digest = fold(fold_bytes(input_digest, lanes), *tag);
    }

    let mut scalar_output = FNV_OFFSET;
    for (lanes, tag) in &cases {
        scalar_output = fold_masks(scalar_output, classify_scalar(lanes, *tag));
    }

    assert!(
        COMPILED_PATHS.contains(&DispatchPath::Scalar),
        "the scalar profile is the specification and is always compiled"
    );

    for &path in COMPILED_PATHS {
        let mut output_digest = FNV_OFFSET;
        let mut mismatches = 0_usize;
        for (lanes, tag) in &cases {
            let expected = classify_scalar(lanes, *tag);
            let actual = classify_via(path, lanes, *tag).unwrap_or_else(|| {
                panic!(
                    "{} is in COMPILED_PATHS but classify_via cannot reach it",
                    path.id()
                )
            });
            if actual != expected {
                mismatches += 1;
                if mismatches <= 4 {
                    println!(
                        "{{\"event\":\"dispatch_case_mismatch\",\"kernel_id\":\"control_group_classify\",\"dispatch_path\":\"{}\",\"lanes\":{lanes:?},\"tag\":{tag},\"expected\":{expected:?},\"actual\":{actual:?}}}",
                        path.id()
                    );
                }
            }
            output_digest = fold_masks(output_digest, actual);
        }
        let bit_identical = mismatches == 0 && output_digest == scalar_output;
        println!(
            "{{\"event\":\"dispatch_differential\",\"kernel_id\":\"control_group_classify\",\"dispatch_path\":\"{}\",\"cases\":{},\"input_digest\":\"fnv1a64:{input_digest:016x}\",\"output_digest\":\"fnv1a64:{output_digest:016x}\",\"mismatches\":{mismatches},\"bit_identical\":{bit_identical},\"outcome\":\"{}\"}}",
            path.id(),
            cases.len(),
            if bit_identical { "pass" } else { "fail" }
        );
        assert_eq!(
            mismatches,
            0,
            "path {} drifted from the scalar profile on {mismatches} of {} cases",
            path.id(),
            cases.len()
        );
        assert_eq!(
            output_digest,
            scalar_output,
            "path {} produced a different output stream than the scalar profile",
            path.id()
        );
    }
}

/// The harness must be able to tell a drifting path from an agreeing one. If
/// it cannot, every "bit_identical: true" above is unlicensed — the same
/// control the ledger checker applies to its own site scanner.
#[test]
fn the_harness_detects_a_deliberately_wrong_path() {
    let cases = cases();
    let mut wrong_output = FNV_OFFSET;
    let mut scalar_output = FNV_OFFSET;
    let mut mismatches = 0_usize;
    for (lanes, tag) in &cases {
        let expected = classify_scalar(lanes, *tag);
        // One bit rotated: the smallest lane-map defect a vector kernel can
        // plausibly have, and the one an aggregate digest must not absorb.
        let wrong = GroupMasks {
            matching: expected.matching.rotate_left(1),
            ..expected
        };
        if wrong != expected {
            mismatches += 1;
        }
        wrong_output = fold_masks(wrong_output, wrong);
        scalar_output = fold_masks(scalar_output, expected);
    }
    assert!(
        mismatches > 0,
        "a rotated mask must be caught case by case"
    );
    assert_ne!(
        wrong_output, scalar_output,
        "the output digest must separate a drifting path from an agreeing one"
    );
}
