//! Scrubbing key material on drop, without `unsafe`.
//!
//! Increment 4 of bead fgdb-w1-crypto-y5o. Every secret this crate handles — a
//! `K_oid`, a DEK, a KEK, an Argon2id output — ultimately occupies byte or word
//! storage, and storage that is simply dropped leaves its contents in the
//! freed allocation or on the stack. [`Secret`] is the fixed-size byte wrapper;
//! [`scrub_slice`] and [`scrub_words`] are the boundaries for owned primitive
//! state whose surrounding type supplies its own `Drop` implementation.
//!
//! **WHAT THIS CAN AND CANNOT PROMISE, stated up front because the usual
//! zeroization claim is overstated.** This crate is `#![forbid(unsafe_code)]`,
//! so the customary tool — `core::ptr::write_volatile` — is unavailable, and
//! `forbid` cannot be lowered (see AGENTS.md's toolchain rule: `unsafe` lives
//! only in the separately named `fgdb-unsafe-*` boundary crates). What is
//! available in safe code is an ordinary overwrite followed by
//! [`compiler_fence`]. The fence is relevant to same-thread asynchronous
//! observers, but Rust does **not** specify it as a general compiler barrier or
//! as a guarantee that a dead store survives optimization.
//!
//! The live `w1_crypto_codegen_e2e` gate therefore makes the narrower claim we
//! can actually witness: for the pinned host toolchain, the optimized production
//! object retains the zeroing call at the non-inlined boundary below. That is a
//! measured artifact property, not a language-level promise and not the claim
//! "the secret is gone":
//!
//! - IT DOES catch the observed failure mode where removing the fence lets the
//!   pinned optimizer delete the final write; the gate mutation-proves that
//!   distinction on the current toolchain.
//! - IT DOES NOT reach copies the compiler already made — spilled registers, a
//!   moved-from temporary, a `Vec` reallocation that copied and freed the old
//!   buffer. [`Secret`] is fixed-size and non-`Clone` to avoid exposing APIs
//!   that deliberately multiply those copies, but the compiler may still move
//!   bytes in ways this type cannot observe or scrub.
//! - IT DOES NOT defeat an attacker who can read the process's memory while it
//!   is running. Zeroization narrows a post-hoc window (core dump, swap,
//!   freed-page reuse); it is not a confidentiality control.
//!
//! Anything stronger belongs behind a ledgered `fgdb-unsafe-*` boundary with a
//! volatile write, and is deliberately NOT claimed here. §12.5's rule against
//! "functional vectors pass therefore secure" applies to this file too: the
//! test below proves the scrub RUNS, not that no copy survives anywhere.

use core::sync::atomic::{Ordering, compiler_fence};

/// A fixed-size secret that scrubs itself on drop.
///
/// **NOT `Clone`, NOT `Copy`, and `Debug` does not print it.** Each of those is
/// load-bearing rather than stylistic: `Copy` would let a secret be duplicated
/// by a move the scrub never sees, and a derived `Debug` is how key material
/// reaches a log line — which the bead's own logging rule forbids ("never keys,
/// nonces, or plaintext").
pub struct Secret<const N: usize> {
    bytes: [u8; N],
}

impl<const N: usize> Secret<N> {
    /// Take ownership of secret bytes.
    ///
    /// Takes by value: a `&[u8; N]` source would leave the caller's copy
    /// unscrubbed and make this type's guarantee a fiction.
    pub fn new(bytes: [u8; N]) -> Self {
        Secret { bytes }
    }

    /// A secret of all zeros, to be filled in place.
    pub fn zeroed() -> Self {
        Secret { bytes: [0u8; N] }
    }

    /// Borrow the bytes.
    ///
    /// Deliberately the only reader, and deliberately a borrow: there is no
    /// `into_inner`, because handing the array out by value would produce a
    /// copy this type cannot scrub.
    pub fn expose(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Mutably borrow the bytes, for a primitive that fills a buffer.
    pub fn expose_mut(&mut self) -> &mut [u8; N] {
        &mut self.bytes
    }

    /// Overwrite with zeros now, without waiting for drop.
    ///
    /// Exposed because a long-lived structure may want to release a secret at a
    /// point its owner chooses rather than at scope exit — the KEK after the
    /// DEKs are unwrapped, for instance.
    pub fn scrub(&mut self) {
        scrub_slice(&mut self.bytes);
    }
}

impl<const N: usize> Drop for Secret<N> {
    fn drop(&mut self) {
        self.scrub();
    }
}

/// Redacted: a secret must never reach a log line through its own formatter.
impl<const N: usize> core::fmt::Debug for Secret<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Secret<{N}>(redacted)")
    }
}

impl<const N: usize> From<[u8; N]> for Secret<N> {
    fn from(bytes: [u8; N]) -> Self {
        Secret::new(bytes)
    }
}

/// Scrub a caller-owned byte slice in place.
///
/// For buffers this crate does not own the type of — a decrypted plaintext
/// `Vec`, an intermediate Argon2 block — where wrapping in [`Secret`] is not
/// possible. Same guarantee and same limits as [`Secret::scrub`].
///
/// This is deliberately one non-inlined code-generation boundary. Every
/// [`Secret`] drop delegates here, and `w1_crypto_codegen_e2e.sh` inspects the
/// optimized production object to require the zeroing call to survive. Without
/// one stable boundary, generic drop glue would be monomorphized into whichever
/// downstream crate happened to use a particular `Secret<N>`, making the
/// claimed code-generation evidence impossible to enumerate honestly.
#[inline(never)]
pub fn scrub_slice(bytes: &mut [u8]) {
    bytes.fill(0);
    // This is not a portable dead-store-elision guarantee. The live codegen
    // gate witnesses that the fill survives in the pinned optimized host
    // object, and goes red when this fence is removed on that toolchain.
    compiler_fence(Ordering::SeqCst);
}

/// Scrub a caller-owned machine-word slice in place.
///
/// Argon2's memory matrix and BLAKE2b's compression state are natively arrays
/// of `u64`. Scrubbing those arrays through their original word storage avoids
/// first manufacturing a byte copy that would itself need erasure. This has
/// exactly the measured-toolchain guarantee and no-claim boundary documented
/// for [`scrub_slice`].
#[inline(never)]
pub fn scrub_words(words: &mut [u64]) {
    words.fill(0);
    compiler_fence(Ordering::SeqCst);
}
