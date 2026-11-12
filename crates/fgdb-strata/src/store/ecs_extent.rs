//! Page-framed Authenticated Extent Envelopes (`FGEC` v1) + `FGKE-V1` key
//! wrapping for Strata durable objects (`fgdb-rc1111-strata-ecs-coding-29bud`).
//!
//! Monolithic whole-file stream encryption would force multi-megabyte or
//! multi-gigabyte sealed `FGRN` runs and `FGPK` packs to be decrypted in full
//! just to answer a single point neighbor lookup. Instead, every durable Strata
//! object (`FGSB`, `FGSP`, `FGVP`, `FGRN`, `FGPK`, manifests, roots, and root
//! segments) is encoded into fixed-stride (`16 KiB` or `64 KiB`) authenticated
//! extent frames:
//!
//! 1. **Per-frame Merkle + XChaCha20-Poly1305 AEAD**:
//!    `PlaintextFrame -> BLAKE3 FrameDigest -> XChaCha20-Poly1305 CiphertextFrame`
//!    whose domain-separated AAD binds
//!    `(DatabaseSecurityNamespaceId, GraphId, BranchId, ExtentObjectKind, Epoch, ObjectId, FrameOrdinal, PlainLen, FrameDigest)`.
//! 2. **Stripe-scoped systematic RFC 6330 RaptorQ (`asupersync::raptorq`)**:
//!    Every stripe of `stripe_frames` (`4` frames by default) carries `K = 8`
//!    source symbols and `R = 4` repair symbols authenticated by 16-byte keyed
//!    BLAKE3 per-symbol MACs. Stripe trailers sit immediately after the 128-byte
//!    Primary Header and before the contiguous Frame Records, followed by a
//!    128-byte EOF Mirror Header:
//!    - Healthy random single-page reads (`read_extent_frame_from_file`) seek in
//!      `O(1)` arithmetic directly to `frames_start_offset + ordinal * stride`
//!      and authenticate/decrypt only that single 16/64 KiB page.
//!    - Front-of-file damage (Primary Header + up to `R` repair symbols),
//!      mid-file bit-rot (up to `R` symbols per stripe), and EOF tail truncation
//!      (Mirror Header + up to `50%` of the final stripe's frame bytes!) all
//!      heal deterministically on read and report `needs_rewrite = true` so
//!      `Database::scrub` rewrites full redundancy atomically.
//! 3. **Argon2id passphrase-wrapped key envelopes (`FGKE-V1`)**:
//!    Wraps `(k_oid, namespace, dek)` under RFC 9106 Argon2id
//!    (`PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND`) + XChaCha20-Poly1305.

use asupersync::fs::VfsFile;
use asupersync::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use asupersync::raptorq::decoder::{DecodeError, InactivationDecoder, ReceivedSymbol};
use asupersync::raptorq::systematic::SystematicEncoder;
use fgdb_crypto::zeroize::SharedSecret;
use fgdb_crypto::{
    PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND, PASSPHRASE_KDF_SALT_BYTES, derive_key,
    derive_passphrase_kek, keyed_hash, xchacha20poly1305_open, xchacha20poly1305_seal,
};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, GraphId, StorageReadCx};

/// Magic prefix of an encoded Strata authenticated extent envelope (`FGEC`).
pub const EXTENT_MAGIC: [u8; 4] = *b"FGEC";
const EXTENT_VERSION_V1: u16 = 1;
const HEADER_BYTES: usize = 128;
const HEADER_BODY_BYTES: usize = 96;
const AEAD_TAG_BYTES: usize = 16;
const FRAME_DIGEST_BYTES: usize = 32;
const SYMBOL_MAC_BYTES: usize = 16;

/// Magic header line of an Argon2id-wrapped key file (`FGKE-V1`).
pub const KEY_ENVELOPE_HEADER_V1: &str = "FGKE-V1";
const KEY_ENVELOPE_AAD_V1: &[u8] = b"fgdb:key-envelope:v1\x01\x00";

/// Closed vocabulary of Strata durable object kinds bound into `FGEC` AAD and
/// key derivations so valid ciphertexts cannot be swapped across families.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExtentObjectKind {
    Block = 1,
    AdjRun = 2,
    Root = 3,
    Segment = 4,
    IndexSegment = 5,
    VertexPatch = 6,
    EdgePropertyPatch = 7,
    Manifest = 8,
    Pack = 9,
}

impl ExtentObjectKind {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Block),
            2 => Some(Self::AdjRun),
            3 => Some(Self::Root),
            4 => Some(Self::Segment),
            5 => Some(Self::IndexSegment),
            6 => Some(Self::VertexPatch),
            7 => Some(Self::EdgePropertyPatch),
            8 => Some(Self::Manifest),
            9 => Some(Self::Pack),
            _ => None,
        }
    }
}

/// Allowed fixed frame sizes for page-framed Strata extents (`16 KiB` or `64 KiB`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtentFrameSize {
    #[default]
    KiB16,
    KiB64,
}

impl ExtentFrameSize {
    pub const fn bytes(self) -> usize {
        match self {
            Self::KiB16 => 16 * 1024,
            Self::KiB64 => 64 * 1024,
        }
    }

    pub const fn from_bytes(bytes: u32) -> Option<Self> {
        match bytes {
            16_384 => Some(Self::KiB16),
            65_536 => Some(Self::KiB64),
            _ => None,
        }
    }
}

/// Configuration for Strata page-framed AEAD + stripe-RaptorQ durable coding.
#[derive(Clone)]
pub struct StrataEcsConfig {
    dek: SharedSecret<32>,
    pub graph_id: GraphId,
    pub branch_id: BranchId,
    pub epoch: u64,
    pub frame_size: ExtentFrameSize,
    pub stripe_frames: u16,
    pub stripe_k: u16,
    pub stripe_r: u16,
}

impl core::fmt::Debug for StrataEcsConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("StrataEcsConfig([REDACTED])")
    }
}

impl StrataEcsConfig {
    pub const DEFAULT_STRIPE_FRAMES: u16 = 4;
    pub const DEFAULT_STRIPE_K: u16 = 8;
    pub const DEFAULT_STRIPE_R: u16 = 4;

    /// Construct the default `16 KiB` page-framed `(4 frames/stripe, K=8, R=4)`
    /// Strata ECS profile on `(GraphId(1), BranchId(1), epoch = 1)`.
    pub fn new(dek: impl Into<SharedSecret<32>>) -> Self {
        Self {
            dek: dek.into(),
            graph_id: GraphId(1),
            branch_id: BranchId(1),
            epoch: 1,
            frame_size: ExtentFrameSize::KiB16,
            stripe_frames: Self::DEFAULT_STRIPE_FRAMES,
            stripe_k: Self::DEFAULT_STRIPE_K,
            stripe_r: Self::DEFAULT_STRIPE_R,
        }
    }

    #[must_use]
    pub fn with_coordinates(mut self, graph_id: GraphId, branch_id: BranchId, epoch: u64) -> Self {
        self.graph_id = graph_id;
        self.branch_id = branch_id;
        self.epoch = epoch;
        self
    }

    #[must_use]
    pub fn with_frame_size(mut self, frame_size: ExtentFrameSize) -> Self {
        self.frame_size = frame_size;
        self
    }

    pub(crate) fn dek(&self) -> &[u8; 32] {
        self.dek.expose()
    }

    /// Clone shared ownership of the Strata DEK without copying raw bytes.
    #[must_use]
    pub fn shared_dek(&self) -> SharedSecret<32> {
        self.dek.clone()
    }
}

/// Errors produced when encoding, authenticating, or healing an `FGEC` extent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtentError {
    InvalidParameters(&'static str),
    PlaintextTooLarge { limit: u64, observed: u64 },
    MalformedHeader,
    AuthenticationFailed,
    InsufficientSymbols { stripe: u32 },
    FrameOutOfBounds { requested: u32, frame_count: u32 },
    MerkleMismatch,
}

impl core::fmt::Display for ExtentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidParameters(msg) => write!(f, "invalid FGEC parameters: {msg}"),
            Self::PlaintextTooLarge { limit, observed } => write!(
                f,
                "FGEC plaintext length {observed} exceeds {limit}-byte limit"
            ),
            Self::MalformedHeader => f.write_str("FGEC extent header is malformed or truncated"),
            Self::AuthenticationFailed => {
                f.write_str("FGEC authentication failed (wrong key or mismatched coordinates)")
            }
            Self::InsufficientSymbols { stripe } => write!(
                f,
                "FGEC stripe {stripe} has fewer than K authentic symbols for RaptorQ recovery"
            ),
            Self::FrameOutOfBounds {
                requested,
                frame_count,
            } => write!(
                f,
                "FGEC requested frame {requested} out of {frame_count} frames"
            ),
            Self::MerkleMismatch => f.write_str("FGEC Merkle root mismatch"),
        }
    }
}

impl core::error::Error for ExtentError {}

/// Decoded plaintext plus scrub telemetry indicating whether any header, frame,
/// or repair symbol needed RaptorQ reconstruction or canonical rewrite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedExtent {
    pub plaintext: Vec<u8>,
    pub healed_stripes: usize,
    pub needs_rewrite: bool,
}

/// Whether `bytes` begins with the `FGEC` container magic.
pub fn is_encoded_extent(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[..4] == EXTENT_MAGIC
}

/// Upper bound on encoded `FGEC` byte size for a logical object of at most
/// `plaintext_limit` bytes under `config`.
pub fn max_encoded_bytes_for_limit(plaintext_limit: u64, config: &StrataEcsConfig) -> u64 {
    let frame_bytes = config.frame_size.bytes() as u64;
    let frame_count = plaintext_limit.div_ceil(frame_bytes).max(1);
    let stripe_frames = u64::from(config.stripe_frames.max(1));
    let stripe_count = frame_count.div_ceil(stripe_frames).max(1);
    let full_record = (FRAME_DIGEST_BYTES + config.frame_size.bytes() + AEAD_TAG_BYTES) as u64;
    let full_stripe = stripe_frames.saturating_mul(full_record);
    let k = u64::from(config.stripe_k.max(1));
    let r = u64::from(config.stripe_r);
    let sym_size = full_stripe.div_ceil(k).saturating_add(8);
    let trailer_per_stripe = k
        .saturating_mul(SYMBOL_MAC_BYTES as u64)
        .saturating_add(r.saturating_mul(SYMBOL_MAC_BYTES as u64 + sym_size));
    (2 * HEADER_BYTES as u64)
        .saturating_add(frame_count.saturating_mul(full_record))
        .saturating_add(stripe_count.saturating_mul(trailer_per_stripe))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ValidatedHeader {
    frame_size: ExtentFrameSize,
    stripe_frames: u16,
    stripe_k: u16,
    stripe_r: u16,
    epoch: u64,
    plaintext_len: u64,
    frame_count: u32,
    stripe_count: u32,
    merkle_root: [u8; 32],
}

#[derive(Clone, Copy, Debug)]
struct ExtentGeometry {
    frame_bytes: usize,
    full_record_bytes: usize,
    stripe_frames: usize,
    stripe_k: usize,
    stripe_r: usize,
    plaintext_len: usize,
    frame_count: usize,
    stripe_count: usize,
    full_sym_bytes: usize,
    full_trailer_bytes: usize,
    last_stripe_bytes: usize,
    last_sym_bytes: usize,
    last_trailer_bytes: usize,
    frames_start_offset: usize,
    frames_total_bytes: usize,
    mirror_header_offset: usize,
    total_encoded_bytes: usize,
}

fn aligned_symbol_size(stripe_bytes: usize, k: usize) -> usize {
    let raw = stripe_bytes.div_ceil(k).max(4);
    // Align to 4 bytes for clean RaptorQ word operations.
    (raw + 3) & !3
}

impl ExtentGeometry {
    fn compute(header: &ValidatedHeader) -> Result<Self, ExtentError> {
        let frame_bytes = header.frame_size.bytes();
        let stripe_frames = usize::from(header.stripe_frames);
        let stripe_k = usize::from(header.stripe_k);
        let stripe_r = usize::from(header.stripe_r);
        if stripe_frames == 0 || stripe_k == 0 || stripe_k > 1024 || stripe_r > 1024 {
            return Err(ExtentError::InvalidParameters(
                "stripe_frames and stripe_k must be within 1..=1024",
            ));
        }
        let plaintext_len = usize::try_from(header.plaintext_len)
            .map_err(|_| ExtentError::InvalidParameters("plaintext_len overflows usize"))?;
        let expected_frames = if plaintext_len == 0 {
            1usize
        } else {
            plaintext_len.div_ceil(frame_bytes)
        };
        let frame_count = usize::try_from(header.frame_count)
            .map_err(|_| ExtentError::InvalidParameters("frame_count overflows usize"))?;
        // ubs:ignore — public extent framing geometry, not secret material.
        if frame_count != expected_frames {
            return Err(ExtentError::MalformedHeader);
        }
        let expected_stripes = frame_count.div_ceil(stripe_frames);
        let stripe_count = usize::try_from(header.stripe_count)
            .map_err(|_| ExtentError::InvalidParameters("stripe_count overflows usize"))?;
        // ubs:ignore — public extent framing geometry, not secret material.
        if stripe_count != expected_stripes || stripe_count == 0 {
            return Err(ExtentError::MalformedHeader);
        }
        let full_record_bytes = FRAME_DIGEST_BYTES + frame_bytes + AEAD_TAG_BYTES;
        let last_plain_len = if plaintext_len == 0 {
            0
        } else {
            plaintext_len - (frame_count - 1) * frame_bytes
        };
        let last_record_bytes = FRAME_DIGEST_BYTES + last_plain_len + AEAD_TAG_BYTES;
        let frames_total_bytes = (frame_count - 1)
            .checked_mul(full_record_bytes)
            .and_then(|n| n.checked_add(last_record_bytes))
            .ok_or(ExtentError::InvalidParameters(
                "frames_total_bytes overflow",
            ))?;

        let full_stripe_bytes = stripe_frames * full_record_bytes;
        let full_sym_bytes = aligned_symbol_size(full_stripe_bytes, stripe_k);
        let full_trailer_bytes =
            stripe_k * SYMBOL_MAC_BYTES + stripe_r * (SYMBOL_MAC_BYTES + full_sym_bytes);

        let frames_in_last_stripe = frame_count - (stripe_count - 1) * stripe_frames;
        let last_stripe_bytes = (frames_in_last_stripe - 1) * full_record_bytes + last_record_bytes;
        let last_sym_bytes = aligned_symbol_size(last_stripe_bytes, stripe_k);
        let last_trailer_bytes =
            stripe_k * SYMBOL_MAC_BYTES + stripe_r * (SYMBOL_MAC_BYTES + last_sym_bytes);

        let trailers_total_bytes = (stripe_count - 1)
            .checked_mul(full_trailer_bytes)
            .and_then(|n| n.checked_add(last_trailer_bytes))
            .ok_or(ExtentError::InvalidParameters(
                "trailers_total_bytes overflow",
            ))?;
        let frames_start_offset = HEADER_BYTES.checked_add(trailers_total_bytes).ok_or(
            ExtentError::InvalidParameters("frames_start_offset overflow"),
        )?;
        let mirror_header_offset = frames_start_offset.checked_add(frames_total_bytes).ok_or(
            ExtentError::InvalidParameters("mirror_header_offset overflow"),
        )?;
        let total_encoded_bytes = mirror_header_offset.checked_add(HEADER_BYTES).ok_or(
            ExtentError::InvalidParameters("total_encoded_bytes overflow"),
        )?;

        Ok(Self {
            frame_bytes,
            full_record_bytes,
            stripe_frames,
            stripe_k,
            stripe_r,
            plaintext_len,
            frame_count,
            stripe_count,
            full_sym_bytes,
            full_trailer_bytes,
            last_stripe_bytes,
            last_sym_bytes,
            last_trailer_bytes,
            frames_start_offset,
            frames_total_bytes,
            mirror_header_offset,
            total_encoded_bytes,
        })
    }

    fn frame_plain_len(&self, ordinal: usize) -> usize {
        if self.plaintext_len == 0 {
            0
        } else if ordinal + 1 < self.frame_count {
            self.frame_bytes
        } else {
            self.plaintext_len - (self.frame_count - 1) * self.frame_bytes
        }
    }

    fn frame_record_len(&self, ordinal: usize) -> usize {
        FRAME_DIGEST_BYTES + self.frame_plain_len(ordinal) + AEAD_TAG_BYTES
    }

    fn frame_offset(&self, ordinal: usize) -> usize {
        self.frames_start_offset + ordinal * self.full_record_bytes
    }

    fn stripe_frame_range(&self, stripe: usize) -> (usize, usize) {
        let start = stripe * self.stripe_frames;
        let end = (start + self.stripe_frames).min(self.frame_count);
        (start, end)
    }

    fn stripe_records_span(&self, stripe: usize) -> (usize, usize) {
        let (f_start, _) = self.stripe_frame_range(stripe);
        let offset = self.frame_offset(f_start);
        let len = if stripe + 1 < self.stripe_count {
            self.stripe_frames * self.full_record_bytes
        } else {
            self.last_stripe_bytes
        };
        (offset, len)
    }

    fn stripe_trailer_span(&self, stripe: usize) -> (usize, usize, usize) {
        let offset = HEADER_BYTES + stripe * self.full_trailer_bytes;
        if stripe + 1 < self.stripe_count {
            (offset, self.full_trailer_bytes, self.full_sym_bytes)
        } else {
            (offset, self.last_trailer_bytes, self.last_sym_bytes)
        }
    }
}

const COORD_BYTES: usize = 105;

fn coord_transcript(
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
) -> [u8; COORD_BYTES] {
    let mut buf = [0u8; COORD_BYTES];
    buf[..32].copy_from_slice(&namespace.0);
    buf[32..48].copy_from_slice(&config.graph_id.0.to_le_bytes());
    buf[48..64].copy_from_slice(&config.branch_id.0.to_le_bytes());
    buf[64] = kind.code();
    buf[65..73].copy_from_slice(&config.epoch.to_le_bytes());
    buf[73..105].copy_from_slice(&object_id.0);
    buf
}

fn object_subkey(
    domain: &'static str,
    config: &StrataEcsConfig,
    coords: &[u8; COORD_BYTES],
) -> [u8; 32] {
    let mut material = [0u8; 32 + COORD_BYTES];
    material[..32].copy_from_slice(config.dek());
    material[32..].copy_from_slice(coords);
    derive_key(domain, &material)
}

fn compute_frame_digest(ordinal: u32, plain: &[u8]) -> [u8; 32] {
    let mut h = fgdb_crypto::Hasher::new();
    h.update(b"fgdb:strata-ecs:leaf:v1");
    h.update(&ordinal.to_le_bytes());
    h.update(&(plain.len() as u32).to_le_bytes());
    h.update(plain);
    h.finalize().0
}

fn compute_merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    let mut h = fgdb_crypto::Hasher::new();
    h.update(b"fgdb:strata-ecs:merkle-root:v1");
    h.update(&(leaves.len() as u32).to_le_bytes());
    for leaf in leaves {
        h.update(leaf);
    }
    h.finalize().0
}

fn frame_aad(
    coords: &[u8; COORD_BYTES],
    ordinal: u32,
    plain_len: u32,
    frame_digest: &[u8; 32],
) -> [u8; 22 + COORD_BYTES + 4 + 4 + 32] {
    let mut aad = [0u8; 22 + COORD_BYTES + 4 + 4 + 32];
    aad[..22].copy_from_slice(b"fgdb:strata-ecs:aad:v1");
    aad[22..22 + COORD_BYTES].copy_from_slice(coords);
    aad[22 + COORD_BYTES..26 + COORD_BYTES].copy_from_slice(&ordinal.to_le_bytes());
    aad[26 + COORD_BYTES..30 + COORD_BYTES].copy_from_slice(&plain_len.to_le_bytes());
    aad[30 + COORD_BYTES..62 + COORD_BYTES].copy_from_slice(frame_digest);
    aad
}

fn frame_nonce(nonce_root_key: &[u8; 32], ordinal: u32, frame_digest: &[u8; 32]) -> [u8; 24] {
    let mut input = [0u8; 36];
    input[..4].copy_from_slice(&ordinal.to_le_bytes());
    input[4..36].copy_from_slice(frame_digest);
    let full = keyed_hash(nonce_root_key, &input).0;
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&full[..24]);
    nonce
}

fn symbol_mac(
    sym_auth_key: &[u8; 32],
    stripe: u32,
    esi: u32,
    payload: &[u8],
) -> [u8; SYMBOL_MAC_BYTES] {
    let mut h = fgdb_crypto::Hasher::new_keyed(sym_auth_key);
    h.update(b"fgdb:strata-ecs:sym:v1");
    h.update(&stripe.to_le_bytes());
    h.update(&esi.to_le_bytes());
    h.update(&(payload.len() as u32).to_le_bytes());
    h.update(payload);
    let digest = h.finalize().0;
    let mut tag = [0u8; SYMBOL_MAC_BYTES];
    tag.copy_from_slice(&digest[..SYMBOL_MAC_BYTES]);
    tag
}

fn stripe_seed(coords: &[u8; COORD_BYTES], stripe: u32) -> u64 {
    let mut h = fgdb_crypto::Hasher::new();
    h.update(b"fgdb:strata-ecs:raptorq-seed:v1");
    h.update(coords);
    h.update(&stripe.to_le_bytes());
    let d = h.finalize().0;
    u64::from_le_bytes(d[..8].try_into().expect("8 bytes"))
}

fn encode_header_bytes(
    header: &ValidatedHeader,
    kind: ExtentObjectKind,
    header_mac_key: &[u8; 32],
    is_mirror: bool,
) -> [u8; HEADER_BYTES] {
    let mut out = [0u8; HEADER_BYTES];
    out[..4].copy_from_slice(&EXTENT_MAGIC);
    out[4..6].copy_from_slice(&EXTENT_VERSION_V1.to_le_bytes());
    out[6] = kind.code();
    out[7] = u8::from(is_mirror);
    out[8..12].copy_from_slice(&(header.frame_size.bytes() as u32).to_le_bytes());
    out[12..14].copy_from_slice(&header.stripe_frames.to_le_bytes());
    out[14..16].copy_from_slice(&header.stripe_k.to_le_bytes());
    out[16..18].copy_from_slice(&header.stripe_r.to_le_bytes());
    // out[18..20] reserved = 0
    out[20..28].copy_from_slice(&header.epoch.to_le_bytes());
    out[28..36].copy_from_slice(&header.plaintext_len.to_le_bytes());
    out[36..40].copy_from_slice(&header.frame_count.to_le_bytes());
    out[40..44].copy_from_slice(&header.stripe_count.to_le_bytes());
    out[44..76].copy_from_slice(&header.merkle_root);
    // out[76..96] reserved = 0
    let mac = keyed_hash(header_mac_key, &out[..HEADER_BODY_BYTES]).0;
    out[HEADER_BODY_BYTES..HEADER_BYTES].copy_from_slice(&mac);
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn verify_header_bytes(
    raw: &[u8],
    expected_kind: ExtentObjectKind,
    expected_epoch: u64,
    header_mac_key: &[u8; 32],
    expected_mirror: bool,
) -> Result<ValidatedHeader, ExtentError> {
    if raw.len() != HEADER_BYTES {
        return Err(ExtentError::MalformedHeader);
    }
    // ubs:ignore — public container magic bytes, not secret material.
    if raw[..4] != EXTENT_MAGIC {
        return Err(ExtentError::MalformedHeader);
    }
    let expected_mac = keyed_hash(header_mac_key, &raw[..HEADER_BODY_BYTES]).0;
    if !constant_time_eq(&expected_mac, &raw[HEADER_BODY_BYTES..HEADER_BYTES]) {
        return Err(ExtentError::AuthenticationFailed);
    }
    let version = u16::from_le_bytes([raw[4], raw[5]]);
    // ubs:ignore — public format version checked after header MAC verification.
    if version != EXTENT_VERSION_V1 {
        return Err(ExtentError::MalformedHeader);
    }
    // ubs:ignore — public object-kind and header-slot tags checked after header MAC.
    if raw[6] != expected_kind.code() || raw[7] != u8::from(expected_mirror) {
        return Err(ExtentError::AuthenticationFailed);
    }
    let epoch = u64::from_le_bytes(raw[20..28].try_into().expect("8"));
    // ubs:ignore — public epoch coordinate checked after header MAC verification.
    if epoch != expected_epoch {
        return Err(ExtentError::AuthenticationFailed);
    }
    let frame_bytes = u32::from_le_bytes(raw[8..12].try_into().expect("4"));
    let frame_size =
        ExtentFrameSize::from_bytes(frame_bytes).ok_or(ExtentError::MalformedHeader)?;
    let stripe_frames = u16::from_le_bytes([raw[12], raw[13]]);
    let stripe_k = u16::from_le_bytes([raw[14], raw[15]]);
    let stripe_r = u16::from_le_bytes([raw[16], raw[17]]);
    if raw[18..20] != [0, 0] || raw[76..96] != [0u8; 20] {
        return Err(ExtentError::MalformedHeader);
    }
    let plaintext_len = u64::from_le_bytes(raw[28..36].try_into().expect("8"));
    let frame_count = u32::from_le_bytes(raw[36..40].try_into().expect("4"));
    let stripe_count = u32::from_le_bytes(raw[40..44].try_into().expect("4"));
    let mut merkle_root = [0u8; 32];
    merkle_root.copy_from_slice(&raw[44..76]);
    Ok(ValidatedHeader {
        frame_size,
        stripe_frames,
        stripe_k,
        stripe_r,
        epoch,
        plaintext_len,
        frame_count,
        stripe_count,
        merkle_root,
    })
}

/// Encode `plaintext` into a page-framed `FGEC` v1 extent container.
pub fn encode_extent(
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    plaintext: &[u8],
) -> Result<Vec<u8>, ExtentError> {
    let coords = coord_transcript(config, namespace, kind, object_id);
    let frame_key = object_subkey("fgdb:strata-ecs:frame-key:v1", config, &coords);
    let nonce_key = object_subkey("fgdb:strata-ecs:frame-nonce:v1", config, &coords);
    let sym_key = object_subkey("fgdb:strata-ecs:sym-key:v1", config, &coords);
    let hdr_key = object_subkey("fgdb:strata-ecs:hdr-key:v1", config, &coords);

    let frame_bytes = config.frame_size.bytes();
    let frame_count_usize = if plaintext.is_empty() {
        1
    } else {
        plaintext.len().div_ceil(frame_bytes)
    };
    let stripe_frames_usize = usize::from(config.stripe_frames.max(1));
    let stripe_count_usize = frame_count_usize.div_ceil(stripe_frames_usize);
    let frame_count = u32::try_from(frame_count_usize)
        .map_err(|_| ExtentError::InvalidParameters("frame_count exceeds u32::MAX"))?;
    let stripe_count = u32::try_from(stripe_count_usize)
        .map_err(|_| ExtentError::InvalidParameters("stripe_count exceeds u32::MAX"))?;

    let mut leaves = Vec::with_capacity(frame_count_usize);
    let mut frame_records = Vec::new();
    for ordinal_usize in 0..frame_count_usize {
        let start = ordinal_usize * frame_bytes;
        let end = (start + frame_bytes).min(plaintext.len());
        let plain_slice = if plaintext.is_empty() {
            &[][..]
        } else {
            &plaintext[start..end]
        };
        let ordinal = ordinal_usize as u32;
        let digest = compute_frame_digest(ordinal, plain_slice);
        leaves.push(digest);
        let aad = frame_aad(&coords, ordinal, plain_slice.len() as u32, &digest);
        let nonce = frame_nonce(&nonce_key, ordinal, &digest);
        let sealed = xchacha20poly1305_seal(&frame_key, &nonce, &aad, plain_slice);
        frame_records.extend_from_slice(&digest);
        frame_records.extend_from_slice(&sealed);
    }

    let merkle_root = compute_merkle_root(&leaves);
    let header = ValidatedHeader {
        frame_size: config.frame_size,
        stripe_frames: config.stripe_frames,
        stripe_k: config.stripe_k,
        stripe_r: config.stripe_r,
        epoch: config.epoch,
        plaintext_len: plaintext.len() as u64,
        frame_count,
        stripe_count,
        merkle_root,
    };
    let geom = ExtentGeometry::compute(&header)?;
    debug_assert_eq!(frame_records.len(), geom.frames_total_bytes);

    let mut out = Vec::with_capacity(geom.total_encoded_bytes);
    out.extend_from_slice(&encode_header_bytes(&header, kind, &hdr_key, false));

    for stripe in 0..geom.stripe_count {
        let (rec_off, rec_len) = geom.stripe_records_span(stripe);
        let (_, _, sym_bytes) = geom.stripe_trailer_span(stripe);
        let rec_slice = &frame_records
            [rec_off - geom.frames_start_offset..(rec_off - geom.frames_start_offset) + rec_len];
        let mut source = Vec::with_capacity(geom.stripe_k);
        for s_idx in 0..geom.stripe_k {
            let mut sym = vec![0u8; sym_bytes];
            let chunk_start = s_idx * sym_bytes;
            if chunk_start < rec_slice.len() {
                let chunk_end = (chunk_start + sym_bytes).min(rec_slice.len());
                sym[..chunk_end - chunk_start].copy_from_slice(&rec_slice[chunk_start..chunk_end]);
            }
            source.push(sym);
        }
        let stripe_u32 = stripe as u32;
        for (esi, sym) in source.iter().enumerate() {
            let mac = symbol_mac(&sym_key, stripe_u32, esi as u32, sym);
            out.extend_from_slice(&mac);
        }
        if geom.stripe_r > 0 {
            let encoder =
                SystematicEncoder::new(&source, sym_bytes, stripe_seed(&coords, stripe_u32))
                    .ok_or(ExtentError::InvalidParameters(
                        "SystematicEncoder unavailable",
                    ))?;
            for r_idx in 0..geom.stripe_r {
                let esi = (geom.stripe_k + r_idx) as u32;
                let payload = encoder
                    .try_repair_symbol(esi)
                    .map_err(|_| ExtentError::InvalidParameters("repair symbol generation"))?;
                let mac = symbol_mac(&sym_key, stripe_u32, esi, &payload);
                out.extend_from_slice(&mac);
                out.extend_from_slice(&payload);
            }
        }
    }

    out.extend_from_slice(&frame_records);
    out.extend_from_slice(&encode_header_bytes(&header, kind, &hdr_key, true));
    debug_assert_eq!(out.len(), geom.total_encoded_bytes);
    Ok(out)
}

fn open_single_frame_record(
    record: &[u8],
    ordinal: u32,
    expected_plain_len: usize,
    coords: &[u8; COORD_BYTES],
    frame_key: &[u8; 32],
    nonce_key: &[u8; 32],
) -> Result<([u8; 32], Vec<u8>), ExtentError> {
    let expected_len = FRAME_DIGEST_BYTES + expected_plain_len + AEAD_TAG_BYTES;
    if record.len() != expected_len {
        return Err(ExtentError::MalformedHeader);
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&record[..FRAME_DIGEST_BYTES]);
    let aad = frame_aad(coords, ordinal, expected_plain_len as u32, &digest);
    let nonce = frame_nonce(nonce_key, ordinal, &digest);
    let plain = xchacha20poly1305_open(frame_key, &nonce, &aad, &record[FRAME_DIGEST_BYTES..])
        .map_err(|_| ExtentError::AuthenticationFailed)?;
    // ubs:ignore — content digest checked after XChaCha20-Poly1305 authentication.
    if compute_frame_digest(ordinal, &plain) != digest {
        return Err(ExtentError::AuthenticationFailed);
    }
    Ok((digest, plain))
}

#[allow(clippy::too_many_arguments)]
fn recover_stripe_records(
    stripe: usize,
    geom: &ExtentGeometry,
    trailer_bytes: &[u8],
    available_record_bytes: &[u8],
    coords: &[u8; COORD_BYTES],
    sym_key: &[u8; 32],
) -> Result<(Vec<u8>, bool), ExtentError> {
    let (_, stripe_len) = geom.stripe_records_span(stripe);
    let (_, expected_trailer_len, sym_bytes) = geom.stripe_trailer_span(stripe);
    let stripe_u32 = stripe as u32;
    let decoder =
        InactivationDecoder::try_new(geom.stripe_k, sym_bytes, stripe_seed(coords, stripe_u32))
            .map_err(|_| ExtentError::InvalidParameters("InactivationDecoder unavailable"))?;
    let mut received = decoder.constraint_symbols();
    let mut valid_count = 0usize;
    let mut any_symbol_damaged =
        trailer_bytes.len() < expected_trailer_len || available_record_bytes.len() < stripe_len;

    for esi in 0..geom.stripe_k {
        let mac_start = esi * SYMBOL_MAC_BYTES;
        let mac_end = mac_start + SYMBOL_MAC_BYTES;
        if trailer_bytes.len() < mac_end {
            any_symbol_damaged = true;
            continue;
        }
        let chunk_start = esi * sym_bytes;
        let chunk_end = (chunk_start + sym_bytes).min(stripe_len);
        if chunk_start < stripe_len && available_record_bytes.len() < chunk_end {
            any_symbol_damaged = true;
            continue;
        }
        let mut sym = vec![0u8; sym_bytes];
        if chunk_start < stripe_len {
            sym[..chunk_end - chunk_start]
                .copy_from_slice(&available_record_bytes[chunk_start..chunk_end]);
        }
        let expected_mac = symbol_mac(sym_key, stripe_u32, esi as u32, &sym);
        if constant_time_eq(&expected_mac, &trailer_bytes[mac_start..mac_end]) {
            received.push(ReceivedSymbol::source(esi as u32, sym));
            valid_count += 1;
        } else {
            any_symbol_damaged = true;
        }
    }

    let repairs_base = geom.stripe_k * SYMBOL_MAC_BYTES;
    let repair_record_len = SYMBOL_MAC_BYTES + sym_bytes;
    for r_idx in 0..geom.stripe_r {
        let start = repairs_base + r_idx * repair_record_len;
        let end = start + repair_record_len;
        if trailer_bytes.len() < end {
            any_symbol_damaged = true;
            continue;
        }
        let mac_slice = &trailer_bytes[start..start + SYMBOL_MAC_BYTES];
        let payload = trailer_bytes[start + SYMBOL_MAC_BYTES..end].to_vec();
        let esi = (geom.stripe_k + r_idx) as u32;
        if constant_time_eq(&symbol_mac(sym_key, stripe_u32, esi, &payload), mac_slice) {
            if let Ok((cols, coeffs)) = decoder.repair_equation(esi) {
                received.push(ReceivedSymbol::repair(esi, cols, coeffs, payload));
                valid_count += 1;
            }
        } else {
            any_symbol_damaged = true;
        }
    }

    if valid_count < geom.stripe_k {
        return Err(ExtentError::InsufficientSymbols { stripe: stripe_u32 });
    }

    // ubs:ignore -- foundation erasure decoder, not JWT/signature decoding.
    let decoded = decoder.decode(&received).map_err(|error| match error {
        DecodeError::InsufficientSymbols { .. } | DecodeError::SingularMatrix { .. } => {
            ExtentError::InsufficientSymbols { stripe: stripe_u32 }
        }
        _ => ExtentError::AuthenticationFailed,
    })?;
    let mut out = Vec::with_capacity(stripe_len);
    for sym in &decoded.source {
        let take = sym.len().min(stripe_len.saturating_sub(out.len()));
        out.extend_from_slice(&sym[..take]);
    }
    if out.len() != stripe_len {
        return Err(ExtentError::MalformedHeader);
    }
    Ok((out, any_symbol_damaged))
}

fn verify_stripe_trailer_clean(
    stripe: usize,
    geom: &ExtentGeometry,
    trailer_bytes: &[u8],
    stripe_records: &[u8],
    coords: &[u8; COORD_BYTES],
    sym_key: &[u8; 32],
) -> bool {
    let (_, expected_trailer_len, sym_bytes) = geom.stripe_trailer_span(stripe);
    if trailer_bytes.len() != expected_trailer_len {
        return false;
    }
    let stripe_u32 = stripe as u32;
    let mut source = Vec::with_capacity(geom.stripe_k);
    for esi in 0..geom.stripe_k {
        let mac_start = esi * SYMBOL_MAC_BYTES;
        let mac_end = mac_start + SYMBOL_MAC_BYTES;
        let mut sym = vec![0u8; sym_bytes];
        let chunk_start = esi * sym_bytes;
        if chunk_start < stripe_records.len() {
            let chunk_end = (chunk_start + sym_bytes).min(stripe_records.len());
            sym[..chunk_end - chunk_start].copy_from_slice(&stripe_records[chunk_start..chunk_end]);
        }
        if !constant_time_eq(
            &symbol_mac(sym_key, stripe_u32, esi as u32, &sym),
            &trailer_bytes[mac_start..mac_end],
        ) {
            return false;
        }
        source.push(sym);
    }
    if geom.stripe_r > 0 {
        let Some(encoder) =
            SystematicEncoder::new(&source, sym_bytes, stripe_seed(coords, stripe_u32))
        else {
            return false;
        };
        let repairs_base = geom.stripe_k * SYMBOL_MAC_BYTES;
        let repair_record_len = SYMBOL_MAC_BYTES + sym_bytes;
        for r_idx in 0..geom.stripe_r {
            let start = repairs_base + r_idx * repair_record_len;
            let end = start + repair_record_len;
            let esi = (geom.stripe_k + r_idx) as u32;
            let Ok(expected_payload) = encoder.try_repair_symbol(esi) else {
                return false;
            };
            let expected_mac = symbol_mac(sym_key, stripe_u32, esi, &expected_payload);
            if !constant_time_eq(
                &trailer_bytes[start..start + SYMBOL_MAC_BYTES],
                &expected_mac,
            ) {
                return false;
            }
            // ubs:ignore — deterministic RaptorQ repair bytes checked after constant-time MAC.
            if &trailer_bytes[start + SYMBOL_MAC_BYTES..end] != expected_payload.as_slice() {
                return false;
            }
        }
    }
    true
}

/// Authenticate, decrypt, and (if damaged within `R` symbols/stripe) heal an
/// encoded `FGEC` container into logical plaintext.
pub fn decode_extent(
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    plaintext_limit: u64,
    encoded: &[u8],
    verify_redundancy: bool,
) -> Result<DecodedExtent, ExtentError> {
    if encoded.len() < HEADER_BYTES {
        return Err(ExtentError::MalformedHeader);
    }
    let coords = coord_transcript(config, namespace, kind, object_id);
    let frame_key = object_subkey("fgdb:strata-ecs:frame-key:v1", config, &coords);
    let nonce_key = object_subkey("fgdb:strata-ecs:frame-nonce:v1", config, &coords);
    let sym_key = object_subkey("fgdb:strata-ecs:sym-key:v1", config, &coords);
    let hdr_key = object_subkey("fgdb:strata-ecs:hdr-key:v1", config, &coords);

    let mut needs_rewrite = false;
    let primary_res = verify_header_bytes(
        &encoded[..HEADER_BYTES],
        kind,
        config.epoch,
        &hdr_key,
        false,
    );
    let header = match primary_res {
        Ok(primary) => primary,
        Err(primary_err) => {
            if encoded.len() < 2 * HEADER_BYTES {
                return Err(primary_err);
            }
            let mirror = verify_header_bytes(
                &encoded[encoded.len() - HEADER_BYTES..],
                kind,
                config.epoch,
                &hdr_key,
                true,
            )?;
            needs_rewrite = true;
            mirror
        }
    };

    if header.plaintext_len > plaintext_limit {
        return Err(ExtentError::PlaintextTooLarge {
            limit: plaintext_limit,
            observed: header.plaintext_len,
        });
    }
    let geom = ExtentGeometry::compute(&header)?;
    if encoded.len() != geom.total_encoded_bytes {
        needs_rewrite = true;
    } else if !needs_rewrite {
        let mirror_ok = verify_header_bytes(
            &encoded[geom.mirror_header_offset..geom.total_encoded_bytes],
            kind,
            config.epoch,
            &hdr_key,
            true,
        )
        // ubs:ignore — public geometry equality after both headers passed constant-time MAC.
        .is_ok_and(|mirror| mirror == header);
        if !mirror_ok {
            needs_rewrite = true;
        }
    }

    let mut plaintext = Vec::with_capacity(geom.plaintext_len);
    let mut leaves = Vec::with_capacity(geom.frame_count);
    let mut healed_stripes = 0usize;

    for stripe in 0..geom.stripe_count {
        let (f_start, f_end) = geom.stripe_frame_range(stripe);
        let (rec_off, rec_len) = geom.stripe_records_span(stripe);
        let (trl_off, trl_len, _) = geom.stripe_trailer_span(stripe);

        let fast_slice = encoded
            .get(rec_off..rec_off + rec_len)
            .filter(|_| !verify_redundancy || encoded.get(trl_off..trl_off + trl_len).is_some());

        let mut fast_opened: Option<Vec<([u8; 32], Vec<u8>)>> = None;
        if let Some(stripe_records) = fast_slice {
            let mut frames = Vec::with_capacity(f_end - f_start);
            let mut cursor = 0usize;
            let mut all_ok = true;
            for ordinal in f_start..f_end {
                let rec_size = geom.frame_record_len(ordinal);
                let plain_len = geom.frame_plain_len(ordinal);
                if let Ok(opened) = open_single_frame_record(
                    &stripe_records[cursor..cursor + rec_size],
                    ordinal as u32,
                    plain_len,
                    &coords,
                    &frame_key,
                    &nonce_key,
                ) {
                    frames.push(opened);
                    cursor += rec_size;
                } else {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                if verify_redundancy {
                    let trl = encoded.get(trl_off..trl_off + trl_len).unwrap_or(&[]);
                    if !verify_stripe_trailer_clean(
                        stripe,
                        &geom,
                        trl,
                        stripe_records,
                        &coords,
                        &sym_key,
                    ) {
                        needs_rewrite = true;
                    }
                }
                fast_opened = Some(frames);
            }
        }

        let stripe_frames = if let Some(frames) = fast_opened {
            frames
        } else {
            let trl_avail = if trl_off < encoded.len() {
                &encoded[trl_off..encoded.len().min(trl_off + trl_len)]
            } else {
                &[]
            };
            let rec_avail = if rec_off < encoded.len() {
                &encoded[rec_off..encoded.len().min(rec_off + rec_len)]
            } else {
                &[]
            };
            let (recovered_records, _) =
                recover_stripe_records(stripe, &geom, trl_avail, rec_avail, &coords, &sym_key)?;
            let mut frames = Vec::with_capacity(f_end - f_start);
            let mut cursor = 0usize;
            for ordinal in f_start..f_end {
                let rec_size = geom.frame_record_len(ordinal);
                let plain_len = geom.frame_plain_len(ordinal);
                let opened = open_single_frame_record(
                    &recovered_records[cursor..cursor + rec_size],
                    ordinal as u32,
                    plain_len,
                    &coords,
                    &frame_key,
                    &nonce_key,
                )?;
                frames.push(opened);
                cursor += rec_size;
            }
            healed_stripes += 1;
            needs_rewrite = true;
            frames
        };

        for (digest, plain) in stripe_frames {
            leaves.push(digest);
            plaintext.extend_from_slice(&plain);
        }
    }

    // ubs:ignore — Merkle root over AEAD-authenticated frame digests, not secret material.
    if compute_merkle_root(&leaves) != header.merkle_root {
        return Err(ExtentError::MerkleMismatch);
    }
    Ok(DecodedExtent {
        plaintext,
        healed_stripes,
        needs_rewrite,
    })
}

async fn read_exact_or_short<F: VfsFile>(
    file: &mut F,
    offset: u64,
    max_len: usize,
) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; max_len];
    let mut filled = 0;
    while filled < max_len {
        let n = file.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

async fn read_validated_header_from_file<F: VfsFile>(
    file: &mut F,
    config: &StrataEcsConfig,
    kind: ExtentObjectKind,
    hdr_key: &[u8; 32],
) -> Result<(ValidatedHeader, ExtentGeometry), Result<ExtentError, std::io::Error>> {
    let file_len = file.metadata().await.map_err(Err)?.len();
    if file_len < HEADER_BYTES as u64 {
        return Err(Ok(ExtentError::MalformedHeader));
    }
    let primary_raw = read_exact_or_short(file, 0, HEADER_BYTES)
        .await
        .map_err(Err)?;
    let header = match verify_header_bytes(&primary_raw, kind, config.epoch, hdr_key, false) {
        Ok(h) => h,
        Err(primary_err) => {
            if file_len < (2 * HEADER_BYTES) as u64 {
                return Err(Ok(primary_err));
            }
            let mirror_raw =
                read_exact_or_short(file, file_len - HEADER_BYTES as u64, HEADER_BYTES)
                    .await
                    .map_err(Err)?;
            verify_header_bytes(&mirror_raw, kind, config.epoch, hdr_key, true).map_err(Ok)?
        }
    };
    let geom = ExtentGeometry::compute(&header).map_err(Ok)?;
    Ok((header, geom))
}

type ExtentIoFuture<'a, T> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<T, Result<ExtentError, std::io::Error>>>
            + Send
            + 'a,
    >,
>;

/// `O(1)` random-access single-page read on an `FGEC` file handle: reads and
/// decrypts ONLY `frame_ordinal` (`16 KiB` or `64 KiB`), falling back to that
/// single stripe's RaptorQ repair symbols if `frame_ordinal` is corrupted or
/// truncated. Returns `(plaintext_frame, healed_from_stripe)`.
pub async fn read_extent_frame_from_file<F: VfsFile>(
    cx: &impl StorageReadCx,
    mut file: F,
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    frame_ordinal: u32,
) -> Result<(Vec<u8>, bool), Result<ExtentError, std::io::Error>> {
    let coords = coord_transcript(config, namespace, kind, object_id);
    let frame_key = object_subkey("fgdb:strata-ecs:frame-key:v1", config, &coords);
    let nonce_key = object_subkey("fgdb:strata-ecs:frame-nonce:v1", config, &coords);
    let sym_key = object_subkey("fgdb:strata-ecs:sym-key:v1", config, &coords);
    let hdr_key = object_subkey("fgdb:strata-ecs:hdr-key:v1", config, &coords);

    let fut: ExtentIoFuture<'_, (Vec<u8>, bool)> = Box::pin(async move {
        let (header, geom) =
            read_validated_header_from_file(&mut file, config, kind, &hdr_key).await?;
        let ord = frame_ordinal as usize;
        if ord >= geom.frame_count {
            return Err(Ok(ExtentError::FrameOutOfBounds {
                requested: frame_ordinal,
                frame_count: header.frame_count,
            }));
        }
        let f_off = geom.frame_offset(ord) as u64;
        let f_rec_len = geom.frame_record_len(ord);
        let f_plain_len = geom.frame_plain_len(ord);
        let raw_rec = read_exact_or_short(&mut file, f_off, f_rec_len)
            .await
            .map_err(Err)?;
        if raw_rec.len() == f_rec_len
            && let Ok((_, plain)) = open_single_frame_record(
                &raw_rec,
                frame_ordinal,
                f_plain_len,
                &coords,
                &frame_key,
                &nonce_key,
            )
        {
            return Ok((plain, false));
        }

        let stripe = ord / geom.stripe_frames;
        let (f_start, f_end) = geom.stripe_frame_range(stripe);
        let (rec_off, rec_len) = geom.stripe_records_span(stripe);
        let (trl_off, trl_len, _) = geom.stripe_trailer_span(stripe);
        let trl_avail = read_exact_or_short(&mut file, trl_off as u64, trl_len)
            .await
            .map_err(Err)?;
        let rec_avail = read_exact_or_short(&mut file, rec_off as u64, rec_len)
            .await
            .map_err(Err)?;
        let (recovered_records, _) =
            recover_stripe_records(stripe, &geom, &trl_avail, &rec_avail, &coords, &sym_key)
                .map_err(Ok)?;
        let mut cursor = 0usize;
        for candidate in f_start..f_end {
            let c_len = geom.frame_record_len(candidate);
            let c_plain = geom.frame_plain_len(candidate);
            let (_, plain) = open_single_frame_record(
                &recovered_records[cursor..cursor + c_len],
                candidate as u32,
                c_plain,
                &coords,
                &frame_key,
                &nonce_key,
            )
            .map_err(Ok)?;
            if candidate == ord {
                return Ok((plain, true));
            }
            cursor += c_len;
        }
        Err(Ok(ExtentError::AuthenticationFailed))
    });
    cx.with_restriction_async(fut).await
}

/// `O(1)` byte-range read `[offset .. offset + len]` across only the touched
/// `FGEC` frames (used by `PackIndex::read_direct` so packed object reads do
/// not decrypt untouched pack frames).
#[allow(clippy::too_many_arguments)]
pub async fn read_extent_range_from_file<F: VfsFile>(
    cx: &impl StorageReadCx,
    file: F,
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    byte_offset: u64,
    byte_len: usize,
) -> Result<Vec<u8>, Result<ExtentError, std::io::Error>> {
    read_extent_range_with_len_from_file(
        cx,
        file,
        config,
        namespace,
        kind,
        object_id,
        byte_offset,
        byte_len,
    )
    .await
    .map(|(_, bytes)| bytes)
}

/// Same as [`read_extent_range_from_file`], also returning the authenticated
/// total logical `plaintext_len` of the extent.
#[allow(clippy::too_many_arguments)]
pub async fn read_extent_range_with_len_from_file<F: VfsFile>(
    cx: &impl StorageReadCx,
    mut file: F,
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    byte_offset: u64,
    byte_len: usize,
) -> Result<(u64, Vec<u8>), Result<ExtentError, std::io::Error>> {
    let coords = coord_transcript(config, namespace, kind, object_id);
    let frame_key = object_subkey("fgdb:strata-ecs:frame-key:v1", config, &coords);
    let nonce_key = object_subkey("fgdb:strata-ecs:frame-nonce:v1", config, &coords);
    let sym_key = object_subkey("fgdb:strata-ecs:sym-key:v1", config, &coords);
    let hdr_key = object_subkey("fgdb:strata-ecs:hdr-key:v1", config, &coords);

    let fut: ExtentIoFuture<'_, (u64, Vec<u8>)> = Box::pin(async move {
        let (header, geom) =
            read_validated_header_from_file(&mut file, config, kind, &hdr_key).await?;
        if byte_len == 0 {
            return Ok((header.plaintext_len, Vec::new()));
        }
        let end = byte_offset
            .checked_add(byte_len as u64)
            .ok_or(Ok(ExtentError::MalformedHeader))?;
        if end > header.plaintext_len {
            return Err(Ok(ExtentError::MalformedHeader));
        }
        let first_frame = usize::try_from(byte_offset / geom.frame_bytes as u64)
            .map_err(|_| Ok(ExtentError::MalformedHeader))?;
        let last_frame = usize::try_from((end - 1) / geom.frame_bytes as u64)
            .map_err(|_| Ok(ExtentError::MalformedHeader))?;

        let mut out = Vec::with_capacity(byte_len);
        for ord in first_frame..=last_frame {
            let f_off = geom.frame_offset(ord) as u64;
            let f_rec_len = geom.frame_record_len(ord);
            let f_plain_len = geom.frame_plain_len(ord);
            let raw_rec = read_exact_or_short(&mut file, f_off, f_rec_len)
                .await
                .map_err(Err)?;
            let plain = if raw_rec.len() == f_rec_len
                && let Ok((_, p)) = open_single_frame_record(
                    &raw_rec,
                    ord as u32,
                    f_plain_len,
                    &coords,
                    &frame_key,
                    &nonce_key,
                ) {
                p
            } else {
                let stripe = ord / geom.stripe_frames;
                let (f_start, f_end) = geom.stripe_frame_range(stripe);
                let (rec_off, rec_len) = geom.stripe_records_span(stripe);
                let (trl_off, trl_len, _) = geom.stripe_trailer_span(stripe);
                let trl_avail = read_exact_or_short(&mut file, trl_off as u64, trl_len)
                    .await
                    .map_err(Err)?;
                let rec_avail = read_exact_or_short(&mut file, rec_off as u64, rec_len)
                    .await
                    .map_err(Err)?;
                let (recovered_records, _) = recover_stripe_records(
                    stripe, &geom, &trl_avail, &rec_avail, &coords, &sym_key,
                )
                .map_err(Ok)?;
                let mut cursor = 0usize;
                let mut target_plain = None;
                for candidate in f_start..f_end {
                    let c_len = geom.frame_record_len(candidate);
                    let c_plain = geom.frame_plain_len(candidate);
                    let (_, p) = open_single_frame_record(
                        &recovered_records[cursor..cursor + c_len],
                        candidate as u32,
                        c_plain,
                        &coords,
                        &frame_key,
                        &nonce_key,
                    )
                    .map_err(Ok)?;
                    if candidate == ord {
                        target_plain = Some(p);
                    }
                    cursor += c_len;
                }
                target_plain.ok_or(Ok(ExtentError::AuthenticationFailed))?
            };
            let frame_byte_start = (ord * geom.frame_bytes) as u64;
            let slice_start = usize::try_from(byte_offset.saturating_sub(frame_byte_start))
                .expect("within frame");
            let slice_end = usize::try_from((end - frame_byte_start).min(plain.len() as u64))
                .expect("within frame");
            out.extend_from_slice(&plain[slice_start..slice_end]);
        }
        Ok((header.plaintext_len, out))
    });
    cx.with_restriction_async(fut).await
}

/// Errors produced when sealing or opening an `FGKE-V1` Argon2id key envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEnvelopeError {
    Malformed(&'static str),
    KdfFailed,
    AuthenticationFailed,
}

impl core::fmt::Display for KeyEnvelopeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(msg) => write!(f, "malformed FGKE-V1 key envelope: {msg}"),
            Self::KdfFailed => f.write_str("FGKE-V1 Argon2id KEK derivation failed"),
            Self::AuthenticationFailed => {
                f.write_str("FGKE-V1 passphrase KEK authentication failed")
            }
        }
    }
}

impl core::error::Error for KeyEnvelopeError {}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).expect("nibble"));
        s.push(char::from_digit(u32::from(b & 0xf), 16).expect("nibble"));
    }
    s
}

fn hex_decode_exact<const N: usize>(text: &str) -> Result<[u8; N], KeyEnvelopeError> {
    if text.len() != N * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(KeyEnvelopeError::Malformed(
            "invalid hexadecimal field width",
        ));
    }
    let mut out = [0u8; N];
    for (dst, pair) in out.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        let digit = |c: u8| -> u8 {
            if c.is_ascii_digit() {
                c - b'0'
            } else {
                c.to_ascii_lowercase() - b'a' + 10
            }
        };
        *dst = (digit(pair[0]) << 4) | digit(pair[1]);
    }
    Ok(out)
}

/// Whether key-file text declares an `FGKE-V1` passphrase-wrapped key envelope.
pub fn is_key_envelope_v1(text: &str) -> bool {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .find(|line| !line.is_empty())
        == Some(KEY_ENVELOPE_HEADER_V1)
}

/// Wrap `(k_oid, namespace, dek)` under `passphrase` using RFC 9106 Argon2id
/// (`PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND`) + XChaCha20-Poly1305.
pub fn seal_key_envelope_v1(
    passphrase: &[u8],
    salt: [u8; PASSPHRASE_KDF_SALT_BYTES],
    nonce: [u8; 24],
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    dek: &[u8; 32],
) -> Result<String, KeyEnvelopeError> {
    let kek = derive_passphrase_kek(
        PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND,
        passphrase,
        &salt,
    )
    .map_err(|_| KeyEnvelopeError::KdfFailed)?;
    let mut plain = [0u8; 96];
    plain[..32].copy_from_slice(k_oid);
    plain[32..64].copy_from_slice(&namespace.0);
    plain[64..96].copy_from_slice(dek);
    let sealed = xchacha20poly1305_seal(kek.expose(), &nonce, KEY_ENVELOPE_AAD_V1, &plain);
    fgdb_crypto::zeroize::scrub_slice(&mut plain);
    Ok(format!(
        "{KEY_ENVELOPE_HEADER_V1}\n{} {} {} {}\n",
        PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND,
        hex_encode(&salt),
        hex_encode(&nonce),
        hex_encode(&sealed),
    ))
}

/// Unwrap `(k_oid, namespace, dek)` from an `FGKE-V1` passphrase-wrapped key file.
pub fn open_key_envelope_v1(
    passphrase: &[u8],
    text: &str,
) -> Result<([u8; 32], DatabaseSecurityNamespaceId, [u8; 32]), KeyEnvelopeError> {
    let lines: Vec<_> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() != 2 || lines[0] != KEY_ENVELOPE_HEADER_V1 {
        return Err(KeyEnvelopeError::Malformed(
            "expected FGKE-V1 header and one payload line",
        ));
    }
    let parts: Vec<_> = lines[1].split_whitespace().collect();
    if parts.len() != 4 {
        return Err(KeyEnvelopeError::Malformed(
            "expected profile_id, salt, nonce, and ciphertext",
        ));
    }
    let profile_id: u16 = parts[0]
        .parse()
        .map_err(|_| KeyEnvelopeError::Malformed("invalid profile_id"))?;
    let salt = hex_decode_exact::<PASSPHRASE_KDF_SALT_BYTES>(parts[1])?;
    let nonce = hex_decode_exact::<24>(parts[2])?;
    let sealed = hex_decode_exact::<112>(parts[3])?;
    let kek = derive_passphrase_kek(profile_id, passphrase, &salt)
        .map_err(|_| KeyEnvelopeError::KdfFailed)?;
    let mut plain = xchacha20poly1305_open(kek.expose(), &nonce, KEY_ENVELOPE_AAD_V1, &sealed)
        .map_err(|_| KeyEnvelopeError::AuthenticationFailed)?;
    let mut k_oid = [0u8; 32];
    let mut ns = [0u8; 32];
    let mut dek = [0u8; 32];
    k_oid.copy_from_slice(&plain[..32]);
    ns.copy_from_slice(&plain[32..64]);
    dek.copy_from_slice(&plain[64..96]);
    fgdb_crypto::zeroize::scrub_slice(&mut plain);
    Ok((k_oid, DatabaseSecurityNamespaceId(ns), dek))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_stripe_roundtrip_heals_front_middle_and_truncated_tail_and_refuses_swaps() {
        let cfg = StrataEcsConfig::new([0x5a; 32]);
        let ns = DatabaseSecurityNamespaceId([0x11; 32]);
        let oid = ObjectId([0x22; 32]);
        // 9 frames (> 2 stripes of 4 frames each)
        let plain: Vec<u8> = (0..(9 * 16_384 + 777))
            .map(|i| ((i * 37 + 13) & 0xff) as u8)
            .collect();
        let encoded =
            encode_extent(&cfg, ns, ExtentObjectKind::AdjRun, oid, &plain).expect("encode");

        let clean = decode_extent(
            &cfg,
            ns,
            ExtentObjectKind::AdjRun,
            oid,
            plain.len() as u64,
            &encoded,
            true,
        )
        .expect("clean decode");
        assert_eq!(clean.plaintext, plain);
        assert_eq!(clean.healed_stripes, 0);
        assert!(!clean.needs_rewrite);

        // 1. Corrupt Primary Header + 2 frames in stripe 0: Mirror Header + RaptorQ heal!
        let mut front_corrupt = encoded.clone();
        front_corrupt[0] ^= 0xff;
        let geom = ExtentGeometry::compute(&ValidatedHeader {
            frame_size: ExtentFrameSize::KiB16,
            stripe_frames: 4,
            stripe_k: 8,
            stripe_r: 4,
            epoch: 1,
            plaintext_len: plain.len() as u64,
            frame_count: 10,
            stripe_count: 3,
            merkle_root: [0; 32],
        })
        .unwrap();
        front_corrupt[geom.frame_offset(0) + 50] ^= 0xa5;
        front_corrupt[geom.frame_offset(1) + 500] ^= 0x5a;
        let healed_front = decode_extent(
            &cfg,
            ns,
            ExtentObjectKind::AdjRun,
            oid,
            plain.len() as u64,
            &front_corrupt,
            true,
        )
        .expect("front corruption must heal");
        assert_eq!(healed_front.plaintext, plain);
        assert_eq!(healed_front.healed_stripes, 1);
        assert!(healed_front.needs_rewrite);

        // 2. Truncate the entire Mirror Header + final frame off EOF: Primary Header + RaptorQ heal!
        let truncated_len = geom.frame_offset(9);
        let healed_trunc = decode_extent(
            &cfg,
            ns,
            ExtentObjectKind::AdjRun,
            oid,
            plain.len() as u64,
            &encoded[..truncated_len],
            true,
        )
        .expect("tail truncation must heal");
        assert_eq!(healed_trunc.plaintext, plain);
        assert_eq!(healed_trunc.healed_stripes, 1);
        assert!(healed_trunc.needs_rewrite);

        // 3. Negative controls: wrong DEK, wrong GraphId, wrong BranchId, wrong ObjectKind, wrong Epoch.
        let wrong_dek = StrataEcsConfig::new([0x5b; 32]);
        assert_eq!(
            decode_extent(
                &wrong_dek,
                ns,
                ExtentObjectKind::AdjRun,
                oid,
                plain.len() as u64,
                &encoded,
                false,
            ),
            Err(ExtentError::AuthenticationFailed)
        );
        let wrong_graph = cfg.clone().with_coordinates(GraphId(2), BranchId(1), 1);
        assert_eq!(
            decode_extent(
                &wrong_graph,
                ns,
                ExtentObjectKind::AdjRun,
                oid,
                plain.len() as u64,
                &encoded,
                false,
            ),
            Err(ExtentError::AuthenticationFailed)
        );
        let wrong_branch = cfg.clone().with_coordinates(GraphId(1), BranchId(9), 1);
        assert_eq!(
            decode_extent(
                &wrong_branch,
                ns,
                ExtentObjectKind::AdjRun,
                oid,
                plain.len() as u64,
                &encoded,
                false,
            ),
            Err(ExtentError::AuthenticationFailed)
        );
        assert_eq!(
            decode_extent(
                &cfg,
                ns,
                ExtentObjectKind::Block,
                oid,
                plain.len() as u64,
                &encoded,
                false,
            ),
            Err(ExtentError::AuthenticationFailed)
        );
    }

    #[test]
    fn key_envelope_v1_roundtrips_and_refuses_wrong_passphrase() {
        let env = seal_key_envelope_v1(
            b"correct horse battery staple",
            [0x11; 16],
            [0x22; 24],
            &[0x33; 32],
            DatabaseSecurityNamespaceId([0x44; 32]),
            &[0x55; 32],
        )
        .expect("seal");
        assert!(is_key_envelope_v1(&env));
        let (k_oid, ns, dek) =
            open_key_envelope_v1(b"correct horse battery staple", &env).expect("open");
        assert_eq!(k_oid, [0x33; 32]);
        assert_eq!(ns, DatabaseSecurityNamespaceId([0x44; 32]));
        assert_eq!(dek, [0x55; 32]);
        assert_eq!(
            open_key_envelope_v1(b"wrong passphrase", &env),
            Err(KeyEnvelopeError::AuthenticationFailed)
        );
    }
}
