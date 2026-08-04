//! Entropy-separation tests (bead fgdb-w1-crypto-y5o, increment 4b).
//!
//! The bead's requirement: "lab-runtime replay seeds never influence `CryptoCx`
//! production entropy". The hazard being defended against is real and pinned to
//! a measurement — asupersync installs a deterministic `EntropySource` under the
//! lab runtime, so `Cx::random_bytes` is a function of the replay seed there.
//! Drawing a key from it would produce one reproducible by anyone holding a
//! seed, and B5 encourages publishing seeds in crashpacks.
//!
//! **A NEGATIVE PROPERTY NEEDS A POSITIVE CONTROL.** "The production source is
//! not seeded" is untestable on its own: a source that is never exercised, or
//! one whose output nobody compares across runs, looks unseeded. Every test
//! below that asserts non-reproducibility is therefore paired with
//! `DeterministicEntropy`, which genuinely does reproduce — so the assertion is
//! known to be capable of failing.

use fgdb_crypto::cx::{CryptoCx, DeterministicEntropy, EntropyError, EntropySource, SystemEntropy};

/// THE LOAD-BEARING TEST: production entropy is not reproducible.
///
/// Two secrets minted from the production handle must differ. With 32 bytes a
/// collision from a healthy CSPRNG is not going to happen; what this actually
/// catches is the failure that matters — a source wired to a constant, a
/// counter, or a seed.
#[test]
fn production_entropy_is_never_seeded() {
    let cx = CryptoCx::production();

    assert_eq!(
        cx.entropy_source_id(),
        "system",
        "the production handle is not on the OS source"
    );
    assert!(
        !cx.is_deterministic(),
        "the production handle reports itself reproducible, which is the exact \
         property a key source must not have"
    );

    let first = cx.secret::<32>().expect("OS entropy is available");
    let second = cx.secret::<32>().expect("OS entropy is available");
    assert_ne!(
        first.expose(),
        second.expose(),
        "two production secrets came out identical — the source is seeded, \
         constant, or not being read at all"
    );

    // And not the trivial failure of returning an unwritten buffer.
    assert_ne!(
        first.expose(),
        &[0u8; 32],
        "the production source returned an all-zero secret"
    );
}

/// THE CONTROL: a seeded source really does reproduce.
///
/// Without this, `production_entropy_is_never_seeded` proves nothing — it would
/// pass against a test suite that simply never exhibits a reproducible source.
/// This is what makes that assertion falsifiable.
#[test]
fn a_seeded_source_reproduces_exactly() {
    let first = CryptoCx::new(DeterministicEntropy::for_test(0xfeed))
        .secret::<32>()
        .expect("the deterministic source cannot fail");
    let second = CryptoCx::new(DeterministicEntropy::for_test(0xfeed))
        .secret::<32>()
        .expect("the deterministic source cannot fail");

    assert_eq!(
        first.expose(),
        second.expose(),
        "the control source did not reproduce, so it cannot witness the negative \
         property the production test asserts"
    );

    // A different seed gives different bytes — otherwise "reproduces" would be
    // satisfied by a constant, which is a weaker control.
    let other = CryptoCx::new(DeterministicEntropy::for_test(0xbeef))
        .secret::<32>()
        .expect("the deterministic source cannot fail");
    assert_ne!(
        first.expose(),
        other.expose(),
        "two seeds produced one stream; the control is a constant, not a seeded source"
    );
}

/// A deterministic handle must ANNOUNCE itself, both ways.
///
/// This is what lets a caller — or a future gate — refuse to mint durable key
/// material under a replay handle, instead of having to infer it from a type
/// name at the construction site.
#[test]
fn a_deterministic_handle_is_self_identifying() {
    let cx = CryptoCx::new(DeterministicEntropy::for_test(1));
    assert!(cx.is_deterministic(), "a seeded handle must admit it");
    assert_ne!(
        cx.entropy_source_id(),
        "system",
        "a seeded handle must not claim the production source id"
    );
    assert_eq!(cx.entropy_source_id(), "deterministic-test-only");

    // The two handles are distinguishable on both signals, not just one.
    let production = CryptoCx::production();
    assert_ne!(cx.entropy_source_id(), production.entropy_source_id());
    assert_ne!(cx.is_deterministic(), production.is_deterministic());
}

/// An unavailable OS source fails closed rather than yielding weak bytes.
///
/// The dangerous version of this failure is silent: a source that cannot be
/// read and returns its zeroed buffer produces an all-zero "key" that every
/// downstream check accepts. The error path is the whole point.
#[test]
fn an_unavailable_source_fails_closed() {
    let missing = SystemEntropy::from_path_for_test("/nonexistent/entropy/device");
    let cx = CryptoCx::new(missing);

    let result = cx.secret::<32>();
    assert!(
        matches!(result, Err(EntropyError::Unavailable { .. })),
        "a missing entropy device must error, not substitute a weaker source"
    );

    // The error must not carry key material, and must name the source.
    if let Err(error) = result {
        let rendered = format!("{error}");
        assert!(
            rendered.contains("system"),
            "the error should name the source it came from: {rendered}"
        );
        assert!(
            rendered.contains("refusing"),
            "the error should say it refused to substitute: {rendered}"
        );
    }
}

/// A short read must not leave the tail of a key unwritten.
///
/// `/dev/null` opens successfully and yields zero bytes, which is precisely the
/// shape that a `read` (rather than `read_exact`) would turn into a
/// half-initialized key while reporting success.
#[test]
fn a_source_that_opens_but_yields_nothing_still_fails_closed() {
    let empty = SystemEntropy::from_path_for_test("/dev/null");
    let cx = CryptoCx::new(empty);

    assert!(
        matches!(cx.secret::<32>(), Err(EntropyError::Unavailable { .. })),
        "a source that opens and returns no bytes must fail, not hand back the \
         zeroed buffer as a secret"
    );
}

/// The two halves of increment 4 are joined: a minted secret is a scrubbing
/// `Secret`, not a bare array a caller has to remember to wipe.
#[test]
fn minted_secrets_are_scrubbing_secrets() {
    assert!(
        core::mem::needs_drop::<fgdb_crypto::zeroize::Secret<32>>(),
        "the type CryptoCx mints must still scrub on drop"
    );

    let mut secret = CryptoCx::production()
        .secret::<32>()
        .expect("OS entropy is available");
    secret.scrub();
    assert_eq!(secret.expose(), &[0u8; 32], "a minted secret must scrub");
}

/// The trait is usable behind a reference, so a component can hold a source it
/// did not construct without being generic over it at every layer.
#[test]
fn the_source_trait_is_object_safe_enough_to_pass_around() {
    fn id_of(source: &dyn EntropySource) -> &'static str {
        source.source_id()
    }
    assert_eq!(id_of(&SystemEntropy::new()), "system");
    assert_eq!(
        id_of(&DeterministicEntropy::for_test(7)),
        "deterministic-test-only"
    );
}
