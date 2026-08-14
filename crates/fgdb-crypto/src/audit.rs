//! External-crypto-audit evidence and the release-admission interlock.
//!
//! The crypto bead requires an independent review and a release-blocking
//! external audit.  This module makes the *blocking* part executable: a release
//! candidate pins the exact source, registered profile set, approved auditor,
//! report, and attestation digests; the supplied audit artifact must cover the
//! complete registered methodology, conclude `Accepted`, attest independence,
//! and carry no unresolved finding at any severity.
//!
//! This is deliberately not an assertion that an audit has happened.  Code
//! cannot establish an auditor's competence, independence, or signature trust
//! root by inspecting a digest.  G4 owns those external trust decisions and
//! must put their approved digests into the release candidate.  This module
//! only makes substitution, omission, stale-scope reuse, and unresolved
//! findings fail closed once those decisions exist.

use crate::aead::ObjectAeadProfile;
use crate::argon2id::{PassphraseKdfProfile, Variant};
use crate::blake3::{Hasher, hash};
use crate::{
    SYMBOL_AUTH_DOMAIN, SYMBOL_AUTH_KEY_BYTES, SYMBOL_AUTH_PROFILE_BLAKE3_128,
    SYMBOL_AUTH_TAG_BYTES,
};
use core::fmt;

const ARTIFACT_MAGIC: [u8; 8] = *b"FGCAUD01";
const PLAN_DIGEST_DOMAIN: &[u8] = b"fgdb:crypto-external-audit-plan:v1";
const ARTIFACT_DIGEST_DOMAIN: &[u8] = b"fgdb:crypto-external-audit-artifact:v1";
const PROFILE_SET_DIGEST_DOMAIN: &[u8] = b"fgdb:registered-crypto-profile-set:v1";

/// The strict canonical artifact schema understood by this release interlock.
pub const EXTERNAL_CRYPTO_AUDIT_SCHEMA_VERSION: u16 = 1;

/// The fixed-width canonical artifact length.
pub const EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN: usize = 222;

/// Canonical schema version for the registered crypto-profile-set transcript.
pub const REGISTERED_CRYPTO_PROFILE_SET_SCHEMA_VERSION: u16 = 1;

/// Exact number of V1 crypto profiles bound into release-audit evidence.
pub const REGISTERED_CRYPTO_PROFILE_COUNT: u16 = 3;

/// Stable registered engagement-plan identity.
pub const EXTERNAL_CRYPTO_AUDIT_ENGAGEMENT_ID: &str = "fgdb.crypto.external-audit.v1";

/// The owner that must keep the engagement plan and evidence lane current.
pub const EXTERNAL_CRYPTO_AUDIT_OWNER_BEAD: &str = "fgdb-w1-crypto-y5o";

/// The release gate that supplies the approved external trust decisions.
pub const EXTERNAL_CRYPTO_AUDIT_RELEASE_GATE: &str = "fgdb-gate-g4-3uc";

/// One required external-audit methodology class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum AuditMethod {
    /// Published primitive/profile vectors and independent oracle comparison.
    PrimitiveAndProfileVectors = 1 << 0,
    /// Statistical timing-leak measurement under a named host methodology.
    StatisticalTiming = 1 << 1,
    /// Compiled/source inspection for secret-dependent control flow/addressing.
    SecretControlFlowAudit = 1 << 2,
    /// Nonce, AAD, tag, identity, and cross-encoding misuse resistance.
    MisuseResistance = 1 << 3,
    /// Drop-path and owned-storage zeroization evidence.
    Zeroization = 1 << 4,
    /// Production entropy separation and incident-artifact redaction.
    EntropyAndRedaction = 1 << 5,
    /// End-to-end identity/AEAD/FEC-symbol/key-lifecycle composition review.
    CompositionAndKeyLifecycle = 1 << 6,
}

impl AuditMethod {
    /// The canonical bit assigned to this methodology class.
    #[must_use]
    pub const fn bit(self) -> u16 {
        self as u16
    }
}

const KNOWN_AUDIT_METHOD_BITS: u16 = AuditMethod::PrimitiveAndProfileVectors.bit()
    | AuditMethod::StatisticalTiming.bit()
    | AuditMethod::SecretControlFlowAudit.bit()
    | AuditMethod::MisuseResistance.bit()
    | AuditMethod::Zeroization.bit()
    | AuditMethod::EntropyAndRedaction.bit()
    | AuditMethod::CompositionAndKeyLifecycle.bit();

/// A closed set of external-audit methodology classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCoverage(u16);

impl AuditCoverage {
    /// The complete methodology required for crypto release admission.
    pub const REQUIRED: Self = Self(KNOWN_AUDIT_METHOD_BITS);

    /// Construct a coverage set, rejecting unknown future bits.
    pub const fn try_from_bits(bits: u16) -> Result<Self, CryptoAuditError> {
        let unknown = bits & !KNOWN_AUDIT_METHOD_BITS;
        if unknown == 0 {
            Ok(Self(bits))
        } else {
            Err(CryptoAuditError::UnknownCoverageBits(unknown))
        }
    }

    /// The canonical bit representation.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether every registered methodology class is present.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.0 & KNOWN_AUDIT_METHOD_BITS == KNOWN_AUDIT_METHOD_BITS
    }

    /// Which registered methodology bits are absent.
    #[must_use]
    pub const fn missing_required_bits(self) -> u16 {
        KNOWN_AUDIT_METHOD_BITS & !self.0
    }
}

/// The registered external-audit engagement plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalCryptoAuditEngagementPlan {
    /// Stable plan identity.
    pub id: &'static str,
    /// Owning Bead.
    pub owner_bead: &'static str,
    /// Release gate that must pin external trust artifacts.
    pub release_gate: &'static str,
    /// Exact required methodology coverage.
    pub required_coverage: AuditCoverage,
}

/// The one registered external-crypto-audit engagement plan.
pub const REGISTERED_EXTERNAL_CRYPTO_AUDIT_PLAN: ExternalCryptoAuditEngagementPlan =
    ExternalCryptoAuditEngagementPlan {
        id: EXTERNAL_CRYPTO_AUDIT_ENGAGEMENT_ID,
        owner_bead: EXTERNAL_CRYPTO_AUDIT_OWNER_BEAD,
        release_gate: EXTERNAL_CRYPTO_AUDIT_RELEASE_GATE,
        required_coverage: AuditCoverage::REQUIRED,
    };

/// Digest of the exact registered engagement plan and required methodology.
#[must_use]
pub fn external_crypto_audit_plan_digest() -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(PLAN_DIGEST_DOMAIN);
    update_len_prefixed(&mut hasher, EXTERNAL_CRYPTO_AUDIT_ENGAGEMENT_ID.as_bytes());
    update_len_prefixed(&mut hasher, EXTERNAL_CRYPTO_AUDIT_OWNER_BEAD.as_bytes());
    update_len_prefixed(&mut hasher, EXTERNAL_CRYPTO_AUDIT_RELEASE_GATE.as_bytes());
    hasher.update(&EXTERNAL_CRYPTO_AUDIT_SCHEMA_VERSION.to_le_bytes());
    hasher.update(&AuditCoverage::REQUIRED.bits().to_le_bytes());
    hasher.finalize().0
}

/// Digest the exact closed crypto-profile inventory implemented by this crate.
///
/// The transcript is deliberately derived from the public profile rows rather
/// than supplied by a release caller.  Changing an algorithm, parameter,
/// width, domain, profile ID, row order, or row count therefore invalidates
/// old external-audit evidence.  The three V1 rows are object AEAD,
/// passphrase KDF, and symbol authentication; a future signing profile must be
/// added here and necessarily changes this digest.
#[must_use]
pub fn registered_crypto_profile_set_digest() -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(PROFILE_SET_DIGEST_DOMAIN);
    hasher.update(&REGISTERED_CRYPTO_PROFILE_SET_SCHEMA_VERSION.to_le_bytes());
    hasher.update(&REGISTERED_CRYPTO_PROFILE_COUNT.to_le_bytes());

    let aead = ObjectAeadProfile::XChaCha20Poly1305V1;
    update_profile_header(&mut hasher, 1, aead.id(), b"xchacha20-poly1305-ietf");
    hasher.update(&32_u16.to_le_bytes());
    hasher.update(&aead.nonce_len().to_le_bytes());
    hasher.update(&aead.tag_len().to_le_bytes());

    let kdf = PassphraseKdfProfile::Argon2idRfc9106SecondV1.spec();
    update_profile_header(&mut hasher, 2, kdf.profile_id, b"argon2id");
    hasher.update(&kdf.argon2_version.to_le_bytes());
    let variant_code = match kdf.variant {
        Variant::Argon2d => 0_u32,
        Variant::Argon2i => 1_u32,
        Variant::Argon2id => 2_u32,
    };
    hasher.update(&variant_code.to_le_bytes());
    hasher.update(&kdf.params.memory_kib.to_le_bytes());
    hasher.update(&kdf.params.passes.to_le_bytes());
    hasher.update(&kdf.params.lanes.to_le_bytes());
    hasher.update(&kdf.salt_len.to_le_bytes());
    hasher.update(&kdf.kek_len.to_le_bytes());

    update_profile_header(
        &mut hasher,
        3,
        SYMBOL_AUTH_PROFILE_BLAKE3_128,
        b"blake3-keyed-128",
    );
    update_len_prefixed(&mut hasher, SYMBOL_AUTH_DOMAIN.as_bytes());
    hasher.update(&SYMBOL_AUTH_KEY_BYTES.to_le_bytes());
    hasher.update(&SYMBOL_AUTH_TAG_BYTES.to_le_bytes());

    hasher.finalize().0
}

fn update_profile_header(hasher: &mut Hasher, class_tag: u16, profile_id: u16, algorithm: &[u8]) {
    hasher.update(&class_tag.to_le_bytes());
    hasher.update(&profile_id.to_le_bytes());
    update_len_prefixed(hasher, algorithm);
}

fn update_len_prefixed(hasher: &mut Hasher, bytes: &[u8]) {
    let length = u32::try_from(bytes.len()).expect("registered audit-plan fields fit u32");
    hasher.update(&length.to_le_bytes());
    hasher.update(bytes);
}

/// External auditor's report conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuditConclusion {
    /// The audited scope is accepted under the report's stated methodology.
    Accepted = 1,
    /// The audited scope is rejected and cannot release.
    Rejected = 2,
}

impl AuditConclusion {
    fn try_from_tag(tag: u8) -> Result<Self, CryptoAuditError> {
        match tag {
            1 => Ok(Self::Accepted),
            2 => Ok(Self::Rejected),
            other => Err(CryptoAuditError::UnknownConclusion(other)),
        }
    }
}

/// Unresolved findings grouped by the four release-significant severities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditFindingCounts {
    /// Unresolved critical findings.
    pub critical: u32,
    /// Unresolved high findings.
    pub high: u32,
    /// Unresolved medium findings.
    pub medium: u32,
    /// Unresolved low findings.
    pub low: u32,
}

impl AuditFindingCounts {
    /// Whether the accepted report has no unresolved finding at any severity.
    #[must_use]
    pub const fn is_clear(self) -> bool {
        self.critical == 0 && self.high == 0 && self.medium == 0 && self.low == 0
    }
}

/// Exact release-candidate pins supplied by G4's external trust decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoReleaseCandidate {
    source_revision_digest: [u8; 32],
    profile_set_digest: [u8; 32],
    approved_auditor_identity_digest: [u8; 32],
    approved_report_digest: [u8; 32],
    approved_attestation_digest: [u8; 32],
}

impl CryptoReleaseCandidate {
    /// Build exact candidate pins. Zero is never a valid external artifact ID.
    ///
    /// The registered profile-set digest is not caller-selectable: it is
    /// derived from the live closed profile rows so two mutually echoing stale
    /// inputs cannot manufacture release admission.
    pub fn try_new(
        source_revision_digest: [u8; 32],
        approved_auditor_identity_digest: [u8; 32],
        approved_report_digest: [u8; 32],
        approved_attestation_digest: [u8; 32],
    ) -> Result<Self, CryptoAuditError> {
        require_nonzero("source_revision_digest", source_revision_digest)?;
        require_nonzero(
            "approved_auditor_identity_digest",
            approved_auditor_identity_digest,
        )?;
        require_nonzero("approved_report_digest", approved_report_digest)?;
        require_nonzero("approved_attestation_digest", approved_attestation_digest)?;
        Ok(Self {
            source_revision_digest,
            profile_set_digest: registered_crypto_profile_set_digest(),
            approved_auditor_identity_digest,
            approved_report_digest,
            approved_attestation_digest,
        })
    }

    /// Exact source revision admitted for release.
    #[must_use]
    pub const fn source_revision_digest(self) -> [u8; 32] {
        self.source_revision_digest
    }

    /// Exact registered profile-set manifest admitted for release.
    #[must_use]
    pub const fn profile_set_digest(self) -> [u8; 32] {
        self.profile_set_digest
    }
}

/// Canonical external-audit evidence. It contains digests, never report bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCryptoAuditEvidence {
    plan_digest: [u8; 32],
    conclusion: AuditConclusion,
    auditor_independence_attested: bool,
    coverage: AuditCoverage,
    findings: AuditFindingCounts,
    auditor_identity_digest: [u8; 32],
    auditor_attestation_digest: [u8; 32],
    report_digest: [u8; 32],
    source_revision_digest: [u8; 32],
    profile_set_digest: [u8; 32],
}

impl ExternalCryptoAuditEvidence {
    /// Construct a report-bound artifact. Release policy is evaluated
    /// separately so rejected/incomplete reports remain representable evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        conclusion: AuditConclusion,
        auditor_independence_attested: bool,
        coverage: AuditCoverage,
        findings: AuditFindingCounts,
        auditor_identity_digest: [u8; 32],
        auditor_attestation_digest: [u8; 32],
        report_digest: [u8; 32],
        source_revision_digest: [u8; 32],
        profile_set_digest: [u8; 32],
    ) -> Result<Self, CryptoAuditError> {
        require_nonzero("auditor_identity_digest", auditor_identity_digest)?;
        require_nonzero("auditor_attestation_digest", auditor_attestation_digest)?;
        require_nonzero("report_digest", report_digest)?;
        require_nonzero("source_revision_digest", source_revision_digest)?;
        require_nonzero("profile_set_digest", profile_set_digest)?;
        Ok(Self {
            plan_digest: external_crypto_audit_plan_digest(),
            conclusion,
            auditor_independence_attested,
            coverage,
            findings,
            auditor_identity_digest,
            auditor_attestation_digest,
            report_digest,
            source_revision_digest,
            profile_set_digest,
        })
    }

    /// Strict fixed-width canonical encoding for release ledgers.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN);
        out.extend_from_slice(&ARTIFACT_MAGIC);
        out.extend_from_slice(&EXTERNAL_CRYPTO_AUDIT_SCHEMA_VERSION.to_le_bytes());
        out.push(self.conclusion as u8);
        out.push(u8::from(self.auditor_independence_attested));
        out.extend_from_slice(&self.coverage.bits().to_le_bytes());
        for count in [
            self.findings.critical,
            self.findings.high,
            self.findings.medium,
            self.findings.low,
        ] {
            out.extend_from_slice(&count.to_le_bytes());
        }
        for digest in [
            self.plan_digest,
            self.auditor_identity_digest,
            self.auditor_attestation_digest,
            self.report_digest,
            self.source_revision_digest,
            self.profile_set_digest,
        ] {
            out.extend_from_slice(&digest);
        }
        debug_assert_eq!(out.len(), EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN);
        out
    }

    /// Decode and validate the strict canonical artifact.
    pub fn try_from_canonical_bytes(bytes: &[u8]) -> Result<Self, CryptoAuditError> {
        if bytes.len() != EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN {
            return Err(CryptoAuditError::InvalidArtifactLength {
                expected: EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN,
                actual: bytes.len(),
            });
        }
        if bytes[..ARTIFACT_MAGIC.len()] != ARTIFACT_MAGIC {
            return Err(CryptoAuditError::InvalidArtifactMagic);
        }
        let mut cursor = ARTIFACT_MAGIC.len();
        let schema_version = read_u16(bytes, &mut cursor);
        if schema_version != EXTERNAL_CRYPTO_AUDIT_SCHEMA_VERSION {
            return Err(CryptoAuditError::UnsupportedSchemaVersion(schema_version));
        }
        let conclusion = AuditConclusion::try_from_tag(read_u8(bytes, &mut cursor))?;
        let auditor_independence_attested = match read_u8(bytes, &mut cursor) {
            0 => false,
            1 => true,
            other => return Err(CryptoAuditError::InvalidBoolean(other)),
        };
        let coverage = AuditCoverage::try_from_bits(read_u16(bytes, &mut cursor))?;
        let findings = AuditFindingCounts {
            critical: read_u32(bytes, &mut cursor),
            high: read_u32(bytes, &mut cursor),
            medium: read_u32(bytes, &mut cursor),
            low: read_u32(bytes, &mut cursor),
        };
        let plan_digest = read_digest(bytes, &mut cursor);
        if plan_digest != external_crypto_audit_plan_digest() {
            return Err(CryptoAuditError::EngagementPlanMismatch);
        }
        let auditor_identity_digest = read_digest(bytes, &mut cursor);
        let auditor_attestation_digest = read_digest(bytes, &mut cursor);
        let report_digest = read_digest(bytes, &mut cursor);
        let source_revision_digest = read_digest(bytes, &mut cursor);
        let profile_set_digest = read_digest(bytes, &mut cursor);
        debug_assert_eq!(cursor, bytes.len());

        Self::try_new(
            conclusion,
            auditor_independence_attested,
            coverage,
            findings,
            auditor_identity_digest,
            auditor_attestation_digest,
            report_digest,
            source_revision_digest,
            profile_set_digest,
        )
    }

    /// Domain-separated digest used by the release ledger.
    #[must_use]
    pub fn evidence_digest(&self) -> [u8; 32] {
        let bytes = self.to_canonical_bytes();
        let mut transcript = Vec::with_capacity(ARTIFACT_DIGEST_DOMAIN.len() + bytes.len());
        transcript.extend_from_slice(ARTIFACT_DIGEST_DOMAIN);
        transcript.extend_from_slice(&bytes);
        hash(&transcript).0
    }

    /// Report conclusion.
    #[must_use]
    pub const fn conclusion(&self) -> AuditConclusion {
        self.conclusion
    }

    /// Registered methodology coverage.
    #[must_use]
    pub const fn coverage(&self) -> AuditCoverage {
        self.coverage
    }

    /// Unresolved finding counts.
    #[must_use]
    pub const fn findings(&self) -> AuditFindingCounts {
        self.findings
    }
}

/// Successful release admission, binding the candidate to exact audit bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoReleaseAdmission {
    source_revision_digest: [u8; 32],
    profile_set_digest: [u8; 32],
    audit_evidence_digest: [u8; 32],
}

impl CryptoReleaseAdmission {
    /// Exact admitted source revision.
    #[must_use]
    pub const fn source_revision_digest(self) -> [u8; 32] {
        self.source_revision_digest
    }

    /// Exact admitted registered profile-set manifest.
    #[must_use]
    pub const fn profile_set_digest(self) -> [u8; 32] {
        self.profile_set_digest
    }

    /// Digest of the canonical external-audit evidence artifact.
    #[must_use]
    pub const fn audit_evidence_digest(self) -> [u8; 32] {
        self.audit_evidence_digest
    }
}

/// Admit a release only under exact, complete, accepted external-audit evidence.
pub fn admit_external_crypto_audit(
    candidate: &CryptoReleaseCandidate,
    evidence: Option<&ExternalCryptoAuditEvidence>,
) -> Result<CryptoReleaseAdmission, CryptoAuditError> {
    let evidence = evidence.ok_or(CryptoAuditError::MissingAuditEvidence)?;
    if evidence.conclusion != AuditConclusion::Accepted {
        return Err(CryptoAuditError::AuditRejected);
    }
    if !evidence.auditor_independence_attested {
        return Err(CryptoAuditError::IndependenceNotAttested);
    }
    if !evidence.coverage.is_complete() {
        return Err(CryptoAuditError::IncompleteCoverage(
            evidence.coverage.missing_required_bits(),
        ));
    }
    if !evidence.findings.is_clear() {
        return Err(CryptoAuditError::UnresolvedFindings(evidence.findings));
    }
    if evidence.source_revision_digest != candidate.source_revision_digest {
        return Err(CryptoAuditError::SourceRevisionMismatch);
    }
    if evidence.profile_set_digest != candidate.profile_set_digest {
        return Err(CryptoAuditError::ProfileSetMismatch);
    }
    if evidence.auditor_identity_digest != candidate.approved_auditor_identity_digest {
        return Err(CryptoAuditError::AuditorIdentityMismatch);
    }
    if evidence.report_digest != candidate.approved_report_digest {
        return Err(CryptoAuditError::ReportMismatch);
    }
    if evidence.auditor_attestation_digest != candidate.approved_attestation_digest {
        return Err(CryptoAuditError::AttestationMismatch);
    }
    Ok(CryptoReleaseAdmission {
        source_revision_digest: candidate.source_revision_digest,
        profile_set_digest: candidate.profile_set_digest,
        audit_evidence_digest: evidence.evidence_digest(),
    })
}

/// Typed construction, codec, and release-admission errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoAuditError {
    /// A required digest is the all-zero sentinel.
    ZeroDigest(&'static str),
    /// Coverage carries unregistered bits.
    UnknownCoverageBits(u16),
    /// Canonical bytes have the wrong fixed length.
    InvalidArtifactLength { expected: usize, actual: usize },
    /// Canonical magic is wrong.
    InvalidArtifactMagic,
    /// Canonical schema version is unsupported.
    UnsupportedSchemaVersion(u16),
    /// Conclusion tag is outside the closed vocabulary.
    UnknownConclusion(u8),
    /// A canonical boolean is not zero or one.
    InvalidBoolean(u8),
    /// Artifact names a different engagement plan.
    EngagementPlanMismatch,
    /// No external-audit artifact was supplied.
    MissingAuditEvidence,
    /// The report rejected the audited scope.
    AuditRejected,
    /// Auditor independence was not attested.
    IndependenceNotAttested,
    /// One or more required methodology classes are absent.
    IncompleteCoverage(u16),
    /// At least one finding remains unresolved.
    UnresolvedFindings(AuditFindingCounts),
    /// Artifact covers a different source revision.
    SourceRevisionMismatch,
    /// Artifact covers a different registered profile set.
    ProfileSetMismatch,
    /// Artifact comes from a different auditor than G4 approved.
    AuditorIdentityMismatch,
    /// Artifact names a different report than G4 approved.
    ReportMismatch,
    /// Artifact names a different attestation than G4 approved.
    AttestationMismatch,
}

impl fmt::Display for CryptoAuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDigest(field) => write!(f, "{field} must not be the all-zero sentinel"),
            Self::UnknownCoverageBits(bits) => {
                write!(f, "audit coverage contains unknown bits {bits:#06x}")
            }
            Self::InvalidArtifactLength { expected, actual } => write!(
                f,
                "external-audit artifact has {actual} bytes; expected {expected}"
            ),
            Self::InvalidArtifactMagic => f.write_str("external-audit artifact magic is invalid"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "external-audit schema version {version} is unsupported")
            }
            Self::UnknownConclusion(tag) => {
                write!(f, "external-audit conclusion tag {tag} is unknown")
            }
            Self::InvalidBoolean(value) => {
                write!(f, "external-audit boolean byte {value} is noncanonical")
            }
            Self::EngagementPlanMismatch => {
                f.write_str("external-audit artifact names a different engagement plan")
            }
            Self::MissingAuditEvidence => f.write_str("external-audit evidence is missing"),
            Self::AuditRejected => f.write_str("external audit did not accept the release scope"),
            Self::IndependenceNotAttested => {
                f.write_str("external auditor independence is not attested")
            }
            Self::IncompleteCoverage(bits) => {
                write!(f, "external audit is missing methodology bits {bits:#06x}")
            }
            Self::UnresolvedFindings(counts) => write!(
                f,
                "external audit has unresolved findings: critical={}, high={}, medium={}, low={}",
                counts.critical, counts.high, counts.medium, counts.low
            ),
            Self::SourceRevisionMismatch => {
                f.write_str("external audit covers a different source revision")
            }
            Self::ProfileSetMismatch => {
                f.write_str("external audit covers a different registered profile set")
            }
            Self::AuditorIdentityMismatch => {
                f.write_str("external audit comes from an unapproved auditor identity")
            }
            Self::ReportMismatch => f.write_str("external audit report digest is not approved"),
            Self::AttestationMismatch => {
                f.write_str("external audit attestation digest is not approved")
            }
        }
    }
}

impl std::error::Error for CryptoAuditError {}

fn require_nonzero(field: &'static str, digest: [u8; 32]) -> Result<(), CryptoAuditError> {
    if digest == [0; 32] {
        Err(CryptoAuditError::ZeroDigest(field))
    } else {
        Ok(())
    }
}

fn read_u8(bytes: &[u8], cursor: &mut usize) -> u8 {
    let value = bytes[*cursor];
    *cursor += 1;
    value
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> u16 {
    let mut value = [0; 2];
    value.copy_from_slice(&bytes[*cursor..*cursor + 2]);
    *cursor += 2;
    u16::from_le_bytes(value)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[*cursor..*cursor + 4]);
    *cursor += 4;
    u32::from_le_bytes(value)
}

fn read_digest(bytes: &[u8], cursor: &mut usize) -> [u8; 32] {
    let mut digest = [0; 32];
    digest.copy_from_slice(&bytes[*cursor..*cursor + 32]);
    *cursor += 32;
    digest
}
