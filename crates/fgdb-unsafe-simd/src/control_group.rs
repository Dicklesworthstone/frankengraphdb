//! Sixteen-lane control-group classification, scalar and vector.
//!
//! The semantic boundary is owned by `fgdb-collections::probe`, not by this
//! crate: a group has sixteen lanes, lane `n` maps to mask bit `n`, and mask
//! iteration therefore visits lower lanes first. This island implements the
//! same function over a raw `[u8; 16]` so that it depends on nothing, and
//! `tests/control_group_dispatch_compat.rs` proves the two agree by adapting
//! this kernel into a `ControlGroupDispatch` and differencing it against
//! `SCALAR_CONTROL_GROUP_DISPATCH`.
//!
//! [`classify_scalar`] is the specification. Every other path in
//! [`COMPILED_PATHS`] must equal it bit for bit on every input, which is what
//! `tests/dispatch_differential.rs` exercises across the whole matrix.

/// Number of control bytes in one probe group.
pub const CONTROL_GROUP_WIDTH: usize = 16;

/// Control byte denoting a bucket that has never held an entry.
pub const EMPTY_CONTROL: u8 = 0x80;

/// Control byte denoting a removed entry whose probe chain remains live.
pub const DELETED_CONTROL: u8 = 0xfe;

/// Classification masks for one control group, one bit per lane.
///
/// Bit `n` is lane `n` in every path, which is the property that lets a caller
/// use `trailing_zeros` for probe order without knowing which backend ran.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GroupMasks {
    /// Occupied lanes whose fingerprint equals the requested tag.
    pub matching: u16,
    /// Never-occupied lanes.
    pub empty: u16,
    /// Removed lanes that retain a probe chain.
    pub deleted: u16,
}

/// One implementation of the classification kernel.
///
/// This is a *compiled* dispatch matrix, not a runtime feature probe: SSE2 is
/// guaranteed by the x86-64 ABI, so the vector path is selected at compile time
/// and no CPUID check can drift from it. A path gated on a non-baseline feature
/// would need runtime detection and a decision card; none is gated that way
/// yet, and [`COMPILED_PATHS`] is the honest statement of what this build has.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DispatchPath {
    /// The portable safe reference every other path must match.
    Scalar,
    /// x86-64 SSE2, baseline on every x86-64 target.
    Sse2,
}

impl DispatchPath {
    /// Stable identifier for evidence logs.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::Sse2 => "sse2",
        }
    }
}

/// Every path compiled into this build, scalar first.
///
/// The differential harness iterates this rather than the active path, because
/// a harness that can only reach the selected backend proves only the selected
/// backend.
#[cfg(target_arch = "x86_64")]
pub const COMPILED_PATHS: &[DispatchPath] = &[DispatchPath::Scalar, DispatchPath::Sse2];

/// Every path compiled into this build, scalar first.
#[cfg(not(target_arch = "x86_64"))]
pub const COMPILED_PATHS: &[DispatchPath] = &[DispatchPath::Scalar];

/// The path [`classify`] takes on this build.
#[must_use]
pub const fn active_path() -> DispatchPath {
    #[cfg(target_arch = "x86_64")]
    {
        DispatchPath::Sse2
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        DispatchPath::Scalar
    }
}

/// Classifies `lanes` for `tag` through the best path compiled for this target.
#[must_use]
#[inline]
pub fn classify(lanes: &[u8; CONTROL_GROUP_WIDTH], tag: u8) -> GroupMasks {
    #[cfg(target_arch = "x86_64")]
    {
        classify_sse2(lanes, tag)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        classify_scalar(lanes, tag)
    }
}

/// Classifies through one named path, or `None` if this build does not contain
/// it.
///
/// `None` is a real answer and must not be read as "passed": the differential
/// harness asserts it only for paths absent from [`COMPILED_PATHS`], so a path
/// cannot silently drop out of the matrix and take its coverage with it.
#[must_use]
pub fn classify_via(
    path: DispatchPath,
    lanes: &[u8; CONTROL_GROUP_WIDTH],
    tag: u8,
) -> Option<GroupMasks> {
    match path {
        DispatchPath::Scalar => Some(classify_scalar(lanes, tag)),
        #[cfg(target_arch = "x86_64")]
        DispatchPath::Sse2 => Some(classify_sse2(lanes, tag)),
        #[cfg(not(target_arch = "x86_64"))]
        DispatchPath::Sse2 => None,
    }
}

/// The portable scalar specification.
///
/// Written branchlessly so it is a plain data-dependent function of the group,
/// but its contract is the loop it reads as, not its codegen: lane `n` sets bit
/// `n` of `matching` when the control byte equals the tag, of `empty` when it
/// is [`EMPTY_CONTROL`], and of `deleted` when it is [`DELETED_CONTROL`].
#[must_use]
#[inline]
pub fn classify_scalar(lanes: &[u8; CONTROL_GROUP_WIDTH], tag: u8) -> GroupMasks {
    let mut masks = GroupMasks::default();
    for (lane, &control) in lanes.iter().enumerate() {
        let bit = 1_u16 << lane;
        masks.matching |= bit * u16::from(control == tag);
        masks.empty |= bit * u16::from(control == EMPTY_CONTROL);
        masks.deleted |= bit * u16::from(control == DELETED_CONTROL);
    }
    masks
}

/// SSE2 classification: three compares and three mask extractions.
///
/// LEDGER ROW `simd-control-group-sse2`. The `allow` is unconditional because
/// this item exists only on x86-64 in the first place.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
#[must_use]
#[inline]
fn classify_sse2(lanes: &[u8; CONTROL_GROUP_WIDTH], tag: u8) -> GroupMasks {
    use core::arch::x86_64::{
        __m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_set1_epi8,
    };

    // SAFETY: three separate obligations, all discharged by construction.
    //
    // 1. `_mm_loadu_si128` reads exactly 16 bytes with no alignment
    //    requirement (it is the UNALIGNED load; `_mm_load_si128` is the one
    //    that would need 16-byte alignment). The pointer comes from a
    //    `&[u8; CONTROL_GROUP_WIDTH]` — a live shared reference to exactly 16
    //    initialized bytes — so the read is in bounds for its whole width and
    //    the borrow outlives the load.
    // 2. Every intrinsic here is SSE2, which the x86-64 ABI guarantees on
    //    every x86-64 target, so no runtime feature check can be missing. The
    //    `#[cfg(target_arch = "x86_64")]` on this item is what makes that
    //    statement true rather than assumed.
    // 3. `_mm_movemask_epi8` yields 16 meaningful bits in an `i32`, so the
    //    `as u16` truncation is total rather than lossy, and bit `n` is lane
    //    `n` — the same lane-to-bit map `classify_scalar` writes.
    //
    // The masks are computed from one immutable load; nothing here writes
    // through a pointer, and no value outlives the block.
    unsafe {
        let group = _mm_loadu_si128(lanes.as_ptr().cast::<__m128i>());
        GroupMasks {
            matching: _mm_movemask_epi8(_mm_cmpeq_epi8(group, _mm_set1_epi8(tag as i8))) as u16,
            empty: _mm_movemask_epi8(_mm_cmpeq_epi8(group, _mm_set1_epi8(EMPTY_CONTROL as i8)))
                as u16,
            deleted: _mm_movemask_epi8(_mm_cmpeq_epi8(
                group,
                _mm_set1_epi8(DELETED_CONTROL as i8),
            )) as u16,
        }
    }
}

/// Issues a physical-only prefetch hint for `controls[offset]`.
///
/// LEDGER ROW `simd-control-prefetch`. The `allow` is `cfg_attr`-scoped
/// because the `unsafe` block below exists only on x86-64; an unconditional
/// `allow` would be a relaxation standing open on every other target, for a
/// site that is not there. That spelling is also precisely the form that used
/// to walk past the site scanner unseen — the scanner now matches the
/// attribute structurally, so this site is counted like any other.
///
/// **Physical only** (§8.7): a prefetch may never change a result or a logical
/// order. This returns nothing, reads nothing, and is a no-op wherever the
/// hint does not exist, so removing every call must leave every observable
/// output identical. Out-of-range `offset` is not an error — it simply has no
/// hint to issue.
#[cfg_attr(target_arch = "x86_64", allow(unsafe_code))]
#[inline]
pub fn prefetch_controls(controls: &[u8], offset: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(control) = controls.get(offset) {
            // SAFETY: `control` is a live shared reference into `controls`, so
            // the pointer is valid for reads of at least one byte for the
            // duration of the call. `_mm_prefetch` issues a cache hint and
            // performs no architecturally visible access: it cannot fault, it
            // cannot write, and it cannot be observed in any result. SSE is
            // baseline on x86-64, and the `_MM_HINT_T0` strategy is a const
            // generic, so no invalid strategy can reach the instruction.
            unsafe {
                core::arch::x86_64::_mm_prefetch::<{ core::arch::x86_64::_MM_HINT_T0 }>(
                    core::ptr::from_ref(control).cast::<i8>(),
                );
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (controls, offset);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONTROL_GROUP_WIDTH, COMPILED_PATHS, DELETED_CONTROL, DispatchPath, EMPTY_CONTROL,
        GroupMasks, active_path, classify, classify_scalar, classify_via, prefetch_controls,
    };

    /// A third implementation, written as the obvious loop, so the scalar
    /// "specification" is itself checked against something rather than
    /// asserted to be correct.
    fn naive_reference(lanes: &[u8; CONTROL_GROUP_WIDTH], tag: u8) -> GroupMasks {
        let mut masks = GroupMasks::default();
        for lane in 0..CONTROL_GROUP_WIDTH {
            let bit = 1_u16 << lane;
            if lanes[lane] == tag {
                masks.matching |= bit;
            }
            if lanes[lane] == EMPTY_CONTROL {
                masks.empty |= bit;
            }
            if lanes[lane] == DELETED_CONTROL {
                masks.deleted |= bit;
            }
        }
        masks
    }

    #[test]
    fn scalar_matches_the_naive_reference_on_every_uniform_group_and_tag() {
        for control in u8::MIN..=u8::MAX {
            let lanes = [control; CONTROL_GROUP_WIDTH];
            for tag in u8::MIN..EMPTY_CONTROL {
                assert_eq!(classify_scalar(&lanes, tag), naive_reference(&lanes, tag));
            }
        }
    }

    #[test]
    fn every_sixteen_lane_mask_round_trips_through_every_compiled_path() {
        // 65_536 masks: each one places the tag in exactly the selected lanes,
        // so a path that permuted or dropped a lane cannot pass.
        let tag = 0x2a_u8;
        for expected in u16::MIN..=u16::MAX {
            let lanes = core::array::from_fn(|lane| {
                if expected & (1_u16 << lane) == 0 {
                    0x11
                } else {
                    tag
                }
            });
            for &path in COMPILED_PATHS {
                let masks = classify_via(path, &lanes, tag)
                    .unwrap_or_else(|| panic!("{} is compiled but unreachable", path.id()));
                assert_eq!(masks.matching, expected, "path {}", path.id());
                assert_eq!(masks.empty, 0, "path {}", path.id());
                assert_eq!(masks.deleted, 0, "path {}", path.id());
            }
        }
    }

    #[test]
    fn the_active_path_is_in_the_matrix_and_agrees_with_classify() {
        assert!(COMPILED_PATHS.contains(&active_path()));
        assert_eq!(COMPILED_PATHS[0], DispatchPath::Scalar);
        let lanes = core::array::from_fn(|lane| lane as u8);
        assert_eq!(
            classify(&lanes, 3),
            classify_via(active_path(), &lanes, 3).expect("the active path is compiled")
        );
    }

    #[test]
    fn an_uncompiled_path_answers_none_rather_than_a_wrong_mask() {
        // The negative half of `classify_via`: on a target without the vector
        // path, asking for it must not silently fall back to scalar and report
        // a pass for a backend that is not there.
        let lanes = [0_u8; CONTROL_GROUP_WIDTH];
        for path in [DispatchPath::Scalar, DispatchPath::Sse2] {
            assert_eq!(
                classify_via(path, &lanes, 0).is_some(),
                COMPILED_PATHS.contains(&path),
                "path {} answered out of step with the matrix",
                path.id()
            );
        }
    }

    #[test]
    fn prefetch_is_a_no_op_in_and_out_of_range() {
        // The whole claim about the hint is that it changes nothing, so the
        // test is that classification is identical either side of it.
        let controls: Vec<u8> = (0..64_u8).collect();
        let lanes: [u8; CONTROL_GROUP_WIDTH] = core::array::from_fn(|lane| controls[lane]);
        let before = classify(&lanes, 7);
        prefetch_controls(&controls, 0);
        prefetch_controls(&controls, controls.len() - 1);
        prefetch_controls(&controls, controls.len());
        prefetch_controls(&controls, usize::MAX);
        prefetch_controls(&[], 0);
        assert_eq!(classify(&lanes, 7), before);
    }
}
