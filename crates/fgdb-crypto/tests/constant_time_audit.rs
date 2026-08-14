//! The constant-time review lane (bead fgdb-w1-crypto-y5o, increment 5).
//!
//! The bead asks for "a code audit lane asserting no secret-dependent branches
//! or table indices". This file IS that lane: the audit verdict for every
//! module in the crate, plus the two mechanical checks that stop the audit
//! going quietly stale.
//!
//! **WHAT THIS IS NOT, said first so the green bar is not over-read.** This is a
//! REVIEW artifact, not a timing measurement. It contains no dudect-style
//! statistical timing tests and makes no claim that the compiled cipher code is
//! constant-time on any particular microarchitecture. The separately
//! registered `w1_crypto_codegen_e2e` gate now inspects the optimized zeroization
//! boundary, but that narrow witness does not generalize to every kernel. §12.5
//! is explicit that "the vectors pass therefore it is secure" is not an
//! inference this project makes. Constant-time behaviour remains a
//! `bounded_model` claim with named methodology, and the statistical lane plus
//! the release-blocking external audit are still owed by this bead.
//!
//! **WHY THE AUDIT IS A TABLE AND NOT A COMMENT.** An audit written as prose in
//! a doc comment is true on the day it is written and unfalsifiable afterwards.
//! `every_module_has_an_audit_verdict` enumerates `src/*.rs` from disk at run
//! time, so a NEW module — a future SIMD kernel, a key-wrap routine — cannot
//! join the crate without someone recording what its secret-dependence is. That
//! is the completeness guard; without it this file would be decoration.

use std::collections::BTreeSet;
use std::path::PathBuf;

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
const AUDIT: [(&str, Verdict, &str); 9] = [
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

/// Every generic `Secret<N>` drop reaches the one release-codegen boundary.
///
/// The release-object gate can only prove something about the public
/// `scrub_slice` symbol if `Secret::scrub` actually delegates to it. Keeping the
/// linkage assertion beside the source audit makes the two halves mutually
/// load-bearing: a surviving symbol that no secret calls is not evidence.
#[test]
fn secret_scrub_delegates_to_codegen_witnessed_boundary() {
    let source = std::fs::read_to_string(crate_root().join("src/zeroize.rs"))
        .expect("zeroize.rs is readable");

    fn code_lines(body: &str) -> Vec<&str> {
        body.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .collect::<Vec<_>>()
    }

    let scrub_start = source
        .find("pub fn scrub(&mut self)")
        .expect("Secret::scrub is present");
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
}
