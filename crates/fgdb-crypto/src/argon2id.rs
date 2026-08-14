//! Argon2id (RFC 9106): the memory-hard passphrase KDF at the head of the
//! at-rest key chain.
//!
//! README `at_rest_encryption`: `Argon2id → KEK → per-DB DEK; XChaCha20-Poly1305,
//! encrypt-then-code`. This is the first link. Everything downstream of the KEK
//! is a fast KDF or an AEAD; this is the only place where a *human-chosen*
//! secret enters, so it is the only place that has to be expensive on purpose.
//!
//! **WHY ARGON2id AND NOT ARGON2i OR ARGON2d.** The `id` variant runs the first
//! half of the first pass with data-INDEPENDENT addressing and everything after
//! with data-DEPENDENT addressing (RFC 9106 §3.4). That split is the entire
//! point: data-independent addressing resists the side-channel attack that
//! breaks Argon2d for passphrases, and data-dependent addressing resists the
//! time-memory tradeoff that weakens Argon2i. Implementing only one half of the
//! split would silently produce the variant we did not choose, which is why
//! [`Variant`] exists and why the addressing rule is asserted by its own test
//! rather than left implicit in the loop.
//!
//! **CLOSED UNIVERSE** (doctrine #1). Built on this crate's own
//! [`crate::blake2b`], because Argon2 is *defined* in terms of BLAKE2b — the
//! variable-length hash `H'` and the compression function `G` are both derived
//! from it. Golden vectors come from a scratchpad-only oracle that never enters
//! the dependency graph.
//!
//! **NOT A PASSWORD-HASH STRING FORMAT.** This produces raw tag bytes. There is
//! no PHC-string encoder here, and deliberately so: the encoded form carries
//! parameters as untrusted input, and every parameter this crate accepts is
//! chosen by a registered profile, never parsed from a stored string.

use crate::blake2b::{Blake2b, blake2b};
use crate::zeroize::Secret;

/// Bytes per Argon2 block (RFC 9106 §3.1: the memory is addressed in 1 KiB
/// blocks, and `G` is defined over exactly two of them).
const BLOCK_BYTES: usize = 1024;
/// 1 KiB expressed as 64-bit words, which is how `G` actually manipulates it.
const BLOCK_WORDS: usize = BLOCK_BYTES / 8;
/// RFC 9106 §3.1: every lane is divided into exactly four slices, and the slice
/// boundary is where lanes synchronize.
const SYNC_POINTS: u32 = 4;
/// The Argon2 version this implements (0x13 — the current one; 0x10 is the
/// pre-2016 construction and is NOT accepted, because silently computing the
/// old one would produce a wrong key that looks fine).
pub const VERSION: u32 = 0x13;

/// Durable numeric ID for the V1 passphrase-KDF profile.
///
/// This is a FrankenGraphDB profile choice, not a claim that an arbitrary
/// Argon2 parameter tuple is interchangeable with it.  Recovery resolves the
/// ID through [`registered_passphrase_kdf_profile`] and obtains every cost and
/// width from that closed row.
pub const PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND: u16 = 1;

/// The exact salt width registered by the V1 passphrase-KDF profile.
pub const PASSPHRASE_KDF_SALT_BYTES: usize = 16;

/// The exact KEK width produced by the V1 passphrase-KDF profile.
pub const PASSPHRASE_KEK_BYTES: usize = 32;

/// Which Argon2 addressing discipline to use.
///
/// Present as a type rather than a constant because the three variants differ
/// *only* in the addressing rule, so a single boolean buried in the fill loop
/// would make "which KDF did we actually run" unanswerable at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Data-dependent addressing throughout. Fast, but side-channel exposed —
    /// never correct for a passphrase.
    Argon2d,
    /// Data-independent addressing throughout.
    Argon2i,
    /// Data-independent for the first half of the first pass, data-dependent
    /// afterwards. The passphrase choice, and this crate's at-rest profile.
    Argon2id,
}

impl Variant {
    /// The `y` field of `H0` (RFC 9106 §3.2).
    const fn type_code(self) -> u32 {
        match self {
            Self::Argon2d => 0,
            Self::Argon2i => 1,
            Self::Argon2id => 2,
        }
    }
}

/// Why a KDF invocation was refused.
///
/// Every arm is a parameter the RFC bounds. They are refused rather than
/// clamped: a caller who asked for one memory cost and silently received
/// another has no way to reproduce the key later, and an unreproducible KEK is
/// indistinguishable from a lost database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Argon2Error {
    /// Parallelism was zero or above 2^24 - 1.
    Lanes { requested: u32 },
    /// Passes was zero.
    Passes { requested: u32 },
    /// Memory was below the `8 * lanes` floor RFC 9106 §3.1 sets.
    MemoryTooSmall { requested: u32, minimum: u32 },
    /// Tag length was below 4 bytes.
    TagTooShort { requested: usize },
    /// Salt was below the 8-byte floor.
    SaltTooShort { requested: usize },
}

impl core::fmt::Display for Argon2Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Lanes { requested } => {
                write!(f, "Argon2 parallelism must be 1..=2^24-1, got {requested}")
            }
            Self::Passes { requested } => {
                write!(f, "Argon2 passes must be at least 1, got {requested}")
            }
            Self::MemoryTooSmall { requested, minimum } => write!(
                f,
                "Argon2 memory {requested} KiB is below the 8*lanes floor of {minimum} KiB"
            ),
            Self::TagTooShort { requested } => {
                write!(f, "Argon2 tag must be at least 4 bytes, got {requested}")
            }
            Self::SaltTooShort { requested } => {
                write!(f, "Argon2 salt must be at least 8 bytes, got {requested}")
            }
        }
    }
}

impl core::error::Error for Argon2Error {}

/// The cost parameters of one Argon2 invocation.
///
/// These are exactly the values that must be stored beside a wrapped key to
/// re-derive it, which is why they are one struct: a KEK derived under
/// parameters nobody recorded cannot be reproduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Memory cost in KiB (`m`).
    pub memory_kib: u32,
    /// Number of passes over memory (`t`).
    pub passes: u32,
    /// Degree of parallelism (`p`).
    pub lanes: u32,
}

/// One complete row in the closed passphrase-KDF profile registry.
///
/// The row deliberately carries the full tuple rather than merely naming
/// "Argon2id".  Changing any field changes the derived KEK, so a partial row is
/// not algorithm agility; it is an unrecoverable database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassphraseKdfProfileSpec {
    /// Durable numeric profile identifier.
    pub profile_id: u16,
    /// Argon2 format version.  V1 accepts only RFC 9106 version 0x13.
    pub argon2_version: u32,
    /// Addressing variant.  A passphrase profile must be Argon2id.
    pub variant: Variant,
    /// Exact memory, pass, and lane costs.
    pub params: Params,
    /// Exact salt width in bytes.
    pub salt_len: u16,
    /// Exact derived KEK width in bytes.
    pub kek_len: u16,
}

/// Closed V1 passphrase-KDF profile vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassphraseKdfProfile {
    /// RFC 9106's second recommended option: Argon2id v0x13, 64 MiB, three
    /// passes, four lanes, a 128-bit salt, and a 256-bit derived key.
    ///
    /// The first recommended option uses 2 GiB per invocation.  V1 chooses the
    /// RFC's explicitly memory-constrained option so database open remains
    /// operationally bounded on embedded postures.  This is a fixed profile,
    /// not runtime auto-tuning; a future tuple receives another numeric ID.
    Argon2idRfc9106SecondV1,
}

impl PassphraseKdfProfile {
    /// The complete immutable registry row selected by this profile.
    pub const fn spec(self) -> PassphraseKdfProfileSpec {
        match self {
            Self::Argon2idRfc9106SecondV1 => PassphraseKdfProfileSpec {
                profile_id: PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND,
                argon2_version: VERSION,
                variant: Variant::Argon2id,
                params: Params {
                    memory_kib: 65_536,
                    passes: 3,
                    lanes: 4,
                },
                salt_len: PASSPHRASE_KDF_SALT_BYTES as u16,
                kek_len: PASSPHRASE_KEK_BYTES as u16,
            },
        }
    }

    /// Durable numeric identifier.
    pub const fn id(self) -> u16 {
        self.spec().profile_id
    }
}

/// Resolve a durable numeric profile ID through the closed V1 registry.
pub const fn registered_passphrase_kdf_profile(id: u16) -> Option<PassphraseKdfProfile> {
    match id {
        PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND => {
            Some(PassphraseKdfProfile::Argon2idRfc9106SecondV1)
        }
        _ => None,
    }
}

/// Why profile-bound passphrase derivation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassphraseKdfError {
    /// The durable numeric profile ID is not in the closed registry.
    UnsupportedProfile { profile_id: u16 },
    /// The supplied salt width disagrees with the selected profile.
    SaltLength {
        profile_id: u16,
        expected: u16,
        actual: usize,
    },
    /// The registered tuple was rejected by the Argon2 primitive.
    Primitive(Argon2Error),
}

impl core::fmt::Display for PassphraseKdfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedProfile { profile_id } => {
                write!(f, "unsupported passphrase-KDF profile {profile_id}")
            }
            Self::SaltLength {
                profile_id,
                expected,
                actual,
            } => write!(
                f,
                "passphrase-KDF profile {profile_id} requires salt length {expected}, not {actual}"
            ),
            Self::Primitive(error) => write!(f, "registered passphrase KDF failed: {error}"),
        }
    }
}

impl core::error::Error for PassphraseKdfError {}

impl From<Argon2Error> for PassphraseKdfError {
    fn from(error: Argon2Error) -> Self {
        Self::Primitive(error)
    }
}

/// Derive a profile-bound 256-bit KEK from a passphrase.
///
/// The caller supplies only the numeric profile ID, passphrase, and salt.  It
/// cannot weaken the memory cost, pass count, lane count, variant, version, or
/// output width.  The result uses [`Secret`] so the owned KEK is scrubbed on
/// drop within the safe-code guarantee documented by that type.
pub fn derive_passphrase_kek(
    profile_id: u16,
    passphrase: &[u8],
    salt: &[u8],
) -> Result<Secret<PASSPHRASE_KEK_BYTES>, PassphraseKdfError> {
    let profile = registered_passphrase_kdf_profile(profile_id)
        .ok_or(PassphraseKdfError::UnsupportedProfile { profile_id })?;
    let spec = profile.spec();
    if salt.len() != usize::from(spec.salt_len) {
        return Err(PassphraseKdfError::SaltLength {
            profile_id,
            expected: spec.salt_len,
            actual: salt.len(),
        });
    }

    let mut kek = Secret::zeroed();
    hash_into(
        spec.variant,
        spec.params,
        passphrase,
        salt,
        kek.expose_mut(),
    )?;
    Ok(kek)
}

/// A 1 KiB Argon2 block.
#[derive(Clone, Copy)]
struct Block([u64; BLOCK_WORDS]);

impl Block {
    const fn zero() -> Self {
        Block([0u64; BLOCK_WORDS])
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let mut block = Block::zero();
        let (words, _) = bytes.as_chunks::<8>();
        for (slot, word) in block.0.iter_mut().zip(words) {
            *slot = u64::from_le_bytes(*word);
        }
        block
    }

    fn to_bytes(self) -> [u8; BLOCK_BYTES] {
        let mut out = [0u8; BLOCK_BYTES];
        let (chunks, _) = out.as_chunks_mut::<8>();
        for (chunk, word) in chunks.iter_mut().zip(self.0.iter()) {
            *chunk = word.to_le_bytes();
        }
        out
    }

    fn xor(&self, other: &Block) -> Block {
        let mut out = Block::zero();
        for i in 0..BLOCK_WORDS {
            out.0[i] = self.0[i] ^ other.0[i];
        }
        out
    }
}

/// Argon2's `GB` (RFC 9106 §3.5).
///
/// **NOT BLAKE2b's `G`, despite the shape.** The additions carry an extra
/// `2 * trunc(a) * trunc(b)` term over the low 32 bits. That multiplication is
/// what makes the compression resistant to cheap hardware evaluation, so
/// dropping it produces a function that still mixes, still passes casual
/// round-trip checks, and is not Argon2.
#[inline(always)]
fn gb(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize) {
    let mul = |x: u64, y: u64| -> u64 {
        let lo = |z: u64| z & 0xffff_ffff;
        2u64.wrapping_mul(lo(x)).wrapping_mul(lo(y))
    };
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(mul(v[a], v[b]));
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]).wrapping_add(mul(v[c], v[d]));
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(mul(v[a], v[b]));
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]).wrapping_add(mul(v[c], v[d]));
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// The permutation `P` over sixteen 64-bit words (RFC 9106 §3.5) — the BLAKE2b
/// round structure with `GB` and no message words.
#[inline(always)]
fn permute(v: &mut [u64; 16]) {
    gb(v, 0, 4, 8, 12);
    gb(v, 1, 5, 9, 13);
    gb(v, 2, 6, 10, 14);
    gb(v, 3, 7, 11, 15);
    gb(v, 0, 5, 10, 15);
    gb(v, 1, 6, 11, 12);
    gb(v, 2, 7, 8, 13);
    gb(v, 3, 4, 9, 14);
}

/// The compression function `G(X, Y)` (RFC 9106 §3.5): `P` over every row, then
/// over every column, of `R = X XOR Y`, finally XORed back with `R`.
fn compress(x: &Block, y: &Block) -> Block {
    let r = x.xor(y);
    let mut q = r;

    // Rows: eight groups of sixteen consecutive words.
    for row in 0..8 {
        let mut v = [0u64; 16];
        v.copy_from_slice(&q.0[row * 16..row * 16 + 16]);
        permute(&mut v);
        q.0[row * 16..row * 16 + 16].copy_from_slice(&v);
    }

    // Columns: eight groups of sixteen words strided across the rows, taken two
    // words at a time — the interleaving is part of the specification, not an
    // implementation convenience.
    for col in 0..8 {
        let mut v = [0u64; 16];
        for k in 0..8 {
            v[2 * k] = q.0[16 * k + 2 * col];
            v[2 * k + 1] = q.0[16 * k + 2 * col + 1];
        }
        permute(&mut v);
        for k in 0..8 {
            q.0[16 * k + 2 * col] = v[2 * k];
            q.0[16 * k + 2 * col + 1] = v[2 * k + 1];
        }
    }

    q.xor(&r)
}

/// `H'^T(X)` — Argon2's variable-length hash (RFC 9106 §3.3).
///
/// Not simply "BLAKE2b with a longer output": above 64 bytes it is a chain of
/// 64-byte BLAKE2b outputs contributing 32 bytes each, with a final short
/// block. Getting this wrong yields plausible bytes for every length and the
/// right bytes only at 64.
fn variable_hash(out_len: usize, input: &[u8]) -> Vec<u8> {
    let mut prefixed = Vec::with_capacity(4 + input.len());
    prefixed.extend_from_slice(&(out_len as u32).to_le_bytes());
    prefixed.extend_from_slice(input);

    if out_len <= 64 {
        return blake2b(out_len, &prefixed).expect("out_len is within BLAKE2b's range");
    }

    let mut out = Vec::with_capacity(out_len);
    let mut v = blake2b(64, &prefixed).expect("64 is a legal digest length");
    out.extend_from_slice(&v[..32]);

    // Each further full block contributes its first 32 bytes.
    let r = out_len.div_ceil(32) - 2;
    for _ in 1..r {
        v = blake2b(64, &v).expect("64 is a legal digest length");
        out.extend_from_slice(&v[..32]);
    }

    let tail = out_len - 32 * r;
    let last = blake2b(tail, &v).expect("the tail is within BLAKE2b's range");
    out.extend_from_slice(&last);
    out
}

/// Derive `out.len()` bytes from a passphrase.
///
/// `secret` is the optional keyed pepper (`K`) and `associated_data` the
/// optional context (`X`) of RFC 9106 §3.2. Both are hashed into `H0` and both
/// default to empty via [`hash_into`].
pub fn hash_into_with_secret(
    variant: Variant,
    params: Params,
    password: &[u8],
    salt: &[u8],
    secret: &[u8],
    associated_data: &[u8],
    out: &mut [u8],
) -> Result<(), Argon2Error> {
    if params.lanes == 0 || params.lanes > 0x00ff_ffff {
        return Err(Argon2Error::Lanes {
            requested: params.lanes,
        });
    }
    if params.passes == 0 {
        return Err(Argon2Error::Passes {
            requested: params.passes,
        });
    }
    let minimum_memory = 8u32.saturating_mul(params.lanes);
    if params.memory_kib < minimum_memory {
        return Err(Argon2Error::MemoryTooSmall {
            requested: params.memory_kib,
            minimum: minimum_memory,
        });
    }
    if out.len() < 4 {
        return Err(Argon2Error::TagTooShort {
            requested: out.len(),
        });
    }
    if salt.len() < 8 {
        return Err(Argon2Error::SaltTooShort {
            requested: salt.len(),
        });
    }

    // H0 (RFC 9106 §3.2): every parameter is bound, so two derivations that
    // differ in any of them cannot collide.
    let mut h0_input = Blake2b::new(64).expect("64 is a legal digest length");
    for value in [
        params.lanes,
        out.len() as u32,
        params.memory_kib,
        params.passes,
        VERSION,
        variant.type_code(),
    ] {
        h0_input.update(&value.to_le_bytes());
    }
    for field in [password, salt, secret, associated_data] {
        h0_input.update(&(field.len() as u32).to_le_bytes());
        h0_input.update(field);
    }
    let h0 = h0_input.finalize();

    // Memory layout: m' is rounded DOWN to a multiple of 4*lanes so every slice
    // of every lane has the same length.
    let lanes = params.lanes as usize;
    let block_count = (params.memory_kib as usize / (SYNC_POINTS as usize * lanes))
        * (SYNC_POINTS as usize * lanes);
    let lane_length = block_count / lanes;
    let segment_length = lane_length / SYNC_POINTS as usize;

    let mut memory = vec![Block::zero(); block_count];

    // The first two blocks of every lane come straight from H0.
    for (lane, _) in (0..lanes).map(|l| (l, ())) {
        for index in 0..2usize {
            let mut input = Vec::with_capacity(72);
            input.extend_from_slice(&h0);
            input.extend_from_slice(&(index as u32).to_le_bytes());
            input.extend_from_slice(&(lane as u32).to_le_bytes());
            let bytes = variable_hash(BLOCK_BYTES, &input);
            memory[lane * lane_length + index] = Block::from_bytes(&bytes);
        }
    }

    for pass in 0..params.passes as usize {
        for slice in 0..SYNC_POINTS as usize {
            // Lanes are independent within a slice; they synchronize at the
            // slice boundary, which is why this loop nests inside `slice`.
            for lane in 0..lanes {
                fill_segment(
                    &mut memory,
                    variant,
                    params,
                    pass,
                    lane,
                    slice,
                    lanes,
                    lane_length,
                    segment_length,
                    block_count,
                );
            }
        }
    }

    // The final block is the XOR of every lane's last block.
    let mut final_block = memory[lane_length - 1];
    for lane in 1..lanes {
        final_block = final_block.xor(&memory[lane * lane_length + lane_length - 1]);
    }

    let tag = variable_hash(out.len(), &final_block.to_bytes());
    out.copy_from_slice(&tag);
    Ok(())
}

/// Derive `out.len()` bytes from a passphrase, with no pepper or context.
pub fn hash_into(
    variant: Variant,
    params: Params,
    password: &[u8],
    salt: &[u8],
    out: &mut [u8],
) -> Result<(), Argon2Error> {
    hash_into_with_secret(variant, params, password, salt, &[], &[], out)
}

/// Whether this position uses data-INDEPENDENT addressing.
///
/// The Argon2id rule (RFC 9106 §3.4), isolated into one function and made
/// public so a test can assert it directly rather than inferring it.
///
/// **WHY IT IS PUBLIC, which is a real design decision and not laziness.** The
/// obvious test of "is this really the hybrid" — deriving under all three
/// variants and asserting the three outputs differ — CANNOT detect a collapse.
/// The variant's type code is bound into `H0`, so Argon2id and Argon2i produce
/// different tags even when their addressing is byte-identical. That was
/// measured: collapsing this function to `true` reds four tests and leaves the
/// three-way inequality green. The addressing split needs a witness that looks
/// at the split itself, so the split is exposed.
pub fn uses_independent_addressing(variant: Variant, pass: usize, slice: usize) -> bool {
    match variant {
        Variant::Argon2i => true,
        Variant::Argon2d => false,
        Variant::Argon2id => pass == 0 && slice < 2,
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_segment(
    memory: &mut [Block],
    variant: Variant,
    params: Params,
    pass: usize,
    lane: usize,
    slice: usize,
    lanes: usize,
    lane_length: usize,
    segment_length: usize,
    block_count: usize,
) {
    let independent = uses_independent_addressing(variant, pass, slice);

    // Data-independent addressing derives its J1/J2 from a counter block rather
    // than from the previous block's contents — that is the whole side-channel
    // argument, so the address block is regenerated every 128 positions.
    let mut address_block = Block::zero();
    let mut input_block = Block::zero();
    if independent {
        input_block.0[0] = pass as u64;
        input_block.0[1] = lane as u64;
        input_block.0[2] = slice as u64;
        input_block.0[3] = block_count as u64;
        input_block.0[4] = u64::from(params.passes);
        input_block.0[5] = u64::from(variant.type_code());
    }

    // Positions 0 and 1 of every lane's first slice are already filled from H0.
    let start = if pass == 0 && slice == 0 { 2 } else { 0 };

    // THE FIRST SEGMENT NEEDS ITS ADDRESS BLOCK GENERATED HERE, and this is the
    // easiest thing in Argon2 to get wrong. The in-loop trigger below fires on
    // `index % 128 == 0`, but this segment starts at index 2, so it never fires
    // and the segment would address itself through an all-zero block.
    //
    // RFC 9106's own §5.3 known-answer test CANNOT CATCH THIS. Its parameters
    // (m=32, p=4) give a segment length of 2, so this segment's loop body runs
    // zero times and the data-independent addressing path is never entered. The
    // bug was found by the parameter sweep at m=64, p=2 — segment length 8 —
    // where six positions do run. A published KAT is a strong oracle for what it
    // covers and proves nothing about what it does not.
    if independent && pass == 0 && slice == 0 {
        input_block.0[6] += 1;
        address_block = compress(&Block::zero(), &compress(&Block::zero(), &input_block));
    }

    for index in start..segment_length {
        let position = slice * segment_length + index;
        let previous = if position == 0 {
            lane * lane_length + lane_length - 1
        } else {
            lane * lane_length + position - 1
        };

        let (j1, j2) = if independent {
            if index % BLOCK_WORDS == 0 {
                input_block.0[6] += 1;
                address_block = compress(&Block::zero(), &compress(&Block::zero(), &input_block));
            }
            let word = address_block.0[index % BLOCK_WORDS];
            ((word & 0xffff_ffff) as u32, (word >> 32) as u32)
        } else {
            let word = memory[previous].0[0];
            ((word & 0xffff_ffff) as u32, (word >> 32) as u32)
        };

        // In the very first slice of the very first pass no other lane has any
        // finished block, so a cross-lane reference is not yet legal.
        let ref_lane = if pass == 0 && slice == 0 {
            lane
        } else {
            (j2 as usize) % lanes
        };
        let same_lane = ref_lane == lane;

        let reference_area = if pass == 0 {
            if slice == 0 {
                index - 1
            } else if same_lane {
                slice * segment_length + index - 1
            } else {
                slice * segment_length - usize::from(index == 0)
            }
        } else if same_lane {
            lane_length - segment_length + index - 1
        } else {
            lane_length - segment_length - usize::from(index == 0)
        };

        // RFC 9106 §3.4.1.2's quadratic mapping: it biases references toward
        // recent blocks, and the two 32-bit shifts are exactly as specified.
        let mut relative = u64::from(j1);
        relative = (relative * relative) >> 32;
        relative = reference_area as u64 - 1 - ((reference_area as u64 * relative) >> 32);

        // The reference window starts at the beginning of the lane on the first
        // pass (nothing later exists yet) and on the last slice (the window has
        // wrapped); otherwise it starts just past the current slice. The first
        // two cases share a value but not a reason, which is why they are
        // written as one condition rather than two identical branches.
        let start_position = if pass == 0 || slice == SYNC_POINTS as usize - 1 {
            0
        } else {
            (slice + 1) * segment_length
        };
        let ref_index = (start_position + relative as usize) % lane_length;

        let current = lane * lane_length + position;
        let reference = memory[ref_lane * lane_length + ref_index];
        let fresh = compress(&memory[previous], &reference);

        // After the first pass the new block is XORed into the old one rather
        // than replacing it, which is what makes later passes strictly add
        // work instead of recomputing the first.
        memory[current] = if pass == 0 {
            fresh
        } else {
            fresh.xor(&memory[current])
        };
    }
}
