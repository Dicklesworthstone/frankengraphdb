# Warden first-party capability core

This crate contains the saved implementation of foundation-backed macaroon
issuance, attenuation and verification, immutable graph predicates, typed
read/write execution permits and checked per-execution ceilings. It reuses
asupersync at the workspace's existing exact revision; it adds no direct
third-party cryptography dependency.

**Status: published implementation, not compiled or runtime-validated in the
publication environment, and not integrated into database query/storage access.**
It is not FG-INV-20 acceptance, the complete plan section 12 token protocol,
or a production-ready authorization boundary.

## Authenticated subset

The signed identifier binds database security namespace, graph name, catalog
epoch and policy epoch. Issuance attaches all ten required restrictions before
returning bearer bytes. Verification checks the HMAC chain and compiles every
accepted caveat. Unknown caveats and unsupported foundation caveat families
fail closed. Location is an unsigned routing hint and grants no authority.

Branch restrictions must agree. Relation/property masks intersect; property
denials accumulate; rights intersect; ceilings and expiry take minima;
not-before takes the maximum. Multiple finite label restrictions remain a
conjunction of any-label clauses; their metadata mask is the intersection.
This distinction matters for vertices with more than one label.

Token ingress is limited to 32 KiB and 64 caveats, with canonical re-encoding
rejecting unused predicate-packet bytes. Keys and bearer material are redacted
from token, issuer and verified-capability Debug implementations. A macaroon
signature is an attenuation secret, never a public certificate/cache key.

Read/write permits are different Rust types. Expiry or an overflowing/exceeded
budget terminally stops that execution. `with_relation_at` refuses a hidden
relation before calling its descriptor opener; callers must additionally check
both endpoints before admitting an edge. Limits are per execution, not lifetime
quotas, rate limits or shared tenant accounting. All supplied times must be
trusted host observations in the same clock epoch, not client input.

## Required integration before security acceptance

The existing `fgdb-policy` crate and database sources are unchanged. Warden's
current label clauses use any-label membership while `ReadCaveat::Labels` uses
all-label containment. These are not interchangeable wire meanings. Reconcile
and version the policy semantics explicitly before bridging either into a
mandatory secure-view boundary; do not silently reinterpret retained tokens.

Both the borrowed-Snapshot fast path and owned fallback of
`AdmittedGqlSnapshot` require source-level enforcement. Cover every scan,
transit vertex, descriptor, degree, index, property predicate/projection,
aggregate, statistic, catalog output, certificate and error before observation.
A result post-filter or a wrapper around only the owned fallback is insufficient.
Writes need authorization over complete before/after images, touched properties,
relations and endpoints, independently checked by the coordinator. Neither the
raw database nor issuer key may be exposed to untrusted callers.

The normative root issuance receipt/counter protocol, stable per-root token ID,
tenant/principal/issuer and presentation bindings, complete current security
state binding, persistent policy/revocation state, per-token revocation,
third-party discharges, leases and in-flight output revocation fencing are not
implemented. Changing the host-selected policy epoch rejects old tokens at new
admissions; it does not cancel already borrowed permits. Runtime Cx narrowing,
scalar policy caveats, named catalog/glob resolution and lifetime quotas also
remain outstanding. The current bytes are an unreleased core envelope, not the
complete durable `MacaroonTokenBytes` contract in plan section 12.2.

## Evidence and validation status

`tests/security.rs` contains 33 authored tests: tampering, caveat removal,
cross-authority substitution, widening attempts, exhaustive three-label
conjunction checks, descriptor non-opening, endpoint/degree masking, expiry,
budget overflow, bounds, malformed encodings and secret redaction. The core
also has one compile-fail read-to-write promotion doctest. These tests are
source fixtures, not evidence of an executed passing suite.

On 2026-09-22, both TOML manifests parsed and `git diff --cached --check`
passed for the new crate source/test files. The three published core Rust
blobs were checked against the saved source hashes and matched exactly.
`cargo test -p fgdb-warden` could not start: Cargo is absent (exit 127).
Rustfmt, check, Clippy, tests, UBS, full registry gates and exact-tree local
proof are UNRUN. Cargo.lock and workspace-topology/registry reconciliation
remain outstanding. No acceptance bead or invariant is marked complete.

Run the following in a complete checkout with the pinned toolchain, after
reconciling the lockfile and registry entries; fix failures before security
acceptance:

```sh
cargo fmt --all
cargo test -p fgdb-warden
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
bash scripts/check.sh
```

The project AGENTS.md and comprehensive plan remain authoritative.
