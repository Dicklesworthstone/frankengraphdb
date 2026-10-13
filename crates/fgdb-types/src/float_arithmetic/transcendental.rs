//! Pinned binary64 transcendental kernels: exp, ln, log10, sin, cos, tan,
//! atan, asin, acos and atan2 (plan section 8.6: STRICT_PORTABLE floating
//! execution "uses pinned math kernels").
//!
//! Every floating step is one checked operation of the parent integer-only
//! kernel, so each result is the same bits on every target. No host libm, FMA
//! or ambient rounding mode is consulted; a host `sin` can differ in its last
//! bit across platforms. The algorithms and constants are FreeBSD msun's (the
//! fdlibm lineage). The trigonometric argument reduction is an exact integer
//! Payne-Hanek over 1280 bits of 2/pi, not msun's floating reduction. Against
//! a 60-digit reference every measured result is within one ulp, and atan2's
//! within 1.2 ulp. This is not a correctly rounded library.
//!
//! Domains follow openCypher wherever the arithmetic kernel has no rule of
//! its own:
//! - NaN in is NaN out.
//! - An argument outside the real domain (ln of a negative, asin(2)) is NaN,
//!   as sqrt's is in fgdb-gql.
//! - A pole (ln 0) is DivisionByZero, and a finite argument whose result
//!   overflows (exp 710) is Overflow, as the arithmetic kernel reports them.
//! - Infinite arguments follow IEEE 754: exp(-inf) is 0, atan(inf) is pi/2,
//!   sin(inf) is NaN.

use super::{FloatArithmeticError, pack};
use crate::CanonicalF64;
use core::ops::{Add, Div, Mul, Neg, Sub};

type Outcome = Result<CanonicalF64, FloatArithmeticError>;

const NAN_BITS: u64 = 0x7FF8_0000_0000_0000;
const ONE: u64 = 0x3FF0_0000_0000_0000;
const TWO: u64 = 0x4000_0000_0000_0000;
const THREE: u64 = 0x4008_0000_0000_0000;
const HALF: u64 = 0x3FE0_0000_0000_0000;
const NEG_HALF: u64 = 0xBFE0_0000_0000_0000;
const ONE_HALF: u64 = 0x3FF8_0000_0000_0000;
/// msun's 0.33333333333333333 literal (ln's tiny-f branch).
const THIRD: u64 = 0x3FD5_5555_5555_5555;
const TWO54: u64 = 0x4350_0000_0000_0000;
const TWOM1000: u64 = 0x0170_0000_0000_0000;
// exp
const O_THRESHOLD: u64 = 0x4086_2E42_FEFA_39EF;
const U_THRESHOLD: u64 = 0xC087_4910_D52D_3051;
const P1: u64 = 0x3FC5_5555_5555_553E;
const P2: u64 = 0xBF66_C16C_16BE_BD93;
const P3: u64 = 0x3F11_566A_AF25_DE2C;
const P4: u64 = 0xBEBB_BD41_C5D2_6BF1;
const P5: u64 = 0x3E66_3769_72BE_A4D0;
const LN2_HI: u64 = 0x3FE6_2E42_FEE0_0000;
const LN2_LO: u64 = 0x3DEA_39EF_3579_3C76;
const INVLN2: u64 = 0x3FF7_1547_652B_82FE;
// ln, log10
const LG1: u64 = 0x3FE5_5555_5555_5593;
const LG2: u64 = 0x3FD9_9999_9997_FA04;
const LG3: u64 = 0x3FD2_4924_9422_9359;
const LG4: u64 = 0x3FCC_71C5_1D8E_78AF;
const LG5: u64 = 0x3FC7_4664_96CB_03DE;
const LG6: u64 = 0x3FC3_9A09_D078_C69F;
const LG7: u64 = 0x3FC2_F112_DF3E_5244;
const IVLN10HI: u64 = 0x3FDB_CB7B_1520_0000;
const IVLN10LO: u64 = 0x3DBB_9438_CA9A_ADD5;
const LOG10_2HI: u64 = 0x3FD3_4413_509F_6000;
const LOG10_2LO: u64 = 0x3D59_FEF3_11F1_2B36;
// sin, cos, tan
const S1: u64 = 0xBFC5_5555_5555_5549;
const S2: u64 = 0x3F81_1111_1110_F8A6;
const S3: u64 = 0xBF2A_01A0_19C1_61D5;
const S4: u64 = 0x3EC7_1DE3_57B1_FE7D;
const S5: u64 = 0xBE5A_E5E6_8A2B_9CEB;
const S6: u64 = 0x3DE5_D93A_5ACF_D57C;
const C1: u64 = 0x3FA5_5555_5555_554C;
const C2: u64 = 0xBF56_C16C_16C1_5177;
const C3: u64 = 0x3EFA_01A0_19CB_1590;
const C4: u64 = 0xBE92_7E4F_809C_52AD;
const C5: u64 = 0x3E21_EE9E_BDB4_B1C4;
const C6: u64 = 0xBDA8_FAE9_BE88_38D4;
const TAN: [u64; 13] = [
    0x3FD5_5555_5555_5563,
    0x3FC1_1111_1110_FE7A,
    0x3FAB_A1BA_1BB3_41FE,
    0x3F96_64F4_8406_D637,
    0x3F82_26E3_E96E_8493,
    0x3F6D_6D22_C956_0328,
    0x3F57_DBC8_FEE0_8315,
    0x3F43_44D8_F2F2_6501,
    0x3F30_26F7_1A8D_1068,
    0x3F14_7E88_A037_92A6,
    0x3F12_B80F_32F0_A7E9,
    0xBEF3_75CB_DB60_5373,
    0x3EFB_2A70_74BF_7AD4,
];
const PIO4: u64 = 0x3FE9_21FB_5444_2D18;
const PIO4LO: u64 = 0x3C81_A626_3314_5C07;
// atan, atan2, asin, acos
const ATANHI: [u64; 4] = [
    0x3FDD_AC67_0561_BB4F,
    0x3FE9_21FB_5444_2D18,
    0x3FEF_730B_D281_F69B,
    0x3FF9_21FB_5444_2D18,
];
const ATANLO: [u64; 4] = [
    0x3C7A_2B7F_222F_65E2,
    0x3C81_A626_3314_5C07,
    0x3C70_0788_7AF0_CBBD,
    0x3C91_A626_3314_5C07,
];
const ATAN_T: [u64; 11] = [
    0x3FD5_5555_5555_550D,
    0xBFC9_9999_9998_EBC4,
    0x3FC2_4924_9200_83FF,
    0xBFBC_71C6_FE23_1671,
    0x3FB7_45CD_C54C_206E,
    0xBFB3_B0F2_AF74_9A6D,
    0x3FB1_0D66_A0D0_3D51,
    0xBFAD_DE2D_52DE_FD9A,
    0x3FA9_7B4B_2476_0DEB,
    0xBFA2_B444_2C6A_6C2F,
    0x3F90_AD3A_E322_DA11,
];
const PI: u64 = 0x4009_21FB_5444_2D18;
const PI_LO: u64 = 0x3CA1_A626_3314_5C07;
/// pi/2 rounded, msun's pio2_hi (and atan2's pi_o_2).
const PI_O_2: u64 = 0x3FF9_21FB_5444_2D18;
const PIO2_LO: u64 = 0x3C91_A626_3314_5C07;
const PS: [u64; 6] = [
    0x3FC5_5555_5555_5555,
    0xBFD4_D612_03EB_6F7D,
    0x3FC9_C155_0E88_4455,
    0xBFA4_8228_B568_8F3B,
    0x3F49_EFE0_7501_B288,
    0x3F02_3DE1_0DFD_F709,
];
const QS: [u64; 4] = [
    0xC003_3A27_1C8A_2D4B,
    0x4000_2AE5_9C59_8AC8,
    0xBFE6_066C_1B8D_0159,
    0x3FB3_B8C5_B12E_9282,
];
/// Bits 1 through 1280 of the binary fraction of 2/pi, most significant
/// first: the widest window rem_pio2 reads ends at bit 1161.
const TWO_OVER_PI: [u64; 20] = [
    0xA2F9_836E_4E44_1529,
    0xFC27_57D1_F534_DDC0,
    0xDB62_9599_3C43_9041,
    0xFE51_63AB_DEBB_C561,
    0xB724_6E3A_424D_D2E0,
    0x0649_2EEA_09D1_921C,
    0xFE1D_EB1C_B129_A73E,
    0xE882_35F5_2EBB_4484,
    0xE99C_7026_B45F_7E41,
    0x3991_D639_8353_39F4,
    0x9C84_5F8B_BDF9_283B,
    0x1FF8_97FF_DE05_980F,
    0xEF2F_118B_5A0A_6D1F,
    0x6D36_7ECF_27CB_09B7,
    0x4F46_3F66_9E5F_EA2D,
    0x7527_BAC7_EBE5_F17B,
    0x3D07_39F7_8A52_92EA,
    0x6BFB_5FB1_1F8D_5D08,
    0x5603_3046_FC7B_6BAB,
    0xF0CF_BC20_9AF4_361D,
];
/// floor(pi/2 * 2^126).
const PIO2_Q126: u128 = 0x6487_ED51_10B4_611A_6263_3145_C06E_0E68;

impl CanonicalF64 {
    /// e raised to this value.
    pub fn exp(self) -> Result<Self, FloatArithmeticError> {
        exp(self)
    }

    /// The natural logarithm.
    pub fn ln(self) -> Result<Self, FloatArithmeticError> {
        ln(self)
    }

    /// The base-10 logarithm; exact at every power of ten a binary64 holds.
    pub fn log10(self) -> Result<Self, FloatArithmeticError> {
        log10(self)
    }

    /// The sine of this angle in radians.
    pub fn sin(self) -> Result<Self, FloatArithmeticError> {
        sin(self)
    }

    /// The cosine of this angle in radians.
    pub fn cos(self) -> Result<Self, FloatArithmeticError> {
        cos(self)
    }

    /// The tangent of this angle in radians.
    pub fn tan(self) -> Result<Self, FloatArithmeticError> {
        tan(self)
    }

    /// The arc tangent, in [-pi/2, pi/2].
    pub fn atan(self) -> Result<Self, FloatArithmeticError> {
        atan(self)
    }

    /// The arc sine, in [-pi/2, pi/2]; NaN outside [-1, 1].
    pub fn asin(self) -> Result<Self, FloatArithmeticError> {
        asin(self)
    }

    /// The arc cosine, in [0, pi]; NaN outside [-1, 1].
    pub fn acos(self) -> Result<Self, FloatArithmeticError> {
        acos(self)
    }

    /// The angle of the point (x, self) from the positive x axis, in
    /// (-pi, pi]. There is no negative zero, so atan2(0, x) is pi for every
    /// negative x and 0 otherwise.
    pub fn atan2(self, x: Self) -> Result<Self, FloatArithmeticError> {
        atan2(self, x)
    }
}

/// A binary64 intermediate whose first refusal is sticky, so each kernel
/// reads as its formula. No step refuses for an admitted argument; if one
/// ever did, the refusal surfaces from `get` instead of a panic.
#[derive(Clone, Copy)]
struct F(Outcome);

impl F {
    fn bits(bits: u64) -> Self {
        Self(CanonicalF64::from_bits_canonical(bits).ok_or(FloatArithmeticError::NonFinite))
    }
    fn of(value: CanonicalF64) -> Self {
        Self(Ok(value))
    }
    fn int(value: i64) -> Self {
        Self(Ok(CanonicalF64::from_i64_rounded(value)))
    }
    fn get(self) -> Outcome {
        self.0
    }
    /// The value's bits; a refused intermediate reads as NaN, and its
    /// refusal still surfaces from `get`.
    fn to_bits(self) -> u64 {
        self.0.map_or(NAN_BITS, |value| value.to_bits())
    }
    /// The high word without its sign bit, the magnitude msun branches on.
    fn magnitude_high(self) -> u32 {
        (self.to_bits() >> 32) as u32 & 0x7FFF_FFFF
    }
    fn is_negative(self) -> bool {
        self.to_bits() >> 63 == 1
    }
    /// The value with its low word cleared, msun's SET_LOW_WORD(x, 0).
    fn with_low_zero(self) -> Self {
        match self.0 {
            Ok(value) => Self::bits(value.to_bits() & 0xFFFF_FFFF_0000_0000),
            Err(error) => Self(Err(error)),
        }
    }
    fn abs(self) -> Self {
        Self(self.0.and_then(CanonicalF64::checked_abs))
    }
    fn sqrt(self) -> Self {
        Self(self.0.and_then(CanonicalF64::checked_sqrt))
    }
}

fn binary(left: F, right: F, op: fn(CanonicalF64, CanonicalF64) -> Outcome) -> F {
    F(left
        .0
        .and_then(|left| right.0.and_then(|right| op(left, right))))
}
impl Add for F {
    type Output = F;
    fn add(self, right: F) -> F {
        binary(self, right, CanonicalF64::checked_add)
    }
}
impl Sub for F {
    type Output = F;
    fn sub(self, right: F) -> F {
        binary(self, right, CanonicalF64::checked_sub)
    }
}
impl Mul for F {
    type Output = F;
    fn mul(self, right: F) -> F {
        binary(self, right, CanonicalF64::checked_mul)
    }
}
impl Div for F {
    type Output = F;
    fn div(self, right: F) -> F {
        binary(self, right, CanonicalF64::checked_div)
    }
}
impl Neg for F {
    type Output = F;
    fn neg(self) -> F {
        F(self.0.and_then(CanonicalF64::checked_neg))
    }
}

fn c(bits: u64) -> F {
    F::bits(bits)
}
fn zero() -> CanonicalF64 {
    CanonicalF64::from_i64_rounded(0)
}
fn nan() -> CanonicalF64 {
    CanonicalF64::new(f64::NAN)
}

fn exp(x: CanonicalF64) -> Outcome {
    let value = x.get();
    if value.is_nan() || value == f64::INFINITY {
        return Ok(x);
    }
    if value > f64::from_bits(O_THRESHOLD) {
        return Err(FloatArithmeticError::Overflow);
    }
    if value < f64::from_bits(U_THRESHOLD) {
        return Ok(zero());
    }
    let x = F::of(x);
    let ix = x.magnitude_high();
    let negative = x.is_negative();
    let (mut r, mut k, mut hi, mut lo) = (x, 0_i64, F::int(0), F::int(0));
    if ix > 0x3FD6_2E42 {
        // |x| > ln2 / 2: x = k ln2 + r, |r| <= ln2 / 2.
        if ix < 0x3FF0_A2B2 {
            (hi, lo, k) = if negative {
                (x + c(LN2_HI), -c(LN2_LO), -1)
            } else {
                (x - c(LN2_HI), c(LN2_LO), 1)
            };
        } else {
            let half = c(if negative { NEG_HALF } else { HALF });
            k = (c(INVLN2) * x + half).get()?.checked_to_i64_truncated()?;
            let t = F::int(k);
            hi = x - t * c(LN2_HI);
            lo = t * c(LN2_LO);
        }
        r = hi - lo;
    } else if ix < 0x3E30_0000 {
        return (c(ONE) + x).get();
    }
    let t = r * r;
    let poly = r - t * (c(P1) + t * (c(P2) + t * (c(P3) + t * (c(P4) + t * c(P5)))));
    if k == 0 {
        return (c(ONE) - ((r * poly) / (poly - c(TWO)) - r)).get();
    }
    let y = (c(ONE) - ((lo - (r * poly) / (c(TWO) - poly)) - hi)).get()?;
    if k >= -1021 {
        scale(y, k)
    } else {
        scale(y, k + 1000)?.checked_mul(c(TWOM1000).get()?)
    }
}

/// y times 2^k by an exact exponent addition, for a normal y and result.
fn scale(y: CanonicalF64, k: i64) -> Outcome {
    let bits = y.to_bits().wrapping_add((k << 52) as u64);
    match CanonicalF64::from_bits_canonical(bits) {
        Some(value) if value.get().is_finite() => Ok(value),
        _ => Err(FloatArithmeticError::Overflow),
    }
}

/// (k, f, hx) with x = 2^k (1 + f), sqrt(2)/2 < 1 + f < sqrt(2), for a finite
/// positive x; hx is the high fraction word msun's ln branches read.
fn log_reduce(x: CanonicalF64) -> (i64, F, u32) {
    let mut x = F::of(x);
    let mut k = 0_i64;
    let mut hx = (x.to_bits() >> 32) as u32;
    if hx < 0x0010_0000 {
        // Subnormal: scale into the normal range first.
        k -= 54;
        x = x * c(TWO54);
        hx = (x.to_bits() >> 32) as u32;
    }
    k += i64::from(hx >> 20) - 1023;
    hx &= 0x000F_FFFF;
    let i = (hx + 0x95F64) & 0x10_0000;
    let normalized =
        F::bits((u64::from(hx | (i ^ 0x3FF0_0000)) << 32) | (x.to_bits() & 0xFFFF_FFFF));
    k += i64::from(i >> 20);
    (k, normalized - c(ONE), hx)
}

/// The domain checks ln and log10 share: Some(result) settles the argument.
fn log_domain(x: CanonicalF64) -> Option<Outcome> {
    let value = x.get();
    if value.is_nan() || value == f64::INFINITY {
        Some(Ok(x))
    } else if value < 0.0 {
        Some(Ok(nan()))
    } else if value == 0.0 {
        Some(Err(FloatArithmeticError::DivisionByZero))
    } else {
        None
    }
}

fn ln(x: CanonicalF64) -> Outcome {
    if let Some(settled) = log_domain(x) {
        return settled;
    }
    let (k, f, hx) = log_reduce(x);
    let dk = F::int(k);
    let (ln2_hi, ln2_lo) = (c(LN2_HI), c(LN2_LO));
    if (0x000F_FFFF & (2 + hx)) < 3 {
        // -2^-20 <= f < 2^-20
        if f.to_bits() == 0 {
            return if k == 0 {
                Ok(zero())
            } else {
                (dk * ln2_hi + dk * ln2_lo).get()
            };
        }
        let r = f * f * (c(HALF) - c(THIRD) * f);
        return if k == 0 {
            (f - r).get()
        } else {
            (dk * ln2_hi - ((r - dk * ln2_lo) - f)).get()
        };
    }
    let s = f / (c(TWO) + f);
    let z = s * s;
    let w = z * z;
    let t1 = w * (c(LG2) + w * (c(LG4) + w * c(LG6)));
    let t2 = z * (c(LG1) + w * (c(LG3) + w * (c(LG5) + w * c(LG7))));
    let r = t2 + t1;
    let (i, j) = (hx as i32 - 0x6147A, 0x6B851 - hx as i32);
    if (i | j) > 0 {
        let hfsq = c(HALF) * f * f;
        if k == 0 {
            (f - (hfsq - s * (hfsq + r))).get()
        } else {
            (dk * ln2_hi - ((hfsq - (s * (hfsq + r) + dk * ln2_lo)) - f)).get()
        }
    } else if k == 0 {
        (f - s * (f - r)).get()
    } else {
        (dk * ln2_hi - ((s * (f - r) - dk * ln2_lo) - f)).get()
    }
}

/// msun's k_log1p: ln(1 + f) - f + f^2/2 for |f| < sqrt(2) - 1.
fn k_log1p(f: F) -> F {
    let s = f / (c(TWO) + f);
    let z = s * s;
    let w = z * z;
    let t1 = w * (c(LG2) + w * (c(LG4) + w * c(LG6)));
    let t2 = z * (c(LG1) + w * (c(LG3) + w * (c(LG5) + w * c(LG7))));
    let hfsq = c(HALF) * f * f;
    s * (hfsq + (t2 + t1))
}

fn log10(x: CanonicalF64) -> Outcome {
    if let Some(settled) = log_domain(x) {
        return settled;
    }
    if x.to_bits() == ONE {
        return Ok(zero());
    }
    let (k, f, _) = log_reduce(x);
    let y = F::int(k);
    let hfsq = c(HALF) * f * f;
    let r = k_log1p(f);
    let hi = (f - hfsq).with_low_zero();
    let lo = (f - hi) - hfsq + r;
    let val_hi = hi * c(IVLN10HI);
    let y2 = y * c(LOG10_2HI);
    let val_lo = y * c(LOG10_2LO) + (lo + hi) * c(IVLN10LO) + lo * c(IVLN10HI);
    let w = y2 + val_hi;
    let val_lo = val_lo + ((y2 - w) + val_hi);
    (val_lo + w).get()
}

/// A 256-bit unsigned integer, least significant limb first.
type U256 = [u64; 4];

/// 64 bits of 2/pi starting at fraction bit `first` (1-based).
fn two_over_pi_chunk(first: u32) -> u64 {
    let index = (first - 1) as usize;
    let (word, offset) = (index / 64, index % 64);
    let at = |i: usize| TWO_OVER_PI.get(i).copied().unwrap_or(0);
    if offset == 0 {
        at(word)
    } else {
        (at(word) << offset) | (at(word + 1) >> (64 - offset))
    }
}

/// Bits `offset` through `offset + 127` of `value`.
fn extract(value: &U256, offset: u32) -> u128 {
    let limb = |i: usize| value.get(i).copied().unwrap_or(0);
    let (word, shift) = ((offset / 64) as usize, offset % 64);
    let piece = |i: usize| {
        if shift == 0 {
            limb(i)
        } else {
            (limb(i) >> shift) | (limb(i + 1) << (64 - shift))
        }
    };
    (u128::from(piece(word + 1)) << 64) | u128::from(piece(word))
}

fn bit_length(value: &U256) -> u32 {
    (0..4)
        .rev()
        .find(|&i| value[i] != 0)
        .map_or(0, |i| i as u32 * 64 + 64 - value[i].leading_zeros())
}

fn bit(value: &U256, index: u32) -> bool {
    value[(index / 64) as usize] >> (index % 64) & 1 == 1
}

/// Whether any bit below `index` is set.
fn any_below(value: &U256, index: u32) -> bool {
    let (word, shift) = ((index / 64) as usize, index % 64);
    value[..word].iter().any(|&limb| limb != 0)
        || (shift != 0 && value[word] & ((1_u64 << shift) - 1) != 0)
}

/// `value` modulo 2^count.
fn low_bits(value: &U256, count: u32) -> U256 {
    let mut out = *value;
    for (i, limb) in out.iter_mut().enumerate() {
        let start = i as u32 * 64;
        if start >= count {
            *limb = 0;
        } else if count - start < 64 {
            *limb &= (1_u64 << (count - start)) - 1;
        }
    }
    out
}

/// 2^count minus a nonzero `value` below 2^count.
fn complement(value: &U256, count: u32) -> U256 {
    let mut out = [0_u64; 4];
    let mut carry = 1_u64;
    for (slot, &limb) in out.iter_mut().zip(value) {
        let (sum, overflow) = (!limb).overflowing_add(carry);
        *slot = sum;
        carry = u64::from(overflow);
    }
    low_bits(&out, count)
}

fn mul_128(a: u128, b: u128) -> U256 {
    let (a0, a1) = (u128::from(a as u64), a >> 64);
    let (b0, b1) = (u128::from(b as u64), b >> 64);
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    let mid = (p00 >> 64) + u128::from(p01 as u64) + u128::from(p10 as u64);
    // At most 2^128 - 1: the whole product is below 2^256.
    let high = (mid >> 64) + (p01 >> 64) + (p10 >> 64) + p11;
    [p00 as u64, mid as u64, high as u64, (high >> 64) as u64]
}

/// `value` times 2^exponent rounded to nearest even, for any 256-bit value.
fn round_wide(value: &U256, negative: bool, exponent: i32) -> Outcome {
    let width = bit_length(value);
    if width <= 128 {
        return pack(negative, extract(value, 0), exponent);
    }
    // 128 leading bits with the rest folded into a sticky low bit: rounding
    // to 53 bits then sees exactly what the full value would show.
    let shift = width - 128;
    let sticky = u128::from(any_below(value, shift));
    pack(
        negative,
        extract(value, shift) | sticky,
        exponent + shift as i32,
    )
}

/// x - n pi/2 = y0 + y1, |y0 + y1| <= pi/4, for a finite |x| > pi/4: an exact
/// integer Payne-Hanek reduction. Returns n mod 4; y0 is the reduced value
/// rounded to nearest, y1 the exact remainder rounded the same way.
fn rem_pio2(x: CanonicalF64) -> Result<(u32, F, F), FloatArithmeticError> {
    let bits = x.to_bits();
    let negative = bits >> 63 == 1;
    // |x| > pi/4 is normal: x = m 2^e with a 53-bit m.
    let m = (bits & ((1 << 52) - 1)) | (1 << 52);
    let e = ((bits >> 52) & 0x7FF) as i32 - 1075;
    // Fraction bits before `first` contribute multiples of 4 (whole turns).
    let first = (e - 1).max(1) as u32;
    let window = [
        two_over_pi_chunk(first + 128),
        two_over_pi_chunk(first + 64),
        two_over_pi_chunk(first),
    ];
    let mut product = [0_u64; 4];
    let mut carry = 0_u128;
    for (limb, word) in product.iter_mut().zip(window) {
        let t = u128::from(m) * u128::from(word) + carry;
        *limb = t as u64;
        carry = t >> 64;
    }
    product[3] = carry as u64;
    // x 2/pi = product 2^-shift; shift is at least 190.
    let shift = (first as i32 + 191 - e) as u32;
    let mut quadrant = (extract(&product, shift) & 3) as u32;
    let mut fraction = extract(&product, shift - 128);
    let mut reduced_negative = false;
    if fraction >> 127 == 1 {
        // A fraction of a half or more rounds n up and leaves r negative.
        quadrant = (quadrant + 1) & 3;
        fraction = fraction.wrapping_neg();
        reduced_negative = true;
    }
    let negative_result = reduced_negative != negative;
    if negative {
        quadrant = (4 - quadrant) & 3;
    }
    if fraction == 0 {
        return Ok((quadrant, F::int(0), F::int(0)));
    }
    // r = fraction 2^-128 times pi/2 = scaled 2^-254.
    let scaled = mul_128(fraction, PIO2_Q126);
    let width = bit_length(&scaled);
    let drop = width - 53;
    let q = extract(&scaled, drop);
    let up = bit(&scaled, drop - 1) && (any_below(&scaled, drop - 1) || q & 1 == 1);
    let y0 = pack(negative_result, q + u128::from(up), drop as i32 - 254)?;
    let rest = low_bits(&scaled, drop);
    let (rest, rest_negative) = if up {
        (complement(&rest, drop), !negative_result)
    } else {
        (rest, negative_result)
    };
    let y1 = round_wide(&rest, rest_negative, -254)?;
    Ok((quadrant, F::of(y0), F::of(y1)))
}

/// msun's __kernel_sin on [-pi/4, pi/4]; `tail` says y is a live remainder.
fn k_sin(x: F, y: F, tail: bool) -> F {
    let z = x * x;
    let w = z * z;
    let r = c(S2) + z * (c(S3) + z * c(S4)) + z * w * (c(S5) + z * c(S6));
    let v = z * x;
    if tail {
        x - ((z * (c(HALF) * y - v * r) - y) - v * c(S1))
    } else {
        x + v * (c(S1) + z * r)
    }
}

/// msun's __kernel_cos on [-pi/4, pi/4].
fn k_cos(x: F, y: F) -> F {
    let z = x * x;
    let w = z * z;
    let r = z * (c(C1) + z * (c(C2) + z * c(C3))) + w * w * (c(C4) + z * (c(C5) + z * c(C6)));
    let hz = c(HALF) * z;
    let w = c(ONE) - hz;
    w + (((c(ONE) - w) - hz) + (z * r - x * y))
}

/// msun's __kernel_tan on [-pi/4, pi/4]: tan(x + y), or -1/tan(x + y) when
/// `odd` (an odd quadrant).
fn k_tan(x: F, y: F, odd: bool) -> F {
    let t = |i: usize| c(TAN[i]);
    let negative = x.is_negative();
    let big = x.magnitude_high() >= 0x3FE5_9428;
    let (mut x, mut y) = (x, y);
    if big {
        // |x| >= 0.6744: tan(pi/4 - x) is better conditioned.
        if negative {
            (x, y) = (-x, -y);
        }
        x = (c(PIO4) - x) + (c(PIO4LO) - y);
        y = F::int(0);
    }
    let z = x * x;
    let w = z * z;
    let r = t(1) + w * (t(3) + w * (t(5) + w * (t(7) + w * (t(9) + w * t(11)))));
    let v = z * (t(2) + w * (t(4) + w * (t(6) + w * (t(8) + w * (t(10) + w * t(12))))));
    let s = z * x;
    let r = y + z * (s * (r + v) + y);
    let r = r + t(0) * s;
    let w = x + r;
    if big {
        let v = F::int(if odd { -1 } else { 1 });
        let result = v - c(TWO) * (x - (w * w / (w + v) - r));
        return if negative { -result } else { result };
    }
    if !odd {
        return w;
    }
    // -1/(x + r), accurately: z + v = x + r with z's low word cleared.
    let z = w.with_low_zero();
    let v = r - (z - x);
    let a = -c(ONE) / w;
    let t = a.with_low_zero();
    let s = c(ONE) + t * z;
    t + a * (s + t * v)
}

fn sin(x: CanonicalF64) -> Outcome {
    if !x.get().is_finite() {
        return Ok(nan());
    }
    let ix = F::of(x).magnitude_high();
    if ix <= 0x3FE9_21FB {
        if ix < 0x3E50_0000 {
            return Ok(x);
        }
        return k_sin(F::of(x), F::int(0), false).get();
    }
    let (n, y0, y1) = rem_pio2(x)?;
    match n {
        0 => k_sin(y0, y1, true),
        1 => k_cos(y0, y1),
        2 => -k_sin(y0, y1, true),
        _ => -k_cos(y0, y1),
    }
    .get()
}

fn cos(x: CanonicalF64) -> Outcome {
    if !x.get().is_finite() {
        return Ok(nan());
    }
    let ix = F::of(x).magnitude_high();
    if ix <= 0x3FE9_21FB {
        if ix < 0x3E46_A09E {
            return c(ONE).get();
        }
        return k_cos(F::of(x), F::int(0)).get();
    }
    let (n, y0, y1) = rem_pio2(x)?;
    match n {
        0 => k_cos(y0, y1),
        1 => -k_sin(y0, y1, true),
        2 => -k_cos(y0, y1),
        _ => k_sin(y0, y1, true),
    }
    .get()
}

fn tan(x: CanonicalF64) -> Outcome {
    if !x.get().is_finite() {
        return Ok(nan());
    }
    let ix = F::of(x).magnitude_high();
    if ix <= 0x3FE9_21FB {
        if ix < 0x3E40_0000 {
            return Ok(x);
        }
        return k_tan(F::of(x), F::int(0), false).get();
    }
    let (n, y0, y1) = rem_pio2(x)?;
    k_tan(y0, y1, n & 1 == 1).get()
}

fn atan(x: CanonicalF64) -> Outcome {
    if x.get().is_nan() {
        return Ok(x);
    }
    let x = F::of(x);
    let ix = x.magnitude_high();
    let negative = x.is_negative();
    let signed = |z: F| if negative { -z } else { z };
    if ix >= 0x4410_0000 {
        // |x| >= 2^66, infinities included.
        return signed(c(ATANHI[3]) + c(ATANLO[3])).get();
    }
    let (reduced, id) = if ix < 0x3FDC_0000 {
        if ix < 0x3E40_0000 {
            return x.get();
        }
        (x, None)
    } else {
        let a = x.abs();
        if ix < 0x3FE6_0000 {
            ((c(TWO) * a - c(ONE)) / (c(TWO) + a), Some(0))
        } else if ix < 0x3FF3_0000 {
            ((a - c(ONE)) / (a + c(ONE)), Some(1))
        } else if ix < 0x4003_8000 {
            ((a - c(ONE_HALF)) / (c(ONE) + c(ONE_HALF) * a), Some(2))
        } else {
            (-c(ONE) / a, Some(3))
        }
    };
    let at = |i: usize| c(ATAN_T[i]);
    let z = reduced * reduced;
    let w = z * z;
    let s1 = z * (at(0) + w * (at(2) + w * (at(4) + w * (at(6) + w * (at(8) + w * at(10))))));
    let s2 = w * (at(1) + w * (at(3) + w * (at(5) + w * (at(7) + w * at(9)))));
    match id {
        None => (reduced - reduced * (s1 + s2)).get(),
        Some(id) => signed(c(ATANHI[id]) - ((reduced * (s1 + s2) - c(ATANLO[id])) - reduced)).get(),
    }
}

fn atan2(y: CanonicalF64, x: CanonicalF64) -> Outcome {
    let (yv, xv) = (y.get(), x.get());
    if yv.is_nan() || xv.is_nan() {
        return Ok(nan());
    }
    if x.to_bits() == ONE {
        return atan(y);
    }
    // Bit 0: y is negative; bit 1: x is negative.
    let m = u8::from(yv < 0.0) | (u8::from(xv < 0.0) << 1);
    let quarter = c(PI_O_2);
    if yv == 0.0 {
        return if m & 2 != 0 { c(PI).get() } else { Ok(zero()) };
    }
    if xv == 0.0 || (yv.is_infinite() && !xv.is_infinite()) {
        return if yv < 0.0 { -quarter } else { quarter }.get();
    }
    if xv.is_infinite() {
        let three_quarters = c(THREE) * c(PIO4);
        return if yv.is_infinite() {
            match m {
                0 => c(PIO4),
                1 => -c(PIO4),
                2 => three_quarters,
                _ => -three_quarters,
            }
        } else {
            match m {
                0 | 1 => F::int(0),
                2 => c(PI),
                _ => -c(PI),
            }
        }
        .get();
    }
    let k = (F::of(y).magnitude_high() as i32 - F::of(x).magnitude_high() as i32) >> 20;
    let (z, m) = if k > 60 {
        // |y/x| > 2^60
        (quarter + c(HALF) * c(PI_LO), m & 1)
    } else if xv < 0.0 && k < -60 {
        // 0 > |y|/x > -2^-60
        (F::int(0), m)
    } else {
        (F::of(atan((F::of(y) / F::of(x)).abs().get()?)?), m)
    };
    match m {
        0 => z,
        1 => -z,
        2 => c(PI) - (z - c(PI_LO)),
        _ => (z - c(PI_LO)) - c(PI),
    }
    .get()
}

fn asin_ratio(t: F) -> (F, F) {
    let p = |i: usize| c(PS[i]);
    let q = |i: usize| c(QS[i]);
    let numerator = t * (p(0) + t * (p(1) + t * (p(2) + t * (p(3) + t * (p(4) + t * p(5))))));
    let denominator = c(ONE) + t * (q(0) + t * (q(1) + t * (q(2) + t * q(3))));
    (numerator, denominator)
}

fn asin(x: CanonicalF64) -> Outcome {
    let value = x.get();
    if value.is_nan() {
        return Ok(x);
    }
    let x = F::of(x);
    let ix = x.magnitude_high();
    if ix >= 0x3FF0_0000 {
        if value == 1.0 || value == -1.0 {
            return (x * c(PI_O_2) + x * c(PIO2_LO)).get();
        }
        return Ok(nan());
    }
    if ix < 0x3FE0_0000 {
        if ix < 0x3E50_0000 {
            return x.get();
        }
        let (p, q) = asin_ratio(x * x);
        return (x + x * (p / q)).get();
    }
    let t = (c(ONE) - x.abs()) * c(HALF);
    let (p, q) = asin_ratio(t);
    let s = t.sqrt();
    let t = if ix >= 0x3FEF_3333 {
        // |x| > 0.975
        c(PI_O_2) - (c(TWO) * (s + s * (p / q)) - c(PIO2_LO))
    } else {
        let w = s.with_low_zero();
        let correction = (t - w * w) / (s + w);
        let p = c(TWO) * s * (p / q) - (c(PIO2_LO) - c(TWO) * correction);
        let q = c(PIO4) - c(TWO) * w;
        c(PIO4) - (p - q)
    };
    if value > 0.0 { t } else { -t }.get()
}

fn acos(x: CanonicalF64) -> Outcome {
    let value = x.get();
    if value.is_nan() {
        return Ok(x);
    }
    let x = F::of(x);
    let ix = x.magnitude_high();
    if ix >= 0x3FF0_0000 {
        if value == 1.0 {
            return Ok(zero());
        }
        if value == -1.0 {
            return (c(PI) + c(TWO) * c(PIO2_LO)).get();
        }
        return Ok(nan());
    }
    if ix < 0x3FE0_0000 {
        if ix <= 0x3C60_0000 {
            return (c(PI_O_2) + c(PIO2_LO)).get();
        }
        let (p, q) = asin_ratio(x * x);
        return (c(PI_O_2) - (x - (c(PIO2_LO) - x * (p / q)))).get();
    }
    if value < 0.0 {
        let z = (c(ONE) + x) * c(HALF);
        let (p, q) = asin_ratio(z);
        let s = z.sqrt();
        let w = (p / q) * s - c(PIO2_LO);
        return (c(PI) - c(TWO) * (s + w)).get();
    }
    let z = (c(ONE) - x) * c(HALF);
    let s = z.sqrt();
    let df = s.with_low_zero();
    let correction = (z - df * df) / (s + df);
    let (p, q) = asin_ratio(z);
    let w = (p / q) * s + correction;
    (c(TWO) * (df + w)).get()
}

#[cfg(test)]
mod tests;
