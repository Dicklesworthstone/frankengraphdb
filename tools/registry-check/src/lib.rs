//! registry-check — G0 claim-registry validator (fgdb-g0-claim-registries-myx).
//!
//! Enforces the claim constitution (plan §1 constraints 11–12, Appendix F):
//! schema validation of `registries/{constitution,invariants,evidence,slo,
//! proof_lanes,checker_index}.toml`, the class-strength lattice, the
//! twenty-ID invariant spine hash pin, claims-lint over the normative prose
//! artifacts, and the compiled activation closure over a capability manifest.
//!
//! Std-only by constitution: the closed dependency universe (FG-CON-01)
//! applies to the tooling that enforces it, so the TOML parser, predicate
//! language, JSON emitter, and hash pin are all in-house.

pub mod closure;
pub mod hash;
pub mod jsonl;
pub mod lint;
pub mod model;
pub mod predicate;
pub mod toml;
pub mod validate;
