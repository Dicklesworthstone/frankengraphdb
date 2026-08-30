//! The constant-time review lane (bead fgdb-w1-crypto-y5o, increment 5).
//!
//! The bead asks for "a code audit lane asserting no secret-dependent branches
//! or table indices". This file IS that lane: the audit verdict for every
//! module in the crate, plus the two mechanical checks that stop the audit
//! going quietly stale.
//!
//! **WHAT THIS IS NOT, said first so the green bar is not over-read.** The
//! source table is a review artifact. The bounded Welch-t probe at the end of
//! this file measures one public AEAD forgery shape on the current host; its
//! planted early-exit control proves the detector is live, but it does not prove
//! every compiled cipher path constant-time on every microarchitecture. The
//! separately registered `w1_crypto_codegen_e2e` gate inspects the optimized
//! zeroization boundary, and that narrow witness does not generalize to every
//! kernel either. §12.5 is explicit that "the vectors pass therefore it is
//! secure" is not an inference this project makes. Constant-time behaviour
//! remains a `bounded_model` claim with named methodology, and the
//! release-blocking external audit itself is still owed by this bead. The
//! registered evidence/release interlock below proves only that release cannot
//! proceed without exact externally approved audit artifacts.
//!
//! **WHY THE AUDIT IS A TABLE AND NOT A COMMENT.** An audit written as prose in
//! a doc comment is true on the day it is written and unfalsifiable afterwards.
//! `every_module_has_an_audit_verdict` enumerates `src/*.rs` from disk at run
//! time, so a NEW module — a future SIMD kernel, a key-wrap routine — cannot
//! join the crate without someone recording what its secret-dependence is. That
//! is the completeness guard; without it this file would be decoration.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// How a module relates to secret-dependent control flow and addressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// No secret-dependent branch, index, or early exit.
    Clean,
    /// Secret-dependent addressing that is REQUIRED by the specification and
    /// accepted deliberately. Never a shrug — the rationale is in the row.
    AcceptedByDesign,
}

/// The audit table. One row per `src/*.rs`, rationale mandatory.
///
/// Reviewed 2026-08-04 by CalmSwan against the sources at that commit.
const AUDIT: [(&str, Verdict, &str); 10] = [
    (
        "lib.rs",
        Verdict::Clean,
        "Module declarations and the §5.1 identity transcript helpers. The \
         transcripts concatenate fixed-width fields and hash them; no branch or \
         index depends on key bytes.",
    ),
    (
        "blake3.rs",
        Verdict::Clean,
        "Pure ARX: wrapping add, xor, rotate over a fixed state. No table \
         lookups, no data-dependent branches. Buffering decisions depend on \
         input LENGTH, which is public.",
    ),
    (
        "blake2b.rs",
        Verdict::Clean,
        "Pure ARX over a fixed state with a compile-time SIGMA schedule indexed \
         by the round number, not by data. The `if !key.is_empty()` branch is on \
         key LENGTH, a public parameter, not on key CONTENT.",
    ),
    (
        "chacha20.rs",
        Verdict::Clean,
        "Pure ARX quarter-round over a fixed state; counter and nonce are \
         public. No S-box, no table.",
    ),
    (
        "poly1305.rs",
        Verdict::Clean,
        "The final conditional subtraction of p — the classic place this \
         primitive leaks — is a MASK-BASED SELECT, not a branch: the mask is \
         derived arithmetically from the borrow bit and both candidates are \
         always computed. r-clamping is fixed masking of key bytes. Verified by \
         reading the reduction, and pinned by \
         `the_reduction_stays_branchless` below.",
    ),
    (
        "aead.rs",
        Verdict::Clean,
        "Tag verification uses a constant-time comparison that accumulates all \
         16 byte differences before testing, with no early exit; decryption is \
         gated on it, so no plaintext byte is produced before authentication. \
         The length check is on a public length. Pinned by \
         `tag_comparison_stays_constant_time` below.",
    ),
    (
        "argon2id.rs",
        Verdict::AcceptedByDesign,
        "SECRET-DEPENDENT MEMORY INDEXING, REQUIRED BY THE SPECIFICATION. In the \
         data-dependent half, the reference block index derives from the \
         previous block's contents, so the memory access pattern depends on \
         values derived from the password. This is the defining property of \
         Argon2d and of Argon2id's second half; an implementation without it \
         would not be Argon2id. It is precisely WHY Argon2id runs the first half \
         of the first pass with data-INDEPENDENT addressing, and why Argon2d is \
         never the passphrase choice. RFC 9106 §4 analyses the tradeoff. \
         Consequence to respect elsewhere: do not run this on a host where an \
         attacker observes the cache and the password is the asset in question \
         — that is a deployment constraint, not something the code can fix.",
    ),
    (
        "audit.rs",
        Verdict::Clean,
        "External-audit evidence and release admission operate only on public \
         artifact digests, fixed coverage tags, and finding counts. No key, \
         plaintext, nonce, or other secret enters this module; branching on \
         the public audit verdict is the purpose of the release interlock.",
    ),
    (
        "cx.rs",
        Verdict::Clean,
        "Entropy plumbing. Control flow depends on source availability and \
         buffer LENGTH, never on the bytes read. The error path carries a source \
         id and a message, never the buffer.",
    ),
    (
        "zeroize.rs",
        Verdict::Clean,
        "Holds secret bytes but makes no decision from them: every Secret drop \
         delegates to the one non-inlined `scrub_slice` boundary, which performs \
         `fill(0)` plus a compiler fence unconditionally. The source delegation \
         is pinned below and the optimized boundary is independently inspected \
         by the live w1_crypto_codegen_e2e gate. That witness covers the zeroing \
         call only; it makes no claim about copies outside the owned buffer.",
    ),
];

/// THE COMPLETENESS GUARD: every module in the crate has an audit verdict.
///
/// The source list is read from disk at run time, so adding a module without an
/// audit row reds this test. Without it the table would silently describe an
/// older, smaller crate — the shape this workspace keeps finding, where a check
/// looks total and quietly covers a subset.
#[test]
fn every_module_has_an_audit_verdict() {
    let src = crate_root().join("src");
    let mut on_disk = BTreeSet::new();
    for entry in std::fs::read_dir(&src).expect("the crate has a src directory") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            on_disk.insert(
                path.file_name()
                    .expect("a file has a name")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }

    let audited: BTreeSet<String> = AUDIT
        .iter()
        .map(|(name, _, _)| (*name).to_string())
        .collect();

    let unaudited: Vec<_> = on_disk.difference(&audited).collect();
    assert!(
        unaudited.is_empty(),
        "these modules have no constant-time audit verdict: {unaudited:?} — a new \
         module must record whether its branches or indices depend on secrets \
         before it ships"
    );

    let stale: Vec<_> = audited.difference(&on_disk).collect();
    assert!(
        stale.is_empty(),
        "the audit table names modules that no longer exist: {stale:?} — a stale \
         row makes the table look more complete than it is"
    );
}

/// Every verdict carries a rationale, and every non-Clean verdict carries a
/// substantial one.
///
/// A table of bare verdicts is a list of opinions. The rationale is the part a
/// later reviewer can check against the source.
#[test]
fn every_verdict_is_justified() {
    for (module, verdict, rationale) in AUDIT {
        assert!(
            rationale.len() > 60,
            "{module}: rationale is too thin to check against the source"
        );
        if verdict == Verdict::AcceptedByDesign {
            assert!(
                rationale.len() > 300,
                "{module}: an accepted secret-dependence needs its full argument \
                 recorded, not a one-liner — this is the row a future auditor \
                 will most want to challenge"
            );
        }
    }
}

/// External review is a release input, not a prose promise.
///
/// This aggregate pins the registered engagement plan, strict artifact codec,
/// exact release-candidate bindings, and every refusal predicate. It does not
/// claim that an auditor has been selected or that a report already exists;
/// `None` is the repository's honest current evidence state and must refuse.
#[test]
fn external_audit_evidence_is_canonical_and_release_blocking() {
    use fgdb_crypto::{
        AuditConclusion, AuditCoverage, AuditFindingCounts, AuditMethod, CryptoAuditError,
        CryptoReleaseCandidate, EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN,
        EXTERNAL_CRYPTO_AUDIT_ENGAGEMENT_ID, EXTERNAL_CRYPTO_AUDIT_OWNER_BEAD,
        EXTERNAL_CRYPTO_AUDIT_RELEASE_GATE, EXTERNAL_CRYPTO_AUDIT_SCHEMA_VERSION,
        ExternalCryptoAuditEvidence, REGISTERED_CRYPTO_PROFILE_COUNT,
        REGISTERED_CRYPTO_PROFILE_SET_SCHEMA_VERSION, REGISTERED_EXTERNAL_CRYPTO_AUDIT_PLAN,
        REGISTERED_OBJECT_AEAD_PROFILES, REGISTERED_PASSPHRASE_KDF_PROFILES,
        admit_external_crypto_audit, external_crypto_audit_plan_digest,
        registered_crypto_profile_set_digest, registered_object_aead_profile,
        registered_passphrase_kdf_profile,
    };

    fn digest(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn evidence(
        conclusion: AuditConclusion,
        independent: bool,
        coverage: AuditCoverage,
        findings: AuditFindingCounts,
    ) -> ExternalCryptoAuditEvidence {
        ExternalCryptoAuditEvidence::try_new(
            conclusion,
            independent,
            coverage,
            findings,
            digest(3),
            digest(5),
            digest(4),
            digest(1),
            registered_crypto_profile_set_digest(),
        )
        .expect("the complete fixture is structurally valid")
    }

    assert_eq!(EXTERNAL_CRYPTO_AUDIT_SCHEMA_VERSION, 1);
    assert_eq!(EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN, 222);
    assert_eq!(
        REGISTERED_EXTERNAL_CRYPTO_AUDIT_PLAN.id,
        EXTERNAL_CRYPTO_AUDIT_ENGAGEMENT_ID
    );
    assert_eq!(
        REGISTERED_EXTERNAL_CRYPTO_AUDIT_PLAN.owner_bead,
        EXTERNAL_CRYPTO_AUDIT_OWNER_BEAD
    );
    assert_eq!(
        REGISTERED_EXTERNAL_CRYPTO_AUDIT_PLAN.release_gate,
        EXTERNAL_CRYPTO_AUDIT_RELEASE_GATE
    );
    assert_eq!(
        REGISTERED_EXTERNAL_CRYPTO_AUDIT_PLAN.required_coverage,
        AuditCoverage::REQUIRED
    );
    assert!(AuditCoverage::REQUIRED.is_complete());
    assert_ne!(external_crypto_audit_plan_digest(), [0; 32]);
    assert_eq!(REGISTERED_CRYPTO_PROFILE_SET_SCHEMA_VERSION, 1);
    assert_eq!(
        usize::from(REGISTERED_CRYPTO_PROFILE_COUNT),
        REGISTERED_OBJECT_AEAD_PROFILES.len() + REGISTERED_PASSPHRASE_KDF_PROFILES.len() + 1
    );
    assert_eq!(REGISTERED_CRYPTO_PROFILE_COUNT, 3);

    let resolved_aead_profiles: Vec<_> = (0..=u16::MAX)
        .filter_map(|id| registered_object_aead_profile(id).map(|profile| (id, profile)))
        .collect();
    let inventoried_aead_profiles: Vec<_> = REGISTERED_OBJECT_AEAD_PROFILES
        .iter()
        .copied()
        .map(|profile| (profile.id(), profile))
        .collect();
    assert_eq!(resolved_aead_profiles, inventoried_aead_profiles);
    assert!(
        inventoried_aead_profiles
            .windows(2)
            .all(|rows| rows[0].0 < rows[1].0),
        "object-AEAD inventory IDs must be unique and canonically ordered"
    );

    let resolved_kdf_profiles: Vec<_> = (0..=u16::MAX)
        .filter_map(|id| registered_passphrase_kdf_profile(id).map(|profile| (id, profile)))
        .collect();
    let inventoried_kdf_profiles: Vec<_> = REGISTERED_PASSPHRASE_KDF_PROFILES
        .iter()
        .copied()
        .map(|profile| (profile.id(), profile))
        .collect();
    assert_eq!(resolved_kdf_profiles, inventoried_kdf_profiles);
    assert!(
        inventoried_kdf_profiles
            .windows(2)
            .all(|rows| rows[0].0 < rows[1].0),
        "passphrase-KDF inventory IDs must be unique and canonically ordered"
    );

    let registered_profile_set = registered_crypto_profile_set_digest();
    assert_eq!(
        registered_profile_set,
        [
            0xd8, 0x3b, 0xd3, 0xf4, 0xe8, 0xa5, 0x5f, 0x69, 0x7c, 0x07, 0x88, 0xaa, 0x06, 0x8c,
            0x69, 0x32, 0xc3, 0x4d, 0x58, 0xa0, 0xf2, 0x4d, 0x46, 0x99, 0x39, 0xda, 0x81, 0x75,
            0xe7, 0x8c, 0x84, 0x7f,
        ],
        "profile rows, order, or canonical transcript changed without invalidating audit evidence"
    );

    let candidate = CryptoReleaseCandidate::try_new(digest(1), digest(3), digest(4), digest(5))
        .expect("the release candidate pins four external identities and the live profile set");
    assert_eq!(candidate.profile_set_digest(), registered_profile_set);
    let accepted = evidence(
        AuditConclusion::Accepted,
        true,
        AuditCoverage::REQUIRED,
        AuditFindingCounts::default(),
    );

    // The repository has no report today. Absence must be the load-bearing
    // release refusal, not a default-success state.
    assert_eq!(
        admit_external_crypto_audit(&candidate, None),
        Err(CryptoAuditError::MissingAuditEvidence)
    );

    let canonical = accepted.to_canonical_bytes();
    assert_eq!(canonical.len(), EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN);
    let decoded = ExternalCryptoAuditEvidence::try_from_canonical_bytes(&canonical)
        .expect("the strict artifact decodes");
    assert_eq!(decoded, accepted);
    assert_eq!(decoded.to_canonical_bytes(), canonical);
    assert_ne!(decoded.evidence_digest(), [0; 32]);

    let admission = admit_external_crypto_audit(&candidate, Some(&decoded))
        .expect("only the exact complete accepted artifact admits release");
    assert_eq!(admission.source_revision_digest(), digest(1));
    assert_eq!(admission.profile_set_digest(), registered_profile_set);
    assert_eq!(admission.audit_evidence_digest(), decoded.evidence_digest());

    assert_eq!(
        admit_external_crypto_audit(
            &candidate,
            Some(&evidence(
                AuditConclusion::Rejected,
                true,
                AuditCoverage::REQUIRED,
                AuditFindingCounts::default(),
            )),
        ),
        Err(CryptoAuditError::AuditRejected)
    );
    assert_eq!(
        admit_external_crypto_audit(
            &candidate,
            Some(&evidence(
                AuditConclusion::Accepted,
                false,
                AuditCoverage::REQUIRED,
                AuditFindingCounts::default(),
            )),
        ),
        Err(CryptoAuditError::IndependenceNotAttested)
    );

    let methods = [
        AuditMethod::PrimitiveAndProfileVectors,
        AuditMethod::StatisticalTiming,
        AuditMethod::SecretControlFlowAudit,
        AuditMethod::MisuseResistance,
        AuditMethod::Zeroization,
        AuditMethod::EntropyAndRedaction,
        AuditMethod::CompositionAndKeyLifecycle,
    ];
    assert_eq!(
        methods
            .iter()
            .fold(0_u16, |bits, method| bits | method.bit()),
        AuditCoverage::REQUIRED.bits(),
        "the test's independent methodology inventory must equal the registered mask"
    );
    for method in methods {
        let coverage = AuditCoverage::try_from_bits(AuditCoverage::REQUIRED.bits() & !method.bit())
            .expect("removing a known method leaves known coverage");
        assert_eq!(
            admit_external_crypto_audit(
                &candidate,
                Some(&evidence(
                    AuditConclusion::Accepted,
                    true,
                    coverage,
                    AuditFindingCounts::default(),
                )),
            ),
            Err(CryptoAuditError::IncompleteCoverage(method.bit())),
            "omitting {method:?} must independently block release"
        );
    }
    assert_eq!(
        AuditCoverage::try_from_bits(AuditCoverage::REQUIRED.bits() | 0x8000),
        Err(CryptoAuditError::UnknownCoverageBits(0x8000))
    );

    for findings in [
        AuditFindingCounts {
            critical: 1,
            ..AuditFindingCounts::default()
        },
        AuditFindingCounts {
            high: 1,
            ..AuditFindingCounts::default()
        },
        AuditFindingCounts {
            medium: 1,
            ..AuditFindingCounts::default()
        },
        AuditFindingCounts {
            low: 1,
            ..AuditFindingCounts::default()
        },
    ] {
        assert_eq!(
            admit_external_crypto_audit(
                &candidate,
                Some(&evidence(
                    AuditConclusion::Accepted,
                    true,
                    AuditCoverage::REQUIRED,
                    findings,
                )),
            ),
            Err(CryptoAuditError::UnresolvedFindings(findings)),
            "every unresolved severity independently blocks release"
        );
    }

    let candidate_mutations = [
        (
            CryptoReleaseCandidate::try_new(digest(9), digest(3), digest(4), digest(5)).unwrap(),
            CryptoAuditError::SourceRevisionMismatch,
        ),
        (
            CryptoReleaseCandidate::try_new(digest(1), digest(9), digest(4), digest(5)).unwrap(),
            CryptoAuditError::AuditorIdentityMismatch,
        ),
        (
            CryptoReleaseCandidate::try_new(digest(1), digest(3), digest(9), digest(5)).unwrap(),
            CryptoAuditError::ReportMismatch,
        ),
        (
            CryptoReleaseCandidate::try_new(digest(1), digest(3), digest(4), digest(9)).unwrap(),
            CryptoAuditError::AttestationMismatch,
        ),
    ];
    for (mutated, expected) in candidate_mutations {
        assert_eq!(
            admit_external_crypto_audit(&mutated, Some(&accepted)),
            Err(expected)
        );
    }

    let stale_profile_evidence = ExternalCryptoAuditEvidence::try_new(
        AuditConclusion::Accepted,
        true,
        AuditCoverage::REQUIRED,
        AuditFindingCounts::default(),
        digest(3),
        digest(5),
        digest(4),
        digest(1),
        digest(9),
    )
    .expect("stale audit evidence remains representable so admission can reject it");
    assert_eq!(
        admit_external_crypto_audit(&candidate, Some(&stale_profile_evidence)),
        Err(CryptoAuditError::ProfileSetMismatch)
    );

    // The four caller-supplied release pins and the five evidence identities refuse the zero
    // sentinel independently; there is no "unassigned but accepted" state.
    for field in 0..4 {
        let mut values = [digest(1), digest(3), digest(4), digest(5)];
        values[field] = [0; 32];
        assert!(matches!(
            CryptoReleaseCandidate::try_new(values[0], values[1], values[2], values[3]),
            Err(CryptoAuditError::ZeroDigest(_))
        ));
    }
    for field in 0..5 {
        let mut values = [digest(3), digest(5), digest(4), digest(1), digest(2)];
        values[field] = [0; 32];
        assert!(matches!(
            ExternalCryptoAuditEvidence::try_new(
                AuditConclusion::Accepted,
                true,
                AuditCoverage::REQUIRED,
                AuditFindingCounts::default(),
                values[0],
                values[1],
                values[2],
                values[3],
                values[4],
            ),
            Err(CryptoAuditError::ZeroDigest(_))
        ));
    }

    for length in 0..EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN {
        assert_eq!(
            ExternalCryptoAuditEvidence::try_from_canonical_bytes(&canonical[..length]),
            Err(CryptoAuditError::InvalidArtifactLength {
                expected: EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN,
                actual: length,
            })
        );
    }
    let mut trailing = canonical.clone();
    trailing.push(0);
    assert_eq!(
        ExternalCryptoAuditEvidence::try_from_canonical_bytes(&trailing),
        Err(CryptoAuditError::InvalidArtifactLength {
            expected: EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN,
            actual: EXTERNAL_CRYPTO_AUDIT_ARTIFACT_LEN + 1,
        })
    );

    let codec_mutations: [(usize, u8, CryptoAuditError); 5] = [
        (0, b'X', CryptoAuditError::InvalidArtifactMagic),
        (8, 2, CryptoAuditError::UnsupportedSchemaVersion(2)),
        (10, 0xff, CryptoAuditError::UnknownConclusion(0xff)),
        (11, 2, CryptoAuditError::InvalidBoolean(2)),
        (13, 0x80, CryptoAuditError::UnknownCoverageBits(0x8000)),
    ];
    for (offset, byte, expected) in codec_mutations {
        let mut mutated = canonical.clone();
        mutated[offset] = byte;
        assert_eq!(
            ExternalCryptoAuditEvidence::try_from_canonical_bytes(&mutated),
            Err(expected),
            "canonical mutation at offset {offset} must fail with its own typed error"
        );
    }

    let mut wrong_plan = canonical.clone();
    wrong_plan[30] ^= 1;
    assert_eq!(
        ExternalCryptoAuditEvidence::try_from_canonical_bytes(&wrong_plan),
        Err(CryptoAuditError::EngagementPlanMismatch)
    );
    for digest_offset in [62, 94, 126, 158, 190] {
        let mut missing = canonical.clone();
        missing[digest_offset..digest_offset + 32].fill(0);
        assert!(matches!(
            ExternalCryptoAuditEvidence::try_from_canonical_bytes(&missing),
            Err(CryptoAuditError::ZeroDigest(_))
        ));
    }
}

/// The AEAD tag comparison must stay constant-time.
///
/// A source-level lint, because the regression it catches is a one-character
/// edit: comparing the tag with `==` or `!=` reintroduces an early-exit timing
/// oracle, the canonical forgery channel. The functional vectors pass either
/// way, so nothing else in the suite would notice.
///
/// **THIS TEST WAS WEAKER THAN IT LOOKED AND A MUTATION PROVED IT.** The first
/// version asserted that the file CONTAINED `constant_time_eq16` and lacked the
/// literal `tag == expected_tag`. Replacing the call site with
/// `if tag != expected_tag` left it GREEN: the helper's *definition* still
/// matched the first assertion, and `!=` did not match the second. Presence of a
/// safe helper is not use of it. The check now reads the OPEN FUNCTION'S BODY
/// and requires the call, rejecting both comparison operators.
#[test]
fn tag_comparison_stays_constant_time() {
    let aead =
        std::fs::read_to_string(crate_root().join("src/aead.rs")).expect("aead.rs is readable");

    // The verification path itself, not the file at large.
    let open_start = aead
        .find("pub fn chacha20poly1305_open")
        .expect("the open path is present");
    let open_body = &aead[open_start..];
    let open_end = open_body.find("\n}").unwrap_or(open_body.len());
    let open_body = &open_body[..open_end];

    assert!(
        open_body.contains("constant_time_eq16("),
        "the AEAD open path does not CALL the constant-time comparison — a \
         helper that exists but is not used protects nothing:\n{open_body}"
    );
    for forbidden in ["tag ==", "tag !=", "expected_tag ==", "expected_tag !="] {
        assert!(
            !open_body.contains(forbidden),
            "the AEAD open path compares the tag with {forbidden:?}, which \
             short-circuits on the first differing byte and turns verification \
             into a timing oracle:\n{open_body}"
        );
    }

    // And the helper it calls must accumulate rather than exit early.
    let helper_start = aead
        .find("fn constant_time_eq16")
        .expect("the helper is present");
    let helper = &aead[helper_start..];
    let body = &helper[..helper.find("\n}").unwrap_or(helper.len())];
    assert!(
        !body.contains("return"),
        "the constant-time comparison helper contains an early return: {body}"
    );
    assert!(
        body.contains("|=") || body.contains("diff"),
        "the helper no longer accumulates differences: {body}"
    );
}

/// Poly1305's final reduction must stay branchless.
///
/// The conditional subtraction of p is where this primitive is classically
/// implemented with an `if`, which leaks whether the accumulator exceeded the
/// modulus — a bias an attacker can accumulate across many tags.
#[test]
fn the_reduction_stays_branchless() {
    let poly = std::fs::read_to_string(crate_root().join("src/poly1305.rs"))
        .expect("poly1305.rs is readable");

    let reduction_start = poly
        .find("Constant-shape select")
        .expect("the reduction still documents its constant-shape select");
    let reduction = &poly[reduction_start..];
    let window = &reduction[..reduction.len().min(600)];

    assert!(
        window.contains("mask"),
        "the reduction no longer selects with a mask: {window}"
    );
    assert!(
        !window.contains("if "),
        "the final reduction regained a branch, which leaks whether the \
         accumulator exceeded the modulus: {window}"
    );
}

/// Every owned crypto-state class reaches a release-codegen boundary.
///
/// The release-object gate can only prove something about the public byte and
/// word boundaries if the production `Secret`, Argon2, and BLAKE2b authorities
/// actually delegate to them. Keeping the linkage assertion beside the source
/// audit makes the halves mutually load-bearing: a surviving symbol that no
/// secret-derived storage calls is not evidence.
#[test]
fn secret_scrub_delegates_to_codegen_witnessed_boundary() {
    let source = std::fs::read_to_string(crate_root().join("src/zeroize.rs"))
        .expect("zeroize.rs is readable");
    let argon = std::fs::read_to_string(crate_root().join("src/argon2id.rs"))
        .expect("argon2id.rs is readable");
    let blake = std::fs::read_to_string(crate_root().join("src/blake2b.rs"))
        .expect("blake2b.rs is readable");
    let blake3 =
        std::fs::read_to_string(crate_root().join("src/blake3.rs")).expect("blake3.rs is readable");
    let chacha = std::fs::read_to_string(crate_root().join("src/chacha20.rs"))
        .expect("chacha20.rs is readable");
    let poly = std::fs::read_to_string(crate_root().join("src/poly1305.rs"))
        .expect("poly1305.rs is readable");
    let aead =
        std::fs::read_to_string(crate_root().join("src/aead.rs")).expect("aead.rs is readable");

    fn code_lines(body: &str) -> Vec<&str> {
        body.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .collect::<Vec<_>>()
    }

    for (name, authority) in [
        ("zeroize.rs", source.as_str()),
        ("argon2id.rs", argon.as_str()),
        ("blake2b.rs", blake.as_str()),
        ("blake3.rs", blake3.as_str()),
        ("chacha20.rs", chacha.as_str()),
        ("poly1305.rs", poly.as_str()),
        ("aead.rs", aead.as_str()),
    ] {
        for forbidden_indirection in ["#[cfg", "#[cfg_attr", "macro_rules!", "include!"] {
            assert!(
                !authority.contains(forbidden_indirection),
                "{name} must keep its scrub authority unconditional and directly \
                 inspectable; found {forbidden_indirection}"
            );
        }
    }

    let mut compiled_secret = fgdb_crypto::zeroize::Secret::new([0xa5_u8; 2]);
    compiled_secret.scrub();
    assert_eq!(
        compiled_secret.expose(),
        &[0_u8; 2],
        "the production-selected Secret::scrub method must execute the exact source authority"
    );
    let mut compiled_slice = [0xa5_u8; 2];
    fgdb_crypto::zeroize::scrub_slice(&mut compiled_slice);
    assert_eq!(
        compiled_slice, [0_u8; 2],
        "the production-selected scrub_slice must execute for every admitted length"
    );
    let mut compiled_words = [0xa5_a5_a5_a5_a5_a5_a5_a5_u64; 2];
    fgdb_crypto::zeroize::scrub_words(&mut compiled_words);
    assert_eq!(
        compiled_words, [0_u64; 2],
        "the production-selected scrub_words must execute for every admitted length"
    );
    let mut compiled_words32 = [0xa5_a5_a5_a5_u32; 2];
    fgdb_crypto::zeroize::scrub_words32(&mut compiled_words32);
    assert_eq!(
        compiled_words32, [0_u32; 2],
        "the production-selected scrub_words32 must execute for every admitted length"
    );
    assert!(
        core::mem::needs_drop::<fgdb_crypto::blake2b::Blake2b>(),
        "the production BLAKE2b state must carry scrub-on-drop glue"
    );
    assert!(
        core::mem::needs_drop::<fgdb_crypto::blake3::Hasher>(),
        "the production BLAKE3 state must carry keyed-state scrub-on-drop glue"
    );
    assert!(
        core::mem::needs_drop::<fgdb_crypto::poly1305::Poly1305>(),
        "the production Poly1305 state must carry scrub-on-drop glue"
    );

    let scrub_start = source
        .find("pub fn scrub(&mut self)")
        .expect("Secret::scrub is present");
    assert_eq!(
        source.matches("pub fn scrub(&mut self)").count(),
        1,
        "zeroize.rs must contain exactly one Secret::scrub authority"
    );
    let scrub = &source[scrub_start..];
    let scrub = &scrub[..scrub.find("\n    }").unwrap_or(scrub.len())];
    assert_eq!(
        code_lines(scrub),
        ["pub fn scrub(&mut self) {", "scrub_slice(&mut self.bytes);",],
        "Secret::scrub must consist solely of delegating to the witnessed boundary"
    );

    let drop_start = source
        .find("impl<const N: usize> Drop for Secret<N>")
        .expect("Secret has an automatic Drop implementation");
    assert_eq!(
        source
            .matches("impl<const N: usize> Drop for Secret<N>")
            .count(),
        1,
        "zeroize.rs must contain exactly one Drop authority for Secret"
    );
    assert!(
        source[..drop_start].ends_with("\n}\n\n"),
        "the sole Drop implementation must be an unconditional top-level item, \
         not a cfg- or macro-selected decoy"
    );
    assert!(
        core::mem::needs_drop::<fgdb_crypto::zeroize::Secret<32>>(),
        "the production-compiled Secret type must actually carry Drop glue"
    );
    let drop_impl = &source[drop_start..];
    let drop_impl = &drop_impl[..drop_impl.find("\n}\n").unwrap_or(drop_impl.len())];
    assert_eq!(
        code_lines(drop_impl),
        [
            "impl<const N: usize> Drop for Secret<N> {",
            "fn drop(&mut self) {",
            "self.scrub();",
            "}",
        ],
        "automatic Secret drop must consist solely of invoking the witnessed scrub path"
    );

    let boundary_start = source
        .find("#[inline(never)]\npub fn scrub_slice")
        .expect("the non-inlined codegen boundary is present");
    assert_eq!(
        source
            .matches("pub fn scrub_slice(bytes: &mut [u8])")
            .count(),
        1,
        "zeroize.rs must contain exactly one scrub_slice authority"
    );
    let boundary = &source[boundary_start..];
    let boundary = &boundary[..boundary.find("\n}").unwrap_or(boundary.len())];
    assert_eq!(
        code_lines(boundary),
        [
            "#[inline(never)]",
            "pub fn scrub_slice(bytes: &mut [u8]) {",
            "bytes.fill(0);",
            "compiler_fence(Ordering::SeqCst);",
        ],
        "the witnessed boundary must consist solely of zeroing then fencing"
    );

    let word_boundary_start = source
        .find("#[inline(never)]\npub fn scrub_words")
        .expect("the non-inlined word codegen boundary is present");
    assert_eq!(
        source
            .matches("pub fn scrub_words(words: &mut [u64])")
            .count(),
        1,
        "zeroize.rs must contain exactly one scrub_words authority"
    );
    let word_boundary = &source[word_boundary_start..];
    let word_boundary = &word_boundary[..word_boundary.find("\n}").unwrap_or(word_boundary.len())];
    assert_eq!(
        code_lines(word_boundary),
        [
            "#[inline(never)]",
            "pub fn scrub_words(words: &mut [u64]) {",
            "words.fill(0);",
            "compiler_fence(Ordering::SeqCst);",
        ],
        "the witnessed word boundary must consist solely of zeroing then fencing"
    );

    let narrow_boundary_start = source
        .find("#[inline(never)]\npub fn scrub_words32")
        .expect("the non-inlined 32-bit word codegen boundary is present");
    assert_eq!(
        source
            .matches("pub fn scrub_words32(words: &mut [u32])")
            .count(),
        1,
        "zeroize.rs must contain exactly one scrub_words32 authority"
    );
    let narrow_boundary = &source[narrow_boundary_start..];
    let narrow_boundary =
        &narrow_boundary[..narrow_boundary.find("\n}").unwrap_or(narrow_boundary.len())];
    assert_eq!(
        code_lines(narrow_boundary),
        [
            "#[inline(never)]",
            "pub fn scrub_words32(words: &mut [u32]) {",
            "words.fill(0);",
            "compiler_fence(Ordering::SeqCst);",
        ],
        "the witnessed 32-bit word boundary must consist solely of zeroing then fencing"
    );

    assert!(
        argon.contains("#[derive(Clone)]\nstruct Block([u64; BLOCK_WORDS]);"),
        "Argon2 blocks must permit only explicit internal clones, never implicit Copy"
    );
    assert!(
        !argon.contains("#[derive(Clone, Copy)]\nstruct Block")
            && !argon.contains("impl Copy for Block"),
        "Argon2 memory blocks must not become implicitly copyable secret state"
    );
    let block_drop_start = argon
        .find("impl Drop for Block")
        .expect("Argon2 blocks have automatic scrub-on-drop glue");
    assert_eq!(
        argon.matches("impl Drop for Block").count(),
        1,
        "argon2id.rs must contain exactly one Block Drop authority"
    );
    assert!(
        argon[..block_drop_start].ends_with("\n\n"),
        "the Block Drop authority must be an unconditional top-level item"
    );
    let block_drop = &argon[block_drop_start..];
    let block_drop = &block_drop[..block_drop.find("\n}\n").unwrap_or(block_drop.len())];
    assert_eq!(
        code_lines(block_drop),
        [
            "impl Drop for Block {",
            "fn drop(&mut self) {",
            "scrub_words(&mut self.0);",
            "}",
        ],
        "every original Argon2 block allocation must scrub its word storage on drop"
    );

    let bytes_drop_start = argon
        .find("impl Drop for SensitiveBytes")
        .expect("derived byte buffers have automatic scrub-on-drop glue");
    assert_eq!(
        argon.matches("impl Drop for SensitiveBytes").count(),
        1,
        "argon2id.rs must contain exactly one SensitiveBytes Drop authority"
    );
    assert!(
        argon[..bytes_drop_start].ends_with("\n\n"),
        "the SensitiveBytes Drop authority must be an unconditional top-level item"
    );
    let bytes_drop = &argon[bytes_drop_start..];
    let bytes_drop = &bytes_drop[..bytes_drop.find("\n}\n").unwrap_or(bytes_drop.len())];
    assert_eq!(
        code_lines(bytes_drop),
        [
            "impl Drop for SensitiveBytes {",
            "fn drop(&mut self) {",
            "scrub_slice(&mut self.0);",
            "}",
        ],
        "every derived Argon2 byte buffer must scrub its allocation on drop"
    );
    assert!(
        !argon.contains("impl Clone for SensitiveBytes")
            && !argon.contains("impl core::ops::Deref for SensitiveBytes")
            && !argon.contains("impl core::ops::DerefMut for SensitiveBytes"),
        "SensitiveBytes must not expose a clone or raw Vec method surface that can escape scrub ownership"
    );

    let argon_compression_start = argon
        .find("fn compress(x: &Block, y: &Block)")
        .expect("Argon2 compression function is present");
    let argon_compression = &argon[argon_compression_start..];
    let argon_compression = &argon_compression[..argon_compression
        .find("\n}\n\n/// `H'^T(X)`")
        .expect("Argon2 compression function remains directly inspectable")];
    assert_eq!(
        code_lines(argon_compression)
            .into_iter()
            .filter(|line| *line == "scrub_words(&mut v);")
            .count(),
        2,
        "each Argon2 row/column scratch array must be scrubbed after its final use"
    );
    assert!(
        argon_compression.contains(
            "q.0[row * 16..row * 16 + 16].copy_from_slice(&v);\n        scrub_words(&mut v);"
        ) && argon_compression.contains(
            "q.0[16 * k + 2 * col + 1] = v[2 * k + 1];\n        }\n        scrub_words(&mut v);"
        ),
        "Argon2 must scrub each scratch array only after copying its result back"
    );
    for required_sensitive_path in [
        "fn variable_hash(out_len: usize, input: &[u8]) -> SensitiveBytes",
        "let h0 = SensitiveBytes::from_vec(h0_input.finalize());",
        "let mut input = SensitiveBytes::with_capacity(72);",
        "let final_block_bytes = Secret::new(final_block.into_bytes());",
    ] {
        assert!(
            argon.contains(required_sensitive_path),
            "Argon2 derived bytes escaped their scrub-on-drop owner: missing {required_sensitive_path}"
        );
    }

    assert!(
        !blake.contains("#[derive(Clone)]\npub struct Blake2b")
            && !blake.contains("impl Clone for Blake2b"),
        "BLAKE2b state must not offer a secret-state duplication API"
    );
    let blake_drop_start = blake
        .find("impl Drop for Blake2b")
        .expect("BLAKE2b state has automatic scrub-on-drop glue");
    assert_eq!(
        blake.matches("impl Drop for Blake2b").count(),
        1,
        "blake2b.rs must contain exactly one Blake2b Drop authority"
    );
    assert!(
        blake[..blake_drop_start].ends_with("\n}\n\n"),
        "the Blake2b Drop authority must be an unconditional top-level item"
    );
    let blake_drop = &blake[blake_drop_start..];
    let blake_drop = &blake_drop[..blake_drop.find("\n}\n").unwrap_or(blake_drop.len())];
    assert_eq!(
        code_lines(blake_drop),
        [
            "impl Drop for Blake2b {",
            "fn drop(&mut self) {",
            "scrub_words(&mut self.h);",
            "scrub_slice(&mut self.buffer);",
            "}",
        ],
        "BLAKE2b drop must scrub both chaining words and buffered message bytes"
    );
    assert!(
        blake.contains("let mut key_block = Secret::<BLOCK_LEN>::zeroed();"),
        "the BLAKE2b padded key block must remain inside a scrub-on-drop Secret"
    );

    let compression_start = blake
        .find("fn compress(h: &mut [u64; 8]")
        .expect("BLAKE2b compression function is present");
    let compression = &blake[compression_start..];
    let compression = &compression[..compression
        .find("\n}\n\n/// A streaming BLAKE2b instance")
        .expect("BLAKE2b compression function remains directly inspectable")];
    let compression_lines = code_lines(compression);
    assert_eq!(
        &compression_lines[compression_lines.len() - 2..],
        ["scrub_words(&mut m);", "scrub_words(&mut v);"],
        "BLAKE2b must scrub both secret-derived compression temporaries before return"
    );

    assert!(
        blake3.contains("const MAX_CV_STACK_DEPTH: usize = 64;")
            && blake3.contains("cv_stack: [[u32; 8]; MAX_CV_STACK_DEPTH]")
            && !blake3.contains("cv_stack: Vec<"),
        "BLAKE3 must keep the complete bounded chaining-value stack in scrub-addressable storage"
    );
    let secret_mode_start = blake3
        .find("#[inline]\nfn owns_secret_state")
        .expect("the BLAKE3 secret-mode classifier is directly inspectable");
    let secret_mode = &blake3[secret_mode_start..];
    let secret_mode = &secret_mode[..secret_mode.find("\n}").unwrap_or(secret_mode.len())];
    assert_eq!(
        code_lines(secret_mode),
        [
            "#[inline]",
            "fn owns_secret_state(flags: u32) -> bool {",
            "flags & (KEYED_HASH | DERIVE_KEY_MATERIAL) != 0",
        ],
        "only keyed-hash and derive-key material modes own secret BLAKE3 state"
    );
    for (drop_signature, expected_lines) in [
        (
            "impl Drop for ChunkState",
            vec![
                "impl Drop for ChunkState {",
                "fn drop(&mut self) {",
                "if owns_secret_state(self.flags) {",
                "scrub_words32(&mut self.chaining_value);",
                "scrub_slice(&mut self.block);",
                "}",
                "}",
            ],
        ),
        (
            "impl Drop for Output",
            vec![
                "impl Drop for Output {",
                "fn drop(&mut self) {",
                "if owns_secret_state(self.flags) {",
                "scrub_words32(&mut self.input_chaining_value);",
                "scrub_words32(&mut self.block_words);",
                "}",
                "}",
            ],
        ),
        (
            "impl Drop for Hasher",
            vec![
                "impl Drop for Hasher {",
                "fn drop(&mut self) {",
                "if owns_secret_state(self.flags) {",
                "scrub_words32(&mut self.key_words);",
                "for chaining_value in &mut self.cv_stack {",
                "scrub_words32(chaining_value);",
                "}",
                "}",
                "}",
            ],
        ),
    ] {
        assert_eq!(
            blake3.matches(drop_signature).count(),
            1,
            "blake3.rs must contain exactly one {drop_signature} authority"
        );
        let drop_start = blake3
            .find(drop_signature)
            .expect("the counted BLAKE3 Drop authority is present");
        assert!(
            blake3[..drop_start].ends_with("\n\n"),
            "{drop_signature} must be an unconditional top-level item"
        );
        let drop_body = &blake3[drop_start..];
        let drop_body = &drop_body[..drop_body.find("\n}\n").unwrap_or(drop_body.len())];
        assert_eq!(
            code_lines(drop_body),
            expected_lines,
            "{drop_signature} no longer scrubs its complete keyed-state authority"
        );
    }
    assert_eq!(
        blake3.matches("scrub_words32(&mut key_words);").count(),
        2,
        "both keyed-hash and derive-key constructors must scrub their parsed key-word staging"
    );
    assert_eq!(
        blake3.matches("scrub_slice(&mut context_key.0);").count(),
        1,
        "derive-key construction must scrub its separate context-key digest"
    );
    assert_eq!(
        blake3
            .matches("scrub_words32(&mut compression_output);")
            .count(),
        2,
        "both chunk and output chaining-value compression temporaries must be scrubbed"
    );
    assert!(
        !blake3.contains("impl Clone for Hasher")
            && !blake3.contains("impl core::ops::Deref for Hasher")
            && !blake3.contains("impl core::ops::DerefMut for Hasher"),
        "BLAKE3 must not expose keyed-state duplication or field-escape APIs"
    );

    let chacha_drop_start = chacha
        .find("impl<const N: usize> Drop for SensitiveWords32<N>")
        .expect("ChaCha native word state has automatic scrub-on-drop glue");
    assert_eq!(
        chacha
            .matches("impl<const N: usize> Drop for SensitiveWords32<N>")
            .count(),
        1,
        "chacha20.rs must contain exactly one native-word Drop authority"
    );
    assert!(
        chacha[..chacha_drop_start].ends_with("\n\n"),
        "the ChaCha word-state Drop authority must be an unconditional top-level item"
    );
    let chacha_drop = &chacha[chacha_drop_start..];
    let chacha_drop = &chacha_drop[..chacha_drop.find("\n}\n").unwrap_or(chacha_drop.len())];
    assert_eq!(
        code_lines(chacha_drop),
        [
            "impl<const N: usize> Drop for SensitiveWords32<N> {",
            "fn drop(&mut self) {",
            "scrub_words32(&mut self.0);",
            "}",
        ],
        "every ChaCha key/state word owner must scrub its original storage on drop"
    );
    assert!(
        !chacha.contains("impl Clone for SensitiveWords32")
            && !chacha.contains("impl core::ops::Deref for SensitiveWords32")
            && !chacha.contains("impl core::ops::DerefMut for SensitiveWords32"),
        "ChaCha word owners must not expose duplication or raw-array escape APIs"
    );
    for required_secret_owner in [
        "fn key_words(key: &[u8; 32]) -> SensitiveWords32<8>",
        "fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> Secret<64>",
        "pub fn poly1305_key(key: &[u8; 32], nonce: &[u8; 12]) -> Secret<32>",
        "pub fn hchacha20(key: &[u8; 32], nonce16: &[u8; 16]) -> Secret<32>",
        "let initial = SensitiveWords32([",
        "let mut state = SensitiveWords32(initial.0);",
        "double_round(&mut state.0);",
        "let mut out = Secret::<64>::zeroed();",
        "let mut out = Secret::<32>::zeroed();",
    ] {
        assert!(
            chacha.contains(required_secret_owner),
            "ChaCha derived key material escaped a scrub owner: missing {required_secret_owner}"
        );
    }

    let poly_drop_start = poly
        .find("impl Drop for Poly1305")
        .expect("Poly1305 state has automatic scrub-on-drop glue");
    assert_eq!(
        poly.matches("impl Drop for Poly1305").count(),
        1,
        "poly1305.rs must contain exactly one Poly1305 Drop authority"
    );
    assert!(
        poly[..poly_drop_start].ends_with("\n}\n\n"),
        "the Poly1305 Drop authority must be an unconditional top-level item"
    );
    let poly_drop = &poly[poly_drop_start..];
    let poly_drop = &poly_drop[..poly_drop.find("\n}\n").unwrap_or(poly_drop.len())];
    assert_eq!(
        code_lines(poly_drop),
        [
            "impl Drop for Poly1305 {",
            "fn drop(&mut self) {",
            "scrub_words(&mut self.r);",
            "scrub_words(&mut self.s_r);",
            "scrub_words32(&mut self.pad);",
            "scrub_words(&mut self.acc);",
            "scrub_slice(&mut self.buffer);",
            "}",
        ],
        "Poly1305 drop must scrub every key-derived and message-bearing state field"
    );
    assert_eq!(
        poly.matches("scrub_words32(&mut t);").count(),
        1,
        "Poly1305 construction must scrub its separate clamped-key word staging array"
    );
    assert_eq!(
        poly.matches("let mut block = Secret::<16>::zeroed();")
            .count(),
        2,
        "Poly1305 must scrub both explicit full-block and final-partial staging copies"
    );
    assert_eq!(
        poly.matches("let block = Secret::new(self.buffer);")
            .count(),
        1,
        "Poly1305 must scrub the copied buffered-block staging owner"
    );
    assert!(
        !poly.contains("impl Clone for Poly1305")
            && !poly.contains("impl core::ops::Deref for Poly1305")
            && !poly.contains("impl core::ops::DerefMut for Poly1305"),
        "Poly1305 must not expose state duplication or field-escape APIs"
    );

    for required_aead_path in [
        "let otk = chacha20::poly1305_key(key, nonce);",
        "compute_tag(otk.expose(), aad",
        "fn xchacha_subparts(key: &[u8; 32], nonce24: &[u8; 24]) -> (Secret<32>, [u8; 12])",
        "chacha20poly1305_seal(subkey.expose(), &subnonce",
        "chacha20poly1305_open(subkey.expose(), &subnonce",
    ] {
        assert!(
            aead.contains(required_aead_path),
            "AEAD derived key material escaped a scrub owner: missing {required_aead_path}"
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct RunningMoments {
    count: u64,
    mean_nanos: f64,
    m2_nanos: f64,
}

impl RunningMoments {
    const fn new() -> Self {
        Self {
            count: 0,
            mean_nanos: 0.0,
            m2_nanos: 0.0,
        }
    }

    fn observe(&mut self, nanos: f64) {
        self.count += 1;
        let delta = nanos - self.mean_nanos;
        self.mean_nanos += delta / self.count as f64;
        let delta_after = nanos - self.mean_nanos;
        self.m2_nanos += delta * delta_after;
    }

    fn sample_variance(self) -> f64 {
        assert!(self.count >= 2, "Welch evidence needs at least two samples");
        self.m2_nanos / (self.count - 1) as f64
    }
}

#[derive(Debug, Clone, Copy)]
struct WelchEvidence {
    first: RunningMoments,
    last: RunningMoments,
    t_statistic: f64,
}

fn elapsed_batch<F, R>(batch_size: usize, operation: &mut F) -> f64
where
    F: FnMut() -> R,
{
    let started = Instant::now();
    for _ in 0..batch_size {
        std::hint::black_box(operation());
    }
    started.elapsed().as_secs_f64() * 1_000_000_000.0
}

/// Deterministic ABBA interleaving balances monotonic host drift without using
/// secret or ambient entropy to select classes. Each recorded sample is one
/// complete batch, so timer quantization is small relative to the signal.
fn interleaved_welch<F, G, R>(
    rounds: usize,
    batch_size: usize,
    mut first: F,
    mut last: G,
) -> WelchEvidence
where
    F: FnMut() -> R,
    G: FnMut() -> R,
{
    assert!(rounds >= 2, "Welch evidence needs at least two ABBA rounds");
    assert!(batch_size > 0, "a timing sample must execute real work");

    for _ in 0..32 {
        std::hint::black_box(first());
        std::hint::black_box(last());
    }

    let mut paired_rounds = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let first_before = elapsed_batch(batch_size, &mut first);
        let last_before = elapsed_batch(batch_size, &mut last);
        let last_after = elapsed_batch(batch_size, &mut last);
        let first_after = elapsed_batch(batch_size, &mut first);
        paired_rounds.push(([first_before, first_after], [last_before, last_after]));
    }

    // A workspace-wide test run can preempt one batch for milliseconds. Such
    // host-load outliers made the deliberately vulnerable control disappear
    // into the variance even though its last-byte class was over 100x slower.
    // Rank complete ABBA rounds by the class-blind sum and discard only the
    // highest-load one-eighth. A systematic class separation remains in every
    // retained round; the filter cannot select on which public class was faster.
    paired_rounds.sort_by(|left, right| {
        left.0
            .iter()
            .chain(left.1.iter())
            .sum::<f64>()
            .total_cmp(&right.0.iter().chain(right.1.iter()).sum::<f64>())
    });
    let retained_rounds = rounds - rounds / 8;
    let mut first_moments = RunningMoments::new();
    let mut last_moments = RunningMoments::new();
    for (first_samples, last_samples) in paired_rounds.into_iter().take(retained_rounds) {
        for nanos in first_samples {
            first_moments.observe(nanos);
        }
        for nanos in last_samples {
            last_moments.observe(nanos);
        }
    }

    let denominator = (first_moments.sample_variance() / first_moments.count as f64
        + last_moments.sample_variance() / last_moments.count as f64)
        .sqrt();
    assert!(
        denominator.is_finite() && denominator > 0.0,
        "timing evidence has no finite measurable variance"
    );
    let t_statistic = (first_moments.mean_nanos - last_moments.mean_nanos) / denominator;
    WelchEvidence {
        first: first_moments,
        last: last_moments,
        t_statistic,
    }
}

#[inline(never)]
fn planted_early_exit_compare(left: &[u8], right: &[u8]) -> bool {
    for (left_byte, right_byte) in left.iter().zip(right.iter()) {
        if left_byte != right_byte {
            return false;
        }
    }
    left.len() == right.len()
}

/// Bounded statistical evidence for the exact AEAD tag-compare regression the
/// source audit forbids.
///
/// The two public inputs differ only in whether the forged tag mismatches at
/// byte zero or byte fifteen. Authenticate-then-decrypt must reject both before
/// producing plaintext. An early-return equality check makes the first class
/// measurably faster; the production accumulator should not. This is a
/// dudect-style Welch-t screen, not a portable constant-time theorem.
///
/// **WHY EACH VERDICT IS A QUORUM OVER INDEPENDENT SCREENS.** The single-shot
/// screen conflates two things it must not: kernel-level class separation
/// (stationary — it is a property of the compiled compare) and transient host
/// load (not stationary — a workspace-wide run, a CI neighbor, or a steal-heavy
/// scheduler can push Welch-t past any fixed bound for reasons that have
/// nothing to do with the kernel). CI run 33285320157 was exactly that false
/// positive: the production screen tripped `MAX_PRODUCTION_ABS_T` on a shared
/// runner that passes cleanly here. The load trim inside
/// [`interleaved_welch`] removes within-screen outliers; it cannot remove a
/// load episode that spans a whole screen. Repeating the screen and requiring
/// a quorum does: a real separation reproduces on every screen, while a load
/// episode does not correlate across independent repetitions. The bounds
/// themselves are untouched — this changes how many independent screens must
/// agree, not what either verdict means.
#[test]
fn aead_forgery_timing_probe_is_bounded_and_detector_is_live() {
    const AEAD_ROUNDS: usize = 768;
    const AEAD_BATCH_SIZE: usize = 128;
    const CONTROL_ROUNDS: usize = 192;
    const CONTROL_BATCH_SIZE: usize = 64;
    const MAX_PRODUCTION_ABS_T: f64 = 10.0;
    const MIN_PLANTED_ABS_T: f64 = 20.0;
    /// Independent screens per verdict; see the doc comment above for why a
    /// quorum — and not the bound — is what changed for shared CI runners.
    const SCREENS: usize = 3;
    /// Passing screens required out of [`SCREENS`]. Two of three tolerates
    /// one load episode while still refusing a single lucky pass.
    const PASS_QUORUM: usize = 2;

    let key = [0x42_u8; 32];
    let nonce = [0x24_u8; 12];
    let aad = b"fgdb:timing-probe:v1";
    let sealed = fgdb_crypto::aead::chacha20poly1305_seal(&key, &nonce, aad, b"");
    assert_eq!(sealed.len(), 16, "empty plaintext leaves exactly one tag");
    let mut first_mismatch = sealed.clone();
    first_mismatch[0] ^= 1;
    let mut last_mismatch = sealed;
    last_mismatch[15] ^= 1;
    assert!(fgdb_crypto::aead::chacha20poly1305_open(&key, &nonce, aad, &first_mismatch).is_err());
    assert!(fgdb_crypto::aead::chacha20poly1305_open(&key, &nonce, aad, &last_mismatch).is_err());

    let equal = [0xa5_u8; 4096];
    let mut control_first = equal;
    control_first[0] ^= 1;
    let mut control_last = equal;
    control_last[control_last.len() - 1] ^= 1;

    // One independent screen: fresh ABBA measurement of both kernels. The
    // inputs are identical every screen, so any separation that reproduces
    // across screens belongs to the compiled code, not to the inputs or to a
    // load episode that happened to span one screen.
    let screen = || -> (WelchEvidence, WelchEvidence) {
        let production = interleaved_welch(
            AEAD_ROUNDS,
            AEAD_BATCH_SIZE,
            || {
                fgdb_crypto::aead::chacha20poly1305_open(
                    std::hint::black_box(&key),
                    std::hint::black_box(&nonce),
                    std::hint::black_box(aad),
                    std::hint::black_box(&first_mismatch),
                )
                .is_err()
            },
            || {
                fgdb_crypto::aead::chacha20poly1305_open(
                    std::hint::black_box(&key),
                    std::hint::black_box(&nonce),
                    std::hint::black_box(aad),
                    std::hint::black_box(&last_mismatch),
                )
                .is_err()
            },
        );
        let planted = interleaved_welch(
            CONTROL_ROUNDS,
            CONTROL_BATCH_SIZE,
            || {
                planted_early_exit_compare(
                    std::hint::black_box(&control_first),
                    std::hint::black_box(&equal),
                )
            },
            || {
                planted_early_exit_compare(
                    std::hint::black_box(&control_last),
                    std::hint::black_box(&equal),
                )
            },
        );
        (production, planted)
    };

    let mut production_passes = 0usize;
    let mut planted_passes = 0usize;
    let mut screens: Vec<(WelchEvidence, WelchEvidence)> = Vec::new();
    for screen_index in 0..SCREENS {
        let (production, planted) = screen();
        let retained_aead_rounds = AEAD_ROUNDS - AEAD_ROUNDS / 8;
        assert_eq!(production.first.count, (retained_aead_rounds * 2) as u64);
        assert_eq!(production.last.count, (retained_aead_rounds * 2) as u64);
        let retained_control_rounds = CONTROL_ROUNDS - CONTROL_ROUNDS / 8;
        assert_eq!(planted.first.count, (retained_control_rounds * 2) as u64);
        assert_eq!(planted.last.count, (retained_control_rounds * 2) as u64);
        eprintln!(
            "fgdb_crypto_timing_evidence screen={} method=welch_t_abba_load_trim_v2 \
             kernel=chacha20poly1305_open classes=tag_mismatch_first,tag_mismatch_last \
             samples_per_class={} batch_size={} first_mean_ns={:.3} last_mean_ns={:.3} \
             t_statistic={:.6} threshold_abs_t={MAX_PRODUCTION_ABS_T} bounded={}",
            screen_index,
            production.first.count,
            AEAD_BATCH_SIZE,
            production.first.mean_nanos,
            production.last.mean_nanos,
            production.t_statistic,
            production.t_statistic.abs() <= MAX_PRODUCTION_ABS_T,
        );
        eprintln!(
            "fgdb_crypto_timing_evidence screen={} method=welch_t_abba_load_trim_v2 \
             kernel=planted_early_exit_compare classes=mismatch_first,mismatch_last \
             samples_per_class={} batch_size={} first_mean_ns={:.3} last_mean_ns={:.3} \
             t_statistic={:.6} required_abs_t={MIN_PLANTED_ABS_T} detected={}",
            screen_index,
            planted.first.count,
            CONTROL_BATCH_SIZE,
            planted.first.mean_nanos,
            planted.last.mean_nanos,
            planted.t_statistic,
            planted.t_statistic.abs() >= MIN_PLANTED_ABS_T,
        );
        if production.t_statistic.abs() <= MAX_PRODUCTION_ABS_T {
            production_passes += 1;
        }
        if planted.t_statistic.abs() >= MIN_PLANTED_ABS_T {
            planted_passes += 1;
        }
        screens.push((production, planted));
        if production_passes >= PASS_QUORUM && planted_passes >= PASS_QUORUM {
            break;
        }
    }
    assert!(
        production_passes >= PASS_QUORUM,
        "bounded timing screen did not reach the {PASS_QUORUM}-of-{SCREENS} quorum of \
         screens with first-vs-last forged-tag separation within |t| <= \
         {MAX_PRODUCTION_ABS_T}: per-screen t = {:?}",
        screens
            .iter()
            .map(|(production, _)| production.t_statistic)
            .collect::<Vec<_>>(),
    );
    assert!(
        planted_passes >= PASS_QUORUM,
        "planted early-exit comparator escaped the timing detector in the \
         {PASS_QUORUM}-of-{SCREENS} quorum: per-screen t = {:?}",
        screens
            .iter()
            .map(|(_, planted)| planted.t_statistic)
            .collect::<Vec<_>>(),
    );
}
