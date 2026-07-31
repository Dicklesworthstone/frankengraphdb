//! Identity-constitution validation (bead fgdb-g0-identity-registries-hrx).
//!
//! Loads and validates the five disjoint identity-class registries plus the
//! `durable_fields.toml` cross-index (plan §5.1):
//!
//!   logical_object_kinds.toml        keyed-ObjectId logical schemas
//!   physical_record_kinds.toml       non-ObjectId identity laws
//!   bootstrap_frames.toml            fixed-location mutable frames
//!   prebootstrap_artifact_kinds.toml restore artifacts predating K_oid
//!   wire_types.toml                  embedded canonical types / closed tags
//!   durable_fields.toml              the sole per-field cross-index +
//!                                    ordinary and generated reference unions
//!
//! Violation codes (stable, asserted by negative fixtures):
//!   code_invalid            code is 0x0000/0xffff or outside u16
//!   code_duplicate          code/tag reuse (retired codes are never reassigned)
//!   experimental_in_production  0xc000..=0xfffe row in a shipped registry
//!   range_status_mismatch   status/code-range coherence violation
//!   disjointness_dual_class one schema name in two identity classes
//!   field_unresolved_schema containing_schema resolves nowhere
//!   arm_payload_shape_field_row  a StrongRef-only union-arm payload shape left
//!                           the wire path or grew a field row
//!   field_unresolved_wire_type  exact_wire_type resolves nowhere
//!   bare_strong_ref         polymorphic strong ref without a generated union
//!   ref_target_not_logical  strong/conditional target outside class 1
//!   ref_target_unresolved   named target resolves nowhere
//!   frame_strong_ref        bootstrap frame with a retaining reference
//!   union_field_mismatch    union not anchored to its declaring field row
//!   union_arm_duplicate_tag duplicate arm tag in one union
//!   union_arm_unresolved    arm target is not a live logical row
//!   ordinary_union_duplicate_path  two ordinary unions claim one schema path
//!   ordinary_union_name_collision ordinary/reference union name collision
//!   reference_union_name_collision reference union shadows another wire type
//!   ordinary_union_unresolved_schema containing schema has no unique identity class
//!   ordinary_union_wire_contract_mismatch top-level union/wire cross-index drift
//!   ordinary_union_logical_contract_mismatch whole-schema union/logical kind or consumer drift
//!   ordinary_union_container_contract_mismatch open or inconsistent consumer closure
//!   ordinary_union_arm_duplicate_tag duplicate ordinary-union arm tag
//!   ordinary_union_arm_metadata_mismatch arm does not match its union owner
//!   ordinary_union_arm_lifecycle_mismatch arm outlives its ordinary union
//!   ordinary_union_arm_role_mismatch arm role scope exceeds its union
//!   dag_self_edge / dag_cycle / dag_future_result   construction-DAG faults
//!   digest_missing_class    digest-typed field without a declared class
//!   digest_missing_recipe   transcript digest without a recipe
//!   bodydigest_two_fields   two BodyDigest rows in one schema
//!   bodydigest_unknown_exclusion  include/exclude names an unregistered tag
//!   bodydigest_self_included      the digest's own tag is not excluded
//!   bodydigest_pin_mismatch       recipe drift against the FNV pin
//!   unregistered_field      encodability check: field not in the table
//!   refinement_claim_unparseable  arm-refinement prose outside the grammar
//!   refinement_conjunction_invalid  conjunctive refinement is malformed or duplicates a location
//!   refinement_union_unresolved   refined union is not a registered union
//!   refinement_arm_unresolved     refined arm name is not an arm of that union
//!   refinement_arm_tag_mismatch   arm resolves but under a different arm_tag
//!   refinement_tag_only_payload_unaccounted  a tag-only discriminant refines
//!                           a payload-bearing arm without accounting for the
//!                           payload in its claim
//!   arm_value_claim_missing  a payload-preserving arm value carries no
//!                           refinement claim at all
//!   arm_value_conjunction_invalid  an arm value claims a two-location
//!                           conjunction, which is a precondition shape
//!   arm_value_on_unit_payload  an arm value refines a unit-payload arm,
//!                           where the tag-only discriminant is the complete
//!                           and smaller instrument
//!   arm_value_payload_pin_missing  an arm value carries no parseable
//!                           complete-payload pin
//!   arm_value_payload_pin_malformed  an arm value advertises the pin but
//!                           breaks its closed spelling
//!   arm_value_payload_pin_mismatch  the pinned arm name or payload digest
//!                           disagrees with the resolved registered arm
//!   restore_service_promotion_manifest_coherence  manifest posture, BODY, and
//!                           authority-profile tag domains are not bound to
//!                           their one legal cross-field truth table
//!   bad_field               enum/shape violation

use crate::hash::fnv1a64;
use crate::model::LoadError;
use crate::toml::{
    self, ReadError, Table, get_int, get_opt_str, get_str, get_str_array, get_table,
    get_table_array,
};
use crate::validate::Violation;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Builtin scalar wire types (documented in durable_fields.toml).
/// `digest256` REQUIRES a declared digest_class; `id256`/`oid256` are raw
/// 256-bit identities, not digests-of-something.
pub const BUILTIN_WIRE_TYPES: [&str; 11] = [
    "u8",
    "u16",
    "u32",
    "u64",
    "i64",
    "bool",
    "bytes",
    "string",
    "id256",
    "digest256",
    "oid256",
];

/// The sole typed refinement that can cut a co-phased schema edge.
///
/// Keep this vocabulary in one reader: the loader, validator, DAG builder, and
/// robot output all consume `FieldRow::construction_relation`; no second
/// allowlist is permitted to decide which edges are instance-prior.
pub const PRIOR_OBJECT_CONSTRUCTION_RELATION: &str = "prior_object";

/// The `reference_semantics` a field row MUST carry, given its `exact_wire_type`.
///
/// LAW: **the wire tag declares the reference strength.** Appendix A: "Every
/// ObjectId-bearing edge declares a wire tag: `StrongRef{oid}` (always
/// followed); `StrongMarkerRef`/`StrongCommandRef` (retain the named history
/// object); `ConditionalCoordinateRef` ...; `ConditionalMarkerRef`/
/// `ConditionalCommandRef` (followed until an authenticated matching
/// checkpoint/cut); `WeakMarkerIdentity` (provenance/identity only);
/// ... or `WeakDigest{digest}` (comparison only)", and for the W12 wrappers
/// "Strong variants retain, conditional variants stop only at a verified
/// matching meta/shard checkpoint cut, and weak variants compare only."
///
/// The declaration was already enforced on Appendix A catalog ANNOTATIONS
/// (`catalog_annotation_reference_semantics_mismatch`) and on nothing else, so a
/// `[[field]]` row typed `StrongRef` could declare `reference_semantics =
/// "none"`, keep its target, and pass every gate — silently switching off
/// `dag_future_result`, `bare_strong_ref`, and every generated reachability /
/// GC / checkpoint-vector walker for that member, then freezing behind the
/// append-only field pin (fgdb-refsem-not-forced-by-wire-type-gls4).
///
/// Delegates to the one table rather than restating it. Two spelling
/// adjustments, both forced by the same prose:
///   * catalog `"identity"` (bare `MarkerRef`/`CommandRef`, which Appendix A
///     calls "identities, **not reachability by themselves**") has no field
///     spelling; a member that creates no edge carries `"none"`.
///   * the three W12 weak-identity tags are wire tags, not catalog *definition*
///     families, so the catalog table does not name them. "weak variants
///     compare only" — and both landed rows carry `"none"` with an explicit
///     "creates no reachability edge" retention rule.
///
/// `None` means the type declares nothing; such a row is constrained instead by
/// `reference_semantics_without_reference_type` plus the target/union guards.
fn declared_field_reference_semantics(exact_wire_type: &str) -> Option<&'static str> {
    match crate::appendix_a::registered_reference_definition_semantics(exact_wire_type) {
        Some("identity") => Some("none"),
        Some(other) => Some(other),
        None => match exact_wire_type {
            "WeakGlobalCommandIdentity" | "WeakMarkerIdentity" | "WeakShardCommandIdentity" => {
                Some("none")
            }
            _ => None,
        },
    }
}

/// The marker that opens an arm-refinement claim in a wire tag's
/// `encoding_context`. A tag-refined wrapper is a wire type that admits a
/// STRICT SUBSET of its union's arms — Appendix A a20:2593 mints two of them
/// and states the rule they exist to serve: "variant syntax is never used as a
/// reference target."
pub const REFINEMENT_CLAIM_MARKER: &str = "admits only the ";

/// The marker for one atomic two-location refinement. Exactly two is
/// deliberate: this is the minimum language needed for source spellings such
/// as `OperationalPendingIndependentReopen::Sealed`, without growing a general
/// Boolean-expression language inside registry prose.
pub const REFINEMENT_CONJUNCTION_MARKER: &str = "admits only when both the ";

/// Why a refinement claim that advertises itself as machine-readable could not
/// be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefinementClaimParseError {
    Single,
    Conjunction,
}

type RefinementClause<'a> = (&'a str, i64, &'a str);

/// Parse one `<Arm> arm (arm_tag 0x<hex>) of the <Union> union` clause and
/// return the unconsumed suffix.
fn parse_refinement_clause(input: &str) -> Option<(RefinementClause<'_>, &str)> {
    let (arm, rest) = input.split_once(" arm (arm_tag 0x")?;
    let (tag_hex, rest) = rest.split_once(") of the ")?;
    let (union, tail) = rest.split_once(" union")?;
    if arm.is_empty() || union.is_empty() {
        return None;
    }
    // Each name is one identifier. A multi-word phrase ("Sharded/ExternalCas
    // RestoreServicePromotionManifest") is the prose dialect, not this grammar.
    if arm.contains(' ') || union.contains(' ') {
        return None;
    }
    let tag = i64::from_str_radix(tag_hex, 16).ok()?;
    Some(((arm, tag, union), tail))
}

/// The canonical, machine-readable spellings of an arm-refinement claim:
///
/// ```text
/// admits only the <SourceArmName> arm (arm_tag 0x<hex>) of the <Union> union
/// admits only when both the <Arm1> arm (arm_tag 0x<hex>) of the <Union1> union
///     and the <Arm2> arm (arm_tag 0x<hex>) of the <Union2> union
/// ```
///
/// A conjunction is one claim with exactly two DISTINCT locations. It is not
/// two independent claims: every returned clause must resolve or the row
/// fails. Repeated markers, a third clause, mixing the two dialects, and a
/// duplicate clause all fail closed.
///
/// `None` means the row makes no refinement claim. `Some(Err(_))` means it
/// advertises one but is outside its grammar; callers MUST report that rather
/// than skip it. Union generic suffixes remain intact here and are stripped at
/// lookup.
fn parse_refinement_claim(
    encoding_context: &str,
) -> Option<Result<Vec<RefinementClause<'_>>, RefinementClaimParseError>> {
    let single_count = encoding_context.matches(REFINEMENT_CLAIM_MARKER).count();
    let conjunction_count = encoding_context
        .matches(REFINEMENT_CONJUNCTION_MARKER)
        .count();

    match (single_count, conjunction_count) {
        (0, 0) => None,
        (1, 0) => {
            let Some((_, rest)) = encoding_context.split_once(REFINEMENT_CLAIM_MARKER) else {
                return Some(Err(RefinementClaimParseError::Single));
            };
            let parsed = parse_refinement_clause(rest)
                .ok_or(RefinementClaimParseError::Single)
                .and_then(|(clause, tail)| {
                    if tail.trim_start().starts_with("and the ") {
                        Err(RefinementClaimParseError::Single)
                    } else {
                        Ok(vec![clause])
                    }
                });
            Some(parsed)
        }
        (0, 1) => {
            let Some((_, rest)) = encoding_context.split_once(REFINEMENT_CONJUNCTION_MARKER) else {
                return Some(Err(RefinementClaimParseError::Conjunction));
            };
            let parsed = parse_refinement_clause(rest)
                .ok_or(RefinementClaimParseError::Conjunction)
                .and_then(|(first, rest)| {
                    let rest = rest
                        .strip_prefix(" and the ")
                        .ok_or(RefinementClaimParseError::Conjunction)?;
                    let (second, tail) = parse_refinement_clause(rest)
                        .ok_or(RefinementClaimParseError::Conjunction)?;
                    if first == second || tail.trim_start().starts_with("and the ") {
                        return Err(RefinementClaimParseError::Conjunction);
                    }
                    Ok(vec![first, second])
                });
            Some(parsed)
        }
        (0, _) => Some(Err(RefinementClaimParseError::Conjunction)),
        (_, 0) => Some(Err(RefinementClaimParseError::Single)),
        _ => Some(Err(RefinementClaimParseError::Conjunction)),
    }
}

/// The registered `union_name` for a prose union spelling: generics are dropped
/// at the first `<`. `RestoreTerminalPinBasis<Role>` and the registered
/// `RestoreTerminalPinBasis` are one union; so are the `<Role:Bound>` spellings.
fn refinement_union_base(name: &str) -> &str {
    match name.split_once('<') {
        Some((base, _)) => base,
        None => name,
    }
}

// ---------------------------------------------------------------------------
// Value-position refinement classes (fgdb-payload-bearing-arm-values-5u56).
//
// The fgdb-gpms ruling minted the tag-refined discriminant for PRECONDITION
// fields: the field gates on which arm the state is in, so it carries the
// refined union's tag and never the arm payload. That instrument is complete
// only while the payload is accounted for. Every value-position refinement
// therefore lands in exactly one of two classes:
//
//   TAG-ONLY PRECONDITION (`kind = "discriminant"`). The field is a gate,
//   not a carrier. Its claim must contain the accounting clause
//   `carries the refined union's u8 tag and never the arm payload` whenever
//   the refined arm is payload-bearing (`payload_kind != "unit"`): either
//   the full stop (the precondition's semantics are tag-level, as on the
//   PromotedAwaitingReopen landing) or a `, while ... bind ...` continuation
//   naming the sibling fields that carry the payload independently (the
//   LocalAbort landing). A payload-bearing arm refined by a discriminant
//   whose claim is silent about the payload is an erasure, not a refinement.
//
//   PAYLOAD-BEARING ARM VALUE (`kind = "arm_value"`). The field IS the arm
//   value — `AuditTicketClaimRecord.owner : AuditTicketOwner::Operation`,
//   where the Operation payload {global_txn_id, registration_generation,
//   operation_request_basis_commitment} is the record's owner identity and
//   no sibling field carries it. The representation carries the refined
//   union's tag AND the complete selected-arm payload, and the claim pins
//   that payload by the digest the census registered for the arm:
//
//     carries the refined union's u8 tag and the complete <SourceArmName>
//     arm payload (payload_sha256 <64 lowercase hex>)
//
//   The pin makes "complete" mechanical: dropping it, corrupting the digest,
//   or naming a different arm all fail closed. An `arm_value` on a
//   unit-payload arm is an obfuscated discriminant; an `arm_value` with a
//   two-location conjunction is a precondition shape wearing the wrong kind.
//
// ---------------------------------------------------------------------------

/// The accounting clause a tag-only discriminant must carry when it refines a
/// payload-bearing arm. Both spellings in the landed corpus contain it
/// verbatim: the full-stop form and the `, while ... bind ...` form.
pub const TAG_ONLY_PAYLOAD_ACCOUNTING_MARKER: &str =
    "carries the refined union's u8 tag and never the arm payload";

/// The prefix of the complete-payload pin an `arm_value` claim must carry.
pub const ARM_VALUE_PAYLOAD_PIN_PREFIX: &str =
    "carries the refined union's u8 tag and the complete ";

/// The infix separating the pinned arm name from its registered digest.
pub const ARM_VALUE_PAYLOAD_PIN_INFIX: &str = " arm payload (payload_sha256 ";

/// One parsed complete-payload pin: the named arm and its claimed digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArmValuePayloadPin<'a> {
    arm: &'a str,
    sha256: &'a str,
}

/// Parse the complete-payload pin out of an `arm_value` `encoding_context`.
///
/// `None` means no pin prefix is present at all (the
/// `arm_value_payload_pin_missing` case). `Some(Err(()))` means the row
/// advertises the pin but breaks its closed spelling — truncated name,
/// missing infix, wrong digest length, non-hex, an unterminated `)`, or a
/// repeated pin marker — which must fail closed rather than be skipped the way
/// a prose claim would be. `Some(Ok(pin))` is one well-formed pin; agreement
/// with the resolved registered arm is the caller's check.
fn parse_arm_value_payload_pin(
    encoding_context: &str,
) -> Option<Result<ArmValuePayloadPin<'_>, ()>> {
    let pin_count = encoding_context
        .matches(ARM_VALUE_PAYLOAD_PIN_PREFIX)
        .count();
    let infix_count = encoding_context
        .matches(ARM_VALUE_PAYLOAD_PIN_INFIX)
        .count();
    // Totality at BOTH levels of the closed spelling: exactly one pin prefix
    // and exactly one digest infix. A repeated prefix is the reviewed
    // fail-open; a repeated digest infix is the same hole one level down — a
    // row could pin one arm correctly and smuggle a contradictory second
    // digest past the ignored tail. Neither is a prose mention to skip.
    match (pin_count, infix_count) {
        (0, 0) => return None,
        (1, 1) => {}
        _ => return Some(Err(())),
    }
    let (_, rest) = encoding_context.split_once(ARM_VALUE_PAYLOAD_PIN_PREFIX)?;
    Some((|| {
        let (arm, rest) = rest.split_once(ARM_VALUE_PAYLOAD_PIN_INFIX).ok_or(())?;
        if arm.is_empty() || arm.contains(' ') {
            return Err(());
        }
        let (sha256, tail) = rest.split_once(')').ok_or(())?;
        if !is_lowercase_sha256(sha256) {
            return Err(());
        }
        let _ = tail;
        Ok(ArmValuePayloadPin { arm, sha256 })
    })())
}

/// Resolve one parsed refinement clause through the ordinary-union catalog.
///
/// Keeping this outside the caller's bounded one-or-two-clause loop also keeps
/// diagnostic construction out of the loop itself.
fn validate_refinement_clause(
    ordinary_unions_by_base: &BTreeMap<&str, Vec<&OrdinaryUnion>>,
    wire_name: &str,
    (arm, tag, union): RefinementClause<'_>,
    out: &mut Vec<Violation>,
) {
    let base = refinement_union_base(union);
    match ordinary_unions_by_base.get(base) {
        None => out.push(v(
            "refinement_union_unresolved",
            "wire_types",
            wire_name,
            format!(
                "refinement names union {union:?}, which has no [[union]] row \
                 under base name {base:?}; a wrapper may not refine a union that \
                 is not registered"
            ),
        )),
        Some(unions) => {
            let named: Vec<&OrdinaryUnionArm> = unions
                .iter()
                .flat_map(|u| u.arms.iter())
                .filter(|a| a.source_arm_name == arm)
                .collect();
            if named.is_empty() {
                out.push(v(
                    "refinement_arm_unresolved",
                    "wire_types",
                    wire_name,
                    format!(
                        "refinement names arm {arm:?} of union {base:?}, which \
                         has no [[union_arm]] row with that source_arm_name"
                    ),
                ));
            } else if !named.iter().any(|a| a.arm_tag == tag) {
                let actual: Vec<String> = named
                    .iter()
                    .map(|a| format!("{:#06x}", a.arm_tag))
                    .collect();
                out.push(v(
                    "refinement_arm_tag_mismatch",
                    "wire_types",
                    wire_name,
                    format!(
                        "refinement claims arm {arm:?} of union {base:?} at \
                         arm_tag {tag:#06x}; the registered arm carries {}",
                        actual.join(", ")
                    ),
                ));
            }
        }
    }
}

/// Resolve one parsed refinement clause to the registered arm rows it names.
///
/// Returns `None` on any resolution failure — unknown union, unknown arm, or
/// tag mismatch — because `validate_refinement_clause` has already emitted
/// the precise violation for that failure, and a payload law must never
/// double-report on an unresolved claim. The `any`-on-tag rule matches the
/// validator: several role instantiations of one generic union may register
/// the arm under different tags, and the claim names the union, not the
/// instantiation, so every instantiation whose arm matches the claimed tag is
/// a resolution.
fn resolve_refinement_clause<'a>(
    ordinary_unions_by_base: &BTreeMap<&str, Vec<&'a OrdinaryUnion>>,
    (arm, tag, union): RefinementClause<'_>,
) -> Option<Vec<&'a OrdinaryUnionArm>> {
    let unions = ordinary_unions_by_base.get(refinement_union_base(union))?;
    let resolved: Vec<&OrdinaryUnionArm> = unions
        .iter()
        .flat_map(|u| u.arms.iter())
        .filter(|a| a.source_arm_name == arm && a.arm_tag == tag)
        .collect();
    if resolved.is_empty() {
        // Either the arm name is unregistered (refinement_arm_unresolved) or
        // every registered instance carries a different tag
        // (refinement_arm_tag_mismatch); both are already reported.
        None
    } else {
        Some(resolved)
    }
}

/// FORWARD DRIFT DETECTOR FROM A STATED BASELINE. **Not** proof that the
/// pre-erratum namespace is reconstructible — fgdb-7yo9 proved it is not.
///
/// What this pin does: the witness rebuilds a historical namespace by removing
/// every post-erratum cohort from the PRESENT registries, hashes it, and
/// compares against this baseline. A change that moves the hash without a
/// matching filter/undo extension is caught. That is a real and useful gate.
///
/// What it does NOT do, and previously claimed to: reconstruct the namespace as
/// it stood before the A10 `CommandRef` erratum. The floor it compared against
/// was computed over 8a704c2's RECONSTRUCTION, while the only surviving
/// artifact of that commit is its RAW FILE. Those are different objects, and
/// requiring equality between them demands something meaningless — a gate that
/// demands the impossible is not stronger, it is permanently red.
///
/// SUPERSEDED HISTORICAL VALUES, recorded and not deleted:
///   `fnv1a64:bdbcdc27ccd92518`  the value 8a704c2 overwrote. Unrecoverable:
///     fgdb-7yo9 proved that filtering exactly the 26 rows 8a704c2 registered
///     yields 71729c11125d59d1, not this.
///   `fnv1a64:236efa5babe190fe`  the floor 8a704c2 re-pinned to, in order to
///     CONCEAL an unexplained mismatch. Unreachable: fgdb-e55p accounted both
///     halves of the transcript with zero remainder — fields 225 = 218 identical
///     + 2 undo-expected + 5 content drift; non-field 37 = 25 identical + 3
///     rename-expected + 9 membership drift — applied both repairs, and the walk
///     a86e33d4020143ef -> 69f3b79b0a6221c0 -> e0245f1bf4c183fd still lands one
///     hash short. The residual is an artifact-class mismatch, not drift.
///
/// This baseline is re-computed THE SAME WAY the witness computes the value it
/// compares, so the assert compares like with like. Extend the filter/undo set
/// when it drifts; re-baselining is an OWNER ruling and must ship its full
/// accounting in the same commit, as this one did (fgdb-e55p).
pub const A10_COMMAND_REF_ERRATUM_PREVIOUS_FIELDS_PIN: &str = "fnv1a64:e0245f1bf4c183fd";

/// One NAMED union-arm payload shape governed by the StrongRef-only
/// arm-payload law (`STRONGREF_ONLY_ARM_PAYLOAD_SHAPES`).
#[derive(Debug, Clone, Copy)]
pub struct ArmPayloadShape {
    /// Generic-free family name, spelled as the source spells it.
    pub name: &'static str,
    /// `slice:line` of the source sentence that defines the shape.
    pub source: &'static str,
    /// The generated union field whose arms carry the shape.
    pub carried_by: &'static str,
    /// Every member of the shape's body. The law admits the shape only while
    /// EVERY member is a retaining reference: one non-reference member and the
    /// shape owes a real field body on a field-owning host instead.
    pub members: &'static [&'static str],
    /// The bead whose ruling placed the shape on the wire path.
    pub ruling: &'static str,
}

/// LAW: **a union-arm payload shape — NAMED OR ANONYMOUS — whose body carries
/// only retaining references takes the WIRE path, and owns NO `[[field]]`
/// row.**
///
/// Stated because it was twice re-derived from the corpus and once decided the
/// wrong way. The corpus reading is unanimous: of 72 landed self-owned ordinary
/// unions registered as a wire type, the number of `[[field]]` rows naming any
/// of them as `containing_schema` is ZERO, and `CommittedDeltaSourceRef.batch_ref`
/// (`Local{batch_ref:StrongRef<LogicalDeltaBatch>,commit_seq}`) is the same edge
/// shape already accepted on that path. The two laws below admit nothing else
/// for an ANONYMOUS interior: `field_unresolved_schema` resolves a
/// `containing_schema` only in {logical, bootstrap, physical, prebootstrap} —
/// wire is not among them — and `ordinary_union_unresolved_schema` forbids the
/// dual class that registering the owner as a logical kind would need.
///
/// The NAMED case is the one that had to be ruled rather than measured: a shape
/// with its own name, reused by several arms, reads like a schema, and the
/// checker accepts BOTH a logical kind with a field row and a wire record with
/// none. `fgdb-a11-residue-unresolved-schema-ref-laws-54sd` ruled that the
/// name changes nothing — a named shape and an anonymous interior take the same
/// path — because a per-union exception moves the contradiction one hop and
/// creates a named-vs-anonymous distinction that NOTHING in the checker encodes.
///
/// WHAT THIS COSTS, stated because it is a real cost and not a free win: the
/// retaining references inside a wire-path shape get no row anywhere, so their
/// edges are invisible to `dag_future_result`, to GC, and to the
/// checkpoint-vector walkers. That is the `fgdb-owlp` latency class, accepted
/// knowingly and CENSUSED there — not hand-waved.
///
/// WHAT THE CHECKER CAN AND CANNOT DO. It cannot choose the path for you: the
/// source census records an arm payload as a digest, not as a parsed member
/// list, so "carries only StrongRefs" is not derivable here. What it CAN do,
/// and what `arm_payload_shape_field_row` below does, is hold every shape the
/// ruling has already governed to that path: no field row may name one, and
/// none may reappear in a field-owning identity class. The table is the
/// enumeration of governed shapes; ADD A ROW HERE when a ruling puts another
/// named shape on the wire path.
///
/// The third guard — that a governed shape is still ON the wire path at all,
/// without which the other two pass vacuously on a deleted row — is a claim
/// about the RELEASED registries rather than about any `IdentityRegistries`,
/// so it lives in the test that loads them
/// (`idr_strongref_only_arm_payload_shapes_stay_on_the_wire_path`). Asserting
/// it inside `validate_identity` fired it on every synthetic fixture in the
/// suite, which is a checker validating rows its input never claimed to have.
pub const STRONGREF_ONLY_ARM_PAYLOAD_SHAPES: [ArmPayloadShape; 2] = [
    ArmPayloadShape {
        name: "DirectResumeCapability",
        source: "a11:1936",
        carried_by: "SubscriptionDeliveryTransitionSpec<Role>.client_action_authority",
        members: &["validation_ref:StrongRef<DurableCapabilityValidationEvidence>"],
        ruling: "fgdb-a11-residue-unresolved-schema-ref-laws-54sd",
    },
    ArmPayloadShape {
        name: "DispositionReceipt",
        source: "a11:1936",
        carried_by: "SubscriptionDeliveryTransitionSpec<Role>.client_action_authority",
        members: &[
            "receipt_ref:StrongRef<AuthenticatedClientSubscriptionDispositionReceipt<Role>>",
        ],
        ruling: "fgdb-a11-residue-unresolved-schema-ref-laws-54sd",
    },
];

#[derive(Debug, Clone, PartialEq)]
pub struct LogicalKind {
    pub object_kind: i64,
    pub name: String,
    pub status: String,
    pub construction_order: i64,
    pub role_predicate: String,
    pub max_size_bytes: i64,
    pub golden_corpus: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalKind {
    pub record_kind: i64,
    pub name: String,
    pub identity_law: String,
    pub status: String,
    pub transcript: String,
    pub owning_identity: String,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapFrame {
    pub frame_kind: i64,
    pub name: String,
    pub status: String,
    pub byte_size: i64,
    pub location: String,
    pub update_protocol: String,
    pub tear_validation: String,
    pub opener_fields: String,
    pub compatibility_gate: String,
    pub recovery_vectors: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrebootstrapKind {
    pub artifact_kind: i64,
    pub name: String,
    pub status: String,
    pub target_claim_domain: String,
    pub allowed_containers: String,
    pub import_target: String,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WireType {
    pub wire_type_id: i64,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub containing_union: Option<String>,
    pub wire_tag: Option<i64>,
    pub encoding_context: String,
    pub allowed_containing_schemas: Vec<String>,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldRow {
    pub containing_schema: String,
    pub field_tag: i64,
    pub stable_name: String,
    pub exact_wire_type: String,
    pub cardinality: String,
    pub identity_class: String,
    pub reference_semantics: String,
    pub target_schema_id: Option<String>,
    pub construction_order: i64,
    /// Optional instance-order refinement for a co-phased schema edge.
    ///
    /// `prior_object` preserves the field's declared reference strength while
    /// asserting that every encoded target instance already exists before the
    /// referrer encoder runs. It may discharge a schema-level cycle only when
    /// the source contract and retention rule say so explicitly.
    pub construction_relation: Option<String>,
    pub role_predicate: String,
    pub retention_and_cut_rule: String,
    pub version_status: String,
    pub max_size_bytes: i64,
    pub digest_class: Option<String>,
    pub transcript_recipe: Option<String>,
    pub bd_domain_separator: Option<String>,
    pub bd_schema_major: Option<i64>,
    pub bd_included_field_tags: Option<Vec<i64>>,
    pub bd_excluded_field_tags: Option<Vec<i64>>,
    pub recipe_pin: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceUnion {
    pub union_name: String,
    pub containing_schema: String,
    pub field_tag: i64,
    pub role: String,
    pub arms: Vec<ReferenceUnionArm>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceUnionArm {
    pub union_name: String,
    pub containing_schema: String,
    pub field_tag: i64,
    pub arm_tag: i64,
    pub stable_name: String,
    pub target_schema_id: String,
    pub role: String,
    pub identity_class: String,
    pub reference_semantics: String,
    pub role_predicate: String,
    pub retention_and_cut_rule: String,
    pub version_status: String,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrdinaryUnion {
    pub union_name: String,
    pub containing_schema: String,
    pub union_path: String,
    pub field_tag: Option<i64>,
    pub tag_wire_type: String,
    pub encoding_context: String,
    pub allowed_containing_schemas: Vec<String>,
    pub role_predicate: String,
    pub version_status: String,
    pub max_size_bytes: i64,
    pub arms: Vec<OrdinaryUnionArm>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrdinaryUnionArm {
    pub union_name: String,
    pub containing_schema: String,
    pub union_path: String,
    pub arm_tag: i64,
    pub source_arm_name: String,
    pub stable_name: String,
    pub payload_kind: String,
    pub payload_sha256: Option<String>,
    pub role_predicate: String,
    pub version_status: String,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IdentityRegistries {
    pub logical: Vec<LogicalKind>,
    pub logical_epoch: i64,
    pub physical: Vec<PhysicalKind>,
    pub physical_epoch: i64,
    pub bootstrap: Vec<BootstrapFrame>,
    pub bootstrap_epoch: i64,
    pub prebootstrap: Vec<PrebootstrapKind>,
    pub prebootstrap_epoch: i64,
    pub wire: Vec<WireType>,
    pub wire_epoch: i64,
    pub fields: Vec<FieldRow>,
    pub fields_epoch: i64,
    pub unions: Vec<ReferenceUnion>,
    pub ordinary_unions: Vec<OrdinaryUnion>,
}

pub type DurableFieldsRows = (i64, Vec<FieldRow>, Vec<OrdinaryUnion>, Vec<ReferenceUnion>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentPin {
    pub registry: &'static str,
    pub expected_epoch: i64,
    pub actual_epoch: i64,
    pub expected_pin: &'static str,
    pub actual_pin: String,
}

fn get_int_array(table: &Table, key: &str, ctx: &str) -> Result<Option<Vec<i64>>, ReadError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                match item {
                    toml::Value::Int(v) => out.push(*v),
                    _ => {
                        return Err(ReadError {
                            path: format!("{ctx}.{key}[{i}]"),
                            msg: "expected integer".into(),
                        });
                    }
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected array of integers".into(),
        }),
    }
}

fn get_opt_int(table: &Table, key: &str, ctx: &str) -> Result<Option<i64>, ReadError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Int(v)) => Ok(Some(*v)),
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected integer".into(),
        }),
    }
}

/// Require that a table contains no keys outside its versioned schema.
///
/// `Table` is a `BTreeMap`, so when several unknown keys are present the
/// lexicographically first one is reported.  This keeps the error path stable
/// across runs while naming the exact rejected key.
fn exact_keys(table: &Table, allowed: &[&str], ctx: &str) -> Result<(), ReadError> {
    if let Some(key) = table.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "unknown key in closed schema".into(),
        });
    }
    Ok(())
}

fn registry_header(
    root: &Table,
    expected: &str,
    file: &str,
    row_keys: &[&str],
) -> Result<i64, ReadError> {
    let mut allowed_root_keys = Vec::with_capacity(2 + row_keys.len());
    allowed_root_keys.extend_from_slice(&["schema_version", "registry"]);
    allowed_root_keys.extend_from_slice(row_keys);
    exact_keys(root, &allowed_root_keys, file)?;

    let schema_version = get_int(root, "schema_version", file)?;
    if schema_version != 1 {
        return Err(ReadError {
            path: format!("{file}.schema_version"),
            msg: format!("expected schema version 1, found {schema_version}"),
        });
    }

    let registry = get_table(root, "registry", file)?;
    let registry_ctx = format!("{file}.registry");
    exact_keys(registry, &["name", "registry_epoch"], &registry_ctx)?;
    let name = get_str(registry, "name", &registry_ctx)?;
    if name != expected {
        return Err(ReadError {
            path: format!("{file}.registry.name"),
            msg: format!("expected {expected:?}, found {name:?}"),
        });
    }
    get_int(registry, "registry_epoch", &registry_ctx)
}

fn load_table(dir: &Path, file: &str) -> Result<Table, LoadError> {
    let path = dir.join(file);
    let text = std::fs::read_to_string(&path).map_err(|e| LoadError {
        file: path.display().to_string(),
        msg: format!("cannot read: {e}"),
    })?;
    toml::parse(&text).map_err(|e| LoadError {
        file: path.display().to_string(),
        msg: e.to_string(),
    })
}

fn wrap(dir: &Path, file: &str, e: ReadError) -> LoadError {
    LoadError {
        file: dir.join(file).display().to_string(),
        msg: e.to_string(),
    }
}

pub fn logical_from(root: &Table) -> Result<(i64, Vec<LogicalKind>), ReadError> {
    let epoch = registry_header(
        root,
        "logical_object_kinds",
        "logical_object_kinds.toml",
        &["kind"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "logical_object_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("logical_object_kinds.toml.kind[{i}]");
        exact_keys(
            t,
            &[
                "object_kind",
                "name",
                "status",
                "construction_order",
                "role_predicate",
                "max_size_bytes",
                "golden_corpus",
            ],
            &ctx,
        )?;
        rows.push(LogicalKind {
            object_kind: get_int(t, "object_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            construction_order: get_int(t, "construction_order", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
            golden_corpus: get_str(t, "golden_corpus", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn physical_from(root: &Table) -> Result<(i64, Vec<PhysicalKind>), ReadError> {
    let epoch = registry_header(
        root,
        "physical_record_kinds",
        "physical_record_kinds.toml",
        &["kind"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "physical_record_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("physical_record_kinds.toml.kind[{i}]");
        exact_keys(
            t,
            &[
                "record_kind",
                "name",
                "identity_law",
                "status",
                "transcript",
                "owning_identity",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        rows.push(PhysicalKind {
            record_kind: get_int(t, "record_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            identity_law: get_str(t, "identity_law", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            transcript: get_str(t, "transcript", &ctx)?,
            owning_identity: get_str(t, "owning_identity", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn bootstrap_from(root: &Table) -> Result<(i64, Vec<BootstrapFrame>), ReadError> {
    let epoch = registry_header(
        root,
        "bootstrap_frames",
        "bootstrap_frames.toml",
        &["frame"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "frame", "bootstrap_frames.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("bootstrap_frames.toml.frame[{i}]");
        exact_keys(
            t,
            &[
                "frame_kind",
                "name",
                "status",
                "byte_size",
                "location",
                "update_protocol",
                "tear_validation",
                "opener_fields",
                "compatibility_gate",
                "recovery_vectors",
            ],
            &ctx,
        )?;
        rows.push(BootstrapFrame {
            frame_kind: get_int(t, "frame_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            byte_size: get_int(t, "byte_size", &ctx)?,
            location: get_str(t, "location", &ctx)?,
            update_protocol: get_str(t, "update_protocol", &ctx)?,
            tear_validation: get_str(t, "tear_validation", &ctx)?,
            opener_fields: get_str(t, "opener_fields", &ctx)?,
            compatibility_gate: get_str(t, "compatibility_gate", &ctx)?,
            recovery_vectors: get_str(t, "recovery_vectors", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn prebootstrap_from(root: &Table) -> Result<(i64, Vec<PrebootstrapKind>), ReadError> {
    let epoch = registry_header(
        root,
        "prebootstrap_artifact_kinds",
        "prebootstrap_artifact_kinds.toml",
        &["kind"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "prebootstrap_artifact_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("prebootstrap_artifact_kinds.toml.kind[{i}]");
        exact_keys(
            t,
            &[
                "artifact_kind",
                "name",
                "status",
                "target_claim_domain",
                "allowed_containers",
                "import_target",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        rows.push(PrebootstrapKind {
            artifact_kind: get_int(t, "artifact_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            target_claim_domain: get_str(t, "target_claim_domain", &ctx)?,
            allowed_containers: get_str(t, "allowed_containers", &ctx)?,
            import_target: get_str(t, "import_target", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn wire_from(root: &Table) -> Result<(i64, Vec<WireType>), ReadError> {
    let epoch = registry_header(root, "wire_types", "wire_types.toml", &["type"])?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "type", "wire_types.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("wire_types.toml.type[{i}]");
        exact_keys(
            t,
            &[
                "wire_type_id",
                "name",
                "kind",
                "status",
                "containing_union",
                "wire_tag",
                "encoding_context",
                "allowed_containing_schemas",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        rows.push(WireType {
            wire_type_id: get_int(t, "wire_type_id", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            kind: get_str(t, "kind", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            containing_union: get_opt_str(t, "containing_union", &ctx)?,
            wire_tag: get_opt_int(t, "wire_tag", &ctx)?,
            encoding_context: get_str(t, "encoding_context", &ctx)?,
            allowed_containing_schemas: get_str_array(t, "allowed_containing_schemas", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn fields_from(root: &Table) -> Result<DurableFieldsRows, ReadError> {
    let epoch = registry_header(
        root,
        "durable_fields",
        "durable_fields.toml",
        &[
            "field",
            "union",
            "union_arm",
            "reference_union",
            "reference_union_arm",
        ],
    )?;
    let mut fields = Vec::new();
    for (i, t) in get_table_array(root, "field", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.field[{i}]");
        exact_keys(
            t,
            &[
                "containing_schema",
                "field_tag",
                "stable_name",
                "exact_wire_type",
                "cardinality",
                "identity_class",
                "reference_semantics",
                "target_schema_id",
                "construction_order",
                "construction_relation",
                "role_predicate",
                "retention_and_cut_rule",
                "version_status",
                "max_size_bytes",
                "digest_class",
                "transcript_recipe",
                "bd_domain_separator",
                "bd_schema_major",
                "bd_included_field_tags",
                "bd_excluded_field_tags",
                "recipe_pin",
            ],
            &ctx,
        )?;
        fields.push(FieldRow {
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            field_tag: get_int(t, "field_tag", &ctx)?,
            stable_name: get_str(t, "stable_name", &ctx)?,
            exact_wire_type: get_str(t, "exact_wire_type", &ctx)?,
            cardinality: get_str(t, "cardinality", &ctx)?,
            identity_class: get_str(t, "identity_class", &ctx)?,
            reference_semantics: get_str(t, "reference_semantics", &ctx)?,
            target_schema_id: get_opt_str(t, "target_schema_id", &ctx)?,
            construction_order: get_int(t, "construction_order", &ctx)?,
            construction_relation: get_opt_str(t, "construction_relation", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            retention_and_cut_rule: get_str(t, "retention_and_cut_rule", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
            digest_class: get_opt_str(t, "digest_class", &ctx)?,
            transcript_recipe: get_opt_str(t, "transcript_recipe", &ctx)?,
            bd_domain_separator: get_opt_str(t, "bd_domain_separator", &ctx)?,
            bd_schema_major: get_opt_int(t, "bd_schema_major", &ctx)?,
            bd_included_field_tags: get_int_array(t, "bd_included_field_tags", &ctx)?,
            bd_excluded_field_tags: get_int_array(t, "bd_excluded_field_tags", &ctx)?,
            recipe_pin: get_opt_str(t, "recipe_pin", &ctx)?,
        });
    }
    let mut reference_unions = Vec::new();
    for (i, t) in get_table_array(root, "reference_union", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.reference_union[{i}]");
        exact_keys(
            t,
            &["union_name", "containing_schema", "field_tag", "role"],
            &ctx,
        )?;
        reference_unions.push(ReferenceUnion {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            field_tag: get_int(t, "field_tag", &ctx)?,
            role: get_str(t, "role", &ctx)?,
            arms: Vec::new(),
        });
    }

    let mut reference_union_index = BTreeMap::new();
    for (index, union) in reference_unions.iter().enumerate() {
        reference_union_index.insert(union.union_name.clone(), index);
    }
    for (i, t) in get_table_array(root, "reference_union_arm", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.reference_union_arm[{i}]");
        exact_keys(
            t,
            &[
                "union_name",
                "containing_schema",
                "field_tag",
                "arm_tag",
                "stable_name",
                "target_schema_id",
                "role",
                "identity_class",
                "reference_semantics",
                "role_predicate",
                "retention_and_cut_rule",
                "version_status",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        let arm = ReferenceUnionArm {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            field_tag: get_int(t, "field_tag", &ctx)?,
            arm_tag: get_int(t, "arm_tag", &ctx)?,
            stable_name: get_str(t, "stable_name", &ctx)?,
            target_schema_id: get_str(t, "target_schema_id", &ctx)?,
            role: get_str(t, "role", &ctx)?,
            identity_class: get_str(t, "identity_class", &ctx)?,
            reference_semantics: get_str(t, "reference_semantics", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            retention_and_cut_rule: get_str(t, "retention_and_cut_rule", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        };
        let Some(index) = reference_union_index.get(&arm.union_name).copied() else {
            return Err(ReadError {
                path: format!("{ctx}.union_name"),
                msg: format!(
                    "reference-union arm names undeclared union {:?}",
                    arm.union_name
                ),
            });
        };
        reference_unions[index].arms.push(arm);
    }

    let mut ordinary_unions = Vec::new();
    for (i, t) in get_table_array(root, "union", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.union[{i}]");
        exact_keys(
            t,
            &[
                "union_name",
                "containing_schema",
                "union_path",
                "field_tag",
                "tag_wire_type",
                "encoding_context",
                "allowed_containing_schemas",
                "role_predicate",
                "version_status",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        ordinary_unions.push(OrdinaryUnion {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            union_path: get_str(t, "union_path", &ctx)?,
            field_tag: get_opt_int(t, "field_tag", &ctx)?,
            tag_wire_type: get_str(t, "tag_wire_type", &ctx)?,
            encoding_context: get_str(t, "encoding_context", &ctx)?,
            allowed_containing_schemas: get_str_array(t, "allowed_containing_schemas", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
            arms: Vec::new(),
        });
    }

    let mut ordinary_union_index = BTreeMap::new();
    for (index, union) in ordinary_unions.iter().enumerate() {
        ordinary_union_index.insert(union.union_name.clone(), index);
    }
    for (i, t) in get_table_array(root, "union_arm", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.union_arm[{i}]");
        exact_keys(
            t,
            &[
                "union_name",
                "containing_schema",
                "union_path",
                "arm_tag",
                "source_arm_name",
                "stable_name",
                "payload_kind",
                "payload_sha256",
                "role_predicate",
                "version_status",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        let arm = OrdinaryUnionArm {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            union_path: get_str(t, "union_path", &ctx)?,
            arm_tag: get_int(t, "arm_tag", &ctx)?,
            source_arm_name: get_str(t, "source_arm_name", &ctx)?,
            stable_name: get_str(t, "stable_name", &ctx)?,
            payload_kind: get_str(t, "payload_kind", &ctx)?,
            payload_sha256: get_opt_str(t, "payload_sha256", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        };
        let Some(index) = ordinary_union_index.get(&arm.union_name).copied() else {
            return Err(ReadError {
                path: format!("{ctx}.union_name"),
                msg: format!(
                    "ordinary-union arm names undeclared union {:?}",
                    arm.union_name
                ),
            });
        };
        ordinary_unions[index].arms.push(arm);
    }

    Ok((epoch, fields, ordinary_unions, reference_unions))
}

/// Load all six identity artifacts from a `registries/` directory.
pub fn load_identity(dir: &Path) -> Result<IdentityRegistries, LoadError> {
    let (logical_epoch, logical) = logical_from(&load_table(dir, "logical_object_kinds.toml")?)
        .map_err(|e| wrap(dir, "logical_object_kinds.toml", e))?;
    let (physical_epoch, physical) = physical_from(&load_table(dir, "physical_record_kinds.toml")?)
        .map_err(|e| wrap(dir, "physical_record_kinds.toml", e))?;
    let (bootstrap_epoch, bootstrap) = bootstrap_from(&load_table(dir, "bootstrap_frames.toml")?)
        .map_err(|e| wrap(dir, "bootstrap_frames.toml", e))?;
    let (prebootstrap_epoch, prebootstrap) =
        prebootstrap_from(&load_table(dir, "prebootstrap_artifact_kinds.toml")?)
            .map_err(|e| wrap(dir, "prebootstrap_artifact_kinds.toml", e))?;
    let (wire_epoch, wire) = wire_from(&load_table(dir, "wire_types.toml")?)
        .map_err(|e| wrap(dir, "wire_types.toml", e))?;
    let (fields_epoch, fields, ordinary_unions, unions) =
        fields_from(&load_table(dir, "durable_fields.toml")?)
            .map_err(|e| wrap(dir, "durable_fields.toml", e))?;
    Ok(IdentityRegistries {
        logical,
        logical_epoch,
        physical,
        physical_epoch,
        bootstrap,
        bootstrap_epoch,
        prebootstrap,
        prebootstrap_epoch,
        wire,
        wire_epoch,
        fields,
        fields_epoch,
        unions,
        ordinary_unions,
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn v(code: &str, registry: &str, row_id: &str, msg: impl Into<String>) -> Violation {
    Violation {
        code: code.into(),
        registry: registry.into(),
        row_id: row_id.into(),
        msg: msg.into(),
    }
}

const RESTORE_MANIFEST_LOCAL_TAG: u8 = 0x01;
const RESTORE_MANIFEST_SHARDED_TAG: u8 = 0x02;
const RESTORE_AUTHORITY_EXTERNAL_CAS_CATALOGED_TAG: u8 = 0x01;
const RESTORE_AUTHORITY_DIRECTORY_BOUND_CATALOGED_TAG: u8 = 0x02;
const RESTORE_AUTHORITY_DIRECTORY_BOUND_EMBEDDED_NO_CATALOG_TAG: u8 = 0x03;

/// Admission law for the three tags that jointly describe one
/// `RestoreServicePromotionManifest` (Appendix A a20:2575).
///
/// The common `target_posture` and exactly-one BODY discriminants must agree.
/// A Local body admits all three authority profiles; a Sharded body admits
/// only `ExternalCasCataloged`. Unknown tags fail closed. This is deliberately
/// expressed over the durable bytes so a format decoder can call the same
/// predicate before publishing a decoded manifest.
pub fn restore_service_promotion_manifest_tags_are_coherent(
    target_posture_tag: u8,
    body_tag: u8,
    authority_profile_tag: u8,
) -> bool {
    matches!(
        (target_posture_tag, body_tag, authority_profile_tag),
        (
            RESTORE_MANIFEST_LOCAL_TAG,
            RESTORE_MANIFEST_LOCAL_TAG,
            RESTORE_AUTHORITY_EXTERNAL_CAS_CATALOGED_TAG
                | RESTORE_AUTHORITY_DIRECTORY_BOUND_CATALOGED_TAG
                | RESTORE_AUTHORITY_DIRECTORY_BOUND_EMBEDDED_NO_CATALOG_TAG,
        ) | (
            RESTORE_MANIFEST_SHARDED_TAG,
            RESTORE_MANIFEST_SHARDED_TAG,
            RESTORE_AUTHORITY_EXTERNAL_CAS_CATALOGED_TAG,
        )
    )
}

fn unique_ordinary_union<'a>(
    r: &'a IdentityRegistries,
    union_name: &str,
) -> Option<&'a OrdinaryUnion> {
    let mut matches = r
        .ordinary_unions
        .iter()
        .filter(|union| union.union_name == union_name);
    let only = matches.next()?;
    matches.next().is_none().then_some(only)
}

fn ordinary_union_has_exact_arms(
    union: &OrdinaryUnion,
    expected: &[(&str, u8, &str, &str)],
) -> bool {
    union.arms.len() == expected.len()
        && expected
            .iter()
            .all(|(source_arm_name, arm_tag, payload_kind, role_predicate)| {
                union.arms.iter().any(|arm| {
                    arm.source_arm_name == *source_arm_name
                        && arm.stable_name == *source_arm_name
                        && arm.arm_tag == i64::from(*arm_tag)
                        && arm.payload_kind == *payload_kind
                        && arm.role_predicate == *role_predicate
                })
            })
}

/// Bind the runtime truth table above to the released durable tag meanings.
///
/// Independent union validation proves that each closed union is internally
/// well formed, but cannot prove that three tags in one manifest agree. This
/// check activates when any part of the manifest family is present and then
/// fails closed unless the fields, all three union domains, and the
/// ExternalCas-refined wrapper agree on the tags consumed by the admission
/// predicate.
fn check_restore_service_promotion_manifest_coherence(
    r: &IdentityRegistries,
    out: &mut Vec<Violation>,
) {
    const MANIFEST: &str = "RestoreServicePromotionManifest";
    const POSTURE: &str = "RestoreServicePromotionManifestTargetPosture";
    const AUTHORITY: &str = "RestorePromotionAuthorityProfile";
    const EXTERNAL_REF: &str = "ExternalCasRestoreServicePromotionManifestRef";

    let domain_present = r.logical.iter().any(|row| row.name == MANIFEST)
        || r.fields.iter().any(|row| row.containing_schema == MANIFEST)
        || r.ordinary_unions
            .iter()
            .any(|row| matches!(row.union_name.as_str(), MANIFEST | POSTURE | AUTHORITY))
        || r.wire.iter().any(|row| row.name == EXTERNAL_REF);
    if !domain_present {
        return;
    }

    let field_has_exact_contract = |stable_name: &str, field_tag: i64, wire_type: &str| {
        let mut rows = r
            .fields
            .iter()
            .filter(|row| row.containing_schema == MANIFEST && row.stable_name == stable_name);
        matches!(
            (rows.next(), rows.next()),
            (Some(row), None)
                if row.field_tag == field_tag
                    && row.exact_wire_type == wire_type
                    && row.cardinality == "one"
                    && row.identity_class == "inline"
                    && row.reference_semantics == "none"
                    && row.target_schema_id.is_none()
                    && row.role_predicate == "true"
        )
    };

    let body = unique_ordinary_union(r, MANIFEST);
    let body_is_bound = body.is_some_and(|union| {
        union.containing_schema == MANIFEST
            && union.union_path == MANIFEST
            && union.field_tag.is_none()
            && union.tag_wire_type == "u8"
            && union.allowed_containing_schemas == [MANIFEST]
            && union.role_predicate == "true"
            && ordinary_union_has_exact_arms(
                union,
                &[
                    ("Local", RESTORE_MANIFEST_LOCAL_TAG, "inline-record", "true"),
                    (
                        "Sharded",
                        RESTORE_MANIFEST_SHARDED_TAG,
                        "inline-record",
                        "true",
                    ),
                ],
            )
    });

    let posture = unique_ordinary_union(r, POSTURE);
    let posture_is_bound = posture.is_some_and(|union| {
        union.containing_schema == MANIFEST
            && union.union_path == "RestoreServicePromotionManifest.target_posture"
            && union.field_tag == Some(0x0005)
            && union.tag_wire_type == "u8"
            && union.allowed_containing_schemas == [MANIFEST]
            && union.role_predicate == "true"
            && ordinary_union_has_exact_arms(
                union,
                &[
                    ("Local", RESTORE_MANIFEST_LOCAL_TAG, "unit", "true"),
                    ("Sharded", RESTORE_MANIFEST_SHARDED_TAG, "unit", "true"),
                ],
            )
    });

    let authority = unique_ordinary_union(r, AUTHORITY);
    let authority_is_bound = authority.is_some_and(|union| {
        union.containing_schema == AUTHORITY
            && union.union_path == AUTHORITY
            && union.field_tag.is_none()
            && union.tag_wire_type == "u8"
            && union.allowed_containing_schemas == [MANIFEST]
            && union.role_predicate == "true"
            && ordinary_union_has_exact_arms(
                union,
                &[
                    (
                        "ExternalCasCataloged",
                        RESTORE_AUTHORITY_EXTERNAL_CAS_CATALOGED_TAG,
                        "inline-record",
                        "true",
                    ),
                    (
                        "DirectoryBoundCataloged",
                        RESTORE_AUTHORITY_DIRECTORY_BOUND_CATALOGED_TAG,
                        "inline-record",
                        "role-local",
                    ),
                    (
                        "DirectoryBoundEmbeddedNoCatalog",
                        RESTORE_AUTHORITY_DIRECTORY_BOUND_EMBEDDED_NO_CATALOG_TAG,
                        "inline-record",
                        "role-local",
                    ),
                ],
            )
    });

    let mut external_refs = r.wire.iter().filter(|row| row.name == EXTERNAL_REF);
    let external_ref_is_bound = matches!(
        (external_refs.next(), external_refs.next()),
        (Some(row), None)
            if row.kind == "reference_wrapper"
                && matches!(
                    parse_refinement_claim(&row.encoding_context),
                    Some(Ok(claims))
                        if matches!(
                            claims.as_slice(),
                            [claim]
                                if claim.0 == "Sharded"
                                    && claim.1
                                        == i64::from(RESTORE_MANIFEST_SHARDED_TAG)
                                    && claim.2 == POSTURE
                        )
                )
    );

    if !field_has_exact_contract("target_posture", 0x0005, POSTURE)
        || !field_has_exact_contract("authority_profile", 0x0007, AUTHORITY)
        || !body_is_bound
        || !posture_is_bound
        || !authority_is_bound
        || !external_ref_is_bound
    {
        out.push(v(
            "restore_service_promotion_manifest_coherence",
            "durable_fields",
            MANIFEST,
            "Appendix A a20:2575 requires target_posture to equal the BODY arm, Local to admit \
             ExternalCasCataloged or either DirectoryBound profile, and Sharded to admit only \
             ExternalCasCataloged; the two fields, BODY/posture/profile tag domains, and \
             ExternalCas-refined wrapper must stay exactly bound to the runtime admission table",
        ));
    }
}

/// The shared code-space law for every class registry.
fn check_code_space(
    registry: &str,
    rows: &[(i64, String, String)], // (code, name, status)
    out: &mut Vec<Violation>,
) {
    let mut seen_codes: BTreeMap<i64, &str> = BTreeMap::new();
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    for (code, name, status) in rows {
        if *code <= 0 || *code >= 0xffff {
            out.push(v(
                "code_invalid",
                registry,
                name,
                format!(
                    "code {code:#06x} outside the valid space (0x0000/0xffff permanently invalid)"
                ),
            ));
        }
        if let Some(prior) = seen_codes.insert(*code, name) {
            out.push(v(
                "code_duplicate",
                registry,
                name,
                format!(
                    "code {code:#06x} already assigned to {prior:?}; a released code is never reassigned"
                ),
            ));
        }
        if !seen_names.insert(name.as_str()) {
            out.push(v("bad_field", registry, name, "duplicate schema name"));
        }
        if !matches!(
            status.as_str(),
            "active" | "reserved" | "retired" | "experimental"
        ) {
            out.push(v(
                "bad_field",
                registry,
                name,
                format!("status {status:?} not in {{active, reserved, retired, experimental}}"),
            ));
        }
        let experimental_range = (0xc000..=0xfffe).contains(code);
        if experimental_range && status != "experimental" {
            out.push(v(
                "range_status_mismatch",
                registry,
                name,
                format!(
                    "code {code:#06x} is in the test/experimental range but status is {status:?}"
                ),
            ));
        }
        if !experimental_range && status == "experimental" {
            out.push(v(
                "range_status_mismatch",
                registry,
                name,
                format!("status experimental requires a 0xc000..=0xfffe code, found {code:#06x}"),
            ));
        }
        if status == "experimental" {
            // Shipped registries are production surfaces: production readers
            // reject experimental codes, so a shipped experimental row fails.
            out.push(v(
                "experimental_in_production",
                registry,
                name,
                "experimental rows are rejected by production readers and may not ship in the registry",
            ));
        }
    }
}

/// Canonical BodyDigest recipe transcript (drift pin input; NOT the BLAKE3
/// identity law — that is implemented by w1-generated-parsers).
pub fn bodydigest_transcript(
    schema: &str,
    domain: &str,
    major: i64,
    included: &[i64],
    excluded: &[i64],
) -> String {
    let join = |tags: &[i64]| {
        let mut sorted: Vec<i64> = tags.to_vec();
        sorted.sort_unstable();
        sorted
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "bodydigest|{schema}|{domain}|major:{major}|included:{}|excluded:{}",
        join(included),
        join(excluded)
    )
}

pub fn bodydigest_pin(transcript: &str) -> String {
    format!("fnv1a64:{:016x}", fnv1a64(transcript.as_bytes()))
}

/// Encodability check: every field a producer wants to encode must have a
/// registered row for its containing schema ("a field absent from the table
/// is unencodable"). Returns one violation per unregistered field.
pub fn check_encodable(
    r: &IdentityRegistries,
    schema: &str,
    field_names: &[&str],
) -> Vec<Violation> {
    let registered: BTreeSet<&str> = r
        .fields
        .iter()
        .filter(|f| f.containing_schema == schema)
        .map(|f| f.stable_name.as_str())
        .collect();
    field_names
        .iter()
        .filter(|name| !registered.contains(**name))
        .map(|name| {
            v(
                "unregistered_field",
                "durable_fields",
                schema,
                format!("field {name:?} has no durable_fields.toml row and is unencodable"),
            )
        })
        .collect()
}

fn rows_pin(mut rows: Vec<String>) -> String {
    rows.sort();
    let transcript = rows.join("\n");
    format!("fnv1a64:{:016x}", fnv1a64(transcript.as_bytes()))
}

fn string_list_pin_transcript(values: &[String]) -> String {
    let framed_values = values
        .iter()
        .map(|value| format!("{}:{value}", value.len()))
        .collect::<Vec<_>>()
        .join("|");
    format!("{}|{framed_values}", values.len())
}

fn predicate_allows_role(predicate: &str, role: &str) -> bool {
    predicate == "true"
        || predicate
            .split("||")
            .map(str::trim)
            .any(|term| term == format!("role-{role}"))
}

fn role_predicate_roles(predicate: &str) -> Option<BTreeSet<&'static str>> {
    const ALL_ROLES: [&str; 3] = ["local", "meta", "shard"];
    if predicate == "true" {
        return Some(ALL_ROLES.into_iter().collect());
    }
    let mut roles = BTreeSet::new();
    for term in predicate.split("||").map(str::trim) {
        let role = match term {
            "role-local" => "local",
            "role-meta" => "meta",
            "role-shard" => "shard",
            _ => return None,
        };
        roles.insert(role);
    }
    (!roles.is_empty()).then_some(roles)
}

fn role_predicate_implies(left: &str, right: &str) -> bool {
    role_predicate_roles(left)
        .zip(role_predicate_roles(right))
        .is_some_and(|(left, right)| left.is_subset(&right))
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn check_ordinary_union_version_status(status: &str, row_id: &str, out: &mut Vec<Violation>) {
    match status {
        "active" | "reserved" | "retired" => {}
        "experimental" => out.push(v(
            "experimental_in_production",
            "durable_fields",
            row_id,
            "experimental ordinary-union rows may not ship in the production registry",
        )),
        _ => out.push(v(
            "bad_field",
            "durable_fields",
            row_id,
            format!("version_status {status:?} is not one of active|reserved|retired"),
        )),
    }
}

pub fn ordinary_union_has_top_level_shape(union: &OrdinaryUnion) -> bool {
    union.field_tag.is_none()
        && union.containing_schema == union.union_name
        && union.union_path == union.union_name
}

/// The generic-free family symbol of a possibly generic-signed schema name.
/// One registered kind row commits every expansion of its family (the same
/// precedent as wire-family census coverage), so ordinary-union schema
/// resolution matches `RoleTimeIssuanceReservationClosure<Role>` to the
/// registered `RoleTimeIssuanceReservationClosure` row.
pub fn generic_free_family(name: &str) -> &str {
    name.split('<').next().unwrap_or(name)
}

/// Independent, review-updated pins for the released identity assignments.
///
/// Registry rows are the canonical descriptions; these constants are compact
/// historical witnesses, not a second allowlist.  Adding or retiring a row
/// requires an epoch bump and an intentional pin update.  Deleting a released
/// row, reassigning its code/tag, or silently changing a union arm therefore
/// fails even when the resulting current snapshot is internally consistent.
pub fn assignment_pins(r: &IdentityRegistries) -> Vec<AssignmentPin> {
    const LOGICAL: &str = "fnv1a64:ebca430285355b80";
    const PHYSICAL: &str = "fnv1a64:6eb820a69bc263b2";
    const BOOTSTRAP: &str = "fnv1a64:c756ad93d4fcbcf7";
    const PREBOOTSTRAP: &str = "fnv1a64:d2a221d86d3adc80";
    const WIRE: &str = "fnv1a64:a4c2fbd09b7b8df8";
    const FIELDS: &str = "fnv1a64:7020ed5083f427dc";

    let logical = rows_pin(
        r.logical
            .iter()
            .map(|row| format!("kind|{:04x}|{}|{}", row.object_kind, row.name, row.status))
            .collect(),
    );
    let physical = rows_pin(
        r.physical
            .iter()
            .map(|row| format!("kind|{:04x}|{}|{}", row.record_kind, row.name, row.status))
            .collect(),
    );
    let bootstrap = rows_pin(
        r.bootstrap
            .iter()
            .map(|row| format!("frame|{:04x}|{}|{}", row.frame_kind, row.name, row.status))
            .collect(),
    );
    let prebootstrap = rows_pin(
        r.prebootstrap
            .iter()
            .map(|row| format!("kind|{:04x}|{}|{}", row.artifact_kind, row.name, row.status))
            .collect(),
    );
    let wire = rows_pin(
        r.wire
            .iter()
            .map(|row| {
                format!(
                    "type|{:04x}|{}|{}|{}|{}|{}",
                    row.wire_type_id,
                    row.name,
                    row.kind,
                    row.status,
                    row.containing_union.as_deref().unwrap_or("-"),
                    row.wire_tag
                        .map(|tag| format!("{tag:04x}"))
                        .unwrap_or_else(|| "-".into())
                )
            })
            .collect(),
    );
    let mut field_rows: Vec<String> = r
        .fields
        .iter()
        .map(|row| {
            format!(
                "field|{}|{:04x}|{}|{}|{}|{}|{}|{}|{}",
                row.containing_schema,
                row.field_tag,
                row.stable_name,
                row.exact_wire_type,
                row.cardinality,
                row.identity_class,
                row.reference_semantics,
                row.target_schema_id.as_deref().unwrap_or("-"),
                row.version_status
            )
        })
        .collect();
    for union in &r.unions {
        field_rows.push(format!(
            "union|{}|{}|{:04x}|{}",
            union.union_name, union.containing_schema, union.field_tag, union.role
        ));
        field_rows.extend(union.arms.iter().map(|arm| {
            format!(
                "arm|{}|{}|{:04x}|{:04x}|{}|{}|{}|{}|{}|{}",
                arm.union_name,
                arm.containing_schema,
                arm.field_tag,
                arm.arm_tag,
                arm.stable_name,
                arm.target_schema_id,
                arm.role,
                arm.identity_class,
                arm.reference_semantics,
                arm.version_status
            )
        }));
    }
    for union in &r.ordinary_unions {
        field_rows.push(format!(
            "ordinary-union|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            union.union_name,
            union.containing_schema,
            union.union_path,
            union
                .field_tag
                .map(|tag| format!("{tag:04x}"))
                .unwrap_or_else(|| "-".into()),
            union.tag_wire_type,
            union.encoding_context,
            string_list_pin_transcript(&union.allowed_containing_schemas),
            union.role_predicate,
            union.version_status,
            union.max_size_bytes
        ));
        field_rows.extend(union.arms.iter().map(|arm| {
            format!(
                "ordinary-arm|{}|{}|{}|{:04x}|{}|{}|{}|{}|{}|{}|{}",
                arm.union_name,
                arm.containing_schema,
                arm.union_path,
                arm.arm_tag,
                arm.source_arm_name,
                arm.stable_name,
                arm.payload_kind,
                arm.payload_sha256.as_deref().unwrap_or("-"),
                arm.role_predicate,
                arm.version_status,
                arm.max_size_bytes
            )
        }));
    }
    let fields = rows_pin(field_rows);

    vec![
        AssignmentPin {
            registry: "logical_object_kinds",
            expected_epoch: 62,
            actual_epoch: r.logical_epoch,
            expected_pin: LOGICAL,
            actual_pin: logical,
        },
        AssignmentPin {
            registry: "physical_record_kinds",
            expected_epoch: 1,
            actual_epoch: r.physical_epoch,
            expected_pin: PHYSICAL,
            actual_pin: physical,
        },
        AssignmentPin {
            registry: "bootstrap_frames",
            expected_epoch: 2,
            actual_epoch: r.bootstrap_epoch,
            expected_pin: BOOTSTRAP,
            actual_pin: bootstrap,
        },
        AssignmentPin {
            registry: "prebootstrap_artifact_kinds",
            expected_epoch: 1,
            actual_epoch: r.prebootstrap_epoch,
            expected_pin: PREBOOTSTRAP,
            actual_pin: prebootstrap,
        },
        AssignmentPin {
            registry: "wire_types",
            expected_epoch: 44,
            actual_epoch: r.wire_epoch,
            expected_pin: WIRE,
            actual_pin: wire,
        },
        AssignmentPin {
            registry: "durable_fields",
            expected_epoch: 80,
            actual_epoch: r.fields_epoch,
            expected_pin: FIELDS,
            actual_pin: fields,
        },
    ]
}

pub fn validate_identity(r: &IdentityRegistries) -> Vec<Violation> {
    let mut out = Vec::new();

    // --- per-registry code-space law ---------------------------------------
    check_code_space(
        "logical_object_kinds",
        &r.logical
            .iter()
            .map(|k| (k.object_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "physical_record_kinds",
        &r.physical
            .iter()
            .map(|k| (k.record_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "bootstrap_frames",
        &r.bootstrap
            .iter()
            .map(|k| (k.frame_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "prebootstrap_artifact_kinds",
        &r.prebootstrap
            .iter()
            .map(|k| (k.artifact_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "wire_types",
        &r.wire
            .iter()
            .map(|k| (k.wire_type_id, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    for pin in assignment_pins(r) {
        if pin.actual_epoch != pin.expected_epoch {
            out.push(v(
                "registry_epoch_mismatch",
                pin.registry,
                "registry",
                format!(
                    "released assignment epoch is {}, found {}; an epoch changes only with an intentional row add/retire and pin update",
                    pin.expected_epoch, pin.actual_epoch
                ),
            ));
        }
        if pin.actual_pin != pin.expected_pin {
            out.push(v(
                "registry_assignment_drift",
                pin.registry,
                "registry",
                format!(
                    "released assignment pin {:?} != recomputed {:?}; released codes, tags, names, lifecycle states, and union arms are append-only",
                    pin.expected_pin, pin.actual_pin
                ),
            ));
        }
    }

    // --- physical identity laws --------------------------------------------
    for k in &r.physical {
        if !matches!(
            k.identity_law.as_str(),
            "ciphertext_id"
                | "encoding_id"
                | "placement_id"
                | "symbol_record"
                | "locator_entry"
                | "pack"
        ) {
            out.push(v(
                "bad_field",
                "physical_record_kinds",
                &k.name,
                format!("unknown identity_law {:?}", k.identity_law),
            ));
        }
        if k.transcript.trim().is_empty()
            || k.owning_identity.trim().is_empty()
            || k.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "physical_record_kinds",
                &k.name,
                "identity transcript, owning identity, and positive resource bound are required",
            ));
        }
    }
    for k in &r.logical {
        if k.role_predicate.trim().is_empty()
            || k.golden_corpus.trim().is_empty()
            || k.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "logical_object_kinds",
                &k.name,
                "role predicate, reserved corpus path, and positive resource bound are required",
            ));
        }
    }
    for frame in &r.bootstrap {
        if frame.byte_size <= 0
            || frame.location.trim().is_empty()
            || frame.update_protocol.trim().is_empty()
            || frame.tear_validation.trim().is_empty()
            || frame.opener_fields.trim().is_empty()
            || frame.compatibility_gate.trim().is_empty()
            || frame.recovery_vectors.trim().is_empty()
        {
            out.push(v(
                "bad_field",
                "bootstrap_frames",
                &frame.name,
                "fixed size, location, update/tear/open/compatibility contracts, and recovery vectors are required",
            ));
        }
    }
    for artifact in &r.prebootstrap {
        if artifact.target_claim_domain.trim().is_empty()
            || artifact.allowed_containers.trim().is_empty()
            || artifact.import_target.trim().is_empty()
            || artifact.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "prebootstrap_artifact_kinds",
                &artifact.name,
                "claim domain, legal container closure, import target, and positive resource bound are required",
            ));
        }
    }

    // --- wire-type shape ----------------------------------------------------
    let wire_names: BTreeSet<&str> = r.wire.iter().map(|w| w.name.as_str()).collect();
    let wire_by_name: BTreeMap<&str, &WireType> =
        r.wire.iter().map(|w| (w.name.as_str(), w)).collect();
    // Ordinary unions keyed by their generic-stripped name, for refinement
    // resolution below. Two rows may share a base name (one union declared per
    // role); an arm found under ANY of them satisfies the claim, because the
    // claim names the union, not the instantiation.
    let mut ordinary_unions_by_base: BTreeMap<&str, Vec<&OrdinaryUnion>> = BTreeMap::new();
    for u in &r.ordinary_unions {
        ordinary_unions_by_base
            .entry(refinement_union_base(u.union_name.as_str()))
            .or_default()
            .push(u);
    }
    for w in &r.wire {
        if !matches!(
            w.kind.as_str(),
            "record"
                | "union"
                | "union_variant"
                | "reference_wrapper"
                | "discriminant"
                | "arm_value"
                | "framing"
        ) {
            out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                format!("unknown kind {:?}", w.kind),
            ));
        }
        // LAW: a reference wrapper's strength must be declared, so a newly
        // minted wrapper cannot escape the wire-tag law by being unknown to it.
        // Without this the field law below fails OPEN on exactly the rows it
        // was written for: an unclassified wrapper resolves to `None` and its
        // fields are then free to pick any semantics.
        if w.kind == "reference_wrapper" && declared_field_reference_semantics(&w.name).is_none() {
            out.push(v(
                "unclassified_reference_wrapper",
                "wire_types",
                &w.name,
                "a kind=\"reference_wrapper\" wire tag must declare its reference strength in \
                 appendix_a::registered_reference_definition_semantics (Appendix A \"Reference \
                 semantics\"); an unclassified wrapper leaves its field rows unconstrained",
            ));
        }
        if w.encoding_context.trim().is_empty()
            || w.allowed_containing_schemas.is_empty()
            || w.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                "encoding context, containing-schema closure, and positive resource bound are required",
            ));
        }
        // LAW: an arm-refinement claim must RESOLVE.
        //
        // A tag-refined wrapper is a wire tag that admits a strict subset of a
        // union's arms — Appendix A a20:2593 mints two and states the rule they
        // serve: "variant syntax is never used as a reference target." The
        // refinement IS the durable constraint: `OperationalRestoreTerminalPin
        // BasisRef` is the difference between "a pin basis" and "an Operational
        // pin basis", and a decoder that admits the Abandoned arm through it
        // accepts a state the source rejects.
        //
        // That constraint lived entirely in `encoding_context` prose. MEASURED
        // 2026-07-27 at dbaab71: 6 claims across the corpus, checked by nothing
        // — `encoding_context` was read only for non-emptiness (above). A claim
        // naming an arm that does not exist, or the right arm under the wrong
        // `arm_tag`, read exactly like a correct one.
        //
        // Both halves are load-bearing. Resolution alone would fail open on the
        // 2 of 6 rows written in a prose dialect ("the Sharded/ExternalCas
        // RestoreServicePromotionManifest arm" — no tag, no union clause), which
        // parse to `None` and would simply be skipped. One construct in two
        // dialects at n=6 is the same defect fgdb-gpms forecasts at n=190, so
        // the unparseable case is a violation, not a pass.
        //
        // A source spelling such as `X::Sealed` can constrain TWO registered
        // locations. Those clauses form one atomic claim: validating either
        // one alone would overstate coverage. The conjunction grammar is
        // therefore closed at exactly two distinct clauses, and this loop
        // resolves every returned clause through the same laws as a singleton.
        let refinement_claim = parse_refinement_claim(&w.encoding_context);
        match refinement_claim.as_ref() {
            None => {}
            Some(Err(RefinementClaimParseError::Single)) => out.push(v(
                "refinement_claim_unparseable",
                "wire_types",
                &w.name,
                format!(
                    "encoding_context claims an arm refinement but is outside the grammar \
                     \"{REFINEMENT_CLAIM_MARKER}<SourceArmName> arm (arm_tag 0x<hex>) of the \
                     <Union> union\"; a refinement stated only in prose is unresolvable and \
                     therefore unenforced"
                ),
            )),
            Some(Err(RefinementClaimParseError::Conjunction)) => out.push(v(
                "refinement_conjunction_invalid",
                "wire_types",
                &w.name,
                format!(
                    "encoding_context claims a conjunctive arm refinement but is outside the \
                     exactly-two-distinct-clause grammar \
                     \"{REFINEMENT_CONJUNCTION_MARKER}<Arm1> arm (arm_tag 0x<hex>) of the \
                     <Union1> union and the <Arm2> arm (arm_tag 0x<hex>) of the <Union2> \
                     union\"; repeated, mixed, missing, third, or duplicate clauses are \
                     unenforced"
                ),
            )),
            Some(Ok(clauses)) => {
                for clause in clauses {
                    validate_refinement_clause(
                        &ordinary_unions_by_base,
                        &w.name,
                        *clause,
                        &mut out,
                    );
                }
            }
        }
        // LAW: the two value-position refinement classes are disjoint and
        // each carries its own payload contract
        // (fgdb-payload-bearing-arm-values-5u56). The fgdb-gpms discriminant
        // is the tag-only PRECONDITION instrument; it is sound on a
        // payload-bearing arm only while the claim accounts for the payload
        // (`never the arm payload`, optionally with a `, while ... bind ...`
        // continuation naming the siblings that carry it). A value field —
        // `AuditTicketClaimRecord.owner : AuditTicketOwner::Operation`, whose
        // payload IS the owner identity and has no sibling carrier — takes
        // the `arm_value` instrument instead: the refined tag PLUS the
        // complete selected-arm payload, pinned by the arm's
        // census-registered digest so "complete" is a comparison, not an
        // adjective. The laws run only over claims that resolved; an
        // unresolved claim is already reported by the refinement law above
        // and must not double-report.
        if w.kind == "arm_value" && refinement_claim.is_none() {
            out.push(v(
                "arm_value_claim_missing",
                "wire_types",
                &w.name,
                "a kind=\"arm_value\" wire tag is defined by its arm refinement but its \
                 encoding_context carries no refinement claim; the value it preserves is \
                 unknowable and the row is unenforced",
            ));
        }
        if let Some(Ok(clauses)) = refinement_claim.as_ref() {
            let resolutions: Vec<Option<Vec<&OrdinaryUnionArm>>> = clauses
                .iter()
                .map(|clause| resolve_refinement_clause(&ordinary_unions_by_base, *clause))
                .collect();
            if resolutions.iter().all(Option::is_some) {
                let resolved: Vec<&OrdinaryUnionArm> = resolutions
                    .iter()
                    .filter_map(Option::as_ref)
                    .flatten()
                    .copied()
                    .collect();
                match w.kind.as_str() {
                    "discriminant" => {
                        if resolved.iter().any(|arm| arm.payload_kind != "unit")
                            && !w
                                .encoding_context
                                .contains(TAG_ONLY_PAYLOAD_ACCOUNTING_MARKER)
                        {
                            out.push(v(
                                "refinement_tag_only_payload_unaccounted",
                                "wire_types",
                                &w.name,
                                format!(
                                    "a tag-only discriminant refines a payload-bearing arm but \
                                     its claim never accounts for the payload; it must carry \
                                     \"{TAG_ONLY_PAYLOAD_ACCOUNTING_MARKER}\" (optionally with \
                                     a `, while ... bind ...` continuation naming the sibling \
                                     fields that carry the payload), or the field erases the \
                                     selected arm's payload"
                                ),
                            ));
                        }
                    }
                    "arm_value" => {
                        if clauses.len() != 1 {
                            out.push(v(
                                "arm_value_conjunction_invalid",
                                "wire_types",
                                &w.name,
                                "an arm value preserves exactly one selected arm's payload; a \
                                 two-location conjunction is a precondition shape and belongs \
                                 to kind=\"discriminant\"",
                            ));
                        } else {
                            if resolved.iter().all(|arm| arm.payload_kind == "unit") {
                                out.push(v(
                                    "arm_value_on_unit_payload",
                                    "wire_types",
                                    &w.name,
                                    "every resolved arm has a unit payload; the tag-only \
                                     discriminant is the complete and smaller instrument for \
                                     a payload-free refinement",
                                ));
                            }
                            match parse_arm_value_payload_pin(&w.encoding_context) {
                                None => out.push(v(
                                    "arm_value_payload_pin_missing",
                                    "wire_types",
                                    &w.name,
                                    format!(
                                        "an arm value must pin its complete payload as \
                                         \"{ARM_VALUE_PAYLOAD_PIN_PREFIX}<SourceArmName>\
                                         {ARM_VALUE_PAYLOAD_PIN_INFIX}<64 lowercase hex>)\"; \
                                         without the pin there is no evidence the complete \
                                         payload is carried"
                                    ),
                                )),
                                Some(Err(())) => out.push(v(
                                    "arm_value_payload_pin_malformed",
                                    "wire_types",
                                    &w.name,
                                    "the complete-payload pin is advertised but breaks its \
                                     closed spelling (empty or multi-word arm name, missing \
                                     infix, non-64-lowercase-hex digest, unterminated `)`, or \
                                     repeated pin marker)",
                                )),
                                Some(Ok(pin)) => {
                                    let agrees = resolved.iter().any(|arm| {
                                        arm.source_arm_name == pin.arm
                                            && arm.payload_sha256.as_deref() == Some(pin.sha256)
                                    });
                                    if !agrees {
                                        let registered: Vec<String> = resolved
                                            .iter()
                                            .map(|arm| {
                                                format!(
                                                    "{} (payload_sha256 {})",
                                                    arm.source_arm_name,
                                                    arm.payload_sha256.as_deref().unwrap_or("none")
                                                )
                                            })
                                            .collect();
                                        out.push(v(
                                            "arm_value_payload_pin_mismatch",
                                            "wire_types",
                                            &w.name,
                                            format!(
                                                "the pin names {} with digest {}, but the \
                                                 resolved claim registers {}",
                                                pin.arm,
                                                pin.sha256,
                                                registered.join(", ")
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        match (w.kind.as_str(), &w.containing_union, w.wire_tag) {
            ("union_variant", Some(union), Some(tag)) => {
                match wire_by_name.get(union.as_str()) {
                    None => out.push(v(
                        "bad_field",
                        "wire_types",
                        &w.name,
                        format!("containing_union {union:?} is not a registered wire type"),
                    )),
                    Some(parent) if !matches!(parent.kind.as_str(), "union" | "discriminant") => out.push(v(
                        "bad_field",
                        "wire_types",
                        &w.name,
                        format!(
                            "containing_union {union:?} is neither kind=union nor kind=discriminant"
                        ),
                    )),
                    Some(parent)
                        if matches!(parent.status.as_str(), "retired" | "experimental")
                            && w.status != parent.status =>
                    {
                        out.push(v(
                            "bad_field",
                            "wire_types",
                            &w.name,
                            format!(
                                "variant lifecycle {:?} is incompatible with containing union lifecycle {:?}",
                                w.status, parent.status
                            ),
                        ));
                    }
                    Some(_) => {}
                }
                if tag <= 0 || tag >= 0xffff {
                    out.push(v(
                        "code_invalid",
                        "wire_types",
                        &w.name,
                        format!("wire_tag {tag:#06x} outside the valid space"),
                    ));
                }
            }
            ("union_variant", _, _) => out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                "union_variant requires containing_union and wire_tag",
            )),
            (_, Some(_), _) | (_, _, Some(_)) => out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                "containing_union/wire_tag are only legal on union_variant rows",
            )),
            _ => {}
        }
    }
    // Variant tags unique within a union.
    let mut variant_tags: BTreeMap<(&str, i64), &str> = BTreeMap::new();
    for w in &r.wire {
        if let (Some(union), Some(tag)) = (&w.containing_union, w.wire_tag)
            && let Some(prior) = variant_tags.insert((union.as_str(), tag), &w.name)
        {
            out.push(v(
                "code_duplicate",
                "wire_types",
                &w.name,
                format!("wire_tag {tag:#06x} in union {union:?} already assigned to {prior:?}"),
            ));
        }
    }

    // --- disjointness across the five classes ------------------------------
    let mut class_of: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for k in &r.logical {
        class_of.entry(k.name.as_str()).or_default().push("logical");
    }
    for k in &r.physical {
        class_of
            .entry(k.name.as_str())
            .or_default()
            .push("physical");
    }
    for k in &r.bootstrap {
        class_of
            .entry(k.name.as_str())
            .or_default()
            .push("bootstrap");
    }
    for k in &r.prebootstrap {
        class_of
            .entry(k.name.as_str())
            .or_default()
            .push("prebootstrap");
    }
    for k in &r.wire {
        class_of.entry(k.name.as_str()).or_default().push("wire");
    }
    for (name, classes) in &class_of {
        if classes.len() > 1 {
            out.push(v(
                "disjointness_dual_class",
                "identity",
                name,
                format!("schema inhabits {classes:?}; no schema may inhabit more than one identity class"),
            ));
        }
    }

    // --- field rows ---------------------------------------------------------
    let logical_by_name: BTreeMap<&str, &LogicalKind> =
        r.logical.iter().map(|k| (k.name.as_str(), k)).collect();
    let bootstrap_names: BTreeSet<&str> = r.bootstrap.iter().map(|k| k.name.as_str()).collect();
    let physical_names: BTreeSet<&str> = r.physical.iter().map(|k| k.name.as_str()).collect();
    let prebootstrap_names: BTreeSet<&str> =
        r.prebootstrap.iter().map(|k| k.name.as_str()).collect();
    let union_by_name: BTreeMap<&str, &ReferenceUnion> = r
        .unions
        .iter()
        .map(|u| (u.union_name.as_str(), u))
        .collect();
    let ordinary_union_names: BTreeSet<&str> = r
        .ordinary_unions
        .iter()
        .map(|u| u.union_name.as_str())
        .collect();

    let mut field_tags: BTreeMap<(&str, i64), &str> = BTreeMap::new();
    let mut body_rows_per_schema: BTreeMap<&str, Vec<&FieldRow>> = BTreeMap::new();
    let tags_per_schema: BTreeMap<&str, BTreeSet<i64>> = {
        let mut m: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
        for f in &r.fields {
            m.entry(f.containing_schema.as_str())
                .or_default()
                .insert(f.field_tag);
        }
        m
    };

    // LAW: the StrongRef-only arm-payload shapes take the wire path and own no
    // field row (see STRONGREF_ONLY_ARM_PAYLOAD_SHAPES for the rule, the
    // corpus that measured it, and the cost it accepts).
    for shape in &STRONGREF_ONLY_ARM_PAYLOAD_SHAPES {
        // NOTE the completeness guard — "a governed shape must still BE a
        // registered wire type" — is NOT here. It is a claim about the RELEASED
        // tree, not about an arbitrary `IdentityRegistries`, and asserting it
        // here made every synthetic fixture in the suite fire it (measured: 4
        // satisfiability witnesses went red on rows they never mention). It
        // lives in `idr_strongref_only_arm_payload_shapes_stay_on_the_wire_path`,
        // which runs against the real registries under `cargo test`. Without it
        // the two guards below pass VACUOUSLY on a deleted row.
        //
        // The wrong path taken outright. `disjointness_dual_class` catches the
        // shape registered in BOTH classes; this catches it moved wholesale
        // into a field-owning one, which that law cannot see.
        let field_owning: Vec<&str> = [
            logical_by_name.get(shape.name).map(|_| "logical"),
            bootstrap_names.contains(shape.name).then_some("bootstrap"),
            physical_names.contains(shape.name).then_some("physical"),
            prebootstrap_names
                .contains(shape.name)
                .then_some("prebootstrap"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !field_owning.is_empty() {
            out.push(v(
                "arm_payload_shape_field_row",
                "identity",
                shape.name,
                format!(
                    "{} ({}) is an arm payload shape of {} carrying only {:?}; it takes the wire \
                     path with no field row under {}, but is registered in {field_owning:?}",
                    shape.name, shape.source, shape.carried_by, shape.members, shape.ruling
                ),
            ));
        }
        for f in &r.fields {
            if generic_free_family(f.containing_schema.as_str()) == shape.name {
                out.push(v(
                    "arm_payload_shape_field_row",
                    "durable_fields",
                    &format!("{}#{}", f.containing_schema, f.stable_name),
                    format!(
                        "{} ({}) is an arm payload shape of {}; a shape whose body carries only \
                         retaining references takes the wire path and owns NO field row ({}). Its \
                         members are committed byte-exactly by the covering arm payload digest",
                        shape.name, shape.source, shape.carried_by, shape.ruling
                    ),
                ));
            }
        }
    }

    for f in &r.fields {
        let row_id = format!("{}#{}", f.containing_schema, f.stable_name);
        // Containing schema must resolve in one identity class.  Resolution is
        // by generic-free family, the same law ordinary-union hosts already
        // use: one registered kind row commits every expansion of its family,
        // so a member of `RestoreSourceLeaseRecord<Role:AuthorityOwningRole>`
        // resolves through the registered `RestoreSourceLeaseRecord` row.  The
        // family is only a *lookup*; every downstream contract (tag
        // uniqueness, anchor matching, BodyDigest recipes) still keys on the
        // exact signed `containing_schema`, so two expansions never merge.
        let containing_family = generic_free_family(f.containing_schema.as_str());
        let containing_logical = logical_by_name.get(containing_family);
        let resolves = containing_logical.is_some()
            || bootstrap_names.contains(containing_family)
            || physical_names.contains(containing_family)
            || prebootstrap_names.contains(containing_family);
        if !resolves {
            // A wire owner is the interesting sub-case and the message used to
            // hide it: the family DOES resolve, in the one class that can never
            // own a field row, and "resolves in no identity class" reads as a
            // missing mint. It is not — it is the arm-payload law
            // (STRONGREF_ONLY_ARM_PAYLOAD_SHAPES) refusing the row, and an
            // author who mis-reads it re-mints the owner as a logical kind.
            let detail = if wire_names.contains(containing_family) {
                format!(
                    "containing_schema {:?} resolves as a WIRE type, which can never own a field \
                     row; its members are committed byte-exactly by the covering wire envelope. \
                     Do not re-mint the owner as a logical kind to make this row land — see the \
                     arm-payload law in identity::STRONGREF_ONLY_ARM_PAYLOAD_SHAPES",
                    f.containing_schema
                )
            } else {
                format!(
                    "containing_schema {:?} resolves in no identity class",
                    f.containing_schema
                )
            };
            out.push(v(
                "field_unresolved_schema",
                "durable_fields",
                &row_id,
                detail,
            ));
        }
        // Tag uniqueness + validity.
        if f.field_tag <= 0 || f.field_tag >= 0xffff {
            out.push(v(
                "code_invalid",
                "durable_fields",
                &row_id,
                format!("field_tag {:#06x} outside the valid space", f.field_tag),
            ));
        }
        if let Some(prior) =
            field_tags.insert((f.containing_schema.as_str(), f.field_tag), &f.stable_name)
        {
            out.push(v(
                "code_duplicate",
                "durable_fields",
                &row_id,
                format!("field_tag {} already assigned to {prior:?}", f.field_tag),
            ));
        }
        // Enum shapes.
        if !matches!(f.cardinality.as_str(), "one" | "optional" | "many") {
            out.push(v("bad_field", "durable_fields", &row_id, "bad cardinality"));
        }
        if !matches!(
            f.identity_class.as_str(),
            "scalar" | "inline" | "logical" | "physical" | "bootstrap_local"
        ) {
            // LAW: the FIELD identity_class vocabulary is narrower than the
            // top_level_candidate one. A candidate row legitimately carries
            // `wire` or `prebootstrap` because it names what a symbol IS; a
            // field row names what its value contributes to durable identity,
            // and a field can never contribute wire or prebootstrap identity.
            // The admitted set is unchanged - narrowing it to
            // logical|physical|inline would reject 124 landed rows.
            let msg = format!(
                "identity_class {:?} is not a field identity class; a field admits \
                 scalar|inline|logical|physical|bootstrap_local (a top_level_candidate \
                 row does take wire and prebootstrap)",
                f.identity_class
            );
            // Both codes are spelled as LITERALS in the code argument. The
            // satisfiability ratchet extracts the code set from this source
            // text; a code passed as a variable is invisible to it, and it
            // silently read the next literal ("durable_fields") instead --
            // hiding both of these codes from coverage since 722ff22.
            if matches!(f.identity_class.as_str(), "wire" | "prebootstrap") {
                out.push(v(
                    "field_identity_class_not_a_field_class",
                    "durable_fields",
                    &row_id,
                    msg,
                ));
            } else {
                out.push(v(
                    "field_identity_class_invalid",
                    "durable_fields",
                    &row_id,
                    msg,
                ));
            }
        }
        if !matches!(
            f.reference_semantics.as_str(),
            "none" | "strong" | "conditional" | "weak_digest" | "locator" | "external_root"
        ) {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "bad reference_semantics",
            ));
        }
        if !matches!(
            f.version_status.as_str(),
            "active" | "reserved" | "retired" | "experimental"
        ) {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "bad version_status",
            ));
        }
        if f.version_status == "experimental" {
            out.push(v(
                "experimental_in_production",
                "durable_fields",
                &row_id,
                "experimental field rows may not ship in the production registry",
            ));
        }
        if f.max_size_bytes <= 0 {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "max_size_bytes must be positive",
            ));
        }
        if f.role_predicate.trim().is_empty() || f.retention_and_cut_rule.trim().is_empty() {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "role_predicate and retention_and_cut_rule must be nonblank",
            ));
        }
        if let Some(relation) = f.construction_relation.as_deref()
            && (relation != PRIOR_OBJECT_CONSTRUCTION_RELATION
                || !matches!(
                    f.reference_semantics.as_str(),
                    "strong" | "conditional" | "weak_digest"
                )
                || f.target_schema_id.is_none()
                || !f.retention_and_cut_rule.contains("PriorObject")
                || !f.retention_and_cut_rule.contains("already-known"))
        {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "construction_relation is either absent or prior_object; prior_object requires \
                 one direct strong/conditional/weak_digest target and an explicit PriorObject \
                 already-known instance contract in retention_and_cut_rule",
            ));
        }
        // Wire-type resolution: builtin -> wire_types -> ordinary union ->
        // generated reference union.
        let is_builtin = BUILTIN_WIRE_TYPES.contains(&f.exact_wire_type.as_str());
        let is_wire = wire_names.contains(f.exact_wire_type.as_str());
        let is_ordinary_union = ordinary_union_names.contains(f.exact_wire_type.as_str());
        let is_union = union_by_name.contains_key(f.exact_wire_type.as_str());
        // A bootstrap frame may appear inline as a field's exact type
        // (RootSlot.bootstrap: RootBootstrap at a pinned offset, §5.1) —
        // frames are schemas in the bootstrap identity class, not wire types.
        let is_inline_frame = bootstrap_names.contains(f.exact_wire_type.as_str());
        if !is_builtin && !is_wire && !is_ordinary_union && !is_union && !is_inline_frame {
            out.push(v(
                "field_unresolved_wire_type",
                "durable_fields",
                &row_id,
                format!("exact_wire_type {:?} resolves nowhere", f.exact_wire_type),
            ));
        }
        if let Some(wire_type) = wire_by_name.get(f.exact_wire_type.as_str())
            && !wire_type
                .allowed_containing_schemas
                .iter()
                .any(|schema| schema == "*" || schema == &f.containing_schema)
        {
            out.push(v(
                "wire_context_mismatch",
                "durable_fields",
                &row_id,
                format!(
                    "wire type {:?} is not permitted in containing schema {:?}",
                    f.exact_wire_type, f.containing_schema
                ),
            ));
        }
        // LAW: a registered non-reference wire member contributes inline
        // identity.  The foundation's raw-byte consumers cannot distinguish
        // another field class, so accepting scalar/logical/physical here would
        // silently change durable identity semantics.
        if let Some(wire_type) = wire_by_name.get(f.exact_wire_type.as_str())
            && wire_type.kind != "reference_wrapper"
            && f.reference_semantics == "none"
            && f.identity_class != "inline"
        {
            out.push(v(
                "non_reference_wire_identity_class_mismatch",
                "durable_fields",
                &row_id,
                format!(
                    "registered wire type {:?} of kind {:?} with reference_semantics=none must use identity_class=inline",
                    f.exact_wire_type, wire_type.kind
                ),
            ));
        }
        // Completeness guard for laws over registered wire members: a field
        // that names a registered wire type must resolve to one of the closed
        // wire-kind vocabulary, never an unclassified future kind.
        if let Some(wire_type) = wire_by_name.get(f.exact_wire_type.as_str())
            && !matches!(
                wire_type.kind.as_str(),
                "record"
                    | "union"
                    | "union_variant"
                    | "reference_wrapper"
                    | "discriminant"
                    | "arm_value"
                    | "framing"
            )
        {
            out.push(v(
                "field_wire_kind_unclassified",
                "durable_fields",
                &row_id,
                format!(
                    "exact_wire_type {:?} resolves to unclassified wire kind {:?}",
                    f.exact_wire_type, wire_type.kind
                ),
            ));
        }
        // LAW: reference_semantics is FORCED by exact_wire_type.
        //
        // Direction 1 — a type that declares a strength admits exactly that
        // value. Direction 2 — a type that declares none may not be promoted
        // to a retaining edge; `external_root` is excluded because it is the
        // bootstrap slot's raw-oid external GC root (Appendix A ~1435), which
        // `external_root_outside_frame` guards separately.
        //
        // The generated-reference-union spelling (target_schema_id = None, type
        // = the union anchored to this row) is already exact via
        // `union_field_mismatch`, so it is passed through here rather than
        // double-reported.
        match declared_field_reference_semantics(&f.exact_wire_type) {
            Some(declared) if f.reference_semantics != declared => {
                out.push(v(
                    "wire_type_reference_semantics_mismatch",
                    "durable_fields",
                    &row_id,
                    format!(
                        "exact_wire_type {:?} declares reference_semantics {declared:?}, row \
                         carries {:?}; the wire tag declares the strength and a member may not \
                         weaken or strengthen it",
                        f.exact_wire_type, f.reference_semantics
                    ),
                ));
            }
            None if matches!(f.reference_semantics.as_str(), "strong" | "conditional")
                && !union_by_name.contains_key(f.exact_wire_type.as_str()) =>
            {
                out.push(v(
                    "reference_semantics_without_reference_type",
                    "durable_fields",
                    &row_id,
                    format!(
                        "reference_semantics {:?} on exact_wire_type {:?}, which is not a \
                         reference wrapper and not this row's generated reference union; a \
                         retaining edge must be carried by a wire tag that declares one",
                        f.reference_semantics, f.exact_wire_type
                    ),
                ));
            }
            _ => {}
        }
        // Construction-order consistency with the containing logical kind.
        // LAW: a field's construction_order must EQUAL its containing kind's.
        // Vacuity: the previous `if let Some(kind) = ... &&` skipped silently
        // when the containing kind was absent. A schema that resolves to no
        // kind is reported by field_unresolved_schema above, so the input is
        // never evaluated-and-passed; it fails closed under that law.
        match containing_logical {
            Some(kind) if f.construction_order != kind.construction_order => {
                out.push(v(
                    "field_construction_order_mismatch",
                    "durable_fields",
                    &row_id,
                    format!(
                        "construction_order {} != containing kind {} order {}",
                        f.construction_order, kind.name, kind.construction_order
                    ),
                ));
            }
            _ => {}
        }
        // Reference discipline. `external_root` is the distinct traversal
        // class for the bootstrap-slot root identity (Appendix A ~1435):
        // followed as strong by GC from OUTSIDE the object graph, legal only
        // inside a bootstrap frame — an in-graph object must use an ordinary
        // strong/conditional edge instead.
        let is_retaining = matches!(
            f.reference_semantics.as_str(),
            "strong" | "conditional" | "external_root"
        );
        if is_retaining {
            let in_frame = bootstrap_names.contains(containing_family);
            if f.reference_semantics == "external_root" {
                if !in_frame {
                    out.push(v(
                        "external_root_outside_frame",
                        "durable_fields",
                        &row_id,
                        "external_root references are legal only inside bootstrap frames; in-graph objects use strong/conditional edges",
                    ));
                }
            } else if in_frame {
                out.push(v(
                    "frame_strong_ref",
                    "durable_fields",
                    &row_id,
                    "bootstrap frames are not graph nodes and may not carry retaining references",
                ));
            }
            if f.identity_class != "logical" {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &row_id,
                    "strong/conditional references must have identity_class = \"logical\"",
                ));
            }
            match &f.target_schema_id {
                Some(target) => {
                    if physical_names.contains(target.as_str())
                        || bootstrap_names.contains(target.as_str())
                        || prebootstrap_names.contains(target.as_str())
                    {
                        out.push(v(
                            "ref_target_not_logical",
                            "durable_fields",
                            &row_id,
                            format!(
                                "strong/conditional target {target:?} is not a logical object (physical realizations, frames, and prebootstrap artifacts are never StrongRef targets)"
                            ),
                        ));
                    } else if !logical_by_name.contains_key(target.as_str()) {
                        out.push(v(
                            "ref_target_unresolved",
                            "durable_fields",
                            &row_id,
                            format!("target {target:?} resolves nowhere"),
                        ));
                    }
                }
                None => {
                    // Polymorphic: must be a generated union anchored to this row.
                    match union_by_name.get(f.exact_wire_type.as_str()) {
                        Some(u)
                            if u.containing_schema == f.containing_schema
                                && u.field_tag == f.field_tag => {}
                        _ => out.push(v(
                            "bare_strong_ref",
                            "durable_fields",
                            &row_id,
                            "polymorphic strong/conditional field without its generated reference union (bare StrongRef<A|B> is invalid in normative bytes)",
                        )),
                    }
                }
            }
        } else if let Some(target) = &f.target_schema_id {
            // weak_digest/locator targets: must at least resolve somewhere
            // (weak digests of logical objects; locators may name logical
            // or physical realizations).
            let known = logical_by_name.contains_key(target.as_str())
                || physical_names.contains(target.as_str());
            if !known {
                out.push(v(
                    "ref_target_unresolved",
                    "durable_fields",
                    &row_id,
                    format!("nonretaining target {target:?} resolves nowhere"),
                ));
            }
        }
        // Digest discipline: digest-typed fields declare exactly one class;
        // never by naming convention.
        let digest_typed = matches!(f.exact_wire_type.as_str(), "digest256" | "WeakDigest");
        match &f.digest_class {
            None if digest_typed => out.push(v(
                "digest_missing_class",
                "durable_fields",
                &row_id,
                "digest-typed field without a declared digest_class (target|transcript|weak_identity|body)",
            )),
            None => {}
            Some(class) => {
                if !digest_typed
                    && !(class == "transcript" && f.exact_wire_type == "u64")
                {
                    out.push(v(
                        "digest_class_wire_type_mismatch",
                        "durable_fields",
                        &row_id,
                        format!(
                            "digest_class {:?} is not permitted for exact_wire_type {:?}",
                            class, f.exact_wire_type
                        ),
                    ));
                }
                match class.as_str() {
                    "target" | "weak_identity" => {
                        if !digest_typed {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "target/weak-identity digest classes require digest256 or WeakDigest wire types",
                            ));
                        }
                        if f.transcript_recipe.is_some()
                            || f.bd_domain_separator.is_some()
                            || f.bd_schema_major.is_some()
                            || f.bd_included_field_tags.is_some()
                            || f.bd_excluded_field_tags.is_some()
                            || f.recipe_pin.is_some()
                        {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "target/weak-identity digests may not carry transcript or BodyDigest recipe metadata",
                            ));
                        }
                    }
                    "transcript" => {
                        if !digest_typed && f.exact_wire_type != "u64" {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "transcript digest/checksum class requires digest256, WeakDigest, or an explicit u64 checksum wire type",
                            ));
                        }
                        if f.transcript_recipe.as_deref().is_none_or(|t| t.trim().is_empty()) {
                            out.push(v(
                                "digest_missing_recipe",
                                "durable_fields",
                                &row_id,
                                "transcript digest without a registered recipe",
                            ));
                        }
                        if f.bd_domain_separator.is_some()
                            || f.bd_schema_major.is_some()
                            || f.bd_included_field_tags.is_some()
                            || f.bd_excluded_field_tags.is_some()
                            || f.recipe_pin.is_some()
                        {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "transcript digest may not carry BodyDigest recipe metadata",
                            ));
                        }
                    }
                    "body" => {
                        if f.exact_wire_type != "digest256" || f.transcript_recipe.is_some() {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "BodyDigest must use digest256 and its generated BodyDigest metadata, not a transcript_recipe",
                            ));
                        }
                        body_rows_per_schema
                            .entry(f.containing_schema.as_str())
                            .or_default()
                            .push(f);
                    }
                    other => out.push(v(
                        "bad_field",
                        "durable_fields",
                        &row_id,
                        format!("unknown digest_class {other:?}"),
                    )),
                }
            }
        }
    }

    // --- BodyDigest recipes -------------------------------------------------
    for (schema, rows) in &body_rows_per_schema {
        if rows.len() > 1 {
            out.push(v(
                "bodydigest_two_fields",
                "durable_fields",
                schema,
                format!(
                    "{} BodyDigest fields in one schema; exactly one is legal",
                    rows.len()
                ),
            ));
        }
        for f in rows {
            let row_id = format!("{}#{}", f.containing_schema, f.stable_name);
            let (Some(domain), Some(major), Some(included), Some(excluded), Some(pin)) = (
                &f.bd_domain_separator,
                f.bd_schema_major,
                &f.bd_included_field_tags,
                &f.bd_excluded_field_tags,
                &f.recipe_pin,
            ) else {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &row_id,
                    "BodyDigest row requires bd_domain_separator, bd_schema_major, bd_included_field_tags, bd_excluded_field_tags, recipe_pin",
                ));
                continue;
            };
            let known_tags = tags_per_schema.get(schema).cloned().unwrap_or_default();
            for tag in included.iter().chain(excluded.iter()) {
                if !known_tags.contains(tag) {
                    out.push(v(
                        "bodydigest_unknown_exclusion",
                        "durable_fields",
                        &row_id,
                        format!("recipe names unregistered field tag {tag} of {schema}"),
                    ));
                }
            }
            // The digest's own field must be excluded and never included:
            // computing over bytes that include the digest itself is a G0
            // error (self-including computation).
            if included.contains(&f.field_tag) || !excluded.contains(&f.field_tag) {
                out.push(v(
                    "bodydigest_self_included",
                    "durable_fields",
                    &row_id,
                    "the BodyDigest field's own tag must be excluded from its recipe",
                ));
            }
            let included_set: BTreeSet<i64> = included.iter().copied().collect();
            let excluded_set: BTreeSet<i64> = excluded.iter().copied().collect();
            if included_set.len() != included.len()
                || excluded_set.len() != excluded.len()
                || !included_set.is_disjoint(&excluded_set)
                || excluded_set != BTreeSet::from([f.field_tag])
                || (!included_set.is_empty()
                    && included_set
                        .union(&excluded_set)
                        .copied()
                        .collect::<BTreeSet<_>>()
                        != known_tags)
            {
                out.push(v(
                    "bodydigest_incomplete_partition",
                    "durable_fields",
                    &row_id,
                    "BodyDigest include/exclude tags must be unique and disjoint; exclusions contain exactly the BodyDigest field, and an explicit include list must complete the schema partition",
                ));
            }
            let transcript = bodydigest_transcript(schema, domain, major, included, excluded);
            let recomputed = bodydigest_pin(&transcript);
            if recomputed != *pin {
                out.push(v(
                    "bodydigest_pin_mismatch",
                    "durable_fields",
                    &row_id,
                    format!(
                        "recipe drift: pinned {pin:?} != recomputed {recomputed:?} over transcript {transcript:?}"
                    ),
                ));
            }
        }
    }

    // --- ordinary closed tagged unions -------------------------------------
    let reference_union_names: BTreeSet<&str> =
        r.unions.iter().map(|u| u.union_name.as_str()).collect();
    let mut ordinary_union_names_seen = BTreeSet::new();
    let mut ordinary_union_paths: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for u in &r.ordinary_unions {
        let row_id = u.union_name.as_str();
        if u.union_name.trim().is_empty()
            || u.containing_schema.trim().is_empty()
            || u.union_path.trim().is_empty()
        {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                "ordinary union name, containing schema, and union path must be nonblank",
            ));
        }
        let top_level_shape = ordinary_union_has_top_level_shape(u);
        let top_level_wire_parent = top_level_shape
            .then(|| wire_by_name.get(u.union_name.as_str()).copied())
            .flatten();
        let top_level_wire_backed = top_level_wire_parent
            .is_some_and(|parent| matches!(parent.kind.as_str(), "union" | "discriminant"));
        // A whole-schema role union of a logical kind (fgdb-a01): the object
        // body IS the union, so the parent contract is the logical kind row
        // rather than a same-name wire row, which disjointness forbids.  Arms
        // are committed by their source-verified payload digests; there is no
        // wire-variant bijection because the union has no independent wire
        // encoding surface.
        let top_level_logical_parent = (top_level_shape && top_level_wire_parent.is_none())
            .then(|| {
                logical_by_name
                    .get(generic_free_family(u.union_name.as_str()))
                    .copied()
            })
            .flatten();
        let top_level_logical_backed = top_level_logical_parent.is_some();
        // A logical-backed whole-schema union owns its object body's tagged
        // encoding, but the plan may also name that exact union as an inline
        // field type in another schema. Keep the containing object first,
        // then require the complete sorted set of actual inline consumers.
        // This is an exact closure, not an open allowlist: an unrelated name
        // without a matching field is rejected, and a field omitted from the
        // closure is rejected below.
        let mut top_level_logical_consumer_closure = vec![u.containing_schema.as_str()];
        top_level_logical_consumer_closure.extend(
            r.fields
                .iter()
                .filter(|field| {
                    field.exact_wire_type == u.union_name
                        && field.containing_schema != u.containing_schema
                })
                .map(|field| field.containing_schema.as_str()),
        );
        top_level_logical_consumer_closure[1..].sort_unstable();
        top_level_logical_consumer_closure.dedup();
        // Resolution is by generic-free family: a generic-signed whole-schema
        // union or a union embedded in a generic-signed schema resolves
        // through the registered family row, which commits every expansion.
        let containing_family = generic_free_family(u.containing_schema.as_str());
        let containing_schema_classes =
            usize::from(logical_by_name.contains_key(containing_family))
                + usize::from(physical_names.contains(containing_family))
                + usize::from(bootstrap_names.contains(containing_family))
                + usize::from(prebootstrap_names.contains(containing_family))
                + usize::from(wire_names.contains(containing_family));
        if containing_schema_classes != 1 {
            out.push(v(
                "ordinary_union_unresolved_schema",
                "durable_fields",
                row_id,
                format!(
                    "containing_schema {:?} resolves in {containing_schema_classes} identity classes; exactly one is required",
                    u.containing_schema
                ),
            ));
        }
        if top_level_shape && top_level_logical_backed {
            let parent = top_level_logical_parent.expect("logical-backed union has a parent");
            if parent.status != u.version_status
                || u.max_size_bytes > parent.max_size_bytes
                || !role_predicate_implies(&u.role_predicate, &parent.role_predicate)
                || u.allowed_containing_schemas != top_level_logical_consumer_closure
            {
                out.push(v(
                    "ordinary_union_logical_contract_mismatch",
                    "durable_fields",
                    row_id,
                    "a whole-schema union requires a same-name logical kind parent with identical lifecycle, a bound within the object bound, no broader role scope, and an exact self-rooted closure over every inline consumer",
                ));
            }
        } else if top_level_shape {
            match top_level_wire_parent {
                Some(parent)
                    if top_level_wire_backed
                        && parent.status == u.version_status
                        && parent.max_size_bytes == u.max_size_bytes
                        && parent.allowed_containing_schemas == u.allowed_containing_schemas => {}
                _ => out.push(v(
                    "ordinary_union_wire_contract_mismatch",
                    "durable_fields",
                    row_id,
                    "a top-level ordinary union requires one same-name union/discriminant wire parent with identical lifecycle, maximum size, and exact containing-schema closure",
                )),
            }
            if let Some(parent) = top_level_wire_parent {
                let expected_parent_kind = if u.arms.iter().all(|arm| arm.payload_kind == "unit") {
                    "discriminant"
                } else {
                    "union"
                };
                if parent.kind != expected_parent_kind {
                    out.push(v(
                        "ordinary_union_wire_contract_mismatch",
                        "durable_fields",
                        row_id,
                        format!(
                            "wire parent kind {:?} does not match arm payload shape; expected {expected_parent_kind:?}",
                            parent.kind
                        ),
                    ));
                }
            }

            let expected_variants: BTreeSet<String> = u
                .arms
                .iter()
                .map(|arm| format!("{}.{}", u.union_name, arm.stable_name))
                .collect();
            let actual_variants: BTreeSet<String> = r
                .wire
                .iter()
                .filter(|wire| wire.containing_union.as_deref() == Some(u.union_name.as_str()))
                .map(|wire| wire.name.clone())
                .collect();
            if actual_variants != expected_variants {
                out.push(v(
                    "ordinary_union_wire_contract_mismatch",
                    "durable_fields",
                    row_id,
                    "top-level ordinary-union arms and registered wire variants must form an exact name bijection",
                ));
            }
            for arm in &u.arms {
                let expected_name = format!("{}.{}", u.union_name, arm.stable_name);
                match wire_by_name.get(expected_name.as_str()).copied() {
                    Some(variant)
                        if variant.kind == "union_variant"
                            && variant.containing_union.as_deref()
                                == Some(u.union_name.as_str())
                            && variant.wire_tag == Some(arm.arm_tag)
                            && variant.status == arm.version_status
                            && variant.max_size_bytes == arm.max_size_bytes
                            && variant.allowed_containing_schemas.as_slice()
                                == [u.union_name.as_str()] => {}
                    _ => out.push(v(
                        "ordinary_union_wire_contract_mismatch",
                        "durable_fields",
                        &expected_name,
                        "ordinary-union arm name, parent, tag, lifecycle, maximum size, and containing-schema closure must exactly match one wire variant",
                    )),
                }
            }
        }
        if !ordinary_union_names_seen.insert(u.union_name.as_str()) {
            out.push(v(
                "ordinary_union_name_collision",
                "durable_fields",
                row_id,
                "duplicate ordinary-union name",
            ));
        }
        let collides_with_reference = reference_union_names.contains(u.union_name.as_str());
        let collides_with_wire = BUILTIN_WIRE_TYPES.contains(&u.union_name.as_str())
            || (wire_names.contains(u.union_name.as_str()) && !top_level_wire_backed);
        if collides_with_reference {
            out.push(v(
                "ordinary_union_name_collision",
                "durable_fields",
                row_id,
                "ordinary-union name collides with a generated reference-union name",
            ));
        }
        if collides_with_wire {
            out.push(v(
                "ordinary_union_name_collision",
                "durable_fields",
                row_id,
                "ordinary-union name collides with a builtin or registered wire type",
            ));
        }
        if let Some(prior) = ordinary_union_paths.insert(
            (u.containing_schema.as_str(), u.union_path.as_str()),
            u.union_name.as_str(),
        ) {
            out.push(v(
                "ordinary_union_duplicate_path",
                "durable_fields",
                row_id,
                format!(
                    "ordinary-union path {:?} in containing schema {:?} is already assigned to {prior:?}",
                    u.union_path, u.containing_schema
                ),
            ));
        }
        if !matches!(u.tag_wire_type.as_str(), "u8" | "u16") {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                format!("tag_wire_type {:?} is not one of u8|u16", u.tag_wire_type),
            ));
        }
        if u.encoding_context != "closed-tagged" {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                format!(
                    "encoding_context {:?} must be the nonblank closed-tagged encoding",
                    u.encoding_context
                ),
            ));
        }
        check_ordinary_union_version_status(&u.version_status, row_id, &mut out);
        if u.role_predicate.trim().is_empty() || u.max_size_bytes <= 0 {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                "ordinary union requires a nonblank role predicate and positive resource bound",
            ));
        }
        let allowed_containing_schemas: BTreeSet<&str> = u
            .allowed_containing_schemas
            .iter()
            .map(String::as_str)
            .collect();
        if u.allowed_containing_schemas.is_empty()
            || allowed_containing_schemas.len() != u.allowed_containing_schemas.len()
            || u.allowed_containing_schemas
                .iter()
                .any(|schema| schema.trim().is_empty() || schema == "*")
            || (u.field_tag.is_some()
                && u.allowed_containing_schemas.as_slice() != [u.containing_schema.as_str()])
        {
            out.push(v(
                "ordinary_union_container_contract_mismatch",
                "durable_fields",
                row_id,
                "ordinary unions require a nonempty duplicate-free concrete containing-schema closure; embedded unions admit exactly their containing schema",
            ));
        }
        if u.arms.is_empty() {
            out.push(v(
                "ordinary_union_arm_missing",
                "durable_fields",
                row_id,
                "closed ordinary union has no registered arms",
            ));
        }

        let anchor_fields: Vec<_> = if collides_with_reference || collides_with_wire {
            Vec::new()
        } else {
            r.fields
                .iter()
                .filter(|field| field.exact_wire_type == u.union_name)
                .collect()
        };
        if let Some(field_tag) = u.field_tag {
            if field_tag <= 0 || field_tag >= 0xffff {
                out.push(v(
                    "code_invalid",
                    "durable_fields",
                    row_id,
                    format!("ordinary-union field_tag {field_tag:#06x} outside the valid space"),
                ));
            }
            match anchor_fields.iter().copied().find(|field| {
                field.containing_schema == u.containing_schema && field.field_tag == field_tag
            }) {
                Some(field) if anchor_fields.len() == 1 => {
                    if field.identity_class != "inline"
                        || field.reference_semantics != "none"
                        || field.target_schema_id.is_some()
                        || field.max_size_bytes < u.max_size_bytes
                        || field.version_status != u.version_status
                        || !role_predicate_implies(&field.role_predicate, &u.role_predicate)
                    {
                        out.push(v(
                            "ordinary_union_field_mismatch",
                            "durable_fields",
                            row_id,
                            "an embedded ordinary-union anchor must be inline, non-reference, target-free, large enough for the complete union encoding, lifecycle-identical, and no broader in role scope",
                        ));
                    }
                }
                Some(_) => out.push(v(
                    "ordinary_union_field_mismatch",
                    "durable_fields",
                    row_id,
                    "an embedded ordinary union must have exactly one field anchor",
                )),
                None => out.push(v(
                    "ordinary_union_field_mismatch",
                    "durable_fields",
                    row_id,
                    format!(
                        "no field row ({}, tag {}) anchors ordinary union {:?}",
                        u.containing_schema, field_tag, u.union_name
                    ),
                )),
            }
        } else if top_level_wire_backed || top_level_logical_backed {
            for field in anchor_fields {
                if field.identity_class != "inline"
                    || field.reference_semantics != "none"
                    || field.target_schema_id.is_some()
                    || field.max_size_bytes < u.max_size_bytes
                    || field.version_status != u.version_status
                    || !role_predicate_implies(&field.role_predicate, &u.role_predicate)
                    || !allowed_containing_schemas.contains(field.containing_schema.as_str())
                {
                    out.push(v(
                        "ordinary_union_field_mismatch",
                        "durable_fields",
                        row_id,
                        "a top-level ordinary-union consumer must be inline, non-reference, target-free, large enough for the complete union encoding, lifecycle-compatible, and no broader in role scope",
                    ));
                }
            }
        } else if !anchor_fields.is_empty() {
            out.push(v(
                "ordinary_union_field_mismatch",
                "durable_fields",
                row_id,
                "a top-level ordinary union without field_tag must not be used as an embedded field wire type",
            ));
        }

        let maximum_arm_tag = match u.tag_wire_type.as_str() {
            "u8" => Some(i64::from(u8::MAX)),
            // The upper quarter of the u16 space is reserved for experimental
            // assignments and cannot occur in a shipped production registry.
            "u16" => Some(0xbfff),
            _ => None,
        };
        let mut arm_tags = BTreeSet::new();
        let mut arm_names = BTreeSet::new();
        let mut source_arm_names = BTreeSet::new();
        for arm in &u.arms {
            let arm_row_id = format!("{}#{}", u.union_name, arm.stable_name);
            if arm.union_name != u.union_name
                || arm.containing_schema != u.containing_schema
                || arm.union_path != u.union_path
            {
                out.push(v(
                    "ordinary_union_arm_metadata_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "arm union name, containing schema, and union path must exactly match its ordinary union",
                ));
            }
            if arm.source_arm_name.trim().is_empty() || arm.stable_name.trim().is_empty() {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &arm_row_id,
                    "ordinary-union source arm name and stable name must be nonblank",
                ));
            }
            if arm.role_predicate.trim().is_empty() || arm.max_size_bytes <= 0 {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &arm_row_id,
                    "ordinary-union arm requires a nonblank role predicate and positive resource bound",
                ));
            }
            if !role_predicate_implies(&arm.role_predicate, &u.role_predicate) {
                out.push(v(
                    "ordinary_union_arm_role_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "ordinary-union arm role scope must be a known nonempty subset of its parent union role scope",
                ));
            }
            if arm.max_size_bytes > u.max_size_bytes {
                out.push(v(
                    "ordinary_union_arm_bound_exceeds_union",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "arm max_size_bytes {} exceeds union max_size_bytes {}",
                        arm.max_size_bytes, u.max_size_bytes
                    ),
                ));
            }
            check_ordinary_union_version_status(&arm.version_status, &arm_row_id, &mut out);
            let lifecycle_is_coherent = match u.version_status.as_str() {
                "active" => matches!(
                    arm.version_status.as_str(),
                    "active" | "reserved" | "retired"
                ),
                "reserved" => arm.version_status == "reserved",
                "retired" => arm.version_status == "retired",
                _ => true,
            };
            if !lifecycle_is_coherent {
                out.push(v(
                    "ordinary_union_arm_lifecycle_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "arm lifecycle {:?} is incompatible with ordinary-union lifecycle {:?}",
                        arm.version_status, u.version_status
                    ),
                ));
            }
            if let Some(maximum_arm_tag) = maximum_arm_tag
                && (arm.arm_tag <= 0 || arm.arm_tag > maximum_arm_tag)
            {
                out.push(v(
                    "code_invalid",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "ordinary-union arm tag {:#06x} is outside the positive production range for {}",
                        arm.arm_tag, u.tag_wire_type
                    ),
                ));
            }
            if !arm_tags.insert(arm.arm_tag) {
                out.push(v(
                    "ordinary_union_arm_duplicate_tag",
                    "durable_fields",
                    &arm_row_id,
                    format!("duplicate arm tag {}", arm.arm_tag),
                ));
            }
            if !arm_names.insert(arm.stable_name.as_str()) {
                out.push(v(
                    "ordinary_union_arm_duplicate_name",
                    "durable_fields",
                    &arm_row_id,
                    format!("duplicate stable arm name {:?}", arm.stable_name),
                ));
            }
            if !source_arm_names.insert(arm.source_arm_name.as_str()) {
                out.push(v(
                    "ordinary_union_arm_duplicate_source_name",
                    "durable_fields",
                    &arm_row_id,
                    format!("duplicate source arm token {:?}", arm.source_arm_name),
                ));
            }
            match (arm.payload_kind.as_str(), arm.payload_sha256.as_deref()) {
                ("unit", None) => {}
                ("inline-record", Some(payload_sha256))
                    if is_lowercase_sha256(payload_sha256) => {}
                ("unit", Some(_)) => out.push(v(
                    "ordinary_union_arm_payload_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "payload_kind=unit must not declare payload_sha256",
                )),
                ("inline-record", None) => out.push(v(
                    "ordinary_union_arm_payload_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "payload_kind=inline-record requires payload_sha256",
                )),
                ("inline-record", Some(_)) => out.push(v(
                    "ordinary_union_arm_payload_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "inline-record payload_sha256 must be exactly 64 lowercase hexadecimal characters",
                )),
                (payload_kind, _) => out.push(v(
                    "bad_field",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "payload_kind {payload_kind:?} is not one of unit|inline-record"
                    ),
                )),
            }
        }
    }

    // --- reference unions ---------------------------------------------------
    let mut union_names_seen = BTreeSet::new();
    for u in &r.unions {
        if !union_names_seen.insert(u.union_name.as_str()) {
            out.push(v(
                "bad_field",
                "durable_fields",
                &u.union_name,
                "duplicate reference_union name",
            ));
        }
        if BUILTIN_WIRE_TYPES.contains(&u.union_name.as_str())
            || wire_names.contains(u.union_name.as_str())
        {
            out.push(v(
                "reference_union_name_collision",
                "durable_fields",
                &u.union_name,
                "reference-union name collides with a builtin or registered wire type",
            ));
        }
        // Anchor: the declaring field row must exist and use this union.
        let anchor = r.fields.iter().find(|f| {
            f.containing_schema == u.containing_schema
                && f.field_tag == u.field_tag
                && f.exact_wire_type == u.union_name
        });
        if anchor.is_none() {
            out.push(v(
                "union_field_mismatch",
                "durable_fields",
                &u.union_name,
                format!(
                    "no field row ({}, tag {}) declares exact_wire_type {:?}",
                    u.containing_schema, u.field_tag, u.union_name
                ),
            ));
        }
        if !matches!(u.role.as_str(), "local" | "meta" | "shard") {
            out.push(v(
                "union_role_invalid",
                "durable_fields",
                &u.union_name,
                format!("role {:?} is not one of local|meta|shard", u.role),
            ));
        }
        if u.arms.is_empty() {
            out.push(v(
                "union_arm_missing",
                "durable_fields",
                &u.union_name,
                "closed reference union has no registered arms",
            ));
        }
        if let Some(containing) = logical_by_name.get(u.containing_schema.as_str())
            && !predicate_allows_role(&containing.role_predicate, &u.role)
        {
            out.push(v(
                "union_role_mismatch",
                "durable_fields",
                &u.union_name,
                format!(
                    "union role {:?} is excluded by containing schema predicate {:?}",
                    u.role, containing.role_predicate
                ),
            ));
        }
        if let Some(field) = anchor {
            if !matches!(field.reference_semantics.as_str(), "strong" | "conditional")
                || field.target_schema_id.is_some()
                || field.identity_class != "logical"
            {
                out.push(v(
                    "union_field_mismatch",
                    "durable_fields",
                    &u.union_name,
                    "union anchor must be a polymorphic logical strong/conditional reference",
                ));
            }
            if !predicate_allows_role(&field.role_predicate, &u.role) {
                out.push(v(
                    "union_role_mismatch",
                    "durable_fields",
                    &u.union_name,
                    format!(
                        "union role {:?} is excluded by anchor predicate {:?}",
                        u.role, field.role_predicate
                    ),
                ));
            }
        }
        let mut arm_tags = BTreeSet::new();
        let mut arm_targets = BTreeSet::new();
        for arm in &u.arms {
            let row_id = format!("{}#{}", u.union_name, arm.stable_name);
            if arm.union_name != u.union_name
                || arm.containing_schema != u.containing_schema
                || arm.field_tag != u.field_tag
                || arm.role != u.role
            {
                out.push(v(
                    "union_arm_metadata_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm union/anchor/role metadata does not exactly match its reference_union",
                ));
            }
            if arm.stable_name != arm.target_schema_id {
                out.push(v(
                    "union_arm_metadata_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm stable_name must equal its canonical target_schema_id",
                ));
            }
            if arm.identity_class != "logical"
                || !matches!(arm.reference_semantics.as_str(), "strong" | "conditional")
            {
                out.push(v(
                    "union_arm_identity_mismatch",
                    "durable_fields",
                    &row_id,
                    "reference-union arms must be retaining logical references",
                ));
            }
            if let Some(field) = anchor
                && (arm.reference_semantics != field.reference_semantics
                    || arm.version_status != field.version_status)
            {
                out.push(v(
                    "union_arm_lifecycle_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm reference semantics and lifecycle must match the anchored field",
                ));
            }
            if !predicate_allows_role(&arm.role_predicate, &u.role)
                || arm.retention_and_cut_rule.trim().is_empty()
                || arm.max_size_bytes <= 0
            {
                out.push(v(
                    "union_arm_policy_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm role predicate, retention rule, and resource bound must authorize its union role",
                ));
            }
            if arm.arm_tag <= 0 || arm.arm_tag >= 0xc000 {
                out.push(v(
                    "code_invalid",
                    "durable_fields",
                    &row_id,
                    format!(
                        "reference-union arm tag {:#06x} is not a production tag",
                        arm.arm_tag
                    ),
                ));
            }
            if !arm_tags.insert(arm.arm_tag) {
                out.push(v(
                    "union_arm_duplicate_tag",
                    "durable_fields",
                    &row_id,
                    format!("duplicate arm tag {}", arm.arm_tag),
                ));
            }
            if !arm_targets.insert(arm.target_schema_id.as_str()) {
                out.push(v(
                    "union_arm_duplicate_target",
                    "durable_fields",
                    &row_id,
                    format!("duplicate target {:?}", arm.target_schema_id),
                ));
            }
            match logical_by_name.get(arm.target_schema_id.as_str()) {
                None => out.push(v(
                    "union_arm_unresolved",
                    "durable_fields",
                    &row_id,
                    format!(
                        "arm {} target {:?} is not a registered logical object",
                        arm.arm_tag, arm.target_schema_id
                    ),
                )),
                Some(target_kind) => {
                    if matches!(target_kind.status.as_str(), "retired" | "experimental") {
                        out.push(v(
                            "union_arm_lifecycle_mismatch",
                            "durable_fields",
                            &row_id,
                            format!(
                                "arm target {:?} has non-referenceable lifecycle {:?}",
                                arm.target_schema_id, target_kind.status
                            ),
                        ));
                    }
                    if !predicate_allows_role(&target_kind.role_predicate, &u.role) {
                        out.push(v(
                            "union_role_mismatch",
                            "durable_fields",
                            &row_id,
                            format!(
                                "union role {:?} is excluded by target {:?} predicate {:?}",
                                u.role, arm.target_schema_id, target_kind.role_predicate
                            ),
                        ));
                    }
                    if let Some(containing) = logical_by_name.get(u.containing_schema.as_str())
                        && target_kind.construction_order > containing.construction_order
                    {
                        out.push(v(
                            "dag_future_result",
                            "durable_fields",
                            &u.union_name,
                            format!(
                                "arm target {:?} (order {}) is constructed after containing {:?} (order {}): a future result is never referenceable",
                                arm.target_schema_id,
                                target_kind.construction_order,
                                u.containing_schema,
                                containing.construction_order
                            ),
                        ));
                    }
                }
            }
        }
    }

    // --- construction DAG over logical kinds --------------------------------
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for f in &r.fields {
        if !matches!(
            f.reference_semantics.as_str(),
            "strong" | "conditional" | "weak_digest"
        ) {
            continue;
        }
        // Resolve the owner through its generic-free family, exactly as the
        // field-resolution law above does. A row whose containing_schema
        // carries a generic signature (`Foo<Role>`) is registered under the
        // bare family, so an exact-name lookup would skip it and drop its
        // edges out of the DAG entirely -- self-edges and future results
        // included.
        let containing_family = generic_free_family(f.containing_schema.as_str());
        let Some(containing) = logical_by_name.get(containing_family) else {
            continue;
        };
        let mut targets: Vec<&str> = Vec::new();
        if let Some(t) = &f.target_schema_id {
            targets.push(t.as_str());
        } else if let Some(u) = union_by_name.get(f.exact_wire_type.as_str()) {
            targets.extend(u.arms.iter().map(|arm| arm.target_schema_id.as_str()));
        }
        for target in targets {
            let Some(target_kind) = logical_by_name.get(target) else {
                continue;
            };
            let row_id = format!("{}#{}", f.containing_schema, f.stable_name);
            if target == containing_family {
                out.push(v(
                    "dag_self_edge",
                    "durable_fields",
                    &row_id,
                    "a schema may not reference itself: strong, conditional and weak_digest \
                     alike require an already-constructed target, so a lineage \
                     predecessor is compared, never traversed",
                ));
                continue;
            }
            if target_kind.construction_order > containing.construction_order {
                out.push(v(
                    "dag_future_result",
                    "durable_fields",
                    &row_id,
                    format!(
                        "target {target:?} (order {}) is constructed after {:?} (order {}): every strong value must already be known",
                        target_kind.construction_order,
                        f.containing_schema,
                        containing.construction_order
                    ),
                ));
            }
            // A typed PriorObject relation is an instance-order assertion, not
            // a weakening of reference strength: GC/checkpoint/backup walkers
            // still follow the edge exactly as its wire tag declares. It may
            // cut only a co-phased, non-self schema edge whose row carries the
            // explicit source-backed contract checked above. Invalid uses stay
            // in the schema graph and therefore fail closed.
            let valid_prior_object = f.construction_relation.as_deref()
                == Some(PRIOR_OBJECT_CONSTRUCTION_RELATION)
                && target_kind.construction_order == containing.construction_order
                && target != containing_family
                && matches!(
                    f.reference_semantics.as_str(),
                    "strong" | "conditional" | "weak_digest"
                )
                && f.target_schema_id.is_some()
                && f.retention_and_cut_rule.contains("PriorObject")
                && f.retention_and_cut_rule.contains("already-known");
            if valid_prior_object {
                continue;
            }
            edges
                .entry(containing.name.as_str())
                .or_default()
                .insert(target_kind.name.as_str());
        }
    }
    if let Some(cycle) = find_cycle_str(&edges) {
        out.push(v(
            "dag_cycle",
            "durable_fields",
            cycle.first().copied().unwrap_or(""),
            format!("construction-DAG cycle: {cycle:?}"),
        ));
    }

    check_restore_service_promotion_manifest_coherence(r, &mut out);

    out
}

/// The construction-DAG cycle law, shared by both artifacts that enforce it.
///
/// `appendix_a`'s census-level DAG calls this rather than carrying its own
/// traversal: `dag_cycle` is a law SEPARATE from `dag_future_result` — a graph
/// can be free of strict future edges and still cycle among equal-order kinds —
/// so a second implementation would be free to disagree with this one about what
/// a cycle even is.
pub(crate) fn find_construction_cycle<'a>(
    edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
) -> Option<Vec<&'a str>> {
    find_cycle_str(edges)
}

/// Iterative three-color DFS over string-keyed edges.
fn find_cycle_str<'a>(edges: &BTreeMap<&'a str, BTreeSet<&'a str>>) -> Option<Vec<&'a str>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: BTreeMap<&str, Color> = BTreeMap::new();
    for (from, targets) in edges {
        color.entry(from).or_insert(Color::White);
        for t in targets {
            color.entry(t).or_insert(Color::White);
        }
    }
    let nodes: Vec<&str> = color.keys().copied().collect();
    for start in nodes {
        if color.get(start) != Some(&Color::White) {
            continue;
        }
        let mut stack: Vec<(&str, Vec<&str>, usize)> = Vec::new();
        let children: Vec<&str> = edges
            .get(start)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        stack.push((start, children, 0));
        color.insert(start, Color::Gray);
        while let Some((node, children, idx)) = stack.last().cloned() {
            if idx < children.len() {
                if let Some(frame) = stack.last_mut() {
                    frame.2 += 1;
                }
                let child = children[idx];
                match color.get(child) {
                    Some(Color::Gray) => {
                        let mut cycle: Vec<&str> = stack.iter().map(|(n, _, _)| *n).collect();
                        if let Some(pos) = cycle.iter().position(|n| *n == child) {
                            cycle.drain(..pos);
                        }
                        cycle.push(child);
                        return Some(cycle);
                    }
                    Some(Color::White) => {
                        color.insert(child, Color::Gray);
                        let grand: Vec<&str> = edges
                            .get(child)
                            .map(|s| s.iter().copied().collect())
                            .unwrap_or_default();
                        stack.push((child, grand, 0));
                    }
                    _ => {}
                }
            } else {
                color.insert(node, Color::Black);
                stack.pop();
            }
        }
    }
    None
}
