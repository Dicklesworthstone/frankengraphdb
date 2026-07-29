//! The durable capsule container: a committed object stored as RaptorQ symbols.
//!
//! Doctrine 5 says every durable object but `manifest.root` is immutable,
//! content-addressed and erasure-coded, and that this is what removes the need
//! for a double-write journal: **RaptorQ heals torn and corrupt symbols**, so
//! there is nothing to roll back to. Writing capsule plaintext straight to a
//! file would be a different mechanism with the same shape — a substitute for
//! the final abstraction rather than a subset of it (doctrine 7) — and it would
//! quietly withdraw the justification for having no journal.
//!
//! So a capsule on disk is: a self-describing header, then the authenticated
//! symbol records that `symbolize` produced.
//!
//! ```text
//!   plaintext ─▶ IdentifiedObject ─▶ ProtectedObject ─▶ EncodedObject ─▶ symbols
//!                    (ObjectId)        (CiphertextId)     (EncodingId)
//! ```
//!
//! THE HEADER IS NOT TRUSTED. It is *checked*. Recovery rebuilds the encoding
//! from the header's declared descriptors and requires the declared
//! `EncodingId` to recompute from them, then requires the recovered plaintext
//! to recompute the `ObjectId` **the caller asked for** — which comes from the
//! commit marker, not from the file. A rewritten header therefore cannot
//! redirect recovery at other bytes: it can only fail. That is the same
//! discipline `RootBootstrap` uses for the root, applied to ordinary objects.
//!
//! WHAT THE ERASURE BUDGET BUYS. `repair_symbols` extra symbols mean any
//! `repair_symbols` of them may be lost or corrupt and the object still
//! recovers; one more than that and recovery fails closed rather than returning
//! partial bytes. Corrupt is as good as lost here because every symbol carries
//! a MAC under a per-encoding key: a damaged symbol fails authentication and is
//! refused *before* it can enter the linear system, so it subtracts from the
//! budget instead of poisoning the result.

use crate::identity::{
    CipherDescriptor, EncodedObject, EncodingDescriptor, IdentifiedObject, IdentityMismatch,
};
use crate::symbolize::{RecoveryTarget, SymbolizeError, decode_object, encode_object};
use fgdb_crypto::Digest;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

/// Container magic, so a truncated or foreign file is refused by its first four
/// bytes rather than by a confusing failure deeper in.
pub const CAPSULE_MAGIC: [u8; 4] = *b"FGCP";

/// Container format version (§16.6: durable formats are versioned from day one).
pub const CAPSULE_FORMAT_V1: u16 = 1;

/// A capsule's self-sufficient descriptor: everything recovery needs to rebuild
/// the object's identity from the file alone.
///
/// Flat fields rather than the three descriptor structs because this is a
/// durable frame: the structs are the in-memory shape and may gain private
/// invariants, while these bytes must stay readable by a version that has never
/// seen them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleDescriptor {
    pub object_kind: u16,
    pub canonical_plaintext_len: u64,
    pub codec_profile: u16,
    pub compressed_len: u64,
    pub data_crypto_profile: u16,
    pub dek_id: [u8; 16],
    pub object_nonce: [u8; 24],
    pub object_tag_len: u16,
    pub ciphertext_id: [u8; 32],
    pub fec_profile: u16,
    pub transfer_length: u64,
    pub oti_common: u64,
    pub oti_scheme: u32,
    pub symbol_size: u16,
    pub source_block_count: u16,
    pub symbol_auth_profile: u16,
    pub encoding_id: [u8; 32],
    /// Length of the sealed (ciphertext + tag) bytes. Fixes K and trims the
    /// final symbol's padding, and comes from the authenticated descriptor
    /// rather than from any individual symbol — a symbol never authorizes
    /// itself.
    pub protected_len: u64,
    pub repair_symbols: u32,
}

impl CapsuleDescriptor {
    pub fn cipher_descriptor(&self) -> CipherDescriptor {
        CipherDescriptor {
            object_kind: self.object_kind,
            canonical_plaintext_len: self.canonical_plaintext_len,
            codec_profile: self.codec_profile,
            compressed_len: self.compressed_len,
            data_crypto_profile: self.data_crypto_profile,
            dek_id: self.dek_id,
            object_nonce: self.object_nonce,
            object_tag_len: self.object_tag_len,
        }
    }

    pub fn encoding_descriptor(&self) -> EncodingDescriptor {
        EncodingDescriptor {
            fec_profile: self.fec_profile,
            transfer_length: self.transfer_length,
            oti_common: self.oti_common,
            oti_scheme: self.oti_scheme,
            symbol_size: self.symbol_size,
            source_block_count: self.source_block_count,
            symbol_auth_profile: self.symbol_auth_profile,
        }
    }

    /// How many symbols may be lost or corrupt and still recover.
    pub fn erasure_budget(&self) -> usize {
        self.repair_symbols as usize
    }
}

/// The coding profile a capsule is written under.
///
/// `repair_symbols` is priced by the caller because the plan makes the repair
/// budget a policy decision per reconstructibility tier, not a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsuleProfile {
    pub symbol_size: u16,
    pub repair_symbols: u32,
}

impl CapsuleProfile {
    /// A profile that survives losing a quarter of a small object's symbols.
    /// Deliberately explicit rather than a `Default`: a durability parameter
    /// nobody chose is a durability parameter nobody reviewed.
    pub const fn balanced() -> Self {
        Self {
            symbol_size: 256,
            repair_symbols: 8,
        }
    }
}

/// Why sealing or recovering a capsule failed.
#[derive(Debug)]
pub enum CapsuleError {
    Io(std::io::Error),
    /// The container's framing is not a capsule this reader understands.
    MalformedContainer,
    UnsupportedFormat {
        format: u16,
    },
    /// A declared identity does not recompute from its descriptors — the frame
    /// was rewritten.
    DescriptorMismatch(IdentityMismatch),
    /// Recovery failed: too many symbols lost or corrupt, or the recovered
    /// bytes are not the object that was asked for.
    Recovery(SymbolizeError),
}

impl core::fmt::Display for CapsuleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "capsule I/O failed: {error}"),
            Self::MalformedContainer => write!(f, "not a capsule container"),
            Self::UnsupportedFormat { format } => {
                write!(f, "unsupported capsule format version {format}")
            }
            Self::DescriptorMismatch(mismatch) => {
                write!(
                    f,
                    "capsule descriptors are not self-consistent: {mismatch:?}"
                )
            }
            Self::Recovery(error) => write!(f, "capsule recovery failed: {error:?}"),
        }
    }
}

impl core::error::Error for CapsuleError {}

impl From<std::io::Error> for CapsuleError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// A capsule ready to be written: its identity, its descriptor, and its
/// authenticated symbols.
#[derive(Debug, Clone)]
pub struct SealedCapsule {
    pub object_id: ObjectId,
    pub descriptor: CapsuleDescriptor,
    pub symbols: Vec<Vec<u8>>,
}

/// Run the §5.1 pipeline over `plaintext` and erasure-code the result.
///
/// The object id is DERIVED here rather than accepted, so a caller cannot name
/// one object and store another.
pub fn seal(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    dek: &[u8; 32],
    object_kind: u16,
    plaintext: &[u8],
    profile: CapsuleProfile,
) -> Result<SealedCapsule, CapsuleError> {
    let identified = IdentifiedObject::new(k_oid, namespace, object_kind, &[], plaintext);
    let object_id = identified.object_id();

    // No compression yet: the codec profile is `fgdb-codec`'s to choose, and
    // declaring a profile we do not apply would make the descriptor lie.
    let cipher = CipherDescriptor {
        object_kind,
        canonical_plaintext_len: plaintext.len() as u64,
        codec_profile: 0,
        compressed_len: plaintext.len() as u64,
        data_crypto_profile: 1,
        dek_id: [0u8; 16],
        object_nonce: derive_nonce(object_id),
        object_tag_len: 16,
    };
    let protected = identified.protect(dek, cipher.clone(), plaintext);
    let protected_len = protected.protected_bytes().len();

    let encoding_descriptor = EncodingDescriptor {
        fec_profile: 1,
        transfer_length: protected_len as u64,
        oti_common: 0,
        oti_scheme: 0,
        symbol_size: profile.symbol_size,
        source_block_count: 1,
        symbol_auth_profile: 1,
    };
    let encoded = protected.encode(encoding_descriptor.clone());
    let symbols = encode_object(
        &encoded,
        protected.protected_bytes(),
        object_kind,
        0,
        profile.repair_symbols,
        dek,
    )
    .map_err(CapsuleError::Recovery)?;

    Ok(SealedCapsule {
        object_id,
        descriptor: CapsuleDescriptor {
            object_kind,
            canonical_plaintext_len: cipher.canonical_plaintext_len,
            codec_profile: cipher.codec_profile,
            compressed_len: cipher.compressed_len,
            data_crypto_profile: cipher.data_crypto_profile,
            dek_id: cipher.dek_id,
            object_nonce: cipher.object_nonce,
            object_tag_len: cipher.object_tag_len,
            ciphertext_id: protected.ciphertext_id().0,
            fec_profile: encoding_descriptor.fec_profile,
            transfer_length: encoding_descriptor.transfer_length,
            oti_common: encoding_descriptor.oti_common,
            oti_scheme: encoding_descriptor.oti_scheme,
            symbol_size: encoding_descriptor.symbol_size,
            source_block_count: encoding_descriptor.source_block_count,
            symbol_auth_profile: encoding_descriptor.symbol_auth_profile,
            encoding_id: encoded.encoding_id().0,
            protected_len: protected_len as u64,
            repair_symbols: profile.repair_symbols,
        },
        symbols,
    })
}

/// A per-object nonce derived from its identity.
///
/// Deterministic on purpose: the same plaintext under the same key must seal to
/// the same bytes, or a content-addressed store would hold two different
/// encodings of one object and deduplication could never fire. Safe here
/// because the identity already binds the plaintext — two objects with the same
/// nonce would have to be the same object.
fn derive_nonce(object_id: ObjectId) -> [u8; 24] {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(b"fgdb:capsule-nonce:v1");
    hasher.update(&object_id.0);
    let digest = hasher.finalize();
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&digest.0[..24]);
    nonce
}

/// Serialize a sealed capsule into its container bytes.
pub fn encode_container(sealed: &SealedCapsule) -> Vec<u8> {
    let d = &sealed.descriptor;
    let mut out = Vec::new();
    out.extend_from_slice(&CAPSULE_MAGIC);
    out.extend_from_slice(&CAPSULE_FORMAT_V1.to_be_bytes());
    out.extend_from_slice(&d.object_kind.to_be_bytes());
    out.extend_from_slice(&d.canonical_plaintext_len.to_be_bytes());
    out.extend_from_slice(&d.codec_profile.to_be_bytes());
    out.extend_from_slice(&d.compressed_len.to_be_bytes());
    out.extend_from_slice(&d.data_crypto_profile.to_be_bytes());
    out.extend_from_slice(&d.dek_id);
    out.extend_from_slice(&d.object_nonce);
    out.extend_from_slice(&d.object_tag_len.to_be_bytes());
    out.extend_from_slice(&d.ciphertext_id);
    out.extend_from_slice(&d.fec_profile.to_be_bytes());
    out.extend_from_slice(&d.transfer_length.to_be_bytes());
    out.extend_from_slice(&d.oti_common.to_be_bytes());
    out.extend_from_slice(&d.oti_scheme.to_be_bytes());
    out.extend_from_slice(&d.symbol_size.to_be_bytes());
    out.extend_from_slice(&d.source_block_count.to_be_bytes());
    out.extend_from_slice(&d.symbol_auth_profile.to_be_bytes());
    out.extend_from_slice(&d.encoding_id);
    out.extend_from_slice(&d.protected_len.to_be_bytes());
    out.extend_from_slice(&d.repair_symbols.to_be_bytes());
    out.extend_from_slice(&(sealed.symbols.len() as u32).to_be_bytes());
    for symbol in &sealed.symbols {
        out.extend_from_slice(&(symbol.len() as u32).to_be_bytes());
        out.extend_from_slice(symbol);
    }
    out
}

/// Parse a container back into its descriptor and symbols.
///
/// A symbol that is short or malformed is DROPPED rather than failing the
/// parse, because that is exactly the damage the erasure code exists to absorb:
/// refusing to parse would turn a recoverable object into an unrecoverable one.
/// The decision about whether enough survived belongs to the decoder, which is
/// the only thing that can tell.
pub fn decode_container(bytes: &[u8]) -> Result<(CapsuleDescriptor, Vec<Vec<u8>>), CapsuleError> {
    let mut r = Reader::new(bytes);
    if r.take(4)? != CAPSULE_MAGIC {
        return Err(CapsuleError::MalformedContainer);
    }
    let format = r.u16()?;
    if format != CAPSULE_FORMAT_V1 {
        return Err(CapsuleError::UnsupportedFormat { format });
    }
    let descriptor = CapsuleDescriptor {
        object_kind: r.u16()?,
        canonical_plaintext_len: r.u64()?,
        codec_profile: r.u16()?,
        compressed_len: r.u64()?,
        data_crypto_profile: r.u16()?,
        dek_id: r.array16()?,
        object_nonce: r.array24()?,
        object_tag_len: r.u16()?,
        ciphertext_id: r.array32()?,
        fec_profile: r.u16()?,
        transfer_length: r.u64()?,
        oti_common: r.u64()?,
        oti_scheme: r.u32()?,
        symbol_size: r.u16()?,
        source_block_count: r.u16()?,
        symbol_auth_profile: r.u16()?,
        encoding_id: r.array32()?,
        protected_len: r.u64()?,
        repair_symbols: r.u32()?,
    };
    let declared = r.u32()? as usize;
    let mut symbols = Vec::with_capacity(declared.min(4096));
    for _ in 0..declared {
        let Ok(len) = r.u32() else { break };
        let Ok(payload) = r.take(len as usize) else {
            break;
        };
        symbols.push(payload.to_vec());
    }
    Ok((descriptor, symbols))
}

/// Recover a capsule's plaintext, proving it is the object that was asked for.
///
/// `expected_object_id` comes from the commit marker, not from the container.
/// That is the whole point: the file describes itself, and the *stream* says
/// which object it must be.
pub fn recover(
    descriptor: &CapsuleDescriptor,
    symbols: &[Vec<u8>],
    expected_object_id: ObjectId,
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    dek: &[u8; 32],
) -> Result<Vec<u8>, CapsuleError> {
    // Step 1: the descriptor set must be self-consistent. An EncodingId that is
    // not the digest of its own descriptor means the frame was rewritten, and
    // no number of valid symbols makes that safe.
    let encoding = EncodedObject::reconstruct(
        expected_object_id,
        descriptor.cipher_descriptor(),
        Digest(descriptor.ciphertext_id),
        descriptor.encoding_descriptor(),
        Digest(descriptor.encoding_id),
    )
    .map_err(CapsuleError::DescriptorMismatch)?;

    // Step 2: DROP the symbols that do not authenticate.
    //
    // This is where doctrine 5's "RaptorQ heals corrupt symbols" actually
    // happens, and it has to be here rather than in `decode_object`. That
    // function is a strict decoder: it refuses a symbol that fails its MAC
    // instead of skipping it, which is the right contract for a decoder — a
    // caller handing it symbols it believes in deserves to hear that one is
    // rotten rather than have it silently ignored.
    //
    // The capsule layer is the one that does NOT believe in its input: it read
    // whatever was on disk, and some of it may have rotted. Filtering here is
    // what turns corruption into plain erasure, so a bit flip costs one symbol
    // of budget instead of destroying an object that had ample repair capacity.
    //
    // Symbols are verified twice as a result — once to decide membership, once
    // by the decoder as its own precondition. That is deliberate: recovery is
    // not a hot path, and the alternative is a decoder that trusts a caller's
    // filtering.
    let authentic: Vec<Vec<u8>> = symbols
        .iter()
        .filter(|bytes| crate::symbol::SymbolRecord::verify(bytes, &encoding, dek).is_ok())
        .cloned()
        .collect();

    // Steps 3-5: decode, open the AEAD, and recompute the keyed ObjectId.
    // `decode_object` owns that sequence and fails closed at each stage.
    decode_object(
        &encoding,
        &authentic,
        RecoveryTarget {
            k_oid,
            namespace,
            object_id: expected_object_id,
            // The capsule's protected bytes ARE its whole canonical plaintext,
            // so the header is not a separate recomputation input; passing it
            // again would hash it twice and nothing would ever recover.
            canonical_header: &[],
            protected_len: descriptor.protected_len as usize,
        },
        dek,
    )
    .map_err(CapsuleError::Recovery)
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CapsuleError> {
        let end = self
            .position
            .checked_add(len)
            .ok_or(CapsuleError::MalformedContainer)?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or(CapsuleError::MalformedContainer)?;
        self.position = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16, CapsuleError> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Result<u32, CapsuleError> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> Result<u64, CapsuleError> {
        let mut v = [0u8; 8];
        v.copy_from_slice(self.take(8)?);
        Ok(u64::from_be_bytes(v))
    }

    fn array16(&mut self) -> Result<[u8; 16], CapsuleError> {
        let mut v = [0u8; 16];
        v.copy_from_slice(self.take(16)?);
        Ok(v)
    }

    fn array24(&mut self) -> Result<[u8; 24], CapsuleError> {
        let mut v = [0u8; 24];
        v.copy_from_slice(self.take(24)?);
        Ok(v)
    }

    fn array32(&mut self) -> Result<[u8; 32], CapsuleError> {
        let mut v = [0u8; 32];
        v.copy_from_slice(self.take(32)?);
        Ok(v)
    }
}
