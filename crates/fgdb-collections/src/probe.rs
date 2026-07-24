//! Safe control-group classification for open-addressed collections.
//!
//! This module fixes the semantic boundary that a later ledgered vector
//! backend must implement.  A group always has sixteen lanes, lane zero maps
//! to the least-significant mask bit, and mask iteration always visits lower
//! lanes first.  The scalar implementation is the portable reference oracle.

use core::fmt;

/// Number of control bytes in one probe group.
pub const CONTROL_GROUP_WIDTH: usize = 16;

/// Control byte denoting a bucket that has never held an entry.
pub const EMPTY_CONTROL: u8 = 0x80;

/// Control byte denoting a removed entry whose probe chain remains live.
pub const DELETED_CONTROL: u8 = 0xfe;

const BYTE_BROADCAST: u128 = 0x0101_0101_0101_0101_0101_0101_0101_0101;
const LOW_SEVEN_BITS_PER_BYTE: u128 = 0x7f7f_7f7f_7f7f_7f7f_7f7f_7f7f_7f7f_7f7f;
const HIGH_BIT_PER_BYTE: u128 = 0x8080_8080_8080_8080_8080_8080_8080_8080;
const PACK_BYTE_HIGH_BITS: u64 = 0x0102_0408_1020_4080;

/// Seven-bit fingerprint stored in an occupied control lane.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ControlTag(u8);

impl ControlTag {
    /// Creates an occupied tag, rejecting the reserved control-byte space.
    #[must_use]
    pub const fn new(value: u8) -> Option<Self> {
        if value < EMPTY_CONTROL {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Derives the table's seven-bit fingerprint from a full hash.
    #[must_use]
    pub const fn from_hash(hash: u64) -> Self {
        Self(((hash >> 57) & 0x7f) as u8)
    }

    /// Returns the encoded occupied control byte.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// One control group in logical probe-lane order.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ControlGroup {
    lanes: [u8; CONTROL_GROUP_WIDTH],
}

impl ControlGroup {
    /// Creates a group from bytes already arranged in logical lane order.
    #[must_use]
    pub const fn new(lanes: [u8; CONTROL_GROUP_WIDTH]) -> Self {
        Self { lanes }
    }

    /// Gathers sixteen wrapping lanes from a power-of-two control array.
    ///
    /// This safe gather defines wraparound semantics independently of how a
    /// future backend loads the same bytes.  `None` rejects malformed storage
    /// rather than permitting an out-of-range access.
    #[must_use]
    #[inline]
    pub fn gather_wrapping(controls: &[u8], start: usize) -> Option<Self> {
        if controls.len() < CONTROL_GROUP_WIDTH
            || !controls.len().is_power_of_two()
            || start >= controls.len()
        {
            return None;
        }
        let mask = controls.len() - 1;
        Some(Self::new(core::array::from_fn(|lane| {
            controls[(start + lane) & mask]
        })))
    }

    /// Returns the bytes in logical lane order.
    #[must_use]
    pub const fn lanes(&self) -> &[u8; CONTROL_GROUP_WIDTH] {
        &self.lanes
    }
}

impl fmt::Debug for ControlGroup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ControlGroup")
            .field(&self.lanes)
            .finish()
    }
}

/// Bitset over the sixteen logical lanes.
///
/// Bit `n` corresponds exactly to lane `n`; consequently `first()` and
/// iteration preserve scalar probe order.
#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LaneMask(u16);

impl LaneMask {
    /// Empty lane mask.
    pub const NONE: Self = Self(0);

    /// Creates a lane mask from its canonical sixteen bits.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Returns the canonical mask bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether no lane is selected.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether `lane` is selected.
    #[must_use]
    pub const fn contains(self, lane: usize) -> bool {
        lane < CONTROL_GROUP_WIDTH && (self.0 & (1_u16 << lane)) != 0
    }

    /// Lowest selected lane, which is the first lane in probe order.
    #[must_use]
    #[inline]
    pub fn first(self) -> Option<usize> {
        if self.is_empty() {
            None
        } else {
            Some(self.0.trailing_zeros() as usize)
        }
    }

    /// Iterates selected lanes from lowest to highest.
    #[must_use]
    pub const fn iter(self) -> LaneMaskIter {
        LaneMaskIter { remaining: self.0 }
    }
}

impl fmt::Debug for LaneMask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "LaneMask({:#018b})", self.0)
    }
}

/// Ascending iterator over selected lane indexes.
#[derive(Clone, Copy, Debug)]
pub struct LaneMaskIter {
    remaining: u16,
}

impl Iterator for LaneMaskIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let lane = self.remaining.trailing_zeros() as usize;
        self.remaining &= self.remaining - 1;
        Some(lane)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining.count_ones() as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for LaneMaskIter {}
impl core::iter::FusedIterator for LaneMaskIter {}

/// Classification masks for one control group.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControlGroupMasks {
    /// Occupied lanes whose fingerprint equals the requested tag.
    pub matching: LaneMask,
    /// Never-occupied lanes.
    pub empty: LaneMask,
    /// Removed lanes that retain a probe chain.
    pub deleted: LaneMask,
}

impl ControlGroupMasks {
    /// Occupied lanes, including both matching and non-matching fingerprints.
    #[must_use]
    pub const fn occupied(self) -> LaneMask {
        LaneMask::from_bits(!(self.empty.bits() | self.deleted.bits()))
    }
}

/// Semantic interface implemented by a control-group classifier.
///
/// Implementations must be bit-identical to [`ScalarControlGroupClassifier`]:
/// lane `n` maps to bit `n`, and all three masks are computed independently
/// from the same immutable group.
pub trait ControlGroupClassifier {
    /// Classifies `group` for `tag`.
    fn classify(&self, group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks;
}

/// Portable safe reference classifier.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScalarControlGroupClassifier;

impl ControlGroupClassifier for ScalarControlGroupClassifier {
    fn classify(&self, group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks {
        scalar_classify(group, tag)
    }
}

/// Classifies a group with the safe scalar reference algorithm.
#[must_use]
#[inline]
pub fn scalar_classify(group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks {
    let mut matching = 0_u16;
    let mut empty = 0_u16;
    let mut deleted = 0_u16;
    for (lane, &control) in group.lanes().iter().enumerate() {
        let bit = 1_u16 << lane;
        matching |= bit * u16::from(control == tag.get());
        empty |= bit * u16::from(control == EMPTY_CONTROL);
        deleted |= bit * u16::from(control == DELETED_CONTROL);
    }
    ControlGroupMasks {
        matching: LaneMask::from_bits(matching),
        empty: LaneMask::from_bits(empty),
        deleted: LaneMask::from_bits(deleted),
    }
}

/// Portable safe SWAR classifier.
///
/// The implementation compares all sixteen control bytes with word-level
/// integer operations. It is not an unsafe SIMD island: it exists as a second
/// portable implementation so dispatch integration can be differential-tested
/// before a target-specific vector backend is admitted.
#[derive(Clone, Copy, Debug, Default)]
pub struct SwarControlGroupClassifier;

impl ControlGroupClassifier for SwarControlGroupClassifier {
    fn classify(&self, group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks {
        swar_classify(group, tag)
    }
}

/// Classifies a group with portable safe SWAR operations.
///
/// `u128::from_le_bytes` fixes lane zero as the low byte on every target. The
/// exact zero-byte detector cannot borrow or carry between lanes: every masked
/// seven-bit lane plus `0x7f` is at most `0xfe`.
#[must_use]
#[inline]
pub fn swar_classify(group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks {
    let packed = u128::from_le_bytes(*group.lanes());
    ControlGroupMasks {
        matching: compress_byte_high_bits(equal_byte_high_bits(packed, tag.get())),
        empty: compress_byte_high_bits(equal_byte_high_bits(packed, EMPTY_CONTROL)),
        deleted: compress_byte_high_bits(equal_byte_high_bits(packed, DELETED_CONTROL)),
    }
}

#[inline]
fn equal_byte_high_bits(packed: u128, expected: u8) -> u128 {
    let different = packed ^ (u128::from(expected) * BYTE_BROADCAST);
    let low_sum = (different & LOW_SEVEN_BITS_PER_BYTE) + LOW_SEVEN_BITS_PER_BYTE;
    !(low_sum | different | LOW_SEVEN_BITS_PER_BYTE) & HIGH_BIT_PER_BYTE
}

#[inline]
fn compress_byte_high_bits(high_bits: u128) -> LaneMask {
    let low = compress_eight_byte_high_bits(high_bits as u64);
    let high = compress_eight_byte_high_bits((high_bits >> 64) as u64);
    LaneMask::from_bits(u16::from(low) | (u16::from(high) << 8))
}

#[inline]
fn compress_eight_byte_high_bits(high_bits: u64) -> u8 {
    (((high_bits >> 7).wrapping_mul(PACK_BYTE_HIGH_BITS)) >> 56) as u8
}

/// Function signature used by runtime-selected control-group backends.
pub type ControlGroupClassifyFn = fn(&ControlGroup, ControlTag) -> ControlGroupMasks;

/// Copyable classification dispatch selected outside the table's probe loop.
///
/// A later vector boundary can supply a different function only after proving
/// bit-for-bit equivalence against [`SCALAR_CONTROL_GROUP_DISPATCH`].
#[derive(Clone, Copy)]
pub struct ControlGroupDispatch {
    classify: ControlGroupClassifyFn,
}

impl ControlGroupDispatch {
    /// Constructs a dispatch from a classifier function.
    #[must_use]
    pub const fn new(classify: ControlGroupClassifyFn) -> Self {
        Self { classify }
    }

    /// Classifies one group through this dispatch.
    #[must_use]
    #[inline]
    pub fn classify(self, group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks {
        (self.classify)(group, tag)
    }
}

impl fmt::Debug for ControlGroupDispatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlGroupDispatch")
            .finish_non_exhaustive()
    }
}

/// Dispatch for the safe scalar reference backend.
pub const SCALAR_CONTROL_GROUP_DISPATCH: ControlGroupDispatch =
    ControlGroupDispatch::new(scalar_classify);

/// Dispatch for the portable safe SWAR backend.
pub const SWAR_CONTROL_GROUP_DISPATCH: ControlGroupDispatch =
    ControlGroupDispatch::new(swar_classify);

#[cfg(test)]
mod tests {
    use super::{
        CONTROL_GROUP_WIDTH, ControlGroup, ControlGroupClassifier, ControlGroupMasks, ControlTag,
        DELETED_CONTROL, EMPTY_CONTROL, LaneMask, SCALAR_CONTROL_GROUP_DISPATCH,
        SWAR_CONTROL_GROUP_DISPATCH, ScalarControlGroupClassifier, SwarControlGroupClassifier,
        scalar_classify, swar_classify,
    };

    fn direct_reference(group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks {
        let mut matching = 0_u16;
        let mut empty = 0_u16;
        let mut deleted = 0_u16;
        for lane in 0..CONTROL_GROUP_WIDTH {
            let bit = 1_u16 << lane;
            let control = group.lanes()[lane];
            if control == tag.get() {
                matching |= bit;
            }
            if control == EMPTY_CONTROL {
                empty |= bit;
            }
            if control == DELETED_CONTROL {
                deleted |= bit;
            }
        }
        ControlGroupMasks {
            matching: LaneMask::from_bits(matching),
            empty: LaneMask::from_bits(empty),
            deleted: LaneMask::from_bits(deleted),
        }
    }

    #[test]
    fn lane_bits_are_stable_and_iterate_in_probe_order() {
        let tag = ControlTag(0x2a);
        for lane in 0..CONTROL_GROUP_WIDTH {
            let mut controls = [0x11; CONTROL_GROUP_WIDTH];
            controls[lane] = tag.get();
            controls[(lane + 3) % CONTROL_GROUP_WIDTH] = EMPTY_CONTROL;
            controls[(lane + 7) % CONTROL_GROUP_WIDTH] = DELETED_CONTROL;
            let masks = scalar_classify(&ControlGroup::new(controls), tag);
            assert_eq!(masks.matching.bits(), 1_u16 << lane);
            assert_eq!(
                masks.empty.bits(),
                1_u16 << ((lane + 3) % CONTROL_GROUP_WIDTH)
            );
            assert_eq!(
                masks.deleted.bits(),
                1_u16 << ((lane + 7) % CONTROL_GROUP_WIDTH)
            );
        }

        let mask = LaneMask::from_bits(0b1010_0100_0010_1001);
        assert_eq!(mask.first(), Some(0));
        assert_eq!(mask.iter().collect::<Vec<_>>(), vec![0, 3, 5, 10, 13, 15]);
        assert_eq!(mask.iter().len(), 6);
    }

    #[test]
    fn wrapping_gather_preserves_logical_lane_order() {
        let controls: Vec<_> = (0_u8..32).collect();
        for start in 0..controls.len() {
            let expected =
                core::array::from_fn(|lane| controls[(start + lane) & (controls.len() - 1)]);
            assert_eq!(
                ControlGroup::gather_wrapping(&controls, start),
                Some(ControlGroup::new(expected))
            );
        }
        assert_eq!(ControlGroup::gather_wrapping(&[], 0), None);
        assert_eq!(ControlGroup::gather_wrapping(&[0; 15], 0), None);
        assert_eq!(ControlGroup::gather_wrapping(&[0; 24], 0), None);
        assert_eq!(ControlGroup::gather_wrapping(&[0; 16], 16), None);
    }

    #[test]
    fn scalar_trait_and_dispatch_are_directly_equivalent() {
        let classifier = ScalarControlGroupClassifier;
        let swar_classifier = SwarControlGroupClassifier;
        let mut state = 0x243f_6a88_85a3_08d3_u64;
        for _ in 0..4_096 {
            let controls = core::array::from_fn(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 32) as u8
            });
            let tag = ControlTag::from_hash(state);
            let group = ControlGroup::new(controls);
            let expected = direct_reference(&group, tag);
            assert_eq!(scalar_classify(&group, tag), expected);
            assert_eq!(classifier.classify(&group, tag), expected);
            assert_eq!(
                SCALAR_CONTROL_GROUP_DISPATCH.classify(&group, tag),
                expected
            );
            assert_eq!(swar_classify(&group, tag), expected);
            assert_eq!(swar_classifier.classify(&group, tag), expected);
            assert_eq!(SWAR_CONTROL_GROUP_DISPATCH.classify(&group, tag), expected);
        }
    }

    #[test]
    fn classification_is_exhaustive_for_every_control_byte_and_tag() {
        for control in u8::MIN..=u8::MAX {
            let group = ControlGroup::new([control; CONTROL_GROUP_WIDTH]);
            for tag in u8::MIN..EMPTY_CONTROL {
                let tag = ControlTag(tag);
                let expected = direct_reference(&group, tag);
                assert_eq!(scalar_classify(&group, tag), expected);
                assert_eq!(
                    SCALAR_CONTROL_GROUP_DISPATCH.classify(&group, tag),
                    expected
                );
                assert_eq!(swar_classify(&group, tag), expected);
                assert_eq!(SWAR_CONTROL_GROUP_DISPATCH.classify(&group, tag), expected);
            }
        }
    }

    #[test]
    fn swar_compression_preserves_every_sixteen_lane_mask() {
        let tag = ControlTag::new(0x2a).expect("test tag is occupied");
        for expected in u16::MIN..=u16::MAX {
            let controls = core::array::from_fn(|lane| {
                if expected & (1_u16 << lane) == 0 {
                    0x11
                } else {
                    tag.get()
                }
            });
            let masks = swar_classify(&ControlGroup::new(controls), tag);
            assert_eq!(masks.matching.bits(), expected);
            assert!(masks.empty.is_empty());
            assert!(masks.deleted.is_empty());
        }
    }
}
