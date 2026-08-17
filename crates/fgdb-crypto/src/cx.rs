//! `CryptoCx`: where secret entropy comes from, and where it must never come
//! from.
//!
//! Increment 4b of bead fgdb-w1-crypto-y5o. The bead's requirement is one
//! sentence — "`CryptoCx` obtains production entropy independently of
//! deterministic replay seeds" — and the reason it needs its own type is a
//! hazard that is easy to walk into and invisible once you have.
//!
//! **THE HAZARD, REVERIFIED 2026-08-16 against pinned asupersync v0.4.6 source
//! revision (9f7c3769).** `Cx` exposes an entropy capability — `Cx::random_bytes`,
//! `Cx::random_u64` — over a `dyn EntropySource`. There are two production-
//! relevant implementations in `src/util/entropy.rs`: `OsEntropy` (getrandom,
//! `source_id` "os") and `DetEntropy`, the deterministic source the lab runtime
//! installs so that schedules replay. That is exactly right for a runtime whose
//! selling point is seed-replayable execution, and exactly wrong as a source of
//! key material: **under the lab runtime `Cx::random_bytes` is a function of the
//! replay seed.** A DEK drawn from it is reproducible by anyone holding the
//! seed, and B5 encourages seeds to be published in crashpacks and bug reports.
//!
//! So the rule this module exists to enforce is narrow and absolute:
//!
//! > Scheduling entropy and secret entropy are different capabilities. `Cx` is
//! > the right source for a jitter, a backoff, a tie-break, a nonce that only
//! > needs to be unique. It is never the source of a key.
//!
//! **WHY THIS IS NOT WIRED TO asupersync'S `OsEntropy` YET, which doctrine would
//! otherwise prefer.** `fgdb-crypto` currently has *zero* dependencies. Taking
//! asupersync would make this foundation crate depend on the runtime, and crate
//! activation here is a four-artifact change (`workspace_topology.toml`, its
//! generated pins, the docs, the topology tests). That is a real change owed to
//! its own increment, not a side effect of this one. Until then the production
//! source is the OS device directly through `std` — no new dependency, same
//! kernel CSPRNG asupersync's `getrandom` reaches, and a `source_id` that a test
//! can discriminate. When the asupersync edge lands, [`SystemEntropy`] is the
//! one place to re-point.
//!
//! **FAIL CLOSED, NEVER FALL BACK.** If the OS source cannot be read this
//! module returns an error. It does not fall back to a timestamp, a counter, or
//! a zero buffer. A weak key that looks like a key is worse than a failed
//! operation, because the failure is loud and the weak key is silent.

use crate::ObjectAeadProfile;
use crate::zeroize::Secret;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Why secret entropy could not be obtained.
///
/// Carries the source id but never the buffer: an error that quoted the partial
/// bytes it managed to read would put key material into a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntropyError {
    /// The OS entropy device could not be opened or read.
    Unavailable {
        source_id: &'static str,
        detail: String,
    },
    /// A test or replay source reached a production key-minting surface.
    NotApprovedForKeyMaterial { source_id: &'static str },
}

impl core::fmt::Display for EntropyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable { source_id, detail } => write!(
                f,
                "entropy source {source_id:?} is unavailable: {detail}; refusing to \
                 substitute a weaker source"
            ),
            Self::NotApprovedForKeyMaterial { source_id } => write!(
                f,
                "entropy source {source_id:?} is not approved for object key material"
            ),
        }
    }
}

impl core::error::Error for EntropyError {}

/// A freshly minted object DEK and the public descriptor material that belongs
/// to exactly one ciphertext.
///
/// The plan requires a random DEK per ciphertext. Keeping the three values in
/// one non-`Clone`, non-`Copy` authority prevents the easiest misuse: minting a
/// key in one place, then separately inventing or reusing its durable id and
/// nonce at the Chronicle call site. [`FreshObjectProtectionMaterial::use_once`]
/// consumes the authority, borrows the DEK only for the duration of one
/// closure, and scrubs it when that closure returns.
///
/// This type deliberately does not implement serialization. The DEK becomes
/// durable only through the typed `KeyWrap` machinery owned by W9; incident
/// artifacts and ordinary descriptors may carry `dek_id` and `object_nonce`,
/// never the key itself.
///
/// The authority cannot be cloned:
///
/// ```compile_fail
/// fn duplicate(material: fgdb_crypto::FreshObjectProtectionMaterial) {
///     let _copy = material.clone();
/// }
/// ```
///
/// And consuming it makes a second use a compile error:
///
/// ```compile_fail
/// fn reuse(material: fgdb_crypto::FreshObjectProtectionMaterial) {
///     material.use_once(|_| ());
///     material.use_once(|_| ());
/// }
/// ```
pub struct FreshObjectProtectionMaterial {
    profile: ObjectAeadProfile,
    dek: Secret<32>,
    dek_id: [u8; 16],
    object_nonce: [u8; 24],
}

impl FreshObjectProtectionMaterial {
    /// The registered object-AEAD profile this material was minted for.
    pub fn profile(&self) -> ObjectAeadProfile {
        self.profile
    }

    /// The public, random identity that durable `KeyWrap` and cipher
    /// descriptors use to refer to this DEK without exposing it.
    pub fn dek_id(&self) -> [u8; 16] {
        self.dek_id
    }

    /// The public XChaCha nonce for this one ciphertext.
    ///
    /// The nonce is not secret, but the crypto logging contract still forbids
    /// printing it. The hand-written [`Debug`] implementation therefore
    /// redacts it along with the DEK.
    pub fn object_nonce(&self) -> [u8; 24] {
        self.object_nonce
    }

    /// Consume this one-ciphertext authority and borrow its DEK for the exact
    /// operation that builds the protected object and its `KeyWrap`.
    ///
    /// A caller can always deliberately copy a borrowed key in Rust, so this
    /// is not claimed as an absolute non-copy theorem. It removes the ordinary
    /// reusable-key API shape: there is no `dek()` getter and the authority
    /// cannot be cloned or used by a second call after this one. A malicious
    /// closure can still copy or reuse its borrow; the W2/W9 integration must
    /// keep this closure scoped to one protect-plus-wrap operation.
    pub fn use_once<R>(self, use_material: impl FnOnce(ObjectProtectionMaterialRef<'_>) -> R) -> R {
        use_material(ObjectProtectionMaterialRef {
            profile: self.profile,
            dek: self.dek.expose(),
            dek_id: self.dek_id,
            object_nonce: self.object_nonce,
        })
    }
}

/// A closure-bounded view of fresh object protection material.
///
/// The key borrow cannot outlive the consumed
/// [`FreshObjectProtectionMaterial`]. Public descriptor fields are copied
/// because they are not secrets.
pub struct ObjectProtectionMaterialRef<'a> {
    profile: ObjectAeadProfile,
    dek: &'a [u8; 32],
    dek_id: [u8; 16],
    object_nonce: [u8; 24],
}

impl<'a> ObjectProtectionMaterialRef<'a> {
    pub fn profile(&self) -> ObjectAeadProfile {
        self.profile
    }

    pub fn dek(&self) -> &'a [u8; 32] {
        self.dek
    }

    pub fn dek_id(&self) -> [u8; 16] {
        self.dek_id
    }

    pub fn object_nonce(&self) -> [u8; 24] {
        self.object_nonce
    }
}

/// Redacted for the same reason as the owning authority: a closure must not
/// acquire a loggable view of the borrowed key or nonce.
impl core::fmt::Debug for ObjectProtectionMaterialRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ObjectProtectionMaterialRef(redacted)")
    }
}

/// Redacted: neither a DEK nor a nonce may reach a log through formatting.
impl core::fmt::Debug for FreshObjectProtectionMaterial {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("FreshObjectProtectionMaterial(redacted)")
    }
}

/// A source of bytes for secret material.
///
/// `source_id` is not decoration: it is the discriminator that makes the
/// entropy-separation law testable. Without it, "this `CryptoCx` is not seeded"
/// is an assertion about code someone read, rather than a property a test can
/// check.
pub trait EntropySource {
    /// Fill `dest` with entropy, or fail closed.
    fn fill(&self, dest: &mut [u8]) -> Result<(), EntropyError>;

    /// A stable identifier, reported in diagnostics and asserted by tests.
    fn source_id(&self) -> &'static str;

    /// Whether this source is reproducible from a seed.
    ///
    /// The one question a caller about to mint a key actually needs answered,
    /// and it is on the trait so no caller has to infer it from a type name.
    fn is_deterministic(&self) -> bool;
}

/// The OS CSPRNG. The only source legal for production key material.
#[derive(Debug, Clone)]
pub struct SystemEntropy {
    path: PathBuf,
    approved_for_key_material: bool,
}

impl SystemEntropy {
    /// The platform OS entropy device.
    ///
    /// Linux `/dev/urandom` — the plan's reference machine (§17) and the same
    /// kernel CSPRNG `getrandom` reaches. A platform without it gets an error
    /// from [`EntropySource::fill`], never a fallback.
    pub fn new() -> Self {
        SystemEntropy {
            path: PathBuf::from("/dev/urandom"),
            approved_for_key_material: true,
        }
    }

    /// Point the source at an arbitrary path.
    ///
    /// **FOR NEGATIVE TESTS ONLY**, and named to be unmistakable at the call
    /// site. It exists so the fail-closed path can be exercised (a missing
    /// device must error, not silently yield zeros); pointing production key
    /// generation at a file of your choosing is the exact abuse the name is
    /// meant to make obvious in review.
    pub fn from_path_for_test(path: impl AsRef<Path>) -> Self {
        SystemEntropy {
            path: path.as_ref().to_path_buf(),
            approved_for_key_material: false,
        }
    }
}

impl Default for SystemEntropy {
    fn default() -> Self {
        Self::new()
    }
}

impl EntropySource for SystemEntropy {
    fn fill(&self, dest: &mut [u8]) -> Result<(), EntropyError> {
        let mut file = File::open(&self.path).map_err(|error| EntropyError::Unavailable {
            source_id: "system",
            detail: format!("open {}: {error}", self.path.display()),
        })?;
        // read_exact, not read: a short read would leave the tail of a key as
        // whatever the buffer held, which is the quiet failure this whole
        // module is written to avoid.
        file.read_exact(dest)
            .map_err(|error| EntropyError::Unavailable {
                source_id: "system",
                detail: format!("read {}: {error}", self.path.display()),
            })
    }

    fn source_id(&self) -> &'static str {
        "system"
    }

    fn is_deterministic(&self) -> bool {
        false
    }
}

/// A seeded, fully reproducible source. **Never legal for key material.**
///
/// It exists for two honest reasons: to give the separation tests a control
/// that actually reproduces (otherwise "the production source is not seeded"
/// is untestable — every source looks unseeded if you never exhibit a seeded
/// one), and to stand in for asupersync's `DetEntropy` so the hazard this
/// module documents can be demonstrated rather than asserted.
#[derive(Debug, Clone)]
pub struct DeterministicEntropy {
    seed: u64,
}

impl DeterministicEntropy {
    /// Construct from a replay seed. The name of every item on this path says
    /// "test", because there is no legitimate production caller.
    pub fn for_test(seed: u64) -> Self {
        DeterministicEntropy { seed }
    }
}

impl EntropySource for DeterministicEntropy {
    fn fill(&self, dest: &mut [u8]) -> Result<(), EntropyError> {
        // A trivially reproducible stream keyed by the seed. Deliberately NOT a
        // cryptographic construction: nothing here should ever be mistaken for
        // one, and a weak generator makes the "never for keys" rule obvious.
        let mut state = self.seed;
        for byte in dest.iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *byte = (state >> 33) as u8;
        }
        Ok(())
    }

    fn source_id(&self) -> &'static str {
        "deterministic-test-only"
    }

    fn is_deterministic(&self) -> bool {
        true
    }
}

/// The capability handle for minting secrets.
///
/// Purpose-typed in the same spirit as `QueryCx`/`TxnCx` (doctrine #3): a
/// component that can mint keys says so in its signature, and one that only
/// needs a jitter takes a `Cx` instead and cannot reach this.
#[derive(Debug, Clone)]
pub struct CryptoCx<E: EntropySource> {
    entropy: E,
}

impl CryptoCx<SystemEntropy> {
    /// The production handle: OS entropy, independent of any replay seed.
    ///
    /// This is the constructor that satisfies the bead's entropy-separation
    /// requirement, and `production_entropy_is_never_seeded` is the test that
    /// holds it to it.
    pub fn production() -> Self {
        CryptoCx {
            entropy: SystemEntropy::new(),
        }
    }

    /// Mint the complete random material for one object ciphertext.
    ///
    /// This method exists only on `CryptoCx<SystemEntropy>`. A lab/replay
    /// `CryptoCx<DeterministicEntropy>` therefore cannot mint durable object
    /// protection material at compile time:
    ///
    /// ```compile_fail
    /// use fgdb_crypto::{CryptoCx, DeterministicEntropy, ObjectAeadProfile};
    /// let replay = CryptoCx::new(DeterministicEntropy::for_test(7));
    /// let _ = replay.fresh_object_protection_material(
    ///     ObjectAeadProfile::XChaCha20Poly1305V1,
    /// );
    /// ```
    ///
    /// The test-path override on [`SystemEntropy`] is also refused. That
    /// constructor remains useful for exercising read failures through
    /// [`CryptoCx::secret`], but cannot be repurposed to feed a file or replay
    /// stream into this production key-minting surface.
    pub fn fresh_object_protection_material(
        &self,
        profile: ObjectAeadProfile,
    ) -> Result<FreshObjectProtectionMaterial, EntropyError> {
        if !self.entropy.approved_for_key_material {
            return Err(EntropyError::NotApprovedForKeyMaterial {
                source_id: self.entropy.source_id(),
            });
        }

        // Mint the secret separately so it is inside a scrubbing wrapper from
        // its first initialized byte. If the following public-material read
        // fails, `dek` drops and scrubs before the error escapes.
        let dek = self.secret::<32>()?;
        let mut public_material = [0u8; 16 + 24];
        self.entropy.fill(&mut public_material)?;
        let mut dek_id = [0u8; 16];
        dek_id.copy_from_slice(&public_material[..16]);
        let mut object_nonce = [0u8; 24];
        object_nonce.copy_from_slice(&public_material[16..]);

        Ok(FreshObjectProtectionMaterial {
            profile,
            dek,
            dek_id,
            object_nonce,
        })
    }
}

impl<E: EntropySource> CryptoCx<E> {
    /// Wrap an explicit source.
    pub fn new(entropy: E) -> Self {
        CryptoCx { entropy }
    }

    /// Mint a fresh secret, which scrubs itself on drop.
    ///
    /// Returns [`Secret`] rather than a bare array so that key material cannot
    /// be minted into an unscrubbed buffer by accident — the two halves of
    /// increment 4 are deliberately joined here.
    pub fn secret<const N: usize>(&self) -> Result<Secret<N>, EntropyError> {
        let mut secret = Secret::<N>::zeroed();
        self.entropy.fill(secret.expose_mut())?;
        Ok(secret)
    }

    /// Fill a caller-owned buffer.
    pub fn fill_secret(&self, dest: &mut [u8]) -> Result<(), EntropyError> {
        self.entropy.fill(dest)
    }

    /// The source identifier, for the separation assertions and diagnostics.
    pub fn entropy_source_id(&self) -> &'static str {
        self.entropy.source_id()
    }

    /// Whether this handle's entropy is reproducible from a seed.
    pub fn is_deterministic(&self) -> bool {
        self.entropy.is_deterministic()
    }
}
