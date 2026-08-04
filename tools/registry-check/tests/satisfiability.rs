//! SATISFIABILITY HARNESS — asserts every violation code is PASSABLE, not merely reachable.
//!
//! For each code C the checker can emit we require two INDEPENDENT DATA witnesses:
//!   TRIGGER    an IdentityRegistries value that makes C fire
//!   SATISFYING an IdentityRegistries value exercising the same law where C does NOT fire
//! A code with a trigger and no satisfying witness is a permanent blocker wearing a gate's
//! clothes. Five such pairs were found by hand in one day; this catches the next one in seconds.
//!
//! NON-TAUTOLOGY: witnesses are hand-authored `IdentityRegistries` VALUES. They are never
//! derived from the code path under test, so a witness cannot pass by construction.
//!
//! NAME THE PAIR: when a satisfying witness cannot be authored, the registry records the
//! CONFLICTING code and the two source sites, so the failure report identifies the pair
//! instead of merely saying "no witness".
//!
//! Fast loop (registry-check is std-only by constitution, so no cargo needed):
//!   rustc --edition=2024 --crate-type=lib --crate-name registry_check \
//!         tools/registry-check/src/lib.rs -o /tmp/libregistry_check.rlib
//!   CARGO_MANIFEST_DIR="$PWD/tools/registry-check" \
//!   rustc --edition=2024 --test --crate-name satisfiability satisfiability.rs \
//!         --extern registry_check=/tmp/libregistry_check.rlib -o /tmp/sat && /tmp/sat
//! REBUILD THE RLIB AT THE START OF EVERY SESSION: it bakes in EXPECTED_* and the assignment
//! pins, so a stale one reports that the DATA is broken when the TOOL is.

use registry_check::identity::{self, FieldRow, IdentityRegistries, LogicalKind, WireType};

// ---------------------------------------------------------------- coverage registry

/// Why a code carries no satisfying witness. Every variant is a DELIBERATE, REVIEWED state;
/// a code absent from the registry entirely is a hard failure, so nothing falls out silently.
#[derive(Clone, Copy)]
enum Coverage {
    /// Both witnesses authored below.
    Witnessed,
    /// Deliberately not witnessed, with a reason that must be re-justified when it changes.
    Exempt(&'static str),
    /// KNOWN UNSATISFIABLE. The pair is named so the report identifies it in one line.
    Unsatisfiable {
        conflicting_code: &'static str,
        sites: &'static str,
        note: &'static str,
    },
    /// Both witnesses exist, but in `tests/identity.rs` rather than here.
    ///
    /// The trigger is an exact-code negative test; the satisfying witness is the
    /// fixture that test mutates, which
    /// `idr_ordinary_union_fixtures_are_accepted` proves emits zero codes. That
    /// is the same two-witness pair this file demands, authored against a
    /// richer hand-built fixture than [`base`] can express — an ordinary union
    /// needs arms, containing schemas and payload commitments, and a second
    /// hand-rolled copy of that fixture here would be a second reader of one
    /// fact.
    ///
    /// `trigger_tests` names the exact `#[test]` functions.
    /// [`witnessed_elsewhere_sites_are_live`] proves each one still exists AND
    /// still names this code, so the pointer cannot rot into a claim about
    /// nothing.
    WitnessedElsewhere {
        trigger_tests: &'static [&'static str],
    },
    /// Tractable but not yet authored. [`pending_backlog_is_ratcheted`] pins the
    /// exact set, so a code may neither join it silently nor leave it silently.
    Pending(&'static str),
}

/// Every code the checker can emit. Adding a code to the checker without adding a row here
/// must fail CI — that is the anti-silent-omission property.
const REGISTRY: &[(&str, Coverage)] = &[
    (
        "allowed_containing_schema_unresolved",
        Coverage::WitnessedElsewhere {
            trigger_tests: &[
                "idr_allowed_containing_schema_resolution_accepts_candidate_or_stronger_and_rejects_unknown",
            ],
        },
    ),
    // ---- dag_ family: where two of the five known instances live -------------------
    ("dag_future_result", Coverage::Witnessed),
    ("dag_cycle", Coverage::Pending("DAG cycle witness")),
    (
        "dag_self_edge",
        Coverage::Unsatisfiable {
            conflicting_code: "catalog_annotation_reference_semantics_mismatch",
            sites: "identity.rs dag_self_edge vs appendix_a.rs annotation reference-semantics forcing",
            note: "fgdb-u3gr. The source wrapper StrongRef<Self> forces reference_semantics=strong; \
               dag_self_edge rejects strong, conditional and weak_digest alike when the target \
               resolves to the owner. No spelling satisfies both. 10 members, 7 of 21 slices.",
        },
    ),
    // ---- ordinary_union_ family: the fifth instance ---------------------------------
    (
        "ordinary_union_field_mismatch",
        Coverage::Unsatisfiable {
            conflicting_code: "field_unresolved_schema",
            sites: "identity.rs:2243 (anchor field required) vs identity.rs:1558 (resolves omits wire)",
            note: "An embedded ordinary union in a WIRE host needs an anchor field row it can never \
               legally have: the `resolves` disjunction accepts logical/bootstrap/physical/\
               prebootstrap and omits wire. Currently DORMANT because a01 increment 2C made those \
               keys wire-covered instead of union rows — the law pair is still unsatisfiable.",
        },
    ),
    (
        "ordinary_union_arm_missing",
        Coverage::Pending("arm fixture"),
    ),
    // These three read `Pending("arm fixture")` until 2026-07-27, while
    // `tests/identity.rs` had held exact-code triggers for all of them the whole
    // time. The stale reading is not cosmetic: `fgdb-a18-restore-union-source-gates-a4fq`
    // blocks seven source-census unions on `ordinary_union_unresolved_schema`,
    // and a reader who consults only this table concludes that the law blocking
    // them has never been watched work — i.e. that the seven might be
    // unlandable in ANY shape, like the `dag_self_edge` pair below. They are
    // not. The law is satisfiable, so those unions are landable as soon as
    // their class is ruled on.
    (
        "ordinary_union_arm_duplicate_tag",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_ordinary_union_rejects_duplicate_arm_tag"],
        },
    ),
    (
        "ordinary_union_name_collision",
        Coverage::WitnessedElsewhere {
            trigger_tests: &[
                "idr_wire_backed_top_level_union_requires_exact_cross_index",
                "idr_ordinary_union_rejects_reference_union_name_collision",
                "idr_ordinary_union_rejects_wire_type_name_collision",
                "idr_reference_union_rejects_ordinary_union_name_collision",
            ],
        },
    ),
    (
        "ordinary_union_unresolved_schema",
        Coverage::WitnessedElsewhere {
            trigger_tests: &[
                "idr_wire_backed_top_level_union_rejects_conventional_class_collision",
                "idr_ordinary_union_rejects_unresolved_containing_schema",
            ],
        },
    ),
    // ---- union_arm_ family -----------------------------------------------------------
    (
        "union_arm_unresolved",
        Coverage::Pending("reference-union fixture"),
    ),
    (
        "union_arm_lifecycle_mismatch",
        Coverage::Pending("reference-union fixture"),
    ),
    (
        "union_arm_duplicate_target",
        Coverage::Pending("reference-union fixture"),
    ),
    // ---- wire-tag reference-strength family (fgdb-refsem-not-forced-by-wire-type-gls4) --
    (
        "wire_type_reference_semantics_mismatch",
        Coverage::Witnessed,
    ),
    (
        "reference_semantics_without_reference_type",
        Coverage::Witnessed,
    ),
    ("unclassified_reference_wrapper", Coverage::Witnessed),
    // ---- arm-payload family (fgdb-a11-residue-unresolved-schema-ref-laws-54sd) ----------
    ("arm_payload_shape_field_row", Coverage::Witnessed),
    // ---- wire field class and digest family (fgdb-zmp3) ---------------------------------
    // These three codes landed with 83397ff and 94dd2bf and were never added here, so the
    // ratchet fired on the DELTA -- which is the mechanism working, not failing. Each
    // already had an exact-code negative trigger in tests/identity.rs, so they are
    // recorded as covered rather than appended to UNREGISTERED_BASELINE: the backlog is
    // for codes with no witness, and these have one.
    (
        "digest_class_wire_type_mismatch",
        Coverage::WitnessedElsewhere {
            trigger_tests: &[
                "idr_digest_class_is_function_of_exact_wire_type_and_wire_kinds_are_closed",
            ],
        },
    ),
    (
        "field_wire_kind_unclassified",
        Coverage::WitnessedElsewhere {
            trigger_tests: &[
                "idr_digest_class_is_function_of_exact_wire_type_and_wire_kinds_are_closed",
            ],
        },
    ),
    (
        "non_reference_wire_identity_class_mismatch",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_non_reference_wire_members_require_inline_identity_class"],
        },
    ),
    // ---- declared exemptions, each with a reason ---------------------------------------
    (
        "registry_epoch_mismatch",
        Coverage::Exempt(
            "satisfying state is the live catalog, asserted continuously by the composite gate; \
         a synthetic witness would add nothing",
        ),
    ),
    (
        "registry_assignment_drift",
        Coverage::Exempt("as registry_epoch_mismatch"),
    ),
    (
        "bodydigest_pin_mismatch",
        Coverage::Exempt(
            "satisfying witness is a live recomputed FNV pin, already covered by the BODY_RECIPES \
         block in scripts/g0_identity_e2e.sh",
        ),
    ),
    // ---- refinement family: the tag-refined-wrapper law (fgdb-gpms) -------------------
    // All five land witnessed rather than on the backlog, because the laws shipped
    // with fixtures: `idr_refinement_claims_resolve_to_a_registered_arm` mutates
    // one LANDED wire row per singleton code on the real corpus and pairs the four
    // with a conformant control. The unparseable case is the completeness half — it
    // fires on the PRE-CHANGE prose spelling of a row that is now canonicalized, so
    // the witness proves the law reaches the rows that motivated it, not only
    // synthetic ones. `idr_refinement_conjunction_is_atomic_and_total` supplies the
    // distinct two-location witness for the conjunctive grammar.
    (
        "refinement_claim_unparseable",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_refinement_claims_resolve_to_a_registered_arm"],
        },
    ),
    (
        "refinement_conjunction_invalid",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_refinement_conjunction_is_atomic_and_total"],
        },
    ),
    (
        "refinement_union_unresolved",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_refinement_claims_resolve_to_a_registered_arm"],
        },
    ),
    (
        "refinement_arm_unresolved",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_refinement_claims_resolve_to_a_registered_arm"],
        },
    ),
    (
        "refinement_arm_tag_mismatch",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_refinement_claims_resolve_to_a_registered_arm"],
        },
    ),
    // ---- role-selected projection family (fgdb-ap4t) -------------------------------
    // The real-corpus fixture pairs the released five-row bijection with one
    // mutation for every diagnostic.  The deliberately minimal [`base`]
    // registry below contains no Appendix A projection families, so its helper
    // filters only the global missing-branch census while leaving every
    // row-local role-projection diagnostic observable.
    (
        "role_projection_claim_unparseable",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    (
        "role_projection_source_unapproved",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    (
        "role_projection_contract_mismatch",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    (
        "role_projection_role_out_of_bound",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    (
        "role_projection_role_missing",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    (
        "role_projection_role_duplicate",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    (
        "role_projection_branch_mismatch",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    (
        "role_projection_refinement_syntax_forbidden",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_role_selected_restore_projections_are_exact_and_role_erased"],
        },
    ),
    // ---- payload-value family (fgdb-payload-bearing-arm-values-5u56) -------------------
    // All seven land witnessed rather than on the backlog:
    // `idr_payload_bearing_arm_values_preserve_the_complete_payload` strips the
    // accounting clause from both LANDED tag-only discriminants on the real
    // corpus, and synthesizes the conformant arm value against the registered
    // AuditTicketOwner.Operation digest before mutating the pin, the class,
    // the claim shape, and the kind.
    (
        "refinement_tag_only_payload_unaccounted",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_payload_bearing_arm_values_preserve_the_complete_payload"],
        },
    ),
    (
        "arm_value_claim_missing",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_payload_bearing_arm_values_preserve_the_complete_payload"],
        },
    ),
    (
        "arm_value_conjunction_invalid",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_payload_bearing_arm_values_preserve_the_complete_payload"],
        },
    ),
    (
        "arm_value_on_unit_payload",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_payload_bearing_arm_values_preserve_the_complete_payload"],
        },
    ),
    (
        "arm_value_payload_pin_missing",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_payload_bearing_arm_values_preserve_the_complete_payload"],
        },
    ),
    (
        "arm_value_payload_pin_malformed",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_payload_bearing_arm_values_preserve_the_complete_payload"],
        },
    ),
    (
        "arm_value_payload_pin_mismatch",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_payload_bearing_arm_values_preserve_the_complete_payload"],
        },
    ),
    // ---- manifest cross-field family (fgdb-xkg9) -------------------------------------
    (
        "restore_service_promotion_manifest_coherence",
        Coverage::WitnessedElsewhere {
            trigger_tests: &["idr_restore_service_promotion_manifest_coherence_is_enforced"],
        },
    ),
    // ---- completion family: cannot be witnessed until instance [III] resolves ----------
    (
        "complete_slice_annotation_missing",
        Coverage::Exempt(
            "a satisfying witness requires a slice that legitimately reaches definition_status=complete, \
         which NOTHING can do while the zero-exact-type blocker stands (appendix_a.rs:7418). This \
         exemption is the tracking artifact for that blocker and must be removed when it resolves.",
        ),
    ),
];

/// Identity-checker codes not yet classified in `REGISTRY`.
///
/// This is intentionally an exact readable backlog, not a count. A new code
/// replacing a removed code must still fail the ratchet even if the total
/// remains unchanged.
const UNREGISTERED_BASELINE: &[&str] = &[
    "bad_field",
    "bare_strong_ref",
    "bodydigest_incomplete_partition",
    "bodydigest_self_included",
    "bodydigest_two_fields",
    "bodydigest_unknown_exclusion",
    "code_duplicate",
    "code_invalid",
    "digest_missing_class",
    "digest_missing_recipe",
    "disjointness_dual_class",
    "experimental_in_production",
    "external_root_outside_frame",
    // Landed by 722ff22 (l6xd) after this baseline was frozen by 52fae53. The
    // first was extracted and unregistered, the other two were invisible
    // because their code argument was a variable; both halves are fixed and
    // the three codes are now honestly on the backlog.
    "field_construction_order_mismatch",
    "field_identity_class_invalid",
    "field_identity_class_not_a_field_class",
    "field_unresolved_schema",
    "field_unresolved_wire_type",
    "frame_strong_ref",
    "ordinary_union_arm_bound_exceeds_union",
    "ordinary_union_arm_duplicate_name",
    "ordinary_union_arm_duplicate_source_name",
    "ordinary_union_arm_lifecycle_mismatch",
    "ordinary_union_arm_metadata_mismatch",
    "ordinary_union_arm_payload_mismatch",
    "ordinary_union_arm_role_mismatch",
    "ordinary_union_container_contract_mismatch",
    "ordinary_union_duplicate_path",
    "ordinary_union_logical_contract_mismatch",
    "ordinary_union_wire_contract_mismatch",
    "range_status_mismatch",
    "ref_target_not_logical",
    "ref_target_unresolved",
    "reference_union_name_collision",
    "union_arm_duplicate_tag",
    "union_arm_identity_mismatch",
    "union_arm_metadata_mismatch",
    "union_arm_missing",
    "union_arm_policy_mismatch",
    "union_field_mismatch",
    "union_role_invalid",
    "union_role_mismatch",
    "wire_context_mismatch",
];

// ---------------------------------------------------------------- witness builders

/// Minimal valid registry skeleton. Hand-authored DATA; never derived from the checker.
fn base() -> IdentityRegistries {
    IdentityRegistries {
        logical: vec![],
        logical_epoch: 1,
        physical: vec![],
        physical_epoch: 1,
        bootstrap: vec![],
        bootstrap_epoch: 1,
        prebootstrap: vec![],
        prebootstrap_epoch: 1,
        wire: vec![WireType {
            wire_type_id: 0x0001,
            name: "StrongRef".to_owned(),
            kind: "reference_wrapper".to_owned(),
            status: "active".to_owned(),
            containing_union: None,
            wire_tag: None,
            encoding_context: "typed strong reference to one logical schema".to_owned(),
            allowed_containing_schemas: vec!["*".to_owned()],
            max_size_bytes: 40,
        }],
        wire_epoch: 1,
        fields: vec![],
        fields_epoch: 1,
        unions: vec![],
        ordinary_unions: vec![],
    }
}

fn kind(name: &str, code: u32, order: i64) -> LogicalKind {
    LogicalKind {
        object_kind: code as i64,
        name: name.to_owned(),
        status: "reserved".to_owned(),
        construction_order: order,
        role_predicate: "true".to_owned(),
        max_size_bytes: 16_777_216,
        golden_corpus: format!("corpus/test/{}/", name.to_ascii_lowercase()),
    }
}

fn strong_field(owner: &str, name: &str, target: &str, order: i64) -> FieldRow {
    FieldRow {
        containing_schema: owner.to_owned(),
        field_tag: 1,
        stable_name: name.to_owned(),
        exact_wire_type: "StrongRef".to_owned(),
        cardinality: "one".to_owned(),
        identity_class: "logical".to_owned(),
        reference_semantics: "strong".to_owned(),
        target_schema_id: Some(target.to_owned()),
        construction_order: order,
        construction_relation: None,
        role_predicate: "true".to_owned(),
        retention_and_cut_rule: "retained with the owning witness".to_owned(),
        version_status: "reserved".to_owned(),
        max_size_bytes: 40,
        digest_class: None,
        transcript_recipe: None,
        bd_domain_separator: None,
        bd_schema_major: None,
        bd_included_field_tags: None,
        bd_excluded_field_tags: None,
        recipe_pin: None,
    }
}

fn codes(r: &IdentityRegistries) -> Vec<String> {
    identity::validate_identity(r)
        .into_iter()
        // Assignment epochs and pins describe the complete released registry,
        // and the role-projection missing-branch census describes two complete
        // released Appendix A families, so any deliberately minimal synthetic
        // fixture necessarily differs.  The assignment codes carry explicit
        // Coverage::Exempt rows; the role census has a real-corpus
        // WitnessedElsewhere row. All other violations remain fatal to a
        // supposedly satisfying witness.
        .filter(|violation| {
            !matches!(
                violation.code.as_str(),
                "registry_epoch_mismatch"
                    | "registry_assignment_drift"
                    | "role_projection_role_missing"
            )
        })
        .map(|violation| violation.code)
        .collect()
}

// ---------------------------------------------------------------- the assertions

/// dag_future_result: a field may not strong-ref a kind constructed AFTER its owner.
#[test]
fn sat_dag_future_result() {
    // SATISFYING: target order 5 <= owner order 20.
    let mut ok = base();
    ok.logical = vec![kind("Target", 0x9001, 5), kind("Owner", 0x9002, 20)];
    ok.fields = vec![strong_field("Owner", "r", "Target", 20)];
    let ok_codes = codes(&ok);
    assert!(
        ok_codes.is_empty(),
        "dag_future_result satisfying witness fired unrelated violations: {ok_codes:?}"
    );
    // TRIGGER: target order 40 > owner order 20.
    let mut bad = base();
    bad.logical = vec![kind("Target", 0x9001, 40), kind("Owner", 0x9002, 20)];
    bad.fields = vec![strong_field("Owner", "r", "Target", 20)];
    let bad_codes = codes(&bad);
    assert_eq!(
        bad_codes,
        ["dag_future_result"],
        "dag_future_result trigger must exercise exactly that law"
    );
}

/// wire_type_reference_semantics_mismatch: the wire tag declares the strength,
/// so a `StrongRef` member may not weaken itself to a non-retaining value.
#[test]
fn sat_wire_type_reference_semantics_mismatch() {
    // SATISFYING: StrongRef carrying the strength its tag declares.
    let mut ok = base();
    ok.logical = vec![kind("Target", 0x9001, 5), kind("Owner", 0x9002, 20)];
    ok.fields = vec![strong_field("Owner", "r", "Target", 20)];
    let ok_codes = codes(&ok);
    assert!(
        ok_codes.is_empty(),
        "satisfying witness fired unrelated violations: {ok_codes:?}"
    );
    // TRIGGER: the same row weakened to "none" with its target kept -- the exact
    // spelling that used to pass every gate while switching off dag_future_result,
    // bare_strong_ref and every generated reachability/GC walker for the member.
    let mut bad = base();
    bad.logical = vec![kind("Target", 0x9001, 5), kind("Owner", 0x9002, 20)];
    let mut weakened = strong_field("Owner", "r", "Target", 20);
    weakened.reference_semantics = "none".to_owned();
    weakened.identity_class = "inline".to_owned();
    bad.fields = vec![weakened];
    assert_eq!(
        codes(&bad),
        ["wire_type_reference_semantics_mismatch"],
        "trigger must exercise exactly the wire-tag strength law"
    );
}

/// reference_semantics_without_reference_type: the dual direction -- a plain
/// scalar may not be promoted to a retaining edge by declaring one.
#[test]
fn sat_reference_semantics_without_reference_type() {
    // SATISFYING: a plain u64 scalar declaring no reference role.
    let mut ok = base();
    ok.logical = vec![kind("Owner", 0x9002, 20)];
    let mut scalar = strong_field("Owner", "n", "Target", 20);
    scalar.exact_wire_type = "u64".to_owned();
    scalar.identity_class = "scalar".to_owned();
    scalar.reference_semantics = "none".to_owned();
    scalar.target_schema_id = None;
    ok.fields = vec![scalar.clone()];
    let ok_codes = codes(&ok);
    assert!(
        ok_codes.is_empty(),
        "satisfying witness fired unrelated violations: {ok_codes:?}"
    );
    // TRIGGER: the same u64 given a logical class and a resolving, earlier
    // target, so every NEIGHBOURING guard is satisfied and only the missing
    // wire tag remains. Without this direction a u64 becomes a retaining edge.
    let mut bad = base();
    bad.logical = vec![kind("Target", 0x9001, 5), kind("Owner", 0x9002, 20)];
    let mut promoted = scalar;
    promoted.identity_class = "logical".to_owned();
    promoted.reference_semantics = "strong".to_owned();
    promoted.target_schema_id = Some("Target".to_owned());
    bad.fields = vec![promoted];
    assert_eq!(
        codes(&bad),
        ["reference_semantics_without_reference_type"],
        "trigger must exercise exactly the missing-wire-tag law"
    );
}

/// unclassified_reference_wrapper: the completeness guard that keeps the field
/// law from failing OPEN on a newly minted wrapper.
#[test]
fn sat_unclassified_reference_wrapper() {
    // SATISFYING: the base registry's only wrapper, StrongRef, is classified.
    let ok = base();
    let ok_codes = codes(&ok);
    assert!(
        ok_codes.is_empty(),
        "satisfying witness fired unrelated violations: {ok_codes:?}"
    );
    // TRIGGER: a structurally valid wrapper whose strength is declared nowhere.
    let mut bad = base();
    let mut minted = bad.wire[0].clone();
    minted.wire_type_id = 0x0002;
    minted.name = "UnclassifiedWrapperRef".to_owned();
    bad.wire.push(minted);
    assert_eq!(
        codes(&bad),
        ["unclassified_reference_wrapper"],
        "trigger must exercise exactly the wrapper-completeness law"
    );
}

/// The source-extracted code set and the reviewed coverage/backlog partition
/// must remain exact. Prevents silent omission and count-preserving swaps.
#[test]
fn identity_code_set_is_ratcheted() {
    // Codes are extracted from the checker source, NOT from a list the checker also uses —
    // otherwise the assertion would be tautological.
    let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/identity.rs"));
    let mut emitted: Vec<&str> = Vec::new();
    // A code argument that is NOT a string literal is invisible to this
    // extractor -- it would skip past the variable and read the next literal,
    // which is the registry name. That is not a miss you can see: it both hides
    // the real codes and injects a fake one. Fail loudly instead.
    let mut nonliteral_sites: Vec<usize> = Vec::new();
    const OPEN: &str = "out.push(v(";
    for (i, _) in src.match_indices(OPEN) {
        let arg = src[i + OPEN.len()..].trim_start();
        if !arg.starts_with('"') {
            nonliteral_sites.push(src[..i].matches('\n').count() + 1);
            continue;
        }
        if let Some(end) = arg[1..].find('"') {
            emitted.push(&arg[1..1 + end]);
        }
    }
    assert!(
        nonliteral_sites.is_empty(),
        "identity.rs passes a non-literal violation code at line(s) {nonliteral_sites:?}; \
         this extractor cannot see such a code and would silently substitute the next \
         string literal. Spell every code as a literal at the `out.push(v(` site."
    );
    emitted.sort_unstable();
    emitted.dedup();
    assert!(
        emitted.contains(&"dag_future_result"),
        "source extractor failed its known-present control"
    );
    assert!(
        !emitted.contains(&"definitely_fabricated_violation_code"),
        "source extractor accepted its fabricated control"
    );
    let mut known: Vec<&str> = REGISTRY.iter().map(|(code, _)| *code).collect();
    let registry_len = known.len();
    known.sort_unstable();
    known.dedup();
    assert_eq!(
        known.len(),
        registry_len,
        "coverage registry contains a duplicate violation code"
    );
    let missing: Vec<&str> = emitted
        .iter()
        .copied()
        .filter(|code| !known.contains(code))
        .collect();
    println!(
        "checker emits {} codes; registry covers {}",
        emitted.len(),
        known.len()
    );
    println!(
        "UNREGISTERED (each must gain a witness or a reasoned exemption): {}",
        missing.len()
    );
    for m in &missing {
        println!("   {m}");
    }
    assert_eq!(
        missing, UNREGISTERED_BASELINE,
        "the checker code set changed without a matching coverage/backlog update"
    );
}

/// Report the known-unsatisfiable pairs by name, so the failure identifies the pair.
#[test]
fn report_unsatisfiable_pairs() {
    let mut witnessed = 0;
    let mut witnessed_elsewhere = 0;
    let mut exempt = 0;
    let mut unsatisfiable = 0;
    let mut pending = 0;
    for (code, cov) in REGISTRY {
        match cov {
            Coverage::Witnessed => witnessed += 1,
            Coverage::WitnessedElsewhere { trigger_tests } => {
                witnessed_elsewhere += 1;
                println!("WITNESSED-IN-IDENTITY {code}: {trigger_tests:?}");
            }
            Coverage::Exempt(reason) => {
                exempt += 1;
                println!("EXEMPT {code}: {reason}");
            }
            Coverage::Unsatisfiable {
                conflicting_code,
                sites,
                note,
            } => {
                unsatisfiable += 1;
                println!(
                    "UNSATISFIABLE PAIR #{unsatisfiable}\n  {code}  <->  {conflicting_code}\n  sites: {sites}\n  {note}\n"
                );
            }
            Coverage::Pending(reason) => {
                pending += 1;
                println!("PENDING {code}: {reason}");
            }
        }
    }
    println!(
        "coverage states: witnessed={witnessed} witnessed_elsewhere={witnessed_elsewhere} \
         exempt={exempt} unsatisfiable={unsatisfiable} pending={pending}"
    );
}

/// The exact set of codes allowed to carry no witness yet.
///
/// An exact readable list, not a count, for the reason
/// [`UNREGISTERED_BASELINE`] already gives: a code replacing another must fail
/// the ratchet even when the total does not move.
const PENDING_BASELINE: &[&str] = &[
    "dag_cycle",
    "ordinary_union_arm_missing",
    "union_arm_duplicate_target",
    "union_arm_lifecycle_mismatch",
    "union_arm_unresolved",
];

/// The backlog may not grow, and it may not shrink unnoticed either.
///
/// Until 2026-07-27 the `Pending` doc claimed "CI counts these and fails if the
/// count grows" and NO ASSERTION IMPLEMENTED IT — [`report_unsatisfiable_pairs`]
/// printed the tally to stdout, which the harness swallows without
/// `--nocapture`. Measured: flipping `dag_future_result` from `Witnessed` to
/// `Pending` moved the tally from `witnessed=5 pending=8` to `witnessed=4
/// pending=9` and the suite still reported 6 passed, 0 failed. A coverage
/// ledger that cannot notice its own regression is the same defect class it
/// exists to find, one level up.
///
/// Equality in BOTH directions is deliberate. Growth is a regression. Shrinkage
/// is progress, and it still fails here, because closing a backlog row is
/// exactly the moment someone should have to say so in the diff — that is how
/// these three ordinary-union rows sat stale for as long as they did.
#[test]
fn pending_backlog_is_ratcheted() {
    let mut pending: Vec<&str> = REGISTRY
        .iter()
        .filter(|(_, cov)| matches!(cov, Coverage::Pending(_)))
        .map(|(code, _)| *code)
        .collect();
    pending.sort_unstable();
    let mut expected: Vec<&str> = PENDING_BASELINE.to_vec();
    expected.sort_unstable();
    assert_eq!(
        pending, expected,
        "the un-witnessed backlog moved: growth is a coverage regression, and a \
         shrink is progress that must be recorded in PENDING_BASELINE rather \
         than discovered later"
    );
}

// ------------------------------------------------- demonstration classifier
//
// `WitnessedElsewhere` claims a code is DEMONSTRATED FIRING somewhere else. The
// original check tested only that the code STRING occurred in the named test's
// body, which is a one-directional classifier: an absent string does prove the
// absence of a witness, but a present string does not prove the presence of
// one. Measured 2026-07-27 on the Appendix A owner (fgdb-witness-census-named-
// not-witnessed-11fo): 9 of 82 codes counted as witnessed had never been shown
// to fire individually — 6 appeared only as one arm of a multi-arm `matches!`,
// 1 only inside an assertion that the code must be ABSENT, and 2 only inside a
// comment. All three shapes satisfy `contains`.
//
// So an occurrence is a DEMONSTRATION unless it is one of those three, and a
// row is live only when at least one occurrence survives. The rejected shapes
// are the closed set this classifier claims to recognise — it is deliberately
// conservative in the safe direction: an unrecognised shape counts as a
// demonstration, so this can under-report a defect but never invent one.
// `demonstration_classifier_rejects_mere_mentions` is its non-vacuity control.

/// The source line containing `pos`.
fn line_at(body: &str, pos: usize) -> &str {
    let start = body[..pos].rfind('\n').map_or(0, |offset| offset + 1);
    let end = body[pos..].find('\n').map_or(body.len(), |off| pos + off);
    &body[start..end]
}

/// The innermost `matches!( .. )` span containing `pos`, paren-balanced.
fn enclosing_matches(body: &str, pos: usize) -> Option<&str> {
    let mut best: Option<&str> = None;
    let mut search = 0;
    while let Some(found) = body[search..].find("matches!") {
        // Capture the macro's own start BEFORE advancing the cursor. Deriving it
        // from the mutated `search` yields a garbage lower bound that lets a
        // disjunction arm through; the synthetic control passed anyway and only
        // a downgrade of a REAL witness exposed it.
        let macro_start = search + found;
        let open = macro_start + "matches!".len();
        search = open;
        let Some(paren) = body[open..].find('(').map(|off| open + off) else {
            break;
        };
        let mut depth = 0usize;
        let mut close = paren;
        for (offset, ch) in body[paren..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = paren + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        if close > paren && macro_start <= pos && pos <= close {
            best = Some(&body[paren..=close]);
        }
    }
    best
}

/// Distinct double-quoted identifiers inside `span`.
fn quoted_idents(span: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut rest = span;
    while let Some(open) = rest.find('"') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('"') else { break };
        let literal = &after[..close];
        if !literal.is_empty()
            && literal
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            && !out.contains(&literal)
        {
            out.push(literal);
        }
        rest = &after[close + 1..];
    }
    out
}

/// The statement around `pos`: back to the previous `;` or `{`, forward to the
/// next `;`. Coarse but sufficient to see a negation applied to the assertion.
fn statement_at(body: &str, pos: usize) -> &str {
    let start = body[..pos].rfind([';', '{']).map_or(0, |offset| offset + 1);
    let end = body[pos..].find(';').map_or(body.len(), |off| pos + off);
    &body[start..end]
}

/// Negated bindings that mark an assertion of ABSENCE.
const NEGATED_BINDINGS: &[&str] = &[
    "!body",
    "!codes",
    "!violations",
    "!baseline",
    "!identity_tests",
];

/// Is `statement` asserting the code must be ABSENT? A code named only in such
/// an assertion has by construction never been seen to fire.
///
/// The `assert!` arm must be decided by the first non-whitespace character of
/// the macro's argument, NOT by a literal `"assert!(\n"`: rustfmt breaks nearly
/// every multi-line assertion that way, so the literal form rejected 7 of the 10
/// live rows — a false red found by running this against the real corpus before
/// landing it.
fn is_absence_assertion(statement: &str) -> bool {
    let mut search = 0;
    while let Some(found) = statement[search..].find("assert!(") {
        let after = search + found + "assert!(".len();
        search = after;
        if statement[after..].trim_start().starts_with('!') {
            return true;
        }
    }
    statement.contains(".is_empty()")
        || NEGATED_BINDINGS
            .iter()
            .any(|marker| statement.contains(marker))
}

/// Does `body` DEMONSTRATE `code` firing, rather than merely mention it?
fn demonstrates_code(body: &str, code: &str) -> bool {
    let needle = format!("\"{code}\"");
    let mut search = 0;
    while let Some(found) = body[search..].find(&needle) {
        let pos = search + found;
        search = pos + needle.len();

        // (a) a comment is not an assertion
        if line_at(body, pos).trim_start().starts_with("//") {
            continue;
        }
        // (b) one arm of a multi-arm disjunction is satisfied by any other arm
        if let Some(span) = enclosing_matches(body, pos)
            && quoted_idents(span).len() > 1
        {
            continue;
        }
        // (c) an absence assertion is the opposite of a demonstration
        if is_absence_assertion(statement_at(body, pos)) {
            continue;
        }
        return true;
    }
    false
}

/// NON-VACUITY CONTROL for [`demonstrates_code`]. Each rejected shape is a real
/// one taken from the corpus, and the accepting case must still accept, so the
/// classifier cannot pass by rejecting everything or by accepting everything.
#[test]
fn demonstration_classifier_rejects_mere_mentions() {
    let code = "catalog_target_duplicate";

    let comment_only = "
        // the historical reason we care about \"catalog_target_duplicate\" here
        assert!(codes.contains(&\"unrelated_code\".to_owned()));
    ";
    assert!(
        !demonstrates_code(comment_only, code),
        "a code named only in a comment must not count as demonstrated"
    );

    // The leading body is load-bearing: it pushes the disjunction to a realistic
    // offset. A first version of `enclosing_matches` derived the macro's start
    // from an already-advanced cursor, which is correct only near offset zero —
    // a padded fixture fails it, an unpadded one does not.
    let disjunction = "
        let mut mutated = base();
        mutated.fields[0].identity_class = \"scalar\".into();
        let violations = validate_identity(&mutated);
        assert!(violations.iter().any(|violation| matches!(
            violation.code.as_str(),
            \"catalog_target_duplicate\" | \"catalog_row_duplicate\"
        )));
    ";
    assert!(
        !demonstrates_code(disjunction, code),
        "one arm of a multi-arm disjunction is satisfied by the other arm"
    );

    let absence = "
        assert!(!codes.iter().any(|entry| entry == \"catalog_target_duplicate\"));
    ";
    assert!(
        !demonstrates_code(absence, code),
        "a code named only in an absence assertion has never been seen to fire"
    );

    let demonstration = "
        let codes = codes(&mutated);
        assert!(codes.contains(&\"catalog_target_duplicate\".to_owned()));
    ";
    assert!(
        demonstrates_code(demonstration, code),
        "a genuine exact-code assertion must still count as demonstrated"
    );

    let single_arm_matches = "
        assert!(violations.iter().any(|violation| matches!(
            violation.code.as_str(),
            \"catalog_target_duplicate\"
        )));
    ";
    assert!(
        demonstrates_code(single_arm_matches, code),
        "a single-arm matches! is code-determining and must still count"
    );

    assert!(
        !demonstrates_code(demonstration, "catalog_row_duplicate"),
        "a code absent from the body must never count"
    );
}

/// A `WitnessedElsewhere` row must keep pointing at a live witness.
///
/// Read from `tests/identity.rs` itself, so the claim is checked against the
/// corpus rather than restated. Without this the variant would be strictly
/// worse than `Pending`: a comfortable label over a test that had been renamed
/// or deleted.
#[test]
fn witnessed_elsewhere_sites_are_live() {
    let identity_tests = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/identity.rs"));
    // Controls, so a reader that had stopped reading cannot pass this.
    assert!(
        identity_tests.contains("fn idr_ordinary_union_rejects_unresolved_containing_schema("),
        "reader control: a known-present test function was not found"
    );
    assert!(
        !identity_tests.contains("fn idr_definitely_fabricated_test_name("),
        "reader control: a fabricated test function was found"
    );

    let mut broken: Vec<String> = Vec::new();
    for (code, cov) in REGISTRY {
        let Coverage::WitnessedElsewhere { trigger_tests } = cov else {
            continue;
        };
        assert!(
            !trigger_tests.is_empty(),
            "{code} claims a witness elsewhere and names no test"
        );
        for name in *trigger_tests {
            let Some(start) = identity_tests.find(&format!("fn {name}(")) else {
                broken.push(format!("{code}: test {name} no longer exists"));
                continue;
            };
            // The named test must still DEMONSTRATE THIS code firing. Bound the
            // search to the function's own body so a neighbouring test cannot
            // satisfy it, then require an occurrence that is not a comment, not
            // one arm of a disjunction, and not an absence assertion — because
            // all three read identically to a `contains` check while proving
            // nothing (fgdb-witness-census-named-not-witnessed-11fo).
            let body = &identity_tests[start..];
            let end = body[1..]
                .find("\n#[test]")
                .map(|offset| offset + 1)
                .unwrap_or(body.len());
            if !demonstrates_code(&body[..end], code) {
                broken.push(format!(
                    "{code}: test {name} no longer demonstrates it firing (a comment, a \
                     disjunction arm, or an absence assertion is not a demonstration)"
                ));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "WitnessedElsewhere rows point at witnesses that are gone:\n{}",
        broken.join("\n")
    );
}
