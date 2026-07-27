//! Canonical Appendix A catalog, source verifier, and identity projections.
//!
//! The catalog is the one authoring surface.  Its typed projection rows are
//! parsed through the same strict models used by the six checked-in consumer
//! registries; deterministic rendering and byte comparison prevent those
//! projections from becoming independent authorities.

use crate::appendix_reference::{ReferenceTarget, census_plan_references};
use crate::appendix_source::{
    AmbiguityCandidate, AmbiguityKind, AppendixSourceCensus, ArmCandidate, FieldCandidate,
    SchemaCandidate, SchemaOwnerStatus, SourceSliceSpec, UnionCandidate, census_appendix_source,
};
use crate::hash::sha256_hex;
use crate::identity::{self, IdentityRegistries};
use crate::toml::{self, Table, Value};
use crate::{architecture, model};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const CATALOG_SCHEMA_VERSION: i64 = 5;
pub const CATALOG_NAME: &str = "appendix_a_catalog";
pub const CATALOG_EPOCH: i64 = 5;
pub const ROW_ID_GRAMMAR_VERSION: i64 = 3;
pub const DIAGNOSTIC_VERSION: i64 = 1;
pub const CANONICAL_ORDER: &str = "source-key,projection-registry,assigned-code,containing-schema,union-path,field-tag,arm-tag,row-id";
pub const CATALOG_PATH: &str = "registries/appendix_a_catalog.toml";
pub const PLAN_PATH: &str = "COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md";
pub const SOURCE_ENCODING: &str = "utf-8-lf";
pub const HASH_ALGORITHM: &str = "sha256";

pub const APPENDIX_START_LINE: i64 = 1388;
pub const APPENDIX_END_LINE: i64 = 2728;
pub const APPENDIX_LINE_COUNT: i64 = 1341;
pub const APPENDIX_BYTE_COUNT: i64 = 1_022_462;
pub const APPENDIX_SHA256: &str =
    "2c2c119c8b627601933c73c60a161dcca041119034a1b657a6a44c1dbd10d06b";
pub const APPENDIX_HEADING: &str = "## Appendix A — On-Disk Object Formats (normative contract)";
pub const NEXT_HEADING: &str = "## Appendix B — Graph Intent Log (the semantic vocabulary)";
pub const EXPECTED_PROJECTION_ROW_COUNT: usize = 3318;
pub const EXPECTED_PROJECTION_ROW_IDS_SHA256: &str =
    "571ffd2d825d1b41af007ccd28ff88eb3d5faa4776e2eef5f2be0ded7114ff1d";
pub const EXPECTED_PROJECTION_FALLBACK_COUNT: usize = 111;
pub const EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256: &str =
    "ea4bb314b26e1d35f71869223c3efb7d60c2fcbb3a454699beda45e8bb80440a";
pub const EXPECTED_ANNOTATION_COUNT: usize = 0;
pub const EXPECTED_ANNOTATION_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const EXPECTED_SEMANTIC_BINDING_COUNT: usize = 0;
pub const EXPECTED_SEMANTIC_BINDING_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const EXPECTED_EXPANSION_BINDING_COUNT: usize = 0;
pub const EXPECTED_EXPANSION_BINDING_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const EXPECTED_EVIDENCE_BINDING_COUNT: usize = 0;
pub const EXPECTED_EVIDENCE_BINDING_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const COMPLETION_LAYER_SCHEMA_VERSION: i64 = 1;
pub const EXPECTED_COMPLETION_LAYER_SCHEMA_COUNT: usize = 4;
pub const EXPECTED_COMPLETION_LAYER_SCHEMA_SHA256: &str =
    "ee52b411cccac39b2189bf42aaaeb7d5e08c9de4ac59f313e26471ab05f525be";
pub const EXPECTED_AMBIGUITY_ADJUDICATION_COUNT: usize = 438;
pub const EXPECTED_AMBIGUITY_ADJUDICATION_SHA256: &str =
    "3f18c603462fda3f7cf4420426e445cf787128bc3ee668d49fdb466f34578db5";
pub const EXPECTED_TYPE_RESERVATION_COUNT: usize = 813;
pub const EXPECTED_EXISTING_TYPE_RESERVATION_COUNT: usize = 437;
pub const EXPECTED_RESERVED_TYPE_RESERVATION_COUNT: usize = 376;
pub const EXPECTED_RESERVATION_HIGH_WATER: u16 = 0x051d;
pub const EXPECTED_RESERVATION_ASSIGNMENT_SHA256: &str =
    "70461e343be25b55641eac6370c77a67f2aee60c768880e46dd2ab88979fe452";
pub const EXPECTED_REFERENCE_TARGET_IDS_SHA256: &str =
    "84276b6d97342e9ec1619424ddacb5b429e98e1862e03359afc837b65bb3392e";
pub const EXPECTED_REFERENCE_OCCURRENCE_COUNT: usize = 2_455;
pub const EXPECTED_REFERENCE_OCCURRENCE_SHA256: &str =
    "c715512f6adf746157692028a3fdf001dd5ce4f5dfb54b4d87a2f1957d92298a";
pub const EXPECTED_G0_PROJECTION_ROW_COUNT: usize = 35;
pub const EXPECTED_G0_PROJECTION_ROW_IDS_SHA256: &str =
    "ff344794c0f061e83016f9f4844591a75d07bff597d439258d2b2632fc810d61";
pub const EXPECTED_SLICE_PROJECTION_CLASSES_SHA256: &str =
    "1bf2a60d904083bc19a196b6dc86c67f57c33009031460a5f7be2b32c10146fd";
pub const MAINTENANCE_PROOF_ROW_ID: &str = "catalog:maintenance-proof:appendix-a";
pub const MAINTENANCE_OWNER_BEAD: &str = "fgdb-appendix-a-catalog-scaffold-gvvf";
pub const MAINTENANCE_OWNER_CRATE: &str = "registry-check";

pub const APPENDIX_EVIDENCE_EVENT_IDS: [&str; 11] = [
    "appendix_closure_checked",
    "appendix_completed",
    "appendix_generation_completed",
    "appendix_projection_checked",
    "appendix_projection_generated",
    "appendix_projection_regenerated",
    "appendix_regeneration_completed",
    "appendix_reference_manifest",
    "appendix_slice_checked",
    "appendix_source_manifest",
    "appendix_target_manifest",
];

#[derive(Debug, Clone, Copy)]
struct EvidenceScenarioSpec {
    id: &'static str,
    checker_id: &'static str,
    checker_kind: &'static str,
    checker_artifact: &'static str,
    status: &'static str,
    event_ids: &'static [&'static str],
    gate_ids: &'static [&'static str],
    target_manifest_sha256: Option<&'static str>,
    target_row_ids: &'static [&'static str],
}

const APPENDIX_EVIDENCE_SCENARIOS: [EvidenceScenarioSpec; 1] = [EvidenceScenarioSpec {
    id: "g0_identity_e2e",
    checker_id: "g0_identity_e2e",
    checker_kind: "script",
    checker_artifact: "scripts/g0_identity_e2e.sh",
    status: "live",
    event_ids: &APPENDIX_EVIDENCE_EVENT_IDS,
    gate_ids: &["G0"],
    target_manifest_sha256: Some(EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256),
    target_row_ids: &[],
}];

#[derive(Debug, Clone, Copy)]
struct CheckerContractSpec {
    id: &'static str,
    kind: &'static str,
    artifact: &'static str,
    status: &'static str,
}

const APPENDIX_MAINTENANCE_CHECKERS: [CheckerContractSpec; 3] = [
    CheckerContractSpec {
        id: "appendix_a_catalog_closure",
        kind: "binary",
        artifact: "tools/registry-check/src/appendix_a.rs",
        status: "live",
    },
    CheckerContractSpec {
        id: "appendix_a_catalog_projection_diff",
        kind: "binary",
        artifact: "tools/registry-check/src/appendix_a.rs",
        status: "live",
    },
    CheckerContractSpec {
        id: "appendix_a_catalog_source",
        kind: "binary",
        artifact: "tools/registry-check/src/appendix_a.rs",
        status: "live",
    },
];

#[derive(Debug, Clone, Copy)]
struct SemanticBindingContractPin {
    row_id: &'static str,
    target_row_id: &'static str,
    target_source_key: &'static str,
    owner_bead_id: &'static str,
    owner_crate: &'static str,
    owner_status: &'static str,
    consumer_crates: &'static [&'static str],
}

#[derive(Debug, Clone, Copy)]
struct AnnotationContractPin {
    row_id: &'static str,
    target_row_id: &'static str,
    target_source_key: &'static str,
    exact_type: &'static str,
    cardinality: &'static str,
    layout: &'static str,
    role: &'static str,
    posture: &'static str,
    authority: &'static str,
    locality: &'static str,
    generic_expansions: &'static [&'static str],
    role_expansions: &'static [&'static str],
    reference_semantics: &'static str,
    target_schema_ids: &'static [&'static str],
    construction_order: &'static str,
    retention_and_cut_rule: &'static str,
    digest_recipe: &'static str,
    redaction_class: &'static str,
    resource_bounds: &'static str,
    compatibility: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct EvidenceBindingContractPin {
    row_id: &'static str,
    target_row_id: &'static str,
    target_source_key: &'static str,
    evidence_id: &'static str,
    phase: &'static str,
    status: &'static str,
    owner_bead_id: &'static str,
    checker_ids: &'static [&'static str],
    scenario_ids: &'static [&'static str],
    event_ids: &'static [&'static str],
    gate_ids: &'static [&'static str],
}

#[derive(Debug, Clone, Copy)]
struct CompletionLayerSchemaContractPin {
    layer: &'static str,
    schema_version: i64,
    field_contracts: &'static [&'static str],
    target_binding: &'static str,
    target_cardinality: &'static str,
    epoch_domain: &'static str,
    projection_policy: &'static str,
    authoring_policy: &'static str,
    pin_policy: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct ExpansionBindingContractPin {
    row_id: &'static str,
    target_row_id: &'static str,
    target_source_key: &'static str,
    parameter_ordinal: i64,
    formal: &'static str,
    formal_class: &'static str,
    values: &'static [&'static str],
    rationale: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct AmbiguityAdjudicationContractPin {
    row_id: &'static str,
    slice_id: &'static str,
    ambiguity_source_key: &'static str,
    source_locations: &'static [&'static str],
    resolution: &'static str,
    resolved_source_keys: &'static [&'static str],
    rationale: &'static str,
}

const ANNOTATION_FIELD_CONTRACTS: [&str; 19] = [
    "row_id:string:required",
    "target_row_id:string:required",
    "exact_type:string:required",
    "cardinality:string:required",
    "layout:string:required",
    "role:string:required",
    "posture:string:required",
    "authority:string:required",
    "locality:string:required",
    "generic_expansions:string-array:required",
    "role_expansions:string-array:required",
    "reference_semantics:string:required",
    "target_schema_ids:string-array:required",
    "construction_order:string:required",
    "retention_and_cut_rule:string:required",
    "digest_recipe:string:required",
    "redaction_class:string:required",
    "resource_bounds:string:required",
    "compatibility:string:required",
];

const SEMANTIC_BINDING_FIELD_CONTRACTS: [&str; 6] = [
    "row_id:string:required",
    "target_row_id:string:required",
    "owner_bead_id:string:required",
    "owner_crate:string:required",
    "owner_status:string:required",
    "consumer_crates:string-array:required",
];

const EXPANSION_BINDING_FIELD_CONTRACTS: [&str; 7] = [
    "row_id:string:required",
    "target_row_id:string:required",
    "parameter_ordinal:integer:required",
    "formal:string:required",
    "formal_class:string:required",
    "values:string-array:required",
    "rationale:string:required",
];

const EVIDENCE_FIELD_CONTRACTS: [&str; 10] = [
    "row_id:string:required",
    "target_row_id:string:required",
    "evidence_id:string:required",
    "phase:string:required",
    "status:string:required",
    "owner_bead_id:string:required",
    "checker_ids:string-array:required",
    "scenario_ids:string-array:required",
    "event_ids:string-array:required",
    "gate_ids:string-array:required",
];

const COMPLETION_LAYER_SCHEMA_CONTRACT: [CompletionLayerSchemaContractPin; 4] = [
    CompletionLayerSchemaContractPin {
        layer: "annotation",
        schema_version: COMPLETION_LAYER_SCHEMA_VERSION,
        field_contracts: &ANNOTATION_FIELD_CONTRACTS,
        target_binding: "target_row_id->target.target_row_id",
        target_cardinality: "zero-or-one-per-target;exactly-one-approved-when-complete",
        epoch_domain: "catalog-epoch-on-shape-change;content-pins-on-row-change",
        projection_policy: "catalog-only;appendix-regenerate-does-not-project",
        authoring_policy: "reviewed-source-assisted;policy-fields-owner-authored",
        pin_policy: "compiled-count-sha256-readable-row-contract",
    },
    CompletionLayerSchemaContractPin {
        layer: "semantic_binding",
        schema_version: COMPLETION_LAYER_SCHEMA_VERSION,
        field_contracts: &SEMANTIC_BINDING_FIELD_CONTRACTS,
        target_binding: "target_row_id->target.target_row_id",
        target_cardinality: "zero-or-one-per-target;exactly-one-approved-when-complete",
        epoch_domain: "catalog-epoch-on-shape-change;content-pins-on-row-change",
        projection_policy: "catalog-only;appendix-regenerate-does-not-project",
        authoring_policy: "reviewed-owner-authored",
        pin_policy: "compiled-count-sha256-readable-row-contract",
    },
    CompletionLayerSchemaContractPin {
        layer: "expansion_binding",
        schema_version: COMPLETION_LAYER_SCHEMA_VERSION,
        field_contracts: &EXPANSION_BINDING_FIELD_CONTRACTS,
        target_binding: "target_row_id->target.target_row_id",
        target_cardinality: "zero-or-one-per-target-parameter-ordinal;exact-source-dimensions",
        epoch_domain: "catalog-epoch-on-shape-change;content-pins-on-row-change",
        projection_policy: "catalog-only;appendix-regenerate-does-not-project",
        authoring_policy: "reviewed-source-dimension-assisted;rationale-owner-authored",
        pin_policy: "compiled-count-sha256-readable-row-contract",
    },
    CompletionLayerSchemaContractPin {
        layer: "evidence",
        schema_version: COMPLETION_LAYER_SCHEMA_VERSION,
        field_contracts: &EVIDENCE_FIELD_CONTRACTS,
        target_binding: "target_row_id->target.target_row_id",
        target_cardinality: "zero-or-one-per-target-evidence-id;complete-needs-static-live-g0-and-runtime",
        epoch_domain: "catalog-epoch-on-shape-change;content-pins-on-row-change",
        projection_policy: "catalog-only;appendix-regenerate-does-not-project",
        authoring_policy: "reviewed-owner-authored",
        pin_policy: "compiled-count-sha256-readable-row-contract",
    },
];

// These independent, readable pins are deliberately empty while all A01-A21
// slices are declared. A slice may add completion metadata only by adding the
// exact reciprocal target/source/schema/owner/evidence contract here in
// reviewed code; changing the opaque transcript digest alone is never
// authorization.
const ANNOTATION_CONTRACT: [AnnotationContractPin; 0] = [];
const SEMANTIC_BINDING_CONTRACT: [SemanticBindingContractPin; 0] = [];
const EXPANSION_BINDING_CONTRACT: [ExpansionBindingContractPin; 0] = [];
const EVIDENCE_BINDING_CONTRACT: [EvidenceBindingContractPin; 0] = [];
static AMBIGUITY_ADJUDICATION_CONTRACT: [AmbiguityAdjudicationContractPin; 438] = [
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9902cb5d9fadf41a985fd54c1bc021af6ff2e124af9886e02fb808aac5c05459",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ExportLeaf|ExportLeaf<T>.authority_ledger_floor|c4bc39c591a9d281324c07f586b397d3f220f760fd3a710901241bb520821a36|1|b38933c6686aeb4d3685cd28e45711bba35d89372cdc71fbbe206366f4c8d5fe|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.authority_ledger_floor|authority_ledger_floor",
        ],
        rationale: "a01:1400: shorthand member `authority_ledger_floor` carries no inline exact type, and ExportLeaf is a logical kind, so no wire envelope commits the span. The a01 owner ruling landed in 694d14b fixes it to `u64` on the registered durable_fields row, derived from the a01 floor family: authority_retention_floor, minimum_authority_checkpoint_floor and authority_order_index are all u64/8. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:99a87928b4e9051fadedb901f4799986579d307add86f64e1c8848d530e53adf",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|CertifiedRemoteStrongRef|CertifiedRemoteStrongRef<T>|b39fefc96a603234b2dc09f7edf2008ca2d1feb141cd96771a98ef3e16761e41|1|5e088929034b341574f29a45686cdf1d5d9557cda5f89b7dd5e7916593213374|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|CertifiedRemoteStrongRef<T>"],
        rationale: "a01:1402: `CertifiedRemoteStrongRef<T> {...}` is introduced as W12's one cross-consensus edge with its full brace body; the flagged span is the top candidate's own normative definition.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b73053d5a89314ce34bf5ab28ab0942c5ba8aa5c2d1cd43a6f59ff4449e15438",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|ConditionalGlobalCommandRef|ConditionalGlobalCommandRef|1c70ec29af199b478f5d1baad846385bccc9c8edf1fc9f7f9508bf8a5c5219d5|1|18cd870256f63617ce81239b64eb7facf86b7b9c86f83b103dd860e14d94fb69|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalGlobalCommandRef"],
        rationale: "a01:1406: `ConditionalGlobalCommandRef {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|ConditionalGlobalCommandRef` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6b485c80a37d34cd7e268be5fa2499117ce1c88914eaafab9bc9ee53e32cc15f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef|d4a3bb436c751796fe3fb6c545137f95f7652b8c0d31ab5f6fcf505851d80db9|1|87eb43319218196e5ebfb267fd6508096f1a4a7c506cff1673f136d4b93854a2|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalGlobalTxnInputRef"],
        rationale: "a01:1406: `ConditionalGlobalTxnInputRef {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|ConditionalGlobalTxnInputRef` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c1de29a1f04f3d29608d42035d829d168499200bf1449172de752428a74f6ba4",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|unparsed-trailing-tokens|ConditionalMarkerRef|ConditionalMarkerRef.axis.Branch|432cef30c5ade11e7f90c50e8dc1cbb9de5b48248640ae73638235160159ea5d|1|d3c6d14c50bec6f204c2f7e4935cb8a075836aa77c10910db544f330ae064655|tokens after a union arm name are not part of the closed source grammar",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["arm|ConditionalMarkerRef|ConditionalMarkerRef.axis|Branch"],
        rationale: "a01:1394: `ConditionalMarkerRef{marker_ref,axis:Global|Branch(graph,branch)}` renders the `Branch` axis payload in tuple form rather than the brace form the closed source grammar expects, so the parenthesised `(graph,branch)` is flagged as trailing tokens; those tokens are that arm's own payload, not stray text, and the arm and its interior are committed byte-exactly by the exact ConditionalMarkerRef wire envelope contract. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e5067c1188355a4aeedc045cd474f780b8f80e01a0e129dcfd0569e5dbf960c0",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|ConditionalShardCommandRef|ConditionalShardCommandRef|6c093c951aaa6a1f6ac0d7c10df11df3f43dd684da1a7e7dfc26d3b848f95137|1|a43c710d12c846a7f45f8d5d43b5b8ff208be33d1601089bc6afd3248b01a353|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalShardCommandRef"],
        rationale: "a01:1406: `ConditionalShardCommandRef {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|ConditionalShardCommandRef` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6a6f71c5287f6e68eedbe69fa907319d95baf3c892ae49eb331e73f76a5a81bb",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|ExportLeaf|ExportLeaf<T>|2a6149ec6651f4d0d7c096f1be0e9bf960b418fa297725dedbd9ce60427b72d0|1|9a28b0dd7bda9622920d201883d9d5e96eba4f6546f8b80fd13fbfa5d9b79e3d|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ExportLeaf<T>"],
        rationale: "a01:1400: `ExportLeaf<T> {...}` is defined with its full brace body as the imported representation of authority-local `T`; the flagged span is the top candidate's own normative definition.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f6b057d813024d9cdae86474e26e70f832b8b56ea96997303e2ea6e8d9fb180f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|MarkerRef|MarkerRef|bf34630f476a4a5651ddc0bb643f4c3c8ca734034ddf965e1928b3675cbebcfc|1|d3da3893e9e3d9cc0fb28faaaf071e15df018a5ff9c918f0503e2f2fccda5de0|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|MarkerRef"],
        rationale: "a01:1394: `MarkerRef{marker_oid:[u8;32],commit_seq:u64}` is the slice's explicit bare-identity schema ('identities, not reachability by themselves'); the brace body is the normative definition of the top candidate despite lacking a heading cue.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:19071118724e502558c8001fc247894ad3c6e95f24063c3c06a7259543443905",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId|70c0603c29e5e1356b75d95097c8c6169aa4caf40488b24a7890938f164cf6a1|1|7b328a6974a5d4010e974e3c5ed04ed52d1942ce154471d6544eead1534710b8|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale: "a01:1443: the specialized `PlacementDescriptorWithoutId {...}` body that RootSlot+RootBootstrap fields reproduce byte-for-byte is the normative rendering of the top candidate; the surrounding sentence merely lacks an ownership heading.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c4d2564bf7c395c7b349e663138fcb4c1e4361690d3c26044b1aecf73e43ec0e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry|a5e981301589a1520aca35647fa484dc71349aa0420c1e4b4a1609bf1cdc8110|1|c7e39cfafaac6f72e3689ce663cb1ff19b0c66b694880edeaca387ac0874529a|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteReleaseSummaryEntry"],
        rationale: "a01:1404: `RemoteReleaseSummaryEntry {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteReleaseSummaryEntry` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0da0b826f748cf4bd8faa497654351a2a9764542ea6982b41924de7afa6d745f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionAckPublishRecord|RemoteRetentionAckPublishRecord|b135cc54161d56875ec002ba4886b7dbe72fe7e3ef6e505624c30f49a239a02f|1|c2980268d682dbb341097c95de3d1595b76ac4c7aafde14709fffcd917a5f92d|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionAckPublishRecord"],
        rationale: "a01:1404: `RemoteRetentionAckPublishRecord {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionAckPublishRecord` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f91e77715cb9aae0faef9408747017b3a72f0d8d8c57bc1ab44771bba3169884",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionConsumeAckRecord|RemoteRetentionConsumeAckRecord|e12ec4268849c30da06a685d3160d4d4c01ae6e6197c835a2f301bf80aa6a5b2|1|95541dcaf7f3351ca22b6bc8ccfacc1bdd2f499457c1883ad4e1e491c75533c1|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionConsumeAckRecord"],
        rationale: "a01:1404: `RemoteRetentionConsumeAckRecord {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionConsumeAckRecord` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4286033216d0e30f33f3289adca37f9ae9dbd4cdcfb89adbc1b94aa8cf488b43",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence|ae9272fed9c1926d9dd22ee518136372998b6d6473da1f395f8e68839d57a7a4|1|657b3abd8db11dbb6f0da4b18e0f7a5308b2aa35e1bc3f6519fef33f7507a7f5|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionGrantEvidence"],
        rationale: "a01:1402: `RemoteRetentionGrantEvidence {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionGrantEvidence` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ca4727cb8f2c1151bad56af9e8998591d000a55bf9cf95566c5ddcafb4911df2",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionGrantRecord|RemoteRetentionGrantRecord|63cf2d41141b954ad102692841aeb1339b95eb0aab3343333dff25c65ff8e259|1|23fe225999d1d4c0b9a69c357b96198f579c2002eb6b622bf77078ebe3ec79b1|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionGrantRecord"],
        rationale: "a01:1402: `RemoteRetentionGrantRecord {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionGrantRecord` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9201ddc13a840cea2d41c1df2285130c96caaf6c76b5d87456ebd7632f93fb5d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate|9503797f73ba221918b99a5e6110154b4a2be19ee360d0f5d80935dfda5b766d|1|b0baa8c4a8438d18729d8427b18e3d858484938a9e72e731b91c5d5a337d75b7|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseAckCertificate"],
        rationale: "a01:1404: `RemoteRetentionReleaseAckCertificate {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionReleaseAckCertificate` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3e6f76af12b99912355abdcc8a766637447f32aa917a064eb14bfce535122caa",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec|f9712652dd2d96b3250a5c9644bf4aa5b25ed72780f701f815424862821ac4b6|1|c37f4c468412d1813cc72ba13fcfb6d46205da890cc15413897bb9608a10462e|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseApplySpec"],
        rationale: "a01:1404: `RemoteRetentionReleaseApplySpec {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionReleaseApplySpec` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:64ced526a660b98827cbc3ef997b177b68c4a84a15985fafd6640a432f68a5d3",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate|aa7c90ebdbe86775bf9fc1ec490b5877728d8d5bb78ae63606e924be64a24919|1|abe5a713343b96a497b548dd4d0d27df433230303dbb43716e0a9fd9635c83fa|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestCertificate"],
        rationale: "a01:1404: `RemoteRetentionReleaseRequestCertificate {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionReleaseRequestCertificate` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7a55234f5fdc43b6974252edcd0de7eba52e436ac217f10f8401ef6885ae9941",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionReleaseRequestRecord|RemoteRetentionReleaseRequestRecord|61530d8643d85319f1cb388802fdc52e9129c256a3d30de91eac7beec88df354|1|30b24af58989d99905dba44a649accae29ddac62d70449c69bd0eae64d493f37|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestRecord"],
        rationale: "a01:1404: `RemoteRetentionReleaseRequestRecord {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionReleaseRequestRecord` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:18d5436cd38b00236a2ca12e02bdeac86602803425abe2d1fa455996f2ad7f59",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec|7e7fc554f76b1ab633f3ce5abd6f1f383582a4ae95c842bcaaab9ba6f6d358da|1|2fe7a232f60901b8e08d8d9b67dd68e1702d4bb7b7754649245b487de98b2838|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestSpec"],
        rationale: "a01:1404: `RemoteRetentionReleaseRequestSpec {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionReleaseRequestSpec` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e120bc8a45d1cffd3c567b730d1c1c94e3efc66dfdec7c758fc8a7a7ab7bd8af",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|RemoteRetentionReleaseTombstone|RemoteRetentionReleaseTombstone|b3a56e67486b815efd605b198f74ee4c385529f4bb2cab6eb7bff97f03e2fe8d|1|2d9fe3cc679b241d3b6a8180364a187e7f4de76f77eccc5ec43b1a078e08b86c|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseTombstone"],
        rationale: "a01:1404: `RemoteRetentionReleaseTombstone {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|RemoteRetentionReleaseTombstone` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:159ccf72cd3fb33feaaa8a683be064682e50c25785f7cbe598da6b0be0087f92",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|StrongCiphertextRef|StrongCiphertextRef<T>|82193dbf61cc4670f7c7e102979b2f092d28d05fd794a407e80c816d419a38c0|1|654fd967a2ec23010eae0f5a93a68ce28a850893e2fb54070dad57922d656a97|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|StrongCiphertextRef<T>"],
        rationale: "a01:1410: `StrongCiphertextRef<T> {...}` is the slice's definition of the separate retaining physical edge; the brace body is the top candidate's normative rendering, prose-embedded without a heading.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b88b270c8e81a91838a8ad22b084d5f62869bfc4017064ffb7017275d923a751",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|StrongGlobalCommandRef|StrongGlobalCommandRef|ab717bb56140dfc383b871d4b743b3c16b70455638346b2124482ad78b4b3399|1|73185a853b4f6b2c1b0f618018b807b434a60601f41bc6abcd2f8ab532c20494|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|StrongGlobalCommandRef"],
        rationale: "a01:1406: `StrongGlobalCommandRef {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|StrongGlobalCommandRef` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f2ff70af4b775f5145f4f900f742808ffade01b29c77ed78fafb5b4338eb7c37",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ExportLeaf|ExportLeaf<T>.export_projection_version|f623dca97e62855892441afc7df4e85ebcd3de07982e27f7067c0da193b1a433|1|7b8897ff7a937be87f571e969cc4e57a33b88b343f6f59b55f7c6e41d7ef92f7|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.export_projection_version|export_projection_version",
        ],
        rationale: "a01:1400: shorthand member `export_projection_version` carries no inline exact type, and ExportLeaf is a logical kind, so no wire envelope commits the span. The a01 owner ruling landed in 694d14b fixes it to `u16` on the registered durable_fields row, derived from the a01 format-version family: format_major, format_minor and incarnation_continuity_profile_id are all u16/2. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f415e1d2a5f705c55cb0e824abed15b2718e4379274f36ef4173e7fbcdc07b56",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|StrongShardCommandRef|StrongShardCommandRef|adb6a69116e2579e5bb7b4a468c1a52798cc7eb85de04cf95d693c18144b31f5|1|e0e4b624813a9836962d2891bf1e4c7337957238f458e8f9e4df216bde1796a3|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|StrongShardCommandRef"],
        rationale: "a01:1406: `StrongShardCommandRef {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|StrongShardCommandRef` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c7ce81e2e7f285a53c0c12aead439bc99a21ae05d08f9e8cdae9acf3a09e857d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|WeakGlobalCommandIdentity|WeakGlobalCommandIdentity|46370ba4d0accf8846aed85909c57493f0d3268760ef88b4afb7f98dae7a6875|1|295e8aa5e1ba5fe360e1b1886b14c21d150e04cde511fbd5fd5de83343688325|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|WeakGlobalCommandIdentity"],
        rationale: "a01:1406: `WeakGlobalCommandIdentity {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|WeakGlobalCommandIdentity` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:1268173b9b0e90db9b8c6ff9e5fecbccc41c62c0f6eca9a75bcc274bc78c89df",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|WeakShardCommandIdentity|WeakShardCommandIdentity|aaefc54798aaa1b017c44e9478b4bd3e4d6028ecc01c71f9638f28bd5fa64da4|1|840cddfe963bbc285fbf55f8377752676d0d6c5d871b97658605712304a77283|leading named record has no explicit top-level ownership cue",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|WeakShardCommandIdentity"],
        rationale: "a01:1406: `WeakShardCommandIdentity {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|WeakShardCommandIdentity` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c3451eba691ae2bb32b935e0e2f4f563b7ab458f675c0418e8af0b3a7a86b418",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|conflicting-candidate-evidence|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef|d4a3bb436c751796fe3fb6c545137f95f7652b8c0d31ab5f6fcf505851d80db9|1|87eb43319218196e5ebfb267fd6508096f1a4a7c506cff1673f136d4b93854a2|the same schema source key has divergent structural bodies",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalGlobalTxnInputRef"],
        rationale: "a01:1406: the flagged name-span is the W12 history-wrapper sentence's normative rendering of `ConditionalGlobalTxnInputRef{command_oid,assigned_global_logical_command_seq,axis=GlobalLogical}`; it is the top candidate itself, and the divergent body elsewhere (plan line 1962) is a restatement the catalog's structural rows must reconcile, not a different schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:83653995aca02322485f58cf8cc3a4937305ef9f84d50c6659fc2cd9004e136e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|conflicting-candidate-evidence|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId|70c0603c29e5e1356b75d95097c8c6169aa4caf40488b24a7890938f164cf6a1|1|7b328a6974a5d4010e974e3c5ed04ed52d1942ce154471d6544eead1534710b8|the same schema source key has divergent structural bodies",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale: "a01:1443: the flagged name-span is the specialized `PlacementDescriptorWithoutId {...}` that RootSlot/RootBootstrap fields reproduce byte-for-byte; it is the top candidate itself, and the divergent body elsewhere (plan line 1449) is for the catalog's structural rows to reconcile, not a separate schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:892b85a96dad0e9766ca9fbef78fc37c5df29d469861d7b1bc9d6b1f7c567182",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|CanonicalScalarProfile||9e68621b328e90a3c62c448f0ac3fb6d570b290d8e77d707a9d5229961305985|1|50a8b8f79acb19697da1f4247e24fff423714fd5bdccccb34455eb9f0c8e52e7|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a01:1392"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|CanonicalScalarProfile"],
        rationale: "a01:1392: prose states `CanonicalScalarProfile` defines float/decimal/string/time bytes; it names a profile-registry concept and supplies no adjacent structural body, so the mention is definitional prose, not a durable schema rendering.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:608b425da6fb9c8cda3d49a78aae9a3e8c02fc48b30e613e1b2c417b202ae14c",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|None||dc937b59892604f5a86ac96936cd7ff09e25f18ae6b758e8014a24c7fa039e91|1|34a69cd2df6dbb2fad842df2213bde5d8cde2f78b5164d3cef45e8c1506b6710|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a01:1390", "a21:2649"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|None"],
        rationale: "a01:1390: `None` names the absence state ('legal only outside a role transition') reached via the payload's strong field; legality prose about absence, not a standalone durable schema. Duplicate mention at plan line 2649 carries no separate body.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:5c7c00068e6786930898a4cd7ca0936d1be398fa90428449bc501db193221292",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|PayloadAvailabilityCertificateRef||41a4a35c700e7b646ca05717d32a19a6d5d3344589a4e4f359ff850097b0bf24|1|bfb70c0a25e19f1744aaf7f55673ae5d4f9c9f7e132b57588104160371e93bb8|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a01:1410"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PayloadAvailabilityCertificateRef<T>"],
        rationale: "a01:1410: `PayloadAvailabilityCertificateRef<T>` is named as a generated exact union whose arms are generator-owned per ciphertext class and role; the prose supplies no structural body here, so the mention itself is not a durable schema rendering.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:05d7e3bb322be80fda931743566a01b05d3b38cf82f7b0d5c40fd940d655af1c",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|RemoteConfigurationRef||8a877c85a0180443d74fd85afcf7e2c5acbf1302eaf95d0c9680c218ffbe6d41|1|0a078aeeb4201c5125f6e26b1e10e060d0e6b0afd994b55fc1ec4985032e546c|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a01:1398"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|RemoteConfigurationRef"],
        rationale: "a01:1398: `RemoteConfigurationRef` 'means a consumer-local StrongRef<RemoteAuthorityConfigurationEvidence>' — a prose naming alias for an existing reference type, not a distinct durable schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:36c38b8690c34ce658b11fd0ddde6ac14aa37b8c84284910f9de561091d317e3",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|RemoteGrantTargetRef||406be18c12e881c605056f4a9f85648955bdcd6082efb78523d69b963e5af073|1|4439a561ba8a7ba891553f732c92cb874f8d697b266a050475dd2861fc7a423a|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a01:1402", "a04:1578"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|RemoteGrantTargetRef"],
        rationale: "a01:1402: `RemoteGrantTargetRef` is named as the containing-schema-generated closed union with one typed strong-reference arm per exportable target kind and no generic arm; generator-owned with no structural body in this rendering, so the mention is definitional prose, not a durable schema. Duplicate mention at plan line 1578 carries no separate body.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bf3f4910c7babb04019eba3e8a9d5ff90e67cf04fb39ccb54ac7192b1d4ff437",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.adoption_log_prefix_digest|3f6d1ca92a6b5d63424fa952e288dd1682e1120c21cb7308e4da28cd12a9f801|1|94346efcd01e685e0b97191bd948a7030240485414b08046ab67b6c40d716cf9|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.adoption_log_prefix_digest|adoption_log_prefix_digest",
        ],
        rationale: "a01:1398: `adoption_log_prefix_digest` is a digest-commitment field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9ef85f201456d54979f092bb31b1777aaaf90d831e425af9b6701f465bb99d80",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.canonical_configuration_bytes|fc68df4a8100b5e2b3b389194ea8ade0b962901978b0ec30dd7f7665d486a622|1|9a2b0f21be3b76f8d76bf6633a711e6d17f8e04c379e0ac44c67920f4a2ad276|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.canonical_configuration_bytes|canonical_configuration_bytes",
        ],
        rationale: "a01:1398: `canonical_configuration_bytes` is a shorthand-typed field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:44d2f6bcfdaa7e6ac3780a200d27f10a33a0b638fb0615f3c96f5e98d64c6592",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_adoption_raft_index|a6200217fa40abef3d7b65e7e4f187b24e3bab46d99adab5e8ebf760f742514c|1|ac36053b2b2fa43a5e22eaf635396bf8a31bf1f63d6b6b2ef39553c7b9262ccc|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_adoption_raft_index|configuration_adoption_raft_index",
        ],
        rationale: "a01:1398: `configuration_adoption_raft_index` is an ordering-sequence scalar field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:af7e299a09a52c513493942368959abef28db73ce3341a288576b8eb4b53c0f4",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_canonical_digest|ffc6a08972e5e120808c77c98e1682e03807c4a921aeeb61b247ced1bf467bf9|1|ab9b1b2d93c3e647152e98bced47888245368afc05d56612da99313881a7b2d2|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_canonical_digest|configuration_canonical_digest",
        ],
        rationale: "a01:1398: `configuration_canonical_digest` is a digest-commitment field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:86202e40010f1afc8012891816b8808b7e6c8ce542ab9892b8cf0b01af0dd23d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_oid|b4ba7337b02912dbdb1d2556f0f336781bb08d7b8de119e9e03387741a14bf29|1|2ac62ed0f8d3d87ab310c488ed5a9642a786d3aef57e1db582ce1b3d2fbbe12c|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_oid|configuration_oid",
        ],
        rationale: "a01:1398: `configuration_oid` is an object-identifier field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bf26b9e5234ca109bef539c1a4c98e58925e1c2e3dd5e123f0b75fc633ef523e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_quorum_signatures|5e039f2728144d7c6c0dbf60909319f1960786dbc0bd4c354b18466938513b83|1|05d20d4e0a20f87e623723a40753b4bd4d77d452a91c0b685e5841f2fcea7e91|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_quorum_signatures|configuration_quorum_signatures",
        ],
        rationale: "a01:1398: `configuration_quorum_signatures` is a canonically-sorted signature-set field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0cbbda898da10fa9b89f900be61c7b9aed7bbd0934366e1e71f3abdde07956a0",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.member_verification_key_set|9228d0f8c89f45c97b207618a9296c8f8b7951c5168aefb3c15dd36a56e91f46|1|7b19c194224119cc46717130957596f006a55ca994850922b1ccf7b15dce5a7b|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.member_verification_key_set|member_verification_key_set",
        ],
        rationale: "a01:1398: `member_verification_key_set` is a named closed sub-schema field (compact-phrase law) rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ee8990906ae0c1ecb94acbbe2f5723f319918c316d350f7108484833b54ba629",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.minimum_configuration_retention_floor|e56db1355acc784b116b7aabb64186db0bd720d85cddc92c8bfeb17afdfeb57b|1|02dfe7b844480a841425e8136513334e28c5c12f33cb808dcedea988f1900129|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.minimum_configuration_retention_floor|minimum_configuration_retention_floor",
        ],
        rationale: "a01:1398: `minimum_configuration_retention_floor` is a retention/checkpoint floor field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e9dc734b30ce92280487bf83e234b3face8e3ea47b93f841bf472a2ef76643a2",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.payload_predicate_digest|85b5b766cbd934ab08443a080c8789e6312c4e26e9fb42a97186c25fa3f46956|1|0e6034f8e22f941c5a067a2e7bd992b96d0a89ffb57a8e14b512d1cbbe4e4728|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.payload_predicate_digest|payload_predicate_digest",
        ],
        rationale: "a01:1398: `payload_predicate_digest` is a digest-commitment field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:785c16b82f46561a50e849315dd7e84c669b4f3b703746b32b4963ed6625b54e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.signer_epoch|8a955431bf60ecd9ee861704046648542da9fb2d078520d940f63ef1398b4765|1|991d203f9334c99be8acd5d8d4b19dc6153578c08d25dab0b1aa4afcb7624c52|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.signer_epoch|signer_epoch",
        ],
        rationale: "a01:1398: `signer_epoch` is an epoch scalar field rendered shorthand inside the `RemoteAuthorityConfigurationEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:89368ae55192e51984ac23f81f0afa52c478e1ce606f91c776060f4c8a595396",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.authority_quorum_signatures|56de356f4b1371c3c545ba560a7111f346594c418d015419dcb8fef7601d9a4d|1|facd33fc809fe34a79465328721f5f5274f0ffe9f76debdcd65d455d027cca40|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.authority_quorum_signatures|authority_quorum_signatures",
        ],
        rationale: "a01:1400: `authority_quorum_signatures` is a canonically-sorted signature-set field rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:550459906367220aa9ba71ed7b8aab0f60f321bb09c75879e1d3deec8fb0f15d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.authority_retention_floor|faec9ce81314483ac776624bbef8b05e7ba973f3d5508e9a7f3f44608c236ae4|1|fba644be42a7b8337e44cbbef925fe49c41db010b749500a11365d7e00e76e43|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.authority_retention_floor|authority_retention_floor",
        ],
        rationale: "a01:1400: `authority_retention_floor` is a retention/checkpoint floor field rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0780dfaa7be7e082803da9d4d1d980f09c6ab855346e8a7f8638b7c5daea7ea9",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ExportLeaf|ExportLeaf<T>.target_closure_inventory_digest|e9f4ca6621f4e36a6c7058504702c6ef874721b67a2a9208b46097af61cc285a|1|2cc6119c3cc09cc413de2cba219108faa619dd605a0615a204f7dae1985d474d|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale: "a01:1400: shorthand member `target_closure_inventory_digest` carries no inline exact type, and ExportLeaf is a logical kind, so no wire envelope commits the span. The a01 owner ruling landed in 694d14b fixes it to `digest256` on the registered durable_fields row, derived from three landed instances, unanimous at digest256/32 with digest_class=target. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:088c4ff91992149430ae731d5ff92818988720134060669d7f25967c8e35e59f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.encoding_placement_coverage|9b55bcdb27d3c8b9d1c7e27801d59e7f86a64126eca571ae6a0826c753569db5|1|7724f2742410a6affc66d6564aeea475aacec16dce6b21091f2e26d5075016a2|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.encoding_placement_coverage|encoding_placement_coverage",
        ],
        rationale: "a01:1400: `encoding_placement_coverage` is a named closed sub-schema field (compact-phrase law) rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:330ec263173ef0a9576a1913c6ba0487bf4138413b87bd18ecf4f7ed4b08fb49",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.failure_domains|3f3d759ddf7f238e5931f429d410022e227f5d84c9522fae6abf88088d6f1852|1|710e3a0d8f16d56e1e217dc10c0cf628484878501f43c7fb7cb5050871b36ac3|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.failure_domains|failure_domains",
        ],
        rationale: "a01:1400: `failure_domains` is a named closed sub-schema field (compact-phrase law) rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a3d03272c5e918952e4ac5c7fa89e97a8e29c5540a63623ff0ea776fea86e0ee",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.payload_predicate_digest|85b5b766cbd934ab08443a080c8789e6312c4e26e9fb42a97186c25fa3f46956|1|ec097a492f696a2d7af40e999272a7c0d2626b3120e7c4c9ece038872b205deb|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.payload_predicate_digest|payload_predicate_digest",
        ],
        rationale: "a01:1400: `payload_predicate_digest` is a digest-commitment field rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:8891ee1b2dce0bcac481ecf3bb37e10b1194281a42f9f16bc24838fb04f87454",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.receipt_set_commitment|7a15be9ed109d3d4aec9d9a0345adfbfdfaf033eb7be0343f30980205ed815a6|1|86c1e22cf5e3cf16e7391afe6a3e393983e0ef029b466a8aab3537bf1b9b7359|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.receipt_set_commitment|receipt_set_commitment",
        ],
        rationale: "a01:1400: `receipt_set_commitment` is a named closed sub-schema field (compact-phrase law) rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:dadff94a138b7c32a653efd353155f880adab9d0a9d5bd1442b5faed58225b0c",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.signer_epoch|8a955431bf60ecd9ee861704046648542da9fb2d078520d940f63ef1398b4765|1|1a3f2922f3d2e330022c4d733c5f77156fadc78450705f768ad4cd8ba4352b8f|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.signer_epoch|signer_epoch",
        ],
        rationale: "a01:1400: `signer_epoch` is an epoch scalar field rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0b2170ebc0cdcae1b0a8fc5ae73c50e6539cbe87246be6773e4eca53e8c24b7f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.target_closure_inventory_digest|e9f4ca6621f4e36a6c7058504702c6ef874721b67a2a9208b46097af61cc285a|1|52a91b1e32cbe3eaa440b93807aa03cd66e334e58e0b237d1378c2c6ef61ea90|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale: "a01:1400: `target_closure_inventory_digest` is a digest-commitment field rendered shorthand inside the `RemotePayloadAvailabilityEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:aba1b29d59bfb1146158d7c01d9f17f701b1a2f9aceaca04c3e15583a52481b3",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.ack_digest|26a0c8c280895f2a8a5fa2133a0c5da5e937e9ad91457a2cd1967d9b5dfec1e1|1|314c4125d4749187aa267f0fa56aedb9e5c05c635cd6f797a7ad3e52df5965be|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.ack_digest|ack_digest",
        ],
        rationale: "a01:1404: `ack_digest` is a digest-commitment field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ba383604b0f04fa552de5ca7b52083a58cec2bb816d9d50668b4cee84b8cb40e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ExportLeaf|ExportLeaf<T>.object_specific_scalar_projection|db78957fc4e87eec0adcece6ae1bd57370e57206c74050f76c2ca8a3318c53d6|1|fc5ab2a4bb1ddee224c7ccc0bb65bc663b5f8f50dbc769f6266e22d2acb2149b|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.object_specific_scalar_projection|object_specific_scalar_projection",
        ],
        rationale: "a01:1400: shorthand member `object_specific_scalar_projection` carries no inline exact type, and ExportLeaf is a logical kind, so no wire envelope commits the span. The a01 owner ruling landed in 694d14b fixes it to `bytes` on the registered durable_fields row, derived from the a01 canonical_configuration_bytes precedent; the layout is fixed per expansion of T, so the bound is the declared per-kind ceiling rather than a width. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ba4b4e426fd4324114eaaad337e442b5ce7d6e038a34db56d1418c705e15954e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.ack_leaf_identity|61de905f241e3ee171751c85c755cd05a255c541afcd5327ccf6b4e6e41af001|1|5aab29685cf0b0434adec2c3d5a9ab98ed8ca7c939b8a125daa9a590ee70862f|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.ack_leaf_identity|ack_leaf_identity",
        ],
        rationale: "a01:1404: `ack_leaf_identity` is a shorthand-typed field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:23f9c08803b086cdbfc8c97b6f9659e8bb5c6e6a8c9cf04032f5f5d28e079408",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.authority_domain|68b123a21ae2a7c14c4ddc2e38626f5942b3f6ce93eeb69ab412c22694303766|1|ee4f77e22d96f2d4992820aff35712b464ef1264b4cb01676101707349f28530|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.authority_domain|authority_domain",
        ],
        rationale: "a01:1404: `authority_domain` is a shorthand-typed field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7d1a0cc415b4e6a6170783944fabb87802c0c72103c47fae6f2c414de670e118",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.authority_order_index|59013d383aab857a8fbabee16001fe972a1c5e4a6070b69079c2d83bf820ae1a|1|9ef1c20c7848d3d91e50f2d1f6aa39b7dd0a37ad3c68dec2f363931f63d362da|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.authority_order_index|authority_order_index",
        ],
        rationale: "a01:1404: `authority_order_index` is an ordering-sequence scalar field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3ed46e16d278ca8758eb8f03cda81ec00cda011ecabf571c6efd9b1ced1f858d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.consumer_domain|df7455f547e1c8d13dd7f4a1bd780c9d151f3e8c1a2ce5a6b10eccc6c0fd75c3|1|c0b8c50d40d15e1926c2673e57b341268c3fe4e46e26c57da8167c4982b888cd|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.consumer_domain|consumer_domain",
        ],
        rationale: "a01:1404: `consumer_domain` is a shorthand-typed field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0a140168b441efaf4eba40cc6f5f32b10863296cf590c9a62de21e0694f64a5f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.grant_id|27f6bd90fe7bdb302495d31830da1ce66c2fc2efdcae08a90cf59ccd517db115|1|a12f2caed8e900d0337e05eb274562db23bfc99ee8b889511b5bc9577b581ca3|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.grant_id|grant_id",
        ],
        rationale: "a01:1404: `grant_id` is an identifier field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:63373197a5998d375086fa33282c053b58e6273df62146964f86434530696359",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.permanent_release_proof_floor|4b71e9856404eedf9a7a222a9e6a34dc571cc71ef32c0e5d931842a06cde1246|1|b577bacbed11011141cd570bbdaa6b2e79943299d607aabe963fc0be7fdb2fba|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.permanent_release_proof_floor|permanent_release_proof_floor",
        ],
        rationale: "a01:1404: `permanent_release_proof_floor` is a retention/checkpoint floor field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9b5ce7da11e6c3031a9e2ff2d7f3f2c8868508ee8ee79cfe77c289a072e22a74",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.published_at_order_index|365c63a55f77f8b3dc2c90d4a1bbd93ac30870fd79a64e22a618b70ff8a1fb9d|1|355972555c580152bbddc34944d743c3beb62eca3788931b9240d02bab0ac86d|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.published_at_order_index|published_at_order_index",
        ],
        rationale: "a01:1404: `published_at_order_index` is an ordering-sequence scalar field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d010489d0119524a9d50b49a62f4f9944d175fac9a6ee4ad8de8128784e34969",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.release_nonce|15caa9e1be8b93b984ccbf175108151d96f095d573084525d7a4d1deacc79b06|1|2789b41382235330a3fdd089594badd72d22c4c2435b7c6d465e6be7f4ed818d|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.release_nonce|release_nonce",
        ],
        rationale: "a01:1404: `release_nonce` is a nonce scalar field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a8344545b02b95a838fadd3e5bbb725c3d60093f1ac88f08a2cc9acfcecff955",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.target_identity|ac43e82a633b092a515dd15ce3f767c9ab4cfb65bbb0d9ee4866264a2362c2ef|1|3ebb5e7b4bd24f0ec7ab3f29221174b1a9c1d942adbf73be389ffd43b78b63e2|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.target_identity|target_identity",
        ],
        rationale: "a01:1404: `target_identity` is a shorthand-typed field rendered shorthand inside the `RemoteReleaseSummaryEntry` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:104955772015586008e43b5d3d99bd835f456ec5c11f29fce79c03c941ad0be3",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionAckPublishRecord|RemoteRetentionAckPublishRecord.summary_key|358a0abf4235506c63e1eae650b8a4a632095ee98e715c6e52613256d52fffed|1|0c33abc75e8f64d8f44a3d25a9a6535c5618056ddabc557b3d4bbd3d4a516f32|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionAckPublishRecord|RemoteRetentionAckPublishRecord.summary_key|summary_key",
        ],
        rationale: "a01:1404: `summary_key` is a shorthand-typed field rendered shorthand inside the `RemoteRetentionAckPublishRecord` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c63e45723d33f104675de8ed3e9a8417545aa6209c6ef981a9b04c56fcca5bd0",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionConsumeAckRecord|RemoteRetentionConsumeAckRecord.summary_key|358a0abf4235506c63e1eae650b8a4a632095ee98e715c6e52613256d52fffed|1|853b8fbba76f7afb9f08b567f44dbb34e73a91f65c11da08be3360b5fb00820d|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionConsumeAckRecord|RemoteRetentionConsumeAckRecord.summary_key|summary_key",
        ],
        rationale: "a01:1404: `summary_key` is a shorthand-typed field rendered shorthand inside the `RemoteRetentionConsumeAckRecord` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3228dd0b5dd8875265298f3a724ef85adbebeb35fcbb5d05df62e87b91c40f82",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.authority_order_index|59013d383aab857a8fbabee16001fe972a1c5e4a6070b69079c2d83bf820ae1a|1|36fbee5324bd9ffa91aa689fbfa2387b5427f339ab0e06a385ea01cda0b0d871|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.authority_order_index|authority_order_index",
        ],
        rationale: "a01:1402: `authority_order_index` is an ordering-sequence scalar field rendered shorthand inside the `RemoteRetentionGrantEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f18419d17e8a08e7609f35ebbc6f4c09735a946a02c5e7512a3ab0406f72f8cd",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.authority_quorum_signatures|56de356f4b1371c3c545ba560a7111f346594c418d015419dcb8fef7601d9a4d|1|d49df532d66c735518769ecb06a268b3b764730d73666a333095acdc359b81ae|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.authority_quorum_signatures|authority_quorum_signatures",
        ],
        rationale: "a01:1402: `authority_quorum_signatures` is a canonically-sorted signature-set field rendered shorthand inside the `RemoteRetentionGrantEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:fbd189543ad2fee10893b87f6f45d238a17c00595c70c1b415e5ab6dfd125b9a",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.grant_id|27f6bd90fe7bdb302495d31830da1ce66c2fc2efdcae08a90cf59ccd517db115|1|9f2ac593ad8126a942d92717bcd800ec86c12bb4742342ea8cde22b2be346636|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.grant_id|grant_id",
        ],
        rationale: "a01:1402: `grant_id` is an identifier field rendered shorthand inside the `RemoteRetentionGrantEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d6336ec6c39141df42c4ed61b1cac308f22a269f73b8ff4aa55106247f19ce93",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.grant_nonce|4c576db69271ac2c50a56e9f678811f37464b495c81cce34016ed46c0ac6ad63|1|a8757c44786e19f6a52b62fe9093a6722d24b5a5a546b918b82d827ef8bbb83e|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.grant_nonce|grant_nonce",
        ],
        rationale: "a01:1402: `grant_nonce` is a nonce scalar field rendered shorthand inside the `RemoteRetentionGrantEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ee01aff30d2078379b503b7895ae8be464a00a752466263025dfbd0a45fdb667",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.minimum_authority_checkpoint_floor|20c3f12d6501ab5be2cb7969a9465f23511a3ea53238521aca406b02971096e2|1|2f9cb650301fb59f33481e2193836b7dc3579c77dae3920136827a04e9908271|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.minimum_authority_checkpoint_floor|minimum_authority_checkpoint_floor",
        ],
        rationale: "a01:1402: `minimum_authority_checkpoint_floor` is a retention/checkpoint floor field rendered shorthand inside the `RemoteRetentionGrantEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a816a6e2f7d5f4db12015423d9bea5c670a3e072257be3f0931e3ed61d49bee7",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.signer_epoch|8a955431bf60ecd9ee861704046648542da9fb2d078520d940f63ef1398b4765|1|98a6867f372c0af40b38517ad2f64f3ca89fa28006533aff724a6fd48dade551|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.signer_epoch|signer_epoch",
        ],
        rationale: "a01:1402: `signer_epoch` is an epoch scalar field rendered shorthand inside the `RemoteRetentionGrantEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:86987e71c410029e676e72048d9e14928105ac680c68d7b8dd9b3fa3a1e5c49d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.target_closure_inventory_digest|e9f4ca6621f4e36a6c7058504702c6ef874721b67a2a9208b46097af61cc285a|1|5b1625801033ae5993e0926054bc1a5a5f4bf62fabeaadfcc4f018ab2eef99d5|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale: "a01:1402: `target_closure_inventory_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionGrantEvidence` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0ea0c37a2094412a8669dca8c447980a4970f76b3485a3a4cdedc96d530f9740",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.grant_id|27f6bd90fe7bdb302495d31830da1ce66c2fc2efdcae08a90cf59ccd517db115|1|beee8fbb534b5334ebf765f3f348b249413add216fa7875a903745cad81f9e96|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.grant_id|grant_id",
        ],
        rationale: "a01:1402: `grant_id` is an identifier field rendered shorthand inside the `RemoteRetentionGrantSpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d470f3413f5c4faa0a2bf88552faa881c88f926d15a1d7bbbe4ce71f53817a5d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.grant_nonce|4c576db69271ac2c50a56e9f678811f37464b495c81cce34016ed46c0ac6ad63|1|15213f89b32da1621b9d5e6325729cf09889ea8d5af5c258a7b320a100882c98|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.grant_nonce|grant_nonce",
        ],
        rationale: "a01:1402: `grant_nonce` is a nonce scalar field rendered shorthand inside the `RemoteRetentionGrantSpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e14ae8c12903b4309edf9249c9fe2bf44de6671a7cce5cd73ae3e05ff8478495",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.minimum_authority_checkpoint_floor|20c3f12d6501ab5be2cb7969a9465f23511a3ea53238521aca406b02971096e2|1|33cde523f3fd93cb4e0b61f96b67f43ed8dfa53715acac0f5dc6daa98a36027e|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.minimum_authority_checkpoint_floor|minimum_authority_checkpoint_floor",
        ],
        rationale: "a01:1402: `minimum_authority_checkpoint_floor` is a retention/checkpoint floor field rendered shorthand inside the `RemoteRetentionGrantSpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ec608cc085dc6c92eb129bd6aaeaac5f75c75069834e27f23ce68b9733e6f445",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.target_closure_inventory_digest|e9f4ca6621f4e36a6c7058504702c6ef874721b67a2a9208b46097af61cc285a|1|6527dda0f73fba508c31022d5d9f2351c0441e8470d3f7e59efacc00805b64fe|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale: "a01:1402: `target_closure_inventory_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionGrantSpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:38e1b1a453a2d78b3cd9b61fb722eb5dbee4e3ef16190d31191f4024a26a3d9e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.authority_order_index|59013d383aab857a8fbabee16001fe972a1c5e4a6070b69079c2d83bf820ae1a|1|174469a7843c6453c2d41b92c182a89b868b5423e400a1e81ba8b77a29588e97|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.authority_order_index|authority_order_index",
        ],
        rationale: "a01:1404: `authority_order_index` is an ordering-sequence scalar field rendered shorthand inside the `RemoteRetentionReleaseAckCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:41ca748dc55b6431eaf3918bbe1b9a9734df2d1ca956d7e6f852cfb95efe5197",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.authority_state_root_digest|78e75b8b883d40522f329ac6895502ec7ca2cd9141bb0cd4b4cb303141ba6f1d|1|a0ea0bf11753016694ce3fa33b4d7c0e9ff753a6594017aa5b1234f71f6d36ce|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.authority_state_root_digest|authority_state_root_digest",
        ],
        rationale: "a01:1404: `authority_state_root_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionReleaseAckCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7d5b5b36c4658a32b106702e723141f57156946818eca7b8170d5acbe23674ee",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.grant_id|27f6bd90fe7bdb302495d31830da1ce66c2fc2efdcae08a90cf59ccd517db115|1|0b6b5eb92810cd175fd678a707e39f34055f605b3e9156ad51461ea9baedf219|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.grant_id|grant_id",
        ],
        rationale: "a01:1404: `grant_id` is an identifier field rendered shorthand inside the `RemoteRetentionReleaseAckCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ab27fb38ceb289b9a170a10a6a37c35180aea44221f333402b340661c320a043",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.quorum_signatures|4b7382d93588313ac60e777d7671792202dc4445f17d96d2882ed00971b64a35|1|412be3befdcb424ab5bc6bcef67962b647c8644365372e9cfb45c93c1f9b6615|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.quorum_signatures|quorum_signatures",
        ],
        rationale: "a01:1404: `quorum_signatures` is a canonically-sorted signature-set field rendered shorthand inside the `RemoteRetentionReleaseAckCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4bff13c8b469f1d738a8680dbae3d6c5f816043ad51d34dd0bd69416985ff533",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.release_nonce|15caa9e1be8b93b984ccbf175108151d96f095d573084525d7a4d1deacc79b06|1|eba2bea3631e67ba3fbb167d8d27171e26461363879e337724545743a403e777|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.release_nonce|release_nonce",
        ],
        rationale: "a01:1404: `release_nonce` is a nonce scalar field rendered shorthand inside the `RemoteRetentionReleaseAckCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:2d325be7f8f0ffbecc4dcd60c205c8760a79e9e0f90ddbded5e1c491881921b5",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.expected_active_grant_digest|932d8a5a94aa8d19454cefb284ce6ee239b930fd0bf76be1add154de91aa54ff|1|0d61982ca6bd952a2c69dda70e942399093b34267b7c325788017f634c8d7636|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.expected_active_grant_digest|expected_active_grant_digest",
        ],
        rationale: "a01:1404: `expected_active_grant_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionReleaseApplySpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:edc4e45136d059b93d4d936f23332275c3cb4bde7ea64a96fe190c8170a56355",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.successor_transfer_proof|1d3ca7a2e079efd5915d2cfddf2174e951cf49856f1bd7209bba195b2ccd117a|1|8d289995242dd40b765139849e92f169f2de0a1cbf4058c623cf66c8d22572a5|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.successor_transfer_proof|successor_transfer_proof",
        ],
        rationale: "a01:1404: `successor_transfer_proof` is a shorthand-typed field rendered shorthand inside the `RemoteRetentionReleaseApplySpec` body (the trailing '?' declares registry-checked optional cardinality); per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ad98d0bc880733e386ee6412e07437998406030d9bdb0efaff2e3793cb529ad4",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.verified_consumer_no_reference_floor|1067a6e6201ee71729b93d84143d7e4e80e91fcd831d1dd6718e14792741c458|1|0255e6bf55508186e7d79d094c363a62af350099d652082c0e70febdf44726bd|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.verified_consumer_no_reference_floor|verified_consumer_no_reference_floor",
        ],
        rationale: "a01:1404: `verified_consumer_no_reference_floor` is a retention/checkpoint floor field rendered shorthand inside the `RemoteRetentionReleaseApplySpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6f795daeb8ab9c6f9256b4c88ddb79c7fe051c84ffd60b6c0d97a9e9cf557467",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.complete_consumer_root_digest|d76cefdc4c84ad41fa28af991a009ff6b7c12a96fa2d7b7e94bf9afe426ce014|1|6589f24162651b31d5d30e3c6f40a245693595cf8ffd27e738acd76f03c04342|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.complete_consumer_root_digest|complete_consumer_root_digest",
        ],
        rationale: "a01:1404: `complete_consumer_root_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionReleaseRequestCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d0d66f4d6ea6017ab754904e8928b724aa730a3d5dc0354290c5e0f370981533",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.consumer_no_reference_floor_digest|7c7e6486114525898ec198a0f5957a9c584b2bb75acecf76bec19012a493401e|1|f34090437a8669307664a600f72358824566f0bdd4c29e70c61e5db88d6e5486|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.consumer_no_reference_floor_digest|consumer_no_reference_floor_digest",
        ],
        rationale: "a01:1404: `consumer_no_reference_floor_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionReleaseRequestCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b16fd118a0a64392b8ae28941eaaaf3b910196151732a38fe44b7fba9d08cc54",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.quorum_signatures|4b7382d93588313ac60e777d7671792202dc4445f17d96d2882ed00971b64a35|1|b3153c51c7cf9aa357701d7c669b7c1c394b427b4b93998c7a6b39b55a043bfb|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.quorum_signatures|quorum_signatures",
        ],
        rationale: "a01:1404: `quorum_signatures` is a canonically-sorted signature-set field rendered shorthand inside the `RemoteRetentionReleaseRequestCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a5377c0f69a2ceeaea82196dd3cfdfe3b5bc4106771c28224f4a6a90a1c46aae",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.release_nonce|15caa9e1be8b93b984ccbf175108151d96f095d573084525d7a4d1deacc79b06|1|30471b9242da4a6b8657376fd6077c75689154253fe08238ad96a002ec9d03b5|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.release_nonce|release_nonce",
        ],
        rationale: "a01:1404: `release_nonce` is a nonce scalar field rendered shorthand inside the `RemoteRetentionReleaseRequestCertificate` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f08db86581e0fafa4b2e38638a61d8ecda6371c9da72cdb671f1b34212da455f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.successor_grant_identity|e559ba44b5afd503205b24d3c679fdca23610df1575bc64fe7e5f5773118e1f6|1|118e8077069af1876246e3678b616327a8c4056454e3e815e4cacd18c8e23d9c|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.successor_grant_identity|successor_grant_identity",
        ],
        rationale: "a01:1404: `successor_grant_identity` is a shorthand-typed field rendered shorthand inside the `RemoteRetentionReleaseRequestCertificate` body (the trailing '?' declares registry-checked optional cardinality); per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4bbdb01f00f659a0e01412ae5d5cbaa2ddbc312ce44a5997149d1a2cd6d4ce0f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestRecord|RemoteRetentionReleaseRequestRecord.consumer_no_reference_floor_digest|7c7e6486114525898ec198a0f5957a9c584b2bb75acecf76bec19012a493401e|1|7160448bd81d9eb338dea42663f47cccfbef36cd2a711fcad20995f8c07d33e0|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestRecord|RemoteRetentionReleaseRequestRecord.consumer_no_reference_floor_digest|consumer_no_reference_floor_digest",
        ],
        rationale: "a01:1404: `consumer_no_reference_floor_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionReleaseRequestRecord` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:05940c088f9ba416398714a189357ee97c8e2c7e728a68eb8f4bb9291e8e7c13",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.complete_consumer_root_digest|d76cefdc4c84ad41fa28af991a009ff6b7c12a96fa2d7b7e94bf9afe426ce014|1|a782dab5228e38ea5b02deb69bb1adce6d918cb5f1589b90913b8e7533c20d09|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.complete_consumer_root_digest|complete_consumer_root_digest",
        ],
        rationale: "a01:1404: `complete_consumer_root_digest` is a digest-commitment field rendered shorthand inside the `RemoteRetentionReleaseRequestSpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cb107c3b25092a752db12aa072dceaae7e10ce08c8500f44d0e944ec974e7da6",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.consumer_checkpoint_floor|20d326c95db5860b56f58c0f4ad4bf8260cb19da3c43fc0a8845ebd041f5f7a1|1|b36a5d704aa591d2fb41f3f0e317a3bdbee04e0a24c1a63e7cee5e08a22c2027|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.consumer_checkpoint_floor|consumer_checkpoint_floor",
        ],
        rationale: "a01:1404: `consumer_checkpoint_floor` is a retention/checkpoint floor field rendered shorthand inside the `RemoteRetentionReleaseRequestSpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:08913cde0840a5415b20c38f4728fca9d06781a479e6bcaf770b368c5488df0f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.release_nonce|15caa9e1be8b93b984ccbf175108151d96f095d573084525d7a4d1deacc79b06|1|ee09324f4574faaf0e63d84a9fa02e14c5a72906341d42170d91c6ed77da7d83|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.release_nonce|release_nonce",
        ],
        rationale: "a01:1404: `release_nonce` is a nonce scalar field rendered shorthand inside the `RemoteRetentionReleaseRequestSpec` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ce182745bb770c96a671b0eba846d4d9a672cefa40a51f10cc88575445bd0e3c",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseTombstone|RemoteRetentionReleaseTombstone.authority_order_index|59013d383aab857a8fbabee16001fe972a1c5e4a6070b69079c2d83bf820ae1a|1|61db89f3a0d04a6578a4919aa5d7f08ffab70ded556386537fad94838625325c|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseTombstone|RemoteRetentionReleaseTombstone.authority_order_index|authority_order_index",
        ],
        rationale: "a01:1404: `authority_order_index` is an ordering-sequence scalar field rendered shorthand inside the `RemoteRetentionReleaseTombstone` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:259538a996b4f52d0906e85b5e35436eee1012e4ada44589405094738a8b2725",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteRetentionReleaseTombstone|RemoteRetentionReleaseTombstone.release_nonce|15caa9e1be8b93b984ccbf175108151d96f095d573084525d7a4d1deacc79b06|1|11caeb4e5116643b8fa8bf716ade43294f1b42ea169ff786462dd63f3c0c4556|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseTombstone|RemoteRetentionReleaseTombstone.release_nonce|release_nonce",
        ],
        rationale: "a01:1404: `release_nonce` is a nonce scalar field rendered shorthand inside the `RemoteRetentionReleaseTombstone` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0cea5eb3bef0bc9ab4c17b1671ce03661e717d8ec0742dffff4e0566a2255868",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustArtifact|RootAuthorityTrustArtifact.canonical_root_authority_signature_set|98656fd6440f1cb7c354f38d4ab363e0b9d6b6f4cc17fdb4ddf4343fd48ba65d|1|4b3c19c2935b07e62452c4a5aa64d334c807143614e5c8724c42b65897d07a14|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustArtifact|RootAuthorityTrustArtifact.canonical_root_authority_signature_set|canonical_root_authority_signature_set",
        ],
        rationale: "a01:1398: `canonical_root_authority_signature_set` is a canonically-sorted signature-set field rendered shorthand inside the `RootAuthorityTrustArtifact` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:47fbac16db79678402e6382624522139d902f97907bd56096457c3c53d502918",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustBody|RootAuthorityTrustBody.canonical_genesis_or_transition_bytes|2130574491455002d6300d91957a195dc794ab30602dfe7cc22fb1b59e86b92e|1|b30e4155c3a2c5987dee01958c2958939b1d14faf7b20330a952baca2418234d|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.canonical_genesis_or_transition_bytes|canonical_genesis_or_transition_bytes",
        ],
        rationale: "a01:1398: `canonical_genesis_or_transition_bytes` is a shorthand-typed field rendered shorthand inside the `RootAuthorityTrustBody` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b458a33eb43d02f3b156bc7d4539c5ebbb3740aa8e13dc2d369d5d203d0873ef",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustBody|RootAuthorityTrustBody.expected_root_verification_key_set_digest|efdea27d33fb35f504475a8016e2c08830d7d8821a91932d9af0f78ee8d3da97|1|d0d0ce8938ccb50960925f5e72244dc5900ea4c4fec934a1a3f5bb16d6e54f46|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.expected_root_verification_key_set_digest|expected_root_verification_key_set_digest",
        ],
        rationale: "a01:1398: `expected_root_verification_key_set_digest` is a digest-commitment field rendered shorthand inside the `RootAuthorityTrustBody` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:caddd2e243775866bb52f8da1e62fa89adabfb26042229cc138a2f2eb194b950",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustBody|RootAuthorityTrustBody.externally_pinned_root_policy_id|e8f6004ed65e68f7aaf9189d8bd6f45231419ffb9374d8243d1e7ca10cd01f85|1|dbe55110c3b038e62b341a3788242a1302eb8677491b832cf53186b4711a99fc|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.externally_pinned_root_policy_id|externally_pinned_root_policy_id",
        ],
        rationale: "a01:1398: `externally_pinned_root_policy_id` is an identifier field rendered shorthand inside the `RootAuthorityTrustBody` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3fce4db02d1cb690e1e7204de1499f3ed1982f8f8eaecd41e1629b4c5403375a",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustBody|RootAuthorityTrustBody.source_identity_or_transition_continuity_commitment|07351b0457a3192f79b692fd77f43abf48f011f2011cb671db6a99c0601078d7|1|8a2ea05d7f1acac68a37a0dd3e9383bcf03be5c8aea01cfcd9a1fc358c4f910b|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.source_identity_or_transition_continuity_commitment|source_identity_or_transition_continuity_commitment",
        ],
        rationale: "a01:1398: `source_identity_or_transition_continuity_commitment` is a named closed sub-schema field (compact-phrase law) rendered shorthand inside the `RootAuthorityTrustBody` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0cfced7abb4163ebdcf4ffead214a475ffd662cb43bf1bb54c9848ce3cd137e6",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustBody|RootAuthorityTrustBody.target_configuration_canonical_digest|9e05bab4fd4b345b34909a5ed5ddc34f57c92871ab6bc29359c797fa7b6ac9b0|1|832303e888adbe3a970adf13e41562404abdae3ae45cf0bb46b11760942b46db|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.target_configuration_canonical_digest|target_configuration_canonical_digest",
        ],
        rationale: "a01:1398: `target_configuration_canonical_digest` is a digest-commitment field rendered shorthand inside the `RootAuthorityTrustBody` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ef9a226efe47962957214937e4f1158545bb53682355abfc8ee4b464438e32e4",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustBody|RootAuthorityTrustBody.target_configuration_oid|c6b33eecff1094498dea48db10f759d2e16c17fd71abc5105caf65d69d692075|1|72691e6346a8ce2af6a541072f3cb42a777fd24bba45571fcbfa4882e9cf57c0|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.target_configuration_oid|target_configuration_oid",
        ],
        rationale: "a01:1398: `target_configuration_oid` is an object-identifier field rendered shorthand inside the `RootAuthorityTrustBody` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:396e4c7dcfc6962ef4e1b741b23543a382260c4385a4430104792dd47f60108a",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootAuthorityTrustBody|RootAuthorityTrustBody.threshold|497e22fe854a24bcfb8aa568e454fa262cdb64a109e01dabf5793b46326144da|1|af27d97f9068cd10a2fb16ad91626df62a57de1ec00c2f9b4a5187ad20f12392|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.threshold|threshold",
        ],
        rationale: "a01:1398: `threshold` is a shorthand-typed field rendered shorthand inside the `RootAuthorityTrustBody` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ad518b83fc93d2e002e29f0b04c6997a3f4f7db95c0332b3396c97abdddbabce",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RootSlot|RootSlot.reserved_zeroes|430d9b368a63615aab93e1e5a992a6a175e06817ef96d624b1f3e71a3e13dfd3|1|6c638c353afa417955bdd8749fccfd8d0540ad7c312c09e6c694089eb17537c1|shorthand field has no exact type",
        source_locations: &["a01:1425"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|RootSlot|RootSlot.reserved_zeroes|reserved_zeroes"],
        rationale: "a01:1425: `reserved_zeroes` is a shorthand-typed field rendered shorthand inside the `RootSlot` body; per the a01:1412 flattened-rendering law its exact type/cardinality is owned by the durable_fields.toml row, so the flagged token is the field candidate itself.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:09e59fe9e8d42990d61d08b6b8f2c7edb2526c89f0fdb20fae7745ef014a81e8",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|unowned-structural-fragment|||a10a1ee126cf3abfd9b71b87a6e94119944d8bf9383e56c30c3b5311ab4502eb|0|e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855|schema-like notation has no owner under the conservative source grammar",
        source_locations: &["a01:1398"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &[],
        rationale: "a01:1398: the brace tuple `{schema/tag,authority_domain,artifact_kind,body_digest,externally_pinned_root_policy_id}` enumerates the domain-separated signing transcript each root signature signs; transcript-content notation, not a durable schema, and no parsed candidate owns it (empty set legal for unowned-structural-fragment).",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:34577cda100fc597ce5020921e7520ccf2ff9ea71a5bee91bcfed896e09733cc",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|unowned-structural-fragment|||ca9f00a3b8cc175b18ae5563499e963fe0f48db8a7463a85055ad81180bb5f6d|0|e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855|schema-like notation has no owner under the conservative source grammar",
        source_locations: &["a01:1406"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &[],
        rationale: "a01:1406: `GlobalTxnRecord|GlobalControlRecord` is a target-set enumeration naming what the global command wrappers target; the pipe-joined phrase is prose enumeration of externally defined schemas, not a union schema of this slice (empty set legal for unowned-structural-fragment).",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0d9fa91b6888b1d850b7dc8d59eabdf2aa50b4782f6cfe5d9abec75bd9127586",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|unparsed-record-item|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId|e22b532e93a1d233404c44401b800debc6e640d28c0156ee6adf06f9cd9907a2|1|7b328a6974a5d4010e974e3c5ed04ed52d1942ce154471d6544eead1534710b8|record item does not begin with a lowercase stable field name",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale: "a01:1443: `ContiguousSpan { root_failure_domain_id, segment_id, offset, encoded_len, root_symbol_inventory_digest }` is a named closed sub-schema item inside the specialized descriptor per the a01:1412 compact-phrase law; it is part of the `top|PlacementDescriptorWithoutId` candidate, not an open bag or stray schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d7a9a4eb5a85dfb74c358f357b30941729f34bcffd4a4a80e5acbe984df5ca50",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|unparsed-record-item|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate|dec1da9246adf963002710d7196c7ac12701ceb27c317b96f62bdf586e7e16a4|1|b0baa8c4a8438d18729d8427b18e3d858484938a9e72e731b91c5d5a337d75b7|record item does not begin with a lowercase stable field name",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseAckCertificate"],
        rationale: "a01:1404: the leading `SameGroupCertificateHeader` item is a named closed sub-schema (compact-phrase law, a01:1412) embedded in the ack-certificate body; it belongs to the `top|RemoteRetentionReleaseAckCertificate` candidate.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b309ff017e04d9e2ad7b7d57dd82659a085c5ed58fb994ea08f5ca857aeb8b80",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|unparsed-record-item|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate|dec1da9246adf963002710d7196c7ac12701ceb27c317b96f62bdf586e7e16a4|1|abe5a713343b96a497b548dd4d0d27df433230303dbb43716e0a9fd9635c83fa|record item does not begin with a lowercase stable field name",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestCertificate"],
        rationale: "a01:1404: the leading `SameGroupCertificateHeader` item is a named closed sub-schema (compact-phrase law, a01:1412) embedded in the request-certificate body; it belongs to the `top|RemoteRetentionReleaseRequestCertificate` candidate.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ba8e5e4bfced370e72e8c5f2de3110ca3176fc081dcbdf0ebbf5070c8109f914",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AppliedAbortRef|AppliedAbortRef.LocalControl.logical_command_seq|6772e0b779a88c182d3c72658ff39117288f1e21888b6d645f18cbfaf31f4f07|1|020316d94da326c610f0b897f752c5ac34e9bff4c12f6680f768d43cca180259|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.LocalControl.logical_command_seq|logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `logical_command_seq` at census path `AppliedAbortRef.LocalControl.logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AppliedAbortRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:da6796d82ec8f0ad13cb7e98a3dd5027e081d6166fb25010f34fc1d3f942face",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AppliedAbortRef|AppliedAbortRef.LocalTxn.logical_command_seq|6772e0b779a88c182d3c72658ff39117288f1e21888b6d645f18cbfaf31f4f07|1|28934fb3533f5bb21a32c45c48cfce245328d1c6558f4f857e28975fc21f3c5c|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.LocalTxn.logical_command_seq|logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `logical_command_seq` at census path `AppliedAbortRef.LocalTxn.logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AppliedAbortRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cd5263e4e5a623d18abf05fd873322bd2aed0ecf4101650b392c8ac0dfe83342",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AppliedAbortRef|AppliedAbortRef.MetaControl.global_logical_command_seq|3f7b656911969255fdb48c3d3fae9686c1aa03fe2303a5840f7718879dabf821|1|a435b6b799e25900fad7061a75eb6f72e70c9ad0ece40effecd5ed88c14175fe|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.MetaControl.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `global_logical_command_seq` at census path `AppliedAbortRef.MetaControl.global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AppliedAbortRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:86c7e179248aaf7991f43a1f3327c09353cf813cc01b8b33a36e4fa7bc70c63d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AppliedAbortRef|AppliedAbortRef.MetaTxn.global_logical_command_seq|3f7b656911969255fdb48c3d3fae9686c1aa03fe2303a5840f7718879dabf821|1|b6e63e586168ac30d40e23ccf6af531fefe10ee078b9496e0ddc5f30c3b33f11|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.MetaTxn.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `global_logical_command_seq` at census path `AppliedAbortRef.MetaTxn.global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AppliedAbortRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:5a6fd0f3b82c7f0e25cc7f2e54a6979556e86845159ade0fc5db84f20a68ce39",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AppliedControlRef|AppliedControlRef.Local.logical_command_seq|6772e0b779a88c182d3c72658ff39117288f1e21888b6d645f18cbfaf31f4f07|1|e45f2177fcf44b32e9a17193ed8aabe669bb9347daeb8ebf57e3dc4764f21d4f|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedControlRef|AppliedControlRef.Local.logical_command_seq|logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `logical_command_seq` at census path `AppliedControlRef.Local.logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AppliedControlRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:2d3a238643d53101c5c0b1b76309f7842bac2fde143198374ce80f7a28460922",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AppliedControlRef|AppliedControlRef.Meta.global_logical_command_seq|3f7b656911969255fdb48c3d3fae9686c1aa03fe2303a5840f7718879dabf821|1|aeac36594abe2c36dd398f6201e669b186e27be64300033893a5d0f18c6b5f88|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedControlRef|AppliedControlRef.Meta.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `global_logical_command_seq` at census path `AppliedControlRef.Meta.global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AppliedControlRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4cd9ff504a8f45737e9059b60893933afd7d86625b7e0c02f1f076120253317b",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AuditCandidateRef|AuditCandidateRef.Local.blocked_after_logical_command_seq|eb3a91041e06c8ea6319585162eaa84725fc15980da9200e158bf1d836e1a29b|1|e7358e9b2beddda6abf89fee4da29c07f3f46809579c6c1424cb72fc058646a3|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuditCandidateRef|AuditCandidateRef.Local.blocked_after_logical_command_seq|blocked_after_logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `blocked_after_logical_command_seq` at census path `AuditCandidateRef.Local.blocked_after_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AuditCandidateRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:000a7ee23e3de7a2a40e0eeaed2ea1b2597bf1afc47a0fa82604885626570e48",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AuditCandidateRef|AuditCandidateRef.Meta.blocked_after_global_logical_command_seq|21d0e2c97da51ac81a638f3cefee1f55ac8b34162861e2f77161732dfbe8a3da|1|ade225fa1453a73fd4a109654d8a9b203d45e6674cd67c6dfdbfcc88b5826c23|shorthand field has no exact type",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuditCandidateRef|AuditCandidateRef.Meta.blocked_after_global_logical_command_seq|blocked_after_global_logical_command_seq",
        ],
        rationale: "a01:1408: shorthand member `blocked_after_global_logical_command_seq` at census path `AuditCandidateRef.Meta.blocked_after_global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AuditCandidateRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3f8943e11fe38023572621016d3bd2736d76845c6fa86ca01ab11c963ac3d295",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AuthorityAppliedRef|AuthorityAppliedRef.Local.logical_command_seq|6772e0b779a88c182d3c72658ff39117288f1e21888b6d645f18cbfaf31f4f07|1|1dc8eb7afed586eafdf11f9a589affc8f2eef276b63cc8498fe4365d7d63921e|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuthorityAppliedRef|AuthorityAppliedRef.Local.logical_command_seq|logical_command_seq",
        ],
        rationale: "a01:1404: shorthand member `logical_command_seq` at census path `AuthorityAppliedRef.Local.logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AuthorityAppliedRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cd8a157343d76e480a778f55bb074c26f2b450fe8d1a019afcb01160605d0736",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AuthorityAppliedRef|AuthorityAppliedRef.Meta.global_logical_command_seq|3f7b656911969255fdb48c3d3fae9686c1aa03fe2303a5840f7718879dabf821|1|c3480c2d81d0422a6ec095c619f951e715c2452ab223c567bac96566a5a8c8aa|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuthorityAppliedRef|AuthorityAppliedRef.Meta.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale: "a01:1404: shorthand member `global_logical_command_seq` at census path `AuthorityAppliedRef.Meta.global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact AuthorityAppliedRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:645cd3190c29d6877e5b52fbd9a7eb2d12617be3015867f30ebf73aff4d632f6",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|AuthorityAppliedRef|AuthorityAppliedRef.Shard.shard_raft_index|a27fa693b624fba3783550d90e89ab5aefe7ea90b62eb261b9121ba0e77873bb|1|76400cead191ecb9b721044ced97ee7880c32ff12f233ae14e615c9f1562b0a8|shorthand field has no exact type",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuthorityAppliedRef|AuthorityAppliedRef.Shard.shard_raft_index|shard_raft_index",
        ],
        rationale: "a01:1404: shorthand member `shard_raft_index` at census path `AuthorityAppliedRef.Shard.shard_raft_index` carries no inline exact type; the span is committed byte-exactly by the exact AuthorityAppliedRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4798d0e4d5005fcd185c72d5e213ed7330f55d1af1d633542b7eab63fe45cf84",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CertifiedRemoteStrongRef|CertifiedRemoteStrongRef<T>.target_closure_inventory_digest|e9f4ca6621f4e36a6c7058504702c6ef874721b67a2a9208b46097af61cc285a|1|fb1eb2de82450003407412e48333584d6f8cc08dcafcda43f4db07459d0843af|shorthand field has no exact type",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CertifiedRemoteStrongRef<T>|CertifiedRemoteStrongRef<T>.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale: "a01:1402: shorthand member `target_closure_inventory_digest` at census path `CertifiedRemoteStrongRef<T>.target_closure_inventory_digest` carries no inline exact type; the span is committed byte-exactly by the exact CertifiedRemoteStrongRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:8d3b56aa3a767ea542872bc1ba3fd0ab477a7ed2ca662c337ee1bf8949583e7d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalCommandRef|ConditionalCommandRef.command_ref|9b2ddb763a7736180dd65c3329ab8a347e9b3bee70006fb913700f8103032b71|1|a132328a25427bbdcde954a714f2d5a843cf5c92931bf4bbac824f5ce8d195bd|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalCommandRef|ConditionalCommandRef.command_ref|command_ref",
        ],
        rationale: "a01:1394: shorthand member `command_ref` at census path `ConditionalCommandRef.command_ref` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:35bc46ce1fd0d6c8b7488a044ef386504bc03fbd5d1ec2b770e86ab030a492d5",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalCoordinateRef|ConditionalCoordinateRef.branch|f38c764c8aa00b6578f4254a4dc6d9b50f88fa926e270ea7859bd1b707cd8662|1|1e5045ee3f3d13cdfac05bd4ccd2bddf5bfd776336d72ea79fc4ca5d60a7900c|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalCoordinateRef|ConditionalCoordinateRef.branch|branch",
        ],
        rationale: "a01:1394: shorthand member `branch` at census path `ConditionalCoordinateRef.branch` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalCoordinateRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:8bb17f7c3ce721f479db1a83c8e7dda855ec98c2eb641e4e187851f8c7c82723",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalCoordinateRef|ConditionalCoordinateRef.graph|eef93e1d14482804277fca0172464032d1a4fdbcc338524059fa1e861454ad4d|1|eb7d455bcbaf58f754c886d524cc8c717e267921b3e96e794c34600989d7db90|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalCoordinateRef|ConditionalCoordinateRef.graph|graph",
        ],
        rationale: "a01:1394: shorthand member `graph` at census path `ConditionalCoordinateRef.graph` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalCoordinateRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:71b82ee0c32114b11f3406e639c01a44934215169bc01a02acd1b77d779ffe60",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalCoordinateRef|ConditionalCoordinateRef.oid|68c55cdc265db5ebdce885c914c4774fedeb3d824fc79837cad12cf1f10ccff7|1|ea03546b1b30d85877d6cd7fd0310b7d76fea91a96aa3e1efdc53ee9a10ed340|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|ConditionalCoordinateRef|ConditionalCoordinateRef.oid|oid"],
        rationale: "a01:1394: shorthand member `oid` at census path `ConditionalCoordinateRef.oid` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalCoordinateRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ce9bbf77be4d878c886864f08776365de232040b4d71c98b3322860c517e225c",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalGlobalCommandRef|ConditionalGlobalCommandRef.global_logical_command_seq|3f7b656911969255fdb48c3d3fae9686c1aa03fe2303a5840f7718879dabf821|1|a9d7f5f81bf89596e7cc49849c1741fb7a4383595590a0f8aa724f9346d69a16|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalCommandRef|ConditionalGlobalCommandRef.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale: "a01:1406: shorthand member `global_logical_command_seq` at census path `ConditionalGlobalCommandRef.global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalGlobalCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:91b4ecfbd59471f33fe1b99f790946e8559c552ac794ffb308e1dd109479fff3",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalGlobalCommandRef|ConditionalGlobalCommandRef.record_oid|8082f11a53f90519af8045e1538bf1c7f7cccaecff31c30e8a1f3c1dd2365e1d|1|42d2719c29923363b9c82618a46102016ab373c389e80057d0ded691ef2165b0|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalCommandRef|ConditionalGlobalCommandRef.record_oid|record_oid",
        ],
        rationale: "a01:1406: shorthand member `record_oid` at census path `ConditionalGlobalCommandRef.record_oid` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalGlobalCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:fef8d53fe305b0feaba6773e11a19e89f6adbe510b9f58cb35907fed8fdda0ce",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef.assigned_global_logical_command_seq|a90c74bd92f9b5a2c59d35690153d7e7c4c3dd44d58e40532083e4871f436cdd|1|52b1140a2dbc8ecb37ffff58f0a0a0a38fe249b26f3bf13e919c1dcf6a8a32ac|shorthand field has no exact type",
        source_locations: &["a01:1406", "a11:1962"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef.assigned_global_logical_command_seq|assigned_global_logical_command_seq",
        ],
        rationale: "a01:1406, a11:1962: shorthand member `assigned_global_logical_command_seq` at census path `ConditionalGlobalTxnInputRef.assigned_global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalGlobalTxnInputRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:55674524425476cfb59b374d5d338c633a98488fde9d76d879b29e5b956b138c",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ExportLeaf|ExportLeaf<T>.quorum_signatures|4b7382d93588313ac60e777d7671792202dc4445f17d96d2882ed00971b64a35|1|00791af63539c54ccad5d808edf1263c3f05f4e540e9d33fb0355ebd9e8d79ba|shorthand field has no exact type",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.quorum_signatures|quorum_signatures",
        ],
        rationale: "a01:1400: shorthand member `quorum_signatures` carries no inline exact type, and ExportLeaf is a logical kind, so no wire envelope commits the span. The a01 owner ruling landed in 694d14b fixes it to `bytes` on the registered durable_fields row, derived from the two landed instances in this same release family plus authority_quorum_signatures, all bytes/65536. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:56ab0b78574dcc4cc8ada1e27d64228256633d327386f25f3e31c79b082b93d3",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef.command_oid|4de54ef750635a0f9ebc9384f55c1f1b8e9300e65229bd3df6de781c6a1ce55f|1|db5267d46771c819d5d4a720d074a536fc5e3d9cc8cddf5cd34e8ed62fe919a5|shorthand field has no exact type",
        source_locations: &["a01:1406", "a11:1962"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef.command_oid|command_oid",
        ],
        rationale: "a01:1406, a11:1962: shorthand member `command_oid` at census path `ConditionalGlobalTxnInputRef.command_oid` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalGlobalTxnInputRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c9eacf0a0499722ac0abbf874b419638446536d586ed009714f01c2fe685713e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalMarkerRef|ConditionalMarkerRef.marker_ref|8bd7f0937c09a3668e61f28badb35e6deb398cd9a5a4f9665f8015146106a3a0|1|a272df58881c324a3bef40b3bd63b15d8691ab09dba39f7e7e0eac85bb9ce3e0|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalMarkerRef|ConditionalMarkerRef.marker_ref|marker_ref",
        ],
        rationale: "a01:1394: shorthand member `marker_ref` at census path `ConditionalMarkerRef.marker_ref` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalMarkerRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:287525289d2fc48c53af7935b122ec99d01330bf6bc8e7acb3484a796c7b2dc1",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalShardCommandRef|ConditionalShardCommandRef.record_oid|8082f11a53f90519af8045e1538bf1c7f7cccaecff31c30e8a1f3c1dd2365e1d|1|94074dd77fd021f951a4f00d7d1ce2f918f17bf62e465a71f84c5bd92a25064b|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalShardCommandRef|ConditionalShardCommandRef.record_oid|record_oid",
        ],
        rationale: "a01:1406: shorthand member `record_oid` at census path `ConditionalShardCommandRef.record_oid` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalShardCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:27f66e75e71751e7776eb0bf7f45d44cc30ae3153b29daccd9d65a3d1341a0df",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalShardCommandRef|ConditionalShardCommandRef.shard_id|b1bcf07f36014c2b518c95d80caac02f5d996186cf7fcc3fb943fb9d07f34ad0|1|c78dbe4d2d901dd6ce837744cfd507b9c9bc982ad496371310ce2bca90d145f0|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalShardCommandRef|ConditionalShardCommandRef.shard_id|shard_id",
        ],
        rationale: "a01:1406: shorthand member `shard_id` at census path `ConditionalShardCommandRef.shard_id` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalShardCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:448fd4c780118dedd261ff04c0d3fdbb837394d31386becb8935f9bb5a7477f2",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConditionalShardCommandRef|ConditionalShardCommandRef.shard_raft_index|a27fa693b624fba3783550d90e89ab5aefe7ea90b62eb261b9121ba0e77873bb|1|368064c8af700831b0a48f4d438e1a1fe3b1cb69ffcc778a073290fabaa77479|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalShardCommandRef|ConditionalShardCommandRef.shard_raft_index|shard_raft_index",
        ],
        rationale: "a01:1406: shorthand member `shard_raft_index` at census path `ConditionalShardCommandRef.shard_raft_index` carries no inline exact type; the span is committed byte-exactly by the exact ConditionalShardCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:51fde1eb845f5b657f68fc1295946c8f2077008b1f8310d772b449ee0959a974",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConsensusDomain|ConsensusDomain.cluster_incarnation|9d7c51a25eaf4da545c58407190cf355ea305487ea49cf6146f19f577bad1073|1|2ae0033d470b6c79d27264123e549a61dedf776ca0c61f5f80b5c422c10378ed|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConsensusDomain|ConsensusDomain.cluster_incarnation|cluster_incarnation",
        ],
        rationale: "a01:1396: shorthand member `cluster_incarnation` at census path `ConsensusDomain.cluster_incarnation` carries no inline exact type; the span is committed byte-exactly by the exact ConsensusDomain wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:06e4b97e0d741edd0bb8d618d48df3daf7f39f7ff5f2b1aa0d2571b1cede5c2d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConsensusDomain|ConsensusDomain.database_id|731e26be806308135ef676088749224ec417f251d99bb827ed4432e121e2f02b|1|062255e5fc291d68f45dac76138bc7e780c24dfd530db1316ae12302bb9a2710|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|ConsensusDomain|ConsensusDomain.database_id|database_id"],
        rationale: "a01:1396: shorthand member `database_id` at census path `ConsensusDomain.database_id` carries no inline exact type; the span is committed byte-exactly by the exact ConsensusDomain wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bc4a928f57e4004abc66ef087f1484f9d14c2fa3ba3f740e52124e669b091bf2",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConsensusDomain|ConsensusDomain.database_security_namespace_id|27c2c87b407320770f92abe612b2dbea8c1711a0f5fbb2d2464ade311812a4b7|1|3f59203559c5ade5cb13c917378768436c361c17b2bd65f9dab37be776c60448|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConsensusDomain|ConsensusDomain.database_security_namespace_id|database_security_namespace_id",
        ],
        rationale: "a01:1396: shorthand member `database_security_namespace_id` at census path `ConsensusDomain.database_security_namespace_id` carries no inline exact type; the span is committed byte-exactly by the exact ConsensusDomain wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e03210167c369f94d93dc4c8253d36d29ac78011864a802f5843834bf284c4fb",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConsensusDomain|ConsensusDomain.group_id|abf8a60fbb818f7405d613d3760a68a8e794a415546761e42340661f7351f74b|1|aaf70e2f4fcf7470b24711c2607553342fbdad07f6db37d64c8b66540b71d196|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|ConsensusDomain|ConsensusDomain.group_id|group_id"],
        rationale: "a01:1396: shorthand member `group_id` at census path `ConsensusDomain.group_id` carries no inline exact type; the span is committed byte-exactly by the exact ConsensusDomain wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:afbc344d70d6cc6795f928f6378d234d4c01c0738c49b479437ecd307b85b863",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ConsensusDomain|ConsensusDomain.group_role.Shard.shard_id|b1bcf07f36014c2b518c95d80caac02f5d996186cf7fcc3fb943fb9d07f34ad0|1|cf674780f1d985fa5834a46425a25bbbb7ec14144b426419b56ffd5bc50e4651|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConsensusDomain|ConsensusDomain.group_role.Shard.shard_id|shard_id",
        ],
        rationale: "a01:1396: shorthand member `shard_id` at census path `ConsensusDomain.group_role.Shard.shard_id` carries no inline exact type; the span is committed byte-exactly by the exact ConsensusDomain wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:5d0a9654f53322e956d33010ca7df5afc44cb67dbde765dcef51425db02dac13",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.encoding_id|85d360220cc757431dd090a481da78fba2018975f0a2e5ea5fa5e161b7a16177|1|c7e57e032723c3f855ce48be060adea813e685683290a30ad36f3a7648869b0a|shorthand field has no exact type",
        source_locations: &["a01:1443", "a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.encoding_id|encoding_id",
        ],
        rationale: "a01:1443, a02:1449: shorthand member `encoding_id` at census path `PlacementDescriptorWithoutId.encoding_id` carries no inline exact type; the span is committed byte-exactly by the exact PlacementDescriptorWithoutId wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d24695012d4044e09c759f8614fa9a032db2cbec2fc3c93e77ab749ce5a608d8",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.failure_domain_policy_id|04709b2bb4927b600fac8deadfcf2eb5843fb528b15e1ad340a5a4439a9fc3ef|1|dc06ab84e3bce9c11070d01a52853ca7bca4f68cffeaf5d0a78c1aaceacf76ea|shorthand field has no exact type",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.failure_domain_policy_id|failure_domain_policy_id",
        ],
        rationale: "a01:1443: shorthand member `failure_domain_policy_id` at census path `PlacementDescriptorWithoutId.failure_domain_policy_id` carries no inline exact type; the span is committed byte-exactly by the exact PlacementDescriptorWithoutId wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:43d2f0e65a4dc39f329e611ac733251f6188cb5cc544e93beef46e135474beab",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.root_placement_epoch|e7a5dbd0ab1abef202352d60c1b4f5eca3f71b78f494476a4f54a55091c321f0|1|17de7b8f543ef7b2715baeaaf819e8ea35de5ae7fc3a81edecd16c549d972b6b|shorthand field has no exact type",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.root_placement_epoch|root_placement_epoch",
        ],
        rationale: "a01:1443: shorthand member `root_placement_epoch` at census path `PlacementDescriptorWithoutId.root_placement_epoch` carries no inline exact type; the span is committed byte-exactly by the exact PlacementDescriptorWithoutId wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e1c52d6dcbd8885faf4eba1c8406864266ea081cd420b677c5467c4a97d9100b",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.final_retirement_floor_digest|0d19b9043ba1e24e570c9ddb7f4952666ee2a7d62116d617b987fb4a75d0ac4f|1|b83c51729087fffa1fe3e3b3452234e78f5d92ec86b35d7df4fe1979b46f5fc5|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.final_retirement_floor_digest|final_retirement_floor_digest",
        ],
        rationale: "a01:1398: shorthand member `final_retirement_floor_digest` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.Successor.final_retirement_floor_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:404a1f5ffca0b239e6d6304f019a320bfbb2f771e8d8f75075ea4aa41d31fbc6",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.joint_transition_transcript|3a5b732b143e059fa75e5e80c97a6391ec5d5ba9b14e780d2b45b0d92e4f5c78|1|a6c99b864aa1ae5d26e3b7f26bd3c2c19c359c8ce07579d8919793f47ec4d476|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.joint_transition_transcript|joint_transition_transcript",
        ],
        rationale: "a01:1398: shorthand member `joint_transition_transcript` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.Successor.joint_transition_transcript` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6150ede1e1b9372211d8fcedffd089371e78f6ef8bdf839499417b25f27d9e22",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.new_configuration_quorum_signatures|274b2dfe7211b3a80c9d1a48ae7215b3a4e08741d73544a3c68555075e759568|1|15edeb75aa867f73d56562652894b4b13d03cc4fa747ee6ea6d470f97905be0c|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.new_configuration_quorum_signatures|new_configuration_quorum_signatures",
        ],
        rationale: "a01:1398: shorthand member `new_configuration_quorum_signatures` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.Successor.new_configuration_quorum_signatures` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:af2b5edb93577dd590386a385295f80a7ae01d42de718097a430f50357d49612",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.old_configuration_quorum_signatures|4d3d631c393e2608fc220dc9ccc5b46c2ccf2f5adeee9792c26fe42cdae2eff3|1|8826373f9b0b5f49b16452b1d2ea787a1624cda9faa348937d0e6cec7f3540d1|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.old_configuration_quorum_signatures|old_configuration_quorum_signatures",
        ],
        rationale: "a01:1398: shorthand member `old_configuration_quorum_signatures` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.Successor.old_configuration_quorum_signatures` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:299ee60ab1337573ea4994c60bdbe86a53c9be1ecf08fd2f189e0a4f025be75e",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.final_retirement_floor_digest|0d19b9043ba1e24e570c9ddb7f4952666ee2a7d62116d617b987fb4a75d0ac4f|1|1b2384171b391a46d17735555abbd4c7d6734f3d2b3c0539a90069ea6782324a|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.final_retirement_floor_digest|final_retirement_floor_digest",
        ],
        rationale: "a01:1398: shorthand member `final_retirement_floor_digest` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.final_retirement_floor_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3543ce46e3d146757ff29ca8552c7c2c03766d34c11e9c4b2e70e47446a29da2",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.joint_transition_transcript|3a5b732b143e059fa75e5e80c97a6391ec5d5ba9b14e780d2b45b0d92e4f5c78|1|3baab0cda94ea5af5cd9fd2d99923856924760ac9a5703f7d876d0404f122cb4|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.joint_transition_transcript|joint_transition_transcript",
        ],
        rationale: "a01:1398: shorthand member `joint_transition_transcript` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.joint_transition_transcript` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9bac05f943c14519aa377b05808e57bfa117279607768581f79fb2720b221f99",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.new_configuration_quorum_signatures|274b2dfe7211b3a80c9d1a48ae7215b3a4e08741d73544a3c68555075e759568|1|547c7a0fd6a457595fa224c7f5b6a20d8ca308b182fd6270ed8097129182b109|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.new_configuration_quorum_signatures|new_configuration_quorum_signatures",
        ],
        rationale: "a01:1398: shorthand member `new_configuration_quorum_signatures` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.new_configuration_quorum_signatures` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:08cbc807047e544cd6c1e2598630e187feb142c28fc295cc0a7696e8d36dc68f",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.old_configuration_quorum_signatures|4d3d631c393e2608fc220dc9ccc5b46c2ccf2f5adeee9792c26fe42cdae2eff3|1|74220eeea6d3b639145b77c67549571b2ed637d340b8c74474f1f76b0d9db17e|shorthand field has no exact type",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.old_configuration_quorum_signatures|old_configuration_quorum_signatures",
        ],
        rationale: "a01:1398: shorthand member `old_configuration_quorum_signatures` at census path `RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.old_configuration_quorum_signatures` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RemoteAuthorityConfigurationEvidence arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:107590809bb6e12167b2f2dd3e8f10051a169c3a28357753e2ddaeb18d7deb20",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteObjectIdentity|RemoteObjectIdentity.canonical_digest|cb716809bc72c2ad62ef437f82f63bd9bafe47dacf9a8df9b303efdedaa22434|1|f497359ffb570c42d350b2e5ed98ed737eed86381e950dc84fa1cd9af98d9737|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteObjectIdentity|RemoteObjectIdentity.canonical_digest|canonical_digest",
        ],
        rationale: "a01:1396: shorthand member `canonical_digest` at census path `RemoteObjectIdentity.canonical_digest` carries no inline exact type; the span is committed byte-exactly by the exact RemoteObjectIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bd6a0a183874f88db3ea3ec0302152530c28df45a9f5dd4897784ea8a85ec3a4",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteObjectIdentity|RemoteObjectIdentity.object_kind|f79080c76579371982ec2dfc62593cfbe282dd0d9ec076e5638251fc9ce30909|1|d04c9ba70a928a52dec5b4ea457f6fd768aada2b4dc4ff009110b89fd8fbb6b9|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteObjectIdentity|RemoteObjectIdentity.object_kind|object_kind",
        ],
        rationale: "a01:1396: shorthand member `object_kind` at census path `RemoteObjectIdentity.object_kind` carries no inline exact type; the span is committed byte-exactly by the exact RemoteObjectIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b5662f79036ce591940334355f520642a72b0634d4a15befe4513b36e25d90ba",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RemoteObjectIdentity|RemoteObjectIdentity.object_oid|48f28b7b97e516c375e4651abda6b4d059ae71b431f5c7c360b8416382ec0830|1|fcdf56ca4807125d4919d0849d9cbb4cf5b050fa8fad9acda6bbbd5ab7372a21|shorthand field has no exact type",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteObjectIdentity|RemoteObjectIdentity.object_oid|object_oid",
        ],
        rationale: "a01:1396: shorthand member `object_oid` at census path `RemoteObjectIdentity.object_oid` carries no inline exact type; the span is committed byte-exactly by the exact RemoteObjectIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6047adc7965b7865e94cb7668b9a58beefe4bc5ccad991358a95c9be0bc29aab",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTransitionActivationState|RoleTransitionActivationState.Meta.migration_nonce|2b49d522166d09d8de34067bca5548329dbf4485bfe4f55e1f898ccfb886ca6d|1|380da794623fa2dfc4970c85a7c3067446b22af883a5b006a7c580845b5f6127|shorthand field has no exact type",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Meta.migration_nonce|migration_nonce",
        ],
        rationale: "a01:1390: shorthand member `migration_nonce` at census path `RoleTransitionActivationState.Meta.migration_nonce` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RoleTransitionActivationState arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:20a8a347b7c249c90e09deab30ed8ff464b44114e205bbef6f1852afb27419c5",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTransitionActivationState|RoleTransitionActivationState.Meta.phase.Complete.seal_release_state|37b2b3c60ca176d712f9e8320059a0b33f0f391506e7684f64f71ea9bb161e3c|1|4d726a2e5d1b209a1fc23d05f7646b58bad370dcccf8e993c2785ee4bb0b98e3|shorthand field has no exact type",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Meta.phase.Complete.seal_release_state|seal_release_state",
        ],
        rationale: "a01:1390: shorthand member `seal_release_state` at census path `RoleTransitionActivationState.Meta.phase.Complete.seal_release_state` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RoleTransitionActivationState arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ae9bc8dd611d9feec022771782f885fbc37718ed23bab487b4189f9841d51822",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTransitionActivationState|RoleTransitionActivationState.Meta.target_service_visibility_epoch|a2197f22ecc6ffa08da8cee13fd767dd6acbb65efad9a4e75b1e5796d68df04c|1|809e5a9011863b4811a832204015b4490ab210e4628cbc9a9927c8e51307851e|shorthand field has no exact type",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Meta.target_service_visibility_epoch|target_service_visibility_epoch",
        ],
        rationale: "a01:1390: shorthand member `target_service_visibility_epoch` at census path `RoleTransitionActivationState.Meta.target_service_visibility_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RoleTransitionActivationState arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6fa3a8b63ee9d48d3658e670f75400ea12f03518bd4fea540eda6f1d8dc62f94",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTransitionActivationState|RoleTransitionActivationState.Shard.migration_nonce|2b49d522166d09d8de34067bca5548329dbf4485bfe4f55e1f898ccfb886ca6d|1|0b3edf990adbfed6d06979e1d6ad385deff9f3277d26963d49c17913e456b399|shorthand field has no exact type",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.migration_nonce|migration_nonce",
        ],
        rationale: "a01:1390: shorthand member `migration_nonce` at census path `RoleTransitionActivationState.Shard.migration_nonce` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RoleTransitionActivationState arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:530ab398113ef1b97858212d423b792e306d68ce291da23fdb3ae00831158d3a",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTransitionActivationState|RoleTransitionActivationState.Shard.phase.Complete.seal_release_state|37b2b3c60ca176d712f9e8320059a0b33f0f391506e7684f64f71ea9bb161e3c|1|dc1a7c1492c3109347f66575756f3b75912b6c3cb046e8a7e9297211f28409ee|shorthand field has no exact type",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.phase.Complete.seal_release_state|seal_release_state",
        ],
        rationale: "a01:1390: shorthand member `seal_release_state` at census path `RoleTransitionActivationState.Shard.phase.Complete.seal_release_state` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RoleTransitionActivationState arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0dbcee60eff18a6b83fbfca6710c56cef320a2b09b9eacdd495000f524007939",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTransitionActivationState|RoleTransitionActivationState.Shard.shard_id|b1bcf07f36014c2b518c95d80caac02f5d996186cf7fcc3fb943fb9d07f34ad0|1|01e2ac2f4f732616d71c01cbab47af4595f9fdbe36b4b1561537fd26024edd71|shorthand field has no exact type",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.shard_id|shard_id",
        ],
        rationale: "a01:1390: shorthand member `shard_id` at census path `RoleTransitionActivationState.Shard.shard_id` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RoleTransitionActivationState arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:2b3f0d0dc9a723abcaf65c604e43b62aa250900a9806fc691b61ad743557b433",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTransitionActivationState|RoleTransitionActivationState.Shard.target_service_visibility_epoch|a2197f22ecc6ffa08da8cee13fd767dd6acbb65efad9a4e75b1e5796d68df04c|1|cac11bc6b4c7baf91ebc39623cf8e63e2b0d3b36b4ee939334b7aa2c4a4df488|shorthand field has no exact type",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.target_service_visibility_epoch|target_service_visibility_epoch",
        ],
        rationale: "a01:1390: shorthand member `target_service_visibility_epoch` at census path `RoleTransitionActivationState.Shard.target_service_visibility_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of its RoleTransitionActivationState arm body, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cb5825c40682074cd88d59caa2f46d67c9b6115be7ebbe0208b97da0b45d74c9",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongCiphertextRef|StrongCiphertextRef<T>.ciphertext_digest|639e8bce8cd2107b20eb748abd7a4fa29f62ecd3752dab46977890694061e0f8|1|842aaaf6f38e22319299a7cdd6e722024a97dca6e218981ba15530401820482f|shorthand field has no exact type",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.ciphertext_digest|ciphertext_digest",
        ],
        rationale: "a01:1410: shorthand member `ciphertext_digest` at census path `StrongCiphertextRef<T>.ciphertext_digest` carries no inline exact type; the span is committed byte-exactly by the exact StrongCiphertextRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b7504277054b04c1cf2ceed042250c26449c0452fd4d7c8e99ea4b9f75bdb846",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongCiphertextRef|StrongCiphertextRef<T>.ciphertext_id|47142c70021a01c228028cf69cea5c897a6985a67f0ff4234f8aa3bea11a523e|1|096d0abb87e92295bbdc4f05d7ab4753be474e6bbee0d5cedb0bbe6c41fd1cc7|shorthand field has no exact type",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.ciphertext_id|ciphertext_id",
        ],
        rationale: "a01:1410: shorthand member `ciphertext_id` at census path `StrongCiphertextRef<T>.ciphertext_id` carries no inline exact type; the span is committed byte-exactly by the exact StrongCiphertextRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0d955780da7823b64d2ac4832293cb252f40d4f7d23421df903e14e41dc3cab5",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongCiphertextRef|StrongCiphertextRef<T>.dek_id|8af69fe8b94da3d64b510caa0dab1b7e9030eae0b4bb2faac1b7680fc9a7edb1|1|5d5d4c629230065df34c5eef58af70ca02eb34716fca83994a29596faad119f1|shorthand field has no exact type",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.dek_id|dek_id",
        ],
        rationale: "a01:1410: shorthand member `dek_id` at census path `StrongCiphertextRef<T>.dek_id` carries no inline exact type; the span is committed byte-exactly by the exact StrongCiphertextRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a584b1e1a58dc60fff87a8bae2f09810790db317ac3222eccc0058ee781b6f6b",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongCiphertextRef|StrongCiphertextRef<T>.logical_oid|fdd9a74312ec7ab8a436466dbb215bc937ed5e6accb9d7b85a22c9c095a2d444|1|2c3bd464b139f8a31336a89e1897c252d80501b8b8f92aebd479383e0f883b56|shorthand field has no exact type",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.logical_oid|logical_oid",
        ],
        rationale: "a01:1410: shorthand member `logical_oid` at census path `StrongCiphertextRef<T>.logical_oid` carries no inline exact type; the span is committed byte-exactly by the exact StrongCiphertextRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:1e315e718c1dc57fdb221a62568115435bbd13f2273a677b758dcd6bc444e86d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongCiphertextRef|StrongCiphertextRef<T>.protected_length|5df4272c5f39dcef78c410b10c536a5704975cc8d6689dc6fbbcbeaed688ecf1|1|3e9b36e60da97f4543cfc50850f35e26cd72a01de7eb5127152027457f6dc462|shorthand field has no exact type",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.protected_length|protected_length",
        ],
        rationale: "a01:1410: shorthand member `protected_length` at census path `StrongCiphertextRef<T>.protected_length` carries no inline exact type; the span is committed byte-exactly by the exact StrongCiphertextRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:652c475ecabc6c9b76ce0f9c6fc7e5158ce7db6433cbdfd4edd2e8013d9fcb04",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongCiphertextRef|StrongCiphertextRef<T>.recoverability_profile|6871207b09a5b8ea4e3e5059bafceaf24232761332d71407cccc6f45ec96b6e7|1|646b4ed076e19f40c0cf64ae193965d4f401462a33a6c5e37caa9878506aa5ac|shorthand field has no exact type",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.recoverability_profile|recoverability_profile",
        ],
        rationale: "a01:1410: shorthand member `recoverability_profile` at census path `StrongCiphertextRef<T>.recoverability_profile` carries no inline exact type; the span is committed byte-exactly by the exact StrongCiphertextRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bf14b1da59063849fedae6811820b7eac779d226996884d50707d7a33b9016e4",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongCommandRef|StrongCommandRef.command_ref|9b2ddb763a7736180dd65c3329ab8a347e9b3bee70006fb913700f8103032b71|1|4c0e7b3a074b759317ddb4a97054ddcf7bffb77b47748f11cf4cc0eb2170c5a2|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|StrongCommandRef|StrongCommandRef.command_ref|command_ref"],
        rationale: "a01:1394: shorthand member `command_ref` at census path `StrongCommandRef.command_ref` carries no inline exact type; the span is committed byte-exactly by the exact StrongCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6eb7a154015a5376477a8c84497a17f9adcb3359e3f533d0547e2cc9c40a8799",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongGlobalCommandRef|StrongGlobalCommandRef.global_logical_command_seq|3f7b656911969255fdb48c3d3fae9686c1aa03fe2303a5840f7718879dabf821|1|3785c1a77c1091e7462f5cf64078b3905406763659c8364c7cd884406c8ec7c7|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongGlobalCommandRef|StrongGlobalCommandRef.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale: "a01:1406: shorthand member `global_logical_command_seq` at census path `StrongGlobalCommandRef.global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact StrongGlobalCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:acf6c339daf3a326175b85aabd84678e241367cdc7b5c946db9157b194a6ab34",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongGlobalCommandRef|StrongGlobalCommandRef.record_oid|8082f11a53f90519af8045e1538bf1c7f7cccaecff31c30e8a1f3c1dd2365e1d|1|25ea8ff46587c2a3d0bdd8d51de58654bc73b12355eb28c244d84165779f2452|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongGlobalCommandRef|StrongGlobalCommandRef.record_oid|record_oid",
        ],
        rationale: "a01:1406: shorthand member `record_oid` at census path `StrongGlobalCommandRef.record_oid` carries no inline exact type; the span is committed byte-exactly by the exact StrongGlobalCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cd64b0149d18dc9934a66167c0937515a7618074d137a31ebb79344501932118",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongMarkerRef|StrongMarkerRef.marker_ref|8bd7f0937c09a3668e61f28badb35e6deb398cd9a5a4f9665f8015146106a3a0|1|2475f4c2be72c94970090faa3947bfd8ad0c0b5f742dc6cdb3e579dfa6314e06|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|StrongMarkerRef|StrongMarkerRef.marker_ref|marker_ref"],
        rationale: "a01:1394: shorthand member `marker_ref` at census path `StrongMarkerRef.marker_ref` carries no inline exact type; the span is committed byte-exactly by the exact StrongMarkerRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3f8f3176a7718116cb6f1545bd688ad47df7968959d783ff0267d18843369182",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongRef|StrongRef.oid|68c55cdc265db5ebdce885c914c4774fedeb3d824fc79837cad12cf1f10ccff7|1|3fc8080058ece4f672ffa97feb30b98be37bb1f21e59b59fd529f867ddb53257|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|StrongRef|StrongRef.oid|oid"],
        rationale: "a01:1394: shorthand member `oid` at census path `StrongRef.oid` carries no inline exact type; the span is committed byte-exactly by the exact StrongRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a6fb8ede356a5dcb049d10c0e248c928bc56e5eaedc09be55230e95f73c77310",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongShardCommandRef|StrongShardCommandRef.record_oid|8082f11a53f90519af8045e1538bf1c7f7cccaecff31c30e8a1f3c1dd2365e1d|1|99cb4e03942a97d67c9f3b53a90023900a21f8d4e7c6779b69f97311da137dee|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongShardCommandRef|StrongShardCommandRef.record_oid|record_oid",
        ],
        rationale: "a01:1406: shorthand member `record_oid` at census path `StrongShardCommandRef.record_oid` carries no inline exact type; the span is committed byte-exactly by the exact StrongShardCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6b31b163a80c3768fbba1b6cdbfd63504b7796fd719f38e655aa60e1c4c7ed20",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongShardCommandRef|StrongShardCommandRef.shard_id|b1bcf07f36014c2b518c95d80caac02f5d996186cf7fcc3fb943fb9d07f34ad0|1|dcf6603296a32f7c751f560b3506e3a05f95155abd0d3561b2db1ae5790fa463|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongShardCommandRef|StrongShardCommandRef.shard_id|shard_id",
        ],
        rationale: "a01:1406: shorthand member `shard_id` at census path `StrongShardCommandRef.shard_id` carries no inline exact type; the span is committed byte-exactly by the exact StrongShardCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f84e8e64b2b57ad03afd6a8b703c0a8b7ed1979e0ecad941a601b2f9d4d39077",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|StrongShardCommandRef|StrongShardCommandRef.shard_raft_index|a27fa693b624fba3783550d90e89ab5aefe7ea90b62eb261b9121ba0e77873bb|1|1fcbeb355c0ea83b77cf928f2b329b24fa0b5236194fcd94813b17530734271e|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongShardCommandRef|StrongShardCommandRef.shard_raft_index|shard_raft_index",
        ],
        rationale: "a01:1406: shorthand member `shard_raft_index` at census path `StrongShardCommandRef.shard_raft_index` carries no inline exact type; the span is committed byte-exactly by the exact StrongShardCommandRef wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:50cbfe932bcfbb0429e3ec0a3f025fff8b69d37555f229b96b6bff6191c7bdb3",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakDigest|WeakDigest.digest|0bf474896363505e5ea5e5d6ace8ebfb13a760a409b1fb467d428fc716f9f284|1|ac0260b36244a13f921e613b03c09fa4839d98967fade94e64f17d87989e6f60|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|WeakDigest|WeakDigest.digest|digest"],
        rationale: "a01:1394: shorthand member `digest` at census path `WeakDigest.digest` carries no inline exact type; the span is committed byte-exactly by the exact WeakDigest wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:139510a77fde9ead91fa0ec2678ea00991c4e70d1df84f8ab78848677f222a67",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakGlobalCommandIdentity|WeakGlobalCommandIdentity.global_logical_command_seq|3f7b656911969255fdb48c3d3fae9686c1aa03fe2303a5840f7718879dabf821|1|0d0ca5c0470c50842d2b12212f14adb2f5744f3436bb903c77e6d9fe893e7196|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakGlobalCommandIdentity|WeakGlobalCommandIdentity.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale: "a01:1406: shorthand member `global_logical_command_seq` at census path `WeakGlobalCommandIdentity.global_logical_command_seq` carries no inline exact type; the span is committed byte-exactly by the exact WeakGlobalCommandIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7cad9b9c94e36c6ae8838f509fc3020a94487b0f21ebdd0de8116fbb71d64a82",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakGlobalCommandIdentity|WeakGlobalCommandIdentity.record_oid|8082f11a53f90519af8045e1538bf1c7f7cccaecff31c30e8a1f3c1dd2365e1d|1|429832236062df73658c420bd53c6eb1aeb25389c6f3dc6f4c0c6f015534330f|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakGlobalCommandIdentity|WeakGlobalCommandIdentity.record_oid|record_oid",
        ],
        rationale: "a01:1406: shorthand member `record_oid` at census path `WeakGlobalCommandIdentity.record_oid` carries no inline exact type; the span is committed byte-exactly by the exact WeakGlobalCommandIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9f08c1c75203bd3827179ad5f4a0310ca62537ca1fdc04fa423a491b8b53bf7d",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakMarkerIdentity|WeakMarkerIdentity.commit_seq|e2893bd6c4461488df900513741a96335d5e7a20ec2ae2a99aa75835ba663ae2|1|099cc63b339b0492ca7dca7a29b5cbafc7d33af7ce3814ea1e95617f2dffa1ba|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakMarkerIdentity|WeakMarkerIdentity.commit_seq|commit_seq",
        ],
        rationale: "a01:1394: shorthand member `commit_seq` at census path `WeakMarkerIdentity.commit_seq` carries no inline exact type; the span is committed byte-exactly by the exact WeakMarkerIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e838c4e8c16f22e447b9f945df464fd6d6cbcccf0c9eaf87d67d9aaffd2dfc19",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakMarkerIdentity|WeakMarkerIdentity.marker_oid|423c2869afb80889ddfc734cfcc992e00323c5ddc5774daf84d12374b7bd4ed0|1|9e2138ddab1831d94d123f97d5f31b18247add4c6f48efc01826940865b67d92|shorthand field has no exact type",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakMarkerIdentity|WeakMarkerIdentity.marker_oid|marker_oid",
        ],
        rationale: "a01:1394: shorthand member `marker_oid` at census path `WeakMarkerIdentity.marker_oid` carries no inline exact type; the span is committed byte-exactly by the exact WeakMarkerIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:71357f9cccbb9858750afa743c8c43e767f73a60ed20110a01be6f4aa2558826",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakShardCommandIdentity|WeakShardCommandIdentity.record_oid|8082f11a53f90519af8045e1538bf1c7f7cccaecff31c30e8a1f3c1dd2365e1d|1|9199d2431fdd33a1d4f1d09a13360c666944a5839ed03c7eb2251e2dae6d68a2|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakShardCommandIdentity|WeakShardCommandIdentity.record_oid|record_oid",
        ],
        rationale: "a01:1406: shorthand member `record_oid` at census path `WeakShardCommandIdentity.record_oid` carries no inline exact type; the span is committed byte-exactly by the exact WeakShardCommandIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d8cf9ed9c05e68dcf47540f49cf5e166b3e8a251f9c64c5f62afd135b057d125",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakShardCommandIdentity|WeakShardCommandIdentity.shard_id|b1bcf07f36014c2b518c95d80caac02f5d996186cf7fcc3fb943fb9d07f34ad0|1|f9994109430d2cd07ea86ed53b692b86d5f774a3401194dd6505df72961531ad|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakShardCommandIdentity|WeakShardCommandIdentity.shard_id|shard_id",
        ],
        rationale: "a01:1406: shorthand member `shard_id` at census path `WeakShardCommandIdentity.shard_id` carries no inline exact type; the span is committed byte-exactly by the exact WeakShardCommandIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6025faae5c1c51f71fecd00802a3336e4a8d6697a2073a9bab4340fbc38bea46",
        slice_id: "a01",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|WeakShardCommandIdentity|WeakShardCommandIdentity.shard_raft_index|a27fa693b624fba3783550d90e89ab5aefe7ea90b62eb261b9121ba0e77873bb|1|acdd45487fd9d74f5837d87d7b30d0c400ed7ccb0395dd8af27dbf2bf55fa14e|shorthand field has no exact type",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakShardCommandIdentity|WeakShardCommandIdentity.shard_raft_index|shard_raft_index",
        ],
        rationale: "a01:1406: shorthand member `shard_raft_index` at census path `WeakShardCommandIdentity.shard_raft_index` carries no inline exact type; the span is committed byte-exactly by the exact WeakShardCommandIdentity wire envelope contract, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:460202d74810b05f3b369b22e66dd2f8b4a2ee578ae502a4359404a71187455e",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ContinuityAuthorityCurrentBasis|ContinuityAuthorityCurrentBasis<Role>.Local.current_root_manifest_and_slot_identity|ca00f36878beee32c8a5e1df1ba578220ef65af8b69d26790e83f124f67de5e8|1|a0c83299f4c743eb09c17d215b669a9f21e3cda97ba5970b097ed30c2b20eeca|shorthand field has no exact type",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Local.current_root_manifest_and_slot_identity|current_root_manifest_and_slot_identity",
        ],
        rationale: "a16:2225: shorthand member `current_root_manifest_and_slot_identity` at census path `ContinuityAuthorityCurrentBasis<Role>.Local.current_root_manifest_and_slot_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ContinuityAuthorityCurrentBasis<Role>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d84bc693fde08ebdb0e53e54492f602de34d43c66e1286a0c750b8c2eee55df8",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ContinuityAuthorityCurrentBasis|ContinuityAuthorityCurrentBasis<Role>.Local.writer_fence_epoch|084299aa27b15cc16f8e6a51ef83f7e90ba971a6bdc72476ea25ace38bae1be5|1|b931364d7e7d4047b15bd850be65fcb409e866a9bf17f1b6a4455a33a0711679|shorthand field has no exact type",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Local.writer_fence_epoch|writer_fence_epoch",
        ],
        rationale: "a16:2225: shorthand member `writer_fence_epoch` at census path `ContinuityAuthorityCurrentBasis<Role>.Local.writer_fence_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ContinuityAuthorityCurrentBasis<Role>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:87dfaf6e9cc2b8bbde832f18efa9fa3fb950f304d9dd594b8d9e3aa230e9a2d9",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ContinuityAuthorityCurrentBasis|ContinuityAuthorityCurrentBasis<Role>.Meta.current_root_manifest_and_slot_identity|ca00f36878beee32c8a5e1df1ba578220ef65af8b69d26790e83f124f67de5e8|1|4b13ebd23e22b353e2ac7e87f8f3f40fea1fd37f0126b56e43995455c29c78e1|shorthand field has no exact type",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Meta.current_root_manifest_and_slot_identity|current_root_manifest_and_slot_identity",
        ],
        rationale: "a16:2225: shorthand member `current_root_manifest_and_slot_identity` at census path `ContinuityAuthorityCurrentBasis<Role>.Meta.current_root_manifest_and_slot_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ContinuityAuthorityCurrentBasis<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:40f683d4ca2d305185386c3a0779f4a2d359d5d3164c77de96ce2995d4237908",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ContinuityAuthorityCurrentBasis|ContinuityAuthorityCurrentBasis<Role>.Meta.writer_fence_epoch|084299aa27b15cc16f8e6a51ef83f7e90ba971a6bdc72476ea25ace38bae1be5|1|331f4c37256d72366922925461b4827cc93f088c62d36a92059df28533b50d9a|shorthand field has no exact type",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Meta.writer_fence_epoch|writer_fence_epoch",
        ],
        rationale: "a16:2225: shorthand member `writer_fence_epoch` at census path `ContinuityAuthorityCurrentBasis<Role>.Meta.writer_fence_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ContinuityAuthorityCurrentBasis<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ce48b3992c5f79c6151433b0ac626110de8d73a50c9bebde4be1cbcb81201af7",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ContinuityAuthorityCurrentBasis|ContinuityAuthorityCurrentBasis<Role>.Shard.current_root_manifest_and_slot_identity|ca00f36878beee32c8a5e1df1ba578220ef65af8b69d26790e83f124f67de5e8|1|7ff794697f2741e3e009f39bf70e92030883a8dc85f4d28de3c3de9c13181751|shorthand field has no exact type",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Shard.current_root_manifest_and_slot_identity|current_root_manifest_and_slot_identity",
        ],
        rationale: "a16:2225: shorthand member `current_root_manifest_and_slot_identity` at census path `ContinuityAuthorityCurrentBasis<Role>.Shard.current_root_manifest_and_slot_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ContinuityAuthorityCurrentBasis<Role>` `Shard` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f90c20349a918a0e970ca304cc68f78df1783ea71ac6293d72f2fcc573aa80e2",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ContinuityAuthorityCurrentBasis|ContinuityAuthorityCurrentBasis<Role>.Shard.source_meta_prefix_and_configuration|40a2adce98bea5856808f54f68c99eb99b447effb92348d3eab3393b4a2969c4|1|a7afc0a516e9409e5232f80faa7eb73e02358620820ed650a632f58513eec321|shorthand field has no exact type",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Shard.source_meta_prefix_and_configuration|source_meta_prefix_and_configuration",
        ],
        rationale: "a16:2225: shorthand member `source_meta_prefix_and_configuration` at census path `ContinuityAuthorityCurrentBasis<Role>.Shard.source_meta_prefix_and_configuration` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ContinuityAuthorityCurrentBasis<Role>` `Shard` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b7eb51a05d5214237786f8e4895601cd889cd204a9fdbbcc964daaa2d3b8d58b",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ContinuityAuthorityCurrentBasis|ContinuityAuthorityCurrentBasis<Role>.Shard.writer_fence_epoch|084299aa27b15cc16f8e6a51ef83f7e90ba971a6bdc72476ea25ace38bae1be5|1|7324dd2675d6571b6ca1a25ca74174362b84bd8283d4f091526e060bb80a3236|shorthand field has no exact type",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Shard.writer_fence_epoch|writer_fence_epoch",
        ],
        rationale: "a16:2225: shorthand member `writer_fence_epoch` at census path `ContinuityAuthorityCurrentBasis<Role>.Shard.writer_fence_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ContinuityAuthorityCurrentBasis<Role>` `Shard` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0c0b4a16ce9bdbeccf7ec454b8e52e722e171bcf812d4c4413aeb99efccfa625",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.archive_authority_signature|630a95ce21fdf0eff207a3bba353b26c2fb552e499efb57081251e2b231d0158|1|f11a1b03d0c409fb9fb66e44ba9c5cb4f82fe3722da618d6d32423683582112d|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.archive_authority_signature|archive_authority_signature",
        ],
        rationale: "a16:2235: shorthand member `archive_authority_signature` at census path `LeaseWindowSuccessorProof.CrossEpochHandoff.archive_authority_signature` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `CrossEpochHandoff` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b2dd91a2b492e9de178a0dbc850077f422ae4397974f295f38128ee4d3be64a",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.checked_domain_mapping|fa117dfab034779cf32e7ce0296885400f13e6e1c66aee2e25430526ac7714eb|1|0aa118f944603411e30e6a97e7d3a451cbe455e34d4156e5c2c232708fab9d33|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.checked_domain_mapping|checked_domain_mapping",
        ],
        rationale: "a16:2235: shorthand member `checked_domain_mapping` at census path `LeaseWindowSuccessorProof.CrossEpochHandoff.checked_domain_mapping` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `CrossEpochHandoff` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:55f3b93319481ae5515b4979cbc26b3e5cf0fc60b472b45749710d0654a7ac77",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.dual_signed_transition_proof|5ef06ad71fd742f38243b652c5b25c3394bd3ed21e9adf03eeefe45cad9ba50b|1|bbf3c52136cd476714f5283a97a504030d3b6cb01b061413f944b5233fbba881|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.dual_signed_transition_proof|dual_signed_transition_proof",
        ],
        rationale: "a16:2235: shorthand member `dual_signed_transition_proof` at census path `LeaseWindowSuccessorProof.CrossEpochHandoff.dual_signed_transition_proof` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `CrossEpochHandoff` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4604a342b681ed605409625ef352daad3d54bb49066723f90210c5ace15c52dd",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.lease_identity|f99f826b7e8995b00dcf585c7d218e6a5724fca1c2a76501b717ee328cfd10e5|1|d1a128506b190039221dbb50c54a1e501e6dd1c2bae066fac0d2ab4555b0f081|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.lease_identity|lease_identity",
        ],
        rationale: "a16:2235: shorthand member `lease_identity` at census path `LeaseWindowSuccessorProof.CrossEpochHandoff.lease_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `CrossEpochHandoff` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:134d0d4d4be731d4cf4487b042d933a79b058435563c7e9211e3f67d19dc2bfe",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.no_gap_coverage_proof|54cebd5f04b997fb4f3602663147eba44901998934ce981b158316144e8a3e82|1|a11e9f8e32cd0750adbf355e2739cf7f742f3d65a326d30912f51f65b9570cec|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.no_gap_coverage_proof|no_gap_coverage_proof",
        ],
        rationale: "a16:2235: shorthand member `no_gap_coverage_proof` at census path `LeaseWindowSuccessorProof.CrossEpochHandoff.no_gap_coverage_proof` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `CrossEpochHandoff` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e71935c154a5a543fea65cb5dcf070475bffd2fb48f6867e2aeda5bbf4bec6b5",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.old_and_new_profile_identities|381063ee178d952a3fc0e75d22cde76734b080b50b609aa307ec615c83949be4|1|d6e8496819029c6f060ae28e00bb0b089f392df5d6698630ffa43860d7716495|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.old_and_new_profile_identities|old_and_new_profile_identities",
        ],
        rationale: "a16:2235: shorthand member `old_and_new_profile_identities` at census path `LeaseWindowSuccessorProof.CrossEpochHandoff.old_and_new_profile_identities` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `CrossEpochHandoff` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:49fe4ea025fa3b41520aa2c736d9cc92c2ef522a87438d53b4eddce5c15404c8",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.prior_and_successor_generations|32bf996640bd3860db0204d03d6d54cb9b30ba03b362868b97f6fe79f148765e|1|c2d56714d5b73bc91829e2846aa89842a8478bd5e904ca5811b4e468667ff5e3|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.prior_and_successor_generations|prior_and_successor_generations",
        ],
        rationale: "a16:2235: shorthand member `prior_and_successor_generations` at census path `LeaseWindowSuccessorProof.CrossEpochHandoff.prior_and_successor_generations` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `CrossEpochHandoff` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f87519086a8f536dc713ab1365a4bfb83c62192a8d0b5ce8cf81771d2963fe76",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.archive_authority_signature|630a95ce21fdf0eff207a3bba353b26c2fb552e499efb57081251e2b231d0158|1|35dcc275cae276e136ae4c554ad689df94bb44d8ce447ec82163f1986922aa2c|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.archive_authority_signature|archive_authority_signature",
        ],
        rationale: "a16:2235: shorthand member `archive_authority_signature` at census path `LeaseWindowSuccessorProof.SameEpochNonshrinking.archive_authority_signature` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `SameEpochNonshrinking` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8d2ffdea256c376ad5b0006705b27ca6b51ccba5006c908129895cacd22ab53c",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.checked_nonshrinking_bounds_and_counter|b2f8db730f46136fa8ed5eb8e00b107033b7e270446b8eb2101d23a3cdfc2c8c|1|294aae920c882299af719ca63ac9b14203a2a03f14bc054ed890cd72bd08fbd6|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.checked_nonshrinking_bounds_and_counter|checked_nonshrinking_bounds_and_counter",
        ],
        rationale: "a16:2235: shorthand member `checked_nonshrinking_bounds_and_counter` at census path `LeaseWindowSuccessorProof.SameEpochNonshrinking.checked_nonshrinking_bounds_and_counter` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `SameEpochNonshrinking` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fa0cd7bad70d2a11070d4ac7aa07c6e9594bb9a2b1b1420b8b80bfc7457471de",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.lease_identity|f99f826b7e8995b00dcf585c7d218e6a5724fca1c2a76501b717ee328cfd10e5|1|bfcf0c04e713bb7577164ad8dd30526d2ab18473082144c72e137136af718cf0|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.lease_identity|lease_identity",
        ],
        rationale: "a16:2235: shorthand member `lease_identity` at census path `LeaseWindowSuccessorProof.SameEpochNonshrinking.lease_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `SameEpochNonshrinking` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:829355fb5b65191691f8664c311fc856ec13d034e717af269cea446ad14c4216",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_bounds|7a9eac9c366cabd42ac6c78a6e700575fb8b8753687b6d2f867eeb7f3eb83f65|1|28b0fa6a90948bcd0db096826db6086419c94468be8e087924a0faa882fc8a5c|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_bounds|prior_and_successor_bounds",
        ],
        rationale: "a16:2235: shorthand member `prior_and_successor_bounds` at census path `LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_bounds` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `SameEpochNonshrinking` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8ebc993bcd786c4f58842b7f4a0482a6606d5a09b3569a869089a37f43916d4a",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_generations|32bf996640bd3860db0204d03d6d54cb9b30ba03b362868b97f6fe79f148765e|1|929ae095b851b5be90fc86273d9807d1e7f6cc79d4aa618acb413147c773762a|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_generations|prior_and_successor_generations",
        ],
        rationale: "a16:2235: shorthand member `prior_and_successor_generations` at census path `LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_generations` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `SameEpochNonshrinking` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:85dbeb0516fa252e9be5a5de75cd32741741e2125a82a450fc8729828675599e",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.same_profile_domain_epoch|a0642ee43509d4a33780cd018b20c6ed58751e201a6334393a905dd65b9ca1c0|1|a1a813eb53db658568e9d58b89ea5810e869a93012423dd5a8286373bd9cb19d|shorthand field has no exact type",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.same_profile_domain_epoch|same_profile_domain_epoch",
        ],
        rationale: "a16:2235: shorthand member `same_profile_domain_epoch` at census path `LeaseWindowSuccessorProof.SameEpochNonshrinking.same_profile_domain_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `LeaseWindowSuccessorProof` `SameEpochNonshrinking` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:631e05ec22d5bd0bd47535c6abea08a9afbf9bc2da7cf2fa67d12ee696cdfcb7",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claim_id|28f7247f6ea57b4f8368f42c08fe392559ec182a717dfbbaedac6a548058d137|1|adae3088872b736da00228ebb0783256c286fa39bd7de46519e74f6ddeb1548f|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claim_id|claim_id",
        ],
        rationale: "a16:2245: shorthand member `claim_id` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claim_id` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:684dd613cf8d1b1d36038b3422de453aef92d6186e5e77525f46313e3752d1cb",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claimant_identity|22894119e49df78f411ad0b3f0aa2c2f7517513c1e9937b92bc2792ff9e957c2|1|4354df61cd0895b48efa0d52cedc9d35406c4e8ef92ad472ca39fbbb785ed042|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claimant_identity|claimant_identity",
        ],
        rationale: "a16:2245: shorthand member `claimant_identity` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claimant_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:351b5f727acb44a2cc22dcf886346f73878a40a476ba55b19266bc15b0ead3b8",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.directory_bound_creation_evidence_recipe_digest|10d80d0f683a8963bf219ec8e81bdd7fc4af0e5d38030ee58d6c2ef8c1592da0|1|b633776d2372e68c5efc1237267f447a788f0bbfe6810c91ec50cab23b2b4cd9|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.directory_bound_creation_evidence_recipe_digest|directory_bound_creation_evidence_recipe_digest",
        ],
        rationale: "a16:2245: shorthand member `directory_bound_creation_evidence_recipe_digest` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.directory_bound_creation_evidence_recipe_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d51096cf7233f6066f047ab25034303514696d9f1745cd29920e9630706a3a7d",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.filesystem_profile_id|c6d610525410c1e89be47fce9fbf0dd191200c7f4e12883bb82c0d655c8f67d0|1|13bf7c36f238e29318228addafd641505001675449763f7b778bec04e2ff00e7|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.filesystem_profile_id|filesystem_profile_id",
        ],
        rationale: "a16:2245: shorthand member `filesystem_profile_id` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.filesystem_profile_id` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:db72c08571d804730fdad3aa7a4fee4ad481a4e9670b5c46439cd44392a7c819",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.fixed_local_continuity_recipe_and_digest|9cc083ecb3ef04318db43777b92d8c3cdd5c0be3ccc371b83161bae08fc223cd|1|195c0b1dc8b9617a9450226049e25b20f21cf96e928e70627f8c68fc0cfad57e|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.fixed_local_continuity_recipe_and_digest|fixed_local_continuity_recipe_and_digest",
        ],
        rationale: "a16:2245: shorthand member `fixed_local_continuity_recipe_and_digest` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.fixed_local_continuity_recipe_and_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:59e07a8fce6020b3269432fb6b921c8987cb1938688030616a2f5a342f2a62d5",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.mount_device_directory_and_target_manifest_inode_identity|73ff157ae04225cdd50b8aeac29463bbaf7fc5253a187ee3b8e9635fb4f75607|1|980af3db635dda436320f47a44e86468901a4ca8c25a54c98514531e4584a109|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.mount_device_directory_and_target_manifest_inode_identity|mount_device_directory_and_target_manifest_inode_identity",
        ],
        rationale: "a16:2245: shorthand member `mount_device_directory_and_target_manifest_inode_identity` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.mount_device_directory_and_target_manifest_inode_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:66fda3d55f20ffbd14f2a44e4671916cb011e2911f4415fc6b7878dbc49d8df2",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.parent_directory_durability_recipe|5e813fa99dfc3788500db837776d4cd0322cc7024c1126089d59ed6c805353ab|1|b467e6a8bb7944b8540a1b4c80a2cc73ec7c540b036e94f822289011762e4dc4|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.parent_directory_durability_recipe|parent_directory_durability_recipe",
        ],
        rationale: "a16:2245: shorthand member `parent_directory_durability_recipe` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.parent_directory_durability_recipe` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:710de412e21d880e486ff97a204979df9b5f70a5b586e886d210c20c4617b4e6",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.target_manifest_staging_inode_creation_nonce_digest|d7d9a0b435248b3010c90a9eeb7bd8d87d18c2d4ee3e2c0839eeb1de49cd48f6|1|be9ee00a04b26d6810e8c8c223c02910df460b3cf88ed337c1155b7724592e90|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.target_manifest_staging_inode_creation_nonce_digest|target_manifest_staging_inode_creation_nonce_digest",
        ],
        rationale: "a16:2245: shorthand member `target_manifest_staging_inode_creation_nonce_digest` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.target_manifest_staging_inode_creation_nonce_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3cb12c928a14326b5ba0c022926f4558be38a6ff547679081daf0d3e498163d4",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.whole_inode_fence_identity|843c779a7ccc145699aaaef02185f99597de5f95aacf04a7b11a99cb0a65203f|1|5d52c54efe1f2155d1027f7b8c675ac4dd3b99d8099dc924e56cf9823239dd1d|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.whole_inode_fence_identity|whole_inode_fence_identity",
        ],
        rationale: "a16:2245: shorthand member `whole_inode_fence_identity` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.whole_inode_fence_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:31ee4abcd164b58e192c76a563d2ddb710c49d224988e73ec9356854643b5400",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.zero_existing_root_slot_proof_recipe|c46cec78770d95b98ef47da5932cb4cf50d4641a3dcc8b0e626220e9ee2f4b76|1|d40f9fb2a94cf9beeb17b35b054305425897f185e81bd53a63ce17cc98885be1|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.zero_existing_root_slot_proof_recipe|zero_existing_root_slot_proof_recipe",
        ],
        rationale: "a16:2245: shorthand member `zero_existing_root_slot_proof_recipe` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.zero_existing_root_slot_proof_recipe` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3c53ffd2cdd6a62ddcbd58a34a2a90572ba753ab1df0b4b3e713f47ca11af4e3",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.enter_clone_reserved_operation_recipe|a0a095fc7bb1c73ca8c09981086c1263e0ab2cfb74c23c0aecdf2b76924a3b51|1|8518425ffddcd4193149eb48cf3b85b72b0c370f62155703eea2616550529cad|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.enter_clone_reserved_operation_recipe|enter_clone_reserved_operation_recipe",
        ],
        rationale: "a16:2245: shorthand member `enter_clone_reserved_operation_recipe` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.enter_clone_reserved_operation_recipe` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9412f6cc59223ce1a66a34d89a55e685ef653105e6973d6c11215986ca6b9ca0",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.expected_continuity_predecessor_digest_and_cas_version|81f354285ab3e55056286a7644dcff56ee3f4d591b1f47ec5dd1058a0bb80dbc|1|172676fcc66dfcca3477e6822a5901932fe3ec64d3a4f84a1bc23509132109b6|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.expected_continuity_predecessor_digest_and_cas_version|expected_continuity_predecessor_digest_and_cas_version",
        ],
        rationale: "a16:2245: shorthand member `expected_continuity_predecessor_digest_and_cas_version` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.expected_continuity_predecessor_digest_and_cas_version` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8c4b21b6111a8db073ffe5a7a4eedc10e207ce1654f25df674e05a2de987125a",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.recovery_only_lease_recipe|49345734dd62b27c3736a470044afa490bd85d9c3d8d422b7bdfbacc07153ff8|1|3a406404b59fd8a0edca706f81dfb5424b7ad795a22f7ca55a1cb6be96807e45|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.recovery_only_lease_recipe|recovery_only_lease_recipe",
        ],
        rationale: "a16:2245: shorthand member `recovery_only_lease_recipe` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.recovery_only_lease_recipe` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:01d951dd11d6fdaa8d31623cd9fd3539f67d2fcfbbdf285096fe10e57dc62baa",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.expected_reserved_head_digest_and_cas_version|e1af7c8ae26f2c9ccd4dadfe9ad09769938edcc771c700959fce52e11fcf3813|1|aebad033551be000e0f09d57b22664c9099bd86fadf975c48326501e952e412e|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.expected_reserved_head_digest_and_cas_version|expected_reserved_head_digest_and_cas_version",
        ],
        rationale: "a16:2245: shorthand member `expected_reserved_head_digest_and_cas_version` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.expected_reserved_head_digest_and_cas_version` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:1c12d7fe225198c1beef3b67c21b25d50ce94eb39c141f0000aed764d117f1f6",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.identity_reservation_id|1b3b785a538bba87227cdaf20318995dd69dccc219a21ce926861f90f773be6f|1|5913ee80784fdfc7106b5f21841361c7bd24b5c671c5612de0de5cdd4648d92f|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.identity_reservation_id|identity_reservation_id",
        ],
        rationale: "a16:2245: shorthand member `identity_reservation_id` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.identity_reservation_id` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f6359a728522f27ef940fb7dd42aaf5347f9d0c6abe54b411fd0830de7ff3166",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.recovery_writer_authority_digest|e052491427ae4e9cdcdf9dd58fdb7f7187c9d95782ab9faac12a1ba95706ef1f|1|41f6fc2013bfd05360dc42486d9cc032a71f2da1562b87e2c509418a01808d11|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.recovery_writer_authority_digest|recovery_writer_authority_digest",
        ],
        rationale: "a16:2245: shorthand member `recovery_writer_authority_digest` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.recovery_writer_authority_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:cdaa27bf799002729bb11c5025f5d7e89547de30c3d1a4d0d837d82f9033eda4",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.service_visibility_epoch|671bd793460b210d0e24a28527a99986582b2db1e22c53b3263a2bd250df9881|1|1ae844382f56c8c18e0bf3702d8f66d242d1557a4305833109dceccd6db3b379|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.service_visibility_epoch|service_visibility_epoch",
        ],
        rationale: "a16:2245: shorthand member `service_visibility_epoch` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.service_visibility_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0560e2a9ebe97b9c931583bcbf356f270ce83d735769b1d8b38f2868a68ec1c1",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_database_and_security_identity|eda1f9dc030a6db3a97b0ecf0097a2f935a9bdff94f884c14c8b7f12345aaf5c|1|7471f870cdc36971331b18b8def9189dc0dc61183965458602ac09e5d495ab69|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_database_and_security_identity|target_database_and_security_identity",
        ],
        rationale: "a16:2245: shorthand member `target_database_and_security_identity` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_database_and_security_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:6343dff659cb84cb85d6b0f03a664cd385bd0c8157c51f995279634d6d013393",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_incarnation|774c4bf2a28f126d2350fe40b5e43a0fcb2e0422a00d0276c5e4a391319bba78|1|89733f2134b5308039fd858917d1388f4fb037da715a21abd5639fac26f9a791|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_incarnation|target_incarnation",
        ],
        rationale: "a16:2245: shorthand member `target_incarnation` at census path `RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_incarnation` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `CloneNewIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fa8524d7c15de0f18cd494343c8fcb407f67fa565408d7adbe01ef387b1b7a93",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.expected_operational_or_fence_predecessor_digest_and_cas_version|1ee2c302d97a016a367680975958a28a3c10dc5ccbdbbc6be31cf15026e0cc29|1|18dccc1413a855510fca8be0a81cd07e0c451056689cdf82b950fe4de07bc454|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.expected_operational_or_fence_predecessor_digest_and_cas_version|expected_operational_or_fence_predecessor_digest_and_cas_version",
        ],
        rationale: "a16:2245: shorthand member `expected_operational_or_fence_predecessor_digest_and_cas_version` at census path `RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.expected_operational_or_fence_predecessor_digest_and_cas_version` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `RecoverSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e8bf7c31fe0a21de42bdf6e964bcf7763272c4fb6b7baedbab94cbee0e13750e",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.planned_target_incarnation|673af9348a4b17978b49f868a6102284b005c09f80c886827309c822aa2dc6a0|1|d1c0d0fdc035ba837d0305c8853a098c92f5ee7b20a5c0f7cbb7d680dfe55e49|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.planned_target_incarnation|planned_target_incarnation",
        ],
        rationale: "a16:2245: shorthand member `planned_target_incarnation` at census path `RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.planned_target_incarnation` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `RecoverSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b5b4e394a7eedc0c7561d3bd52ecaa6bf8a74810759f105c9e56e85f6a012d6",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.prior_lease_fence_plan_commitment|f161d7399775084c4dbc5cbe0ff7c52bcaaa5a6e8b8eb8813d3c4aca5f2782dd|1|f08ab20ab526ca37cc613797ed549f36653fdd7e2311cca0993c8803b699b362|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.prior_lease_fence_plan_commitment|prior_lease_fence_plan_commitment",
        ],
        rationale: "a16:2245: shorthand member `prior_lease_fence_plan_commitment` at census path `RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.prior_lease_fence_plan_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `RecoverSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:44c745c9b2fed3ed6f450d0c31b581e73b86745cdf4d0533e4092c82e0ce5365",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.recovery_writer_authority_digest|e052491427ae4e9cdcdf9dd58fdb7f7187c9d95782ab9faac12a1ba95706ef1f|1|76f3df84517ff6240ec6d273d8db9c4a44a9ab3603f469a58ae95713f0ecb556|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.recovery_writer_authority_digest|recovery_writer_authority_digest",
        ],
        rationale: "a16:2245: shorthand member `recovery_writer_authority_digest` at census path `RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.recovery_writer_authority_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `RecoverSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9e773b8e490ee9a2dc1d72cd77ac727e2912b017c4009938b3e0ee2515f5f6f1",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.restore_id|4da0f3516fe3e06cbf41b72a22562f1cabb1146cebb7995204a4fc5d12784615|1|81ce31813ef48da639e4625f17df34ffe60c380fb417a5b856a52b59a96f0cbf|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.restore_id|restore_id",
        ],
        rationale: "a16:2245: shorthand member `restore_id` at census path `RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.restore_id` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `RecoverSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4cac0529f2c92be614e26bf98adfa7d72d02a2da52f9875469ed1f10a5e71217",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.service_visibility_epoch|671bd793460b210d0e24a28527a99986582b2db1e22c53b3263a2bd250df9881|1|875578f0e7445a5c310cb349219ec40857ffe50e3ba8e60fe99cb54b57597b5c|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.service_visibility_epoch|service_visibility_epoch",
        ],
        rationale: "a16:2245: shorthand member `service_visibility_epoch` at census path `RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.service_visibility_epoch` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `RecoverSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f32accebd587a2bdf96b2c104ac49bbd63086b664565bf3632aa97cd46327239",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.source_backup_identity|a54b70332d95af1f6e3a6d7bdcbf630a8ccce80fa23e44cc6af9795971a6a10e|1|e3b3c2a61c4ff3e2ace17610daeef8c58d84db75320989cccd49802e64101b3a|shorthand field has no exact type",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.source_backup_identity|source_backup_identity",
        ],
        rationale: "a16:2245: shorthand member `source_backup_identity` at census path `RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.source_backup_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreClaimedTargetAuthorityRecipe` `RecoverSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:eed1694e00ba2fe12597870d419b8963e8ab25f286676fac24006a93fbbf3354",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.distinct_namespace_proof|d50e1bc878f9ff347d98be395d9a24ec51f5190e504cd318f235b8f8da999e23|1|7aade153e952af046b14ac61e1bce459cf2f1f1457a908823ff2b861856992de|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.distinct_namespace_proof|distinct_namespace_proof",
        ],
        rationale: "a16:2243: shorthand member `distinct_namespace_proof` at census path `RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.distinct_namespace_proof` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `ClonedFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8b134d2ad5fc8b1cd4eef543a1b695b76ead1ab868350cb12a1814eaf13fcee0",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.plan_digest|236c09445ed8b2a5d8e92431c48cb4cb99d9f3ce7a79d9063dfe95e138ff2f3c|1|7920059e09e47ba78795e81c7a5522dc5452555be9b35454a9048057be65e7ad|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.plan_digest|plan_digest",
        ],
        rationale: "a16:2243: shorthand member `plan_digest` at census path `RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.plan_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `ClonedFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0ebce0aff84aa60ca53776c1924be86270c3f530e73ccc305f4289d3e2282d66",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_k_oid_source_open_only_commitment|b90e1eb0aed4ab2a470ebcab664ec6b2c216047a5e741061d9b2fa707a214bcd|1|bd8240147ebc1c763641f476ecdc8986da01601ba024a84b22395e616163f15c|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_k_oid_source_open_only_commitment|source_k_oid_source_open_only_commitment",
        ],
        rationale: "a16:2243: shorthand member `source_k_oid_source_open_only_commitment` at census path `RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_k_oid_source_open_only_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `ClonedFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f79e5b28c484822b5d2c57cedff5b89df5eacaab7356c3354811dc31d886164b",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_key_nontransplant_proof|46326db0859a64028a81987f1478bbfa0882f68c441f4c8dfc40c8cd75e56d4f|1|5dc73fc7a126fff2c455e21ff3871cbe7f85cdfe9c1c9541cfa3f288279653c2|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_key_nontransplant_proof|source_key_nontransplant_proof",
        ],
        rationale: "a16:2243: shorthand member `source_key_nontransplant_proof` at census path `RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_key_nontransplant_proof` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `ClonedFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4b15f18ca33a6bbd9618b4d86f6433d4edd1017e5aee708689ecffdf6fa71353",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.target_k_oid_commitment|d9609f2d5f7e57bdf0b886b386bce9719b2450659060e1226759e74a059dc92a|1|2e8733fb65cfbde1039597367eebb3555129cfc9c9e7e9e3241be21e9f854b84|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.target_k_oid_commitment|target_k_oid_commitment",
        ],
        rationale: "a16:2243: shorthand member `target_k_oid_commitment` at census path `RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.target_k_oid_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `ClonedFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:aa8fc751eab932565dddac78db025e788a38646928ef0d5fdaf24f1e33f2f562",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.identical_namespace_proof|3dcab183d6b87691cf26b46fe4c57a4d4dde8f66a0dcf707948acf2eab59dfe1|1|f718fd8e6d212706e52581aa47a7fafb4481946c067f4b06214a2635ba0128ac|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.identical_namespace_proof|identical_namespace_proof",
        ],
        rationale: "a16:2243: shorthand member `identical_namespace_proof` at census path `RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.identical_namespace_proof` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `PreservedSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4f9b9e06323cb762ffcac15cf6a71b19ae9f4bc981caf5ffbf4acbdb5d8c7509",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.key_equality_proof|6da8577ed7c5099063d9b8ce4f61f016626d7020815725c6039a4f1f691bef15|1|4494adebc596a9f84259cb589b6c001934bd20953df26d81c2490af32c50a10a|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.key_equality_proof|key_equality_proof",
        ],
        rationale: "a16:2243: shorthand member `key_equality_proof` at census path `RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.key_equality_proof` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `PreservedSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:a06189964678768435533d90ee460e107015578db70cd22f78b6b892f3f56974",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.plan_digest|236c09445ed8b2a5d8e92431c48cb4cb99d9f3ce7a79d9063dfe95e138ff2f3c|1|4222634d8d95c9e0bd872510143ccc60b8eb9109b75c23a2a84d383312a8d85a|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.plan_digest|plan_digest",
        ],
        rationale: "a16:2243: shorthand member `plan_digest` at census path `RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.plan_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `PreservedSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9560b9c40af5f326f253491aafff7807c693ff33021e16b3b17b466c128224d7",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.source_k_oid_commitment|44c6f825d2a928b8cefe33d3c8bdedea518934e75fed02fadf60c94d95159471|1|85b114c49c331054d581eb1ab4522219aed9b1c755f7b3de8e825b24d4506c35|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.source_k_oid_commitment|source_k_oid_commitment",
        ],
        rationale: "a16:2243: shorthand member `source_k_oid_commitment` at census path `RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.source_k_oid_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `PreservedSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:733122f0708df91db6da6ba6643b262794c139f839bf00461e87caee8c9e32e8",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.target_rewrapped_k_oid_commitment|19de999a5cde7dd52e999f89bcac67cac9ce47b42a7ccaf63e882819177eceda|1|3f991ab5fbe972cb862e4f1e3e5a31d0846ed00e468d9c86ae24c52b2058608c|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.target_rewrapped_k_oid_commitment|target_rewrapped_k_oid_commitment",
        ],
        rationale: "a16:2243: shorthand member `target_rewrapped_k_oid_commitment` at census path `RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.target_rewrapped_k_oid_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `PreservedSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2f946d8a8ebb11b3383ac3c166032aa77d9e95a55135417bea7fd2ee2ac13b34",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.zero_plaintext_persistence_proof|17fd8590a299277bc9c414870fdd930e0a98078eb68b5c701d4fcd5faf4ef656|1|c08aa18277953a6d75ede5e988dea38a833408aedd614da1267c089ee69e88f1|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.zero_plaintext_persistence_proof|zero_plaintext_persistence_proof",
        ],
        rationale: "a16:2243: shorthand member `zero_plaintext_persistence_proof` at census path `RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.zero_plaintext_persistence_proof` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyDispositionEvidence` `PreservedSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f2a2e6ed9f1043d3703e55e55632f3435d305711b0cc99e87d1e620885911483",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.create_target_k_oid_operation_recipe|1fa886e03f1e7b0fb39dddebee092f2b49ed1f14b3dec04d795b211628def3e5|1|37d6720e9518ce46f19d67134b6644e62ae3ff4ea6d127fbee1122ba4a3b7383|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.create_target_k_oid_operation_recipe|create_target_k_oid_operation_recipe",
        ],
        rationale: "a16:2243: shorthand member `create_target_k_oid_operation_recipe` at census path `RestoreIdentityKeyPlan.CloneFreshIdentity.create_target_k_oid_operation_recipe` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `CloneFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f3d13d2ca5b4dd2c2112c77da17209411b75ce69ccd1af10cbce488650a94d46",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.distinct_namespace_basis|958ad3f10b8429cd4ac0e9cdd6db0119cc9e994ed8792b8c8443aac2e7928894|1|78ee392b466ae9a026b2286f9843329fe21c512b2dd107620b8935dcb055e4d1|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.distinct_namespace_basis|distinct_namespace_basis",
        ],
        rationale: "a16:2243: shorthand member `distinct_namespace_basis` at census path `RestoreIdentityKeyPlan.CloneFreshIdentity.distinct_namespace_basis` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `CloneFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b0f6e5b9b71c069852886e98c69a257f258d0d87bf695be73b35d6cb8c223713",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.recover_source_k_oid_for_decode_operation_recipe|62ad006ef188f5e9b1e5f6b549bf29411a5e56312d661d3a0949e2603ce5404d|1|f31584d20d8a4852ebe5707528b91e50d67d00d7afe5de3b8bea93c461e3b20b|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.recover_source_k_oid_for_decode_operation_recipe|recover_source_k_oid_for_decode_operation_recipe",
        ],
        rationale: "a16:2243: shorthand member `recover_source_k_oid_for_decode_operation_recipe` at census path `RestoreIdentityKeyPlan.CloneFreshIdentity.recover_source_k_oid_for_decode_operation_recipe` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `CloneFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:30dc167c4493d4c51e751a3d7e77af70c9c9a0e457eaf00ba57fff01dc0d235d",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.required_source_key_nontransplant_proof_profile|76f29b9213d9a915933579071b70076737f1ec9604e646be4eb985bbef67152f|1|d0fc7259267e8fe6c0e6f79d931a7b82dd8b80f86fa75ef9ea357892b35658e2|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.required_source_key_nontransplant_proof_profile|required_source_key_nontransplant_proof_profile",
        ],
        rationale: "a16:2243: shorthand member `required_source_key_nontransplant_proof_profile` at census path `RestoreIdentityKeyPlan.CloneFreshIdentity.required_source_key_nontransplant_proof_profile` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `CloneFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:425f9c6202f370aebe6ca60eb1c8ceed11acfec76fd0d431e97a1ffd9684f5bb",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.source_k_oid_source_open_only_commitment|b90e1eb0aed4ab2a470ebcab664ec6b2c216047a5e741061d9b2fa707a214bcd|1|cd43247cf3084aa0020922e777f1027a96c6ad4ebdfee79cec1da4cbbef6b433|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.source_k_oid_source_open_only_commitment|source_k_oid_source_open_only_commitment",
        ],
        rationale: "a16:2243: shorthand member `source_k_oid_source_open_only_commitment` at census path `RestoreIdentityKeyPlan.CloneFreshIdentity.source_k_oid_source_open_only_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `CloneFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4408fe8489bf3b8a56e92eddbd6a52f360583c3715cba3b3f83fb452245dd9aa",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.target_k_oid_recipient_commitment|df8d130d09b0aece1dd9534d3110856f9a7b7e730756b239308edf8890539963|1|318c57e562377bb4bb282cba768aa8b365c1ed984d8f886c390a2855dfad4e4a|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.target_k_oid_recipient_commitment|target_k_oid_recipient_commitment",
        ],
        rationale: "a16:2243: shorthand member `target_k_oid_recipient_commitment` at census path `RestoreIdentityKeyPlan.CloneFreshIdentity.target_k_oid_recipient_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `CloneFreshIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:428f4a28cda62c0d2fe400327ee5371384d435132bac156a1160119d2ba2adf9",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.identical_namespace_basis|e230d138e7fc28d75f1285d6da2c2a2036ac8dff57f55efa94f0285e8e4c0ba9|1|125863a16aff9edd2a7fb2f0d273b9ef598a8e7ee1f2ecbbd4deff034c6e5e19|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.identical_namespace_basis|identical_namespace_basis",
        ],
        rationale: "a16:2243: shorthand member `identical_namespace_basis` at census path `RestoreIdentityKeyPlan.PreserveSameIdentity.identical_namespace_basis` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `PreserveSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:5808d0f941f3989cdc6937d82303a4f64634837f2be819f4ae0151e111a5b1de",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.recover_and_rewrap_operation_recipe|31897f30895da2f1e1e336645ffad9ca28b97c4c8c96b3f4516f06427fe5e0c9|1|a8f8e2414ed7e37e89d33a0cae0ee76caf0164ca294aa15038693e7e925a98a3|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.recover_and_rewrap_operation_recipe|recover_and_rewrap_operation_recipe",
        ],
        rationale: "a16:2243: shorthand member `recover_and_rewrap_operation_recipe` at census path `RestoreIdentityKeyPlan.PreserveSameIdentity.recover_and_rewrap_operation_recipe` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `PreserveSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:31cc6283f91ff4bf1c4175dfa89f4f612aa184914b4412d0aa7375a884ae6199",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.required_key_equality_proof_profile|adec260fba8b296cadad2f1929120d68d9f5654a2eb4e5c8574101ac0976db53|1|d2e641bdfbe5719c2e19c987da02ffdf8879e7e98fea4ee39cbac4505bd522fe|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.required_key_equality_proof_profile|required_key_equality_proof_profile",
        ],
        rationale: "a16:2243: shorthand member `required_key_equality_proof_profile` at census path `RestoreIdentityKeyPlan.PreserveSameIdentity.required_key_equality_proof_profile` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `PreserveSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:a3cf39ce48c28bd30f39483bfc16e055768701adb3ccdfb88281f1aa8f2811cd",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.required_zero_plaintext_persistence_proof_profile|5fbadce5cbc2cdcfcb13c7f147f0c0ee2e3563077607362aadae43b7a81dd392|1|c89b86f53b227aa3d7cca3976514399913db2d972b4ea6d9943bb475a94dadae|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.required_zero_plaintext_persistence_proof_profile|required_zero_plaintext_persistence_proof_profile",
        ],
        rationale: "a16:2243: shorthand member `required_zero_plaintext_persistence_proof_profile` at census path `RestoreIdentityKeyPlan.PreserveSameIdentity.required_zero_plaintext_persistence_proof_profile` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `PreserveSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:60b0d02f6527985a2ede2d69f167c9ba7a85fc7b418df1e308f9bc9ae5ad4ebb",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.source_k_oid_identity_and_commitment|7680f99c6f0f8ae82b40efe68881c7519e51e1fa000ff0c40398ae980e652164|1|714a72ad8a54697a5c36ebbf1aff7846636b85378e39c53b3105dd720cd28921|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.source_k_oid_identity_and_commitment|source_k_oid_identity_and_commitment",
        ],
        rationale: "a16:2243: shorthand member `source_k_oid_identity_and_commitment` at census path `RestoreIdentityKeyPlan.PreserveSameIdentity.source_k_oid_identity_and_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `PreserveSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:30eb8375a7e592e229c0fd3c2610ca145d0b62e57e3f247d2234b443b3f7b01f",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.target_rewrap_recipient_commitment|e46bd70d6f9d4c2f4120695f90159b8ac7dc7bbb128e76993c965bf64456fc5c|1|c65b06177d37cf0bc406506aaec9c39071ce461316fb48d12a28c349d443d9ec|shorthand field has no exact type",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.target_rewrap_recipient_commitment|target_rewrap_recipient_commitment",
        ],
        rationale: "a16:2243: shorthand member `target_rewrap_recipient_commitment` at census path `RestoreIdentityKeyPlan.PreserveSameIdentity.target_rewrap_recipient_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreIdentityKeyPlan` `PreserveSameIdentity` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:51e1d45595605b09416540d8f2f7bfc577e100c7cfc34d62c3f8d31ea3b4cdf5",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleConfigurationRetentionBasis|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.configuration_set_digest|0cda49d9e490a8fae26b241fb5e5e586d304af3b8c491349b2439992ad8301f4|1|05ca5c32f88c41337d248a8bf6449095bf12a82044b2cfcb88c78a8f854abbe1|shorthand field has no exact type",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.configuration_set_digest|configuration_set_digest",
        ],
        rationale: "a16:2203: shorthand member `configuration_set_digest` at census path `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.configuration_set_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:63fc7261e030989938c367bdb25963327b60d9ea1d9d24dbb85d7d2dbd41618e",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleConfigurationRetentionBasis|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.root_manifest_and_slot_cut|680949f991343776a254515a6ea0035cdaa9bc560298a7fc054389a1b77f7746|1|afc21c90e390dd1a5f7d4d380d2f7325bbe913a53619984f0e92c78393315a23|shorthand field has no exact type",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.root_manifest_and_slot_cut|root_manifest_and_slot_cut",
        ],
        rationale: "a16:2203: shorthand member `root_manifest_and_slot_cut` at census path `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.root_manifest_and_slot_cut` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:216034541841b4dc0a18c6a9056d541edf9220e6baa68bccd57d9ce2d8447dbe",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleConfigurationRetentionBasis|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.configuration_set_digest|0cda49d9e490a8fae26b241fb5e5e586d304af3b8c491349b2439992ad8301f4|1|f1483c57249202c9cfd8204350aecd30e29d17a278b19668967613a8a30af8b1|shorthand field has no exact type",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.configuration_set_digest|configuration_set_digest",
        ],
        rationale: "a16:2203: shorthand member `configuration_set_digest` at census path `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.configuration_set_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:80e7fb9081083f4cc82c9ab584105b308e38afb8978ba93b8c0e9b4a5fa010e7",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleConfigurationRetentionBasis|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.exact_current_and_prospective_shard_configuration_commitment|8b376f600c2a41afbd371efd1b57075ea595f8e35f64047d1722313338e31af0|1|5a52faf182deb28437bfbd39fbd57971d9f161e12d6805baf03adf49eb0c0b81|shorthand field has no exact type",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.exact_current_and_prospective_shard_configuration_commitment|exact_current_and_prospective_shard_configuration_commitment",
        ],
        rationale: "a16:2203: shorthand member `exact_current_and_prospective_shard_configuration_commitment` at census path `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.exact_current_and_prospective_shard_configuration_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:dbb7edba8ea7628b68f176f2fdcf74df26e3460a1f61f937b6ccbe9ce860dd87",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleConfigurationRetentionBasis|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.root_manifest_and_slot_cut|680949f991343776a254515a6ea0035cdaa9bc560298a7fc054389a1b77f7746|1|04b2fe06f05dbe2765af57aad63827b03091dc5f93d46ebc15b90bbddbe0837c|shorthand field has no exact type",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.root_manifest_and_slot_cut|root_manifest_and_slot_cut",
        ],
        rationale: "a16:2203: shorthand member `root_manifest_and_slot_cut` at census path `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.root_manifest_and_slot_cut` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:6dd465c2c940014a013bc003a9e661deadc21487ac6313f05a50cbecea9097d9",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Local.checkpoint_ref|05fa3858ab9fd350ca08724045faa8f627a4d4cf1377528a317f00c5e1d01cf6|1|1a1e706a8faea4c441a0e719f5a8e5f5fdca95c955577b6dc29cd387defbd8e6|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Local.checkpoint_ref|checkpoint_ref",
        ],
        rationale: "a16:2205: shorthand member `checkpoint_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Local.checkpoint_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:902d267153d8579147f02b8f03e991d7c32f9d7209fff31de61ba342ed72baf4",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Local.config_payload_floor_ref|cfc39395925ea45519d4dcc753299ecfb3d1e9cc9e49ed91dc916c77849f3f7a|1|733d6226d8f85cc2cf6af011d1a27d73a1ca43dc98917c3d969aa21d0455a6aa|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Local.config_payload_floor_ref|config_payload_floor_ref",
        ],
        rationale: "a16:2205: shorthand member `config_payload_floor_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Local.config_payload_floor_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:483a3d514dff97d1044b68ac3509def420cffaa13437bc8473568f75218b61ac",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Local.configuration_ref|a65a4cadd73613e4303c434f3bb414d08dd11b5dbaaa75f5a4ac62e20a64ad1f|1|c0ee6d2d010bf5b15d1f560fba5d70bea8d76aa755182ce617f89093d0891891|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Local.configuration_ref|configuration_ref",
        ],
        rationale: "a16:2205: shorthand member `configuration_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Local.configuration_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:1f20479cfd8941a0d1df5dcfce62960e4f5ef9dd4e3d41b65abd770d83777805",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.configuration_ref|a65a4cadd73613e4303c434f3bb414d08dd11b5dbaaa75f5a4ac62e20a64ad1f|1|bc47f7a5d2734a2251489d4c3688d69a533aa6181ab30ed6f4e8fc2ea24bb6f1|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.configuration_ref|configuration_ref",
        ],
        rationale: "a16:2205: shorthand member `configuration_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Meta.configuration_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:88ec0918d92724ea23c58210e0e16c0c04742f08c40f324c78c8a8f139dee4e6",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.global_checkpoint_ref|e92bdb0252bafab8b355f51bca2d32d7c5320794591820cd170304224102ac2c|1|ced483ebfc979d515d9fad8e6896e4ef258569ad175a3537946eb15aeb776344|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.global_checkpoint_ref|global_checkpoint_ref",
        ],
        rationale: "a16:2205: shorthand member `global_checkpoint_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Meta.global_checkpoint_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b15497009f309c87fbd8f21e8d7d8c6242e4ab3b226e7da1e20b5ea465f1348c",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.meta_config_payload_floor_ref|19d180009a4446243748ba40d3c01d8be753c00d0ef118e190891825162e173d|1|52f61347afdb6179b33aaf197a5da02e1852bd9be4f3623363a7349498448939|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.meta_config_payload_floor_ref|meta_config_payload_floor_ref",
        ],
        rationale: "a16:2205: shorthand member `meta_config_payload_floor_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Meta.meta_config_payload_floor_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:14ebc1cef7254371a9290cabc9931aa7b0a06c7cb57527b89d15186776e7ec50",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.shutdown_receipt_ref|5e062f047be71f798471779dc5142d2ee4b066023bc48caf052f84950a8849cf|1|409cdcd90d4b26f3c31675b0612f23ae53a6de9001606ec85ab3f92c445d9af2|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.shutdown_receipt_ref|shutdown_receipt_ref",
        ],
        rationale: "a16:2205: shorthand member `shutdown_receipt_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Meta.shutdown_receipt_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e91e24a51392d6954d25959e9414a9ad41fbb3a557250c1c659af72b8d65fe35",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeAuthorityRetirementFloorSet|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.verifier_retirement_floor_ref|fcc097004f5bcde950130cf491deaf0af6773dbddef8606054f831a65d5d9e9a|1|54097f3877d755d9238d716da9a420f1563d3415dd51619684954c4be29d1d70|shorthand field has no exact type",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.verifier_retirement_floor_ref|verifier_retirement_floor_ref",
        ],
        rationale: "a16:2205: shorthand member `verifier_retirement_floor_ref` at census path `RoleTimeAuthorityRetirementFloorSet<Role>.Meta.verifier_retirement_floor_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeAuthorityRetirementFloorSet<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b16ef0d3622c032199d593c58ccb018188ef9ba2bb7f11a09fc8be0dc80bf2f3",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeBoundSubjectInventoryClosure|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Local.closure_digest|be898c1de4b4b9688762a2507e264cfb2487049c1cfe7ad06bd45ffde5bb24b5|1|3b13ea8ca1453c0c98ea5a2f658781f27cef324b974fd5241ce659c8efcb4882|shorthand field has no exact type",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Local.closure_digest|closure_digest",
        ],
        rationale: "a16:2197: shorthand member `closure_digest` at census path `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Local.closure_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:139736010760224ea01da42ec72d64e00f649760b94b82519832db88c20c87c4",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeBoundSubjectInventoryClosure|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.aggregate_maximum_expiry_derivation_proof_ref|07a3744fa6da65338903116e9515041389f28866083086d02b23664e9299d547|1|c329501a5ff992e20791db0227d90f214d499e4ff1f86c7590eadabe517a7479|shorthand field has no exact type",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.aggregate_maximum_expiry_derivation_proof_ref|aggregate_maximum_expiry_derivation_proof_ref",
        ],
        rationale: "a16:2197: shorthand member `aggregate_maximum_expiry_derivation_proof_ref` at census path `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.aggregate_maximum_expiry_derivation_proof_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ff4db66414518f7f1180ae56f9e0e1f62eff4eea91062e2e1a4e6021735c4525",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeBoundSubjectInventoryClosure|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.closure_digest|be898c1de4b4b9688762a2507e264cfb2487049c1cfe7ad06bd45ffde5bb24b5|1|da5243d65f9ffc2177f8fa46573638f4550fe155f0daef4689493d7efedadffa|shorthand field has no exact type",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.closure_digest|closure_digest",
        ],
        rationale: "a16:2197: shorthand member `closure_digest` at census path `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.closure_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:198d323e4bcdc1582896d21ce07fa233d15a72464c6d5234a96b053e4af44aba",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeBoundSubjectInventoryClosure|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.configured_group_inventory_bijection_proof_ref|51866ee8d9357d4fd64b9127da5fbd94a8809a8c8d8c44aa67c62ee2d70fa2f2|1|7e8838da3e3b43865c321acb2e6878bff9d00cd426ae93294d7cb6ee694d560b|shorthand field has no exact type",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.configured_group_inventory_bijection_proof_ref|configured_group_inventory_bijection_proof_ref",
        ],
        rationale: "a16:2197: shorthand member `configured_group_inventory_bijection_proof_ref` at census path `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.configured_group_inventory_bijection_proof_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:36978d672f27a9404f4f2140e7de7b1f7d3cba5ba7395ba6247431031c873127",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeIssuanceReservationClosure|RoleTimeIssuanceReservationClosure<Role>.Local.own_complete_bijection_proof_ref|23344973f9a2bdffb3407c30c8530a59f41eaead9a449ac62645ff133fda080d|1|d0091f0d9e6ef5506d6e3b510af539c1dc8ba976722f656735d8a23e37c8ddef|shorthand field has no exact type",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeIssuanceReservationClosure<Role>|RoleTimeIssuanceReservationClosure<Role>.Local.own_complete_bijection_proof_ref|own_complete_bijection_proof_ref",
        ],
        rationale: "a16:2193: shorthand member `own_complete_bijection_proof_ref` at census path `RoleTimeIssuanceReservationClosure<Role>.Local.own_complete_bijection_proof_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeIssuanceReservationClosure<Role>` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e6fe389daff9216daf5eae4a5e9f60371cfee06c2285bc5d94e43ae3d62cf9f2",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeIssuanceReservationClosure|RoleTimeIssuanceReservationClosure<Role>.Meta.configured_group_certificate_bijection_proof_ref|ea00986183ced10b0c4c932be3d9fcdeec8140cc9b4a234cece2b02182318417|1|a4f5e7d10a2c3f90e94ce153372119ffa96e2497031530a21ecb05b6aff03711|shorthand field has no exact type",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeIssuanceReservationClosure<Role>|RoleTimeIssuanceReservationClosure<Role>.Meta.configured_group_certificate_bijection_proof_ref|configured_group_certificate_bijection_proof_ref",
        ],
        rationale: "a16:2193: shorthand member `configured_group_certificate_bijection_proof_ref` at census path `RoleTimeIssuanceReservationClosure<Role>.Meta.configured_group_certificate_bijection_proof_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeIssuanceReservationClosure<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:22afadae2a28ad5464ce9a4779f1939a81035f3992249a1a2a7f9c7eddb44e07",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RoleTimeIssuanceReservationClosure|RoleTimeIssuanceReservationClosure<Role>.Meta.own_group_certificate_ref|3a598979eb2a07ce4bdce6e91786a40e0b1c6adca06722a9319b879f6246e1cb|1|9d473b04dce098b78546704b58288c7603fdba8998d0fdbd5f968bade06042b4|shorthand field has no exact type",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeIssuanceReservationClosure<Role>|RoleTimeIssuanceReservationClosure<Role>.Meta.own_group_certificate_ref|own_group_certificate_ref",
        ],
        rationale: "a16:2193: shorthand member `own_group_certificate_ref` at census path `RoleTimeIssuanceReservationClosure<Role>.Meta.own_group_certificate_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RoleTimeIssuanceReservationClosure<Role>` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8fd0bb570ac876054b677e9705401329de512801ee439276dc674a407df02f8e",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Bootstrap.bootstrap_projection_identity_and_digest|6b747fc47dd4d022d466350b7ca40d70a4279220caca45f5eed772a373c5a1b5|1|3c96b4ad15b5652fe5504665e9c9655cb7da93c761b66003b5a938ebb19c938c|shorthand field has no exact type",
        source_locations: &["a16:2239"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Bootstrap.bootstrap_projection_identity_and_digest|bootstrap_projection_identity_and_digest",
        ],
        rationale: "a16:2239: shorthand member `bootstrap_projection_identity_and_digest` at census path `ShardRestoreSourceLeaseProjectionSource.Bootstrap.bootstrap_projection_identity_and_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ShardRestoreSourceLeaseProjectionSource` `Bootstrap` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:42d2a629736e07406ef6cec496247a08477fd4f24713e6b7f51b0756b4f5df63",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Bootstrap.source_lease_projection_payload_identity_and_digest|3655a3f0b2f36011e9f1e78c8922dbd1f657d9e3229abd039c536b91df50e705|1|0d98f387e36a8972ba7d160a13865f34c3c6eda1eebb4eea562163e25029842f|shorthand field has no exact type",
        source_locations: &["a16:2239"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Bootstrap.source_lease_projection_payload_identity_and_digest|source_lease_projection_payload_identity_and_digest",
        ],
        rationale: "a16:2239: shorthand member `source_lease_projection_payload_identity_and_digest` at census path `ShardRestoreSourceLeaseProjectionSource.Bootstrap.source_lease_projection_payload_identity_and_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ShardRestoreSourceLeaseProjectionSource` `Bootstrap` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3662fa006e86638ee2ff3041186200f8ef75e915a62142622b013db772c927b7",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Refresh.typed_meta_projection_payload_identity_and_digest|ae297e5608bddf1b8c170a6a2b2a0f56ee57b3acd485718fadad7fbbc3904fd1|1|6aabf612d6af2673064d1bc72945493c8a51fa273074d0ea4fbadaaaaf38e6fe|shorthand field has no exact type",
        source_locations: &["a16:2239"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Refresh.typed_meta_projection_payload_identity_and_digest|typed_meta_projection_payload_identity_and_digest",
        ],
        rationale: "a16:2239: shorthand member `typed_meta_projection_payload_identity_and_digest` at census path `ShardRestoreSourceLeaseProjectionSource.Refresh.typed_meta_projection_payload_identity_and_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `ShardRestoreSourceLeaseProjectionSource` `Refresh` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:6e7c4fbf5db3c3cf58224f222c31bb95b20241840a3ce0bb89ccb3e664407b8d",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.current_configuration_writer_fence_and_publication_commitment|9da91888d42803314cfcd8361404a1406dde03f633ad34851fabd694f8dcd169|1|79341167f13410bdeb41b8c08eef8fd0f986dc5ac5ae839554b8aca45a8d1d1a|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.current_configuration_writer_fence_and_publication_commitment|current_configuration_writer_fence_and_publication_commitment",
        ],
        rationale: "a16:2165: shorthand member `current_configuration_writer_fence_and_publication_commitment` at census path `TimeAuthorityObservationImport.Local.current_configuration_writer_fence_and_publication_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f84e5724e1fd386f4850343edec20569c16d3b71309a61c85c66fb582b071114",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.exact_profile_registry_and_transcript_match_digest|ecf6e32fd7ebcd04212b1669b4871d79d03fa3544ef4e76594a20be9ebceccd9|1|74e81866fc8b00376d75109fcb1b06bc18765d4dfcada8cc90f1b5f016807eeb|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.exact_profile_registry_and_transcript_match_digest|exact_profile_registry_and_transcript_match_digest",
        ],
        rationale: "a16:2165: shorthand member `exact_profile_registry_and_transcript_match_digest` at census path `TimeAuthorityObservationImport.Local.exact_profile_registry_and_transcript_match_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:eb4c402fb998455475a495a575802f161a3caf327e654e1e2a2c6f0b08b55f15",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.imported_at_maintenance_identity|e2d41158e8ead6d3787feb00193b6cdc80fb410c03f4604bd555e72f01548f13|1|2965d70424673c8853b230556b9be20308fe597c56919eaa616ffacec8153e17|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.imported_at_maintenance_identity|imported_at_maintenance_identity",
        ],
        rationale: "a16:2165: shorthand member `imported_at_maintenance_identity` at census path `TimeAuthorityObservationImport.Local.imported_at_maintenance_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b88dd4fe39941051f81018d2005d684b7e0a9beec3622d0a86fce28dbb6f690",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.role_and_group|49e0c9c79a05b5bfc0e1e4bf80d85509470442f06c703cd7cc24022f86d4b0b8|1|4b3d1c1ade9821279ae138b8574b9372c0bf9d7f83092af49f1066414f18cf07|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.role_and_group|role_and_group",
        ],
        rationale: "a16:2165: shorthand member `role_and_group` at census path `TimeAuthorityObservationImport.Local.role_and_group` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Local` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8b5c7f1c7920620c2661b436a184d102ac583a25c8f6c251e25854579a6cde97",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.current_configuration_writer_fence_and_publication_commitment|9da91888d42803314cfcd8361404a1406dde03f633ad34851fabd694f8dcd169|1|3fba7dced7264442ae9d0b853f2fc58314c76dc628f8b5acb1b52c0eff3f92a3|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.current_configuration_writer_fence_and_publication_commitment|current_configuration_writer_fence_and_publication_commitment",
        ],
        rationale: "a16:2165: shorthand member `current_configuration_writer_fence_and_publication_commitment` at census path `TimeAuthorityObservationImport.Meta.current_configuration_writer_fence_and_publication_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:77cad945943965f6823afc4ce110982fc90b3f1b7bb55d835b30d45bf74ef876",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.exact_profile_registry_and_transcript_match_digest|ecf6e32fd7ebcd04212b1669b4871d79d03fa3544ef4e76594a20be9ebceccd9|1|83da7c0a2095993df1dec28ec04237ca43e8a371447aaf1a1d63e8a410d7fbb7|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.exact_profile_registry_and_transcript_match_digest|exact_profile_registry_and_transcript_match_digest",
        ],
        rationale: "a16:2165: shorthand member `exact_profile_registry_and_transcript_match_digest` at census path `TimeAuthorityObservationImport.Meta.exact_profile_registry_and_transcript_match_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:bbe2ed9b1b6f16c3558175619522d9be6d442f099cc198c0d6b5e8ebcaa55cc0",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.imported_at_maintenance_identity|e2d41158e8ead6d3787feb00193b6cdc80fb410c03f4604bd555e72f01548f13|1|e79c00b8e813d67571b1fff3c775d055853b29d9569dfc51d5181d9c1500bf13|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.imported_at_maintenance_identity|imported_at_maintenance_identity",
        ],
        rationale: "a16:2165: shorthand member `imported_at_maintenance_identity` at census path `TimeAuthorityObservationImport.Meta.imported_at_maintenance_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4b38e383af6cd3b3152ee3fd60ab247bd488ac0fd1921055815d572c6406d0de",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.role_and_group|49e0c9c79a05b5bfc0e1e4bf80d85509470442f06c703cd7cc24022f86d4b0b8|1|fd3172597a295962a6937fabff6eaf22d42ab72717be1bc957e5eeb6ff01d87f|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.role_and_group|role_and_group",
        ],
        rationale: "a16:2165: shorthand member `role_and_group` at census path `TimeAuthorityObservationImport.Meta.role_and_group` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Meta` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8682f99fbb0811dea1a3c1ff717c7f6e4692127e146efa48a985de59e93214e2",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.current_configuration_writer_fence_and_publication_commitment|9da91888d42803314cfcd8361404a1406dde03f633ad34851fabd694f8dcd169|1|8a1755593fa83283f6c8858aff2c9b836247476cf5f7aa2b3182482407ea2b23|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.current_configuration_writer_fence_and_publication_commitment|current_configuration_writer_fence_and_publication_commitment",
        ],
        rationale: "a16:2165: shorthand member `current_configuration_writer_fence_and_publication_commitment` at census path `TimeAuthorityObservationImport.Shard.current_configuration_writer_fence_and_publication_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Shard` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:53da05c0fc82cf4701fb292f9907292c9088e287c6779e836840352920925ddc",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.exact_profile_projection_and_transcript_match_digest|a24c38beb3b24ab4b92ccd61ace15d53aedaa62d7b8931fca34582af557c1eec|1|6e72dcccfa76d6d4747c1b4de4684d16690f5ec18d5925467b7ecf44aed53c59|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.exact_profile_projection_and_transcript_match_digest|exact_profile_projection_and_transcript_match_digest",
        ],
        rationale: "a16:2165: shorthand member `exact_profile_projection_and_transcript_match_digest` at census path `TimeAuthorityObservationImport.Shard.exact_profile_projection_and_transcript_match_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Shard` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:300d344bf5abdbd2ba3786dcb2f2101f0f44a7aef6794b192d392bc0d5f8e2ad",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.imported_at_maintenance_identity|e2d41158e8ead6d3787feb00193b6cdc80fb410c03f4604bd555e72f01548f13|1|ba800d96bd139a5c8c04aa47e6f47f81f92919a7fec8b99aa5ac3b6b30f7c642|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.imported_at_maintenance_identity|imported_at_maintenance_identity",
        ],
        rationale: "a16:2165: shorthand member `imported_at_maintenance_identity` at census path `TimeAuthorityObservationImport.Shard.imported_at_maintenance_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Shard` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:60b6f049cbffd66661578e441fc181a08f56c8a19c041eea7846ee8e40b70c48",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.role_and_group|49e0c9c79a05b5bfc0e1e4bf80d85509470442f06c703cd7cc24022f86d4b0b8|1|c6d60c656d1f90f8f2144efb16b7b5da291468265a22f08bc7b5944174ef314e|shorthand field has no exact type",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.role_and_group|role_and_group",
        ],
        rationale: "a16:2165: shorthand member `role_and_group` at census path `TimeAuthorityObservationImport.Shard.role_and_group` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeAuthorityObservationImport` `Shard` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0e6c35f78172a67ce5d4652dfa17cca9cfd5c6d68b64ab0aa9f0dccb40305910",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectDisposition|TimeSubjectDisposition<Role>.Reissued.successor_subject_key|223b4a0780c0d5230921df4638a3391104fb9124152ca239ad1c09ed070e4dc7|1|3625e441f178a4fece58be8844ae636cc904ca136412210400d9892582b53aa6|shorthand field has no exact type",
        source_locations: &["a16:2201"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectDisposition<Role>|TimeSubjectDisposition<Role>.Reissued.successor_subject_key|successor_subject_key",
        ],
        rationale: "a16:2201: shorthand member `successor_subject_key` at census path `TimeSubjectDisposition<Role>.Reissued.successor_subject_key` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectDisposition<Role>` `Reissued` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:c39a49d58a1e4f8383dcd973f24f1542a19b8b3a35edcfbb27e8ad08faade723",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.portable_expiry_attestation_identity_and_digest|0e22bc1c37d7bd636722a290650acc760a2169ba3607e6b7466c94d0f860a3fc|1|1fe3c2bff4f80817fad8520b0d00badc0cc06ff074037ade63fa015001932de0|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.portable_expiry_attestation_identity_and_digest|portable_expiry_attestation_identity_and_digest",
        ],
        rationale: "a16:2209: shorthand member `portable_expiry_attestation_identity_and_digest` at census path `TimeSubjectTerminalProjection.Expired.portable_expiry_attestation_identity_and_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Expired` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9e665016cb7a45f5ae6e4d68bb6455ff508789fe83d9e40fe4b0a599dd71bad6",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.subject_body_and_profile_digest|0bd12997bb8d9fdbaf826b28ece45fa3b787ea37f15737c8fdbb3eb002e9ba82|1|3d6fa7b21f271bbd0bfd0fda91552a00413df1b4d0eb4f2e50f7084495937aa3|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.subject_body_and_profile_digest|subject_body_and_profile_digest",
        ],
        rationale: "a16:2209: shorthand member `subject_body_and_profile_digest` at census path `TimeSubjectTerminalProjection.Expired.subject_body_and_profile_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Expired` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:439880508061ce9206720a2ce11022658a71030cc4f8d519a52415f1aa558123",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.subject_key|bcaa270cecc5c1dd9d8b1edc2218b3d91ddbc51e9f8a751535af1fcc83bc7013|1|011b5f8d5d69cc053b78dba8847ecf7a60ac9c35e5673a36274155b7e2a5bbaf|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.subject_key|subject_key",
        ],
        rationale: "a16:2209: shorthand member `subject_key` at census path `TimeSubjectTerminalProjection.Expired.subject_key` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Expired` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:1df3c1bc531c4e2e02d5d3114aaf64b9f3a26aad276f789f75bde2445bca7cc7",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.terminal_authority_commitment|be6a50f42d2083e950fa3ced6469243132be120727c64dbf25b1426f63ff8008|1|07974519074a7a7c5a62762da80c4cef5739d366f84b7c4d4876d08a51f8c68d|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.terminal_authority_commitment|terminal_authority_commitment",
        ],
        rationale: "a16:2209: shorthand member `terminal_authority_commitment` at census path `TimeSubjectTerminalProjection.Expired.terminal_authority_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Expired` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3235f879a698f52d63298366de7f6f35bea2adef2d55058df0350648a94af466",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.nonwidening_proof_digest|cbfdbe4b9eaf039f324f7a6cec100ae09adc4f7c508b9e30f612bba17ededb4c|1|30e6f231b97259cc2a5cce3fdd8777eb6105555d7ebb9e42f8df9f8bfb66471b|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.nonwidening_proof_digest|nonwidening_proof_digest",
        ],
        rationale: "a16:2209: shorthand member `nonwidening_proof_digest` at census path `TimeSubjectTerminalProjection.Reissued.nonwidening_proof_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Reissued` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:403123812a10492300994071d83a66ac3b7e056829d79bdc5ddd69eb9d110d10",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_body_and_profile_digest|15422618b999b4c7ded7c4a764deb957eabd89f49140d82ec693d4b3c3d3fd46|1|8001cc7771f5151406f5d5ade6a2e284fd8cefddb29e3be47fd6576c381cc14c|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_body_and_profile_digest|old_body_and_profile_digest",
        ],
        rationale: "a16:2209: shorthand member `old_body_and_profile_digest` at census path `TimeSubjectTerminalProjection.Reissued.old_body_and_profile_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Reissued` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b515598a6fe76493393ac44fdc64bf6208f036442ef977ba94d33b6a110bdfa",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_identity_tombstone_digest|32ad57c02adcd5cb1262bad397b1a62f589c1fabb06352147a70af00d2ec10b0|1|b6c9b3a69eb3cf4e9b008901cb8001a92faea519f8187064e32afef284c87d08|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_identity_tombstone_digest|old_identity_tombstone_digest",
        ],
        rationale: "a16:2209: shorthand member `old_identity_tombstone_digest` at census path `TimeSubjectTerminalProjection.Reissued.old_identity_tombstone_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Reissued` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2236fed6acd8eee82cfb48df29a034f919dd8aad900b0e20c6cdf98611ffec4a",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_subject_key|39adf495848e8d900cb9dacc8bba361ab194cd001d8cc595f4ca2958a7dd3225|1|95e1a32a65caca17dbbb8030f39d810616fd6f91815db9d861e5425902f29ed6|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_subject_key|old_subject_key",
        ],
        rationale: "a16:2209: shorthand member `old_subject_key` at census path `TimeSubjectTerminalProjection.Reissued.old_subject_key` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Reissued` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ac681d7d9469b0042e8a114098420c4e08f9725dfffcd261b70a2c135d5b2f3f",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.successor_subject_key_and_public_commitment|f78ab0b74887acffce9661aa003f4fcf64a2bb606cd856b2a0d08e77b3acc2f6|1|065b73f48c8298334244b805c102214688d30a658dc767f4a656a7c5bd435a8c|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.successor_subject_key_and_public_commitment|successor_subject_key_and_public_commitment",
        ],
        rationale: "a16:2209: shorthand member `successor_subject_key_and_public_commitment` at census path `TimeSubjectTerminalProjection.Reissued.successor_subject_key_and_public_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Reissued` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:64b787a35f64549ba2fe85e0c0b1ef8290bd992e6f1e28c1432f3af64ea89e86",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.subject_body_and_profile_digest|0bd12997bb8d9fdbaf826b28ece45fa3b787ea37f15737c8fdbb3eb002e9ba82|1|27cb7c67629172a9894b360f8adff968472925396b29f9fa340c956e6c01ceb8|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.subject_body_and_profile_digest|subject_body_and_profile_digest",
        ],
        rationale: "a16:2209: shorthand member `subject_body_and_profile_digest` at census path `TimeSubjectTerminalProjection.Terminal.subject_body_and_profile_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Terminal` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ec3ed5bd31064ce1d87e694da563bfefd7d5838d8b4b1b8a617a8dbabb76d4df",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.subject_key|bcaa270cecc5c1dd9d8b1edc2218b3d91ddbc51e9f8a751535af1fcc83bc7013|1|d3d0d92704bffa08cade3e9d86c52c03ac4fb40ae6cb4c843fcd4145623ac426|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.subject_key|subject_key",
        ],
        rationale: "a16:2209: shorthand member `subject_key` at census path `TimeSubjectTerminalProjection.Terminal.subject_key` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Terminal` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:090cb31de86f72015dffe5b58eddba0fb6ae766050c7d28ee911059708895582",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.terminal_authority_commitment|be6a50f42d2083e950fa3ced6469243132be120727c64dbf25b1426f63ff8008|1|1aeed0beb4a3465d08700395fba03417d013ad5c59abc0d7c0c61a55ccb46c5b|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.terminal_authority_commitment|terminal_authority_commitment",
        ],
        rationale: "a16:2209: shorthand member `terminal_authority_commitment` at census path `TimeSubjectTerminalProjection.Terminal.terminal_authority_commitment` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Terminal` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:412f94e68cbd3fafba3f239ad4edae688817333f62d5379772e577a74fb150ff",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.typed_terminal_evidence_identity_and_digest|c93c0aeb2f95eb0839bcb61b8c3e3aeda7aa5f3bc9c5d39efb9703740b3eb4aa|1|866df57554b43e47ef42014db3325a6181b9e322137cb9befb04186ea38afd58|shorthand field has no exact type",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.typed_terminal_evidence_identity_and_digest|typed_terminal_evidence_identity_and_digest",
        ],
        rationale: "a16:2209: shorthand member `typed_terminal_evidence_identity_and_digest` at census path `TimeSubjectTerminalProjection.Terminal.typed_terminal_evidence_identity_and_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectTerminalProjection` `Terminal` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fe273b4d125585f91d7775d7061204c113cf295fbd53b607ec0e0cade19f25d5",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|unparsed-record-item|ShardTimeAuthorityRetirementAck|ShardTimeAuthorityRetirementAck|dec1da9246adf963002710d7196c7ac12701ceb27c317b96f62bdf586e7e16a4|1|3393f67c657940a6c2170e811e620df24e1f001b648fcef0d81370d91756bed5|record item does not begin with a lowercase stable field name",
        source_locations: &["a16:2215"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ShardTimeAuthorityRetirementAck"],
        rationale: "a16:2215: the leading `SameGroupCertificateHeader` item is a named closed sub-schema (compact-phrase law, a01:1412) embedded in the shard retirement-ack body; it belongs to the `top|ShardTimeAuthorityRetirementAck` candidate.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0799a80b9503af44762c89c9bd28c21ce8615b3e50872f14c3d718cfb6b855ff",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|unparsed-record-item|ShardTimeAuthorityRetirementFloor|ShardTimeAuthorityRetirementFloor|dec1da9246adf963002710d7196c7ac12701ceb27c317b96f62bdf586e7e16a4|1|0e114346b3fa410f12cdf76365090e95a30c4364d257c0f68bea0ad5f5fae4c5|record item does not begin with a lowercase stable field name",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ShardTimeAuthorityRetirementFloor"],
        rationale: "a16:2205: the leading `SameGroupCertificateHeader` item is a named closed sub-schema (compact-phrase law, a01:1412) embedded in the shard retirement-floor body; it belongs to the `top|ShardTimeAuthorityRetirementFloor` candidate.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:89eb7fcff73cdc05f717b5f8888b49680d0d11e5d8b7ac63fbaec93d118bfc84",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|unparsed-record-item|ShardTimeBoundSubjectInventoryCertificate|ShardTimeBoundSubjectInventoryCertificate|dec1da9246adf963002710d7196c7ac12701ceb27c317b96f62bdf586e7e16a4|1|351d39c67b9320fb2dcdfb512bb5355b92fca1f58712817e9a79da90713cd761|record item does not begin with a lowercase stable field name",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ShardTimeBoundSubjectInventoryCertificate"],
        rationale: "a16:2197: the leading `SameGroupCertificateHeader` item is a named closed sub-schema (compact-phrase law, a01:1412) embedded in the shard inventory-certificate body; it belongs to the `top|ShardTimeBoundSubjectInventoryCertificate` candidate.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e4cd82413f511826adaebd47c79e439c7cf26a366db7682136e3e70085ec8902",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|CrossLogTransparencyFreshness|CrossLogTransparencyFreshness|e6fd8551273da0e02996a19f76ba9b4c3837a35abec5cc962bb5043ee49ecd93|1|f6dfde41644028fec673fa4fb518aaa8f7b2aac8e44c9e1d07afc01778375316|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|CrossLogTransparencyFreshness"],
        rationale: "a16:2171: `CrossLogTransparencyFreshness {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|CrossLogTransparencyFreshness` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b9347cd7b769ace4ffbb1db74e730bbdbb736dccd01fa43aabc5f762322d4d2b",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|GroupTimeIssuanceQuiescenceCertificate|GroupTimeIssuanceQuiescenceCertificate|36f0884a28b9eeb213bbb37f209bef91784d68945130271a87f52761d4965079|1|579645ce8221ef09d5878e1545199e33baca205ee9dc323d9e78f18e9c664a1b|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|GroupTimeIssuanceQuiescenceCertificate"],
        rationale: "a16:2193: `GroupTimeIssuanceQuiescenceCertificate {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|GroupTimeIssuanceQuiescenceCertificate` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:bbeb7610f005f870af91589e584a196b7b10d4898f7da64f0d8edc78dc7dbbdf",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|KeyEnvelopeGrantWindow|KeyEnvelopeGrantWindow<Role>|792a3cc43524383ab9cf5694b4105e048e6cc697c0f0472593386767ddb0fdfe|1|423c54c076440f11ceaef6a3e478ee849e95cf1ea107caad42d9c2d63cf77333|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|KeyEnvelopeGrantWindow<Role>"],
        rationale: "a16:2171: `KeyEnvelopeGrantWindow<Role> {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|KeyEnvelopeGrantWindow<Role>` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:994a81b46f241832d3170159b4c71dd7083d9fefe89aa659273a1c4426970fab",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|PayloadReceiptProposalFreshnessBasis|PayloadReceiptProposalFreshnessBasis|f9743bcf9ae4e8a7f495f1b426f349e741b09141ccc8109f9e2febe9910350ec|1|bc84453d9e9f74e49df700727f384cddcf4dda6566f11c1549bcdcb7653a2197|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PayloadReceiptProposalFreshnessBasis"],
        rationale: "a16:2171: `PayloadReceiptProposalFreshnessBasis {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|PayloadReceiptProposalFreshnessBasis` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9c6c718123cf055dc9a456ace50120d2beaf8d0ba42cd7aabab639de7ecf9fd6",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|PriorIncarnationLeaseCohortWindow|PriorIncarnationLeaseCohortWindow|c75ca267ed4bae2e83ad03b73e40f06d59ecd716247848c9439e1580ad838f00|1|fc378d29f3624ee31ed9148d7b87aafc3d73105bb026754800e914248b204881|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PriorIncarnationLeaseCohortWindow"],
        rationale: "a16:2171: `PriorIncarnationLeaseCohortWindow {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|PriorIncarnationLeaseCohortWindow` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:40b07c775488fa9786d84a11088f6f57e4b9a5c95299ad4ffc143fc696574286",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|ProtectedErrorReplayTimeBasis|ProtectedErrorReplayTimeBasis<Role>|26cf9349bc9b17363be804bcd25303ab48c9f23a7218ec98001aca86bf238e0e|1|87378e87ae67e1d42904291d0b6ea6d6cc3b86cc3e481e7cba34cfd0c0173442|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ProtectedErrorReplayTimeBasis<Role>"],
        rationale: "a16:2171: `ProtectedErrorReplayTimeBasis<Role> {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|ProtectedErrorReplayTimeBasis<Role>` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:eaa26b8f61958eeabd97319a5546c82f57bad2e255e80e863d3dcd1900482f97",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|TimeAuthorityIssuanceClosingReceipt|TimeAuthorityIssuanceClosingReceipt|76b81cdccf5723b11cf8d0fc37509f46605f86c67fe959a8b9bea1e2db9dca33|1|14d3f3f51ab2c3281374103f749f68477594b8196b5ae47f0b2ad9c0885b5368|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2185"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TimeAuthorityIssuanceClosingReceipt"],
        rationale: "a16:2185: `TimeAuthorityIssuanceClosingReceipt {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|TimeAuthorityIssuanceClosingReceipt` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8f06d57bcaee0a663a16812a9981d7bd2478b69706dd7c042f70ed8b137e8231",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|TimeAuthorityRegistryTransitionReceipt|TimeAuthorityRegistryTransitionReceipt|15cf30b4ba38da786924cab471d0556463ee03b4d39c5fec4382a625ca9e0bc2|1|41ef94c7e6eeeae332d601613c6ddfd7b4c3c9aeb90cdb2668eb9c726f03e90e|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TimeAuthorityRegistryTransitionReceipt"],
        rationale: "a16:2217: `TimeAuthorityRegistryTransitionReceipt {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|TimeAuthorityRegistryTransitionReceipt` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8382d602a7cfb3b34c9b3adc956a9aceaab94ee8e651d9eafad600a00a1f175a",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|TimeSubjectIssuanceReservation|TimeSubjectIssuanceReservation<Role>|771d2b92737a0ffbea4d2763e85896a473b9a256c4e28ce67b87bc1583758b98|1|e98890295559e48247c398bdd64f7f6491a446e4a49fef5cc0cfd5dae7f7c634|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TimeSubjectIssuanceReservation<Role>"],
        rationale: "a16:2191: `TimeSubjectIssuanceReservation<Role> {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|TimeSubjectIssuanceReservation<Role>` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:a0148dfd5ecafa3bd5cdb3dbc1e191d682e5eeb8d6023b8bcd9e504b33ed85f6",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|ambiguous-schema-owner|TransparencyCheckpointFreshnessBasis|TransparencyCheckpointFreshnessBasis|50f716c3af5a611d94a39836045bfa9def062b77dce0d713a77ee24c05b38907|1|6c6fc662da2ca59132b5ffdef77364102448622cb5be904e9f731e86d2d7a955|leading named record has no explicit top-level ownership cue",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TransparencyCheckpointFreshnessBasis"],
        rationale: "a16:2171: `TransparencyCheckpointFreshnessBasis {...}` appears with its full brace body as the slice's own definition (prose-embedded, no heading cue); the flagged span is the normative rendering of the top candidate `top|TransparencyCheckpointFreshnessBasis` itself, not an alias, enumeration, or citation.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f1c6073209ca7d229ae3f22e6b388d4775edd7625e3c0d7a08492cd3b091968d",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|conflicting-candidate-evidence|PriorIncarnationLeaseCohortWindow|PriorIncarnationLeaseCohortWindow|c75ca267ed4bae2e83ad03b73e40f06d59ecd716247848c9439e1580ad838f00|1|fc378d29f3624ee31ed9148d7b87aafc3d73105bb026754800e914248b204881|the same schema source key has divergent structural bodies",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PriorIncarnationLeaseCohortWindow"],
        rationale: "a16:2171: the flagged span is the top candidate `top|PriorIncarnationLeaseCohortWindow` itself — the a16:2171 rendering is the full sixteen-member definition and the a19:2465 rendering is the ten-member restatement its lease-barrier bootstrap import quotes (adding `time_authority_profile_oid`); the divergence is for the catalog's structural rows to reconcile, not a second schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d0fbe5b2a75940c2e66ca399d38b8efa3ca39e33d44bf639a2c965fba6ef4e0b",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|unparsed-record-item|GroupTimeIssuanceQuiescenceCertificate|GroupTimeIssuanceQuiescenceCertificate|dec1da9246adf963002710d7196c7ac12701ceb27c317b96f62bdf586e7e16a4|1|579645ce8221ef09d5878e1545199e33baca205ee9dc323d9e78f18e9c664a1b|record item does not begin with a lowercase stable field name",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|GroupTimeIssuanceQuiescenceCertificate"],
        rationale: "a16:2193: the leading `SameGroupCertificateHeader` item is a named closed sub-schema (compact-phrase law, a01:1412) embedded in the group quiescence-certificate body; it belongs to the `top|GroupTimeIssuanceQuiescenceCertificate` candidate.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f0d112a99c9db91ac5aacac17a50ab039e9d6dda70ea189fd0428ae7e5f6077e",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|MacaroonRootIssuanceRecord|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Expired.time_validation_evidence_ref|9b178669e1dc7f0c6d2955cbafb2c8e40e0bd2350b1dd724bbaa6d8d1c2ff9a8|1|010eafb9984659de247e640a4c9aa5663839b58689a69df38c522cdbe936f5bf|shorthand field has no exact type",
        source_locations: &["a16:2173"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Expired.time_validation_evidence_ref|time_validation_evidence_ref",
        ],
        rationale: "a16:2173: shorthand member `time_validation_evidence_ref` at census path `MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Expired.time_validation_evidence_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state` `Expired` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:27b38b207a4da96fce9484e5d14596a5c429122c2e0b1828fc0d5a30b9df4ba1",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|MacaroonRootIssuanceRecord|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Revoked.revocation_evidence_ref|2fa228f026a9a831a9353e564a92726f6f9557538dafd535de64350017f7b4d6|1|d9e75f3b4f1e029d615b0bab0f11bd402e5f1df6f6f754e9c65ee030ba2d6be4|shorthand field has no exact type",
        source_locations: &["a16:2173"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Revoked.revocation_evidence_ref|revocation_evidence_ref",
        ],
        rationale: "a16:2173: shorthand member `revocation_evidence_ref` at census path `MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Revoked.revocation_evidence_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state` `Revoked` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d3972d524264e6449e2528313a7976b508eac8acabf9593a4c8ce1ffe06f7c7c",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.receipt_digest|742a7427646cce07c66191aeda9a906cf766c459c8ac499ef03289eaf9d20070|1|38453b1353b2acfc78ce7c8b684a070bd7bc1eb0d9bf34fc5bc36eb5b7d88361|shorthand field has no exact type",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.receipt_digest|receipt_digest",
        ],
        rationale: "a16:2217: shorthand member `receipt_digest` at census path `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.receipt_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition` `AlreadyApplied` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:73143cf7fd34135bee48b34d7acfe28e8d8b7d465f7045f6d3aa6db79174a6f2",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_cas_version|2614fdcd6d71117b6761976ff6a5702485716e43d28a2374a3730418c9c76b0f|1|2d6828fe66bb097c080b4f852752b8a8f6ea8a82a36284371524410733dffa8f|shorthand field has no exact type",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_cas_version|returned_cas_version",
        ],
        rationale: "a16:2217: shorthand member `returned_cas_version` at census path `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_cas_version` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition` `AlreadyApplied` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:40fc6209f306a671ccd4db75f22833ab2aa76d13d8442ebc25674a04196cb132",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_head_digest|e4656807a8ce7931810eff2747a30db1ddda8c35a08b7fe3c285f792610c1f58|1|01b14b25fdf3f7f62d3d52f289f55db6bee5bc3e1ed8c951d471a5e651a9d18b|shorthand field has no exact type",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_head_digest|returned_head_digest",
        ],
        rationale: "a16:2217: shorthand member `returned_head_digest` at census path `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_head_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition` `AlreadyApplied` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8058b555f1a86c8409f8539532fc03036c349fa2f6fee639571fad438e13358b",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.receipt_digest|742a7427646cce07c66191aeda9a906cf766c459c8ac499ef03289eaf9d20070|1|bb7af1fdc823918ec571e0a75a8e3b76d40faceb15df193831dc07366db4694d|shorthand field has no exact type",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.receipt_digest|receipt_digest",
        ],
        rationale: "a16:2217: shorthand member `receipt_digest` at census path `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.receipt_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition` `Applied` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e62694631633aa73ecfadb4ad6d290ed730a138cc9e521583c2200708582fc9b",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_cas_version|2614fdcd6d71117b6761976ff6a5702485716e43d28a2374a3730418c9c76b0f|1|cca349f2e71310c743aada705f8e8b5b10f57ce021a2b7cfc856913e9db410ef|shorthand field has no exact type",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_cas_version|returned_cas_version",
        ],
        rationale: "a16:2217: shorthand member `returned_cas_version` at census path `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_cas_version` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition` `Applied` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:7cae16fadad1f0eaafdf3ebaded743a00bd755995d0a1d8db5d6a6d00507d90a",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_head_digest|e4656807a8ce7931810eff2747a30db1ddda8c35a08b7fe3c285f792610c1f58|1|d25ed4bc07e5387a6f0ba6ecff17291241209ff9efcc310755f63c886a0aff43|shorthand field has no exact type",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_head_digest|returned_head_digest",
        ],
        rationale: "a16:2217: shorthand member `returned_head_digest` at census path `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_head_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition` `Applied` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3f70b6d5956b91cbed5b83cf1c9a008e0582772e43187bbd4307c896c9a58b0f",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|RestoreSourceLeaseRecord|RestoreSourceLeaseRecord<Role:AuthorityOwningRole>.record_kind.AcquireImported.prebootstrap_owner_digest|f772d77a24d4e1d4c6c3692f21b0e41bcfa065760f941fc39d5af6d486aa3755|1|435248a0e8f4327001c517da8850d24f6bacf07fee8c8d06f544c57b7498ed26|shorthand field has no exact type",
        source_locations: &["a16:2237"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreSourceLeaseRecord<Role:AuthorityOwningRole>|RestoreSourceLeaseRecord<Role:AuthorityOwningRole>.record_kind.AcquireImported.prebootstrap_owner_digest|prebootstrap_owner_digest",
        ],
        rationale: "a16:2237: shorthand member `prebootstrap_owner_digest` at census path `RestoreSourceLeaseRecord<Role:AuthorityOwningRole>.record_kind.AcquireImported.prebootstrap_owner_digest` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `RestoreSourceLeaseRecord<Role:AuthorityOwningRole>.record_kind` `AcquireImported` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ee2a0d26ffa8fc46b9f93e2f68ff4faec17728d1615fa20416ac52779fb5fe24",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectIssuanceReservation|TimeSubjectIssuanceReservation<Role>.state.Burned.typed_no_publication_proof_ref|ee56aecfb1b4e9f2209c71f97b8c7b0c9f90b1004bad7a2242a8f8d3e4210c68|1|930dc77c2095e65fd0834f156b409bdfeb2cbe312b004afdea8ec9d04511e550|shorthand field has no exact type",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Burned.typed_no_publication_proof_ref|typed_no_publication_proof_ref",
        ],
        rationale: "a16:2191: shorthand member `typed_no_publication_proof_ref` at census path `TimeSubjectIssuanceReservation<Role>.state.Burned.typed_no_publication_proof_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectIssuanceReservation<Role>.state` `Burned` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:501aac0906acf541265937cca86f1d5e075cbecbad580e128e54fa76e93e0455",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectIssuanceReservation|TimeSubjectIssuanceReservation<Role>.state.Published.publication_cut|6b0cefb4f99834c5ae2db3afb18c4c1b0cf2f1a161b855d3b33aaa17aac05d13|1|06b854a977a010c34f23d5c1314135a47c441a3c78ed9be7b5b26ec56e638ae2|shorthand field has no exact type",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Published.publication_cut|publication_cut",
        ],
        rationale: "a16:2191: shorthand member `publication_cut` at census path `TimeSubjectIssuanceReservation<Role>.state.Published.publication_cut` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectIssuanceReservation<Role>.state` `Published` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8762b46c4d850de631aa9c367c2e30c1f082d0306636f98323fdd1d17bfe7b52",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectIssuanceReservation|TimeSubjectIssuanceReservation<Role>.state.Published.subject_identity|f51fddc65b81c34d3d8b9598d6adb9315a097e48a82176c749ec4cf5f7e41e7d|1|47ec0cc76ae2a4c363613c7f6353283c688a84b129ed52de356db84bd24748be|shorthand field has no exact type",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Published.subject_identity|subject_identity",
        ],
        rationale: "a16:2191: shorthand member `subject_identity` at census path `TimeSubjectIssuanceReservation<Role>.state.Published.subject_identity` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectIssuanceReservation<Role>.state` `Published` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:47ca97c9c1b146df0c47c3f7591362bdb4b898f8ed393ece13b85c505d8d74ca",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectIssuanceReservation|TimeSubjectIssuanceReservation<Role>.state.Published.subject_membership_proof_ref|a916ace16d3918cacbdaa1db7231564d36c1b5bc56f7ff9c2f4816b740bc8954|1|447da631e6a613682838d2b959171a1c5dc8d642b80fbb63f06849a0fca44b31|shorthand field has no exact type",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Published.subject_membership_proof_ref|subject_membership_proof_ref",
        ],
        rationale: "a16:2191: shorthand member `subject_membership_proof_ref` at census path `TimeSubjectIssuanceReservation<Role>.state.Published.subject_membership_proof_ref` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeSubjectIssuanceReservation<Role>.state` `Published` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fa23a8cbb7abc8dd79d1abab0b34316219cdace6a76e2bff9c4b1e0499a10650",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeValidationEvidence|TimeValidationEvidence.classification.Expired.expires_at|a2d000e1977254b1f6671cf27654b07ff556c38c9f077ca7d6c768a5372acc47|1|a7b3837d32fbccba247a581bc0a55e24ee33cdd6c26b1c9edc26b33a971bf3ce|shorthand field has no exact type",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Expired.expires_at|expires_at",
        ],
        rationale: "a16:2167: shorthand member `expires_at` at census path `TimeValidationEvidence.classification.Expired.expires_at` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeValidationEvidence.classification` `Expired` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e3b83ea20d0ae098e66e43bf5c550af8f0ef4e5deb0611dff362dc221147dd87",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeValidationEvidence|TimeValidationEvidence.classification.NotYetValid.not_before|c7c6f927267eba0536df651c835769a72cd1cb27eca35e937f43af180c0334cb|1|60594276a20029409394d7a7be18b638f82cfe5457f298242da1f00d931a75ef|shorthand field has no exact type",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.NotYetValid.not_before|not_before",
        ],
        rationale: "a16:2167: shorthand member `not_before` at census path `TimeValidationEvidence.classification.NotYetValid.not_before` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeValidationEvidence.classification` `NotYetValid` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:130c006a13e9ca83c0077198adc547325eef648f40464ca11c5b7c361ee50b78",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.expires_at|a2d000e1977254b1f6671cf27654b07ff556c38c9f077ca7d6c768a5372acc47|1|af3256eaf8af3ca7761c65289e9965d6a7fbbd5a4ce4a74c2cdff9ff2538b28e|shorthand field has no exact type",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.expires_at|expires_at",
        ],
        rationale: "a16:2167: shorthand member `expires_at` at census path `TimeValidationEvidence.classification.Usable.expires_at` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeValidationEvidence.classification` `Usable` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ae09c78d9160d13ccf4f3c6dd11c1519b46645b455ad4719681f09b22eb4a015",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.guard_deadline_local_monotonic_tick|0579565d9bff49955e25c09b78bb899e0d20c48863c6228d6ba3621d3555460e|1|0e1fe264053c6d8844f36297113e0fe5506253b967f8c42d3d6371bdbba67076|shorthand field has no exact type",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.guard_deadline_local_monotonic_tick|guard_deadline_local_monotonic_tick",
        ],
        rationale: "a16:2167: shorthand member `guard_deadline_local_monotonic_tick` at census path `TimeValidationEvidence.classification.Usable.guard_deadline_local_monotonic_tick` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeValidationEvidence.classification` `Usable` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:92d41da0431b14a88135a2a3b5bbedbb281ab58af478f2ad2f04bcb383441774",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.not_before|c7c6f927267eba0536df651c835769a72cd1cb27eca35e937f43af180c0334cb|1|aeed0f574e2eb7e22718a9be2a67e97493fb29783b73ca2ee5ce33ddc08f728a|shorthand field has no exact type",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.not_before|not_before",
        ],
        rationale: "a16:2167: shorthand member `not_before` at census path `TimeValidationEvidence.classification.Usable.not_before` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeValidationEvidence.classification` `Usable` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e2c84603e3f6477231e1f9b44471df57d2ca0197a763b02659b10b0d5da10fa0",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.validated_process_incarnation_id|3dbeb51887a08703989b5f9a90c13ea3afddf79183638cd200b944187e9fc35b|1|9b76deb6958748bcde12149dba73c45df715be95ff257e1750c02a941c861fa2|shorthand field has no exact type",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.validated_process_incarnation_id|validated_process_incarnation_id",
        ],
        rationale: "a16:2167: shorthand member `validated_process_incarnation_id` at census path `TimeValidationEvidence.classification.Usable.validated_process_incarnation_id` carries no inline exact type; the span is committed byte-exactly by the covering union-arm payload digest of the registered `TimeValidationEvidence.classification` `Usable` arm, and the single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:db66247a27fb86a8603024209e2ddd3fe88d0af75ee5f1ee02c68f9788142a3c",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|conflicting-candidate-evidence|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId|7abf48bb8920aecc18976e2a710332f888bce54c09cc1fdefa34905215fe0c6d|1|7b328a6974a5d4010e974e3c5ed04ed52d1942ce154471d6544eead1534710b8|the same schema source key has divergent structural bodies",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale: "a02:1449: `PlacementDescriptorWithoutId` is rendered with divergent bodies at a01:1443 and a02:1449, which is why the census flags conflicting evidence. The family is nevertheless a registered durable schema: a01 registered it as wire type 0x001e with its own target, so the affected census key maps to that registered source form. Which of the two bodies is normative is a separate a01+a02 erratum and is deliberately not settled by this row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:44e6b6852bdc2c0f3336cc4a9f866a0430e83a64baa491610e066c4e9e0a113f",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|FenceToken||279c58bfcb8cc4560e8d008cca9c52f415afd2c6c327fdf548ee95c0522ed88f|1|3291acdb3936a932cf6ae42a0321f999254d8d265686d2a53e900d254b9aa853|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a02:1447"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|FenceToken"],
        rationale: "a02:1447: definitional prose names `FenceToken` and supplies no structural body because it is a runtime type, not a durable one. FenceToken is explicitly nonserializable and nonconstructible outside the VFS/commit boundary, and PendingFenceGuard is a linear guard that cannot survive restart; neither is encodable, so neither is a durable schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:b6f908b8122821b649d176365f4deb62078c49b328f0f9848bdec7b7dea770b2",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|PendingFenceGuard||1f0bc608faddc0c0ecfd60ca30b5a60b1d81d4daed7e54e38115901dde15fc35|1|3a54d9f90df6efa57d6e8a68e3a5c453e780d5b21775fbde9660cf57b943bddf|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a02:1447"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PendingFenceGuard"],
        rationale: "a02:1447: definitional prose names `PendingFenceGuard` and supplies no structural body because it is a runtime type, not a durable one. FenceToken is explicitly nonserializable and nonconstructible outside the VFS/commit boundary, and PendingFenceGuard is a linear guard that cannot survive restart; neither is encodable, so neither is a durable schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:b7095531bad16e02ee36f522e449e2352c619acf383706f42d879b4794d93dc6",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|SymbolRecord||76d352859de8d9247d0da22ea015d1329c685cfee2bb089500bae5fe4a3f0809|1|48d293b8cad62c698fa7ac5f8249e75e497da321827118e81b20ba54decd8873|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|SymbolRecord"],
        rationale: "a02:1449: definitional prose names `SymbolRecord` without an adjacent structural expression, because its body is supplied as a fenced record block rather than inline. `SymbolRecord` is a registered physical kind, so the flagged span maps to that registered source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:4ccedc0588f7be74f4888666d503fde3bc7bc24371cf1ce1e3626a81d160c577",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.canonical_plaintext_len|c30cb16b42d94ab031378b967734814d2d1c336dfc0af2651fea26257790120e|1|5037208a13e4a9935e71160fa730ea4c9de79f26713d4c07585187e24999cb19|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.canonical_plaintext_len|canonical_plaintext_len",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:09e450db53ed431535948a0409580058cec35c34706c8d2e356e090ba0858523",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.codec_profile|8f6f3471944957612209cd358f31645a79a2c2296c30e7125da906126031b09c|1|b743b73c744d03bebfd399a599d24b036d1f8cfe3d6d3bddacd25dc6a9200b33|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.codec_profile|codec_profile",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:2107f6bb310d1738e09dbc78172590eb86e893bdc1122828d6c89b4872d0dd4c",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.compressed_len|4ca37ffa575d53985b9dc3052f1e93405068450122d22d72a543fcfbd684657d|1|cf0b7605ee455fbd64c805e210b7f2b0033c05da2d0abf5a70396e750858de48|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.compressed_len|compressed_len",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:78bc7d81d3ea77a5f7cbafe8087902fe5408ae96fd0196ccc8853add6213b849",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.data_crypto_profile|20e2fb1140e20ee7f6596d8ba740ce312d4f90980f91db1a1c3c58b41ecbf973|1|3cda350a963893c7dc1fb5b15eb72aa0b7d80bdd36caa19fbf8df058bd4116e9|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.data_crypto_profile|data_crypto_profile",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:01b0e2f905955e3f08d85feea3eaa8ece8b3733508e8bb5f32659f2c5665d2b7",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.dek_id|8af69fe8b94da3d64b510caa0dab1b7e9030eae0b4bb2faac1b7680fc9a7edb1|1|4ee41b6a1e48d72380f52deda275fa0a0199d03bcb9f9f97df92ff5cbfee3e12|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.dek_id|dek_id",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:65f322cfd0c28b197696f849d53882d7a792fd2641775c105f7d6016bfcbdc67",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.logical_oid|fdd9a74312ec7ab8a436466dbb215bc937ed5e6accb9d7b85a22c9c095a2d444|1|305e96c8b0e200c9ffb871882c7f0c13064a8db41a9ba57cb6bcec9e5ce61648|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.logical_oid|logical_oid",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:aff152416fbd814b48c7e182decf54f4e4e8f31a7e39bf85bf7b2c8a35b39ed9",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_kind|f79080c76579371982ec2dfc62593cfbe282dd0d9ec076e5638251fc9ce30909|1|8e2ff580178eedaefde5b39810b1d77293a01f7331155ac6101cef690786555d|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_kind|object_kind",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:3b10032b12d68f801a7a16decacf694eed72525f8432d18257d23fa2974cb862",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_nonce_or_siv|9e3d1c5690ee69b7d69b2bbcda21fe89688d01e6638589bf0ba2201602fee7f3|1|e15fd8e5d6cc706620b71e60b0a6502d09c59127e211fc648c69751dd8402bad|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_nonce_or_siv|object_nonce_or_siv",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:70dd1ce6a619845e395216c30b5cc24523f715d660652f6c59233b2f4a1cdb3b",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_tag_len|56a53094c14d9f87b12c0f75f816189bce109d403ce5ce4557ba55f62a3dc26d|1|9092af5856baa298d7c75230d71fa0adef8d34c510fdc058b609233cfcf5fe1f|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_tag_len|object_tag_len",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `CipherDescriptorWithoutDigest` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:21b51c04904bd6d0f06771ddd3fcff1e8f7dc47b406c10d1d817b81ceee93536",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CiphertextRecord|CiphertextRecord.ciphertext_digest|639e8bce8cd2107b20eb748abd7a4fa29f62ecd3752dab46977890694061e0f8|1|722428c1c230790a9ac59413eb117bf65c1e1d75b4ece333f40960ed2d569777|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.ciphertext_digest|ciphertext_digest",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, and its registered PHYSICAL owner is a field-owning host under `field_unresolved_schema`. The owner-authored durable field fixes ciphertext_digest to source tag 0x0003, exact type digest256, cardinality one and digest_class=target; the affected census key maps to that exact row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:e30d0e52a9b56177e2817418658afed5827dadf120553f44103643ee0f1aae7d",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CiphertextRecord|CiphertextRecord.ciphertext_id|47142c70021a01c228028cf69cea5c897a6985a67f0ff4234f8aa3bea11a523e|1|be43e8869204157592abe4620b1fc0e68e0b32807ce41252d8dfe1dcb6f5ae6f|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.ciphertext_id|ciphertext_id",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, and its registered PHYSICAL owner is a field-owning host under `field_unresolved_schema`. The owner-authored durable field fixes ciphertext_id to source tag 0x0002, exact type id256, cardinality one and physical identity class; the affected census key maps to that exact row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:0b3b432489be5ce60147873ff27823f6a116229dfdc512f64024a49580237c8c",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CiphertextRecord|CiphertextRecord.descriptor|194b520dc30384b3fc233e123778835e2adc362d91c6e33015ed3db2379d7ea1|1|7436baa9a7e0b2e6206451cbdb69f1f2c764e50b84881992c5d6fe3a5eb63684|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|CiphertextRecord|CiphertextRecord.descriptor|descriptor"],
        rationale: "a02:1449: shorthand member carries no inline exact type, and its registered PHYSICAL owner is a field-owning host under `field_unresolved_schema`. The owner-authored durable field fixes descriptor to source tag 0x0001, exact type CipherDescriptorWithoutDigest and cardinality one; the affected census key maps to that exact row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:7e38b5617e877b4c4d04aab2aceb9038d3998e272786e35c565ea11a61fc8402",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CiphertextRecord|CiphertextRecord.object_tag_digest|01382560e2e7615b048ea5367238388ac8c3f98760dcf5aa408bc874969d60e8|1|36b19f8caa5cbb18ae76302874307e79ed8bff813495ac462c6650761add05d5|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.object_tag_digest|object_tag_digest",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, and its registered PHYSICAL owner is a field-owning host under `field_unresolved_schema`. The owner-authored durable field fixes object_tag_digest to source tag 0x0004, exact type digest256, cardinality one and digest_class=target; the affected census key maps to that exact row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:0239498a2cba36ac46ba1927f4162f60e75399abfbb660542f8d307745300617",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|CiphertextRecord|CiphertextRecord.protected_length|5df4272c5f39dcef78c410b10c536a5704975cc8d6689dc6fbbcbeaed688ecf1|1|3c4a8e7fc8a685980c7a6dc0b8a30b64843e61ab27c873fc7557b449b3f50bf9|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.protected_length|protected_length",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, and its registered PHYSICAL owner is a field-owning host under `field_unresolved_schema`. The owner-authored durable field fixes protected_length to source tag 0x0005, exact type u64 and cardinality one; the affected census key maps to that exact row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:58efaec60718539e0d6fe9e4a1d057dd06ac699ceb1a3ab48e932808c2fa8133",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.ciphertext_id|47142c70021a01c228028cf69cea5c897a6985a67f0ff4234f8aa3bea11a523e|1|dd2cecd0e5dee5ca3e67bc3256c7c42bcabad0fdcd4ebb823854c9d018a162a0|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.ciphertext_id|ciphertext_id",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:c93e1bddb4c4026b3a846c187e67d3691154bb09a0d2c27549ed13d33c4faee3",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.fec_profile|74ccf82fa92b5a4e6c15745f28c3d0775462b0818f7aa93050d4f0568e8ed621|1|85dce6a6bc9d473b60140804d01abec258698c3b51da34bf2045c3a1aac134b1|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.fec_profile|fec_profile",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:36ce0f3f763b04170b64e17634530ae0b9f94d01676e993e5ed6cdf4f162bbf6",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.oti_common|0a77bbffbc0b36cce883b694852acdece87fb4fefb1006c2adbdc4f3a3297f46|1|d3927a94bcd8c143f407f4d8f237d3d8f99df9cf0476e74e7e467923a259c23a|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.oti_common|oti_common",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:0fd7f1abd9e2c65b77a7b222d54c3da2786eafb0c0a0ef19dbd85113383b17ff",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.oti_scheme|5788580e290dd22ee752c4a5a5215465706ca8ce39bd7e22b82b6d86515bf7ef|1|a30dc1fe345782d10c88ac2653e14e68a96a51feb26acf03cc2d734c9ddbf611|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.oti_scheme|oti_scheme",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:fc15e98ed9b83aca5947f32942eaef36bfe8ac25720614ccf5eccdd473462033",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.source_block_count|97724dac2757b7c0967f14c74e64d862e0f155784e8be5a51139d998ff7b5c2d|1|ce8a174aa4565cdd6f9bccfc9dd6bda35871faf3b5f093ef653f65bdff2bff8e|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.source_block_count|source_block_count",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:91902b26aa0a57dc07f52ad4d764ced91c8abdf107028a2ff451daf3f658b386",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.symbol_auth_profile|b5ca9a965730e32f4994a5ae5c74c26dd2be10d7a6ca2a8aa06d209eb872602e|1|62c55cc5c01501f15aaae35be55d9cac5e09888ac94a7a2e87877da9f074ba2e|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.symbol_auth_profile|symbol_auth_profile",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:74c44cccf917099778495230c0ec66ff6391deb20367e75fccd7315f3ac6b929",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.symbol_size|242fbd1d86c6ee2801babf811c61a4e92c7041af70ad90d5d389b77fc70b844e|1|511ef117f11cb794b1e2b938c1483fe5442fc1e0c5970cb88a0b8d26e0746a75|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.symbol_size|symbol_size",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:e5a35dd4fe8de0fb93b60737c11309eea3fee0445f97b87965f72b1440a46590",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.transfer_length|4e51c1a20220b0bf2903d73b9463e848aab990b6d0d91f58a75a6c3e7a657b56|1|f39cd190f2e4d525773a4f6fce08ac06bec7cb1fff6de92afee0d7225ac571a0|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.transfer_length|transfer_length",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `EncodingDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:e950732c0153855d0e18984a9a8efc352d8856cc1fab0222138f4b42cc3bec9a",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.allowed_filesystems_and_mount_predicates|ce1818e711027d4422406814d37e193868e745c8877f1dfd11f476d42cda37d3|1|10d56fc38e379d9089becaa3d13f4665f1042b2645f9f1a392a436c87bd8c287|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.allowed_filesystems_and_mount_predicates|allowed_filesystems_and_mount_predicates",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:3ebc591c25f9c7eeab7a5df0a9850620fcfd9db848142c7c1de3a36f7915d444",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.cache_flush_assumption|9ff7c0f5828e08bc21e18592096512a0d15482208b7178f6cc44aae15502abbe|1|97fb758bc911d5b398581b2d6a4337dac20742e62665d4f3e0c3055748a485c5|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.cache_flush_assumption|cache_flush_assumption",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:4b63359434d412cb4e93da35ec045ceae13989d042b84d7ae6314386df91c871",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.directory_sync_rule|d4562636b62950c0c93e5c684279377698844ffe35d22292955b3dbc31101290|1|606bb654c8091f3d605242aa11bf527ef3f379ce1d52f17e4fdbec206d7879ee|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.directory_sync_rule|directory_sync_rule",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:8873904cf4efb6da4b32353b2091f387b40f93ceff6af9fcf79813cf5644c638",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.file_sync_rule|0371d05b6c37b7aac95282a3f22f6b1a9185ed908faf7cad5f4b426f7972b9a2|1|ed3b5c5d0beea85adbc88bdb964ecf88724ec85ed054b11ca90dc62199cfe9e3|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.file_sync_rule|file_sync_rule",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:1ea7c0f52efa23b6ea05499d6477e99f9ba7b7b7643e237788fff9db535671f4",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.forbidden_layers|c8ebde079ef030df5a539237f8ed8d67364a0b71d0d416da37539dd12d1f4ffc|1|408650ee2f95d250a0851d94fe5ac150093a6cb135344c198822edad80a7060a|noncanonical field separator",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.forbidden_layers|forbidden_layers",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:6b2c1e520545d3f0a9f07781ed9ede4bae5756db2e1f8212a9ec94436cac6cf3",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.os|ad40f30433f81ebb9114a3028258d2823275a69722d1f89e68174998ee803e5b|1|2f3c5639e5a8462a4ebe2ee70b26816f53c9cb40ee5d1504566d1aad5951e95a|noncanonical field separator",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.os|os",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:eef13b4b7a2306d6fe972c7c2d821b7e62ba883e6ffb2a9e6fdd313323849073",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.owner_death_rule|fc404bbd2105c2c91734df8ab4e011dffda15f6824f15aa4598c02ef116721e4|1|245a5b4871bf927401a4179a46d4b7afebae153738d14957eadf82ac078b5ee4|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.owner_death_rule|owner_death_rule",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f647dad9e7de474a5854ee8f2254e916bee0d666b285d71f678368395354f05d",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.probe_suite_oid|2f2dab6634a79d74afa2c93284569950921d596841ba5de22b91822b6d912f46|1|fc5c4ec760672d249d932845beacb64d66beba03783f857629f25683902ee44e|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.probe_suite_oid|probe_suite_oid",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:bc62521d65078a2781fe44d3efb08cd84e5d65e876b31ea864407d9332cd20fb",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.profile_id|ce3b035fb5251de7d68263e33bfd5b205d3b0f08700e47ddacfa15100e0f6449|1|ff74e4a802afbfdc42e3a73e06bc0694030406ffa5bf085b331852a8289fd74f|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.profile_id|profile_id",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:05ee487e0b125b8d10eb6d1c00f301fff764d0caa5dafe3ec9da98c6477584b9",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.profile_version|a3bd9ac79d5e412794ba14aaf62f1b98fe12272c515693c89bc300f7d05be43b|1|cd38905627aedc6d3b81036eab74c130c1be49f50c66b5f85c79f9008c9ff7d2|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.profile_version|profile_version",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:99f8784a375d4d11bb423cc86fc89929a0674791e65daad040c44ae0e2415060",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.rename_no_replace_rule|4c95eb0388174bd3c74e4c53f12ed2ab9d1be4edf98a0cbbba4b16ba9271fb8c|1|aaa4bedb20e476eb2c5ead883b0de9ceaad7e42c418debb8da9a8783c41bf90e|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.rename_no_replace_rule|rename_no_replace_rule",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:a846ae0b543e13fc45e6a5f69df06c09d4d13aef5289f4195c9e4490b97ccf67",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.required_lock_primitive|af5bb820b74db4e27f19cf09bace97ea2846f777690291c0d0c57f17cf411e99|1|300f0e69f60a3ae8305a7b222e843d87db40955366bf93137d1e30ba81e7886d|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.required_lock_primitive|required_lock_primitive",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:418f3bc0a20a19de8b690a2bf879b4eb13452aca56deb8885b70a3840edba3af",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.slot_write_rule|2023471cf2f8c98fea286195952923e325148c98fc362ac86e2066e1a07fe193|1|da436c3769714a5f3c7a4f6c67982fdf1c84973188e146bd103c2fce3a54d74f|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.slot_write_rule|slot_write_rule",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:010e36ee5de62c235893df457e3727287ec8a3c933e2350f364f259104b82659",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemDurabilityProfile|FilesystemDurabilityProfile.stable_inode_rule|aa77a140806e960b8932787dd22941e275021bda2f79a25d815d1bb33382a88d|1|3ed4c5d7924a73ccd6426380b00ceae0268f5e636f1a613964ca25435592f091|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.stable_inode_rule|stable_inode_rule",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemDurabilityProfile` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:dfc4ee027f44f5a6de4cbad5c1e1ea667efc9f0bb8caedc8f3c95090c0a1d3b1",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.canonical_mount_options|bc67468a48d0b2b25cd144358d22b2f9b28c7bbc5de3b255bf9940a232020725|1|462bb37401462e063a37f0f44f2d499128898cc5535e86466755675b5a1be9b8|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.canonical_mount_options|canonical_mount_options",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:59835471a2ee6ac1e9331216d06953e05c349a60ac9784235a02802c2514492c",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.checked_at|3a474f9d10ef022d46a4c5fa68916ab897d3f6df414fc20ded06d6ca6abe6ea2|1|56b07be3d307087ba910513571b5168a5a272dd357f0df7a5d9bfeb8fa8c1ff0|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.checked_at|checked_at",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:55cf4f7028a45ef7d65a28a261fa7d7a3516b7d830592ae65bf7df05e455db6a",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.device_chain_digest|c2dd30f3695cd344641bfc0edfb6a4d90ee8fc47f721a8e8113eaec7c3ab56da|1|8887394b58e771a0b4c269cab515b38550d548b857c6ec52bdf3b9b545d16ca5|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.device_chain_digest|device_chain_digest",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:75f019a44ff22a05c88a94feab810d4ea5e37d5d09199e38e376efb9b35325e5",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.directory_device_inode|9b5832dded09553bfd152b4efe0f762bccfa25c1efcb857c45aa45142942b722|1|daaeb30cc41eb93779ae445e8302b4e6da2c4f43e8acbcc3eaff50b1d9c625ca|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.directory_device_inode|directory_device_inode",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:187c4435bf8b3e3c71d4c9991d9f9aa365586d2a8f5d222afd7f1973e98bdad7",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.filesystem_type|894151384ef1a6b5a3607e2d5eec1a1e40a51dbc958ad5fb9cf488b773173a97|1|21bcb47b89cc7fc8ee5a47d25a55af0aa591b8b9548e773c5db72c627c5a0a2d|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.filesystem_type|filesystem_type",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:58e8995d6dff4c106bde4b883c447df2fb5f23741ff4b27a13dffe03b4f5a846",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.manifest_device_inode|2866e5f15ff1f2ed182e3364c86c9598e47b7902c6b726c47562a04c7f7c614b|1|9ac99f5e20865fd0427845edb373540eb817e56ba63ff43ce069f9a787e30a07|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.manifest_device_inode|manifest_device_inode",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f0c77d4630f11fdfdb94e63f0eb32f9a5b62ec529eed748cb92e281f73c269d8",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.mount_id|3d420dbf2eb903c3acb9a49ca4fd47e3e1bf34cfd5931b83e92ad931720d3251|1|77dd1159d36ceea361e008570007bd2ada5cde5d3ffbc4c6bbe968dac22c463d|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.mount_id|mount_id",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:37dd75a6f44baa134b14e608eb509469c77653e9bf407141e9a2d456002e58dc",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.probe_suite_oid|2f2dab6634a79d74afa2c93284569950921d596841ba5de22b91822b6d912f46|1|dcbdf629020c7791e9acb9e126428f5d2537b9bd8beed56d3d0152b7021992c9|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.probe_suite_oid|probe_suite_oid",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:7493dcf2c9989d73978202b23a33520b3dae91d9b3322116b24d8c778479c089",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.profile_id|ce3b035fb5251de7d68263e33bfd5b205d3b0f08700e47ddacfa15100e0f6449|1|e164e2a9e2cba7e950c6b1728852dca2736a7bc2802eb305dd9ba80a6bb127e1|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.profile_id|profile_id",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:c9a8f5c62ed9dfd6d2bb96b7d0041ea2070123fe3a64def0a1341d7f47a1b9b7",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|FilesystemInstanceRecord|FilesystemInstanceRecord.result_digest|7d571540e94c1ac7847e1d3dd315c957f211012bc6ea9f1c622de3af57026aa9|1|d5b325697ef247ef9ef6c0b2feb402bcbff017332ee233e16a96a5fd05acb377|shorthand field has no exact type",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.result_digest|result_digest",
        ],
        rationale: "a02:1445: shorthand member carries no inline exact type, but its owner `FilesystemInstanceRecord` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:4589a1a610a23f057f6b5862ea7c6f0237c355edacd39c5d5b1795cf6b0cc5be",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.failure_domain_policy|cac086cbd127ca4950463583208f78f2b54e0a546a2ddf9f8dc803c73f509607|1|a2013f9c7e6167fa838b6210d0af5802a8bd05823d9302fb4972629a91721e74|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.failure_domain_policy|failure_domain_policy",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `PlacementDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:1121451997661af6c018ac5cb8a92e35e459d9cea025b5dfb39f12bc16c38e20",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.location_form|ad90d7ce88045805125a73e6bc382283cee688f7588f54ad0934a1e549bd2f2e|1|245c1d32e02e982689bcb9f2450dd413eea1b427fb02a64c935f969298c2512b|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.location_form|location_form",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `PlacementDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:d6eb0a856c857d051ccc3db1bbbdfc5333456257cab21d9eec3372302d2a44aa",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.placement_epoch|731e2899b0f9a25c8d09c479b12d7b6535c525fa26005898a294c124f5a9605d|1|2ec38c824e850340c4637b45e960c2315b6eb9a9c6f7c71aaab883a1b6a5ee9c|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.placement_epoch|placement_epoch",
        ],
        rationale: "a02:1449: shorthand member carries no inline exact type, but its owner `PlacementDescriptorWithoutId` is a REGISTERED WIRE TYPE, so the member is committed as a wire interior by that row's encoding_context. Wire-interior coverage projects the affected census field key, so it maps to that registered source form rather than being a non-durable span.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:200f2d59a98b49a305f74582c258b6b32316c146f35b4b436ec9620d319c2b52",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementRecord|PlacementRecord.descriptor|194b520dc30384b3fc233e123778835e2adc362d91c6e33015ed3db2379d7ea1|1|9c9c3dd11d91a87637869a7d6463af494cacd6233a48a3b1322b9bba895dcabc|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|PlacementRecord|PlacementRecord.descriptor|descriptor"],
        rationale: "a02:1449: shorthand member carries no inline exact type, and its registered PHYSICAL owner is a field-owning host under `field_unresolved_schema`. The owner-authored durable field fixes descriptor to source tag 0x0002, exact type PlacementDescriptorWithoutId and cardinality one; the affected census key maps to that exact row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f7c039dd333ca9b1bd1e78736d84f3afe200de8795e7791698012f017f5f5f5a",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PlacementRecord|PlacementRecord.placement_id|862e3bc66e01bc7a9dcedb6247960e1e5a9698e6810cb1a0950981231f908ec5|1|c3e5aeaf40b208c2ad9e3dd1981ca72c82afeb04eaa9a0067daa8d8c64bec5b4|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|PlacementRecord|PlacementRecord.placement_id|placement_id"],
        rationale: "a02:1449: shorthand member carries no inline exact type, and its registered PHYSICAL owner is a field-owning host under `field_unresolved_schema`. The owner-authored durable field fixes placement_id to source tag 0x0001, exact type id256, cardinality one and physical identity class; the affected census key maps to that exact row.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:34cf0a3cb4495aaffdd79a0489e1b6a8caa672ad4ed267b7e32b2c42f13e59b1",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|alias-expression-unparsed|LocalFinalCertificationReserveSpec|LocalFinalCertificationReserveSpec|a32bbf40609069648836f761b1fb115433bc2242b4fa4298af19f4278fe9adf4|1|75484acecbb70003eedd8126b4b23fdfad84b05eff09d3b603bdbd9d01a2b071|alias body is neither a top-level pipe union nor a record body",
        source_locations: &["a09:1904"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|LocalFinalCertificationReserveSpec"],
        rationale: "a09:1904: the flagged span is the body of `FinalCertificationReservationRecord<Local> {plan_ref,registration_identity,finalization_generation,sorted_mappings,permanent_spent_extension_commitment,applied_control_ref,state:Active}`, which the sentence introduces as the object `LocalFinalCertificationReserveSpec`'s apply INSTALLS. The parser attributes the brace body to the sentence's grammatical subject, so the alias expression is unparsed against that subject rather than against the record. The affected key is the subject itself, which a07 registers as a structural definition at a07:1750 and mints at its reserved code 0x030a.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:31cc7fb171ebf8e60218481acbf4ae4a9a46c47d0bf7e2636d2c44c14dbd5dcc",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|conflicting-candidate-evidence|LocalFinalCertificationReserveSpec|LocalFinalCertificationReserveSpec|a32bbf40609069648836f761b1fb115433bc2242b4fa4298af19f4278fe9adf4|1|75484acecbb70003eedd8126b4b23fdfad84b05eff09d3b603bdbd9d01a2b071|the same schema source key has divergent structural bodies",
        source_locations: &["a09:1904"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|LocalFinalCertificationReserveSpec"],
        rationale: "a09:1904: the same span carries a second structural body for `top|LocalFinalCertificationReserveSpec`, whose own definition is a07:1750. a09 references the symbol and shows an installed record inside the same sentence, so the two bodies diverge for one source key. The divergence resolves to the a07 owner's definition; a09 contributes no competing structural claim.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a03:ambiguity-adjudication:8c725fa15a9bc362be089cd1c4a8a996d6f4e20664fcac5528a2df62b880ef17",
        slice_id: "a03",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TxnOutcomeRecord|TxnOutcomeRecord.nonretaining_predecessor_digest|268064209a4df1dbcad1eb66b51e5cadcea9f4b19c5f92015c78ae58219e2557|1|3e4e3c9478ab77306a5f6dd8e456c8ffe9f19f18c63f88c98ffc9f366f3ca420|shorthand field has no exact type",
        source_locations: &["a03:1524"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnOutcomeRecord|TxnOutcomeRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale: "a03:1524: the bare nonretaining predecessor spelling matches the prepared-root family precedent and is fixed to one WeakDigest; it authenticates generation adjacency but is comparison-only and never contributes a traversal edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a06:ambiguity-adjudication:8723ace590fa1a79389ba18bd5aab08bcc1d3ebe7eb3a17528e939386f05cd0c",
        slice_id: "a06",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|MetaPreparedCommandRecord|MetaPreparedCommandRecord.nonretaining_predecessor_digest|268064209a4df1dbcad1eb66b51e5cadcea9f4b19c5f92015c78ae58219e2557|1|983566e6334d24084c27747e97f61d5b4677f958e4b2495c3d00c72b06f52703|shorthand field has no exact type",
        source_locations: &["a06:1698"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|MetaPreparedCommandRecord|MetaPreparedCommandRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale: "a06:1698: the bare nonretaining predecessor spelling matches the prepared-root family precedent and is fixed to one WeakDigest; it authenticates generation adjacency but is comparison-only and never contributes a traversal edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a06:ambiguity-adjudication:d29cf70d19793cee02c2ad22e8df60d5ad6dd3357ba46123cc85f4d5728315f9",
        slice_id: "a06",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|ShardPreparedPayloadRecord|ShardPreparedPayloadRecord.nonretaining_predecessor_digest|268064209a4df1dbcad1eb66b51e5cadcea9f4b19c5f92015c78ae58219e2557|1|b555581d9afcce502838c9520cafa55a9b3fc5571da2c91d1045c7acbf410e09|shorthand field has no exact type",
        source_locations: &["a06:1700"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardPreparedPayloadRecord|ShardPreparedPayloadRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale: "a06:1700: the bare nonretaining predecessor spelling matches the prepared-root family precedent and is fixed to one WeakDigest; it authenticates generation adjacency but is comparison-only and never contributes a traversal edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a10:ambiguity-adjudication:a2ec9c9e4687c66ca7527a460cb9a9629e7e9297e0e9d39a307025de544f061f",
        slice_id: "a10",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|PreparedCommitRecord|PreparedCommitRecord.nonretaining_predecessor_digest|268064209a4df1dbcad1eb66b51e5cadcea9f4b19c5f92015c78ae58219e2557|1|13c8b4d84ea85afa96a67e30397af849c0c6d9af5702a2f53e2ce645100300c2|shorthand field has no exact type",
        source_locations: &["a10:1922"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PreparedCommitRecord|PreparedCommitRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale: "a10:1922: the bare nonretaining predecessor spelling matches PreparedRootHeader<Role> in the same prepared-root family and is fixed to one WeakDigest; it authenticates generation adjacency but is comparison-only and never contributes a traversal edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:733086702e6b63491e3b2a762e92e10d4e93ed043291c93f73edc4769a46c3d2",
        slice_id: "a16",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TimeSubjectIssuanceReservation|TimeSubjectIssuanceReservation<Role>.nonretaining_predecessor_digest|268064209a4df1dbcad1eb66b51e5cadcea9f4b19c5f92015c78ae58219e2557|1|2c193dbc4032a49c21cf531f3f704a9e2cbc7fc8004bd836ecf285213b7d9b58|shorthand field has no exact type",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale: "a16:2191: nonretaining_predecessor_digest is the carrying field for the plan-named comparison digest, so it is fixed to digest256 with digest_class=weak_identity, reference_semantics=none, and identity_class=inline; a plan-named digest is not a wire type, and this field never contributes a traversal edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a11:ambiguity-adjudication:98bfc2c6ef27ede75714946eaa38ebe9b92541fbd58e123d1e2f8a793ce2d21a",
        slice_id: "a11",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|InternalBaselineDigest||21b9e5bdccf75bbd741eda28d5e4c69babc291f8f7d533e512d3216292eb06b0|1|8a71fbb3663c36e570707db375a602127d9c3671f00da205559672fa509a1509|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a11:1932"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|InternalBaselineDigest"],
        rationale: "a11:1932 defines InternalBaselineDigest as the domain-separated BLAKE3 transcript carried by DeliveredBaselinePayload.internal_baseline_digest. The durable field is digest256 with digest_class=transcript; the capitalized prose name is not a separately encoded schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a11:ambiguity-adjudication:d93a7697a2040a94c9e91b9234c0c9ddf0f411dd9a4f6b67e3273e9205b2be4e",
        slice_id: "a11",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|PublicBaselineDigest||ac08a2a7a1e50ffe2d45b6abff8c0b4bb35099bdc438fe571d407cbe293be21c|1|efc0be4d6f6365de0c0a71d876c26c4adea64aa3a68f493fcff00a947d59569c|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a11:1932"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PublicBaselineDigest"],
        rationale: "a11:1932 defines PublicBaselineDigest as the domain-separated BLAKE3 transcript carried by DeliveredBaselinePayload.public_baseline_digest. Wire visibility does not create a distinct wire schema: the carrier is digest256 with digest_class=transcript.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a11:ambiguity-adjudication:4a22f9a9db5d5ed1f2f62f6512dc1e2bcbeee6d916c46c48d39ef610cf35c613",
        slice_id: "a11",
        ambiguity_source_key: "ambiguity|definition-without-structural-body|PublicDeliveryDigest||4b2a0fd8c888f8b43037adfe7ab086514605976366b8ce5f83d6327dcd6b163f|1|104811d277549ada29c1e06badd494e9224bc66cd8ea652e31a76112bff0a832|definitional prose names a type but supplies no adjacent structural expression",
        source_locations: &["a11:1934"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PublicDeliveryDigest"],
        rationale: "a11:1934 says PublicDeliveryDigest is separately constructed under the exact section 9.5 and Appendix D declassification contract and supplies no structural body here. It names a digest transcript value, not an independently encoded durable schema.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:7989140375531b62a0427337b27000ebb3a319364975a0a7de7635fd2990b18a",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.database_security_namespace_id|27c2c87b407320770f92abe612b2dbea8c1711a0f5fbb2d2464ade311812a4b7|1|8677c9c0a1d9da4b3f075d7489b931065778c6ffe638e14988861f6cf6b177f2|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.database_security_namespace_id|database_security_namespace_id",
        ],
        rationale: "a09:1892: shorthand member `database_security_namespace_id` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `id256`; the derivation is that the a01 database_security_namespace_id family is id256/32 on both landed rows (RootSlot and its bootstrap projection), and this is the same namespace identity. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:5eb46aa4d18d045fbab17d48d4fa4c2e04fe241b6339c21e60007af58b89dfef",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.allocation_epoch|d541ed3794809f0ef5d752c928d72ec1bcfd0c17cea449f10920a3d71b9edd9f|1|c3f7e0f218657bdc349ef2ef87e467a77add5df82107190fca1c77e59b0cb24a|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.allocation_epoch|allocation_epoch",
        ],
        rationale: "a09:1892: shorthand member `allocation_epoch` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that the appendix `*_epoch` family is u64/8 on all seven landed rows. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:5a108c87bcaf7af97c68b4de5140f580e0595077c32b9dc2101ce3020e0386ef",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.predecessor_digest|665396d9fd096bbae767069b80e0508b68fc19c46a0e57788888f2dc48248b24|1|19b500df548d61c71aa239ca707d6725cd4378bd7d508491c09ceb82d8092f3e|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.predecessor_digest|predecessor_digest",
        ],
        rationale: "a09:1892: shorthand member `predecessor_digest` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `WeakDigest`; the derivation is that the appendix nonretaining-predecessor family is WeakDigest/32 with reference_semantics=weak_digest, digest_class=weak_identity and NO target_schema_id on the four applicable landed rows (a03, a06 x2, a10). A16's separately settled plan-named digest is carried as digest256/inline/none and is not a wire-type precedent. The WeakDigest shape here keeps a continuity chain from becoming a retaining edge. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:0fc76e8406b9a839f450c938eaff50007c961724ba28ce1f64977219c65d0d15",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.external_registry_id|6bc3502124fcbdca515e4eac242aaa8e585789ebe77287c6f69a9f052dcc856d|1|9316a06ce50767d4adef5b190f94d0c837a5bb0fd5c76cc0ed125fcdc869d106|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.external_registry_id|external_registry_id",
        ],
        rationale: "a09:1892: shorthand member `external_registry_id` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `id256`; the derivation is that the appendix `*_id` identity family is id256/32, and this names an external registry object by identity rather than by reachability. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:7b44d5b03543471edf15dc99e30e657583e45f02cabaf48e320af54c665eb518",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.cas_version|b9189f9e121d3d4ffb094269c468178916a894df898061f41e68d344b94c3112|1|b1a9f78ea88b9c326aab11def768ab20a0a9557dd0373ac7862e537ff61e2fe7|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.cas_version|cas_version",
        ],
        rationale: "a09:1892: shorthand member `cas_version` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that the sibling a01 RootSlot member `continuity_cas_version`, which the slot registers against this same continuity record, is u64/8; the two must byte-match. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:35c1e0d07f3eb18d887676c63f91a58a9ee0a9a4009183bcd7ec016c3b660932",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.status|073c1634c496cdb649d1afe0a312bbb4b7e1741b271542e4a436c3b8824b1761|1|a0ff2e33d048dc5dbf1c464838eb8f6862680dd61a193291a66c3ba59f22617e|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.status|status",
        ],
        rationale: "a09:1892: shorthand member `status` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u16`; the derivation is that the source names a closed status but spells no arms, so no union may be minted without fabricating them; the appendix closed-discriminant scalar convention is u16/2, and a later arm registration remains an additive-minor change. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:afddde1229bb99bc5b43ecf670fb764381aad05dd58da905f2068c9f7cf78f0b",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.issued_at|4d520ac53d4681a6c63ccae9c86713cd1c366506b5aa2b18fb4eb780db866fd9|1|6a184eaf4041cd4db1ee0521a2428cbd945dcc9f6735213307149b7772e3caf0|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.issued_at|issued_at",
        ],
        rationale: "a09:1892: shorthand member `issued_at` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that it is an opaque instant in the issuing registry's own declared domain, the slice pins no width, and u64/8 is the appendix scalar counter/instant convention. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:f6fead0d3b3c1a3cd04821d4626ca07675c108902deda2d4d50aea669073edf7",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.signer_set_epoch|c19a7f95576885cbbec6e343a4f083831384152b2b98871674234e0002ba8ece|1|0b673f179de73e02c2d3464be7f93694e48fb32baa2f16ee8475216b894dc698|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.signer_set_epoch|signer_set_epoch",
        ],
        rationale: "a09:1892: shorthand member `signer_set_epoch` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that the appendix `*_epoch` family is u64/8 on all seven landed rows. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:3bc7da5d29eaeb33f284b3e0664cbfdcce0645a313b045f01ba12501ed2c26a8",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdentityContinuityRecord|IdentityContinuityRecord.threshold_signatures|b8f26912dae26e6b378bbddcb6cd586deb9c8e0124cbe3d36241043b853706ca|1|0b6923d384e539ad614caa9ac77f82ff4d10217b1fc13aca3708f5273147ffba|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.threshold_signatures|threshold_signatures",
        ],
        rationale: "a09:1892: shorthand member `threshold_signatures` carries no inline exact type inside the `IdentityContinuityRecord` body, and IdentityContinuityRecord is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `bytes`; the derivation is that the appendix `*_signatures` family is bytes/65536 on all six landed rows. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:a56e96e992fa004d53648fff60577968c42e7eb1e76e061ec17ad916d8d3199b",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.lease_id|25200705cd3b9a83f430168138f12797f96abe50f85e871810e9dd56950249e2|1|623a488e9eb256c695b4f059ef267cf94f58472334f6673951a0658b2c4d49d7|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.lease_id|lease_id",
        ],
        rationale: "a09:1892: shorthand member `lease_id` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `id256`; the derivation is that the appendix `*_id` identity family is id256/32. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:3cbeed9a9ea13d29a959320cb4c972acdcc149cb61c03b43b64284e6ba669799",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.graph|eef93e1d14482804277fca0172464032d1a4fdbcc338524059fa1e861454ad4d|1|4533798dfbe9a49dee89a7caa466936d73dda3469563e93e24676741b2adde6a|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.graph|graph",
        ],
        rationale: "a09:1892: shorthand member `graph` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `id256`; the derivation is that it is a graph identity and takes the appendix id256/32 identity convention. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:a5247158af450d3874c3381f2ebb1a4bad5b01381a5db492e66e7ba3dbbd42cc",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.element_kind|aa088cbc779de075858de92bb259728767cb8746340b74c6f0f659846c225194|1|6364cd58f851495d3f6072444d06cab1a325c44c2758f6d982ea9e3f4c694b17|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.element_kind|element_kind",
        ],
        rationale: "a09:1892: shorthand member `element_kind` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u16`; the derivation is that it is a closed element-kind discriminant with no arms spelled in the slice, and the appendix `*_kind` scalar convention is u16/2. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:9a3fd9fdb09fd9fd8034325657de2cadf652ec3713ec11fe97391773744c5249",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.allocation_epoch|d541ed3794809f0ef5d752c928d72ec1bcfd0c17cea449f10920a3d71b9edd9f|1|469e63fb38a7c7d50df51b1f1bab75004b0107611bcabd3010d88b70b07a9d6d|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.allocation_epoch|allocation_epoch",
        ],
        rationale: "a09:1892: shorthand member `allocation_epoch` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that the appendix `*_epoch` family is u64/8 on all seven landed rows. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:99f471783470d1394c5e9b58102a4cbe715c18f5353e9784a913513b859235a7",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.partition|2df962d37ee229c547fb4ebef038a1eaecd0cd6f1c109746f4a9c1fa80329254|1|7c6108fea66dd7a0a06c2f0620185ca9faad96daeffec2a6a62dc2f415fb7244|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.partition|partition",
        ],
        rationale: "a09:1892: shorthand member `partition` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u32`; the derivation is that it is a partition ordinal within the graph's element-kind space, the slice makes no width claim (the 128-bit identity shape is w2-id-allocator's remit), and u32/4 is the appendix ordinal convention. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:24a3d803731ca838564933d4c189d3a334877983cff3b22faf085f146d49bfc4",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.half_open_range|eeb6be1e0f4c411cf21b96555377321300e43f22e511392c371bcb17d337b6bc|1|139a67ab5ae0ae008d03ef4ed71fcab6c2a2916dabd2734f222d9757ee05a3cc|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.half_open_range|half_open_range",
        ],
        rationale: "a09:1892: shorthand member `half_open_range` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `bytes`; the derivation is that it is a [start,end) offset pair for which the appendix registers no range wire type, so bytes/16 carries the canonical pair without minting a fabricated record, and the authenticated transcript covers it either way. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:ba4f137cd4482c720db3387c1f579bdd42a41c65b935a93ac146d42d369a3db5",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.time_authority_profile_oid|7d5d151447263b02b1178e7231040315f6002b33000de6b55f1779f43ef4e45c|1|0babcc1b128fed3dd610762d6f03c6dde986645590e0f1fe4c5001e20215d2fd|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.time_authority_profile_oid|time_authority_profile_oid",
        ],
        rationale: "a09:1892: shorthand member `time_authority_profile_oid` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `oid256`; the derivation is that the appendix `*_oid` family is oid256/32 with identity_class=logical and reference_semantics=weak_digest on both landed non-locator rows (a01 configuration_oid and target_configuration_oid): an authority-group object named by identity and never followed locally. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:f92ecc970f0808864f6330f4bea378f455e73f83eac39266660daf596c273b56",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.issuance_not_before|da519880d46cf6df1ca5371629112c9dfab9dfe2bb692e7334ef360f70ca52ac|1|0144a9339f836b21255a552f3630d42927902ec5b16ce3e408c99bd8f78bfe63|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.issuance_not_before|issuance_not_before",
        ],
        rationale: "a09:1892: shorthand member `issuance_not_before` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that it is a bound in the domain named by the sibling time_authority_profile_oid, the appendix registers no profiled-time wire type, and u64/8 is the scalar instant convention. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:e980cabf89166a1b611e2908e02485c320159dfb33dfb8b56bb34ad71c463614",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.issuance_not_after|8cf89b1793eef3266d1567e68225f252efc925a66afb922cbc7aa616f5328b4a|1|6ca377a14a681a5889b8e00d787b00108f709a752800beb2449e7345acd400a2|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.issuance_not_after|issuance_not_after",
        ],
        rationale: "a09:1892: shorthand member `issuance_not_after` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that it is a bound in the domain named by the sibling time_authority_profile_oid, the appendix registers no profiled-time wire type, and u64/8 is the scalar instant convention. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:8df2a4c61a0630de9609946d1278eead5147d6512dbc752ce2fc5adc9f277e13",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.permanent_spent_commitment|01228fb28bd95f97a6b8ea907f31a525d933bb759796795370180ad6da3ea201|1|04fda4fa15c09836ebdf9c492055d1d1b44febe788d4397798a133288591a44c|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.permanent_spent_commitment|permanent_spent_commitment",
        ],
        rationale: "a09:1892: shorthand member `permanent_spent_commitment` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `digest256`; the derivation is that the appendix `*_commitment` family is digest256/32; digest_class=weak_identity matches a01 source_identity_or_transition_continuity_commitment, a comparison-only commitment that is never a reachability edge. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:4f3f680f6528cfc80f55800a442a3251b4075724849f4cd6740839215556c8bc",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|IdRangeLease|IdRangeLease<Role:AuthorityOwningRole>.body_digest|4a30636305daa7c44f8ca32d2d5fbd90130c1d25f94e7c863108d14ac0ae2c8e|1|8578c8f36068bce9b2b579db75d23e94712515c25d9a0b04c95ab03f5cc680fc|shorthand field has no exact type",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.body_digest|body_digest",
        ],
        rationale: "a09:1892: shorthand member `body_digest` carries no inline exact type inside the `IdRangeLease<Role:AuthorityOwningRole>` body, and IdRangeLease is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `digest256`; the derivation is that the appendix body_digest family is digest256/32 with digest_class=body on nine of ten landed rows; the registered BodyDigest recipe excludes exactly this field's own tag. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:8de057a6c84c235bec83c7ba8237e7f991be0809d8863d347cffcf7932c6ed56",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TxnAllocationBindingRoot|TxnAllocationBindingRoot.generation|e661f4c935e8a5a83349afb5e347695c2e972e967b50efcd618f93b0b7b4c24b|1|413bf65f3f7168677a2366d715f0676ce18e5c4fc6dc80b2ac884060d5f928b4|shorthand field has no exact type",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.generation|generation",
        ],
        rationale: "a09:1900: shorthand member `generation` carries no inline exact type inside the `TxnAllocationBindingRoot` body, and TxnAllocationBindingRoot is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that it is a workspace generation counter and takes the appendix u64/8 scalar counter convention. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:dbc2d9bca1bafc5c870e00da7218b31231b1c64558f67415d710bf1f07c6464d",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TxnAllocationBindingRoot|TxnAllocationBindingRoot.attempt_identity|019debbed6eaf087554490210988f724f1d55f4dfb220b23b999763451f3dd75|1|dc46f014cf816414ce6eddeade40b21db2d31d4db6158377319a25da0cca28fd|shorthand field has no exact type",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.attempt_identity|attempt_identity",
        ],
        rationale: "a09:1900: shorthand member `attempt_identity` carries no inline exact type inside the `TxnAllocationBindingRoot` body, and TxnAllocationBindingRoot is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `WeakDigest`; the derivation is that the appendix weak-identity family is WeakDigest/32 with reference_semantics=weak_digest and digest_class=weak_identity (a04 predecessor_topology_identity, a07 reservation_identity, a19 source_manifest_identity); the attempt is named, never retained. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:4576279cf232c7845c55021c16aaf532138f3ef656c0e1d5532ad1bb978b191a",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TxnAllocationBindingRoot|TxnAllocationBindingRoot.through_statement_seq|d76561cc957c944e48f68de06c06a04367cadad5970a21a1aec710e5732ec277|1|84c53e547eb5276256badeaff5c745e1c47ba9eb7c59a2cbaf13b7ca47e181e1|shorthand field has no exact type",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.through_statement_seq|through_statement_seq",
        ],
        rationale: "a09:1900: shorthand member `through_statement_seq` carries no inline exact type inside the `TxnAllocationBindingRoot` body, and TxnAllocationBindingRoot is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `u64`; the derivation is that the appendix `*_seq` family is u64/8 on all four landed rows. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:43c3e5daf2b3bbd201ad393a918221d7137fd173c7abe6529878909c6e449b02",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TxnAllocationBindingRoot|TxnAllocationBindingRoot.sorted_spent_commitments|befc3a163e8404d6ebb0f347693b2df3f1f1f8df38bebe6e665a8824b56faabc|1|a5a4ca6372d9e4df081009744b80045be68b2425680d78c0ef9983725021a4e9|shorthand field has no exact type",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.sorted_spent_commitments|sorted_spent_commitments",
        ],
        rationale: "a09:1900: shorthand member `sorted_spent_commitments` carries no inline exact type inside the `TxnAllocationBindingRoot` body, and TxnAllocationBindingRoot is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `digest256`; the derivation is that it carries the same commitment values as permanent_spent_commitment as a canonically-sorted set, so digest256/weak_identity with cardinality=many, and the appendix `many` convention caps the aggregate at 16777216. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:a654ba46e405b504506c1ecaf92c7fe0f0e8de107a6da5f174150750abd2da5d",
        slice_id: "a09",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|TxnAllocationBindingRoot|TxnAllocationBindingRoot.root_digest|c08174a56840d52bb6d7b73ddadebb835c6b18ab21f844f75c149905b0fdad76|1|de96030b1a75d7708b17640d780510379c02b0795504bea70c915c610fe53649|shorthand field has no exact type",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.root_digest|root_digest",
        ],
        rationale: "a09:1900: shorthand member `root_digest` carries no inline exact type inside the `TxnAllocationBindingRoot` body, and TxnAllocationBindingRoot is a logical kind, so no wire envelope commits the span and its exact type/cardinality is owned by the registered durable_fields row. That row fixes it to `digest256`; the derivation is that the appendix digest family is digest256/32, and the value commits the binding-root leaf mapping rather than the record's own preceding bytes, so it is digest_class=transcript with a registered recipe, matching a01 signed_transcript_digest rather than a BodyDigest. The single affected census key maps to that source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:9fcf021dbaea4ffe1daf6fb1ceac10df49ebc45b5b43054c0c5cf93eb4471643",
        slice_id: "a15",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|KeyDestroyProposal|KeyDestroyProposal.key_identity|98f155babf41c5cf0afa9139f8a96469570cc31dbd6f19ee80e0e280f313bd76|1|fe7a66011064c18fafdfae45be42a091a85d7403c11a2500cf4bba5d6d402590|shorthand field has no exact type",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.key_identity|key_identity",
        ],
        rationale: "A15 repeatedly requires byte-equal key_identity projections but spells neither a KeyIdentity structural producer nor a StrongRef/typed by-value use that would license one. The owner ruling therefore selects the builtin opaque id256 form, not a fabricated record and not WeakStateIdentity/WeakDigest. Its canonical bytes are BLAKE3(\"fgdb:key-identity:v1\" || canonical(0x0001 database_security_namespace_id:id256, 0x0002 material_class:u16, 0x0003 key_id:id256, 0x0004 key_epoch:u64)); the namespace makes clone identities distinct while same-identity restore remains stable, and excluding physical targets keeps rewrap/replication/relocation from changing the logical key identity.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:9d87b7faa3ae557cc0b0eb4aecf03219a98f5758e0e5607b1fa28c5d7eb3d55f",
        slice_id: "a15",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|KeyDestroyProposal|KeyDestroyProposal.expected_state_conditions|98f99be66a4f7e6dae8d94497c1dec6b36cde1de9f37ac488ffa72adbea0c4fe|1|b0269c814fc42bacf204d73a5e53a2a6bd6f885daed52569cd738538190972c1|shorthand field has no exact type",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.expected_state_conditions|expected_state_conditions",
        ],
        rationale: "a15:2059 omits the repeated shared type spelling for readability, while a10:1912 fixes the exact comparison-only many-valued arm set as WeakStateIdentity | WeakMarkerIdentity | ExpectedEpoch | ExpectedIndex and a10:1913 gives that shared source-ordered union its durable name ExpectedStateCondition. The A15 field therefore maps to that shared union rather than an A15-local substitute.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:d14591312dc555272847ff5149204b97bfc8f5ff4ceef5736d45d6b91bf71dda",
        slice_id: "a15",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|KeyDestroyProposal|KeyDestroyProposal.terminal_audit_gate|03aa4f26c42ab045c593f45f87d78e0aa42c1f1e03198d96e839894c7ea7fb6f|1|69deea536c4e2a4367d3d22c27e70a3be3cc6b5d974b443f5109a571a5466530|shorthand field has no exact type",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.terminal_audit_gate|terminal_audit_gate",
        ],
        rationale: "a15:2059 uses the compact shorthand `terminal_audit_gate`; the a10:1914 SequenceNeutralSpec law requires exactly one TerminalAuditGate even where compact Appendix prose omits its type, and a21:2699 fixes the exact source-ordered StructurallyInapplicable | NotRequired | Required union. Prose omission is not absence and does not authorize a local gate alias.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:670d6a3c6e1369d3bc38d6f3076252157264e695e05b76984089286770501c7b",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LocationForm|LocationForm.ContiguousSpan.encoded_len|f62cdb86eea76acd78c8a2d88de6530dc788e524a08d3bd93597a7e1d76ddb2f|1|07d2e4fc78259c76944ac73467587e576594b8a2325c96aa42d57a63728c3d6c|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.encoded_len|encoded_len",
        ],
        rationale: "a02:1449: the shorthand encoded_len member is committed byte-exactly by the registered LocationForm ContiguousSpan arm payload digest; the single affected census key maps to that arm-interior source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:cecf9025af9662200ebec0ca362365459431e220b7d0b989415630838a42ce3d",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LocationForm|LocationForm.ContiguousSpan.failure_domain_id|b848bfdde1d634386de45aa3db6e6082c98954ee49caf3c79fea79ed511b6216|1|9626e9ce8d54d89e97bd245ae8d1ca4b8d84d773ab0b52f74603133b078643bc|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.failure_domain_id|failure_domain_id",
        ],
        rationale: "a02:1449: the shorthand failure_domain_id member is committed byte-exactly by the registered LocationForm ContiguousSpan arm payload digest; the single affected census key maps to that arm-interior source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:1add14f68841c209d9602d82b0d4974f1c7e0307bdc4e70a11b7805ab03369fb",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LocationForm|LocationForm.ContiguousSpan.offset|c1d60fe5815fb5b2d8eadc7c686a2ef228818eb63d14a58be0336c2d4dc3bb65|1|b4352fe740391bb11f5e6ba734ca50b9411f912bb11d37a693ea498c154d8fce|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|LocationForm|LocationForm.ContiguousSpan.offset|offset"],
        rationale: "a02:1449: the shorthand offset member is committed byte-exactly by the registered LocationForm ContiguousSpan arm payload digest; the single affected census key maps to that arm-interior source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:cf1828242c9de970216388ce83137d5debc49b7b633a229d89e52d5409e41d76",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LocationForm|LocationForm.ContiguousSpan.segment_id|067de3ca3742734d7c0fa30cab44ba12e439e6fdaf5a356d04eb34d144a48ad5|1|d9a60afcd14755ce9a68954b370ada06d91ca9c9f313cf3ca1c5e0009a2e4c5a|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.segment_id|segment_id",
        ],
        rationale: "a02:1449: the shorthand segment_id member is committed byte-exactly by the registered LocationForm ContiguousSpan arm payload digest; the single affected census key maps to that arm-interior source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:d73d40486b24cb361fcb957a65f0ceecc6a2c33196af11ea8ce9c8ef527936a7",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LocationForm|LocationForm.ContiguousSpan.symbol_inventory_digest|76c183900154b95828c0ade4981fa8269fdfffbad83471425f467a23d1e81416|1|6c633af6219e605d4300160abddade3be536067603c6da4e10bc4a259c1ed994|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.symbol_inventory_digest|symbol_inventory_digest",
        ],
        rationale: "a02:1449: the shorthand symbol_inventory_digest member is committed byte-exactly by the registered LocationForm ContiguousSpan arm payload digest; the single affected census key maps to that arm-interior source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f889bf772824c1a9353bbd3b25ee290a6aca8bba2a931a9fc09f3018a0ff3355",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LocationForm|LocationForm.Explicit.failure_domains|3f3d759ddf7f238e5931f429d410022e227f5d84c9522fae6abf88088d6f1852|1|717f0fd1684324699149e55c86a7e7d2ae3c9d12be1ce93873020c0155f03b28|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.Explicit.failure_domains|failure_domains",
        ],
        rationale: "a02:1449: the shorthand failure_domains member is committed byte-exactly by the registered LocationForm Explicit arm payload digest; the single affected census key maps to that arm-interior source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:3186d0debf2fb0589fc0c814cd95c8fec9e6c4c4e3b1a3e09b8f284bee7515ae",
        slice_id: "a02",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|LocationForm|LocationForm.Explicit.sorted_symbol_inventory_and_locators|44937ad7c4727b845192190b304679f6c4b9de05dc753b0d4ef253b65849247d|1|8778d463045738849c0d2164c71dcecfd6d72c5736c13365fe885fbe3c50e36c|shorthand field has no exact type",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.Explicit.sorted_symbol_inventory_and_locators|sorted_symbol_inventory_and_locators",
        ],
        rationale: "a02:1449: the shorthand sorted_symbol_inventory_and_locators member is committed byte-exactly by the registered LocationForm Explicit arm payload digest; the single affected census key maps to that arm-interior source form.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a07:ambiguity-adjudication:ee23aced90506d99111b719ae0f8486df181ed161f5ec8a12c8214f574341d65",
        slice_id: "a07",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|GlobalTxnOutcomePreparationRecord|GlobalTxnOutcomePreparationRecord.expected_registered_outcome_digest|4cdd6ebe699aa3b89b0893897f37ab3454f3b8987b0f8b79608e8ff56e081377|1|ad30e2526f4a279dd72dd315bfabe92febf7e04cd8c027fe4307fc355cfbcbc0|shorthand field has no exact type",
        source_locations: &["a07:1780"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|GlobalTxnOutcomePreparationRecord|GlobalTxnOutcomePreparationRecord.expected_registered_outcome_digest|expected_registered_outcome_digest",
        ],
        rationale: "a07:1780 explicitly defines a comparison-only expected Registered predecessor digest; it maps to the source-ordered digest256 field and creates no retention or construction edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:37232bd950b2c30115d0e2e9a2c861fbf52ee2e33dfeff50914c944c05927b86",
        slice_id: "a08",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|NoTerminalSignatureOrOrderProof|NoTerminalSignatureOrOrderProof.freeze_digest|c00af86955aba5af4b7a66fbf8f4effb8029b4520f356181b08c41c24726f57d|1|923fb1d5ce42dfc81fff1073d744ce39733b18c2769fcd352e8185a23eba9e27|shorthand field has no exact type",
        source_locations: &["a08:1838"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|NoTerminalSignatureOrOrderProof|NoTerminalSignatureOrOrderProof.freeze_digest|freeze_digest",
        ],
        rationale: "a08:1838 explicitly defines a comparison-only ReleasePending freeze digest; it maps to the source-ordered digest256 field and creates no retention or construction edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a13:ambiguity-adjudication:3b42cf9567870731386d634a72ce4198def5da7fa1007e561b11c740bd67e521",
        slice_id: "a13",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|KeyEnvelopeNode|KeyEnvelopeNode.inherited_roots.record.source_root_digest|eda96c96e62e2160110a85f02a2152fcba4da71b149f5a700a19f8b7d8f0b247|1|58807e0144c0e5c466ff6ab229699cac2322548e3ee7d276c66c5512d356afd2|shorthand field has no exact type",
        source_locations: &["a13:2006"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyEnvelopeNode|KeyEnvelopeNode.inherited_roots.record.source_root_digest|source_root_digest",
        ],
        rationale: "a13:2006 explicitly defines the inherited source-root identity as a comparison-only digest; it maps to the source-ordered digest256 field and creates no retention or construction edge.",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a13:ambiguity-adjudication:3d70fb474e157bb474917cb69259eb2374ba0bc450888830e9b9d4790efa4da3",
        slice_id: "a13",
        ambiguity_source_key: "ambiguity|field-type-ambiguous|KeyEnvelopeNode|KeyEnvelopeNode.inherited_roots.record.source_root_ciphertext_digest|83204203c8c59d3ab8f3d9d1dfb493f2c193dd411b9c1c18c8b2184026b72176|1|fc2f15f8bf55ca659ff0fe5b9308d88f0c057d6c2ba375d6e51c0230693412fb|shorthand field has no exact type",
        source_locations: &["a13:2006"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyEnvelopeNode|KeyEnvelopeNode.inherited_roots.record.source_root_ciphertext_digest|source_root_ciphertext_digest",
        ],
        rationale: "a13:2006 explicitly defines the selected inherited-root ciphertext realization as a comparison-only digest; it maps to the source-ordered digest256 field and creates no retention or construction edge.",
    },
];

pub const PROJECTION_CLASSES: [&str; 6] = [
    "logical_object_kinds",
    "physical_record_kinds",
    "bootstrap_frames",
    "prebootstrap_artifact_kinds",
    "wire_types",
    "durable_fields",
];

pub const PROJECTION_FILES: [(&str, &str); 6] = [
    ("logical_object_kinds", "logical_object_kinds.toml"),
    ("physical_record_kinds", "physical_record_kinds.toml"),
    ("bootstrap_frames", "bootstrap_frames.toml"),
    (
        "prebootstrap_artifact_kinds",
        "prebootstrap_artifact_kinds.toml",
    ),
    ("wire_types", "wire_types.toml"),
    ("durable_fields", "durable_fields.toml"),
];

const ROOT_KEYS: [&str; 28] = [
    "schema_version",
    "catalog",
    "source_manifest",
    "reference_manifest",
    "target_manifest",
    "maintenance_proof",
    "completion_layer",
    "slice",
    "projection_epoch",
    "reservation",
    "logical_kind",
    "physical_kind",
    "bootstrap_frame",
    "prebootstrap_kind",
    "wire_type",
    "field",
    "union",
    "union_arm",
    "reference_union",
    "reference_union_arm",
    "top_level_candidate",
    "target",
    "annotation",
    "semantic_binding",
    "expansion_binding",
    "evidence",
    "ambiguity_adjudication",
    "source_symbol_disposition",
];

const CATALOG_KEYS: [&str; 7] = [
    "name",
    "catalog_epoch",
    "row_id_grammar_version",
    "canonical_order",
    "diagnostic_version",
    "hash_algorithm",
    "source_encoding",
];

const SOURCE_MANIFEST_KEYS: [&str; 8] = [
    "plan_path",
    "start_line",
    "end_line",
    "line_count",
    "byte_count",
    "sha256",
    "heading",
    "next_heading",
];

const REFERENCE_MANIFEST_KEYS: [&str; 4] = [
    "target_count",
    "target_ids_sha256",
    "occurrence_count",
    "occurrence_transcript_sha256",
];

const TARGET_MANIFEST_KEYS: [&str; 3] = [
    "target_count",
    "projection_fallback_count",
    "target_source_assignment_sha256",
];

const SLICE_KEYS: [&str; 23] = [
    "ordinal",
    "id",
    "bead_id",
    "title",
    "start_line",
    "end_line",
    "line_count",
    "byte_count",
    "sha256",
    "predecessor",
    "successor",
    "expected_projection_classes",
    "definition_status",
    "top_level_candidate_count",
    "top_level_candidate_ids_sha256",
    "field_candidate_count",
    "field_candidate_ids_sha256",
    "union_candidate_count",
    "union_candidate_ids_sha256",
    "arm_candidate_count",
    "arm_candidate_ids_sha256",
    "ambiguity_count",
    "ambiguity_ids_sha256",
];

const MAINTENANCE_PROOF_KEYS: [&str; 9] = [
    "row_id",
    "owner_bead_id",
    "owner_crate",
    "covered_artifacts",
    "checker_ids",
    "scenario_ids",
    "event_ids",
    "gate_ids",
    "evidence_status",
];
const COMPLETION_LAYER_KEYS: [&str; 9] = [
    "layer",
    "schema_version",
    "field_contracts",
    "target_binding",
    "target_cardinality",
    "epoch_domain",
    "projection_policy",
    "authoring_policy",
    "pin_policy",
];
const PROJECTION_EPOCH_KEYS: [&str; 2] = ["registry", "registry_epoch"];
const CATALOG_ROW_KEYS: [&str; 2] = ["slice_id", "row_id"];
const RESERVATION_KEYS: [&str; 7] = [
    "row_id",
    "slice_id",
    "symbol",
    "row_kind",
    "identity_class",
    "code_reservation",
    "disposition",
];
const TOP_LEVEL_CANDIDATE_KEYS: [&str; 8] = [
    "row_id",
    "slice_id",
    "symbol",
    "generic_signature",
    "source_key",
    "source_kind",
    "identity_class",
    "source_locations",
];
const TARGET_KEYS: [&str; 6] = [
    "row_id",
    "target_row_id",
    "slice_id",
    "source_key",
    "target_kind",
    "definition_status",
];
const ANNOTATION_KEYS: [&str; 19] = [
    "row_id",
    "target_row_id",
    "exact_type",
    "cardinality",
    "layout",
    "role",
    "posture",
    "authority",
    "locality",
    "generic_expansions",
    "role_expansions",
    "reference_semantics",
    "target_schema_ids",
    "construction_order",
    "retention_and_cut_rule",
    "digest_recipe",
    "redaction_class",
    "resource_bounds",
    "compatibility",
];
const SEMANTIC_BINDING_KEYS: [&str; 6] = [
    "row_id",
    "target_row_id",
    "owner_bead_id",
    "owner_crate",
    "owner_status",
    "consumer_crates",
];
const EXPANSION_BINDING_KEYS: [&str; 7] = [
    "row_id",
    "target_row_id",
    "parameter_ordinal",
    "formal",
    "formal_class",
    "values",
    "rationale",
];
const EVIDENCE_KEYS: [&str; 10] = [
    "row_id",
    "target_row_id",
    "evidence_id",
    "phase",
    "status",
    "owner_bead_id",
    "checker_ids",
    "scenario_ids",
    "event_ids",
    "gate_ids",
];
const SOURCE_SYMBOL_DISPOSITION_KEYS: [&str; 5] = [
    "row_id",
    "slice_id",
    "symbol",
    "disposition",
    "source_locations",
];
const AMBIGUITY_ADJUDICATION_KEYS: [&str; 7] = [
    "row_id",
    "slice_id",
    "ambiguity_source_key",
    "source_locations",
    "resolution",
    "resolved_source_keys",
    "rationale",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    pub schema_version: i64,
    pub name: String,
    pub catalog_epoch: i64,
    pub row_id_grammar_version: i64,
    pub canonical_order: String,
    pub diagnostic_version: i64,
    pub hash_algorithm: String,
    pub source_encoding: String,
    pub source_manifest: SourceManifest,
    pub reference_manifest: ReferenceManifest,
    pub target_manifest: TargetManifest,
    pub maintenance_proof: MaintenanceProof,
    pub completion_layers: Vec<CompletionLayerSchema>,
    pub slices: Vec<Slice>,
    pub projection_epochs: BTreeMap<String, i64>,
    pub identity: IdentityRegistries,
    pub projection_rows: Vec<ProjectionRowMeta>,
    pub reservations: Vec<Reservation>,
    pub top_level_candidates: Vec<TopLevelCandidate>,
    pub targets: Vec<Target>,
    pub annotations: Vec<Annotation>,
    pub semantic_bindings: Vec<SemanticBinding>,
    pub expansion_bindings: Vec<ExpansionBinding>,
    pub evidence: Vec<EvidenceBinding>,
    pub ambiguity_adjudications: Vec<AmbiguityAdjudication>,
    pub source_symbol_dispositions: Vec<SourceSymbolDisposition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceProof {
    pub row_id: String,
    pub owner_bead_id: String,
    pub owner_crate: String,
    pub covered_artifacts: Vec<String>,
    pub checker_ids: Vec<String>,
    pub scenario_ids: Vec<String>,
    pub event_ids: Vec<String>,
    pub gate_ids: Vec<String>,
    pub evidence_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionLayerSchema {
    pub layer: String,
    pub schema_version: i64,
    pub field_contracts: Vec<String>,
    pub target_binding: String,
    pub target_cardinality: String,
    pub epoch_domain: String,
    pub projection_policy: String,
    pub authoring_policy: String,
    pub pin_policy: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRowMeta {
    pub projection: String,
    pub row_kind: String,
    pub slice_id: String,
    pub row_id: String,
    pub canonical_suffix: String,
    pub canonical_symbol: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub row_id: String,
    pub slice_id: String,
    pub symbol: String,
    pub row_kind: String,
    pub identity_class: String,
    pub code_reservation: String,
    pub disposition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelCandidate {
    pub row_id: String,
    pub slice_id: String,
    pub symbol: String,
    pub generic_signature: String,
    pub source_key: String,
    pub source_kind: String,
    pub identity_class: String,
    pub source_locations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub row_id: String,
    pub target_row_id: String,
    pub slice_id: String,
    pub source_key: String,
    pub target_kind: String,
    pub definition_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub row_id: String,
    pub target_row_id: String,
    pub exact_type: String,
    pub cardinality: String,
    pub layout: String,
    pub role: String,
    pub posture: String,
    pub authority: String,
    pub locality: String,
    pub generic_expansions: Vec<String>,
    pub role_expansions: Vec<String>,
    pub reference_semantics: String,
    pub target_schema_ids: Vec<String>,
    pub construction_order: String,
    pub retention_and_cut_rule: String,
    pub digest_recipe: String,
    pub redaction_class: String,
    pub resource_bounds: String,
    pub compatibility: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticBinding {
    pub row_id: String,
    pub target_row_id: String,
    pub owner_bead_id: String,
    pub owner_crate: String,
    pub owner_status: String,
    pub consumer_crates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionBinding {
    pub row_id: String,
    pub target_row_id: String,
    pub parameter_ordinal: i64,
    pub formal: String,
    pub formal_class: String,
    pub values: Vec<String>,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceBinding {
    pub row_id: String,
    pub target_row_id: String,
    pub evidence_id: String,
    pub phase: String,
    pub status: String,
    pub owner_bead_id: String,
    pub checker_ids: Vec<String>,
    pub scenario_ids: Vec<String>,
    pub event_ids: Vec<String>,
    pub gate_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSymbolDisposition {
    pub row_id: String,
    pub slice_id: String,
    pub symbol: String,
    pub disposition: String,
    pub source_locations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguityAdjudication {
    pub row_id: String,
    pub slice_id: String,
    pub ambiguity_source_key: String,
    pub source_locations: Vec<String>,
    pub resolution: String,
    pub resolved_source_keys: Vec<String>,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceManifest {
    pub plan_path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub line_count: i64,
    pub byte_count: i64,
    pub sha256: String,
    pub heading: String,
    pub next_heading: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceManifest {
    pub target_count: i64,
    pub target_ids_sha256: String,
    pub occurrence_count: i64,
    pub occurrence_transcript_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetManifest {
    pub target_count: i64,
    pub projection_fallback_count: i64,
    pub target_source_assignment_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    pub ordinal: i64,
    pub id: String,
    pub bead_id: String,
    pub title: String,
    pub start_line: i64,
    pub end_line: i64,
    pub line_count: i64,
    pub byte_count: i64,
    pub sha256: String,
    pub predecessor: String,
    pub successor: String,
    pub expected_projection_classes: Vec<String>,
    pub definition_status: String,
    pub top_level_candidate_count: i64,
    pub top_level_candidate_ids_sha256: String,
    pub field_candidate_count: i64,
    pub field_candidate_ids_sha256: String,
    pub union_candidate_count: i64,
    pub union_candidate_ids_sha256: String,
    pub arm_candidate_count: i64,
    pub arm_candidate_ids_sha256: String,
    pub ambiguity_count: i64,
    pub ambiguity_ids_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlicePin {
    pub ordinal: i64,
    pub id: &'static str,
    pub bead_id: &'static str,
    pub title: &'static str,
    pub start_line: i64,
    pub end_line: i64,
    pub line_count: i64,
    pub byte_count: i64,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: String,
    pub row_id: String,
    pub msg: String,
}

impl Violation {
    fn new(code: &str, row_id: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            row_id: row_id.into(),
            msg: msg.into(),
        }
    }
}

pub const SLICE_PINS: [SlicePin; 21] = [
    SlicePin {
        ordinal: 1,
        id: "a01",
        bead_id: "fgdb-a01-reference-roots-2k0q",
        title: "Appendix A exact catalog: Reference semantics, RootSlot, and RootBootstrap",
        start_line: 1388,
        end_line: 1444,
        line_count: 57,
        byte_count: 23_172,
        sha256: "102b572835f29cfa6b8ec5d22a5a2ef9a9c9cd8d0998f4136a914b031812b25b",
    },
    SlicePin {
        ordinal: 2,
        id: "a02",
        bead_id: "fgdb-a02-filesystem-cipher-dsi3",
        title: "Appendix A exact catalog: Filesystem, cipher, encoding, placement, and symbols",
        start_line: 1445,
        end_line: 1463,
        line_count: 19,
        byte_count: 5_156,
        sha256: "b76c0e0fe81b096a2a647ba4d907dc9edac646e2cd3b721ec62802e5170e8c60",
    },
    SlicePin {
        ordinal: 3,
        id: "a03",
        bead_id: "fgdb-a03-local-state-txn-rxjg",
        title: "Appendix A exact catalog: Local logical state and durable transaction formats",
        start_line: 1464,
        end_line: 1543,
        line_count: 80,
        byte_count: 69_131,
        sha256: "40b90aded14032e8d8d5a8173ab2fe8088d43bfbc33e5d04ffec6fa43612d574",
    },
    SlicePin {
        ordinal: 4,
        id: "a04",
        bead_id: "fgdb-a04-manifest-raft-4tgi",
        title: "Appendix A exact catalog: RootManifest, configuration, Raft, and cross-group trust prelude",
        start_line: 1544,
        end_line: 1589,
        line_count: 46,
        byte_count: 30_907,
        sha256: "ecd43f46a9ffd2be372922bf81bf589ab625778eef10d5d13828aa5939b37c2d",
    },
    SlicePin {
        ordinal: 5,
        id: "a05",
        bead_id: "fgdb-a05-w12-role-transition-wjj2",
        title: "Appendix A exact catalog: W12 Genesis, role transition, and activation formats",
        start_line: 1590,
        end_line: 1658,
        line_count: 69,
        byte_count: 61_610,
        sha256: "cd20fc0a748b360856af14324c0d5e03b087b4dea68e673b351fc1ed59e8dd2d",
    },
    SlicePin {
        ordinal: 6,
        id: "a06",
        bead_id: "fgdb-a06-w12-core-zdzx",
        title: "Appendix A exact catalog: W12 Meta and Shard semantic core formats",
        start_line: 1659,
        end_line: 1700,
        line_count: 42,
        byte_count: 38_209,
        sha256: "e6c1d5924a456b28fceddabfd3b6269a446af214f595794d943483e6b326aa68",
    },
    SlicePin {
        ordinal: 7,
        id: "a07",
        bead_id: "fgdb-a07-w12-txn-results-yt4z",
        title: "Appendix A exact catalog: W12 transaction, statement, result, and outcome formats",
        start_line: 1701,
        end_line: 1790,
        line_count: 90,
        byte_count: 87_484,
        sha256: "8faa24e7397c7df244cb52ffb5332d4a828d5b014e71efc844aef0ebea32b44f",
    },
    SlicePin {
        ordinal: 8,
        id: "a08",
        bead_id: "fgdb-a08-w12-lifecycle-pr7j",
        title: "Appendix A exact catalog: W12 retention, compaction, reconfiguration, GC, and topology formats",
        start_line: 1791,
        end_line: 1889,
        line_count: 99,
        byte_count: 92_259,
        sha256: "31b52a7080dbfa02e09c582857066e194ccb03782f54ee1ac76dcc5b16fe329a",
    },
    SlicePin {
        ordinal: 9,
        id: "a09",
        bead_id: "fgdb-a09-storage-identity-02tl",
        title: "Appendix A exact catalog: Strata run, identity continuity, allocator, and lease formats",
        start_line: 1890,
        end_line: 1909,
        line_count: 20,
        byte_count: 12_328,
        sha256: "eea5d9f7257bfefee5cae1077bbe3f17d4948267736dcd79e24d530f2a1873df",
    },
    SlicePin {
        ordinal: 10,
        id: "a10",
        bead_id: "fgdb-a10-command-delta-ooy1",
        title: "Appendix A exact catalog: Committed effects, commands, and logical delta formats",
        start_line: 1910,
        end_line: 1931,
        line_count: 22,
        byte_count: 16_993,
        sha256: "e9d0ae8d2638e7af2889ffe9f6bf52b54e4dfb42453543175a4d29b82d0136c9",
    },
    SlicePin {
        ordinal: 11,
        id: "a11",
        bead_id: "fgdb-a11-delivery-markers-sdh6",
        title: "Appendix A exact catalog: Delivery cursors, envelopes, markers, and physical batching",
        start_line: 1932,
        end_line: 1963,
        line_count: 32,
        byte_count: 7_956,
        sha256: "6efc3cb10c5e8755ae149b92c6189743a9604f222e160a889787d5ba0e7441e3",
    },
    SlicePin {
        ordinal: 12,
        id: "a12",
        bead_id: "fgdb-a12-checkpoint-resources-m9jz",
        title: "Appendix A exact catalog: Checkpoint, retention, constraint, and resource formats",
        start_line: 1964,
        end_line: 1999,
        line_count: 36,
        byte_count: 19_488,
        sha256: "1d9f07d6ccc7c5feb548224d9e5f38ef216143c1dfd63f95ebcf6e84907b76c6",
    },
    SlicePin {
        ordinal: 13,
        id: "a13",
        bead_id: "fgdb-a13-branch-merge-g2ko",
        title: "Appendix A exact catalog: Branch manifest, key grants, retirement, and merge formats",
        start_line: 2000,
        end_line: 2034,
        line_count: 35,
        byte_count: 17_287,
        sha256: "66aecae3d6e14a7c5c2e806f33b988efd343d2df6a3288a3df50e45f79461851",
    },
    SlicePin {
        ordinal: 14,
        id: "a14",
        bead_id: "fgdb-a14-ha-payload-gc-jb82",
        title: "Appendix A exact catalog: Payload availability, configuration floors, and GC epoch formats",
        start_line: 2035,
        end_line: 2056,
        line_count: 22,
        byte_count: 17_540,
        sha256: "de90db8cd87f7b9c4b168ed9357580ffb8e9c64f60ef643ba48f872daf566e93",
    },
    SlicePin {
        ordinal: 15,
        id: "a15",
        bead_id: "fgdb-a15-key-backup-n77c",
        title: "Appendix A exact catalog: Key destruction, backup, publication, and release formats",
        start_line: 2057,
        end_line: 2156,
        line_count: 100,
        byte_count: 80_209,
        sha256: "f781cb54e7df62ad8d695ec62cefa43aeff86d7b91d4e4e21d308d0b07cfa325",
    },
    SlicePin {
        ordinal: 16,
        id: "a16",
        bead_id: "fgdb-a16-time-authority-ytub",
        title: "Appendix A exact catalog: Rollback-protected authority-time formats and rotation",
        start_line: 2157,
        end_line: 2246,
        line_count: 90,
        byte_count: 69_849,
        sha256: "375bd6ee859b02129bb32d3f15a659158d3665a56b990509874e0da076a0d6ad",
    },
    SlicePin {
        ordinal: 17,
        id: "a17",
        bead_id: "fgdb-a17-restore-prebootstrap-hy9w",
        title: "Appendix A exact catalog: Restore prebootstrap journal and source acquisition formats",
        start_line: 2247,
        end_line: 2348,
        line_count: 102,
        byte_count: 72_597,
        sha256: "660aaee44fbc117b6f49156c9f95ec3e1843d9ae171e54f4e08daf435c456cd5",
    },
    SlicePin {
        ordinal: 18,
        id: "a18",
        bead_id: "fgdb-a18-restore-registry-exjt",
        title: "Appendix A exact catalog: Restore registry, cleanup, terminal history, and abandonment formats",
        start_line: 2349,
        end_line: 2458,
        line_count: 110,
        byte_count: 94_976,
        sha256: "5fc84607d338774d06f4dcd1a0aed6f48165f427c7da4716b90d4bbbc949161c",
    },
    SlicePin {
        ordinal: 19,
        id: "a19",
        bead_id: "fgdb-a19-restore-readiness-fd0j",
        title: "Appendix A exact catalog: Restore lease barrier, reservations, bridge, and readiness formats",
        start_line: 2459,
        end_line: 2574,
        line_count: 116,
        byte_count: 77_017,
        sha256: "65c5015fa2243b33579d5b1b6d78ac3e0d55f9b0fcc10c482bbc02a2ebd4d9c0",
    },
    SlicePin {
        ordinal: 20,
        id: "a20",
        bead_id: "fgdb-a20-restore-promotion-ivsp",
        title: "Appendix A exact catalog: Restore promotion, independent reopen, completion, and release formats",
        start_line: 2575,
        end_line: 2608,
        line_count: 34,
        byte_count: 22_805,
        sha256: "6f1b942c046041d3ecefb159e0e86b30a673f03ca86b44dac921ad98ef07a064",
    },
    SlicePin {
        ordinal: 21,
        id: "a21",
        bead_id: "fgdb-a21-replay-security-ye0o",
        title: "Appendix A exact catalog: Replay, authorization, capability, DP, audit, and transparency formats",
        start_line: 2609,
        end_line: 2728,
        line_count: 120,
        byte_count: 105_489,
        sha256: "af85ba1bf3128769a81c3f83c1f0a77543c3f2df14bbf86f22f37cd356b29dae",
    },
];

/// Parse one canonical catalog from the repository's strict TOML subset.
pub fn parse_catalog(text: &str) -> Result<Catalog, Vec<Violation>> {
    let catalog = parse_catalog_structural(text)?;
    enforce_catalog_semantics(catalog)
}

fn parse_catalog_structural(text: &str) -> Result<Catalog, Vec<Violation>> {
    let root = match toml::parse(text) {
        Ok(root) => root,
        Err(error) => {
            return Err(vec![Violation::new(
                "catalog_toml_parse",
                "catalog",
                error.to_string(),
            )]);
        }
    };

    let mut violations = Vec::new();
    exact_keys(&root, &ROOT_KEYS, "catalog", &mut violations);

    let schema_version = read_int(&root, "schema_version", "catalog", &mut violations);
    let catalog_table = read_table(&root, "catalog", "catalog", &mut violations);
    let manifest_table = read_table(&root, "source_manifest", "catalog", &mut violations);
    let reference_manifest_table =
        read_table(&root, "reference_manifest", "catalog", &mut violations);
    let target_manifest_table = read_table(&root, "target_manifest", "catalog", &mut violations);
    let maintenance_table = read_table(&root, "maintenance_proof", "catalog", &mut violations);

    let header = catalog_table.and_then(|table| {
        exact_keys(table, &CATALOG_KEYS, "catalog", &mut violations);
        let name = read_string(table, "name", "catalog", &mut violations);
        let catalog_epoch = read_int(table, "catalog_epoch", "catalog", &mut violations);
        let row_id_grammar_version =
            read_int(table, "row_id_grammar_version", "catalog", &mut violations);
        let canonical_order = read_string(table, "canonical_order", "catalog", &mut violations);
        let diagnostic_version = read_int(table, "diagnostic_version", "catalog", &mut violations);
        let hash_algorithm = read_string(table, "hash_algorithm", "catalog", &mut violations);
        let source_encoding = read_string(table, "source_encoding", "catalog", &mut violations);
        match (
            name,
            catalog_epoch,
            row_id_grammar_version,
            canonical_order,
            diagnostic_version,
            hash_algorithm,
            source_encoding,
        ) {
            (
                Some(name),
                Some(catalog_epoch),
                Some(row_id_grammar_version),
                Some(canonical_order),
                Some(diagnostic_version),
                Some(hash_algorithm),
                Some(source_encoding),
            ) => Some((
                name,
                catalog_epoch,
                row_id_grammar_version,
                canonical_order,
                diagnostic_version,
                hash_algorithm,
                source_encoding,
            )),
            _ => None,
        }
    });

    let source_manifest = manifest_table.and_then(|table| {
        exact_keys(
            table,
            &SOURCE_MANIFEST_KEYS,
            "source_manifest",
            &mut violations,
        );
        parse_source_manifest(table, &mut violations)
    });
    let reference_manifest = reference_manifest_table.and_then(|table| {
        exact_keys(
            table,
            &REFERENCE_MANIFEST_KEYS,
            "reference_manifest",
            &mut violations,
        );
        parse_reference_manifest(table, &mut violations)
    });
    let target_manifest = target_manifest_table.and_then(|table| {
        exact_keys(
            table,
            &TARGET_MANIFEST_KEYS,
            "target_manifest",
            &mut violations,
        );
        parse_target_manifest(table, &mut violations)
    });
    let maintenance_proof = maintenance_table.and_then(|table| {
        exact_keys(
            table,
            &MAINTENANCE_PROOF_KEYS,
            "maintenance_proof",
            &mut violations,
        );
        parse_maintenance_proof(table, &mut violations)
    });
    let completion_layers = parse_completion_layers(&root, &mut violations);

    let slice_tables = read_table_array(&root, "slice", "catalog", &mut violations);
    let mut slices = Vec::new();
    if let Some(tables) = slice_tables {
        for (index, table) in tables.iter().enumerate() {
            let row_id = format!("slice[{index}]");
            exact_keys(table, &SLICE_KEYS, &row_id, &mut violations);
            if let Some(slice) = parse_slice(table, &row_id, &mut violations) {
                slices.push(slice);
            }
        }
    }

    let projection_epochs = parse_projection_epochs(&root, &mut violations);
    let projection_data = projection_epochs
        .as_ref()
        .and_then(|epochs| parse_identity_projections(&root, epochs, &mut violations));
    let reservations = parse_reservations(&root, &mut violations);
    let top_level_candidates = parse_top_level_candidates(&root, &mut violations);
    let targets = parse_targets(&root, &mut violations);
    let annotations = parse_annotations(&root, &mut violations);
    let semantic_bindings = parse_semantic_bindings(&root, &mut violations);
    let expansion_bindings = parse_expansion_bindings(&root, &mut violations);
    let evidence = parse_evidence(&root, &mut violations);
    let ambiguity_adjudications = parse_ambiguity_adjudications(&root, &mut violations);
    let source_symbol_dispositions = parse_source_symbol_dispositions(&root, &mut violations);

    if !violations.is_empty() {
        sort_violations(&mut violations);
        return Err(violations);
    }

    let Some(schema_version) = schema_version else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "catalog",
            "schema_version was not constructed",
        )]);
    };
    let Some((
        name,
        catalog_epoch,
        row_id_grammar_version,
        canonical_order,
        diagnostic_version,
        hash_algorithm,
        source_encoding,
    )) = header
    else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "catalog",
            "catalog header was not constructed",
        )]);
    };
    let Some(source_manifest) = source_manifest else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "source_manifest",
            "source manifest was not constructed",
        )]);
    };
    let Some(reference_manifest) = reference_manifest else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "reference_manifest",
            "reference manifest was not constructed",
        )]);
    };
    let Some(target_manifest) = target_manifest else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "target_manifest",
            "target manifest was not constructed",
        )]);
    };
    let Some(maintenance_proof) = maintenance_proof else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "maintenance_proof",
            "maintenance proof was not constructed",
        )]);
    };
    let Some(completion_layers) = completion_layers else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "completion_layer",
            "completion layer schemas were not constructed",
        )]);
    };
    let Some(projection_epochs) = projection_epochs else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "projection_epoch",
            "projection epochs were not constructed",
        )]);
    };
    let Some((identity, projection_rows)) = projection_data else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "projection_rows",
            "identity projections were not constructed",
        )]);
    };
    let (
        Some(reservations),
        Some(top_level_candidates),
        Some(targets),
        Some(annotations),
        Some(semantic_bindings),
        Some(expansion_bindings),
        Some(evidence),
        Some(ambiguity_adjudications),
        Some(source_symbol_dispositions),
    ) = (
        reservations,
        top_level_candidates,
        targets,
        annotations,
        semantic_bindings,
        expansion_bindings,
        evidence,
        ambiguity_adjudications,
        source_symbol_dispositions,
    )
    else {
        return Err(vec![Violation::new(
            "catalog_schema",
            "catalog_rows",
            "catalog metadata rows were not constructed",
        )]);
    };

    let catalog = Catalog {
        schema_version,
        name,
        catalog_epoch,
        row_id_grammar_version,
        canonical_order,
        diagnostic_version,
        hash_algorithm,
        source_encoding,
        source_manifest,
        reference_manifest,
        target_manifest,
        maintenance_proof,
        completion_layers,
        slices,
        projection_epochs,
        identity,
        projection_rows,
        reservations,
        top_level_candidates,
        targets,
        annotations,
        semantic_bindings,
        expansion_bindings,
        evidence,
        ambiguity_adjudications,
        source_symbol_dispositions,
    };
    Ok(catalog)
}

fn enforce_catalog_semantics(catalog: Catalog) -> Result<Catalog, Vec<Violation>> {
    let mut semantic = validate_catalog(&catalog);
    if semantic.is_empty() {
        Ok(catalog)
    } else {
        sort_violations(&mut semantic);
        Err(semantic)
    }
}

/// Load and parse a catalog file.  The file itself must also be UTF-8 LF
/// without a BOM so the canonical source machinery never has two text modes.
pub fn load_catalog_file(path: &Path) -> Result<Catalog, Vec<Violation>> {
    let catalog = load_catalog_file_structural(path)?;
    enforce_catalog_semantics(catalog)
}

fn load_catalog_file_structural(path: &Path) -> Result<Catalog, Vec<Violation>> {
    let bytes = fs::read(path).map_err(|error| {
        vec![Violation::new(
            "catalog_read",
            "catalog",
            format!("cannot read {}: {error}", path.display()),
        )]
    })?;
    validate_utf8_lf(&bytes, "catalog", "catalog_encoding")?;
    let text = std::str::from_utf8(&bytes).map_err(|error| {
        vec![Violation::new(
            "catalog_encoding",
            "catalog",
            format!("catalog is not UTF-8: {error}"),
        )]
    })?;
    parse_catalog_structural(text)
}

/// Load the canonical repository catalog and verify its pinned plan source.
pub fn load_and_verify(repo_root: &Path) -> Result<Catalog, Vec<Violation>> {
    let catalog = load_catalog_file_structural(&repo_root.join(CATALOG_PATH))?;
    let mut violations = validate_catalog(&catalog);
    if violations.is_empty() {
        let source_path = repo_root.join(&catalog.source_manifest.plan_path);
        match fs::read(&source_path) {
            Ok(source) => violations.extend(appendix_a_catalog_source(&catalog, &source)),
            Err(error) => violations.push(Violation::new(
                "source_read",
                "source_manifest",
                format!("cannot read {}: {error}", source_path.display()),
            )),
        }
    }
    violations.extend(verify_repository_bindings(repo_root, &catalog));
    sort_violations(&mut violations);
    if violations.is_empty() {
        Ok(catalog)
    } else {
        Err(violations)
    }
}

/// Resolve implementation ownership and evidence identifiers against the
/// repository's authoritative Beads, crate, and checker registries.
pub fn verify_repository_bindings(repo_root: &Path, catalog: &Catalog) -> Vec<Violation> {
    let architecture = match architecture::load_from_repo(repo_root) {
        Ok(registry) => registry,
        Err(_) => {
            return vec![Violation::new(
                "catalog_repository_registry_unavailable",
                "repository_bindings",
                "cannot load the architecture registry needed to resolve Appendix metadata",
            )];
        }
    };
    // MEMBERSHIP, not totality. This check needs one thing from the Beads
    // index: whether the specific `owner_bead_id` values Appendix A names
    // resolve. Consuming the TOTAL index coupled that question to every
    // unrelated record in the project — one orphaned bead anywhere returned
    // `catalog_repository_beads_unavailable`, which blocked
    // `appendix-regenerate` for EVERY slice and stalled the catalog. Totality
    // is the architecture registry's own claim and it still reports
    // `bead_provenance_orphan` / `bead_provenance_not_total` for it.
    // An unresolvable record is absent from this set, so a row naming one still
    // fails below, attributed to that row rather than to the whole file.
    let bead_entries = match architecture::bead_provenance_membership(&architecture, repo_root) {
        Ok(entries) => entries,
        Err(_) => {
            return vec![Violation::new(
                "catalog_repository_beads_unavailable",
                "repository_bindings",
                "cannot read the authoritative Beads index needed by Appendix metadata",
            )];
        }
    };
    let bead_ids: BTreeSet<&str> = bead_entries
        .iter()
        .map(|entry| entry.bead_id.as_str())
        .collect();
    let planned_crates: BTreeSet<&str> = architecture
        .registry
        .planned_crates
        .iter()
        .map(String::as_str)
        .collect();
    let workspace_crates = workspace_package_names(repo_root).ok();

    let mut out = Vec::new();
    if workspace_crates.is_none() {
        out.push(Violation::new(
            "catalog_repository_workspace_unavailable",
            "repository_bindings",
            "cannot resolve actual Cargo workspace packages needed by Appendix implementation ownership",
        ));
    }
    if !bead_ids.contains(catalog.maintenance_proof.owner_bead_id.as_str()) {
        out.push(Violation::new(
            "catalog_maintenance_owner_bead_unresolved",
            "maintenance_proof",
            "maintenance owner_bead_id must resolve in the authoritative Beads index",
        ));
    }
    if workspace_crates
        .as_ref()
        .is_some_and(|crates| !crates.contains(catalog.maintenance_proof.owner_crate.as_str()))
    {
        out.push(Violation::new(
            "catalog_maintenance_owner_crate_unresolved",
            "maintenance_proof",
            "maintenance owner_crate must resolve to an actual Cargo workspace package",
        ));
    }
    for row in &catalog.semantic_bindings {
        if !bead_ids.contains(row.owner_bead_id.as_str()) {
            out.push(Violation::new(
                "catalog_semantic_owner_bead_unresolved",
                &row.row_id,
                "semantic owner_bead_id must resolve in the authoritative Beads index",
            ));
        }
        if !planned_crates.contains(row.owner_crate.as_str()) {
            out.push(Violation::new(
                "catalog_semantic_owner_crate_unresolved",
                &row.row_id,
                "semantic owner_crate must resolve in architecture.registry.planned_crates",
            ));
        }
        if row.owner_status == "live"
            && workspace_crates
                .as_ref()
                .is_some_and(|crates| !crates.contains(row.owner_crate.as_str()))
        {
            out.push(Violation::new(
                "catalog_semantic_live_owner_crate_unresolved",
                &row.row_id,
                "live semantic owner_crate must resolve to an actual Cargo workspace package",
            ));
        }
        if row
            .consumer_crates
            .iter()
            .any(|consumer| !planned_crates.contains(consumer.as_str()))
        {
            out.push(Violation::new(
                "catalog_semantic_consumer_crate_unresolved",
                &row.row_id,
                "every semantic consumer_crate must resolve in the planned crate registry",
            ));
        }
    }

    let checkers = match load_appendix_checker_index(repo_root) {
        Some(checkers) => checkers,
        None => {
            out.push(Violation::new(
                "catalog_repository_checker_index_unavailable",
                "repository_bindings",
                "cannot load the checker index needed to resolve Appendix evidence",
            ));
            sort_violations(&mut out);
            return out;
        }
    };
    let checker_by_id: BTreeMap<&str, &model::Checker> = checkers
        .iter()
        .map(|checker| (checker.symbol.as_str(), checker))
        .collect();
    if checker_by_id.len() != checkers.len() {
        out.push(Violation::new(
            "catalog_repository_checker_index_ambiguous",
            "repository_bindings",
            "checker_index.toml contains duplicate symbols",
        ));
    }
    validate_maintenance_checker_registry(&checker_by_id, &mut out);
    // ONE prover for the whole sweep. Liveness is no longer an `is_file()` call
    // (`fgdb-checker-index-live-is-only-file-existence-tl0o`) — it reads the
    // module tree of every crate a `binary` row names — and the catalog asks
    // about a checker once per evidence row.
    let prover = crate::liveness::Prover::new(repo_root);
    validate_scenario_registry(&prover, &checker_by_id, catalog, &mut out);
    validate_checker_bindings(
        &prover,
        "maintenance_proof",
        &catalog.maintenance_proof.evidence_status,
        &catalog.maintenance_proof.checker_ids,
        CheckerBindingCodes {
            unresolved: "catalog_maintenance_checker_unresolved",
            not_live: "catalog_maintenance_checker_not_live",
            artifact_missing: "catalog_maintenance_checker_artifact_missing",
        },
        &checker_by_id,
        &mut out,
    );
    validate_scenario_bindings(
        "maintenance_proof",
        &catalog.maintenance_proof.evidence_status,
        ScenarioBindingRefs {
            scenario_ids: &catalog.maintenance_proof.scenario_ids,
            event_ids: &catalog.maintenance_proof.event_ids,
            gate_ids: &catalog.maintenance_proof.gate_ids,
            target_row_id: None,
        },
        catalog,
        &mut out,
    );
    for row in &catalog.evidence {
        if !bead_ids.contains(row.owner_bead_id.as_str()) {
            out.push(Violation::new(
                "catalog_evidence_owner_bead_unresolved",
                &row.row_id,
                "evidence owner_bead_id must resolve in the authoritative Beads index",
            ));
        }
        validate_checker_bindings(
            &prover,
            &row.row_id,
            &row.status,
            &row.checker_ids,
            CheckerBindingCodes {
                unresolved: "catalog_evidence_checker_unresolved",
                not_live: "catalog_live_evidence_checker_not_live",
                artifact_missing: "catalog_live_evidence_checker_artifact_missing",
            },
            &checker_by_id,
            &mut out,
        );
        validate_scenario_bindings(
            &row.row_id,
            &row.status,
            ScenarioBindingRefs {
                scenario_ids: &row.scenario_ids,
                event_ids: &row.event_ids,
                gate_ids: &row.gate_ids,
                target_row_id: Some(&row.target_row_id),
            },
            catalog,
            &mut out,
        );
    }
    sort_violations(&mut out);
    out
}

fn workspace_package_names(repo_root: &Path) -> Result<BTreeSet<String>, String> {
    let workspace_text = fs::read_to_string(repo_root.join("Cargo.toml"))
        .map_err(|error| format!("Cargo.toml: {error}"))?;
    let workspace_manifest =
        toml::parse(&workspace_text).map_err(|error| format!("Cargo.toml: {error}"))?;
    let workspace = toml::get_table(&workspace_manifest, "workspace", "Cargo.toml")
        .map_err(|error| error.to_string())?;
    let members = toml::get_str_array(workspace, "members", "Cargo.toml.workspace")
        .map_err(|error| error.to_string())?;
    let excluded_paths = workspace_exact_excludes(workspace)?;
    let member_paths = workspace_member_paths(repo_root, &members, &excluded_paths)?;

    let mut packages = BTreeSet::new();
    for member_path in member_paths {
        let manifest_path = repo_root.join(&member_path).join("Cargo.toml");
        let manifest_text = fs::read_to_string(&manifest_path)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        let package_name = cargo_package_name(&manifest_text, &manifest_path)?;
        if !packages.insert(package_name) {
            return Err("Cargo workspace contains duplicate package names".to_owned());
        }
    }
    Ok(packages)
}

pub(crate) fn workspace_exact_excludes(workspace: &Table) -> Result<BTreeSet<PathBuf>, String> {
    let excludes = toml::get_opt_str_array(workspace, "exclude", "Cargo.toml.workspace")
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let mut excluded_paths = BTreeSet::new();
    for exclude in excludes {
        if exclude
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']' | '{' | '}'))
        {
            return Err(format!(
                "unsupported non-exact Cargo workspace exclude {exclude:?}"
            ));
        }
        let Some(excluded_path) = normalized_repository_relative(&exclude) else {
            return Err(format!("unsafe Cargo workspace exclude path {exclude:?}"));
        };
        excluded_paths.insert(excluded_path);
    }
    Ok(excluded_paths)
}

pub(crate) fn workspace_member_paths(
    repo_root: &Path,
    members: &[String],
    excluded_paths: &BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    let mut member_paths = Vec::new();
    for member in members {
        if let Some(parent) = member.strip_suffix("/*") {
            let Some(parent_path) = normalized_repository_relative(parent) else {
                return Err(format!("unsafe Cargo workspace member glob {member:?}"));
            };
            let mut children = fs::read_dir(repo_root.join(&parent_path))
                .map_err(|error| format!("workspace member glob {member:?}: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("workspace member glob {member:?}: {error}"))?;
            children.sort_by_key(|entry| entry.file_name());
            member_paths.extend(
                children
                    .into_iter()
                    .filter(|child| child.path().join("Cargo.toml").is_file())
                    .map(|child| parent_path.join(child.file_name()))
                    .filter(|child_path| !excluded_paths.contains(child_path)),
            );
        } else {
            let Some(member_path) = normalized_repository_relative(member) else {
                return Err(format!("unsafe Cargo workspace member path {member:?}"));
            };
            if !excluded_paths.contains(&member_path) {
                member_paths.push(member_path);
            }
        }
    }
    member_paths.sort();
    member_paths.dedup();
    Ok(member_paths)
}

fn cargo_package_name(manifest_text: &str, manifest_path: &Path) -> Result<String, String> {
    let mut in_package = false;
    let mut package_name = None;

    for line in manifest_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed
                .split_once('#')
                .map_or(trimmed, |(before_comment, _)| before_comment)
                .trim();
            in_package = header == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((raw_key, _)) = trimmed.split_once('=') else {
            continue;
        };
        if raw_key.trim() != "name" {
            continue;
        }
        if package_name.is_some() {
            return Err(format!(
                "{}: duplicate package.name assignment",
                manifest_path.display()
            ));
        }

        // Cargo manifests use TOML's full surface, while registry-check's
        // in-house parser intentionally accepts only the registry subset.
        // Parse the one package identity assignment we own instead of making
        // unrelated dependency syntax part of the live-owner contract.
        let identity_document = format!("[package]\n{line}\n");
        let identity = toml::parse(&identity_document)
            .map_err(|error| format!("{}: package.name: {error}", manifest_path.display()))?;
        let package = toml::get_table(&identity, "package", "workspace member Cargo.toml")
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        package_name = Some(
            toml::get_str(package, "name", "workspace member Cargo.toml.package")
                .map_err(|error| format!("{}: {error}", manifest_path.display()))?,
        );
    }

    package_name.ok_or_else(|| format!("{}: missing package.name", manifest_path.display()))
}

pub(crate) fn safe_repository_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn normalized_repository_relative(path: &str) -> Option<PathBuf> {
    if !safe_repository_relative(path) {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        if let Component::Normal(component) = component {
            normalized.push(component);
        }
    }
    Some(normalized)
}

/// Is this `status = "live"` row the live checker it claims to be?
///
/// Delegates to [`crate::liveness`], the ONE reader for that question. This
/// predicate used to be `safe_repository_relative(...) && ...is_file()`, and
/// `validate::validate_checker_index` asked the same question with a second,
/// weaker copy that had no path guard at all. `is_file()` cannot distinguish a
/// registered gate nobody invokes from one that runs every commit, and every
/// G1–G4 exit gate rests on that distinction
/// (`fgdb-checker-index-live-is-only-file-existence-tl0o`).
fn live_checker_is_live(prover: &crate::liveness::Prover<'_>, checker: &model::Checker) -> bool {
    prover.assess(checker).is_empty()
}

fn load_appendix_checker_index(repo_root: &Path) -> Option<Vec<model::Checker>> {
    let bytes = fs::read(repo_root.join("registries/checker_index.toml")).ok()?;
    validate_utf8_lf(
        &bytes,
        "checker_index",
        "catalog_repository_checker_index_unavailable",
    )
    .ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let root = toml::parse(text).ok()?;
    model::checker_index_from(&root).ok()
}

fn validate_maintenance_checker_registry(
    checker_by_id: &BTreeMap<&str, &model::Checker>,
    out: &mut Vec<Violation>,
) {
    for contract in APPENDIX_MAINTENANCE_CHECKERS {
        match checker_by_id.get(contract.id).copied() {
            Some(checker)
                if checker.kind == contract.kind
                    && checker.artifact == contract.artifact
                    && checker.status == contract.status => {}
            _ => out.push(Violation::new(
                "catalog_maintenance_checker_registry_drift",
                "maintenance_proof",
                "Appendix maintenance checker ID, kind, artifact, and live status must byte-match the compiled contract",
            )),
        }
    }
}

fn validate_scenario_registry(
    prover: &crate::liveness::Prover<'_>,
    checker_by_id: &BTreeMap<&str, &model::Checker>,
    catalog: &Catalog,
    out: &mut Vec<Violation>,
) {
    for scenario in APPENDIX_EVIDENCE_SCENARIOS {
        match checker_by_id.get(scenario.checker_id).copied() {
            Some(checker)
                if checker.kind == scenario.checker_kind
                    && checker.artifact == scenario.checker_artifact
                    && checker.status == scenario.status => {}
            _ => out.push(Violation::new(
                "catalog_scenario_registry_drift",
                "repository_bindings",
                "compiled Appendix scenario does not resolve to its exact checker contract",
            )),
        }
        if checker_by_id
            .get(scenario.checker_id)
            .is_some_and(|checker| {
                checker.status == "live" && !live_checker_is_live(prover, checker)
            })
        {
            out.push(Violation::new(
                "catalog_scenario_checker_artifact_missing",
                "repository_bindings",
                "compiled live Appendix scenario checker must resolve to a safe existing repository artifact",
            ));
        }
        let target_scope_valid = match scenario.target_manifest_sha256 {
            Some(sha256) => {
                scenario.target_row_ids.is_empty()
                    && sha256 == catalog.target_manifest.target_source_assignment_sha256
                    && sha256 == EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256
            }
            None => {
                !scenario.target_row_ids.is_empty()
                    && scenario
                        .target_row_ids
                        .windows(2)
                        .all(|pair| pair[0] < pair[1])
                    && scenario.target_row_ids.iter().all(|target_row_id| {
                        catalog
                            .targets
                            .iter()
                            .any(|target| target.target_row_id == *target_row_id)
                    })
            }
        };
        if !target_scope_valid {
            out.push(Violation::new(
                "catalog_scenario_target_scope_drift",
                "repository_bindings",
                "compiled Appendix scenario must bind either the released target manifest or one exact sorted target set",
            ));
        }
    }
}

struct CheckerBindingCodes<'a> {
    unresolved: &'a str,
    not_live: &'a str,
    artifact_missing: &'a str,
}

fn validate_checker_bindings(
    prover: &crate::liveness::Prover<'_>,
    row_id: &str,
    evidence_status: &str,
    ids: &[String],
    codes: CheckerBindingCodes<'_>,
    checker_by_id: &BTreeMap<&str, &model::Checker>,
    out: &mut Vec<Violation>,
) {
    for id in ids {
        match checker_by_id.get(id.as_str()) {
            None => out.push(Violation::new(
                codes.unresolved,
                row_id,
                "every checker ID must resolve in checker_index.toml",
            )),
            Some(checker) if evidence_status == "live" && checker.status != "live" => {
                out.push(Violation::new(
                    codes.not_live,
                    row_id,
                    "live evidence requires every referenced checker to be live",
                ));
            }
            Some(checker)
                if evidence_status == "live" && !live_checker_is_live(prover, checker) =>
            {
                out.push(Violation::new(
                    codes.artifact_missing,
                    row_id,
                    "live evidence requires every referenced checker artifact to be a safe existing repository file",
                ));
            }
            Some(_) => {}
        }
    }
}

struct ScenarioBindingRefs<'a> {
    scenario_ids: &'a [String],
    event_ids: &'a [String],
    gate_ids: &'a [String],
    target_row_id: Option<&'a str>,
}

fn validate_scenario_bindings(
    row_id: &str,
    evidence_status: &str,
    bindings: ScenarioBindingRefs<'_>,
    catalog: &Catalog,
    out: &mut Vec<Violation>,
) {
    let ScenarioBindingRefs {
        scenario_ids,
        event_ids,
        gate_ids,
        target_row_id,
    } = bindings;
    let mut allowed_events = BTreeSet::new();
    let mut allowed_gates = BTreeSet::new();
    for scenario_id in scenario_ids {
        let Some(scenario) = APPENDIX_EVIDENCE_SCENARIOS
            .iter()
            .find(|scenario| scenario.id == scenario_id)
        else {
            out.push(Violation::new(
                "catalog_evidence_scenario_unresolved",
                row_id,
                "every evidence scenario ID must resolve in the compiled scenario registry",
            ));
            continue;
        };
        if evidence_status == "live" && scenario.status != "live" {
            out.push(Violation::new(
                "catalog_live_evidence_scenario_not_live",
                row_id,
                "live evidence requires every referenced scenario to be live",
            ));
        }
        if target_row_id
            .is_some_and(|target_row_id| !scenario_covers_target(scenario, target_row_id, catalog))
        {
            out.push(Violation::new(
                "catalog_evidence_scenario_target_uncovered",
                row_id,
                "referenced scenario does not cover this exact catalog target",
            ));
        }
        allowed_events.extend(scenario.event_ids.iter().copied());
        allowed_gates.extend(scenario.gate_ids.iter().copied());
        if !event_ids
            .iter()
            .any(|event| scenario.event_ids.contains(&event.as_str()))
        {
            out.push(Violation::new(
                "catalog_evidence_scenario_uncovered",
                row_id,
                "every referenced scenario must contribute at least one evidence event",
            ));
        }
    }
    if event_ids
        .iter()
        .any(|event| !allowed_events.contains(event.as_str()))
    {
        out.push(Violation::new(
            "catalog_evidence_event_unresolved",
            row_id,
            "every evidence event must be declared by a referenced scenario",
        ));
    }
    if gate_ids
        .iter()
        .any(|gate| !allowed_gates.contains(gate.as_str()))
    {
        out.push(Violation::new(
            "catalog_evidence_gate_unresolved",
            row_id,
            "every evidence gate must be declared by a referenced scenario",
        ));
    }
}

fn scenario_covers_target(
    scenario: &EvidenceScenarioSpec,
    target_row_id: &str,
    catalog: &Catalog,
) -> bool {
    match scenario.target_manifest_sha256 {
        Some(sha256) => {
            scenario.target_row_ids.is_empty()
                && sha256 == catalog.target_manifest.target_source_assignment_sha256
                && catalog
                    .targets
                    .iter()
                    .any(|target| target.target_row_id == target_row_id)
        }
        None => scenario.target_row_ids.contains(&target_row_id),
    }
}

/// Render all six consumer registries in their canonical order.
pub fn generated_projections(catalog: &Catalog) -> Vec<(String, String)> {
    PROJECTION_FILES
        .iter()
        .map(|(registry, file)| {
            (
                (*file).to_owned(),
                render_projection(registry, &catalog.identity),
            )
        })
        .collect()
}

/// Byte-compare generated projections with the checked-in consumer files.
pub fn verify_projections(repo_root: &Path, catalog: &Catalog) -> Vec<Violation> {
    let mut out = Vec::new();
    for (file, generated) in generated_projections(catalog) {
        let path = repo_root.join("registries").join(&file);
        let checked_in = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                out.push(Violation::new(
                    "projection_read",
                    &file,
                    format!("cannot read {}: {error}", path.display()),
                ));
                continue;
            }
        };
        if checked_in != generated.as_bytes() {
            let offset = checked_in
                .iter()
                .zip(generated.as_bytes())
                .position(|(actual, expected)| actual != expected)
                .unwrap_or_else(|| checked_in.len().min(generated.len()));
            let prefix = &generated.as_bytes()[..offset.min(generated.len())];
            let line = prefix.iter().filter(|byte| **byte == b'\n').count() + 1;
            let column = prefix
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(offset + 1, |newline| offset - newline);
            out.push(Violation::new(
                "projection_byte_diff",
                &file,
                format!(
                    "first divergence at byte {offset}, line {line}, column {column}; generated={} bytes checked_in={} bytes; generated_byte={} checked_in_byte={}",
                    generated.len(),
                    checked_in.len(),
                    display_byte(generated.as_bytes().get(offset).copied()),
                    display_byte(checked_in.get(offset).copied()),
                ),
            ));
        }
    }
    sort_violations(&mut out);
    out
}

/// Live checker-index entry point for the pinned Appendix source and census.
pub fn appendix_a_catalog_source(catalog: &Catalog, source: &[u8]) -> Vec<Violation> {
    verify_source(catalog, source)
}

/// Live checker-index entry point for deterministic consumer projection diffs.
pub fn appendix_a_catalog_projection_diff(repo_root: &Path, catalog: &Catalog) -> Vec<Violation> {
    verify_projections(repo_root, catalog)
}

/// Live checker-index entry point for exact type/owner/evidence closure.
pub fn appendix_a_catalog_closure(catalog: &Catalog) -> Vec<Violation> {
    let mut out = Vec::new();
    validate_catalog_metadata(catalog, &mut out);
    sort_violations(&mut out);
    out
}

pub fn reservation_assignment_sha256(rows: &[Reservation]) -> String {
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by(|left, right| {
        (&left.symbol, &left.code_reservation, &left.disposition).cmp(&(
            &right.symbol,
            &right.code_reservation,
            &right.disposition,
        ))
    });
    let mut transcript = String::new();
    for row in ordered {
        writeln!(
            &mut transcript,
            "{}|{}|{}",
            row.symbol, row.code_reservation, row.disposition
        )
        .expect("writing to String cannot fail");
    }
    sha256_hex(transcript.as_bytes())
}

pub fn target_source_assignment_sha256(rows: &[Target]) -> String {
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by(|left, right| left.target_row_id.cmp(&right.target_row_id));
    let mut transcript = String::new();
    for row in ordered {
        writeln!(&mut transcript, "{}|{}", row.target_row_id, row.source_key)
            .expect("writing to String cannot fail");
    }
    sha256_hex(transcript.as_bytes())
}

/// Hash the exact target-to-schema annotation contract. This pin is
/// independent of the catalog so prose-only role, retention, digest, or
/// compatibility assertions cannot silently authorize themselves.
pub fn annotation_contract_sha256(rows: &[Annotation]) -> String {
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by(|left, right| left.row_id.cmp(&right.row_id));
    let mut transcript = String::new();
    for row in ordered {
        append_contract_field(&mut transcript, &row.row_id);
        append_contract_field(&mut transcript, &row.target_row_id);
        append_contract_field(&mut transcript, &row.exact_type);
        append_contract_field(&mut transcript, &row.cardinality);
        append_contract_field(&mut transcript, &row.layout);
        append_contract_field(&mut transcript, &row.role);
        append_contract_field(&mut transcript, &row.posture);
        append_contract_field(&mut transcript, &row.authority);
        append_contract_field(&mut transcript, &row.locality);
        append_contract_array(&mut transcript, &row.generic_expansions);
        append_contract_array(&mut transcript, &row.role_expansions);
        append_contract_field(&mut transcript, &row.reference_semantics);
        append_contract_array(&mut transcript, &row.target_schema_ids);
        append_contract_field(&mut transcript, &row.construction_order);
        append_contract_field(&mut transcript, &row.retention_and_cut_rule);
        append_contract_field(&mut transcript, &row.digest_recipe);
        append_contract_field(&mut transcript, &row.redaction_class);
        append_contract_field(&mut transcript, &row.resource_bounds);
        append_contract_field(&mut transcript, &row.compatibility);
        transcript.push('\n');
    }
    sha256_hex(transcript.as_bytes())
}

/// Hash the exact target-to-implementation ownership contract. The transcript
/// is sorted by row ID and length-prefixes every scalar and array item, so a
/// syntactically valid but unrelated Bead or crate cannot become authoritative
/// merely by appearing in the catalog.
pub fn semantic_binding_contract_sha256(rows: &[SemanticBinding]) -> String {
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by(|left, right| left.row_id.cmp(&right.row_id));
    let mut transcript = String::new();
    for row in ordered {
        append_contract_field(&mut transcript, &row.row_id);
        append_contract_field(&mut transcript, &row.target_row_id);
        append_contract_field(&mut transcript, &row.owner_bead_id);
        append_contract_field(&mut transcript, &row.owner_crate);
        append_contract_field(&mut transcript, &row.owner_status);
        append_contract_array(&mut transcript, &row.consumer_crates);
        transcript.push('\n');
    }
    sha256_hex(transcript.as_bytes())
}

pub fn expansion_binding_contract_sha256(rows: &[ExpansionBinding]) -> String {
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by(|left, right| left.row_id.cmp(&right.row_id));
    let mut transcript = String::new();
    for row in ordered {
        append_contract_field(&mut transcript, &row.row_id);
        append_contract_field(&mut transcript, &row.target_row_id);
        append_contract_field(&mut transcript, &row.parameter_ordinal.to_string());
        append_contract_field(&mut transcript, &row.formal);
        append_contract_field(&mut transcript, &row.formal_class);
        append_contract_array(&mut transcript, &row.values);
        append_contract_field(&mut transcript, &row.rationale);
        transcript.push('\n');
    }
    sha256_hex(transcript.as_bytes())
}

/// Hash the exact target-to-evidence contract independently of repository
/// existence checks. Future slice work must deliberately update this compiled
/// pin when it introduces an approved checker/scenario/event/gate binding.
pub fn evidence_binding_contract_sha256(rows: &[EvidenceBinding]) -> String {
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by(|left, right| left.row_id.cmp(&right.row_id));
    let mut transcript = String::new();
    for row in ordered {
        append_contract_field(&mut transcript, &row.row_id);
        append_contract_field(&mut transcript, &row.target_row_id);
        append_contract_field(&mut transcript, &row.evidence_id);
        append_contract_field(&mut transcript, &row.phase);
        append_contract_field(&mut transcript, &row.status);
        append_contract_field(&mut transcript, &row.owner_bead_id);
        append_contract_array(&mut transcript, &row.checker_ids);
        append_contract_array(&mut transcript, &row.scenario_ids);
        append_contract_array(&mut transcript, &row.event_ids);
        append_contract_array(&mut transcript, &row.gate_ids);
        transcript.push('\n');
    }
    sha256_hex(transcript.as_bytes())
}

pub fn completion_layer_schema_sha256(rows: &[CompletionLayerSchema]) -> String {
    let mut transcript = String::new();
    for row in rows {
        append_contract_field(&mut transcript, &row.layer);
        append_contract_field(&mut transcript, &row.schema_version.to_string());
        append_contract_array(&mut transcript, &row.field_contracts);
        append_contract_field(&mut transcript, &row.target_binding);
        append_contract_field(&mut transcript, &row.target_cardinality);
        append_contract_field(&mut transcript, &row.epoch_domain);
        append_contract_field(&mut transcript, &row.projection_policy);
        append_contract_field(&mut transcript, &row.authoring_policy);
        append_contract_field(&mut transcript, &row.pin_policy);
        transcript.push('\n');
    }
    sha256_hex(transcript.as_bytes())
}

pub fn ambiguity_adjudication_contract_sha256(rows: &[AmbiguityAdjudication]) -> String {
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by(|left, right| left.row_id.cmp(&right.row_id));
    let mut transcript = String::new();
    for row in ordered {
        append_contract_field(&mut transcript, &row.row_id);
        append_contract_field(&mut transcript, &row.slice_id);
        append_contract_field(&mut transcript, &row.ambiguity_source_key);
        append_contract_array(&mut transcript, &row.source_locations);
        append_contract_field(&mut transcript, &row.resolution);
        append_contract_array(&mut transcript, &row.resolved_source_keys);
        append_contract_field(&mut transcript, &row.rationale);
        transcript.push('\n');
    }
    sha256_hex(transcript.as_bytes())
}

fn append_contract_field(transcript: &mut String, value: &str) {
    write!(transcript, "{}:", value.len()).expect("writing to String cannot fail");
    transcript.push_str(value);
    transcript.push('|');
}

fn append_contract_array(transcript: &mut String, values: &[String]) {
    write!(transcript, "{}[", values.len()).expect("writing to String cannot fail");
    for value in values {
        append_contract_field(transcript, value);
    }
    transcript.push_str("]|");
}

/// Validate catalog metadata, canonical pins, ordering, adjacency, and enums.
pub fn validate_catalog(catalog: &Catalog) -> Vec<Violation> {
    let mut out = Vec::new();
    pin_i64(
        &mut out,
        "catalog",
        "schema_version",
        CATALOG_SCHEMA_VERSION,
        catalog.schema_version,
    );
    pin_str(&mut out, "catalog", "name", CATALOG_NAME, &catalog.name);
    pin_i64(
        &mut out,
        "catalog",
        "catalog_epoch",
        CATALOG_EPOCH,
        catalog.catalog_epoch,
    );
    pin_i64(
        &mut out,
        "catalog",
        "row_id_grammar_version",
        ROW_ID_GRAMMAR_VERSION,
        catalog.row_id_grammar_version,
    );
    pin_str(
        &mut out,
        "catalog",
        "canonical_order",
        CANONICAL_ORDER,
        &catalog.canonical_order,
    );
    pin_i64(
        &mut out,
        "catalog",
        "diagnostic_version",
        DIAGNOSTIC_VERSION,
        catalog.diagnostic_version,
    );
    pin_str(
        &mut out,
        "catalog",
        "hash_algorithm",
        HASH_ALGORITHM,
        &catalog.hash_algorithm,
    );
    pin_str(
        &mut out,
        "catalog",
        "source_encoding",
        SOURCE_ENCODING,
        &catalog.source_encoding,
    );

    validate_source_manifest_pin(&catalog.source_manifest, &mut out);
    validate_reference_manifest(catalog, &mut out);
    validate_target_manifest(catalog, &mut out);
    validate_completion_layer_schema_contract(catalog, &mut out);

    if catalog.slices.len() != SLICE_PINS.len() {
        out.push(Violation::new(
            "slice_count_mismatch",
            "slice_manifest",
            format!(
                "expected exactly {} slices, found {}",
                SLICE_PINS.len(),
                catalog.slices.len()
            ),
        ));
    }

    let mut ids = BTreeSet::new();
    let mut bead_ids = BTreeSet::new();
    let mut ordinals = BTreeSet::new();
    for (index, slice) in catalog.slices.iter().enumerate() {
        let generated_row_id;
        let row_id = if slice.id.is_empty() {
            generated_row_id = format!("slice[{index}]");
            generated_row_id.as_str()
        } else {
            slice.id.as_str()
        };
        if !ids.insert(slice.id.as_str()) {
            out.push(Violation::new(
                "slice_duplicate",
                row_id,
                format!("duplicate slice id {:?}", slice.id),
            ));
        }
        if !bead_ids.insert(slice.bead_id.as_str()) {
            out.push(Violation::new(
                "slice_duplicate",
                row_id,
                format!("duplicate Bead id {:?}", slice.bead_id),
            ));
        }
        if !ordinals.insert(slice.ordinal) {
            out.push(Violation::new(
                "slice_duplicate",
                row_id,
                format!("duplicate ordinal {}", slice.ordinal),
            ));
        }
        if let Some(pin) = SLICE_PINS.get(index) {
            validate_slice_pin(slice, pin, row_id, &mut out);
        }
        validate_projection_classes(slice, row_id, &mut out);
        if !matches!(slice.definition_status.as_str(), "declared" | "complete") {
            out.push(Violation::new(
                "slice_enum_invalid",
                row_id,
                format!(
                    "definition_status {:?} is not declared|complete",
                    slice.definition_status
                ),
            ));
        }
        let computed_lines = slice
            .end_line
            .checked_sub(slice.start_line)
            .and_then(|delta| delta.checked_add(1));
        if computed_lines != Some(slice.line_count) {
            out.push(Violation::new(
                "slice_range_mismatch",
                row_id,
                format!(
                    "line_count {} does not equal inclusive range {}-{}",
                    slice.line_count, slice.start_line, slice.end_line
                ),
            ));
        }
        if slice.byte_count <= 0 || !valid_sha256_hex(&slice.sha256) {
            out.push(Violation::new(
                "slice_pin_invalid",
                row_id,
                "byte_count must be positive and sha256 must be 64 lowercase hex digits",
            ));
        }
    }

    let mut projection_class_transcript = String::new();
    for slice in &catalog.slices {
        writeln!(
            &mut projection_class_transcript,
            "{}|{}",
            slice.id,
            slice.expected_projection_classes.join(",")
        )
        .expect("writing to String cannot fail");
    }
    let projection_class_sha256 = sha256_hex(projection_class_transcript.as_bytes());
    if projection_class_sha256 != EXPECTED_SLICE_PROJECTION_CLASSES_SHA256 {
        out.push(Violation::new(
            "slice_projection_class_assignment_drift",
            "slice_manifest",
            format!(
                "slice projection-class transcript must have sha256 {EXPECTED_SLICE_PROJECTION_CLASSES_SHA256}, found {projection_class_sha256}"
            ),
        ));
    }

    for (index, slice) in catalog.slices.iter().enumerate() {
        let expected_predecessor = index
            .checked_sub(1)
            .and_then(|previous| catalog.slices.get(previous))
            .map_or("", |previous| previous.id.as_str());
        let expected_successor = catalog
            .slices
            .get(index + 1)
            .map_or("", |next| next.id.as_str());
        if slice.predecessor != expected_predecessor {
            out.push(Violation::new(
                "slice_adjacency_mismatch",
                &slice.id,
                format!(
                    "predecessor {:?} != {:?}",
                    slice.predecessor, expected_predecessor
                ),
            ));
        }
        if slice.successor != expected_successor {
            out.push(Violation::new(
                "slice_adjacency_mismatch",
                &slice.id,
                format!(
                    "successor {:?} != {:?}",
                    slice.successor, expected_successor
                ),
            ));
        }
        if let Some(next) = catalog.slices.get(index + 1) {
            let expected_start = slice.end_line.checked_add(1);
            if expected_start != Some(next.start_line) {
                out.push(Violation::new(
                    "slice_range_mismatch",
                    &slice.id,
                    format!(
                        "range ends at {}, but successor {} starts at {}",
                        slice.end_line, next.id, next.start_line
                    ),
                ));
            }
        }
    }

    if let Some(first) = catalog.slices.first()
        && first.start_line != catalog.source_manifest.start_line
    {
        out.push(Violation::new(
            "slice_endpoint_mismatch",
            &first.id,
            "first slice does not start at the Appendix start",
        ));
    }
    if let Some(last) = catalog.slices.last()
        && last.end_line != catalog.source_manifest.end_line
    {
        out.push(Violation::new(
            "slice_endpoint_mismatch",
            &last.id,
            "last slice does not end at the Appendix end",
        ));
    }

    validate_projection_catalog(catalog, &mut out);
    out.extend(appendix_a_catalog_closure(catalog));

    sort_violations(&mut out);
    out
}

/// Verify the raw plan bytes against the full and per-slice source manifest.
pub fn verify_source(catalog: &Catalog, source: &[u8]) -> Vec<Violation> {
    let mut out = match validate_utf8_lf(source, "source_manifest", "source_encoding") {
        Ok(()) => Vec::new(),
        Err(violations) => return violations,
    };

    let line_spans = source_line_spans(source);
    let manifest = &catalog.source_manifest;
    let Some(appendix) = extract_lines(source, &line_spans, manifest.start_line, manifest.end_line)
    else {
        return vec![Violation::new(
            "source_range_missing",
            "source_manifest",
            format!(
                "source does not contain complete range {}-{}",
                manifest.start_line, manifest.end_line
            ),
        )];
    };

    verify_source_bytes(
        appendix,
        manifest.byte_count,
        &manifest.sha256,
        "source_manifest",
        &mut out,
    );
    verify_heading(
        source,
        &line_spans,
        manifest.start_line,
        &manifest.heading,
        "heading",
        &mut out,
    );
    if let Some(next_line) = manifest.end_line.checked_add(1) {
        verify_heading(
            source,
            &line_spans,
            next_line,
            &manifest.next_heading,
            "next_heading",
            &mut out,
        );
    } else {
        out.push(Violation::new(
            "source_range_invalid",
            "source_manifest",
            "end_line overflow while locating next heading",
        ));
    }

    let mut concatenated = Vec::with_capacity(appendix.len());
    for slice in &catalog.slices {
        let Some(bytes) = extract_lines(source, &line_spans, slice.start_line, slice.end_line)
        else {
            out.push(Violation::new(
                "source_range_missing",
                &slice.id,
                format!(
                    "source does not contain complete range {}-{}",
                    slice.start_line, slice.end_line
                ),
            ));
            continue;
        };
        verify_source_bytes(bytes, slice.byte_count, &slice.sha256, &slice.id, &mut out);
        concatenated.extend_from_slice(bytes);
    }
    if concatenated.as_slice() != appendix {
        out.push(Violation::new(
            "source_concatenation_mismatch",
            "source_manifest",
            "ordered slice bytes do not reconstruct the complete Appendix bytes",
        ));
    }
    if let Some(structural_census) = verify_structural_source_census(catalog, appendix, &mut out) {
        verify_reference_source_census(catalog, source, &structural_census, &mut out);
    }

    sort_violations(&mut out);
    out
}

fn verify_structural_source_census(
    catalog: &Catalog,
    appendix: &[u8],
    out: &mut Vec<Violation>,
) -> Option<AppendixSourceCensus> {
    let source_start_line = match usize::try_from(catalog.source_manifest.start_line) {
        Ok(line) if line > 0 => line,
        _ => {
            out.push(Violation::new(
                "source_census_range_invalid",
                "source_manifest",
                "source census requires a positive Appendix start line",
            ));
            return None;
        }
    };
    let mut specs = Vec::with_capacity(catalog.slices.len());
    for slice in &catalog.slices {
        let (Ok(start_line), Ok(end_line)) = (
            usize::try_from(slice.start_line),
            usize::try_from(slice.end_line),
        ) else {
            out.push(Violation::new(
                "source_census_range_invalid",
                &slice.id,
                "slice source coordinates must fit positive machine-sized integers",
            ));
            return None;
        };
        specs.push(SourceSliceSpec {
            id: &slice.id,
            start_line,
            end_line,
        });
    }

    let census = match census_appendix_source(appendix, source_start_line, &specs) {
        Ok(census) => census,
        Err(error) => {
            out.push(Violation::new(
                "source_structural_census_error",
                error.slice_id.as_deref().unwrap_or("source_manifest"),
                error.to_string(),
            ));
            return None;
        }
    };

    let census_by_slice: BTreeMap<&str, _> = census
        .slices
        .iter()
        .map(|slice| (slice.slice_id.as_str(), slice))
        .collect();
    for slice in &catalog.slices {
        let Some(actual) = census_by_slice.get(slice.id.as_str()).copied() else {
            out.push(Violation::new(
                "source_structural_slice_missing",
                &slice.id,
                "structural census did not return this declared slice",
            ));
            continue;
        };
        for (kind, expected_count, expected_sha256, actual_digest) in [
            (
                "top_level_candidate",
                slice.top_level_candidate_count,
                slice.top_level_candidate_ids_sha256.as_str(),
                &actual.transcripts.schemas,
            ),
            (
                "field_candidate",
                slice.field_candidate_count,
                slice.field_candidate_ids_sha256.as_str(),
                &actual.transcripts.fields,
            ),
            (
                "union_candidate",
                slice.union_candidate_count,
                slice.union_candidate_ids_sha256.as_str(),
                &actual.transcripts.unions,
            ),
            (
                "arm_candidate",
                slice.arm_candidate_count,
                slice.arm_candidate_ids_sha256.as_str(),
                &actual.transcripts.arms,
            ),
            (
                "ambiguity",
                slice.ambiguity_count,
                slice.ambiguity_ids_sha256.as_str(),
                &actual.transcripts.ambiguities,
            ),
        ] {
            let actual_count = i64::try_from(actual_digest.rows).unwrap_or(i64::MAX);
            if expected_count != actual_count || expected_sha256 != actual_digest.sha256 {
                out.push(Violation::new(
                    "source_structural_census_mismatch",
                    &slice.id,
                    format!(
                        "{kind} pin expected {expected_count}/{expected_sha256}, found {actual_count}/{}",
                        actual_digest.sha256
                    ),
                ));
            }
        }
    }

    verify_top_level_source_candidates(catalog, &census, out);
    verify_structural_target_source_keys(catalog, &census, out);
    verify_ordinary_union_source_contracts(catalog, &census, out);
    verify_annotation_source_contracts(catalog, &census, out);
    verify_ambiguity_adjudications(catalog, &census, out);
    verify_complete_field_census_coverage(catalog, &census, out);
    Some(census)
}

fn verify_ordinary_union_source_contracts(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let union_by_key: BTreeMap<String, &UnionCandidate> = census
        .unions
        .iter()
        .map(|row| (row.key.source_key(), row))
        .collect();
    let arm_by_key: BTreeMap<String, &ArmCandidate> = census
        .arms
        .iter()
        .map(|row| (row.key.source_key(), row))
        .collect();
    let target_by_projection: BTreeMap<&str, &Target> = catalog
        .targets
        .iter()
        .map(|row| (row.target_row_id.as_str(), row))
        .collect();
    let annotation_by_target: BTreeMap<&str, &Annotation> = catalog
        .annotations
        .iter()
        .map(|row| (row.target_row_id.as_str(), row))
        .collect();
    let top_level_by_key: BTreeMap<&str, &TopLevelCandidate> = catalog
        .top_level_candidates
        .iter()
        .map(|row| (row.source_key.as_str(), row))
        .collect();
    let projection_by_symbol: BTreeMap<(&str, &str), &ProjectionRowMeta> = catalog
        .projection_rows
        .iter()
        .map(|row| ((row.row_kind.as_str(), row.canonical_symbol.as_str()), row))
        .collect();

    for union in &catalog.identity.ordinary_unions {
        let symbol = format!("{}.{}", union.containing_schema, union.union_path);
        let Some(projection) = projection_by_symbol
            .get(&("union", symbol.as_str()))
            .copied()
        else {
            continue;
        };
        let Some(target) = target_by_projection
            .get(projection.row_id.as_str())
            .copied()
        else {
            continue;
        };
        let Some(source) = union_by_key.get(&target.source_key).copied() else {
            continue;
        };
        let top_level_shape = identity::ordinary_union_has_top_level_shape(union);
        if top_level_shape {
            // A generic-signed whole-schema union owns the candidate whose
            // symbol + generic signature reproduce the signed union name;
            // bare unions keep the exact-symbol contract unchanged.
            let top_level_source_key = format!("top|{}", union.union_name);
            match top_level_by_key.get(top_level_source_key.as_str()).copied() {
                Some(candidate)
                    if candidate.slice_id == target.slice_id
                        && format!("{}{}", candidate.symbol, candidate.generic_signature)
                            == union.union_name
                        && candidate.source_kind == "confirmed" => {}
                _ => out.push(Violation::new(
                    "source_union_top_level_owner_mismatch",
                    &target.row_id,
                    "a top-level ordinary union requires one same-slice confirmed top-level source candidate naming the exact signed union family",
                )),
            }
        }
        if source.key.schema_owner != union.containing_schema
            || source.key.union_path != union.union_path
            || source.arm_set_conflict
            || source.unparsed_arm_count != 0
            || source.parsed_arm_count != source.arm_names.len()
        {
            out.push(Violation::new(
                "source_union_contract_mismatch",
                &target.row_id,
                "ordinary union must exactly match one conflict-free, fully parsed source union owner/path/arm set",
            ));
        }

        let mut projected_arm_names = BTreeSet::new();
        for arm in &union.arms {
            let arm_symbol = format!(
                "{}.{}.{}",
                arm.containing_schema, arm.union_path, arm.source_arm_name
            );
            let Some(arm_projection) = projection_by_symbol
                .get(&("union-arm", arm_symbol.as_str()))
                .copied()
            else {
                continue;
            };
            let Some(arm_target) = target_by_projection
                .get(arm_projection.row_id.as_str())
                .copied()
            else {
                continue;
            };
            let Some(source_arm) = arm_by_key.get(&arm_target.source_key).copied() else {
                continue;
            };
            projected_arm_names.insert(arm.source_arm_name.as_str());
            let payload_matches = match source_arm.payload_sha256s.as_slice() {
                [] => arm.payload_kind == "unit" && arm.payload_sha256.is_none(),
                [sha256] => {
                    arm.payload_kind == "inline-record"
                        && arm.payload_sha256.as_deref() == Some(sha256.as_str())
                }
                _ => false,
            };
            if source_arm.key.schema_owner != union.containing_schema
                || source_arm.key.union_path != union.union_path
                || source_arm.key.arm_name != arm.source_arm_name
                || source_arm.payload_conflict
                || !payload_matches
            {
                out.push(Violation::new(
                    "source_union_arm_contract_mismatch",
                    &arm_target.row_id,
                    "ordinary union arm must exactly match its source parent, token, and normalized payload hash",
                ));
            }
            if arm_target.definition_status == "complete" {
                match annotation_by_target.get(arm_projection.row_id.as_str()).copied() {
                    Some(annotation)
                        if annotation.exact_type == arm.source_arm_name
                            && annotation.cardinality == "one"
                            && annotation.layout == arm.payload_kind
                            && annotation.reference_semantics == "none"
                            && annotation.target_schema_ids.is_empty() => {}
                    _ => out.push(Violation::new(
                        "source_union_arm_annotation_mismatch",
                        &arm_target.row_id,
                        "complete ordinary arm annotation must exactly describe its source token and non-reference payload layout",
                    )),
                }
            }
        }
        let source_arm_names: BTreeSet<&str> =
            source.arm_names.iter().map(String::as_str).collect();
        if projected_arm_names != source_arm_names {
            out.push(Violation::new(
                "source_union_arm_set_mismatch",
                &target.row_id,
                "ordinary union projection arms must be an exact bijection with the source arm set",
            ));
        }
        if target.definition_status == "complete" {
            match annotation_by_target.get(projection.row_id.as_str()).copied() {
                Some(annotation)
                    if annotation.exact_type == union.union_name
                        && annotation.cardinality == "one"
                        && annotation.layout == union.encoding_context
                        && annotation.reference_semantics == "none"
                        && annotation.target_schema_ids.is_empty() => {}
                _ => out.push(Violation::new(
                    "source_union_annotation_mismatch",
                    &target.row_id,
                    "complete ordinary union annotation must exactly describe its tagged non-reference encoding",
                )),
            }
        }
    }
}

/// Census keys covered by a stronger, already-source-verified structural
/// contract instead of a per-key projection row (fgdb-z35a, generalized to
/// unions and arms for the fgdb-a01 role/wire union families).
///
/// Two closed classes, applied uniformly to field, union, and arm keys:
/// - arm-payload interior: the key's container path traverses a union arm
///   that has a catalog union-arm target; the arm row's `payload_sha256`
///   commits the payload shape byte-exactly, so interior fields, nested
///   unions, and nested arms cannot drift without the arm contract failing
///   first.
/// - wire-type interior: the key's schema family is a targeted wire-type
///   projection row; the wire row's exact envelope contract commits the
///   interior (including embedded closed unions such as result-role tags),
///   and the identity constitution deliberately resolves no durable-field
///   host — and permits no anchored embedded union — in the wire class.
///
/// Wire coverage matches the generic-free schema family: one registered wire
/// row commits the envelope for every expansion of its family, so
/// `StrongCiphertextRef<T>` interiors are committed by the
/// `StrongCiphertextRef` row.  Non-generic owners have family == owner, and
/// non-wire generic families stay uncovered.  Lookup is catalog-global by
/// symbol: identity-class disjointness makes schema owners unique, while
/// census occurrences remain slice-scoped.
struct CoveredInteriorKeys {
    fields: BTreeSet<String>,
    unions: BTreeSet<String>,
    arms: BTreeSet<String>,
}

fn covered_interior_keys(catalog: &Catalog, census: &AppendixSourceCensus) -> CoveredInteriorKeys {
    let mut arm_prefixes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for target in &catalog.targets {
        if target.target_kind != "union-arm" {
            continue;
        }
        let mut parts = target.source_key.split('|');
        if parts.next() != Some("arm") {
            continue;
        }
        let (Some(owner), Some(union_path), Some(arm_name), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        arm_prefixes
            .entry(owner)
            .or_default()
            .push(format!("{union_path}.{arm_name}."));
    }
    let targeted_row_ids: BTreeSet<&str> = catalog
        .targets
        .iter()
        .map(|row| row.target_row_id.as_str())
        .collect();
    let wire_symbols: BTreeSet<&str> = catalog
        .projection_rows
        .iter()
        .filter(|row| row.row_kind == "wire-type" && targeted_row_ids.contains(row.row_id.as_str()))
        .map(|row| row.canonical_symbol.as_str())
        .collect();
    let arm_prefix_covers = |owner: &str, container_path: &str| {
        arm_prefixes.get(owner).is_some_and(|prefixes| {
            prefixes
                .iter()
                .any(|prefix| container_path.starts_with(prefix.as_str()))
        })
    };
    let mut covered = CoveredInteriorKeys {
        fields: BTreeSet::new(),
        unions: BTreeSet::new(),
        arms: BTreeSet::new(),
    };
    for field in &census.fields {
        if arm_prefix_covers(field.key.schema_owner.as_str(), &field.key.path)
            || wire_symbols.contains(field.key.schema_family.as_str())
        {
            covered.fields.insert(field.key.source_key());
        }
    }
    for union in &census.unions {
        if arm_prefix_covers(union.key.schema_owner.as_str(), &union.key.union_path)
            || wire_symbols.contains(union.key.schema_family.as_str())
        {
            covered.unions.insert(union.key.source_key());
        }
    }
    for arm in &census.arms {
        if arm_prefix_covers(arm.key.schema_owner.as_str(), &arm.key.union_path)
            || wire_symbols.contains(arm.key.schema_family.as_str())
        {
            covered.arms.insert(arm.key.source_key());
        }
    }
    covered
}

/// The complete-slice field census law (fgdb-z35a): every census field key of
/// a complete slice must be covered by exactly one verified contract — a
/// field target, an approved not-a-durable-schema adjudication, or a covering
/// arm/wire interior contract.  The covered classes are census-derived, so
/// this equality lives in the source pass; a catalog-only sha-equality pin
/// cannot express them.  Extra targeted keys are rejected independently by
/// `verify_structural_target_source_keys`, and adjudicated key sets are
/// byte-matched to the census, so one-directional coverage completeness here
/// closes full set equality.
fn verify_complete_field_census_coverage(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let covered = covered_interior_keys(catalog, census);
    for slice in catalog
        .slices
        .iter()
        .filter(|slice| slice.definition_status == "complete")
    {
        let Some(source_slice) = census
            .slices
            .iter()
            .find(|source_slice| source_slice.slice_id == slice.id)
        else {
            // A missing slice is already reported by the structural census check.
            continue;
        };
        let mut targeted: BTreeSet<&str> = catalog
            .targets
            .iter()
            .filter(|row| {
                row.slice_id == slice.id
                    && (row.source_key.starts_with("field|")
                        || (row.target_kind == "union" && row.source_key.starts_with("union|"))
                        || (row.target_kind == "union-arm" && row.source_key.starts_with("arm|")))
            })
            .map(|row| row.source_key.as_str())
            .collect();
        targeted.extend(
            catalog
                .ambiguity_adjudications
                .iter()
                .filter(|row| {
                    row.slice_id == slice.id
                        && row.resolution == "not-a-durable-schema"
                        && ambiguity_adjudication_contract_matches_with(
                            &AMBIGUITY_ADJUDICATION_CONTRACT,
                            row,
                        )
                })
                .flat_map(|row| row.resolved_source_keys.iter().map(String::as_str))
                .filter(|key| {
                    key.starts_with("field|")
                        || key.starts_with("union|")
                        || key.starts_with("arm|")
                }),
        );
        check_census_class(
            "field",
            source_slice.fields.iter().map(|row| row.key.source_key()),
            &targeted,
            &covered.fields,
            &slice.id,
            out,
        );
        check_census_class(
            "union",
            source_slice.unions.iter().map(|row| row.key.source_key()),
            &targeted,
            &covered.unions,
            &slice.id,
            out,
        );
        check_census_class(
            "arm",
            source_slice.arms.iter().map(|row| row.key.source_key()),
            &targeted,
            &covered.arms,
            &slice.id,
            out,
        );
    }
}

fn check_census_class(
    class: &str,
    keys: impl Iterator<Item = String>,
    targeted: &BTreeSet<&str>,
    covered_keys: &BTreeSet<String>,
    slice_id: &str,
    out: &mut Vec<Violation>,
) {
    for key in keys {
        if !targeted.contains(key.as_str()) && !covered_keys.contains(&key) {
            out.push(Violation::new(
                "source_complete_census_uncovered",
                slice_id,
                format!(
                    "complete slice census {class} key {key:?} has no target, approved adjudication, or covering arm/wire interior contract"
                ),
            ));
        }
    }
}

fn verify_ambiguity_adjudications(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let mut expected: BTreeMap<String, (&str, &AmbiguityCandidate, Vec<String>)> = BTreeMap::new();
    for slice in &census.slices {
        for ambiguity in &slice.ambiguities {
            expected.insert(
                ambiguity.key.source_key(),
                (
                    slice.slice_id.as_str(),
                    ambiguity,
                    structural_locations(catalog, &ambiguity.locations),
                ),
            );
        }
    }
    let actual: BTreeMap<&str, &AmbiguityAdjudication> = catalog
        .ambiguity_adjudications
        .iter()
        .map(|row| (row.ambiguity_source_key.as_str(), row))
        .collect();
    let top_level_source_coverage = approved_top_level_source_coverage(catalog);
    let covered = covered_interior_keys(catalog, census);
    let mut projected_source_keys: BTreeSet<&str> = catalog
        .targets
        .iter()
        .filter(|row| !row.source_key.starts_with("top|"))
        .map(|row| row.source_key.as_str())
        .collect();
    projected_source_keys.extend(top_level_source_coverage.keys().copied());
    // Arm-payload and wire-interior census fields, unions, and arms are
    // projected through their covering arm/wire contracts (fgdb-z35a):
    // maps-to-source may resolve to them, and not-a-durable-schema over them
    // is contradictory.
    projected_source_keys.extend(covered.fields.iter().map(String::as_str));
    projected_source_keys.extend(covered.unions.iter().map(String::as_str));
    projected_source_keys.extend(covered.arms.iter().map(String::as_str));
    for (source_key, row) in &actual {
        let Some((slice_id, ambiguity, locations)) = expected.get(*source_key) else {
            out.push(Violation::new(
                "source_ambiguity_adjudication_orphan",
                &row.row_id,
                "catalog adjudication key is absent from the raw source ambiguity census",
            ));
            continue;
        };
        if row.slice_id != *slice_id || row.source_locations != *locations {
            out.push(Violation::new(
                "source_ambiguity_adjudication_mismatch",
                &row.row_id,
                "adjudication slice and source locations must exactly match the raw ambiguity census",
            ));
        }
        if matches!(
            row.resolution.as_str(),
            "maps-to-source" | "not-a-durable-schema"
        ) {
            if !final_ambiguity_resolution_matches(row, ambiguity) {
                out.push(Violation::new(
                    "source_ambiguity_resolution_relation_mismatch",
                    &row.row_id,
                    "final adjudication must byte-match the parser-owned exact affected source-key set; only an unowned structural fragment may close with an empty set",
                ));
            }
            for resolved in &row.resolved_source_keys {
                let projection_matches = projected_source_keys.contains(resolved.as_str());
                if (row.resolution == "maps-to-source" && !projection_matches)
                    || (row.resolution == "not-a-durable-schema" && projection_matches)
                {
                    out.push(Violation::new(
                        "source_ambiguity_resolution_projection_mismatch",
                        &row.row_id,
                        format!(
                            "resolution {:?} is inconsistent with projected source key {resolved:?}",
                            row.resolution
                        ),
                    ));
                }
            }
        }
    }

    for slice in catalog
        .slices
        .iter()
        .filter(|slice| slice.definition_status == "complete")
    {
        let expected_keys: BTreeSet<String> = census
            .slices
            .iter()
            .find(|source_slice| source_slice.slice_id == slice.id)
            .into_iter()
            .flat_map(|source_slice| &source_slice.ambiguities)
            .map(|row| row.key.source_key())
            .collect();
        let final_keys: BTreeSet<String> =
            catalog
                .ambiguity_adjudications
                .iter()
                .filter(|row| {
                    row.slice_id == slice.id
                        && matches!(
                            row.resolution.as_str(),
                            "maps-to-source" | "not-a-durable-schema"
                        )
                        && ambiguity_adjudication_contract_matches_with(
                            &AMBIGUITY_ADJUDICATION_CONTRACT,
                            row,
                        )
                        && expected.get(&row.ambiguity_source_key).is_some_and(
                            |(_, ambiguity, _)| final_ambiguity_resolution_matches(row, ambiguity),
                        )
                })
                .map(|row| row.ambiguity_source_key.clone())
                .collect();
        if final_keys != expected_keys {
            out.push(Violation::new(
                "source_complete_slice_ambiguity_unresolved",
                &slice.id,
                "complete slice requires one approved final adjudication for every raw source ambiguity and no extras",
            ));
        }
    }
}

fn final_ambiguity_resolution_matches(
    row: &AmbiguityAdjudication,
    ambiguity: &AmbiguityCandidate,
) -> bool {
    if row.resolved_source_keys != ambiguity.affected_source_keys {
        return false;
    }
    match row.resolution.as_str() {
        "maps-to-source" => !ambiguity.affected_source_keys.is_empty(),
        "not-a-durable-schema" => {
            !ambiguity.affected_source_keys.is_empty()
                || ambiguity.key.kind == AmbiguityKind::UnownedStructuralFragment
        }
        _ => false,
    }
}

fn verify_structural_target_source_keys(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let source_keys: BTreeSet<String> = census
        .schemas
        .iter()
        .map(|row| row.key.source_key())
        .chain(census.fields.iter().map(|row| row.key.source_key()))
        .chain(census.unions.iter().map(|row| row.key.source_key()))
        .chain(census.arms.iter().map(|row| row.key.source_key()))
        .collect();
    for target in &catalog.targets {
        if target.slice_id == "g0"
            || target.source_key.starts_with("reference|")
            || target.source_key.starts_with("projection|")
        {
            continue;
        }
        if !source_keys.contains(&target.source_key) {
            out.push(Violation::new(
                "source_target_key_missing",
                &target.row_id,
                format!(
                    "target source_key {:?} is absent from the structural source census",
                    target.source_key
                ),
            ));
        }
    }
}

fn verify_annotation_source_contracts(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let target_by_projection: BTreeMap<&str, &Target> = catalog
        .targets
        .iter()
        .map(|target| (target.target_row_id.as_str(), target))
        .collect();
    let field_by_source_key: BTreeMap<String, &FieldCandidate> = census
        .fields
        .iter()
        .map(|field| (field.key.source_key(), field))
        .collect();
    let schema_by_source_key: BTreeMap<String, &SchemaCandidate> = census
        .schemas
        .iter()
        .map(|schema| (schema.key.source_key(), schema))
        .collect();
    let ambiguity_by_source_key: BTreeMap<String, &AmbiguityCandidate> = census
        .ambiguities
        .iter()
        .map(|ambiguity| (ambiguity.key.source_key(), ambiguity))
        .collect();

    for annotation in &catalog.annotations {
        let Some(target) = target_by_projection
            .get(annotation.target_row_id.as_str())
            .copied()
        else {
            continue;
        };
        if let Some(field) = field_by_source_key.get(&target.source_key).copied() {
            let field_source_key = field.key.source_key();
            let ambiguity_is_discharged = !field.ambiguous
                || catalog.ambiguity_adjudications.iter().any(|row| {
                    row.resolution == "maps-to-source"
                        && row.resolved_source_keys.contains(&field_source_key)
                        && ambiguity_adjudication_contract_matches_with(
                            &AMBIGUITY_ADJUDICATION_CONTRACT,
                            row,
                        )
                        && ambiguity_by_source_key
                            .get(&row.ambiguity_source_key)
                            .is_some_and(|ambiguity| {
                                final_ambiguity_resolution_matches(row, ambiguity)
                            })
                });
            let source_is_exact = ambiguity_is_discharged
                && !field.type_conflict
                && matches!(field.exact_types.as_slice(), [_])
                && matches!(field.cardinalities.as_slice(), [_]);
            if !source_is_exact {
                if target.definition_status == "complete" {
                    out.push(Violation::new(
                        "source_annotation_contract_ambiguous",
                        &annotation.row_id,
                        "complete field annotation requires one unambiguous source exact_type and cardinality",
                    ));
                }
                continue;
            }
            let exact_type = &field.exact_types[0];
            let cardinality = field.cardinalities[0].as_str();
            if annotation.exact_type != exact_type.as_str() || annotation.cardinality != cardinality
            {
                out.push(Violation::new(
                    "source_annotation_contract_mismatch",
                    &annotation.row_id,
                    format!(
                        "field annotation must byte-match source exact_type {exact_type:?} and cardinality {cardinality:?}"
                    ),
                ));
            }
            continue;
        }

        if let Some(schema) = schema_by_source_key.get(&target.source_key).copied() {
            let exact_type_matches = annotation.exact_type == schema.key.family;
            let expansions_match =
                top_level_annotation_expansions_match(catalog, annotation, schema, &census.schemas);
            if !exact_type_matches || !expansions_match {
                out.push(Violation::new(
                    "source_annotation_contract_mismatch",
                    &annotation.row_id,
                    format!(
                        "top-level annotation must name source family {:?} and discharge generic signature {:?} through exact concrete role/generic expansions",
                        schema.key.family, schema.key.generic_signature
                    ),
                ));
            }
        }
    }
}

fn top_level_annotation_expansions_match(
    catalog: &Catalog,
    annotation: &Annotation,
    schema: &SchemaCandidate,
    schemas: &[SchemaCandidate],
) -> bool {
    top_level_annotation_expansions_match_with(
        &EXPANSION_BINDING_CONTRACT,
        catalog,
        annotation,
        schema,
        schemas,
    )
}

fn top_level_annotation_expansions_match_with(
    contract: &[ExpansionBindingContractPin],
    catalog: &Catalog,
    annotation: &Annotation,
    schema: &SchemaCandidate,
    schemas: &[SchemaCandidate],
) -> bool {
    let approved: Vec<_> = catalog
        .expansion_bindings
        .iter()
        .filter(|row| {
            row.target_row_id == annotation.target_row_id
                && expansion_binding_contract_matches_with(contract, catalog, row)
        })
        .collect();
    let family_signatures = schemas
        .iter()
        .filter(|candidate| candidate.key.family == schema.key.family)
        .map(|candidate| candidate.key.generic_signature.as_str());
    let Some(dimensions) = expansion_dimensions(&schema.key.generic_signature, family_signatures)
    else {
        return false;
    };
    if !expansion_bindings_match_dimensions(&approved, &dimensions) {
        return false;
    }
    let mut role_expansions = BTreeSet::new();
    let mut generic_expansions = BTreeSet::new();
    for binding in approved {
        let actual: BTreeSet<String> = binding.values.iter().cloned().collect();
        if binding.values.len() != actual.len() {
            return false;
        }
        expansion_set_for_formal(
            &binding.formal,
            &mut role_expansions,
            &mut generic_expansions,
        )
        .extend(actual);
    }
    annotation.role_expansions.iter().eq(role_expansions.iter())
        && annotation
            .generic_expansions
            .iter()
            .eq(generic_expansions.iter())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpansionDimension {
    parameter_ordinal: i64,
    explicit_formal: Option<String>,
    source_values: BTreeSet<String>,
}

fn expansion_dimensions<'a>(
    selected_signature: &str,
    family_signatures: impl IntoIterator<Item = &'a str>,
) -> Option<Vec<ExpansionDimension>> {
    let selected: Vec<String> = generic_signature_parameters(selected_signature)?
        .into_iter()
        .map(str::to_owned)
        .collect();
    let family: Vec<Vec<String>> = family_signatures
        .into_iter()
        .map(|signature| {
            generic_signature_parameters(signature)
                .map(|parameters| parameters.into_iter().map(str::to_owned).collect())
        })
        .collect::<Option<_>>()?;
    if family
        .iter()
        .any(|parameters| parameters.len() != selected.len())
    {
        return None;
    }

    let mut dimensions = Vec::new();
    for (index, _) in selected.iter().enumerate() {
        let parameter_ordinal = i64::try_from(index).ok()?.checked_add(1)?;
        let raw_parameters: BTreeSet<&str> = family
            .iter()
            .map(|parameters| parameters[index].as_str())
            .collect();
        let explicit_formals: BTreeSet<&str> = raw_parameters
            .iter()
            .filter_map(|parameter| generic_parameter_formal(parameter))
            .collect();
        if explicit_formals.len() > 1 {
            return None;
        }
        let explicit_formal = explicit_formals.first().map(|formal| (*formal).to_owned());
        let mut source_values = BTreeSet::new();
        for parameter in raw_parameters.iter().copied() {
            if let Some(values) = concrete_parameter_values(parameter) {
                source_values.extend(values);
            }
        }
        if explicit_formal.is_none() && raw_parameters.len() > 1 && source_values.is_empty() {
            return None;
        }
        let requires_binding =
            explicit_formal.is_some() || raw_parameters.len() > 1 || source_values.len() > 1;
        if requires_binding {
            dimensions.push(ExpansionDimension {
                parameter_ordinal,
                explicit_formal,
                source_values,
            });
        }
    }
    Some(dimensions)
}

fn concrete_parameter_values(parameter: &str) -> Option<Vec<String>> {
    if let Some((formal, values)) = parameter.split_once(':') {
        if valid_generic_formal_token(formal.trim()) && values.contains('|') {
            concrete_parameter_alternatives(values.trim())
        } else {
            None
        }
    } else if generic_parameter_formal(parameter).is_some() {
        None
    } else {
        concrete_parameter_alternatives(parameter)
    }
}

fn expansion_bindings_match_dimensions(
    bindings: &[&ExpansionBinding],
    dimensions: &[ExpansionDimension],
) -> bool {
    if bindings.len() != dimensions.len() {
        return false;
    }
    let mut used = BTreeSet::new();
    for dimension in dimensions {
        let matches: Vec<_> = bindings
            .iter()
            .enumerate()
            .filter(|(index, binding)| {
                if used.contains(index) {
                    return false;
                }
                let actual: BTreeSet<String> = binding.values.iter().cloned().collect();
                binding.values.len() == actual.len()
                    && binding.parameter_ordinal == dimension.parameter_ordinal
                    && dimension
                        .explicit_formal
                        .as_deref()
                        .is_none_or(|formal| binding.formal == formal)
                    && (dimension.source_values.is_empty() || actual == dimension.source_values)
            })
            .map(|(index, _)| index)
            .collect();
        if matches.len() != 1 {
            return false;
        }
        used.insert(matches[0]);
    }
    true
}

fn approved_top_level_source_coverage(catalog: &Catalog) -> BTreeMap<&str, &Target> {
    approved_top_level_source_coverage_with(&EXPANSION_BINDING_CONTRACT, catalog)
}

fn approved_top_level_source_coverage_with<'a>(
    contract: &[ExpansionBindingContractPin],
    catalog: &'a Catalog,
) -> BTreeMap<&'a str, &'a Target> {
    let mut candidates: BTreeMap<&str, BTreeMap<&str, &Target>> = BTreeMap::new();
    for target in catalog
        .targets
        .iter()
        .filter(|target| target.source_key.starts_with("top|"))
    {
        candidates
            .entry(target.source_key.as_str())
            .or_default()
            .insert(target.row_id.as_str(), target);
        let Some(selected) = catalog
            .top_level_candidates
            .iter()
            .find(|candidate| candidate.source_key == target.source_key)
        else {
            continue;
        };
        let family: Vec<_> = catalog
            .top_level_candidates
            .iter()
            .filter(|candidate| candidate.symbol == selected.symbol)
            .collect();
        if family
            .iter()
            .any(|candidate| candidate.identity_class != selected.identity_class)
        {
            continue;
        }
        let Some(dimensions) = expansion_dimensions(
            &selected.generic_signature,
            family
                .iter()
                .map(|candidate| candidate.generic_signature.as_str()),
        ) else {
            continue;
        };
        let approved: Vec<_> = catalog
            .expansion_bindings
            .iter()
            .filter(|row| {
                row.target_row_id == target.target_row_id
                    && expansion_binding_contract_matches_with(contract, catalog, row)
            })
            .collect();
        if !expansion_bindings_match_dimensions(&approved, &dimensions) {
            continue;
        }
        for candidate in family {
            candidates
                .entry(candidate.source_key.as_str())
                .or_default()
                .insert(target.row_id.as_str(), target);
        }
    }
    candidates
        .into_iter()
        .filter_map(|(source_key, targets)| {
            let mut targets = targets.into_values();
            let target = targets.next()?;
            targets.next().is_none().then_some((source_key, target))
        })
        .collect()
}

fn top_level_coverage_for_slice<'a>(
    catalog: &'a Catalog,
    coverage: &BTreeMap<&'a str, &'a Target>,
    slice_id: &str,
) -> (Vec<&'a str>, BTreeMap<&'a str, &'a Target>) {
    let mut source_keys = Vec::new();
    let mut targets = BTreeMap::new();
    for candidate in catalog
        .top_level_candidates
        .iter()
        .filter(|candidate| candidate.slice_id == slice_id)
    {
        if let Some(target) = coverage.get(candidate.source_key.as_str()).copied() {
            source_keys.push(candidate.source_key.as_str());
            targets.insert(target.target_row_id.as_str(), target);
        }
    }
    (source_keys, targets)
}

fn expansion_set_for_formal<'a>(
    formal: &str,
    role_expansions: &'a mut BTreeSet<String>,
    generic_expansions: &'a mut BTreeSet<String>,
) -> &'a mut BTreeSet<String> {
    if formal == "Role" {
        role_expansions
    } else {
        generic_expansions
    }
}

fn generic_signature_parameters(signature: &str) -> Option<Vec<&str>> {
    if signature.is_empty() {
        return Some(Vec::new());
    }
    let inner = signature.strip_prefix('<')?.strip_suffix('>')?.trim();
    if inner.is_empty() {
        return None;
    }
    Some(inner.split(',').map(str::trim).collect())
}

fn generic_parameter_formal(parameter: &str) -> Option<&str> {
    let (formal, has_bound) = parameter
        .split_once(':')
        .map_or((parameter.trim(), false), |(formal, _)| {
            (formal.trim(), true)
        });
    (valid_generic_formal_token(formal) && (has_bound || KNOWN_GENERIC_FORMALS.contains(&formal)))
        .then_some(formal)
}

fn concrete_parameter_alternatives(parameter: &str) -> Option<Vec<String>> {
    if generic_parameter_formal(parameter).is_some() {
        return None;
    }
    let values: Vec<String> = parameter
        .split('|')
        .map(str::trim)
        .map(str::to_owned)
        .collect();
    (!values.is_empty()
        && values.iter().all(|value| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }))
    .then_some(values)
}

fn verify_top_level_source_candidates(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let mut expected = BTreeMap::new();
    for slice in &census.slices {
        for candidate in &slice.schemas {
            let source_key = candidate.key.source_key();
            let locations = structural_locations(catalog, &candidate.locations);
            expected.insert(
                source_key,
                (
                    slice.slice_id.as_str(),
                    candidate.key.family.as_str(),
                    candidate.key.generic_signature.as_str(),
                    structural_source_kind(candidate),
                    locations,
                ),
            );
        }
    }
    let actual: BTreeMap<&str, &TopLevelCandidate> = catalog
        .top_level_candidates
        .iter()
        .map(|row| (row.source_key.as_str(), row))
        .collect();

    for (source_key, (slice_id, symbol, generic_signature, source_kind, locations)) in &expected {
        match actual.get(source_key.as_str()).copied() {
            Some(row)
                if row.slice_id == *slice_id
                    && row.symbol == *symbol
                    && row.generic_signature == *generic_signature
                    && row.source_kind == *source_kind
                    && row.source_locations == *locations => {}
            Some(row) => out.push(Violation::new(
                "source_top_level_candidate_mismatch",
                &row.row_id,
                format!("catalog row does not exactly match source candidate {source_key:?}"),
            )),
            None => out.push(Violation::new(
                "source_top_level_candidate_missing",
                source_key,
                "source-derived top-level candidate has no catalog row",
            )),
        }
    }
    for (source_key, row) in actual {
        if !expected.contains_key(source_key) {
            out.push(Violation::new(
                "source_top_level_candidate_orphan",
                &row.row_id,
                format!("catalog candidate {source_key:?} is absent from the source census"),
            ));
        }
    }
}

fn verify_reference_source_census(
    catalog: &Catalog,
    source: &[u8],
    structural: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let census = match census_plan_references(source) {
        Ok(census) => census,
        Err(error) => {
            out.push(Violation::new(
                error.code,
                "reference_manifest",
                format!(
                    "reference census failed at line {}, column {}",
                    error.line, error.column
                ),
            ));
            return;
        }
    };
    let target_count = i64::try_from(census.target_count).unwrap_or(i64::MAX);
    let occurrence_count = i64::try_from(census.occurrence_count).unwrap_or(i64::MAX);
    let manifest = &catalog.reference_manifest;
    if manifest.target_count != target_count
        || manifest.target_ids_sha256 != census.target_ids_sha256
        || manifest.occurrence_count != occurrence_count
        || manifest.occurrence_transcript_sha256 != census.occurrence_transcript_sha256
    {
        out.push(Violation::new(
            "reference_source_manifest_mismatch",
            "reference_manifest",
            format!(
                "reference source census is {target_count}/{}/{} occurrences/{}",
                census.target_ids_sha256, occurrence_count, census.occurrence_transcript_sha256
            ),
        ));
    }

    let reservation_by_symbol: BTreeMap<&str, &Reservation> = catalog
        .reservations
        .iter()
        .map(|row| (row.symbol.as_str(), row))
        .collect();
    let reservation_symbols: BTreeSet<&str> = reservation_by_symbol.keys().copied().collect();
    let source_symbols: BTreeSet<&str> = census
        .targets
        .iter()
        .map(|target| target.family.as_str())
        .collect();
    for symbol in source_symbols.difference(&reservation_symbols) {
        out.push(Violation::new(
            "reference_source_reservation_missing",
            *symbol,
            "source-derived reference target has no permanent reservation",
        ));
    }
    for symbol in reservation_symbols.difference(&source_symbols) {
        out.push(Violation::new(
            "reference_source_reservation_orphan",
            *symbol,
            "permanent reservation is absent from the source-derived reference census",
        ));
    }

    let disposition_by_symbol: BTreeMap<&str, &SourceSymbolDisposition> = catalog
        .source_symbol_dispositions
        .iter()
        .filter(|row| row.slice_id != "g0")
        .map(|row| (row.symbol.as_str(), row))
        .collect();
    let structural_dispositions = structural_dispositions(structural);
    for target in &census.targets {
        let (expected_owner, source_derived_disposition) =
            reference_source_owner(catalog, target, structural);
        let expected_locations: Vec<String> = target
            .occurrences
            .iter()
            .map(|occurrence| source_location(catalog, occurrence.line))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let expected_disposition = structural_dispositions
            .get(target.family.as_str())
            .copied()
            .unwrap_or(source_derived_disposition);
        if let Some(reservation) = reservation_by_symbol.get(target.family.as_str()).copied()
            && reservation.slice_id != expected_owner
        {
            out.push(Violation::new(
                "reference_source_reservation_owner_mismatch",
                &reservation.row_id,
                format!("source-derived reservation owner must be {expected_owner:?}"),
            ));
        }
        match disposition_by_symbol.get(target.family.as_str()).copied() {
            Some(row)
                if row.source_locations == expected_locations
                    && row.disposition == expected_disposition
                    && row.slice_id == expected_owner => {}
            Some(row) => out.push(Violation::new(
                "reference_source_disposition_mismatch",
                &row.row_id,
                format!(
                    "reference source requires owner {expected_owner:?}, disposition {expected_disposition:?}, and locations {expected_locations:?}"
                ),
            )),
            None => out.push(Violation::new(
                "reference_source_disposition_missing",
                &target.family,
                "source-derived reference target has no source disposition",
            )),
        }
    }
    for (symbol, row) in disposition_by_symbol {
        if !source_symbols.contains(symbol) {
            out.push(Violation::new(
                "reference_source_disposition_orphan",
                &row.row_id,
                format!("source disposition {symbol:?} is absent from the reference census"),
            ));
        }
    }
}

fn reference_source_owner<'a>(
    catalog: &'a Catalog,
    target: &ReferenceTarget,
    structural: &AppendixSourceCensus,
) -> (&'a str, &'static str) {
    let structural_owner = structural
        .schemas
        .iter()
        .filter(|candidate| candidate.key.family == target.family)
        .filter_map(|candidate| {
            let location = candidate.locations.iter().min()?;
            let disposition = structural_source_kind(candidate);
            let rank = match disposition {
                "confirmed" => 0u8,
                "ambiguous" => 1u8,
                _ => 2u8,
            };
            Some((
                rank,
                location.start.line,
                location.start.column,
                candidate.key.generic_signature.as_str(),
                disposition,
            ))
        })
        .min_by(|left, right| {
            (left.0, left.1, left.2, left.3).cmp(&(right.0, right.1, right.2, right.3))
        });
    if let Some((_, line, _, _, source_kind)) = structural_owner {
        let disposition = match source_kind {
            "confirmed" => "appendix-structural-definition",
            "ambiguous" => "appendix-ambiguous-structure",
            _ => "appendix-name-only",
        };
        return (source_slice_id(catalog, line), disposition);
    }

    let appendix_reference = target
        .occurrences
        .iter()
        .filter(|occurrence| source_slice_id(catalog, occurrence.line) != "plan")
        .min_by(|left, right| {
            (
                left.line,
                left.column,
                left.wrapper.as_str(),
                left.target_expression.as_str(),
            )
                .cmp(&(
                    right.line,
                    right.column,
                    right.wrapper.as_str(),
                    right.target_expression.as_str(),
                ))
        });
    (
        appendix_reference.map_or("plan", |occurrence| {
            source_slice_id(catalog, occurrence.line)
        }),
        "reference-only",
    )
}

fn structural_source_kind(candidate: &SchemaCandidate) -> &'static str {
    if candidate
        .owner_statuses
        .contains(&SchemaOwnerStatus::ConfirmedTopLevel)
    {
        "confirmed"
    } else if candidate
        .owner_statuses
        .contains(&SchemaOwnerStatus::AmbiguousUnownedStructure)
    {
        "ambiguous"
    } else {
        "name-only"
    }
}

fn structural_dispositions(census: &AppendixSourceCensus) -> BTreeMap<&str, &'static str> {
    let mut kinds: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for candidate in &census.schemas {
        kinds
            .entry(candidate.key.family.as_str())
            .or_default()
            .insert(structural_source_kind(candidate));
    }
    kinds
        .into_iter()
        .map(|(family, kinds)| {
            let disposition = if kinds.contains("confirmed") {
                "appendix-structural-definition"
            } else if kinds.contains("ambiguous") {
                "appendix-ambiguous-structure"
            } else {
                "appendix-name-only"
            };
            (family, disposition)
        })
        .collect()
}

fn structural_locations(
    catalog: &Catalog,
    spans: &[crate::appendix_source::SourceSpan],
) -> Vec<String> {
    spans
        .iter()
        .map(|span| source_location(catalog, span.start.line))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn source_slice_id(catalog: &Catalog, line: usize) -> &str {
    i64::try_from(line)
        .ok()
        .and_then(|line| {
            catalog
                .slices
                .iter()
                .find(|slice| (slice.start_line..=slice.end_line).contains(&line))
        })
        .map_or("plan", |slice| slice.id.as_str())
}

fn source_location(catalog: &Catalog, line: usize) -> String {
    format!("{}:{line}", source_slice_id(catalog, line))
}

fn matching_angle(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate().skip(open) {
        match byte {
            b'<' => depth = depth.checked_add(1)?,
            b'>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_projection_epochs(
    root: &Table,
    violations: &mut Vec<Violation>,
) -> Option<BTreeMap<String, i64>> {
    let tables = read_table_array(root, "projection_epoch", "catalog", violations)?;
    if tables.len() != PROJECTION_CLASSES.len() {
        violations.push(Violation::new(
            "projection_epoch_count",
            "projection_epoch",
            format!(
                "expected exactly {} projection epochs, found {}",
                PROJECTION_CLASSES.len(),
                tables.len()
            ),
        ));
    }

    let mut epochs = BTreeMap::new();
    for (index, table) in tables.iter().enumerate() {
        let row_id = format!("projection_epoch[{index}]");
        exact_keys(table, &PROJECTION_EPOCH_KEYS, &row_id, violations);
        let registry = read_string(table, "registry", &row_id, violations);
        let epoch = read_int(table, "registry_epoch", &row_id, violations);
        let (Some(registry), Some(epoch)) = (registry, epoch) else {
            continue;
        };
        if let Some(expected) = PROJECTION_CLASSES.get(index)
            && registry != *expected
        {
            violations.push(Violation::new(
                "projection_epoch_order",
                &row_id,
                format!("expected registry {expected:?}, found {registry:?}"),
            ));
        }
        if !PROJECTION_CLASSES.contains(&registry.as_str()) {
            violations.push(Violation::new(
                "projection_epoch_unknown",
                &row_id,
                format!("unknown projection registry {registry:?}"),
            ));
        }
        if epoch <= 0 {
            violations.push(Violation::new(
                "projection_epoch_invalid",
                &row_id,
                "registry_epoch must be positive",
            ));
        }
        if epochs.insert(registry.clone(), epoch).is_some() {
            violations.push(Violation::new(
                "projection_epoch_duplicate",
                &row_id,
                format!("duplicate registry {registry:?}"),
            ));
        }
    }
    Some(epochs)
}

fn parse_identity_projections(
    root: &Table,
    epochs: &BTreeMap<String, i64>,
    violations: &mut Vec<Violation>,
) -> Option<(IdentityRegistries, Vec<ProjectionRowMeta>)> {
    let mut metadata = Vec::new();
    let logical_root = projection_root(
        root,
        epochs,
        ProjectionSpec {
            catalog_key: "logical_kind",
            registry_name: "logical_object_kinds",
            projection_key: "kind",
            row_kind: "logical-kind",
        },
        &mut metadata,
        violations,
    )?;
    let physical_root = projection_root(
        root,
        epochs,
        ProjectionSpec {
            catalog_key: "physical_kind",
            registry_name: "physical_record_kinds",
            projection_key: "kind",
            row_kind: "physical-kind",
        },
        &mut metadata,
        violations,
    )?;
    let bootstrap_root = projection_root(
        root,
        epochs,
        ProjectionSpec {
            catalog_key: "bootstrap_frame",
            registry_name: "bootstrap_frames",
            projection_key: "frame",
            row_kind: "bootstrap-frame",
        },
        &mut metadata,
        violations,
    )?;
    let prebootstrap_root = projection_root(
        root,
        epochs,
        ProjectionSpec {
            catalog_key: "prebootstrap_kind",
            registry_name: "prebootstrap_artifact_kinds",
            projection_key: "kind",
            row_kind: "prebootstrap-kind",
        },
        &mut metadata,
        violations,
    )?;
    let wire_root = projection_root(
        root,
        epochs,
        ProjectionSpec {
            catalog_key: "wire_type",
            registry_name: "wire_types",
            projection_key: "type",
            row_kind: "wire-type",
        },
        &mut metadata,
        violations,
    )?;
    let fields_root = durable_fields_projection_root(root, epochs, &mut metadata, violations)?;

    let logical = parse_identity_result(
        identity::logical_from(&logical_root),
        "logical_object_kinds",
        violations,
    );
    let physical = parse_identity_result(
        identity::physical_from(&physical_root),
        "physical_record_kinds",
        violations,
    );
    let bootstrap = parse_identity_result(
        identity::bootstrap_from(&bootstrap_root),
        "bootstrap_frames",
        violations,
    );
    let prebootstrap = parse_identity_result(
        identity::prebootstrap_from(&prebootstrap_root),
        "prebootstrap_artifact_kinds",
        violations,
    );
    let wire = parse_identity_result(identity::wire_from(&wire_root), "wire_types", violations);
    let fields = parse_identity_result(
        identity::fields_from(&fields_root),
        "durable_fields",
        violations,
    );
    let (
        Some((logical_epoch, logical)),
        Some((physical_epoch, physical)),
        Some((bootstrap_epoch, bootstrap)),
        Some((prebootstrap_epoch, prebootstrap)),
        Some((wire_epoch, wire)),
        Some((fields_epoch, fields, ordinary_unions, unions)),
    ) = (logical, physical, bootstrap, prebootstrap, wire, fields)
    else {
        return None;
    };

    let mut identity = IdentityRegistries {
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
        ordinary_unions,
        unions,
    };
    canonicalize_identity(&mut identity);
    Some((identity, metadata))
}

fn canonicalize_identity(identity: &mut IdentityRegistries) {
    identity.logical.sort_by(|left, right| {
        (left.object_kind, &left.name).cmp(&(right.object_kind, &right.name))
    });
    identity.physical.sort_by(|left, right| {
        (left.record_kind, &left.name).cmp(&(right.record_kind, &right.name))
    });
    identity
        .bootstrap
        .sort_by(|left, right| (left.frame_kind, &left.name).cmp(&(right.frame_kind, &right.name)));
    identity.prebootstrap.sort_by(|left, right| {
        (left.artifact_kind, &left.name).cmp(&(right.artifact_kind, &right.name))
    });
    identity.wire.sort_by(|left, right| {
        (left.wire_type_id, &left.name).cmp(&(right.wire_type_id, &right.name))
    });
    identity.fields.sort_by(|left, right| {
        (&left.containing_schema, left.field_tag, &left.stable_name).cmp(&(
            &right.containing_schema,
            right.field_tag,
            &right.stable_name,
        ))
    });
    identity.ordinary_unions.sort_by(|left, right| {
        (&left.containing_schema, &left.union_path, &left.union_name).cmp(&(
            &right.containing_schema,
            &right.union_path,
            &right.union_name,
        ))
    });
    for union in &mut identity.ordinary_unions {
        union.arms.sort_by(|left, right| {
            (left.arm_tag, &left.stable_name).cmp(&(right.arm_tag, &right.stable_name))
        });
    }
    identity.unions.sort_by(|left, right| {
        (&left.containing_schema, left.field_tag, &left.union_name).cmp(&(
            &right.containing_schema,
            right.field_tag,
            &right.union_name,
        ))
    });
    for union in &mut identity.unions {
        union.arms.sort_by(|left, right| {
            (left.arm_tag, &left.stable_name).cmp(&(right.arm_tag, &right.stable_name))
        });
    }
}

#[derive(Clone, Copy)]
struct ProjectionSpec {
    catalog_key: &'static str,
    registry_name: &'static str,
    projection_key: &'static str,
    row_kind: &'static str,
}

fn projection_root(
    catalog_root: &Table,
    epochs: &BTreeMap<String, i64>,
    spec: ProjectionSpec,
    metadata: &mut Vec<ProjectionRowMeta>,
    violations: &mut Vec<Violation>,
) -> Option<Table> {
    let rows = catalog_projection_rows(
        catalog_root,
        spec.catalog_key,
        spec.registry_name,
        spec.row_kind,
        metadata,
        violations,
    )?;
    Some(make_projection_root(
        spec.registry_name,
        spec.projection_key,
        projection_epoch(epochs, spec.registry_name, violations),
        rows,
    ))
}

fn durable_fields_projection_root(
    catalog_root: &Table,
    epochs: &BTreeMap<String, i64>,
    metadata: &mut Vec<ProjectionRowMeta>,
    violations: &mut Vec<Violation>,
) -> Option<Table> {
    let fields = catalog_projection_rows(
        catalog_root,
        "field",
        "durable_fields",
        "field",
        metadata,
        violations,
    )?;
    let ordinary_unions = catalog_projection_rows(
        catalog_root,
        "union",
        "durable_fields",
        "union",
        metadata,
        violations,
    )?;
    let ordinary_arms = catalog_projection_rows(
        catalog_root,
        "union_arm",
        "durable_fields",
        "union-arm",
        metadata,
        violations,
    )?;
    let unions = catalog_projection_rows(
        catalog_root,
        "reference_union",
        "durable_fields",
        "reference-union",
        metadata,
        violations,
    )?;
    let arms = catalog_projection_rows(
        catalog_root,
        "reference_union_arm",
        "durable_fields",
        "reference-union-arm",
        metadata,
        violations,
    )?;
    let mut root = base_projection_root(
        "durable_fields",
        projection_epoch(epochs, "durable_fields", violations),
    );
    root.insert("field".into(), Value::Array(fields));
    root.insert("union".into(), Value::Array(ordinary_unions));
    root.insert("union_arm".into(), Value::Array(ordinary_arms));
    root.insert("reference_union".into(), Value::Array(unions));
    root.insert("reference_union_arm".into(), Value::Array(arms));
    Some(root)
}

fn catalog_projection_rows(
    catalog_root: &Table,
    catalog_key: &str,
    registry_name: &str,
    row_kind: &str,
    metadata: &mut Vec<ProjectionRowMeta>,
    violations: &mut Vec<Violation>,
) -> Option<Vec<Value>> {
    let tables = read_table_array(catalog_root, catalog_key, "catalog", violations)?;
    let mut rows = Vec::with_capacity(tables.len());
    for (index, table) in tables.iter().enumerate() {
        let context = format!("{catalog_key}[{index}]");
        let slice_id = read_string(table, "slice_id", &context, violations);
        let row_id = read_string(table, "row_id", &context, violations);
        let mut projection = (*table).clone();
        for key in CATALOG_ROW_KEYS {
            projection.remove(key);
        }
        if let (Some(slice_id), Some(row_id)) = (slice_id, row_id) {
            let identity = projection_row_identity(catalog_key, table);
            if let Some((suffix, _)) = &identity {
                let expected = format!("{slice_id}:{row_kind}:{suffix}");
                if row_id != expected {
                    violations.push(Violation::new(
                        "catalog_row_id_derived_mismatch",
                        &row_id,
                        format!(
                            "row_id must be derived from the typed row identity; expected {expected:?}"
                        ),
                    ));
                }
            }
            let (canonical_suffix, canonical_symbol) =
                identity.unwrap_or_else(|| (String::new(), String::new()));
            metadata.push(ProjectionRowMeta {
                projection: registry_name.to_owned(),
                row_kind: row_kind.to_owned(),
                slice_id,
                row_id,
                canonical_suffix,
                canonical_symbol,
            });
        }
        rows.push(Value::Table(projection));
    }
    Some(rows)
}

fn projection_row_identity(catalog_key: &str, table: &Table) -> Option<(String, String)> {
    let components: &[&str] = match catalog_key {
        "logical_kind" | "physical_kind" | "bootstrap_frame" | "prebootstrap_kind"
        | "wire_type" => &["name"],
        "field" => &["containing_schema", "stable_name"],
        "union" => {
            let Value::Str(containing_schema) = table.get("containing_schema")? else {
                return None;
            };
            let Value::Str(union_path) = table.get("union_path")? else {
                return None;
            };
            let Value::Str(union_name) = table.get("union_name")? else {
                return None;
            };
            let source_key = format!("union|{containing_schema}|{union_path}");
            let digest = sha256_hex(source_key.as_bytes());
            return Some((
                format!("{}-{}", lower_kebab(union_name), &digest[..16]),
                format!("{containing_schema}.{union_path}"),
            ));
        }
        "union_arm" => {
            let Value::Str(containing_schema) = table.get("containing_schema")? else {
                return None;
            };
            let Value::Str(union_path) = table.get("union_path")? else {
                return None;
            };
            let Value::Str(source_arm_name) = table.get("source_arm_name")? else {
                return None;
            };
            let Value::Str(union_name) = table.get("union_name")? else {
                return None;
            };
            let Value::Str(stable_name) = table.get("stable_name")? else {
                return None;
            };
            let source_key = format!("arm|{containing_schema}|{union_path}|{source_arm_name}");
            let digest = sha256_hex(source_key.as_bytes());
            return Some((
                format!(
                    "{}-{}-{}",
                    lower_kebab(union_name),
                    lower_kebab(stable_name),
                    &digest[..16]
                ),
                format!("{containing_schema}.{union_path}.{source_arm_name}"),
            ));
        }
        "reference_union" => &["containing_schema", "union_name"],
        "reference_union_arm" => &["union_name", "stable_name"],
        _ => return None,
    };
    let mut suffix_identity = String::new();
    let mut symbol = String::new();
    for key in components {
        let Value::Str(value) = table.get(*key)? else {
            return None;
        };
        if !suffix_identity.is_empty() {
            suffix_identity.push('-');
            symbol.push('.');
        }
        suffix_identity.push_str(value);
        symbol.push_str(value);
    }
    Some((lower_kebab(&suffix_identity), symbol))
}

fn lower_kebab(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch.is_ascii_alphanumeric() {
            let previous = index.checked_sub(1).and_then(|at| chars.get(at)).copied();
            let next = chars.get(index + 1).copied();
            let starts_word = ch.is_ascii_uppercase()
                && previous.is_some_and(|prior| {
                    prior.is_ascii_lowercase()
                        || prior.is_ascii_digit()
                        || (prior.is_ascii_uppercase()
                            && next.is_some_and(|following| following.is_ascii_lowercase()))
                });
            if starts_word && !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

fn top_level_candidate_row_id(
    slice_id: &str,
    symbol: &str,
    generic_signature: &str,
    source_key: &str,
) -> String {
    let digest = sha256_hex(source_key.as_bytes());
    format!(
        "{slice_id}:top-level-candidate:{}-{}",
        lower_kebab(&format!("{symbol}{generic_signature}")),
        &digest[..16]
    )
}

fn projection_identity_class(row_kind: &str) -> Option<&'static str> {
    match row_kind {
        "logical-kind" => Some("logical"),
        "physical-kind" => Some("physical"),
        "bootstrap-frame" => Some("bootstrap"),
        "prebootstrap-kind" => Some("prebootstrap"),
        "wire-type" => Some("wire"),
        _ => None,
    }
}

fn projection_epoch(
    epochs: &BTreeMap<String, i64>,
    registry_name: &str,
    violations: &mut Vec<Violation>,
) -> i64 {
    match epochs.get(registry_name).copied() {
        Some(epoch) => epoch,
        None => {
            violations.push(Violation::new(
                "projection_epoch_missing",
                registry_name,
                "projection registry has no epoch row",
            ));
            0
        }
    }
}

fn make_projection_root(
    registry_name: &str,
    projection_key: &str,
    epoch: i64,
    rows: Vec<Value>,
) -> Table {
    let mut root = base_projection_root(registry_name, epoch);
    root.insert(projection_key.to_owned(), Value::Array(rows));
    root
}

fn base_projection_root(registry_name: &str, epoch: i64) -> Table {
    let mut registry = Table::new();
    registry.insert("name".into(), Value::Str(registry_name.to_owned()));
    registry.insert("registry_epoch".into(), Value::Int(epoch));
    let mut root = Table::new();
    root.insert("schema_version".into(), Value::Int(1));
    root.insert("registry".into(), Value::Table(registry));
    root
}

fn parse_identity_result<T>(
    result: Result<T, toml::ReadError>,
    registry_name: &str,
    violations: &mut Vec<Violation>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            violations.push(Violation::new(
                "catalog_projection_schema",
                registry_name,
                error.to_string(),
            ));
            None
        }
    }
}

fn parse_maintenance_proof(
    table: &Table,
    violations: &mut Vec<Violation>,
) -> Option<MaintenanceProof> {
    let values = (
        read_string(table, "row_id", "maintenance_proof", violations),
        read_string(table, "owner_bead_id", "maintenance_proof", violations),
        read_string(table, "owner_crate", "maintenance_proof", violations),
        read_string_array(table, "covered_artifacts", "maintenance_proof", violations),
        read_string_array(table, "checker_ids", "maintenance_proof", violations),
        read_string_array(table, "scenario_ids", "maintenance_proof", violations),
        read_string_array(table, "event_ids", "maintenance_proof", violations),
        read_string_array(table, "gate_ids", "maintenance_proof", violations),
        read_string(table, "evidence_status", "maintenance_proof", violations),
    );
    match values {
        (
            Some(row_id),
            Some(owner_bead_id),
            Some(owner_crate),
            Some(covered_artifacts),
            Some(checker_ids),
            Some(scenario_ids),
            Some(event_ids),
            Some(gate_ids),
            Some(evidence_status),
        ) => Some(MaintenanceProof {
            row_id,
            owner_bead_id,
            owner_crate,
            covered_artifacts,
            checker_ids,
            scenario_ids,
            event_ids,
            gate_ids,
            evidence_status,
        }),
        _ => None,
    }
}

fn parse_completion_layers(
    root: &Table,
    violations: &mut Vec<Violation>,
) -> Option<Vec<CompletionLayerSchema>> {
    let tables = read_table_array(root, "completion_layer", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("completion_layer[{index}]");
        exact_keys(table, &COMPLETION_LAYER_KEYS, &context, violations);
        let values = (
            read_string(table, "layer", &context, violations),
            read_int(table, "schema_version", &context, violations),
            read_string_array(table, "field_contracts", &context, violations),
            read_string(table, "target_binding", &context, violations),
            read_string(table, "target_cardinality", &context, violations),
            read_string(table, "epoch_domain", &context, violations),
            read_string(table, "projection_policy", &context, violations),
            read_string(table, "authoring_policy", &context, violations),
            read_string(table, "pin_policy", &context, violations),
        );
        if let (
            Some(layer),
            Some(schema_version),
            Some(field_contracts),
            Some(target_binding),
            Some(target_cardinality),
            Some(epoch_domain),
            Some(projection_policy),
            Some(authoring_policy),
            Some(pin_policy),
        ) = values
        {
            rows.push(CompletionLayerSchema {
                layer,
                schema_version,
                field_contracts,
                target_binding,
                target_cardinality,
                epoch_domain,
                projection_policy,
                authoring_policy,
                pin_policy,
            });
        }
    }
    Some(rows)
}

fn parse_reservations(root: &Table, violations: &mut Vec<Violation>) -> Option<Vec<Reservation>> {
    let tables = read_table_array(root, "reservation", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("reservation[{index}]");
        exact_keys(table, &RESERVATION_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "slice_id", &context, violations),
            read_string(table, "symbol", &context, violations),
            read_string(table, "row_kind", &context, violations),
            read_string(table, "identity_class", &context, violations),
            read_string(table, "code_reservation", &context, violations),
            read_string(table, "disposition", &context, violations),
        );
        if let (
            Some(row_id),
            Some(slice_id),
            Some(symbol),
            Some(row_kind),
            Some(identity_class),
            Some(code_reservation),
            Some(disposition),
        ) = values
        {
            rows.push(Reservation {
                row_id,
                slice_id,
                symbol,
                row_kind,
                identity_class,
                code_reservation,
                disposition,
            });
        }
    }
    Some(rows)
}

fn parse_top_level_candidates(
    root: &Table,
    violations: &mut Vec<Violation>,
) -> Option<Vec<TopLevelCandidate>> {
    let tables = read_table_array(root, "top_level_candidate", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("top_level_candidate[{index}]");
        exact_keys(table, &TOP_LEVEL_CANDIDATE_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "slice_id", &context, violations),
            read_string(table, "symbol", &context, violations),
            read_string(table, "generic_signature", &context, violations),
            read_string(table, "source_key", &context, violations),
            read_string(table, "source_kind", &context, violations),
            read_string(table, "identity_class", &context, violations),
            read_string_array(table, "source_locations", &context, violations),
        );
        if let (
            Some(row_id),
            Some(slice_id),
            Some(symbol),
            Some(generic_signature),
            Some(source_key),
            Some(source_kind),
            Some(identity_class),
            Some(source_locations),
        ) = values
        {
            rows.push(TopLevelCandidate {
                row_id,
                slice_id,
                symbol,
                generic_signature,
                source_key,
                source_kind,
                identity_class,
                source_locations,
            });
        }
    }
    Some(rows)
}

fn parse_targets(root: &Table, violations: &mut Vec<Violation>) -> Option<Vec<Target>> {
    let tables = read_table_array(root, "target", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("target[{index}]");
        exact_keys(table, &TARGET_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "target_row_id", &context, violations),
            read_string(table, "slice_id", &context, violations),
            read_string(table, "source_key", &context, violations),
            read_string(table, "target_kind", &context, violations),
            read_string(table, "definition_status", &context, violations),
        );
        if let (
            Some(row_id),
            Some(target_row_id),
            Some(slice_id),
            Some(source_key),
            Some(target_kind),
            Some(definition_status),
        ) = values
        {
            rows.push(Target {
                row_id,
                target_row_id,
                slice_id,
                source_key,
                target_kind,
                definition_status,
            });
        }
    }
    Some(rows)
}

fn parse_annotations(root: &Table, violations: &mut Vec<Violation>) -> Option<Vec<Annotation>> {
    let tables = read_table_array(root, "annotation", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("annotation[{index}]");
        exact_keys(table, &ANNOTATION_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "target_row_id", &context, violations),
            read_string(table, "exact_type", &context, violations),
            read_string(table, "cardinality", &context, violations),
            read_string(table, "layout", &context, violations),
            read_string(table, "role", &context, violations),
            read_string(table, "posture", &context, violations),
            read_string(table, "authority", &context, violations),
            read_string(table, "locality", &context, violations),
            read_string_array(table, "generic_expansions", &context, violations),
            read_string_array(table, "role_expansions", &context, violations),
            read_string(table, "reference_semantics", &context, violations),
            read_string_array(table, "target_schema_ids", &context, violations),
            read_string(table, "construction_order", &context, violations),
            read_string(table, "retention_and_cut_rule", &context, violations),
            read_string(table, "digest_recipe", &context, violations),
            read_string(table, "redaction_class", &context, violations),
            read_string(table, "resource_bounds", &context, violations),
            read_string(table, "compatibility", &context, violations),
        );
        if let (
            Some(row_id),
            Some(target_row_id),
            Some(exact_type),
            Some(cardinality),
            Some(layout),
            Some(role),
            Some(posture),
            Some(authority),
            Some(locality),
            Some(generic_expansions),
            Some(role_expansions),
            Some(reference_semantics),
            Some(target_schema_ids),
            Some(construction_order),
            Some(retention_and_cut_rule),
            Some(digest_recipe),
            Some(redaction_class),
            Some(resource_bounds),
            Some(compatibility),
        ) = values
        {
            rows.push(Annotation {
                row_id,
                target_row_id,
                exact_type,
                cardinality,
                layout,
                role,
                posture,
                authority,
                locality,
                generic_expansions,
                role_expansions,
                reference_semantics,
                target_schema_ids,
                construction_order,
                retention_and_cut_rule,
                digest_recipe,
                redaction_class,
                resource_bounds,
                compatibility,
            });
        }
    }
    Some(rows)
}

fn parse_semantic_bindings(
    root: &Table,
    violations: &mut Vec<Violation>,
) -> Option<Vec<SemanticBinding>> {
    let tables = read_table_array(root, "semantic_binding", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("semantic_binding[{index}]");
        exact_keys(table, &SEMANTIC_BINDING_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "target_row_id", &context, violations),
            read_string(table, "owner_bead_id", &context, violations),
            read_string(table, "owner_crate", &context, violations),
            read_string(table, "owner_status", &context, violations),
            read_string_array(table, "consumer_crates", &context, violations),
        );
        if let (
            Some(row_id),
            Some(target_row_id),
            Some(owner_bead_id),
            Some(owner_crate),
            Some(owner_status),
            Some(consumer_crates),
        ) = values
        {
            rows.push(SemanticBinding {
                row_id,
                target_row_id,
                owner_bead_id,
                owner_crate,
                owner_status,
                consumer_crates,
            });
        }
    }
    Some(rows)
}

fn parse_expansion_bindings(
    root: &Table,
    violations: &mut Vec<Violation>,
) -> Option<Vec<ExpansionBinding>> {
    let tables = read_table_array(root, "expansion_binding", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("expansion_binding[{index}]");
        exact_keys(table, &EXPANSION_BINDING_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "target_row_id", &context, violations),
            read_int(table, "parameter_ordinal", &context, violations),
            read_string(table, "formal", &context, violations),
            read_string(table, "formal_class", &context, violations),
            read_string_array(table, "values", &context, violations),
            read_string(table, "rationale", &context, violations),
        );
        if let (
            Some(row_id),
            Some(target_row_id),
            Some(parameter_ordinal),
            Some(formal),
            Some(formal_class),
            Some(values),
            Some(rationale),
        ) = values
        {
            rows.push(ExpansionBinding {
                row_id,
                target_row_id,
                parameter_ordinal,
                formal,
                formal_class,
                values,
                rationale,
            });
        }
    }
    Some(rows)
}

fn parse_evidence(root: &Table, violations: &mut Vec<Violation>) -> Option<Vec<EvidenceBinding>> {
    let tables = read_table_array(root, "evidence", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("evidence[{index}]");
        exact_keys(table, &EVIDENCE_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "target_row_id", &context, violations),
            read_string(table, "evidence_id", &context, violations),
            read_string(table, "phase", &context, violations),
            read_string(table, "status", &context, violations),
            read_string(table, "owner_bead_id", &context, violations),
            read_string_array(table, "checker_ids", &context, violations),
            read_string_array(table, "scenario_ids", &context, violations),
            read_string_array(table, "event_ids", &context, violations),
            read_string_array(table, "gate_ids", &context, violations),
        );
        if let (
            Some(row_id),
            Some(target_row_id),
            Some(evidence_id),
            Some(phase),
            Some(status),
            Some(owner_bead_id),
            Some(checker_ids),
            Some(scenario_ids),
            Some(event_ids),
            Some(gate_ids),
        ) = values
        {
            rows.push(EvidenceBinding {
                row_id,
                target_row_id,
                evidence_id,
                phase,
                status,
                owner_bead_id,
                checker_ids,
                scenario_ids,
                event_ids,
                gate_ids,
            });
        }
    }
    Some(rows)
}

fn parse_source_symbol_dispositions(
    root: &Table,
    violations: &mut Vec<Violation>,
) -> Option<Vec<SourceSymbolDisposition>> {
    let tables = read_table_array(root, "source_symbol_disposition", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("source_symbol_disposition[{index}]");
        exact_keys(table, &SOURCE_SYMBOL_DISPOSITION_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "slice_id", &context, violations),
            read_string(table, "symbol", &context, violations),
            read_string(table, "disposition", &context, violations),
            read_string_array(table, "source_locations", &context, violations),
        );
        if let (
            Some(row_id),
            Some(slice_id),
            Some(symbol),
            Some(disposition),
            Some(source_locations),
        ) = values
        {
            rows.push(SourceSymbolDisposition {
                row_id,
                slice_id,
                symbol,
                disposition,
                source_locations,
            });
        }
    }
    Some(rows)
}

fn parse_ambiguity_adjudications(
    root: &Table,
    violations: &mut Vec<Violation>,
) -> Option<Vec<AmbiguityAdjudication>> {
    let tables = read_table_array(root, "ambiguity_adjudication", "catalog", violations)?;
    let mut rows = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let context = format!("ambiguity_adjudication[{index}]");
        exact_keys(table, &AMBIGUITY_ADJUDICATION_KEYS, &context, violations);
        let values = (
            read_string(table, "row_id", &context, violations),
            read_string(table, "slice_id", &context, violations),
            read_string(table, "ambiguity_source_key", &context, violations),
            read_string_array(table, "source_locations", &context, violations),
            read_string(table, "resolution", &context, violations),
            read_string_array(table, "resolved_source_keys", &context, violations),
            read_string(table, "rationale", &context, violations),
        );
        if let (
            Some(row_id),
            Some(slice_id),
            Some(ambiguity_source_key),
            Some(source_locations),
            Some(resolution),
            Some(resolved_source_keys),
            Some(rationale),
        ) = values
        {
            rows.push(AmbiguityAdjudication {
                row_id,
                slice_id,
                ambiguity_source_key,
                source_locations,
                resolution,
                resolved_source_keys,
                rationale,
            });
        }
    }
    Some(rows)
}

fn parse_source_manifest(table: &Table, violations: &mut Vec<Violation>) -> Option<SourceManifest> {
    let plan_path = read_string(table, "plan_path", "source_manifest", violations);
    let start_line = read_int(table, "start_line", "source_manifest", violations);
    let end_line = read_int(table, "end_line", "source_manifest", violations);
    let line_count = read_int(table, "line_count", "source_manifest", violations);
    let byte_count = read_int(table, "byte_count", "source_manifest", violations);
    let sha256 = read_string(table, "sha256", "source_manifest", violations);
    let heading = read_string(table, "heading", "source_manifest", violations);
    let next_heading = read_string(table, "next_heading", "source_manifest", violations);
    match (
        plan_path,
        start_line,
        end_line,
        line_count,
        byte_count,
        sha256,
        heading,
        next_heading,
    ) {
        (
            Some(plan_path),
            Some(start_line),
            Some(end_line),
            Some(line_count),
            Some(byte_count),
            Some(sha256),
            Some(heading),
            Some(next_heading),
        ) => Some(SourceManifest {
            plan_path,
            start_line,
            end_line,
            line_count,
            byte_count,
            sha256,
            heading,
            next_heading,
        }),
        _ => None,
    }
}

fn parse_reference_manifest(
    table: &Table,
    violations: &mut Vec<Violation>,
) -> Option<ReferenceManifest> {
    let target_count = read_int(table, "target_count", "reference_manifest", violations);
    let target_ids_sha256 =
        read_string(table, "target_ids_sha256", "reference_manifest", violations);
    let occurrence_count = read_int(table, "occurrence_count", "reference_manifest", violations);
    let occurrence_transcript_sha256 = read_string(
        table,
        "occurrence_transcript_sha256",
        "reference_manifest",
        violations,
    );
    match (
        target_count,
        target_ids_sha256,
        occurrence_count,
        occurrence_transcript_sha256,
    ) {
        (
            Some(target_count),
            Some(target_ids_sha256),
            Some(occurrence_count),
            Some(occurrence_transcript_sha256),
        ) => Some(ReferenceManifest {
            target_count,
            target_ids_sha256,
            occurrence_count,
            occurrence_transcript_sha256,
        }),
        _ => None,
    }
}

fn parse_target_manifest(table: &Table, violations: &mut Vec<Violation>) -> Option<TargetManifest> {
    let target_count = read_int(table, "target_count", "target_manifest", violations);
    let projection_fallback_count = read_int(
        table,
        "projection_fallback_count",
        "target_manifest",
        violations,
    );
    let target_source_assignment_sha256 = read_string(
        table,
        "target_source_assignment_sha256",
        "target_manifest",
        violations,
    );
    match (
        target_count,
        projection_fallback_count,
        target_source_assignment_sha256,
    ) {
        (
            Some(target_count),
            Some(projection_fallback_count),
            Some(target_source_assignment_sha256),
        ) => Some(TargetManifest {
            target_count,
            projection_fallback_count,
            target_source_assignment_sha256,
        }),
        _ => None,
    }
}

fn parse_slice(table: &Table, row_id: &str, violations: &mut Vec<Violation>) -> Option<Slice> {
    let ordinal = read_int(table, "ordinal", row_id, violations);
    let id = read_string(table, "id", row_id, violations);
    let bead_id = read_string(table, "bead_id", row_id, violations);
    let title = read_string(table, "title", row_id, violations);
    let start_line = read_int(table, "start_line", row_id, violations);
    let end_line = read_int(table, "end_line", row_id, violations);
    let line_count = read_int(table, "line_count", row_id, violations);
    let byte_count = read_int(table, "byte_count", row_id, violations);
    let sha256 = read_string(table, "sha256", row_id, violations);
    let predecessor = read_string(table, "predecessor", row_id, violations);
    let successor = read_string(table, "successor", row_id, violations);
    let expected_projection_classes =
        read_string_array(table, "expected_projection_classes", row_id, violations);
    let definition_status = read_string(table, "definition_status", row_id, violations);
    let top_level_candidate_count =
        read_int(table, "top_level_candidate_count", row_id, violations);
    let top_level_candidate_ids_sha256 =
        read_string(table, "top_level_candidate_ids_sha256", row_id, violations);
    let field_candidate_count = read_int(table, "field_candidate_count", row_id, violations);
    let field_candidate_ids_sha256 =
        read_string(table, "field_candidate_ids_sha256", row_id, violations);
    let union_candidate_count = read_int(table, "union_candidate_count", row_id, violations);
    let union_candidate_ids_sha256 =
        read_string(table, "union_candidate_ids_sha256", row_id, violations);
    let arm_candidate_count = read_int(table, "arm_candidate_count", row_id, violations);
    let arm_candidate_ids_sha256 =
        read_string(table, "arm_candidate_ids_sha256", row_id, violations);
    let ambiguity_count = read_int(table, "ambiguity_count", row_id, violations);
    let ambiguity_ids_sha256 = read_string(table, "ambiguity_ids_sha256", row_id, violations);
    match (
        ordinal,
        id,
        bead_id,
        title,
        start_line,
        end_line,
        line_count,
        byte_count,
        sha256,
        predecessor,
        successor,
        expected_projection_classes,
        definition_status,
        top_level_candidate_count,
        top_level_candidate_ids_sha256,
        field_candidate_count,
        field_candidate_ids_sha256,
        union_candidate_count,
        union_candidate_ids_sha256,
        arm_candidate_count,
        arm_candidate_ids_sha256,
        ambiguity_count,
        ambiguity_ids_sha256,
    ) {
        (
            Some(ordinal),
            Some(id),
            Some(bead_id),
            Some(title),
            Some(start_line),
            Some(end_line),
            Some(line_count),
            Some(byte_count),
            Some(sha256),
            Some(predecessor),
            Some(successor),
            Some(expected_projection_classes),
            Some(definition_status),
            Some(top_level_candidate_count),
            Some(top_level_candidate_ids_sha256),
            Some(field_candidate_count),
            Some(field_candidate_ids_sha256),
            Some(union_candidate_count),
            Some(union_candidate_ids_sha256),
            Some(arm_candidate_count),
            Some(arm_candidate_ids_sha256),
            Some(ambiguity_count),
            Some(ambiguity_ids_sha256),
        ) => Some(Slice {
            ordinal,
            id,
            bead_id,
            title,
            start_line,
            end_line,
            line_count,
            byte_count,
            sha256,
            predecessor,
            successor,
            expected_projection_classes,
            definition_status,
            top_level_candidate_count,
            top_level_candidate_ids_sha256,
            field_candidate_count,
            field_candidate_ids_sha256,
            union_candidate_count,
            union_candidate_ids_sha256,
            arm_candidate_count,
            arm_candidate_ids_sha256,
            ambiguity_count,
            ambiguity_ids_sha256,
        }),
        _ => None,
    }
}

fn validate_reference_manifest(catalog: &Catalog, out: &mut Vec<Violation>) {
    let manifest = &catalog.reference_manifest;
    let mut symbols: Vec<&str> = catalog
        .reservations
        .iter()
        .map(|row| row.symbol.as_str())
        .collect();
    symbols.sort_unstable();
    let mut transcript = symbols.join("\n");
    if !transcript.is_empty() {
        transcript.push('\n');
    }
    let target_count = i64::try_from(symbols.len()).unwrap_or(i64::MAX);
    let target_ids_sha256 = sha256_hex(transcript.as_bytes());
    if manifest.target_count != target_count
        || manifest.target_ids_sha256 != target_ids_sha256
        || target_count != i64::try_from(EXPECTED_TYPE_RESERVATION_COUNT).unwrap_or(i64::MAX)
        || manifest.target_ids_sha256 != EXPECTED_REFERENCE_TARGET_IDS_SHA256
        || manifest.occurrence_count
            != i64::try_from(EXPECTED_REFERENCE_OCCURRENCE_COUNT).unwrap_or(i64::MAX)
        || manifest.occurrence_transcript_sha256 != EXPECTED_REFERENCE_OCCURRENCE_SHA256
        || !valid_sha256_hex(&manifest.target_ids_sha256)
        || !valid_sha256_hex(&manifest.occurrence_transcript_sha256)
    {
        out.push(Violation::new(
            "reference_manifest_mismatch",
            "reference_manifest",
            format!(
                "reference manifest must match {target_count} sorted reservation targets/{target_ids_sha256} and the released full-plan occurrence census"
            ),
        ));
    }
}

fn validate_target_manifest(catalog: &Catalog, out: &mut Vec<Violation>) {
    let manifest = &catalog.target_manifest;
    let target_count = i64::try_from(catalog.targets.len()).unwrap_or(i64::MAX);
    let projection_fallback_count = i64::try_from(
        catalog
            .targets
            .iter()
            .filter(|row| row.source_key.starts_with("projection|"))
            .count(),
    )
    .unwrap_or(i64::MAX);
    let assignment_sha256 = target_source_assignment_sha256(&catalog.targets);
    if manifest.target_count != target_count
        || target_count != i64::try_from(EXPECTED_PROJECTION_ROW_COUNT).unwrap_or(i64::MAX)
        || manifest.projection_fallback_count != projection_fallback_count
        || projection_fallback_count
            != i64::try_from(EXPECTED_PROJECTION_FALLBACK_COUNT).unwrap_or(i64::MAX)
        || manifest.target_source_assignment_sha256 != assignment_sha256
        || manifest.target_source_assignment_sha256 != EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256
        || !valid_sha256_hex(&manifest.target_source_assignment_sha256)
    {
        out.push(Violation::new(
            "catalog_target_source_assignment_drift",
            "target_manifest",
            format!(
                "target/source assignment must remain pinned at {target_count} targets, {projection_fallback_count} projection fallbacks, and sha256 {assignment_sha256}"
            ),
        ));
    }
}

fn validate_completion_layer_schema_contract(catalog: &Catalog, out: &mut Vec<Violation>) {
    let actual_sha256 = completion_layer_schema_sha256(&catalog.completion_layers);
    if catalog.completion_layers.len() != EXPECTED_COMPLETION_LAYER_SCHEMA_COUNT
        || actual_sha256 != EXPECTED_COMPLETION_LAYER_SCHEMA_SHA256
    {
        out.push(Violation::new(
            "catalog_completion_layer_schema_drift",
            "completion_layer",
            format!(
                "completion layer schema must contain {EXPECTED_COMPLETION_LAYER_SCHEMA_COUNT} rows with sha256 {EXPECTED_COMPLETION_LAYER_SCHEMA_SHA256}; found {} rows with sha256 {actual_sha256}",
                catalog.completion_layers.len()
            ),
        ));
    }
    if COMPLETION_LAYER_SCHEMA_CONTRACT.len() != EXPECTED_COMPLETION_LAYER_SCHEMA_COUNT {
        out.push(Violation::new(
            "catalog_completion_layer_schema_pin_inconsistent",
            "completion_layer",
            "readable completion layer schema pins and released count must be updated together",
        ));
    }
    let readable_matches = catalog.completion_layers.len()
        == COMPLETION_LAYER_SCHEMA_CONTRACT.len()
        && catalog
            .completion_layers
            .iter()
            .zip(&COMPLETION_LAYER_SCHEMA_CONTRACT)
            .all(|(row, pin)| completion_layer_schema_matches(row, pin));
    if !readable_matches {
        out.push(Violation::new(
            "catalog_completion_layer_schema_mismatch",
            "completion_layer",
            "completion layer rows must byte-match the readable field, binding, cardinality, epoch, projection, authoring, and pin contracts in canonical layer order",
        ));
    }
    for (layer, field_contracts, parser_keys) in [
        (
            "annotation",
            ANNOTATION_FIELD_CONTRACTS.as_slice(),
            ANNOTATION_KEYS.as_slice(),
        ),
        (
            "semantic_binding",
            SEMANTIC_BINDING_FIELD_CONTRACTS.as_slice(),
            SEMANTIC_BINDING_KEYS.as_slice(),
        ),
        (
            "expansion_binding",
            EXPANSION_BINDING_FIELD_CONTRACTS.as_slice(),
            EXPANSION_BINDING_KEYS.as_slice(),
        ),
        (
            "evidence",
            EVIDENCE_FIELD_CONTRACTS.as_slice(),
            EVIDENCE_KEYS.as_slice(),
        ),
    ] {
        if !completion_field_contracts_match_parser(field_contracts, parser_keys) {
            out.push(Violation::new(
                "catalog_completion_layer_schema_implementation_drift",
                layer,
                "readable required field contracts no longer match the strict parser key set and supported scalar/array type vocabulary",
            ));
        }
    }
}

fn completion_field_contracts_match_parser(field_contracts: &[&str], parser_keys: &[&str]) -> bool {
    field_contracts.len() == parser_keys.len()
        && field_contracts
            .iter()
            .zip(parser_keys)
            .all(|(contract, key)| {
                let mut parts = contract.split(':');
                parts.next() == Some(*key)
                    && matches!(parts.next(), Some("string" | "integer" | "string-array"))
                    && parts.next() == Some("required")
                    && parts.next().is_none()
            })
}

fn completion_layer_schema_matches(
    row: &CompletionLayerSchema,
    pin: &CompletionLayerSchemaContractPin,
) -> bool {
    row.layer == pin.layer
        && row.schema_version == pin.schema_version
        && row
            .field_contracts
            .iter()
            .map(String::as_str)
            .eq(pin.field_contracts.iter().copied())
        && row.target_binding == pin.target_binding
        && row.target_cardinality == pin.target_cardinality
        && row.epoch_domain == pin.epoch_domain
        && row.projection_policy == pin.projection_policy
        && row.authoring_policy == pin.authoring_policy
        && row.pin_policy == pin.pin_policy
}

fn validate_binding_contract_pins(catalog: &Catalog, out: &mut Vec<Violation>) {
    let annotation_sha256 = annotation_contract_sha256(&catalog.annotations);
    if catalog.annotations.len() != EXPECTED_ANNOTATION_COUNT
        || annotation_sha256 != EXPECTED_ANNOTATION_SHA256
    {
        out.push(Violation::new(
            "catalog_annotation_contract_drift",
            "annotation",
            format!(
                "annotation contract must contain {EXPECTED_ANNOTATION_COUNT} independently pinned rows with sha256 {EXPECTED_ANNOTATION_SHA256}; found {} rows with sha256 {annotation_sha256}",
                catalog.annotations.len()
            ),
        ));
    }

    let semantic_sha256 = semantic_binding_contract_sha256(&catalog.semantic_bindings);
    if catalog.semantic_bindings.len() != EXPECTED_SEMANTIC_BINDING_COUNT
        || semantic_sha256 != EXPECTED_SEMANTIC_BINDING_SHA256
    {
        out.push(Violation::new(
            "catalog_semantic_binding_contract_drift",
            "semantic_binding",
            format!(
                "semantic binding contract must contain {EXPECTED_SEMANTIC_BINDING_COUNT} independently pinned rows with sha256 {EXPECTED_SEMANTIC_BINDING_SHA256}; found {} rows with sha256 {semantic_sha256}",
                catalog.semantic_bindings.len()
            ),
        ));
    }

    let evidence_sha256 = evidence_binding_contract_sha256(&catalog.evidence);
    if catalog.evidence.len() != EXPECTED_EVIDENCE_BINDING_COUNT
        || evidence_sha256 != EXPECTED_EVIDENCE_BINDING_SHA256
    {
        out.push(Violation::new(
            "catalog_evidence_binding_contract_drift",
            "evidence",
            format!(
                "evidence binding contract must contain {EXPECTED_EVIDENCE_BINDING_COUNT} independently pinned rows with sha256 {EXPECTED_EVIDENCE_BINDING_SHA256}; found {} rows with sha256 {evidence_sha256}",
                catalog.evidence.len()
            ),
        ));
    }
    let expansion_sha256 = expansion_binding_contract_sha256(&catalog.expansion_bindings);
    if catalog.expansion_bindings.len() != EXPECTED_EXPANSION_BINDING_COUNT
        || expansion_sha256 != EXPECTED_EXPANSION_BINDING_SHA256
    {
        out.push(Violation::new(
            "catalog_expansion_binding_contract_drift",
            "expansion_binding",
            format!(
                "expansion binding contract must contain {EXPECTED_EXPANSION_BINDING_COUNT} independently pinned rows with sha256 {EXPECTED_EXPANSION_BINDING_SHA256}; found {} rows with sha256 {expansion_sha256}",
                catalog.expansion_bindings.len()
            ),
        ));
    }
    let ambiguity_sha256 = ambiguity_adjudication_contract_sha256(&catalog.ambiguity_adjudications);
    if catalog.ambiguity_adjudications.len() != EXPECTED_AMBIGUITY_ADJUDICATION_COUNT
        || ambiguity_sha256 != EXPECTED_AMBIGUITY_ADJUDICATION_SHA256
    {
        out.push(Violation::new(
            "catalog_ambiguity_adjudication_contract_drift",
            "ambiguity_adjudication",
            format!(
                "ambiguity adjudication contract must contain {EXPECTED_AMBIGUITY_ADJUDICATION_COUNT} independently pinned rows with sha256 {EXPECTED_AMBIGUITY_ADJUDICATION_SHA256}; found {} rows with sha256 {ambiguity_sha256}",
                catalog.ambiguity_adjudications.len()
            ),
        ));
    }
    validate_readable_annotation_contract(catalog, out);
    validate_readable_binding_contract(catalog, out);
    validate_readable_expansion_contract(catalog, out);
    validate_readable_ambiguity_contract(catalog, out);
}

fn validate_readable_annotation_contract(catalog: &Catalog, out: &mut Vec<Violation>) {
    validate_readable_annotation_contract_with(
        catalog,
        &ANNOTATION_CONTRACT,
        EXPECTED_ANNOTATION_COUNT,
        out,
    );
}

fn validate_readable_annotation_contract_with(
    catalog: &Catalog,
    contract: &[AnnotationContractPin],
    expected_count: usize,
    out: &mut Vec<Violation>,
) {
    if contract.len() != expected_count {
        out.push(Violation::new(
            "catalog_annotation_contract_pin_inconsistent",
            "annotation",
            "readable annotation pins and released transcript count must be updated together",
        ));
    }
    let pins: BTreeMap<&str, &AnnotationContractPin> =
        contract.iter().map(|pin| (pin.row_id, pin)).collect();
    if pins.len() != contract.len() {
        out.push(Violation::new(
            "catalog_annotation_contract_ambiguous",
            "annotation",
            "readable annotation contract contains duplicate row IDs",
        ));
    }
    for row in &catalog.annotations {
        match pins.get(row.row_id.as_str()).copied() {
            Some(pin) if annotation_contract_matches_with(contract, catalog, row) => {
                debug_assert_eq!(pin.row_id, row.row_id);
            }
            Some(_) => out.push(Violation::new(
                "catalog_annotation_contract_mismatch",
                &row.row_id,
                "annotation does not byte-match its readable target/source/schema contract",
            )),
            None => out.push(Violation::new(
                "catalog_annotation_contract_unapproved",
                &row.row_id,
                "annotation has no independent readable per-target contract",
            )),
        }
    }
    let rows: BTreeSet<&str> = catalog
        .annotations
        .iter()
        .map(|row| row.row_id.as_str())
        .collect();
    for pin in contract {
        if !rows.contains(pin.row_id) {
            out.push(Violation::new(
                "catalog_annotation_contract_missing",
                pin.row_id,
                "readable annotation contract has no reciprocal catalog row",
            ));
        }
    }
}

fn validate_readable_binding_contract(catalog: &Catalog, out: &mut Vec<Violation>) {
    validate_readable_binding_contract_with(
        catalog,
        &SEMANTIC_BINDING_CONTRACT,
        &EVIDENCE_BINDING_CONTRACT,
        EXPECTED_SEMANTIC_BINDING_COUNT,
        EXPECTED_EVIDENCE_BINDING_COUNT,
        out,
    );
}

fn validate_readable_binding_contract_with(
    catalog: &Catalog,
    semantic_contract: &[SemanticBindingContractPin],
    evidence_contract: &[EvidenceBindingContractPin],
    expected_semantic_count: usize,
    expected_evidence_count: usize,
    out: &mut Vec<Violation>,
) {
    if semantic_contract.len() != expected_semantic_count
        || evidence_contract.len() != expected_evidence_count
    {
        out.push(Violation::new(
            "catalog_binding_contract_pin_inconsistent",
            "binding_contract",
            "readable per-target binding pins and released transcript counts must be updated together",
        ));
    }

    let target_by_id: BTreeMap<&str, &Target> = catalog
        .targets
        .iter()
        .map(|target| (target.target_row_id.as_str(), target))
        .collect();
    let semantic_pins: BTreeMap<&str, &SemanticBindingContractPin> = semantic_contract
        .iter()
        .map(|pin| (pin.row_id, pin))
        .collect();
    if semantic_pins.len() != semantic_contract.len() {
        out.push(Violation::new(
            "catalog_semantic_binding_contract_ambiguous",
            "semantic_binding",
            "readable semantic binding contract contains duplicate row IDs",
        ));
    }
    for row in &catalog.semantic_bindings {
        let source_key = target_by_id
            .get(row.target_row_id.as_str())
            .map(|target| target.source_key.as_str());
        match semantic_pins.get(row.row_id.as_str()).copied() {
            Some(pin)
                if row.target_row_id == pin.target_row_id
                    && source_key == Some(pin.target_source_key)
                    && row.owner_bead_id == pin.owner_bead_id
                    && row.owner_crate == pin.owner_crate
                    && row.owner_status == pin.owner_status
                    && row
                        .consumer_crates
                        .iter()
                        .map(String::as_str)
                        .eq(pin.consumer_crates.iter().copied()) => {}
            Some(_) => out.push(Violation::new(
                "catalog_semantic_binding_contract_mismatch",
                &row.row_id,
                "semantic binding does not byte-match its readable target/source/owner/consumer contract",
            )),
            None => out.push(Violation::new(
                "catalog_semantic_binding_contract_unapproved",
                &row.row_id,
                "semantic binding has no independent readable per-target contract",
            )),
        }
    }
    let semantic_rows: BTreeSet<&str> = catalog
        .semantic_bindings
        .iter()
        .map(|row| row.row_id.as_str())
        .collect();
    for pin in semantic_contract {
        if !semantic_rows.contains(pin.row_id) {
            out.push(Violation::new(
                "catalog_semantic_binding_contract_missing",
                pin.row_id,
                "readable semantic binding contract has no reciprocal catalog row",
            ));
        }
    }

    let evidence_pins: BTreeMap<&str, &EvidenceBindingContractPin> = evidence_contract
        .iter()
        .map(|pin| (pin.row_id, pin))
        .collect();
    if evidence_pins.len() != evidence_contract.len() {
        out.push(Violation::new(
            "catalog_evidence_binding_contract_ambiguous",
            "evidence",
            "readable evidence binding contract contains duplicate row IDs",
        ));
    }
    for row in &catalog.evidence {
        let source_key = target_by_id
            .get(row.target_row_id.as_str())
            .map(|target| target.source_key.as_str());
        match evidence_pins.get(row.row_id.as_str()).copied() {
            Some(pin)
                if row.target_row_id == pin.target_row_id
                    && source_key == Some(pin.target_source_key)
                    && row.evidence_id == pin.evidence_id
                    && row.phase == pin.phase
                    && row.status == pin.status
                    && row.owner_bead_id == pin.owner_bead_id
                    && row
                        .checker_ids
                        .iter()
                        .map(String::as_str)
                        .eq(pin.checker_ids.iter().copied())
                    && row
                        .scenario_ids
                        .iter()
                        .map(String::as_str)
                        .eq(pin.scenario_ids.iter().copied())
                    && row
                        .event_ids
                        .iter()
                        .map(String::as_str)
                        .eq(pin.event_ids.iter().copied())
                    && row
                        .gate_ids
                        .iter()
                        .map(String::as_str)
                        .eq(pin.gate_ids.iter().copied()) => {}
            Some(_) => out.push(Violation::new(
                "catalog_evidence_binding_contract_mismatch",
                &row.row_id,
                "evidence binding does not byte-match its readable target/source/owner/checker/scenario/event/gate contract",
            )),
            None => out.push(Violation::new(
                "catalog_evidence_binding_contract_unapproved",
                &row.row_id,
                "evidence binding has no independent readable per-target contract",
            )),
        }
    }
    let evidence_rows: BTreeSet<&str> = catalog
        .evidence
        .iter()
        .map(|row| row.row_id.as_str())
        .collect();
    for pin in evidence_contract {
        if !evidence_rows.contains(pin.row_id) {
            out.push(Violation::new(
                "catalog_evidence_binding_contract_missing",
                pin.row_id,
                "readable evidence binding contract has no reciprocal catalog row",
            ));
        }
    }
}

fn semantic_binding_contract_matches_with(
    contract: &[SemanticBindingContractPin],
    catalog: &Catalog,
    row: &SemanticBinding,
) -> bool {
    let Some(pin) = contract.iter().find(|pin| pin.row_id == row.row_id) else {
        return false;
    };
    let source_key = catalog
        .targets
        .iter()
        .find(|target| target.target_row_id == row.target_row_id)
        .map(|target| target.source_key.as_str());
    row.target_row_id == pin.target_row_id
        && source_key == Some(pin.target_source_key)
        && row.owner_bead_id == pin.owner_bead_id
        && row.owner_crate == pin.owner_crate
        && row.owner_status == pin.owner_status
        && row
            .consumer_crates
            .iter()
            .map(String::as_str)
            .eq(pin.consumer_crates.iter().copied())
}

fn annotation_contract_matches_with(
    contract: &[AnnotationContractPin],
    catalog: &Catalog,
    row: &Annotation,
) -> bool {
    let Some(pin) = contract.iter().find(|pin| pin.row_id == row.row_id) else {
        return false;
    };
    let source_key = catalog
        .targets
        .iter()
        .find(|target| target.target_row_id == row.target_row_id)
        .map(|target| target.source_key.as_str());
    row.target_row_id == pin.target_row_id
        && source_key == Some(pin.target_source_key)
        && row.exact_type == pin.exact_type
        && row.cardinality == pin.cardinality
        && row.layout == pin.layout
        && row.role == pin.role
        && row.posture == pin.posture
        && row.authority == pin.authority
        && row.locality == pin.locality
        && row
            .generic_expansions
            .iter()
            .map(String::as_str)
            .eq(pin.generic_expansions.iter().copied())
        && row
            .role_expansions
            .iter()
            .map(String::as_str)
            .eq(pin.role_expansions.iter().copied())
        && row.reference_semantics == pin.reference_semantics
        && row
            .target_schema_ids
            .iter()
            .map(String::as_str)
            .eq(pin.target_schema_ids.iter().copied())
        && row.construction_order == pin.construction_order
        && row.retention_and_cut_rule == pin.retention_and_cut_rule
        && row.digest_recipe == pin.digest_recipe
        && row.redaction_class == pin.redaction_class
        && row.resource_bounds == pin.resource_bounds
        && row.compatibility == pin.compatibility
}

fn expansion_binding_contract_matches_with(
    contract: &[ExpansionBindingContractPin],
    catalog: &Catalog,
    row: &ExpansionBinding,
) -> bool {
    let Some(pin) = contract.iter().find(|pin| pin.row_id == row.row_id) else {
        return false;
    };
    let source_key = catalog
        .targets
        .iter()
        .find(|target| target.target_row_id == row.target_row_id)
        .map(|target| target.source_key.as_str());
    row.target_row_id == pin.target_row_id
        && source_key == Some(pin.target_source_key)
        && row.parameter_ordinal == pin.parameter_ordinal
        && row.formal == pin.formal
        && row.formal_class == pin.formal_class
        && row
            .values
            .iter()
            .map(String::as_str)
            .eq(pin.values.iter().copied())
        && row.rationale == pin.rationale
}

fn ambiguity_adjudication_contract_matches_with(
    contract: &[AmbiguityAdjudicationContractPin],
    row: &AmbiguityAdjudication,
) -> bool {
    let Some(pin) = contract.iter().find(|pin| pin.row_id == row.row_id) else {
        return false;
    };
    row.slice_id == pin.slice_id
        && row.ambiguity_source_key == pin.ambiguity_source_key
        && row
            .source_locations
            .iter()
            .map(String::as_str)
            .eq(pin.source_locations.iter().copied())
        && row.resolution == pin.resolution
        && row
            .resolved_source_keys
            .iter()
            .map(String::as_str)
            .eq(pin.resolved_source_keys.iter().copied())
        && row.rationale == pin.rationale
}

fn approved_final_ambiguity_keys_with<'a>(
    contract: &[AmbiguityAdjudicationContractPin],
    catalog: &'a Catalog,
    slice_id: &str,
) -> Vec<&'a str> {
    catalog
        .ambiguity_adjudications
        .iter()
        .filter(|row| {
            row.slice_id == slice_id
                && matches!(
                    row.resolution.as_str(),
                    "maps-to-source" | "not-a-durable-schema"
                )
                && ambiguity_adjudication_contract_matches_with(contract, row)
        })
        .map(|row| row.ambiguity_source_key.as_str())
        .collect()
}

fn validate_readable_expansion_contract(catalog: &Catalog, out: &mut Vec<Violation>) {
    if EXPANSION_BINDING_CONTRACT.len() != EXPECTED_EXPANSION_BINDING_COUNT {
        out.push(Violation::new(
            "catalog_expansion_binding_contract_pin_inconsistent",
            "expansion_binding",
            "readable expansion pins and released transcript count must be updated together",
        ));
    }
    let pins: BTreeMap<&str, &ExpansionBindingContractPin> = EXPANSION_BINDING_CONTRACT
        .iter()
        .map(|pin| (pin.row_id, pin))
        .collect();
    if pins.len() != EXPANSION_BINDING_CONTRACT.len() {
        out.push(Violation::new(
            "catalog_expansion_binding_contract_ambiguous",
            "expansion_binding",
            "readable expansion contract contains duplicate row IDs",
        ));
    }
    for row in &catalog.expansion_bindings {
        match pins.get(row.row_id.as_str()).copied() {
            Some(_) if expansion_binding_contract_matches_with(
                &EXPANSION_BINDING_CONTRACT,
                catalog,
                row,
            ) => {}
            Some(_) => out.push(Violation::new(
                "catalog_expansion_binding_contract_mismatch",
                &row.row_id,
                "expansion binding does not byte-match its readable target/source/ordinal/formal/value contract",
            )),
            None => out.push(Violation::new(
                "catalog_expansion_binding_contract_unapproved",
                &row.row_id,
                "expansion binding has no independent readable per-formal contract",
            )),
        }
    }
    let rows: BTreeSet<&str> = catalog
        .expansion_bindings
        .iter()
        .map(|row| row.row_id.as_str())
        .collect();
    for pin in &EXPANSION_BINDING_CONTRACT {
        if !rows.contains(pin.row_id) {
            out.push(Violation::new(
                "catalog_expansion_binding_contract_missing",
                pin.row_id,
                "readable expansion contract has no reciprocal catalog row",
            ));
        }
    }
}

fn validate_readable_ambiguity_contract(catalog: &Catalog, out: &mut Vec<Violation>) {
    if AMBIGUITY_ADJUDICATION_CONTRACT.len() != EXPECTED_AMBIGUITY_ADJUDICATION_COUNT {
        out.push(Violation::new(
            "catalog_ambiguity_adjudication_contract_pin_inconsistent",
            "ambiguity_adjudication",
            "readable ambiguity pins and released transcript count must be updated together",
        ));
    }
    let pins: BTreeMap<&str, &AmbiguityAdjudicationContractPin> = AMBIGUITY_ADJUDICATION_CONTRACT
        .iter()
        .map(|pin| (pin.row_id, pin))
        .collect();
    if pins.len() != AMBIGUITY_ADJUDICATION_CONTRACT.len() {
        out.push(Violation::new(
            "catalog_ambiguity_adjudication_contract_ambiguous",
            "ambiguity_adjudication",
            "readable ambiguity contract contains duplicate row IDs",
        ));
    }
    for row in &catalog.ambiguity_adjudications {
        match pins.get(row.row_id.as_str()).copied() {
            Some(_)
                if ambiguity_adjudication_contract_matches_with(
                    &AMBIGUITY_ADJUDICATION_CONTRACT,
                    row,
                ) => {}
            Some(_) => out.push(Violation::new(
                "catalog_ambiguity_adjudication_contract_mismatch",
                &row.row_id,
                "ambiguity adjudication does not byte-match its readable source/resolution contract",
            )),
            None => out.push(Violation::new(
                "catalog_ambiguity_adjudication_contract_unapproved",
                &row.row_id,
                "ambiguity adjudication has no independent readable source contract",
            )),
        }
    }
    let rows: BTreeSet<&str> = catalog
        .ambiguity_adjudications
        .iter()
        .map(|row| row.row_id.as_str())
        .collect();
    for pin in &AMBIGUITY_ADJUDICATION_CONTRACT {
        if !rows.contains(pin.row_id) {
            out.push(Violation::new(
                "catalog_ambiguity_adjudication_contract_missing",
                pin.row_id,
                "readable ambiguity contract has no reciprocal catalog row",
            ));
        }
    }
}

fn evidence_binding_contract_matches_with(
    contract: &[EvidenceBindingContractPin],
    catalog: &Catalog,
    row: &EvidenceBinding,
) -> bool {
    let Some(pin) = contract.iter().find(|pin| pin.row_id == row.row_id) else {
        return false;
    };
    let source_key = catalog
        .targets
        .iter()
        .find(|target| target.target_row_id == row.target_row_id)
        .map(|target| target.source_key.as_str());
    row.target_row_id == pin.target_row_id
        && source_key == Some(pin.target_source_key)
        && row.evidence_id == pin.evidence_id
        && row.phase == pin.phase
        && row.status == pin.status
        && row.owner_bead_id == pin.owner_bead_id
        && row
            .checker_ids
            .iter()
            .map(String::as_str)
            .eq(pin.checker_ids.iter().copied())
        && row
            .scenario_ids
            .iter()
            .map(String::as_str)
            .eq(pin.scenario_ids.iter().copied())
        && row
            .event_ids
            .iter()
            .map(String::as_str)
            .eq(pin.event_ids.iter().copied())
        && row
            .gate_ids
            .iter()
            .map(String::as_str)
            .eq(pin.gate_ids.iter().copied())
}

fn approved_annotation_counts(catalog: &Catalog) -> BTreeMap<String, usize> {
    approved_annotation_counts_with(catalog, &ANNOTATION_CONTRACT)
}

fn approved_annotation_counts_with(
    catalog: &Catalog,
    contract: &[AnnotationContractPin],
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for row in &catalog.annotations {
        if annotation_contract_matches_with(contract, catalog, row) {
            *counts.entry(row.target_row_id.clone()).or_default() += 1;
        }
    }
    counts
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ApprovedBindingCounts {
    semantic: BTreeMap<String, usize>,
    static_live: BTreeMap<String, usize>,
    runtime: BTreeMap<String, usize>,
}

fn approved_binding_counts(catalog: &Catalog) -> ApprovedBindingCounts {
    approved_binding_counts_with(
        catalog,
        &SEMANTIC_BINDING_CONTRACT,
        &EVIDENCE_BINDING_CONTRACT,
    )
}

fn approved_binding_counts_with(
    catalog: &Catalog,
    semantic_contract: &[SemanticBindingContractPin],
    evidence_contract: &[EvidenceBindingContractPin],
) -> ApprovedBindingCounts {
    let mut counts = ApprovedBindingCounts::default();
    for row in &catalog.semantic_bindings {
        if semantic_binding_contract_matches_with(semantic_contract, catalog, row) {
            *counts
                .semantic
                .entry(row.target_row_id.clone())
                .or_default() += 1;
        }
    }
    for row in &catalog.evidence {
        if !evidence_binding_contract_matches_with(evidence_contract, catalog, row) {
            continue;
        }
        if row.phase == "static"
            && row.status == "live"
            && row.gate_ids.iter().any(|gate| gate == "G0")
        {
            *counts
                .static_live
                .entry(row.target_row_id.clone())
                .or_default() += 1;
        }
        if row.phase == "runtime" {
            *counts.runtime.entry(row.target_row_id.clone()).or_default() += 1;
        }
    }
    counts
}

fn validate_runtime_live_owner_coupling(catalog: &Catalog, out: &mut Vec<Violation>) {
    for evidence in &catalog.evidence {
        if evidence.phase != "runtime"
            || evidence.status != "live"
            || !evidence_binding_contract_matches_with(
                &EVIDENCE_BINDING_CONTRACT,
                catalog,
                evidence,
            )
        {
            continue;
        }
        let owners: Vec<_> = catalog
            .semantic_bindings
            .iter()
            .filter(|binding| {
                binding.target_row_id == evidence.target_row_id
                    && semantic_binding_contract_matches_with(
                        &SEMANTIC_BINDING_CONTRACT,
                        catalog,
                        binding,
                    )
            })
            .collect();
        if owners.len() != 1 || owners[0].owner_status != "live" {
            out.push(Violation::new(
                "catalog_runtime_live_owner_mismatch",
                &evidence.row_id,
                "runtime live evidence requires exactly one approved live semantic implementation owner",
            ));
        }
    }
}

fn validate_source_manifest_pin(manifest: &SourceManifest, out: &mut Vec<Violation>) {
    pin_str(
        out,
        "source_manifest",
        "plan_path",
        PLAN_PATH,
        &manifest.plan_path,
    );
    pin_i64(
        out,
        "source_manifest",
        "start_line",
        APPENDIX_START_LINE,
        manifest.start_line,
    );
    pin_i64(
        out,
        "source_manifest",
        "end_line",
        APPENDIX_END_LINE,
        manifest.end_line,
    );
    pin_i64(
        out,
        "source_manifest",
        "line_count",
        APPENDIX_LINE_COUNT,
        manifest.line_count,
    );
    pin_i64(
        out,
        "source_manifest",
        "byte_count",
        APPENDIX_BYTE_COUNT,
        manifest.byte_count,
    );
    pin_str(
        out,
        "source_manifest",
        "sha256",
        APPENDIX_SHA256,
        &manifest.sha256,
    );
    pin_str(
        out,
        "source_manifest",
        "heading",
        APPENDIX_HEADING,
        &manifest.heading,
    );
    pin_str(
        out,
        "source_manifest",
        "next_heading",
        NEXT_HEADING,
        &manifest.next_heading,
    );
    let computed_lines = manifest
        .end_line
        .checked_sub(manifest.start_line)
        .and_then(|delta| delta.checked_add(1));
    if computed_lines != Some(manifest.line_count) {
        out.push(Violation::new(
            "source_manifest_range_mismatch",
            "source_manifest",
            "line_count does not equal the inclusive source range",
        ));
    }
    if manifest.byte_count <= 0 || !valid_sha256_hex(&manifest.sha256) {
        out.push(Violation::new(
            "source_manifest_pin_invalid",
            "source_manifest",
            "byte_count must be positive and sha256 must be 64 lowercase hex digits",
        ));
    }
}

fn validate_slice_pin(slice: &Slice, pin: &SlicePin, row_id: &str, out: &mut Vec<Violation>) {
    pin_i64(out, row_id, "ordinal", pin.ordinal, slice.ordinal);
    pin_str(out, row_id, "id", pin.id, &slice.id);
    pin_str(out, row_id, "bead_id", pin.bead_id, &slice.bead_id);
    pin_str(out, row_id, "title", pin.title, &slice.title);
    pin_i64(out, row_id, "start_line", pin.start_line, slice.start_line);
    pin_i64(out, row_id, "end_line", pin.end_line, slice.end_line);
    pin_i64(out, row_id, "line_count", pin.line_count, slice.line_count);
    pin_i64(out, row_id, "byte_count", pin.byte_count, slice.byte_count);
    pin_str(out, row_id, "sha256", pin.sha256, &slice.sha256);
}

fn validate_projection_classes(slice: &Slice, row_id: &str, out: &mut Vec<Violation>) {
    let mut seen = BTreeSet::new();
    if slice.expected_projection_classes.is_empty() {
        out.push(Violation::new(
            "slice_projection_invalid",
            row_id,
            "expected_projection_classes must not be empty",
        ));
    }
    for class in &slice.expected_projection_classes {
        if !PROJECTION_CLASSES.contains(&class.as_str()) {
            out.push(Violation::new(
                "slice_projection_invalid",
                row_id,
                format!("unknown projection class {class:?}"),
            ));
        }
        if !seen.insert(class.as_str()) {
            out.push(Violation::new(
                "slice_projection_invalid",
                row_id,
                format!("duplicate projection class {class:?}"),
            ));
        }
    }
}

fn validate_projection_catalog(catalog: &Catalog, out: &mut Vec<Violation>) {
    let expected_epochs = [
        ("logical_object_kinds", catalog.identity.logical_epoch),
        ("physical_record_kinds", catalog.identity.physical_epoch),
        ("bootstrap_frames", catalog.identity.bootstrap_epoch),
        (
            "prebootstrap_artifact_kinds",
            catalog.identity.prebootstrap_epoch,
        ),
        ("wire_types", catalog.identity.wire_epoch),
        ("durable_fields", catalog.identity.fields_epoch),
    ];
    for (registry, actual) in expected_epochs {
        if catalog.projection_epochs.get(registry).copied() != Some(actual) {
            out.push(Violation::new(
                "projection_epoch_mismatch",
                registry,
                format!(
                    "catalog epoch {:?} does not match parsed projection epoch {actual}",
                    catalog.projection_epochs.get(registry)
                ),
            ));
        }
    }

    let expected_row_count = catalog.identity.logical.len()
        + catalog.identity.physical.len()
        + catalog.identity.bootstrap.len()
        + catalog.identity.prebootstrap.len()
        + catalog.identity.wire.len()
        + catalog.identity.fields.len()
        + catalog.identity.ordinary_unions.len()
        + catalog
            .identity
            .ordinary_unions
            .iter()
            .map(|union| union.arms.len())
            .sum::<usize>()
        + catalog.identity.unions.len()
        + catalog
            .identity
            .unions
            .iter()
            .map(|union| union.arms.len())
            .sum::<usize>();
    if catalog.projection_rows.len() != expected_row_count {
        out.push(Violation::new(
            "projection_row_count",
            "projection_rows",
            format!(
                "expected {expected_row_count} typed row metadata records, found {}",
                catalog.projection_rows.len()
            ),
        ));
    }
    if catalog.projection_rows.len() != EXPECTED_PROJECTION_ROW_COUNT {
        out.push(Violation::new(
            "projection_row_count",
            "projection_rows",
            format!(
                "released catalog requires exactly {EXPECTED_PROJECTION_ROW_COUNT} projection rows, found {}",
                catalog.projection_rows.len()
            ),
        ));
    }

    let slice_map: BTreeMap<&str, &Slice> = catalog
        .slices
        .iter()
        .map(|slice| (slice.id.as_str(), slice))
        .collect();
    let mut row_ids = BTreeSet::new();
    for row in &catalog.projection_rows {
        validate_row_identity(&row.row_id, &row.slice_id, &row.row_kind, out);
        validate_projection_row_derived_identity(row, out);
        if !row_ids.insert(row.row_id.as_str()) {
            out.push(Violation::new(
                "catalog_row_duplicate",
                &row.row_id,
                "duplicate projection row_id",
            ));
        }
        if row.slice_id == "g0" {
            continue;
        }
        let Some(slice) = slice_map.get(row.slice_id.as_str()) else {
            out.push(Violation::new(
                "catalog_slice_unknown",
                &row.row_id,
                format!("unknown slice_id {:?}", row.slice_id),
            ));
            continue;
        };
        if !slice
            .expected_projection_classes
            .iter()
            .any(|class| class == &row.projection)
        {
            out.push(Violation::new(
                "catalog_projection_unexpected",
                &row.row_id,
                format!(
                    "slice {} does not declare projection {:?}",
                    slice.id, row.projection
                ),
            ));
        }
    }

    let mut released_row_ids: Vec<&str> = catalog
        .projection_rows
        .iter()
        .map(|row| row.row_id.as_str())
        .collect();
    released_row_ids.sort_unstable();
    let mut released_transcript = released_row_ids.join("\n");
    if !released_transcript.is_empty() {
        released_transcript.push('\n');
    }
    let released_sha256 = sha256_hex(released_transcript.as_bytes());
    if released_row_ids.len() != EXPECTED_PROJECTION_ROW_COUNT
        || released_sha256 != EXPECTED_PROJECTION_ROW_IDS_SHA256
    {
        out.push(Violation::new(
            "projection_owner_assignment_drift",
            "projection_rows",
            format!(
                "released row-id transcript must contain {EXPECTED_PROJECTION_ROW_COUNT} rows with sha256 {EXPECTED_PROJECTION_ROW_IDS_SHA256}; found {} rows with sha256 {released_sha256}",
                released_row_ids.len()
            ),
        ));
    }

    let mut g0_row_ids: Vec<&str> = catalog
        .projection_rows
        .iter()
        .filter(|row| row.slice_id == "g0")
        .map(|row| row.row_id.as_str())
        .collect();
    g0_row_ids.sort_unstable();
    let mut g0_transcript = g0_row_ids.join("\n");
    if !g0_transcript.is_empty() {
        g0_transcript.push('\n');
    }
    if g0_row_ids.len() != EXPECTED_G0_PROJECTION_ROW_COUNT
        || sha256_hex(g0_transcript.as_bytes()) != EXPECTED_G0_PROJECTION_ROW_IDS_SHA256
    {
        out.push(Violation::new(
            "g0_projection_allowlist_drift",
            "g0",
            format!(
                "expected {EXPECTED_G0_PROJECTION_ROW_COUNT} pinned g0 rows with sha256 {}, found {} rows with sha256 {}",
                EXPECTED_G0_PROJECTION_ROW_IDS_SHA256,
                g0_row_ids.len(),
                sha256_hex(g0_transcript.as_bytes())
            ),
        ));
    }

    for violation in identity::validate_identity(&catalog.identity) {
        out.push(Violation::new(
            &format!("projection_{}", violation.code),
            format!("{}::{}", violation.registry, violation.row_id),
            violation.msg,
        ));
    }
}

fn validate_projection_row_derived_identity(row: &ProjectionRowMeta, out: &mut Vec<Violation>) {
    let expected_row_id = format!("{}:{}:{}", row.slice_id, row.row_kind, row.canonical_suffix);
    if row.canonical_suffix.trim().is_empty()
        || row.canonical_symbol.trim().is_empty()
        || row.row_id != expected_row_id
    {
        out.push(Violation::new(
            "catalog_row_id_derived_mismatch",
            &row.row_id,
            format!(
                "projection row_id must derive from canonical typed suffix {:?} for symbol {:?}; expected {expected_row_id:?}",
                row.canonical_suffix, row.canonical_symbol
            ),
        ));
    }
}

fn validate_catalog_metadata(catalog: &Catalog, out: &mut Vec<Violation>) {
    let expected_keys = expected_structural_keys(catalog);
    let slice_map: BTreeMap<&str, &Slice> = catalog
        .slices
        .iter()
        .map(|slice| (slice.id.as_str(), slice))
        .collect();
    let known_slices: BTreeSet<&str> = slice_map.keys().copied().collect();
    let mut all_row_ids = BTreeSet::new();
    let mut projection_targets: BTreeMap<String, String> = BTreeMap::new();
    let mut projection_by_row_id: BTreeMap<&str, &ProjectionRowMeta> = BTreeMap::new();
    for row in &catalog.projection_rows {
        if !all_row_ids.insert(row.row_id.clone()) {
            out.push(Violation::new(
                "catalog_row_duplicate",
                &row.row_id,
                "duplicate primary projection row_id",
            ));
        }
        projection_targets.insert(row.row_id.clone(), row.row_kind.clone());
        projection_by_row_id.insert(row.row_id.as_str(), row);
    }

    validate_maintenance_proof(&catalog.maintenance_proof, out);
    validate_binding_contract_pins(catalog, out);
    validate_reservations(catalog, &known_slices, &mut all_row_ids, out);
    let reservation_symbols: BTreeSet<&str> = catalog
        .reservations
        .iter()
        .map(|row| row.symbol.as_str())
        .collect();

    let mut projected_classes_by_symbol: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for projection in &catalog.projection_rows {
        if let Some(identity_class) = projection_identity_class(&projection.row_kind) {
            projected_classes_by_symbol
                .entry(projection.canonical_symbol.as_str())
                .or_default()
                .insert(identity_class);
        }
    }

    let mut candidate_by_key = BTreeMap::new();
    let mut candidate_keys_by_slice: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for row in &catalog.top_level_candidates {
        validate_row_identity(&row.row_id, &row.slice_id, "top-level-candidate", out);
        validate_slice_id(&row.row_id, &row.slice_id, &known_slices, out);
        insert_owned_row_id(&mut all_row_ids, &row.row_id, out);
        let expected_row_id = top_level_candidate_row_id(
            &row.slice_id,
            &row.symbol,
            &row.generic_signature,
            &row.source_key,
        );
        if row.row_id != expected_row_id {
            out.push(Violation::new(
                "catalog_row_id_derived_mismatch",
                &row.row_id,
                format!("top-level candidate row_id must be {expected_row_id:?}"),
            ));
        }
        if !matches!(
            row.source_kind.as_str(),
            "confirmed" | "ambiguous" | "name-only"
        ) {
            out.push(Violation::new(
                "catalog_candidate_kind_invalid",
                &row.row_id,
                "source_kind must be confirmed|ambiguous|name-only",
            ));
        }
        if !valid_source_candidate_symbol(&row.symbol)
            || !valid_generic_signature(&row.generic_signature)
        {
            out.push(Violation::new(
                "catalog_candidate_symbol_invalid",
                &row.row_id,
                "symbol must be one source candidate name and generic_signature must be empty or one balanced angle-bracket suffix",
            ));
        }
        if !matches!(
            row.identity_class.as_str(),
            "logical" | "physical" | "bootstrap" | "prebootstrap" | "wire" | "unclassified"
        ) {
            out.push(Violation::new(
                "catalog_candidate_class_invalid",
                &row.row_id,
                "identity_class must be one of the five durable classes or unclassified while declared",
            ));
        }
        match projected_classes_by_symbol.get(row.symbol.as_str()) {
            Some(classes) if classes.len() == 1 => {
                let expected = classes.iter().next().copied().unwrap_or("unclassified");
                if row.identity_class != expected {
                    out.push(Violation::new(
                        "catalog_candidate_class_mismatch",
                        &row.row_id,
                        format!(
                            "identity_class must match the checked-in {expected} projection for this symbol"
                        ),
                    ));
                }
            }
            Some(_) => out.push(Violation::new(
                "catalog_candidate_class_conflict",
                &row.row_id,
                "one top-level symbol is projected into more than one disjoint identity class",
            )),
            None if row.identity_class != "unclassified" => out.push(Violation::new(
                "catalog_candidate_class_unproved",
                &row.row_id,
                "an unprojected source candidate must remain unclassified",
            )),
            None => {}
        }
        // Source identity is deliberately independent of the catalog's
        // semantic classification.  Feeding `identity_class` into this key
        // would let a manual catalog decision rewrite the supposedly
        // source-derived census transcript.
        let expected_source_key = format!("top|{}{}", row.symbol, row.generic_signature);
        if row.source_key != expected_source_key {
            out.push(Violation::new(
                "catalog_candidate_source_key_invalid",
                &row.row_id,
                format!("source_key must be {expected_source_key:?}"),
            ));
        }
        validate_sorted_nonempty(&row.row_id, "source_locations", &row.source_locations, out);
        for location in &row.source_locations {
            validate_appendix_location(&row.row_id, location, &slice_map, out);
        }
        if candidate_by_key
            .insert(row.source_key.as_str(), row)
            .is_some()
        {
            out.push(Violation::new(
                "catalog_candidate_duplicate",
                &row.row_id,
                "duplicate top-level source_key",
            ));
        }
        candidate_keys_by_slice
            .entry(row.slice_id.as_str())
            .or_default()
            .push(row.source_key.as_str());
    }

    for slice in &catalog.slices {
        let keys = candidate_keys_by_slice
            .get(slice.id.as_str())
            .cloned()
            .unwrap_or_default();
        validate_census_pin(
            &slice.id,
            "top_level_candidate",
            slice.top_level_candidate_count,
            &slice.top_level_candidate_ids_sha256,
            keys,
            out,
        );
        for (kind, count, digest) in [
            (
                "field_candidate",
                slice.field_candidate_count,
                slice.field_candidate_ids_sha256.as_str(),
            ),
            (
                "union_candidate",
                slice.union_candidate_count,
                slice.union_candidate_ids_sha256.as_str(),
            ),
            (
                "arm_candidate",
                slice.arm_candidate_count,
                slice.arm_candidate_ids_sha256.as_str(),
            ),
            (
                "ambiguity",
                slice.ambiguity_count,
                slice.ambiguity_ids_sha256.as_str(),
            ),
        ] {
            if count < 0 || !valid_sha256_hex(digest) {
                out.push(Violation::new(
                    "slice_census_pin_invalid",
                    &slice.id,
                    format!(
                        "{kind} count must be nonnegative and digest must be lowercase SHA-256"
                    ),
                ));
            }
        }
    }

    let mut target_by_projection = BTreeMap::new();
    for row in &catalog.targets {
        validate_metadata_row_id(&row.row_id, "target", out);
        insert_owned_row_id(&mut all_row_ids, &row.row_id, out);
        validate_metadata_target(
            &row.row_id,
            &row.target_row_id,
            "target",
            &projection_targets,
            out,
        );
        let Some((target_scope, target_kind, _)) = split_catalog_row_id(&row.target_row_id) else {
            continue;
        };
        if row.slice_id != target_scope || row.target_kind != target_kind {
            out.push(Violation::new(
                "catalog_target_identity_mismatch",
                &row.row_id,
                "slice_id and target_kind must byte-match the target projection row",
            ));
        }
        if !matches!(row.definition_status.as_str(), "declared" | "complete") {
            out.push(Violation::new(
                "catalog_definition_status_invalid",
                &row.row_id,
                "definition_status must be declared|complete",
            ));
        }
        let declared_reference_symbol = reference_source_symbol(&row.source_key)
            .filter(|symbol| reservation_symbols.contains(symbol));
        if row.slice_id != "g0"
            && !row.source_key.starts_with("field|")
            && !row.source_key.starts_with("union|")
            && !row.source_key.starts_with("arm|")
            && !row.source_key.starts_with("projection|")
            && !candidate_by_key.contains_key(row.source_key.as_str())
            && declared_reference_symbol.is_none()
        {
            out.push(Violation::new(
                "catalog_target_source_unresolved",
                &row.row_id,
                "target source_key is not a top-level, field, union, arm, or declared reference target",
            ));
        }
        if declared_reference_symbol.is_some() && row.definition_status != "declared" {
            out.push(Violation::new(
                "catalog_target_reference_incomplete",
                &row.row_id,
                "a reservation-only reference source cannot back a complete target",
            ));
        }
        if let Some(projection) = projection_by_row_id.get(row.target_row_id.as_str()) {
            let ordinary_union_wire_source = ordinary_union_wire_source_key(catalog, projection);
            validate_target_source_identity(
                row,
                projection,
                candidate_by_key.get(row.source_key.as_str()).copied(),
                ordinary_union_wire_source.as_deref(),
                &expected_keys,
                out,
            );
        }
        if let Some(candidate) = candidate_by_key.get(row.source_key.as_str()) {
            if row.definition_status == "complete" && candidate.slice_id != row.slice_id {
                out.push(Violation::new(
                    "catalog_target_source_owner_mismatch",
                    &row.row_id,
                    "complete top-level projection target must be owned by the candidate's canonical source slice",
                ));
            }
            if let Some(expected_class) = projection_identity_class(&row.target_kind)
                && candidate.identity_class != expected_class
            {
                out.push(Violation::new(
                    "catalog_target_class_mismatch",
                    &row.row_id,
                    format!(
                        "top-level source candidate class {:?} does not match target class {expected_class:?}",
                        candidate.identity_class
                    ),
                ));
            }
        }
        if target_by_projection
            .insert(row.target_row_id.as_str(), row)
            .is_some()
        {
            out.push(Violation::new(
                "catalog_target_duplicate",
                &row.row_id,
                "projection row has more than one target row",
            ));
        }
    }
    for projection in &catalog.projection_rows {
        if !target_by_projection.contains_key(projection.row_id.as_str()) {
            out.push(Violation::new(
                "catalog_projection_target_missing",
                &projection.row_id,
                "every checked-in projection row requires exactly one declared or complete target row",
            ));
        }
    }

    let mut schema_family_by_id: BTreeMap<&str, String> = BTreeMap::new();
    for reservation in &catalog.reservations {
        schema_family_by_id.insert(reservation.row_id.as_str(), reservation.symbol.clone());
    }
    let known_schema_ids: BTreeSet<&str> = schema_family_by_id.keys().copied().collect();
    let mut reference_alias_semantics = BTreeMap::new();
    for union in &catalog.identity.unions {
        let semantics: BTreeSet<&str> = union
            .arms
            .iter()
            .map(|arm| arm.reference_semantics.as_str())
            .collect();
        if let Some(semantics) = semantics.first().filter(|_| semantics.len() == 1) {
            reference_alias_semantics.insert(union.union_name.clone(), (*semantics).to_owned());
        }
    }
    let mut annotation_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &catalog.annotations {
        let top_level_definition_family = target_by_projection
            .get(row.target_row_id.as_str())
            .and_then(|target| candidate_by_key.get(target.source_key.as_str()))
            .map(|candidate| candidate.symbol.as_str());
        let mut generic_formals = annotation_generic_formals(
            row,
            &target_by_projection,
            &candidate_by_key,
            &catalog.top_level_candidates,
        );
        generic_formals.insert("T".to_owned());
        generic_formals.insert("Role".to_owned());
        validate_metadata_row_id(&row.row_id, "annotation", out);
        insert_owned_row_id(&mut all_row_ids, &row.row_id, out);
        validate_metadata_target(
            &row.row_id,
            &row.target_row_id,
            "annotation",
            &projection_targets,
            out,
        );
        *annotation_counts
            .entry(row.target_row_id.as_str())
            .or_default() += 1;
        if [
            &row.exact_type,
            &row.cardinality,
            &row.layout,
            &row.role,
            &row.posture,
            &row.authority,
            &row.locality,
            &row.reference_semantics,
            &row.construction_order,
            &row.retention_and_cut_rule,
            &row.digest_recipe,
            &row.redaction_class,
            &row.resource_bounds,
            &row.compatibility,
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            out.push(Violation::new(
                "catalog_metadata_blank",
                &row.row_id,
                "annotation scalar fields must be nonblank",
            ));
        }
        if annotation_scalar_values(row)
            .iter()
            .any(|value| contains_placeholder_marker(value))
            || contains_residual_formal(&row.exact_type, &generic_formals)
            || generic_formals.contains(row.role.trim())
            || row
                .generic_expansions
                .iter()
                .chain(&row.role_expansions)
                .chain(&row.target_schema_ids)
                .any(|value| {
                    contains_placeholder_marker(value)
                        || contains_residual_formal(value, &generic_formals)
                })
        {
            out.push(Violation::new(
                "catalog_annotation_placeholder",
                &row.row_id,
                "annotation assertions must not contain placeholders or residual generic formals",
            ));
        }
        validate_concrete_expansions(&row.row_id, &row.generic_expansions, out);
        validate_concrete_expansions(&row.row_id, &row.role_expansions, out);
        validate_concrete_expansions(&row.row_id, &row.target_schema_ids, out);
        if row
            .target_schema_ids
            .iter()
            .any(|schema_id| !known_schema_ids.contains(schema_id.as_str()))
        {
            out.push(Violation::new(
                "catalog_annotation_target_schema_unresolved",
                &row.row_id,
                "every target_schema_id must resolve to the one canonical permanent reservation row ID for that schema family",
            ));
        }
        let reference_families = validate_annotation_reference_shape(
            AnnotationReferenceRequest {
                row_id: &row.row_id,
                exact_type: &row.exact_type,
                reference_semantics: &row.reference_semantics,
                top_level_definition_family,
            },
            &reference_alias_semantics,
            &reservation_symbols,
            &generic_formals,
            out,
        );
        if top_level_definition_family.is_some_and(|family| row.exact_type.trim() == family)
            && !row.target_schema_ids.is_empty()
        {
            out.push(Violation::new(
                "catalog_annotation_reference_target_mismatch",
                &row.row_id,
                "a top-level schema definition cannot claim arbitrary reference targets",
            ));
        }
        validate_annotation_reference_targets(row, &reference_families, &schema_family_by_id, out);
        validate_annotation_identity_field_contract(
            row,
            &projection_by_row_id,
            &catalog.identity,
            &schema_family_by_id,
            out,
        );
        if row.exact_type.contains(['<', '>'])
            && row.generic_expansions.is_empty()
            && row.role_expansions.is_empty()
        {
            out.push(Violation::new(
                "catalog_expansion_missing",
                &row.row_id,
                "generic exact_type requires at least one concrete generic or role expansion",
            ));
        }
    }
    for (target, count) in &annotation_counts {
        if *count > 1 {
            out.push(Violation::new(
                "catalog_annotation_duplicate",
                *target,
                format!("primary target has {count} annotation rows; at most one is legal"),
            ));
        }
    }

    let approved_annotation_counts = approved_annotation_counts(catalog);
    let ApprovedBindingCounts {
        semantic: binding_counts,
        static_live: static_live_counts,
        runtime: runtime_counts,
    } = approved_binding_counts(catalog);
    validate_runtime_live_owner_coupling(catalog, out);
    let mut semantic_targets = BTreeSet::new();
    for row in &catalog.semantic_bindings {
        validate_metadata_row_id(&row.row_id, "semantic-binding", out);
        insert_owned_row_id(&mut all_row_ids, &row.row_id, out);
        validate_metadata_target(
            &row.row_id,
            &row.target_row_id,
            "semantic-binding",
            &projection_targets,
            out,
        );
        validate_semantic_binding(row, &slice_map, out);
        if !semantic_targets.insert(row.target_row_id.as_str()) {
            out.push(Violation::new(
                "catalog_semantic_binding_duplicate",
                &row.row_id,
                "target has more than one semantic binding",
            ));
        }
    }
    validate_expansion_binding_rows(
        catalog,
        &projection_targets,
        &candidate_by_key,
        &mut all_row_ids,
        out,
    );

    let mut evidence_keys = BTreeSet::new();
    for row in &catalog.evidence {
        validate_metadata_row_id(&row.row_id, "evidence", out);
        insert_owned_row_id(&mut all_row_ids, &row.row_id, out);
        validate_metadata_target(
            &row.row_id,
            &row.target_row_id,
            "",
            &projection_targets,
            out,
        );
        validate_evidence(row, out);
        if !evidence_keys.insert((row.target_row_id.as_str(), row.evidence_id.as_str())) {
            out.push(Violation::new(
                "catalog_evidence_duplicate",
                &row.row_id,
                "duplicate target/evidence_id pair",
            ));
        }
    }

    validate_source_dispositions(catalog, &slice_map, &known_slices, &mut all_row_ids, out);
    validate_ambiguity_adjudication_rows(catalog, &slice_map, &known_slices, &mut all_row_ids, out);

    let top_level_source_coverage = approved_top_level_source_coverage(catalog);
    for slice in catalog
        .slices
        .iter()
        .filter(|slice| slice.definition_status == "complete")
    {
        let slice_targets: Vec<_> = catalog
            .targets
            .iter()
            .filter(|row| row.slice_id == slice.id)
            .collect();
        let mut closure_targets: BTreeMap<&str, &Target> = slice_targets
            .iter()
            .map(|target| (target.target_row_id.as_str(), *target))
            .collect();
        let (mut top_keys, top_level_closure_targets) =
            top_level_coverage_for_slice(catalog, &top_level_source_coverage, &slice.id);
        closure_targets.extend(top_level_closure_targets);
        if closure_targets.is_empty() {
            out.push(Violation::new(
                "complete_slice_target_missing",
                &slice.id,
                "complete slice has no source-backed targets",
            ));
        }
        // The complete-slice field, union, and arm census laws are enforced
        // against the raw source census by
        // `verify_complete_field_census_coverage` (fgdb-z35a, generalized for
        // fgdb-a01): arm-payload and wire-interior census keys are covered by
        // their arm/wire contracts, which a catalog-only sha-equality pin
        // cannot express.
        for source_key in catalog
            .ambiguity_adjudications
            .iter()
            .filter(|row| {
                row.slice_id == slice.id
                    && row.resolution == "not-a-durable-schema"
                    && ambiguity_adjudication_contract_matches_with(
                        &AMBIGUITY_ADJUDICATION_CONTRACT,
                        row,
                    )
            })
            .flat_map(|row| row.resolved_source_keys.iter().map(String::as_str))
        {
            if source_key.starts_with("top|") {
                top_keys.push(source_key);
            }
        }
        validate_census_pin(
            &slice.id,
            "complete_top_level",
            slice.top_level_candidate_count,
            &slice.top_level_candidate_ids_sha256,
            top_keys,
            out,
        );
        let ambiguity_keys = approved_final_ambiguity_keys_with(
            &AMBIGUITY_ADJUDICATION_CONTRACT,
            catalog,
            &slice.id,
        );
        validate_census_pin(
            &slice.id,
            "complete_ambiguity_adjudication",
            slice.ambiguity_count,
            &slice.ambiguity_ids_sha256,
            ambiguity_keys,
            out,
        );
        for row in closure_targets.into_values() {
            if row.definition_status != "complete" {
                out.push(Violation::new(
                    "complete_slice_target_declared",
                    &row.row_id,
                    "complete slice contains a target that is still declared",
                ));
            }
            let ordinary_union_wire_source_supported = projection_by_row_id
                .get(row.target_row_id.as_str())
                .and_then(|projection| ordinary_union_wire_source_key(catalog, projection))
                .as_deref()
                == Some(row.source_key.as_str());
            let source_contract_supported = row.source_key.starts_with("top|")
                || row.source_key.starts_with("field|")
                || (row.target_kind == "union" && row.source_key.starts_with("union|"))
                || (row.target_kind == "union-arm" && row.source_key.starts_with("arm|"))
                || ordinary_union_wire_source_supported
                || expected_keys.generated_reference_union_supported(row);
            if !source_contract_supported {
                out.push(Violation::new(
                    "complete_slice_source_contract_unverified",
                    &row.target_row_id,
                    "complete target requires a source-reconciled top-level, field, union, or arm contract; reference-only and projection fallback targets remain declared",
                ));
            }
            let annotation_count = approved_annotation_counts
                .get(row.target_row_id.as_str())
                .copied()
                .unwrap_or_default();
            if annotation_count != 1 {
                out.push(Violation::new(
                    "complete_slice_annotation_missing",
                    &row.target_row_id,
                    format!(
                        "complete projection target requires exactly one annotation, found {annotation_count}"
                    ),
                ));
            }
            let binding_count = binding_counts
                .get(row.target_row_id.as_str())
                .copied()
                .unwrap_or_default();
            if binding_count != 1 {
                out.push(Violation::new(
                    "complete_slice_semantic_binding_missing",
                    &row.target_row_id,
                    format!("complete target requires exactly one semantic binding, found {binding_count}"),
                ));
            }
            let static_count = static_live_counts
                .get(row.target_row_id.as_str())
                .copied()
                .unwrap_or_default();
            if static_count == 0 {
                out.push(Violation::new(
                    "complete_slice_static_evidence_missing",
                    &row.target_row_id,
                    "complete target requires static live evidence covering G0",
                ));
            }
            let runtime_count = runtime_counts
                .get(row.target_row_id.as_str())
                .copied()
                .unwrap_or_default();
            if runtime_count == 0 {
                out.push(Violation::new(
                    "complete_slice_runtime_evidence_missing",
                    &row.target_row_id,
                    "complete target requires explicit runtime planned or live evidence",
                ));
            }
        }
    }
}

fn validate_reservations(
    catalog: &Catalog,
    known_slices: &BTreeSet<&str>,
    all_row_ids: &mut BTreeSet<String>,
    out: &mut Vec<Violation>,
) {
    if catalog.reservations.len() != EXPECTED_TYPE_RESERVATION_COUNT {
        out.push(Violation::new(
            "catalog_reservation_count",
            "reservation",
            format!(
                "expected exactly {EXPECTED_TYPE_RESERVATION_COUNT} type reservations, found {}",
                catalog.reservations.len()
            ),
        ));
    }

    let logical_by_name: BTreeMap<&str, i64> = catalog
        .identity
        .logical
        .iter()
        .map(|row| (row.name.as_str(), row.object_kind))
        .collect();
    let logical_by_code: BTreeMap<i64, &str> = catalog
        .identity
        .logical
        .iter()
        .map(|row| (row.object_kind, row.name.as_str()))
        .collect();
    let mut symbols = BTreeSet::new();
    let mut codes = BTreeMap::new();
    let mut existing_count = 0usize;
    let mut reserved_count = 0usize;
    let mut reserved_high_water = None;

    for row in &catalog.reservations {
        validate_row_identity(&row.row_id, &row.slice_id, "reservation", out);
        validate_slice_id(&row.row_id, &row.slice_id, known_slices, out);
        insert_owned_row_id(all_row_ids, &row.row_id, out);

        let expected_row_id = format!("{}:reservation:{}", row.slice_id, lower_kebab(&row.symbol));
        if row.row_id != expected_row_id {
            out.push(Violation::new(
                "catalog_row_id_derived_mismatch",
                &row.row_id,
                format!("reservation row_id must be {expected_row_id:?}"),
            ));
        }
        if !valid_type_family(&row.symbol) {
            out.push(Violation::new(
                "catalog_reservation_symbol_invalid",
                &row.row_id,
                format!("symbol {:?} is not one concrete type family", row.symbol),
            ));
        }
        if !symbols.insert(row.symbol.as_str()) {
            out.push(Violation::new(
                "catalog_reservation_duplicate",
                &row.row_id,
                format!("duplicate reservation symbol {:?}", row.symbol),
            ));
        }
        if row.row_kind != "logical-kind" || row.identity_class != "logical" {
            out.push(Violation::new(
                "catalog_reservation_class_invalid",
                &row.row_id,
                "StrongRef target reservations must use row_kind=logical-kind and identity_class=logical",
            ));
        }

        let Some(code) = parse_code_reservation(&row.code_reservation) else {
            out.push(Violation::new(
                "catalog_reservation_code_invalid",
                &row.row_id,
                "code_reservation must be exact lowercase 0x0001..0xbfff",
            ));
            continue;
        };
        if let Some(previous) = codes.insert(code, row.row_id.as_str()) {
            out.push(Violation::new(
                "catalog_reservation_code_duplicate",
                &row.row_id,
                format!("code {code:#06x} is already reserved by {previous:?}"),
            ));
        }

        match logical_by_name.get(row.symbol.as_str()).copied() {
            Some(existing_code) => {
                existing_count += 1;
                if row.disposition != "existing" || i64::from(code) != existing_code {
                    out.push(Violation::new(
                        "catalog_reservation_existing_mismatch",
                        &row.row_id,
                        format!(
                            "existing logical symbol {:?} must reuse {existing_code:#06x} with disposition=existing",
                            row.symbol
                        ),
                    ));
                }
            }
            None => {
                reserved_count += 1;
                reserved_high_water =
                    Some(reserved_high_water.map_or(code, |prior: u16| prior.max(code)));
                if row.disposition != "reserved" {
                    out.push(Violation::new(
                        "catalog_reservation_disposition_invalid",
                        &row.row_id,
                        "unprojected type family must use disposition=reserved",
                    ));
                }
                if let Some(existing_name) = logical_by_code.get(&i64::from(code)) {
                    out.push(Violation::new(
                        "catalog_reservation_code_collision",
                        &row.row_id,
                        format!(
                            "reserved code {code:#06x} collides with projected logical symbol {existing_name:?}"
                        ),
                    ));
                }
            }
        }
    }

    if existing_count != EXPECTED_EXISTING_TYPE_RESERVATION_COUNT
        || reserved_count != EXPECTED_RESERVED_TYPE_RESERVATION_COUNT
        || reserved_high_water != Some(EXPECTED_RESERVATION_HIGH_WATER)
    {
        out.push(Violation::new(
            "catalog_reservation_epoch_drift",
            "reservation",
            format!(
                "epoch-1 reservation partition/high-water must be {EXPECTED_EXISTING_TYPE_RESERVATION_COUNT} existing, {EXPECTED_RESERVED_TYPE_RESERVATION_COUNT} reserved, 0x{EXPECTED_RESERVATION_HIGH_WATER:04x}; found {existing_count}, {reserved_count}, {reserved_high_water:?}"
            ),
        ));
    }

    let assignment_sha256 = reservation_assignment_sha256(&catalog.reservations);
    if assignment_sha256 != EXPECTED_RESERVATION_ASSIGNMENT_SHA256 {
        out.push(Violation::new(
            "catalog_reservation_assignment_drift",
            "reservation",
            format!(
                "released reservation assignment transcript must have sha256 {EXPECTED_RESERVATION_ASSIGNMENT_SHA256}, found {assignment_sha256}"
            ),
        ));
    }
}

fn validate_metadata_target(
    row_id: &str,
    target_row_id: &str,
    metadata_kind: &str,
    primary_targets: &BTreeMap<String, String>,
    out: &mut Vec<Violation>,
) {
    if row_id == target_row_id {
        out.push(Violation::new(
            "catalog_target_self_reference",
            row_id,
            "metadata rows cannot target themselves",
        ));
    }
    if !primary_targets.contains_key(target_row_id) {
        out.push(Violation::new(
            "catalog_target_unresolved",
            row_id,
            format!("target_row_id {target_row_id:?} is not a primary projection row"),
        ));
        return;
    }
    if !metadata_kind.is_empty()
        && let Some(expected) = derived_metadata_row_id(metadata_kind, target_row_id)
        && row_id != expected
    {
        out.push(Violation::new(
            "catalog_row_id_derived_mismatch",
            row_id,
            format!("metadata row_id must be {expected:?}"),
        ));
    }
}

fn ordinary_union_wire_source_key(
    catalog: &Catalog,
    projection: &ProjectionRowMeta,
) -> Option<String> {
    if projection.row_kind != "wire-type" {
        return None;
    }
    let mut wire_rows = catalog
        .identity
        .wire
        .iter()
        .filter(|wire| wire.name == projection.canonical_symbol);
    let wire = wire_rows.next()?;
    if wire_rows.next().is_some() {
        return None;
    }
    let containing_union = match wire.kind.as_str() {
        "union" | "discriminant" => wire.name.as_str(),
        "union_variant" => wire.containing_union.as_deref()?,
        _ => return None,
    };
    let mut unions = catalog.identity.ordinary_unions.iter().filter(|union| {
        identity::ordinary_union_has_top_level_shape(union) && union.union_name == containing_union
    });
    let union = unions.next()?;
    if unions.next().is_some() {
        return None;
    }
    if matches!(wire.kind.as_str(), "union" | "discriminant") {
        return Some(format!("top|{}", union.union_name));
    }
    let wire_tag = wire.wire_tag?;
    let mut arms = union.arms.iter().filter(|arm| {
        arm.arm_tag == wire_tag && wire.name == format!("{}.{}", union.union_name, arm.stable_name)
    });
    let arm = arms.next()?;
    if arms.next().is_some() {
        return None;
    }
    Some(format!(
        "arm|{}|{}|{}",
        union.containing_schema, union.union_path, arm.source_arm_name
    ))
}

/// Expected structural source keys, rebuilt from the typed catalog identity
/// rows and indexed by the projection symbol those same components derive.
///
/// This exists so `validate_target_source_identity` never has to parse a
/// `source_key`: the key grammar separates components with `|`, which is also
/// legal inside a generic signature, so parsing is ambiguous for owners like
/// `TimeBoundSubjectInventory<Role:Local|Meta|Shard>` (fgdb-tfow).  Unions and
/// arms reconstruct exactly; fields carry no `path` column, so only their owner
/// and stable name are recoverable and the field arm anchors on those.
#[derive(Default)]
struct ExpectedStructuralKeys {
    field_owner_and_name: BTreeMap<String, (String, String)>,
    union_expected: BTreeMap<String, String>,
    arm_expected: BTreeMap<String, String>,
    /// Canonical durable-fields projection keys for the per-anchor reference
    /// unions and arms this catalog's own identity rows GENERATE.
    ///
    /// A reference union has no source census key and never can have one.  The
    /// plan specifies the RULE, not the instance: a01:1402 names
    /// `RemoteGrantTargetRef` as "the containing-schema-generated closed union
    /// with one typed strong-reference arm per exportable authority-local
    /// target kind", and the generator mints one such name per anchor.  This
    /// catalog has already ADJUDICATED that doctrine — the a01
    /// `definition-without-structural-body` row for `RemoteGrantTargetRef`
    /// resolves `not-a-durable-schema` because the construct is
    /// "generator-owned with no structural body in this rendering".  That
    /// adjudication could re-home its subject onto `top|RemoteGrantTargetRef`
    /// only because the plan NAMES it; the per-anchor unions below are never
    /// named in any spelling, so no `top|`, `union|` or `arm|` key can exist
    /// for them and the generator rule is the only contract there is.
    ///
    /// So it is checked the way every other structural key is: by
    /// RECONSTRUCTION from the typed `[[reference_union]]` /
    /// `[[reference_union_arm]]` rows, never by parsing the key.  That is
    /// strictly stronger than admitting the projection fallback, which asserts
    /// nothing at all about the name.
    generated_reference_union: BTreeSet<String>,
    generated_reference_union_arm: BTreeSet<String>,
}

impl ExpectedStructuralKeys {
    /// True when `row` is a generated reference union or arm whose `source_key`
    /// byte-matches the key its own identity row derives.  A drifted or
    /// hand-edited name is rejected exactly as an ordinary union's would be.
    fn generated_reference_union_supported(&self, row: &Target) -> bool {
        match row.target_kind.as_str() {
            "reference-union" => self.generated_reference_union.contains(&row.source_key),
            "reference-union-arm" => self.generated_reference_union_arm.contains(&row.source_key),
            _ => false,
        }
    }
}

fn expected_structural_keys(catalog: &Catalog) -> ExpectedStructuralKeys {
    let mut keys = ExpectedStructuralKeys::default();
    for field in &catalog.identity.fields {
        keys.field_owner_and_name.insert(
            format!("{}.{}", field.containing_schema, field.stable_name),
            (field.containing_schema.clone(), field.stable_name.clone()),
        );
    }
    for union in &catalog.identity.ordinary_unions {
        keys.union_expected.insert(
            format!("{}.{}", union.containing_schema, union.union_path),
            format!("union|{}|{}", union.containing_schema, union.union_path),
        );
        for arm in &union.arms {
            keys.arm_expected.insert(
                format!(
                    "{}.{}.{}",
                    arm.containing_schema, arm.union_path, arm.source_arm_name
                ),
                format!(
                    "arm|{}|{}|{}",
                    arm.containing_schema, arm.union_path, arm.source_arm_name
                ),
            );
        }
    }
    for union in &catalog.identity.unions {
        keys.generated_reference_union.insert(format!(
            "projection|durable_fields|{}.{}",
            union.containing_schema, union.union_name
        ));
        for arm in &union.arms {
            keys.generated_reference_union_arm.insert(format!(
                "projection|durable_fields|{}.{}",
                arm.union_name, arm.stable_name
            ));
        }
    }
    keys
}

fn validate_target_source_identity(
    row: &Target,
    projection: &ProjectionRowMeta,
    top_candidate: Option<&TopLevelCandidate>,
    ordinary_union_wire_source: Option<&str>,
    keys: &ExpectedStructuralKeys,
    out: &mut Vec<Violation>,
) {
    let projection_source_key = format!(
        "projection|{}|{}",
        projection.projection, projection.canonical_symbol
    );
    if let Some(expected_source) = ordinary_union_wire_source {
        if row.source_key != expected_source {
            out.push(Violation::new(
                "catalog_target_source_identity_mismatch",
                &row.row_id,
                format!(
                    "ordinary-union wire row must map to exact union or arm source {expected_source:?}"
                ),
            ));
        }
        return;
    }
    if row.source_key == projection_source_key {
        if matches!(projection.row_kind.as_str(), "union" | "union-arm") {
            out.push(Violation::new(
                "catalog_target_source_identity_mismatch",
                &row.row_id,
                "ordinary union and arm projections require their exact structural source; projection fallback is forbidden",
            ));
            return;
        }
        // A generated reference union is not using the fallback as a
        // placeholder for a source key it has not found yet: the key it carries
        // is the one its own identity row derives, and no other key exists for
        // it in any spelling.  It is therefore a completable contract, unlike
        // every other projection-only source, which is still waiting on a
        // census key that could arrive.
        //
        // g0 is excluded deliberately.  It owns no `[[slice]]` row, so every
        // law that gives `complete` its meaning — annotation, semantic binding,
        // static and runtime evidence — iterates past it and can never fire.
        // This law is the only one still standing between a g0 target and an
        // unverifiable completion claim, so g0's generated unions keep the
        // `declared` requirement they have always had.  They are structurally
        // exempt from the completion battery and need nothing from this escape.
        if row.slice_id != "g0" && keys.generated_reference_union_supported(row) {
            return;
        }
        if row.definition_status != "declared" {
            out.push(Violation::new(
                "catalog_target_projection_incomplete",
                &row.row_id,
                "a projection-only source cannot back a complete target",
            ));
        }
        return;
    }
    if row.slice_id == "g0" {
        out.push(Violation::new(
            "catalog_target_source_identity_mismatch",
            &row.row_id,
            format!("g0 source_key must be {projection_source_key:?}"),
        ));
        return;
    }

    if let Some(candidate) = top_candidate {
        if candidate.symbol != projection.canonical_symbol {
            out.push(Violation::new(
                "catalog_target_source_identity_mismatch",
                &row.row_id,
                "top-level projection symbol does not match its source candidate",
            ));
        }
        return;
    }

    if let Some(symbol) = reference_source_symbol(&row.source_key) {
        if projection_identity_class(&projection.row_kind).is_none()
            || symbol != projection.canonical_symbol
        {
            out.push(Violation::new(
                "catalog_target_source_identity_mismatch",
                &row.row_id,
                "reservation-only source must name the same top-level projection symbol",
            ));
        }
        return;
    }

    match projection.row_kind.as_str() {
        "logical-kind" | "physical-kind" | "bootstrap-frame" | "prebootstrap-kind"
        | "wire-type" => out.push(Violation::new(
            "catalog_target_source_identity_mismatch",
            &row.row_id,
            "top-level projection must map to a matching top-level candidate or reservation-only reference",
        )),
        // Structural keys are matched by RECONSTRUCTION from the typed catalog
        // row, never by parsing the key.  `|` is the key separator and is also
        // legal inside a generic signature (`TimeBoundSubjectInventory<Role:
        // Local|Meta|Shard>`), so a `split('|')` with a fixed part count
        // mis-segments such an owner and rejects a byte-exact row (fgdb-tfow).
        // Rebuilding the key from components and comparing bytes is strictly
        // stronger than the old parse and has no grammar dependency.
        "field" => {
            // A field's census `path` is not a catalog column, so the key is
            // anchored on the two components that are: the owner prefix and the
            // stable-name suffix.  The interior `path` segment is validated
            // independently by the source pass, which requires the whole key to
            // exist in the frozen census.
            let source_matches = keys
                .field_owner_and_name
                .get(projection.canonical_symbol.as_str())
                .is_some_and(|(schema, stable_name)| {
                    let prefix = format!("field|{schema}|");
                    let suffix = format!("|{stable_name}");
                    row.source_key.starts_with(&prefix)
                        && row.source_key.ends_with(&suffix)
                        // The interior path must be present and must itself be
                        // rooted at the owning schema, the census invariant for
                        // every field path.
                        && row.source_key.len() > prefix.len() + suffix.len()
                        && row.source_key[prefix.len()..row.source_key.len() - suffix.len()]
                            .starts_with(schema.as_str())
                });
            if !source_matches {
                out.push(Violation::new(
                    "catalog_target_source_identity_mismatch",
                    &row.row_id,
                    "durable-field projection must map to the same source schema and stable field name",
                ));
            }
        }
        "union" => {
            let source_matches = keys
                .union_expected
                .get(projection.canonical_symbol.as_str())
                .is_some_and(|expected| &row.source_key == expected);
            if !source_matches {
                out.push(Violation::new(
                    "catalog_target_source_identity_mismatch",
                    &row.row_id,
                    "ordinary union projection must map to the exact source schema owner and union path",
                ));
            }
        }
        "union-arm" => {
            let source_matches = keys
                .arm_expected
                .get(projection.canonical_symbol.as_str())
                .is_some_and(|expected| &row.source_key == expected);
            if !source_matches {
                out.push(Violation::new(
                    "catalog_target_source_identity_mismatch",
                    &row.row_id,
                    "ordinary union-arm projection must map to the exact source parent and arm token",
                ));
            }
        }
        "reference-union" if !row.source_key.starts_with("union|") => {
            out.push(Violation::new(
                "catalog_target_source_identity_mismatch",
                &row.row_id,
                "reference-union projection must map to a union source candidate",
            ));
        }
        "reference-union-arm" if !row.source_key.starts_with("arm|") => {
            out.push(Violation::new(
                "catalog_target_source_identity_mismatch",
                &row.row_id,
                "reference-union-arm projection must map to an arm source candidate",
            ));
        }
        _ => {}
    }
}

fn reference_source_symbol(source_key: &str) -> Option<&str> {
    let mut parts = source_key.split('|');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("reference"), Some(symbol), None) if valid_type_family(symbol) => Some(symbol),
        _ => None,
    }
}

fn validate_maintenance_proof(row: &MaintenanceProof, out: &mut Vec<Violation>) {
    const ARTIFACTS: [&str; 7] = [
        "registries/appendix_a_catalog.toml",
        "registries/bootstrap_frames.toml",
        "registries/durable_fields.toml",
        "registries/logical_object_kinds.toml",
        "registries/physical_record_kinds.toml",
        "registries/prebootstrap_artifact_kinds.toml",
        "registries/wire_types.toml",
    ];
    const CHECKERS: [&str; 3] = [
        "appendix_a_catalog_closure",
        "appendix_a_catalog_projection_diff",
        "appendix_a_catalog_source",
    ];
    const EVENTS: [&str; 5] = [
        "appendix_closure_checked",
        "appendix_projection_checked",
        "appendix_projection_regenerated",
        "appendix_regeneration_completed",
        "appendix_source_manifest",
    ];
    if row.row_id != MAINTENANCE_PROOF_ROW_ID
        || row.owner_bead_id != MAINTENANCE_OWNER_BEAD
        || row.owner_crate != MAINTENANCE_OWNER_CRATE
        || row
            .covered_artifacts
            .iter()
            .map(String::as_str)
            .ne(ARTIFACTS)
        || row.checker_ids.iter().map(String::as_str).ne(CHECKERS)
        || !exact_single(&row.scenario_ids, "g0_identity_e2e")
        || row.event_ids.iter().map(String::as_str).ne(EVENTS)
        || !exact_single(&row.gate_ids, "G0")
        || row.evidence_status != "live"
    {
        out.push(Violation::new(
            "catalog_maintenance_proof_mismatch",
            &row.row_id,
            "maintenance proof must exactly bind the scaffold owner, seven checked-in artifacts, three live checkers, G0 scenario/events including regeneration, and G0",
        ));
    }
}

fn validate_semantic_binding(
    row: &SemanticBinding,
    slice_map: &BTreeMap<&str, &Slice>,
    out: &mut Vec<Violation>,
) {
    let forbidden_slice_owner = slice_map
        .values()
        .any(|slice| slice.bead_id == row.owner_bead_id);
    if row.owner_bead_id.trim().is_empty()
        || !row.owner_bead_id.starts_with("fgdb-")
        || row.owner_bead_id == MAINTENANCE_OWNER_BEAD
        || forbidden_slice_owner
        || row.owner_crate.trim().is_empty()
        || !(row.owner_crate == "fgdb" || row.owner_crate.starts_with("fgdb-"))
        || row.owner_crate == MAINTENANCE_OWNER_CRATE
        || row.owner_crate == "appendix-a-catalog"
        || !matches!(row.owner_status.as_str(), "planned" | "live")
    {
        out.push(Violation::new(
            "catalog_semantic_owner_invalid",
            &row.row_id,
            "semantic owner must be a non-maintenance implementation Bead and crate with owner_status planned|live",
        ));
    }
    validate_sorted_nonempty(&row.row_id, "consumer_crates", &row.consumer_crates, out);
    if row.consumer_crates.iter().any(|consumer| {
        !(consumer == "fgdb" || consumer.starts_with("fgdb-"))
            || consumer == "appendix-a-catalog"
            || consumer == MAINTENANCE_OWNER_CRATE
    }) {
        out.push(Violation::new(
            "catalog_semantic_consumer_invalid",
            &row.row_id,
            "catalog-maintenance components are not semantic consumer crates",
        ));
    }
}

fn validate_expansion_binding_rows(
    catalog: &Catalog,
    projection_targets: &BTreeMap<String, String>,
    candidate_by_key: &BTreeMap<&str, &TopLevelCandidate>,
    all_row_ids: &mut BTreeSet<String>,
    out: &mut Vec<Violation>,
) {
    let target_by_projection: BTreeMap<&str, &Target> = catalog
        .targets
        .iter()
        .map(|target| (target.target_row_id.as_str(), target))
        .collect();
    let mut target_ordinals = BTreeSet::new();
    for row in &catalog.expansion_bindings {
        validate_metadata_row_id(&row.row_id, "expansion-binding", out);
        insert_owned_row_id(all_row_ids, &row.row_id, out);
        validate_metadata_target(&row.row_id, &row.target_row_id, "", projection_targets, out);
        let expected = split_catalog_row_id(&row.target_row_id).map(|(scope, kind, suffix)| {
            format!(
                "{scope}:expansion-binding:{kind}-{suffix}-parameter-{}-{}",
                row.parameter_ordinal,
                lower_kebab(&row.formal)
            )
        });
        if expected.as_deref() != Some(row.row_id.as_str()) {
            out.push(Violation::new(
                "catalog_row_id_derived_mismatch",
                &row.row_id,
                format!(
                    "expansion binding row_id must be {:?}",
                    expected.unwrap_or_default()
                ),
            ));
        }
        if row.parameter_ordinal <= 0 {
            out.push(Violation::new(
                "catalog_expansion_parameter_ordinal_invalid",
                &row.row_id,
                "parameter_ordinal must be a positive 1-based source parameter position",
            ));
        }
        let source_candidate = target_by_projection
            .get(row.target_row_id.as_str())
            .and_then(|target| candidate_by_key.get(target.source_key.as_str()))
            .copied();
        let source_formals = source_candidate
            .map(|candidate| generic_formals_from_signature(&candidate.generic_signature))
            .unwrap_or_default();
        let dimensions = source_candidate.and_then(|candidate| {
            expansion_dimensions(
                &candidate.generic_signature,
                catalog
                    .top_level_candidates
                    .iter()
                    .filter(|peer| peer.symbol == candidate.symbol)
                    .map(|peer| peer.generic_signature.as_str()),
            )
        });
        let actual_values: BTreeSet<String> = row.values.iter().cloned().collect();
        let matching_dimensions = dimensions
            .as_ref()
            .map(|dimensions| {
                dimensions
                    .iter()
                    .filter(|dimension| {
                        dimension.parameter_ordinal == row.parameter_ordinal
                            && dimension
                                .explicit_formal
                                .as_deref()
                                .is_none_or(|formal| formal == row.formal)
                            && (dimension.source_values.is_empty()
                                || dimension.source_values == actual_values)
                    })
                    .count()
            })
            .unwrap_or_default();
        let expected_class = if row.formal == "Role" {
            "role"
        } else {
            "generic"
        };
        if !valid_generic_formal_token(&row.formal)
            || (row.parameter_ordinal > 0 && matching_dimensions != 1)
            || row.formal_class != expected_class
        {
            out.push(Violation::new(
                "catalog_expansion_formal_invalid",
                &row.row_id,
                "formal and parameter_ordinal must identify exactly one explicit or concrete-varying source parameter and use class role exactly for Role, generic otherwise",
            ));
        }
        validate_sorted_nonempty(&row.row_id, "values", &row.values, out);
        let mut residual_formals = source_formals;
        residual_formals.insert(row.formal.clone());
        if row.values.iter().any(|value| {
            contains_placeholder_marker(value)
                || contains_residual_formal(value, &residual_formals)
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) || row.rationale.trim().is_empty()
            || contains_placeholder_marker(&row.rationale)
        {
            out.push(Violation::new(
                "catalog_expansion_contract_invalid",
                &row.row_id,
                "expansion values must be concrete identifiers and rationale must be nonblank and final",
            ));
        }
        if row.parameter_ordinal > 0
            && !target_ordinals.insert((row.target_row_id.as_str(), row.parameter_ordinal))
        {
            out.push(Violation::new(
                "catalog_expansion_parameter_ordinal_duplicate",
                &row.row_id,
                "target has more than one expansion binding for the same parameter_ordinal",
            ));
        }
    }

    for target_row_id in catalog
        .expansion_bindings
        .iter()
        .map(|row| row.target_row_id.as_str())
        .collect::<BTreeSet<_>>()
    {
        let Some(candidate) = target_by_projection
            .get(target_row_id)
            .and_then(|target| candidate_by_key.get(target.source_key.as_str()))
            .copied()
        else {
            continue;
        };
        let Some(dimensions) = expansion_dimensions(
            &candidate.generic_signature,
            catalog
                .top_level_candidates
                .iter()
                .filter(|peer| peer.symbol == candidate.symbol)
                .map(|peer| peer.generic_signature.as_str()),
        ) else {
            out.push(Violation::new(
                "catalog_expansion_source_coverage_mismatch",
                target_row_id,
                "source-family generic signatures do not have one compatible parameter arity",
            ));
            continue;
        };
        let bindings: Vec<_> = catalog
            .expansion_bindings
            .iter()
            .filter(|row| row.target_row_id == target_row_id)
            .collect();
        if !expansion_bindings_match_dimensions(&bindings, &dimensions) {
            out.push(Violation::new(
                "catalog_expansion_source_coverage_mismatch",
                target_row_id,
                "expansion bindings must cover every explicit or concrete-varying source parameter ordinal exactly once",
            ));
        }
    }
}

fn validate_ambiguity_adjudication_rows(
    catalog: &Catalog,
    slice_map: &BTreeMap<&str, &Slice>,
    known_slices: &BTreeSet<&str>,
    all_row_ids: &mut BTreeSet<String>,
    out: &mut Vec<Violation>,
) {
    let mut source_keys = BTreeSet::new();
    for row in &catalog.ambiguity_adjudications {
        validate_metadata_row_id(&row.row_id, "ambiguity-adjudication", out);
        validate_slice_id(&row.row_id, &row.slice_id, known_slices, out);
        insert_owned_row_id(all_row_ids, &row.row_id, out);
        let digest = sha256_hex(row.ambiguity_source_key.as_bytes());
        let expected = format!("{}:ambiguity-adjudication:{digest}", row.slice_id);
        if row.row_id != expected {
            out.push(Violation::new(
                "catalog_row_id_derived_mismatch",
                &row.row_id,
                format!("ambiguity adjudication row_id must be {expected:?}"),
            ));
        }
        if !row.ambiguity_source_key.starts_with("ambiguity|")
            || row.rationale.trim().is_empty()
            || contains_placeholder_marker(&row.rationale)
            || !matches!(
                row.resolution.as_str(),
                "maps-to-source" | "not-a-durable-schema" | "needs-parser-fix" | "needs-source-fix"
            )
        {
            out.push(Violation::new(
                "catalog_ambiguity_adjudication_invalid",
                &row.row_id,
                "adjudication requires an ambiguity source key, final rationale, and a closed resolution",
            ));
        }
        validate_sorted_nonempty(&row.row_id, "source_locations", &row.source_locations, out);
        for location in &row.source_locations {
            validate_appendix_location(&row.row_id, location, slice_map, out);
        }
        if row.resolution == "maps-to-source" {
            validate_sorted_nonempty(
                &row.row_id,
                "resolved_source_keys",
                &row.resolved_source_keys,
                out,
            );
        } else if row.resolution == "not-a-durable-schema" {
            if !row.resolved_source_keys.is_empty() {
                validate_sorted_nonempty(
                    &row.row_id,
                    "resolved_source_keys",
                    &row.resolved_source_keys,
                    out,
                );
            }
        } else if !row.resolved_source_keys.is_empty() {
            out.push(Violation::new(
                "catalog_ambiguity_resolution_target_invalid",
                &row.row_id,
                "only final adjudications may name the exact structural source keys they accept or reject",
            ));
        }
        if !source_keys.insert(row.ambiguity_source_key.as_str()) {
            out.push(Violation::new(
                "catalog_ambiguity_adjudication_duplicate",
                &row.row_id,
                "ambiguity source key has more than one adjudication",
            ));
        }
    }
}

fn validate_evidence(row: &EvidenceBinding, out: &mut Vec<Violation>) {
    if !matches!(row.phase.as_str(), "static" | "runtime")
        || !matches!(row.status.as_str(), "planned" | "live")
        || row.evidence_id.trim().is_empty()
        || row.owner_bead_id.trim().is_empty()
        || !row.owner_bead_id.starts_with("fgdb-")
    {
        out.push(Violation::new(
            "catalog_evidence_contract_invalid",
            &row.row_id,
            "evidence requires a stable ID, owner Bead, phase static|runtime, and status planned|live",
        ));
    }
    for (name, values) in [
        ("checker_ids", &row.checker_ids),
        ("scenario_ids", &row.scenario_ids),
        ("event_ids", &row.event_ids),
        ("gate_ids", &row.gate_ids),
    ] {
        validate_sorted_nonempty(&row.row_id, name, values, out);
    }
    if row
        .gate_ids
        .iter()
        .any(|gate| !matches!(gate.as_str(), "G0" | "G1" | "G2" | "G3" | "G4"))
    {
        out.push(Violation::new(
            "catalog_evidence_gate_invalid",
            &row.row_id,
            "evidence gate IDs must be canonical G0 through G4",
        ));
    }
    if let Some((scope, target_kind, suffix)) = split_catalog_row_id(&row.target_row_id) {
        let expected = format!(
            "{scope}:evidence:{target_kind}-{suffix}-{}",
            lower_kebab(&row.evidence_id)
        );
        if row.row_id != expected {
            out.push(Violation::new(
                "catalog_row_id_derived_mismatch",
                &row.row_id,
                format!("evidence row_id must be {expected:?}"),
            ));
        }
    }
}

fn validate_sorted_nonempty(
    row_id: &str,
    field: &str,
    values: &[String],
    out: &mut Vec<Violation>,
) {
    if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
        out.push(Violation::new(
            "catalog_metadata_blank",
            row_id,
            format!("{field} must be nonempty and contain no blank item"),
        ));
    }
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        out.push(Violation::new(
            "catalog_metadata_order",
            row_id,
            format!("{field} must be strictly sorted and duplicate-free"),
        ));
    }
}

fn validate_census_pin(
    row_id: &str,
    kind: &str,
    expected_count: i64,
    expected_sha256: &str,
    mut keys: Vec<&str>,
    out: &mut Vec<Violation>,
) {
    keys.sort_unstable();
    if keys.windows(2).any(|pair| pair[0] == pair[1]) {
        out.push(Violation::new(
            "slice_census_duplicate",
            row_id,
            format!("{kind} source keys must be unique"),
        ));
    }
    let mut transcript = keys.join("\n");
    if !transcript.is_empty() {
        transcript.push('\n');
    }
    let actual_sha256 = sha256_hex(transcript.as_bytes());
    let actual_count = i64::try_from(keys.len()).unwrap_or(i64::MAX);
    if expected_count != actual_count || expected_sha256 != actual_sha256 {
        out.push(Violation::new(
            "slice_census_pin_mismatch",
            row_id,
            format!(
                "{kind} expected {expected_count} rows/{expected_sha256}, found {actual_count}/{actual_sha256}"
            ),
        ));
    }
}

fn validate_source_dispositions(
    catalog: &Catalog,
    slice_map: &BTreeMap<&str, &Slice>,
    known_slices: &BTreeSet<&str>,
    all_row_ids: &mut BTreeSet<String>,
    out: &mut Vec<Violation>,
) {
    let expected_total = catalog.reservations.len() + EXPECTED_G0_PROJECTION_ROW_COUNT;
    if catalog.source_symbol_dispositions.len() != expected_total {
        out.push(Violation::new(
            "catalog_source_disposition_count",
            "source_symbol_disposition",
            format!(
                "expected exactly {expected_total} source dispositions, found {}",
                catalog.source_symbol_dispositions.len()
            ),
        ));
    }

    let mut census_by_symbol: BTreeMap<&str, &SourceSymbolDisposition> = BTreeMap::new();
    let mut g0_by_row_id: BTreeMap<&str, &SourceSymbolDisposition> = BTreeMap::new();
    for row in &catalog.source_symbol_dispositions {
        validate_row_identity(&row.row_id, &row.slice_id, "source-symbol-disposition", out);
        validate_slice_id(&row.row_id, &row.slice_id, known_slices, out);
        insert_owned_row_id(all_row_ids, &row.row_id, out);
        if row.symbol.trim().is_empty() || row.source_locations.is_empty() {
            out.push(Violation::new(
                "catalog_metadata_blank",
                &row.row_id,
                "source disposition requires a symbol and at least one exact source location",
            ));
        }
        if row
            .source_locations
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            out.push(Violation::new(
                "catalog_source_location_order",
                &row.row_id,
                "source_locations must be strictly sorted and duplicate-free",
            ));
        }

        if row.slice_id == "g0" {
            if row.disposition != "projection-source" {
                out.push(Violation::new(
                    "catalog_disposition_invalid",
                    &row.row_id,
                    "g0 projection disposition must be projection-source",
                ));
            }
            if g0_by_row_id.insert(row.row_id.as_str(), row).is_some() {
                out.push(Violation::new(
                    "catalog_source_disposition_duplicate",
                    &row.row_id,
                    "duplicate g0 source disposition row_id",
                ));
            }
            continue;
        }

        let expected_row_id = format!(
            "{}:source-symbol-disposition:{}",
            row.slice_id,
            lower_kebab(&row.symbol)
        );
        if row.row_id != expected_row_id {
            out.push(Violation::new(
                "catalog_row_id_derived_mismatch",
                &row.row_id,
                format!("source census row_id must be {expected_row_id:?}"),
            ));
        }
        if !matches!(
            row.disposition.as_str(),
            "appendix-structural-definition"
                | "appendix-ambiguous-structure"
                | "appendix-name-only"
                | "reference-only"
        ) {
            out.push(Violation::new(
                "catalog_disposition_invalid",
                &row.row_id,
                "reference target requires one truthful Appendix structural/name/reference-only disposition",
            ));
        }
        if census_by_symbol.insert(row.symbol.as_str(), row).is_some() {
            out.push(Violation::new(
                "catalog_source_disposition_duplicate",
                &row.row_id,
                format!("duplicate type-census disposition for {:?}", row.symbol),
            ));
        }
        for location in &row.source_locations {
            validate_appendix_location(&row.row_id, location, slice_map, out);
        }
    }

    let reservation_by_symbol: BTreeMap<&str, &Reservation> = catalog
        .reservations
        .iter()
        .map(|row| (row.symbol.as_str(), row))
        .collect();
    for (symbol, reservation) in &reservation_by_symbol {
        match census_by_symbol.get(symbol).copied() {
            Some(disposition) if disposition.slice_id == reservation.slice_id => {}
            Some(disposition) => out.push(Violation::new(
                "catalog_reservation_owner_mismatch",
                &reservation.row_id,
                format!(
                    "reservation slice {:?} differs from source disposition slice {:?}",
                    reservation.slice_id, disposition.slice_id
                ),
            )),
            None => out.push(Violation::new(
                "catalog_reservation_disposition_missing",
                &reservation.row_id,
                format!("reservation symbol {symbol:?} has no source disposition"),
            )),
        }
    }
    for (symbol, disposition) in &census_by_symbol {
        if !reservation_by_symbol.contains_key(symbol) {
            out.push(Violation::new(
                "catalog_source_disposition_orphan",
                &disposition.row_id,
                format!("source disposition symbol {symbol:?} has no reservation"),
            ));
        }
    }

    if g0_by_row_id.len() != EXPECTED_G0_PROJECTION_ROW_COUNT {
        out.push(Violation::new(
            "g0_projection_disposition_count",
            "g0",
            format!(
                "expected {EXPECTED_G0_PROJECTION_ROW_COUNT} g0 dispositions, found {}",
                g0_by_row_id.len()
            ),
        ));
    }
    for projection in catalog
        .projection_rows
        .iter()
        .filter(|row| row.slice_id == "g0")
    {
        let Some(expected_id) = g0_disposition_row_id(&projection.row_id) else {
            continue;
        };
        let expected_file = PROJECTION_FILES
            .iter()
            .find(|(registry, _)| *registry == projection.projection)
            .map(|(_, file)| format!("registries/{file}"));
        match g0_by_row_id.get(expected_id.as_str()).copied() {
            Some(disposition)
                if disposition.symbol == projection.canonical_symbol
                    && expected_file.as_ref().is_some_and(|file| {
                        disposition.source_locations.as_slice() == [file.as_str()]
                    }) => {}
            Some(disposition) => out.push(Violation::new(
                "g0_projection_disposition_mismatch",
                &projection.row_id,
                format!(
                    "g0 disposition must bind symbol {:?} and source {:?}; found symbol {:?} and source {:?}",
                    projection.canonical_symbol,
                    expected_file,
                    disposition.symbol,
                    disposition.source_locations
                ),
            )),
            None => out.push(Violation::new(
                "g0_projection_disposition_missing",
                &projection.row_id,
                format!("missing exact g0 disposition row {expected_id:?}"),
            )),
        }
    }
}

fn validate_appendix_location(
    row_id: &str,
    location: &str,
    slice_map: &BTreeMap<&str, &Slice>,
    out: &mut Vec<Violation>,
) {
    let Some((slice_id, line_text)) = location.split_once(':') else {
        out.push(Violation::new(
            "catalog_source_location_invalid",
            row_id,
            format!("source location {location:?} must be aNN:<line>"),
        ));
        return;
    };
    let Ok(line) = line_text.parse::<i64>() else {
        out.push(Violation::new(
            "catalog_source_location_invalid",
            row_id,
            format!("source location {location:?} has a nonnumeric line"),
        ));
        return;
    };
    if slice_id == "plan" {
        if line <= 0 {
            out.push(Violation::new(
                "catalog_source_location_invalid",
                row_id,
                format!("source location {location:?} must use a positive plan line"),
            ));
        }
        return;
    }
    let Some(slice) = slice_map.get(slice_id) else {
        out.push(Violation::new(
            "catalog_source_location_invalid",
            row_id,
            format!("source location {location:?} names an unknown slice"),
        ));
        return;
    };
    if !(slice.start_line..=slice.end_line).contains(&line) {
        out.push(Violation::new(
            "catalog_source_location_invalid",
            row_id,
            format!(
                "source location {location:?} lies outside slice {} range {}-{}",
                slice.id, slice.start_line, slice.end_line
            ),
        ));
    }
}

fn annotation_scalar_values(row: &Annotation) -> [&str; 14] {
    [
        &row.exact_type,
        &row.cardinality,
        &row.layout,
        &row.role,
        &row.posture,
        &row.authority,
        &row.locality,
        &row.reference_semantics,
        &row.construction_order,
        &row.retention_and_cut_rule,
        &row.digest_recipe,
        &row.redaction_class,
        &row.resource_bounds,
        &row.compatibility,
    ]
}

fn annotation_generic_formals(
    annotation: &Annotation,
    targets: &BTreeMap<&str, &Target>,
    candidates_by_key: &BTreeMap<&str, &TopLevelCandidate>,
    candidates: &[TopLevelCandidate],
) -> BTreeSet<String> {
    let Some(target) = targets.get(annotation.target_row_id.as_str()).copied() else {
        return BTreeSet::new();
    };
    if let Some(candidate) = candidates_by_key.get(target.source_key.as_str()).copied() {
        return candidates
            .iter()
            .filter(|peer| peer.symbol == candidate.symbol)
            .flat_map(|peer| generic_formals_from_signature(&peer.generic_signature))
            .collect();
    }
    let source_family = target.source_key.split('|').nth(1).filter(|_| {
        target.source_key.starts_with("field|")
            || target.source_key.starts_with("union|")
            || target.source_key.starts_with("arm|")
    });
    candidates
        .iter()
        .filter(|candidate| source_family == Some(candidate.symbol.as_str()))
        .flat_map(|candidate| generic_formals_from_signature(&candidate.generic_signature))
        .collect()
}

const KNOWN_GENERIC_FORMALS: [&str; 9] = [
    "T",
    "Role",
    "Contract",
    "Kind",
    "Profile",
    "Disposition",
    "Operation",
    "Action",
    "Tag",
];

fn valid_generic_formal_token(formal: &str) -> bool {
    !formal.is_empty()
        && !formal.contains('|')
        && formal
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn generic_formals_from_signature(signature: &str) -> BTreeSet<String> {
    let mut formals = BTreeSet::new();
    let Some(inner) = signature
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
    else {
        return formals;
    };
    for parameter in inner.split(',') {
        let (formal, has_bound) = parameter
            .split_once(':')
            .map_or((parameter.trim(), false), |(formal, _)| {
                (formal.trim(), true)
            });
        if valid_generic_formal_token(formal)
            && (has_bound || KNOWN_GENERIC_FORMALS.contains(&formal))
        {
            formals.insert(formal.to_owned());
        }
    }
    formals
}

fn contains_placeholder_marker(value: &str) -> bool {
    const EXACT_SENTINELS: [&str; 11] = [
        "TODO",
        "TBD",
        "FIXME",
        "PLACEHOLDER",
        "UNKNOWN",
        "UNRESOLVED",
        "GENERIC",
        "ANY",
        "T",
        "Role",
        "...",
    ];
    let trimmed = value.trim();
    if trimmed == "*"
        || EXACT_SENTINELS
            .iter()
            .any(|sentinel| trimmed.eq_ignore_ascii_case(sentinel))
    {
        return true;
    }
    let tokens: Vec<_> = trimmed
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .collect();
    for (index, token) in tokens.iter().enumerate() {
        if ["TODO", "TBD", "FIXME", "PLACEHOLDER"]
            .iter()
            .any(|sentinel| token.eq_ignore_ascii_case(sentinel))
        {
            return true;
        }
        if token.eq_ignore_ascii_case("UNKNOWN") || token.eq_ignore_ascii_case("UNRESOLVED") {
            let negated = index.checked_sub(1).is_some_and(|previous| {
                ["NO", "NONE", "WITHOUT", "ZERO"]
                    .iter()
                    .any(|negation| tokens[previous].eq_ignore_ascii_case(negation))
            });
            if !negated {
                return true;
            }
        }
    }
    let upper = trimmed.to_ascii_uppercase();
    [
        "TODO",
        "TBD",
        "FIXME",
        "PLACEHOLDER",
        "UNKNOWN",
        "UNRESOLVED",
    ]
    .iter()
    .any(|sentinel| {
        upper.strip_prefix(sentinel).is_some_and(|remainder| {
            remainder.as_bytes().first().is_some_and(|byte| {
                byte.is_ascii_whitespace()
                    || matches!(*byte, b':' | b'/' | b'-' | b'_' | b'(' | b'[')
            })
        })
    })
}

fn contains_residual_formal(value: &str, formals: &BTreeSet<String>) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|token| !token.is_empty() && formals.contains(token))
}

#[derive(Debug, Default)]
struct AnnotationReferenceShape {
    families: BTreeSet<String>,
    requires_targets: bool,
}

struct AnnotationReferenceRequest<'a> {
    row_id: &'a str,
    exact_type: &'a str,
    reference_semantics: &'a str,
    top_level_definition_family: Option<&'a str>,
}

fn validate_annotation_reference_shape(
    request: AnnotationReferenceRequest<'_>,
    reference_alias_semantics: &BTreeMap<String, String>,
    known_reference_families: &BTreeSet<&str>,
    generic_formals: &BTreeSet<String>,
    out: &mut Vec<Violation>,
) -> AnnotationReferenceShape {
    let AnnotationReferenceRequest {
        row_id,
        exact_type,
        reference_semantics,
        top_level_definition_family,
    } = request;
    const GENERIC_STRONG_WRAPPERS: [&str; 4] = [
        "CertifiedRemoteStrongRef",
        "RegisteredStrongRef",
        "StrongCiphertextRef",
        "StrongRef",
    ];
    const FIXED_STRONG_WRAPPERS: [(&str, &[&str]); 5] = [
        (
            "RemoteConfigurationRef",
            &["RemoteAuthorityConfigurationEvidence"],
        ),
        ("StrongCommandRef", &["LogicalCommandRecord"]),
        (
            "StrongGlobalCommandRef",
            &["GlobalControlRecord", "GlobalTxnRecord"],
        ),
        ("StrongMarkerRef", &["CommitMarker"]),
        ("StrongShardCommandRef", &["ShardCommandRecord"]),
    ];
    const CONDITIONAL_WRAPPERS: [(&str, &[&str]); 6] = [
        ("ConditionalCommandRef", &["LogicalCommandRecord"]),
        ("ConditionalCoordinateRef", &[]),
        (
            "ConditionalGlobalCommandRef",
            &["GlobalControlRecord", "GlobalTxnRecord"],
        ),
        ("ConditionalGlobalTxnInputRef", &["GlobalTxnCommand"]),
        ("ConditionalMarkerRef", &["CommitMarker"]),
        ("ConditionalShardCommandRef", &["ShardCommandRecord"]),
    ];
    let is_declared_definition_type =
        top_level_definition_family.is_some_and(|family| exact_type.trim() == family);
    let bytes = exact_type.as_bytes();
    let mut cursor = 0usize;
    let mut shape = AnnotationReferenceShape::default();
    let mut observed_semantics = BTreeSet::new();
    while cursor < bytes.len() {
        if !bytes[cursor].is_ascii_alphabetic() && bytes[cursor] != b'_' {
            cursor += 1;
            continue;
        }
        let identifier_start = cursor;
        cursor += 1;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        let identifier = &exact_type[identifier_start..cursor];
        let is_definition_identifier =
            is_declared_definition_type && top_level_definition_family == Some(identifier);
        if is_definition_identifier {
            if let Some(semantics) = registered_reference_definition_semantics(identifier) {
                observed_semantics.insert(semantics.to_owned());
            }
            continue;
        }
        if let Some(alias_semantics) = reference_alias_semantics.get(identifier) {
            observed_semantics.insert(alias_semantics.clone());
            if matches!(alias_semantics.as_str(), "strong" | "conditional") {
                shape.requires_targets = true;
            }
            continue;
        }
        let is_generic_strong = GENERIC_STRONG_WRAPPERS.contains(&identifier);
        let fixed_strong_families = FIXED_STRONG_WRAPPERS
            .iter()
            .find(|(wrapper, _)| *wrapper == identifier)
            .map(|(_, families)| *families);
        let is_fixed_strong = fixed_strong_families.is_some();
        let looks_like_strong = identifier.ends_with("StrongRef")
            || (identifier.starts_with("Strong") && identifier.ends_with("Ref"));
        let is_strong = is_generic_strong || is_fixed_strong || looks_like_strong;
        let is_conditional = identifier.starts_with("Conditional") && identifier.ends_with("Ref");
        let fixed_conditional_families = CONDITIONAL_WRAPPERS
            .iter()
            .find(|(wrapper, _)| *wrapper == identifier)
            .map(|(_, families)| *families);
        let is_weak_digest = identifier == "WeakDigest";
        if !is_strong && !is_conditional && !is_weak_digest {
            continue;
        }
        shape.requires_targets = true;
        let wrapper_registered = if is_strong {
            observed_semantics.insert("strong".to_owned());
            is_generic_strong || is_fixed_strong
        } else if is_conditional {
            observed_semantics.insert("conditional".to_owned());
            fixed_conditional_families.is_some()
        } else {
            observed_semantics.insert("weak_digest".to_owned());
            true
        };
        if !wrapper_registered {
            out.push(Violation::new(
                "catalog_annotation_reference_invalid",
                row_id,
                "annotation exact_type uses an unregistered reference wrapper",
            ));
        }
        if let Some(families) = fixed_strong_families {
            shape
                .families
                .extend(families.iter().map(|family| (*family).to_owned()));
        }
        if let Some(families) = fixed_conditional_families {
            shape
                .families
                .extend(families.iter().map(|family| (*family).to_owned()));
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'<') {
            if is_generic_strong {
                out.push(Violation::new(
                    "catalog_annotation_reference_invalid",
                    row_id,
                    "StrongRef wrappers must carry one concrete catalog target",
                ));
            }
            continue;
        }
        if is_fixed_strong
            || fixed_conditional_families.is_some_and(|families| !families.is_empty())
        {
            out.push(Violation::new(
                "catalog_annotation_reference_invalid",
                row_id,
                "fixed-target reference wrappers cannot carry a generic target",
            ));
        }
        let open = cursor;
        let Some(close) = matching_angle(bytes, open) else {
            out.push(Violation::new(
                "catalog_annotation_reference_invalid",
                row_id,
                "StrongRef wrapper has an unbalanced target expression",
            ));
            return shape;
        };
        let target = exact_type[open + 1..close].trim();
        let family = concrete_reference_family(target);
        let valid_family = family.is_some_and(|family| {
            !generic_formals.contains(family) && known_reference_families.contains(family)
        });
        if !valid_family {
            out.push(Violation::new(
                "catalog_annotation_reference_invalid",
                row_id,
                "reference wrappers must carry one concrete catalog target",
            ));
        } else if let Some(family) = family {
            shape.families.insert(family.to_owned());
        }
        // Continue inside the target so nested StrongRef wrappers are checked
        // independently instead of being hidden by the outer application.
        cursor = open + 1;
    }
    let semantics_allowed = matches!(
        reference_semantics,
        "none" | "embedded" | "strong" | "conditional" | "weak_digest" | "locator" | "identity"
    );
    let unregistered_definition_semantics = is_declared_definition_type
        && top_level_definition_family
            .and_then(registered_reference_definition_semantics)
            .is_none()
        && !matches!(reference_semantics, "none" | "embedded");
    let declares_wrapped_reference = matches!(reference_semantics, "strong" | "conditional");
    if !semantics_allowed
        || unregistered_definition_semantics
        || observed_semantics.len() > 1
        || (declares_wrapped_reference
            && !is_declared_definition_type
            && observed_semantics.len() != 1)
        || observed_semantics
            .first()
            .is_some_and(|observed| observed != reference_semantics)
    {
        out.push(Violation::new(
            "catalog_annotation_reference_semantics_mismatch",
            row_id,
            "reference_semantics must be a registered value and match the concrete reference wrapper",
        ));
    }
    if matches!(reference_semantics, "strong" | "conditional") && !is_declared_definition_type {
        shape.requires_targets = true;
    }
    shape
}

/// The plan's wire-tag -> reference-strength declaration (Appendix A,
/// "Reference semantics" and the W12 history-wrapper paragraph).
///
/// Shared with `identity::declared_field_reference_semantics`, which applies the
/// same declaration to `durable_fields.toml` `[[field]]` rows. ONE table: a
/// wrapper whose strength changes here must change on both artifacts at once.
pub(crate) fn registered_reference_definition_semantics(family: &str) -> Option<&'static str> {
    match family {
        "CertifiedRemoteStrongRef"
        | "RegisteredStrongRef"
        | "RemoteConfigurationRef"
        | "StrongCiphertextRef"
        | "StrongCommandRef"
        | "StrongGlobalCommandRef"
        | "StrongMarkerRef"
        | "StrongRef"
        | "StrongShardCommandRef" => Some("strong"),
        "ConditionalCommandRef"
        | "ConditionalCoordinateRef"
        | "ConditionalGlobalCommandRef"
        | "ConditionalGlobalTxnInputRef"
        | "ConditionalMarkerRef"
        | "ConditionalShardCommandRef" => Some("conditional"),
        "CommandRef" | "MarkerRef" => Some("identity"),
        "PreBootstrapArtifactRef" => Some("locator"),
        "WeakDigest" => Some("weak_digest"),
        _ => None,
    }
}

fn concrete_reference_family(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty()
        || value.contains("::")
        || value.contains(['[', ']'])
        || has_top_level_separator(value, b'|')
        || has_top_level_separator(value, b',')
    {
        return None;
    }
    let family_end = value
        .bytes()
        .position(|byte| !byte.is_ascii_alphanumeric() && byte != b'_')
        .unwrap_or(value.len());
    if family_end == 0 {
        return None;
    }
    let family = &value[..family_end];
    let suffix = value[family_end..].trim();
    if suffix.is_empty() {
        return Some(family);
    }
    if !suffix.starts_with('<') {
        return None;
    }
    let close = matching_angle(suffix.as_bytes(), 0)?;
    if close + 1 != suffix.len() || !valid_concrete_type_arguments(&suffix[1..close]) {
        return None;
    }
    Some(family)
}

fn valid_concrete_type_arguments(value: &str) -> bool {
    let mut depth = 0usize;
    let mut start = 0usize;
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' if depth == 0 => return false,
            b'>' => depth -= 1,
            b',' if depth == 0 => {
                if !valid_concrete_type_expression(&value[start..index]) {
                    return false;
                }
                start = index + 1;
            }
            b'|' if depth == 0 => return false,
            _ => {}
        }
    }
    depth == 0 && valid_concrete_type_expression(&value[start..])
}

fn valid_concrete_type_expression(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.contains("::") || value.contains(['[', ']']) {
        return false;
    }
    let identifier_end = value
        .bytes()
        .position(|byte| !byte.is_ascii_alphanumeric() && byte != b'_')
        .unwrap_or(value.len());
    if identifier_end == 0 {
        return false;
    }
    let suffix = value[identifier_end..].trim();
    if suffix.is_empty() {
        return true;
    }
    if !suffix.starts_with('<') {
        return false;
    }
    matching_angle(suffix.as_bytes(), 0).is_some_and(|close| {
        close + 1 == suffix.len() && valid_concrete_type_arguments(&suffix[1..close])
    })
}

fn has_top_level_separator(value: &str, separator: u8) -> bool {
    let mut depth = 0usize;
    for byte in value.bytes() {
        match byte {
            b'<' => depth = depth.saturating_add(1),
            b'>' => depth = depth.saturating_sub(1),
            byte if byte == separator && depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn validate_annotation_reference_targets(
    row: &Annotation,
    reference_shape: &AnnotationReferenceShape,
    schema_family_by_id: &BTreeMap<&str, String>,
    out: &mut Vec<Violation>,
) {
    if !reference_shape.requires_targets {
        return;
    }
    let mut resolved_families = BTreeSet::new();
    let mut all_resolved = true;
    for schema_id in &row.target_schema_ids {
        match schema_family_by_id.get(schema_id.as_str()) {
            Some(family) => {
                resolved_families.insert(family.clone());
            }
            None => all_resolved = false,
        }
    }
    let explicit_families_match = reference_shape.families.is_empty()
        || (row.target_schema_ids.len() == reference_shape.families.len()
            && resolved_families == reference_shape.families);
    if !all_resolved || row.target_schema_ids.is_empty() || !explicit_families_match {
        out.push(Violation::new(
            "catalog_annotation_reference_target_mismatch",
            &row.row_id,
            "StrongRef families must map one-for-one to exact catalog target_schema_ids",
        ));
    }
}

fn validate_annotation_identity_field_contract(
    row: &Annotation,
    projection_by_row_id: &BTreeMap<&str, &ProjectionRowMeta>,
    identity: &IdentityRegistries,
    schema_family_by_id: &BTreeMap<&str, String>,
    out: &mut Vec<Violation>,
) {
    let Some(projection) = projection_by_row_id
        .get(row.target_row_id.as_str())
        .copied()
    else {
        return;
    };
    if projection.projection != "durable_fields" || projection.row_kind != "field" {
        return;
    }
    let Some(field) = identity.fields.iter().find(|field| {
        format!("{}.{}", field.containing_schema, field.stable_name) == projection.canonical_symbol
    }) else {
        out.push(Violation::new(
            "catalog_annotation_field_contract_unresolved",
            &row.row_id,
            "field annotation target does not resolve in the authoritative durable-field registry",
        ));
        return;
    };

    let mut expected_targets = BTreeSet::new();
    if let Some(target) = &field.target_schema_id {
        expected_targets.insert(target.clone());
    } else {
        for union in identity.unions.iter().filter(|union| {
            union.containing_schema == field.containing_schema && union.field_tag == field.field_tag
        }) {
            expected_targets.extend(union.arms.iter().map(|arm| arm.target_schema_id.clone()));
        }
    }
    let mut actual_targets = BTreeSet::new();
    let mut all_targets_resolved = true;
    for schema_id in &row.target_schema_ids {
        match schema_family_by_id.get(schema_id.as_str()) {
            Some(target) => {
                actual_targets.insert(target.clone());
            }
            None => all_targets_resolved = false,
        }
    }
    if row.cardinality != field.cardinality
        || row.reference_semantics != field.reference_semantics
        || !all_targets_resolved
        || row.target_schema_ids.len() != expected_targets.len()
        || actual_targets != expected_targets
    {
        out.push(Violation::new(
            "catalog_annotation_field_contract_mismatch",
            &row.row_id,
            "field annotation cardinality, reference semantics, and exact target schema IDs must byte-match the authoritative durable-field row or reference union",
        ));
    }
}

fn validate_concrete_expansions(row_id: &str, values: &[String], out: &mut Vec<Violation>) {
    if values.windows(2).any(|pair| pair[0] >= pair[1])
        || values.iter().any(|value| {
            value.trim().is_empty()
                || value.contains(['<', '>'])
                || matches!(value.as_str(), "T" | "Role")
        })
    {
        out.push(Violation::new(
            "catalog_expansion_invalid",
            row_id,
            "generic/role expansions must be concrete, strictly sorted, and duplicate-free",
        ));
    }
}

fn derived_metadata_row_id(metadata_kind: &str, target_row_id: &str) -> Option<String> {
    let (scope, target_kind, suffix) = split_catalog_row_id(target_row_id)?;
    Some(format!("{scope}:{metadata_kind}:{target_kind}-{suffix}"))
}

fn g0_disposition_row_id(target_row_id: &str) -> Option<String> {
    let (scope, target_kind, suffix) = split_catalog_row_id(target_row_id)?;
    (scope == "g0").then(|| format!("g0:source-symbol-disposition:{target_kind}-{suffix}"))
}

fn split_catalog_row_id(row_id: &str) -> Option<(&str, &str, &str)> {
    let mut parts = row_id.split(':');
    let scope = parts.next()?;
    let kind = parts.next()?;
    let suffix = parts.next()?;
    (parts.next().is_none()).then_some((scope, kind, suffix))
}

fn exact_single(values: &[String], expected: &str) -> bool {
    matches!(values, [actual] if actual == expected)
}

fn parse_code_reservation(value: &str) -> Option<u16> {
    if value.len() != 6
        || !value.starts_with("0x")
        || !value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let code = u16::from_str_radix(&value[2..], 16).ok()?;
    (code != 0 && code <= 0xbfff).then_some(code)
}

fn valid_type_family(symbol: &str) -> bool {
    symbol
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase())
        && symbol.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn valid_source_candidate_symbol(symbol: &str) -> bool {
    symbol
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase())
        && symbol
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_generic_signature(signature: &str) -> bool {
    signature.is_empty()
        || (!signature.contains(['\r', '\n'])
            && signature.as_bytes().first() == Some(&b'<')
            && matching_angle(signature.as_bytes(), 0) == signature.len().checked_sub(1))
}

fn validate_row_identity(row_id: &str, slice_id: &str, row_kind: &str, out: &mut Vec<Violation>) {
    let expected_prefix = format!("{slice_id}:{row_kind}:");
    if !row_id.starts_with(&expected_prefix) || !valid_row_id(row_id) {
        out.push(Violation::new(
            "catalog_row_id_invalid",
            row_id,
            format!("expected row_id grammar {expected_prefix}<lower-kebab-name>"),
        ));
    }
}

fn validate_metadata_row_id(row_id: &str, row_kind: &str, out: &mut Vec<Violation>) {
    let parts: Vec<&str> = row_id.split(':').collect();
    if parts.len() != 3 || parts[1] != row_kind || !valid_row_id(row_id) {
        out.push(Violation::new(
            "catalog_row_id_invalid",
            row_id,
            format!("metadata row_id must be <scope>:{row_kind}:<lower-kebab-name>"),
        ));
    }
}

fn valid_row_id(row_id: &str) -> bool {
    let parts: Vec<&str> = row_id.split(':').collect();
    parts.len() == 3 && parts.iter().all(|part| valid_lower_kebab_part(part))
}

fn valid_lower_kebab_part(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('-')
        && !part.ends_with('-')
        && !part.contains("--")
        && part
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_slice_id(
    row_id: &str,
    slice_id: &str,
    known_slices: &BTreeSet<&str>,
    out: &mut Vec<Violation>,
) {
    if !matches!(slice_id, "g0" | "plan") && !known_slices.contains(slice_id) {
        out.push(Violation::new(
            "catalog_slice_unknown",
            row_id,
            format!("unknown slice_id {slice_id:?}"),
        ));
    }
}

fn insert_owned_row_id(row_ids: &mut BTreeSet<String>, row_id: &str, out: &mut Vec<Violation>) {
    if !row_ids.insert(row_id.to_owned()) {
        out.push(Violation::new(
            "catalog_row_duplicate",
            row_id,
            "duplicate catalog row_id",
        ));
    }
}

fn render_projection(registry: &str, identity: &IdentityRegistries) -> String {
    match registry {
        "logical_object_kinds" => render_logical(identity),
        "physical_record_kinds" => render_physical(identity),
        "bootstrap_frames" => render_bootstrap(identity),
        "prebootstrap_artifact_kinds" => render_prebootstrap(identity),
        "wire_types" => render_wire(identity),
        "durable_fields" => render_fields(identity),
        _ => String::new(),
    }
}

fn projection_header(registry: &str, epoch: i64) -> String {
    let mut out = String::new();
    writeln!(
        &mut out,
        "# GENERATED from registries/appendix_a_catalog.toml; DO NOT EDIT THIS PROJECTION."
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# Normative source: COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# Appendix A lines {APPENDIX_START_LINE}-{APPENDIX_END_LINE}; sha256={APPENDIX_SHA256}."
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# Identity laws and code-space constraints are enforced by registry-check (plan section 5.1)."
    )
    .expect("writing to String cannot fail");
    writeln!(&mut out, "schema_version = 1\n").expect("writing to String cannot fail");
    writeln!(&mut out, "[registry]").expect("writing to String cannot fail");
    writeln!(&mut out, "name = {}", toml_string(registry)).expect("writing to String cannot fail");
    writeln!(&mut out, "registry_epoch = {epoch}").expect("writing to String cannot fail");
    out
}

fn render_logical(identity: &IdentityRegistries) -> String {
    let mut out = projection_header("logical_object_kinds", identity.logical_epoch);
    let mut rows: Vec<_> = identity.logical.iter().collect();
    rows.sort_by_key(|row| (row.object_kind, row.name.as_str()));
    for row in rows {
        writeln!(&mut out, "\n[[kind]]").expect("writing to String cannot fail");
        writeln!(&mut out, "object_kind = {:#06x}", row.object_kind)
            .expect("writing to String cannot fail");
        write_string(&mut out, "name", &row.name);
        write_string(&mut out, "status", &row.status);
        writeln!(&mut out, "construction_order = {}", row.construction_order)
            .expect("writing to String cannot fail");
        write_string(&mut out, "role_predicate", &row.role_predicate);
        writeln!(&mut out, "max_size_bytes = {}", row.max_size_bytes)
            .expect("writing to String cannot fail");
        write_string(&mut out, "golden_corpus", &row.golden_corpus);
    }
    out
}

fn render_physical(identity: &IdentityRegistries) -> String {
    let mut out = projection_header("physical_record_kinds", identity.physical_epoch);
    let mut rows: Vec<_> = identity.physical.iter().collect();
    rows.sort_by_key(|row| (row.record_kind, row.name.as_str()));
    for row in rows {
        writeln!(&mut out, "\n[[kind]]").expect("writing to String cannot fail");
        writeln!(&mut out, "record_kind = {:#06x}", row.record_kind)
            .expect("writing to String cannot fail");
        write_string(&mut out, "name", &row.name);
        write_string(&mut out, "identity_law", &row.identity_law);
        write_string(&mut out, "status", &row.status);
        write_string(&mut out, "transcript", &row.transcript);
        write_string(&mut out, "owning_identity", &row.owning_identity);
        writeln!(&mut out, "max_size_bytes = {}", row.max_size_bytes)
            .expect("writing to String cannot fail");
    }
    out
}

fn render_bootstrap(identity: &IdentityRegistries) -> String {
    let mut out = projection_header("bootstrap_frames", identity.bootstrap_epoch);
    let mut rows: Vec<_> = identity.bootstrap.iter().collect();
    rows.sort_by_key(|row| (row.frame_kind, row.name.as_str()));
    for row in rows {
        writeln!(&mut out, "\n[[frame]]").expect("writing to String cannot fail");
        writeln!(&mut out, "frame_kind = {:#06x}", row.frame_kind)
            .expect("writing to String cannot fail");
        write_string(&mut out, "name", &row.name);
        write_string(&mut out, "status", &row.status);
        writeln!(&mut out, "byte_size = {}", row.byte_size).expect("writing to String cannot fail");
        write_string(&mut out, "location", &row.location);
        write_string(&mut out, "update_protocol", &row.update_protocol);
        write_string(&mut out, "tear_validation", &row.tear_validation);
        write_string(&mut out, "opener_fields", &row.opener_fields);
        write_string(&mut out, "compatibility_gate", &row.compatibility_gate);
        write_string(&mut out, "recovery_vectors", &row.recovery_vectors);
    }
    out
}

fn render_prebootstrap(identity: &IdentityRegistries) -> String {
    let mut out = projection_header("prebootstrap_artifact_kinds", identity.prebootstrap_epoch);
    let mut rows: Vec<_> = identity.prebootstrap.iter().collect();
    rows.sort_by_key(|row| (row.artifact_kind, row.name.as_str()));
    for row in rows {
        writeln!(&mut out, "\n[[kind]]").expect("writing to String cannot fail");
        writeln!(&mut out, "artifact_kind = {:#06x}", row.artifact_kind)
            .expect("writing to String cannot fail");
        write_string(&mut out, "name", &row.name);
        write_string(&mut out, "status", &row.status);
        write_string(&mut out, "target_claim_domain", &row.target_claim_domain);
        write_string(&mut out, "allowed_containers", &row.allowed_containers);
        write_string(&mut out, "import_target", &row.import_target);
        writeln!(&mut out, "max_size_bytes = {}", row.max_size_bytes)
            .expect("writing to String cannot fail");
    }
    out
}

fn render_wire(identity: &IdentityRegistries) -> String {
    let mut out = projection_header("wire_types", identity.wire_epoch);
    let mut rows: Vec<_> = identity.wire.iter().collect();
    rows.sort_by_key(|row| (row.wire_type_id, row.name.as_str()));
    for row in rows {
        writeln!(&mut out, "\n[[type]]").expect("writing to String cannot fail");
        writeln!(&mut out, "wire_type_id = {:#06x}", row.wire_type_id)
            .expect("writing to String cannot fail");
        write_string(&mut out, "name", &row.name);
        write_string(&mut out, "kind", &row.kind);
        write_string(&mut out, "status", &row.status);
        if let Some(containing_union) = &row.containing_union {
            write_string(&mut out, "containing_union", containing_union);
        }
        if let Some(wire_tag) = row.wire_tag {
            writeln!(&mut out, "wire_tag = {wire_tag:#06x}")
                .expect("writing to String cannot fail");
        }
        write_string(&mut out, "encoding_context", &row.encoding_context);
        write_string_array(
            &mut out,
            "allowed_containing_schemas",
            &row.allowed_containing_schemas,
        );
        writeln!(&mut out, "max_size_bytes = {}", row.max_size_bytes)
            .expect("writing to String cannot fail");
    }
    out
}

fn render_fields(identity: &IdentityRegistries) -> String {
    let mut out = projection_header("durable_fields", identity.fields_epoch);
    let mut fields: Vec<_> = identity.fields.iter().collect();
    fields.sort_by_key(|row| {
        (
            row.containing_schema.as_str(),
            row.field_tag,
            row.stable_name.as_str(),
        )
    });
    for row in fields {
        writeln!(&mut out, "\n[[field]]").expect("writing to String cannot fail");
        write_string(&mut out, "containing_schema", &row.containing_schema);
        writeln!(&mut out, "field_tag = {:#06x}", row.field_tag)
            .expect("writing to String cannot fail");
        write_string(&mut out, "stable_name", &row.stable_name);
        write_string(&mut out, "exact_wire_type", &row.exact_wire_type);
        write_string(&mut out, "cardinality", &row.cardinality);
        write_string(&mut out, "identity_class", &row.identity_class);
        write_string(&mut out, "reference_semantics", &row.reference_semantics);
        if let Some(target) = &row.target_schema_id {
            write_string(&mut out, "target_schema_id", target);
        }
        writeln!(&mut out, "construction_order = {}", row.construction_order)
            .expect("writing to String cannot fail");
        write_string(&mut out, "role_predicate", &row.role_predicate);
        write_string(
            &mut out,
            "retention_and_cut_rule",
            &row.retention_and_cut_rule,
        );
        write_string(&mut out, "version_status", &row.version_status);
        writeln!(&mut out, "max_size_bytes = {}", row.max_size_bytes)
            .expect("writing to String cannot fail");
        if let Some(value) = &row.digest_class {
            write_string(&mut out, "digest_class", value);
        }
        if let Some(value) = &row.transcript_recipe {
            write_string(&mut out, "transcript_recipe", value);
        }
        if let Some(value) = &row.bd_domain_separator {
            write_string(&mut out, "bd_domain_separator", value);
        }
        if let Some(value) = row.bd_schema_major {
            writeln!(&mut out, "bd_schema_major = {value}").expect("writing to String cannot fail");
        }
        if let Some(values) = &row.bd_included_field_tags {
            write_int_array(&mut out, "bd_included_field_tags", values);
        }
        if let Some(values) = &row.bd_excluded_field_tags {
            write_int_array(&mut out, "bd_excluded_field_tags", values);
        }
        if let Some(value) = &row.recipe_pin {
            write_string(&mut out, "recipe_pin", value);
        }
    }
    let mut ordinary_unions: Vec<_> = identity.ordinary_unions.iter().collect();
    ordinary_unions.sort_by_key(|union| {
        (
            union.containing_schema.as_str(),
            union.union_path.as_str(),
            union.union_name.as_str(),
        )
    });
    for union in &ordinary_unions {
        writeln!(&mut out, "\n[[union]]").expect("writing to String cannot fail");
        write_string(&mut out, "union_name", &union.union_name);
        write_string(&mut out, "containing_schema", &union.containing_schema);
        write_string(&mut out, "union_path", &union.union_path);
        if let Some(field_tag) = union.field_tag {
            writeln!(&mut out, "field_tag = {field_tag:#06x}")
                .expect("writing to String cannot fail");
        }
        write_string(&mut out, "tag_wire_type", &union.tag_wire_type);
        write_string(&mut out, "encoding_context", &union.encoding_context);
        write_string_array(
            &mut out,
            "allowed_containing_schemas",
            &union.allowed_containing_schemas,
        );
        write_string(&mut out, "role_predicate", &union.role_predicate);
        write_string(&mut out, "version_status", &union.version_status);
        writeln!(&mut out, "max_size_bytes = {}", union.max_size_bytes)
            .expect("writing to String cannot fail");
    }
    for union in ordinary_unions {
        let mut arms: Vec<_> = union.arms.iter().collect();
        arms.sort_by_key(|arm| (arm.arm_tag, arm.stable_name.as_str()));
        for arm in arms {
            writeln!(&mut out, "\n[[union_arm]]").expect("writing to String cannot fail");
            write_string(&mut out, "union_name", &arm.union_name);
            write_string(&mut out, "containing_schema", &arm.containing_schema);
            write_string(&mut out, "union_path", &arm.union_path);
            writeln!(&mut out, "arm_tag = {:#06x}", arm.arm_tag)
                .expect("writing to String cannot fail");
            write_string(&mut out, "source_arm_name", &arm.source_arm_name);
            write_string(&mut out, "stable_name", &arm.stable_name);
            write_string(&mut out, "payload_kind", &arm.payload_kind);
            if let Some(payload_sha256) = &arm.payload_sha256 {
                write_string(&mut out, "payload_sha256", payload_sha256);
            }
            write_string(&mut out, "role_predicate", &arm.role_predicate);
            write_string(&mut out, "version_status", &arm.version_status);
            writeln!(&mut out, "max_size_bytes = {}", arm.max_size_bytes)
                .expect("writing to String cannot fail");
        }
    }
    let mut unions: Vec<_> = identity.unions.iter().collect();
    unions.sort_by_key(|union| {
        (
            union.containing_schema.as_str(),
            union.field_tag,
            union.union_name.as_str(),
        )
    });
    for union in &unions {
        writeln!(&mut out, "\n[[reference_union]]").expect("writing to String cannot fail");
        write_string(&mut out, "union_name", &union.union_name);
        write_string(&mut out, "containing_schema", &union.containing_schema);
        writeln!(&mut out, "field_tag = {:#06x}", union.field_tag)
            .expect("writing to String cannot fail");
        write_string(&mut out, "role", &union.role);
    }
    for union in unions {
        let mut arms: Vec<_> = union.arms.iter().collect();
        arms.sort_by_key(|arm| (arm.arm_tag, arm.stable_name.as_str()));
        for arm in arms {
            writeln!(&mut out, "\n[[reference_union_arm]]").expect("writing to String cannot fail");
            write_string(&mut out, "union_name", &arm.union_name);
            write_string(&mut out, "containing_schema", &arm.containing_schema);
            writeln!(&mut out, "field_tag = {:#06x}", arm.field_tag)
                .expect("writing to String cannot fail");
            writeln!(&mut out, "arm_tag = {:#06x}", arm.arm_tag)
                .expect("writing to String cannot fail");
            write_string(&mut out, "stable_name", &arm.stable_name);
            write_string(&mut out, "target_schema_id", &arm.target_schema_id);
            write_string(&mut out, "role", &arm.role);
            write_string(&mut out, "identity_class", &arm.identity_class);
            write_string(&mut out, "reference_semantics", &arm.reference_semantics);
            write_string(&mut out, "role_predicate", &arm.role_predicate);
            write_string(
                &mut out,
                "retention_and_cut_rule",
                &arm.retention_and_cut_rule,
            );
            write_string(&mut out, "version_status", &arm.version_status);
            writeln!(&mut out, "max_size_bytes = {}", arm.max_size_bytes)
                .expect("writing to String cannot fail");
        }
    }
    out
}

fn write_string(out: &mut String, key: &str, value: &str) {
    writeln!(out, "{key} = {}", toml_string(value)).expect("writing to String cannot fail");
}

fn write_string_array(out: &mut String, key: &str, values: &[String]) {
    let rendered = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "{key} = [{rendered}]").expect("writing to String cannot fail");
}

fn write_int_array(out: &mut String, key: &str, values: &[i64]) {
    let rendered = values
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "{key} = [{rendered}]").expect("writing to String cannot fail");
}

fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character.is_control() => {
                write!(&mut out, "\\u{:04x}", u32::from(character))
                    .expect("writing to String cannot fail");
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn display_byte(byte: Option<u8>) -> String {
    byte.map_or_else(|| "<eof>".to_owned(), |value| format!("0x{value:02x}"))
}

fn verify_source_bytes(
    bytes: &[u8],
    expected_count: i64,
    expected_hash: &str,
    row_id: &str,
    out: &mut Vec<Violation>,
) {
    let actual_count = i64::try_from(bytes.len());
    if actual_count != Ok(expected_count) {
        out.push(Violation::new(
            "source_byte_count_mismatch",
            row_id,
            format!("expected {expected_count} bytes, found {}", bytes.len()),
        ));
    }
    let actual = sha256_hex(bytes);
    if actual != expected_hash {
        out.push(Violation::new(
            "source_sha256_mismatch",
            row_id,
            format!("expected {expected_hash}, found {actual}"),
        ));
    }
}

fn verify_heading(
    source: &[u8],
    spans: &[(usize, usize)],
    line: i64,
    expected: &str,
    field: &str,
    out: &mut Vec<Violation>,
) {
    let Some(bytes) = extract_lines(source, spans, line, line) else {
        out.push(Violation::new(
            "source_heading_missing",
            "source_manifest",
            format!("{field} line {line} is missing"),
        ));
        return;
    };
    let without_lf = match bytes.strip_suffix(b"\n") {
        Some(value) => value,
        None => bytes,
    };
    if without_lf != expected.as_bytes() {
        out.push(Violation::new(
            "source_heading_mismatch",
            "source_manifest",
            format!("{field} at line {line} does not match its exact pin"),
        ));
    }
}

fn source_line_spans(source: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (index, byte) in source.iter().enumerate() {
        if *byte == b'\n' {
            spans.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < source.len() {
        spans.push((start, source.len()));
    }
    spans
}

fn extract_lines<'a>(
    source: &'a [u8],
    spans: &[(usize, usize)],
    start_line: i64,
    end_line: i64,
) -> Option<&'a [u8]> {
    if start_line <= 0 || end_line < start_line {
        return None;
    }
    let first = usize::try_from(start_line.checked_sub(1)?).ok()?;
    let last = usize::try_from(end_line.checked_sub(1)?).ok()?;
    let (start, _) = *spans.get(first)?;
    let (_, end) = *spans.get(last)?;
    source.get(start..end)
}

fn validate_utf8_lf(bytes: &[u8], row_id: &str, code: &str) -> Result<(), Vec<Violation>> {
    let mut out = Vec::new();
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        out.push(Violation::new(code, row_id, "UTF-8 BOM is forbidden"));
    }
    if bytes.contains(&b'\r') {
        out.push(Violation::new(
            code,
            row_id,
            "carriage returns are forbidden; canonical text is LF-only",
        ));
    }
    if let Err(error) = std::str::from_utf8(bytes) {
        out.push(Violation::new(
            code,
            row_id,
            format!("invalid UTF-8: {error}"),
        ));
    }
    if out.is_empty() {
        Ok(())
    } else {
        sort_violations(&mut out);
        Err(out)
    }
}

fn exact_keys(table: &Table, allowed: &[&str], row_id: &str, out: &mut Vec<Violation>) {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            out.push(Violation::new(
                "catalog_unknown_key",
                row_id,
                format!("unknown key {key:?} in closed schema"),
            ));
        }
    }
}

fn read_table<'a>(
    table: &'a Table,
    key: &str,
    row_id: &str,
    out: &mut Vec<Violation>,
) -> Option<&'a Table> {
    match toml::get_table(table, key, row_id) {
        Ok(value) => Some(value),
        Err(error) => {
            out.push(Violation::new("catalog_schema", row_id, error.to_string()));
            None
        }
    }
}

fn read_table_array<'a>(
    table: &'a Table,
    key: &str,
    row_id: &str,
    out: &mut Vec<Violation>,
) -> Option<Vec<&'a Table>> {
    match toml::get_table_array(table, key, row_id) {
        Ok(value) => Some(value),
        Err(error) => {
            out.push(Violation::new("catalog_schema", row_id, error.to_string()));
            None
        }
    }
}

fn read_string(table: &Table, key: &str, row_id: &str, out: &mut Vec<Violation>) -> Option<String> {
    match toml::get_str(table, key, row_id) {
        Ok(value) => Some(value),
        Err(error) => {
            out.push(Violation::new("catalog_schema", row_id, error.to_string()));
            None
        }
    }
}

fn read_int(table: &Table, key: &str, row_id: &str, out: &mut Vec<Violation>) -> Option<i64> {
    match toml::get_int(table, key, row_id) {
        Ok(value) => Some(value),
        Err(error) => {
            out.push(Violation::new("catalog_schema", row_id, error.to_string()));
            None
        }
    }
}

fn read_string_array(
    table: &Table,
    key: &str,
    row_id: &str,
    out: &mut Vec<Violation>,
) -> Option<Vec<String>> {
    match toml::get_str_array(table, key, row_id) {
        Ok(value) => Some(value),
        Err(error) => {
            out.push(Violation::new("catalog_schema", row_id, error.to_string()));
            None
        }
    }
}

fn pin_str(out: &mut Vec<Violation>, row_id: &str, field: &str, expected: &str, actual: &str) {
    if actual != expected {
        out.push(Violation::new(
            "catalog_pin_mismatch",
            row_id,
            format!("{field} expected {expected:?}, found {actual:?}"),
        ));
    }
}

fn pin_i64(out: &mut Vec<Violation>, row_id: &str, field: &str, expected: i64, actual: i64) {
    if actual != expected {
        out.push(Violation::new(
            "catalog_pin_mismatch",
            row_id,
            format!("{field} expected {expected}, found {actual}"),
        ));
    }
}

fn valid_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sort_violations(violations: &mut [Violation]) {
    violations.sort_by(|left, right| {
        (&left.row_id, &left.code, &left.msg).cmp(&(&right.row_id, &right.code, &right.msg))
    });
}

#[cfg(test)]
mod binding_contract_tests {
    use super::*;
    use crate::appendix_source::{
        AmbiguityKey, CensusCounts, CensusTranscripts, DefinitionKind, FieldCandidateKey,
        SchemaCandidateKey, SliceSourceCensus, TranscriptDigest,
    };

    const TARGET_ROW_ID: &str = "a01:bootstrap-frame:root-slot";
    const TARGET_SOURCE_KEY: &str = "top|RootSlot";

    #[test]
    fn cargo_package_identity_ignores_unrelated_full_toml_syntax() {
        let manifest = r#"
[package]
name = "fgdb-fixture"
version = "0.0.1"

[dependencies]
asupersync = { git = "https://example.invalid/asupersync", default-features = false }
"#;
        assert_eq!(
            cargo_package_name(manifest, Path::new("fixture/Cargo.toml")),
            Ok("fgdb-fixture".to_owned())
        );
    }

    #[test]
    fn workspace_member_paths_preserve_unexcluded_explicit_member() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let document = toml::parse(
            r#"
[workspace]
members = ["tools/registry-check"]
"#,
        )
        .expect("workspace fixture parses");
        let workspace =
            toml::get_table(&document, "workspace", "Cargo.toml").expect("workspace exists");
        let members = toml::get_str_array(workspace, "members", "Cargo.toml.workspace")
            .expect("members parse");
        let excludes = workspace_exact_excludes(workspace).expect("missing exclude is empty");

        assert_eq!(
            workspace_member_paths(&root, &members, &excludes).expect("explicit member resolves"),
            vec![PathBuf::from("tools/registry-check")]
        );
    }

    #[test]
    fn workspace_member_paths_apply_exact_exclude_to_glob() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let document = toml::parse(
            r#"
[workspace]
members = ["crates/*"]
exclude = ["crates/fgdb-types"]
"#,
        )
        .expect("workspace fixture parses");
        let workspace =
            toml::get_table(&document, "workspace", "Cargo.toml").expect("workspace exists");
        let members = toml::get_str_array(workspace, "members", "Cargo.toml.workspace")
            .expect("members parse");
        let excludes = workspace_exact_excludes(workspace).expect("exact exclude parses");
        let member_paths =
            workspace_member_paths(&root, &members, &excludes).expect("member glob resolves");

        assert!(member_paths.contains(&PathBuf::from("crates/fgdb-bigint")));
        assert!(!member_paths.contains(&PathBuf::from("crates/fgdb-types")));
    }

    #[test]
    fn workspace_exclude_patterns_fail_closed() {
        let document = toml::parse(
            r#"
[workspace]
members = ["crates/*"]
exclude = ["crates/fgdb-*"]
"#,
        )
        .expect("workspace fixture parses");
        let workspace =
            toml::get_table(&document, "workspace", "Cargo.toml").expect("workspace exists");

        assert_eq!(
            workspace_exact_excludes(workspace),
            Err("unsupported non-exact Cargo workspace exclude \"crates/fgdb-*\"".to_owned())
        );
    }

    #[test]
    fn ordinary_union_row_identity_is_shared_by_projection_consumers() {
        let union_digest = sha256_hex(b"union|RestoreOutcome|result");
        let arm_digest = sha256_hex(b"arm|RestoreOutcome|result|Ready");
        let union_row_id = format!("a20:union:restore-result-{}", &union_digest[..16]);
        let arm_row_id = format!("a20:union-arm:restore-result-ready-{}", &arm_digest[..16]);
        let document = format!(
            r#"
[[union]]
row_id = "{union_row_id}"
slice_id = "a20"
containing_schema = "RestoreOutcome"
union_path = "result"
union_name = "RestoreResult"

[[union_arm]]
row_id = "{arm_row_id}"
slice_id = "a20"
containing_schema = "RestoreOutcome"
union_path = "result"
union_name = "RestoreResult"
source_arm_name = "Ready"
stable_name = "Ready"
"#
        );
        let root = toml::parse(&document).expect("ordinary union fixture parses");
        let mut metadata = Vec::new();
        let mut producer_violations = Vec::new();
        catalog_projection_rows(
            &root,
            "union",
            "durable_fields",
            "union",
            &mut metadata,
            &mut producer_violations,
        )
        .expect("union projection rows");
        catalog_projection_rows(
            &root,
            "union_arm",
            "durable_fields",
            "union-arm",
            &mut metadata,
            &mut producer_violations,
        )
        .expect("union-arm projection rows");
        assert!(producer_violations.is_empty(), "{producer_violations:?}");
        assert_eq!(metadata.len(), 2);

        let mut consumer_violations = Vec::new();
        for row in &metadata {
            validate_projection_row_derived_identity(row, &mut consumer_violations);
        }
        assert!(consumer_violations.is_empty(), "{consumer_violations:?}");

        metadata[0].row_id.push('0');
        validate_projection_row_derived_identity(&metadata[0], &mut consumer_violations);
        assert_eq!(
            consumer_violations
                .iter()
                .filter(|violation| violation.code == "catalog_row_id_derived_mismatch")
                .count(),
            1,
            "a mutated hash-bearing row ID must fail closed: {consumer_violations:?}"
        );
    }

    fn catalog_with_bindings() -> Catalog {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        catalog.semantic_bindings.push(SemanticBinding {
            row_id: "a01:semantic-binding:bootstrap-frame-root-slot".to_owned(),
            target_row_id: TARGET_ROW_ID.to_owned(),
            owner_bead_id: "fgdb-w2-owner-fixture".to_owned(),
            owner_crate: "fgdb-chronicle".to_owned(),
            owner_status: "planned".to_owned(),
            consumer_crates: vec!["fgdb".to_owned(), "fgdb-server".to_owned()],
        });
        catalog.evidence.push(EvidenceBinding {
            row_id: "a01:evidence:bootstrap-frame-root-slot-static-contract".to_owned(),
            target_row_id: TARGET_ROW_ID.to_owned(),
            evidence_id: "static-contract".to_owned(),
            phase: "static".to_owned(),
            status: "live".to_owned(),
            owner_bead_id: "fgdb-verification-owner-fixture".to_owned(),
            checker_ids: vec!["appendix_a_catalog_closure".to_owned()],
            scenario_ids: vec!["g0_identity_e2e".to_owned()],
            event_ids: vec!["appendix_closure_checked".to_owned()],
            gate_ids: vec!["G0".to_owned()],
        });
        catalog.evidence.push(EvidenceBinding {
            row_id: "a01:evidence:bootstrap-frame-root-slot-runtime-contract".to_owned(),
            target_row_id: TARGET_ROW_ID.to_owned(),
            evidence_id: "runtime-contract".to_owned(),
            phase: "runtime".to_owned(),
            status: "planned".to_owned(),
            owner_bead_id: "fgdb-verification-owner-fixture".to_owned(),
            checker_ids: vec!["appendix_a_catalog_closure".to_owned()],
            scenario_ids: vec!["g0_identity_e2e".to_owned()],
            event_ids: vec!["appendix_closure_checked".to_owned()],
            gate_ids: vec!["G0".to_owned()],
        });
        catalog
    }

    fn catalog_with_annotation() -> Catalog {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        catalog.annotations.push(Annotation {
            row_id: "a01:annotation:bootstrap-frame-root-slot".to_owned(),
            target_row_id: TARGET_ROW_ID.to_owned(),
            exact_type: "RootSlot".to_owned(),
            cardinality: "one".to_owned(),
            layout: "fixed".to_owned(),
            role: "Local".to_owned(),
            posture: "bootstrap".to_owned(),
            authority: "root".to_owned(),
            locality: "local".to_owned(),
            generic_expansions: Vec::new(),
            role_expansions: Vec::new(),
            reference_semantics: "embedded".to_owned(),
            target_schema_ids: Vec::new(),
            construction_order: "bootstrap-root-slot".to_owned(),
            retention_and_cut_rule: "fixed-location".to_owned(),
            digest_recipe: "slot-checksum".to_owned(),
            redaction_class: "public-commitment".to_owned(),
            resource_bounds: "fixed-4096-bytes".to_owned(),
            compatibility: "v1".to_owned(),
        });
        catalog
    }

    const fn annotation_pin() -> AnnotationContractPin {
        AnnotationContractPin {
            row_id: "a01:annotation:bootstrap-frame-root-slot",
            target_row_id: TARGET_ROW_ID,
            target_source_key: TARGET_SOURCE_KEY,
            exact_type: "RootSlot",
            cardinality: "one",
            layout: "fixed",
            role: "Local",
            posture: "bootstrap",
            authority: "root",
            locality: "local",
            generic_expansions: &[],
            role_expansions: &[],
            reference_semantics: "embedded",
            target_schema_ids: &[],
            construction_order: "bootstrap-root-slot",
            retention_and_cut_rule: "fixed-location",
            digest_recipe: "slot-checksum",
            redaction_class: "public-commitment",
            resource_bounds: "fixed-4096-bytes",
            compatibility: "v1",
        }
    }

    const fn semantic_pin() -> SemanticBindingContractPin {
        SemanticBindingContractPin {
            row_id: "a01:semantic-binding:bootstrap-frame-root-slot",
            target_row_id: TARGET_ROW_ID,
            target_source_key: TARGET_SOURCE_KEY,
            owner_bead_id: "fgdb-w2-owner-fixture",
            owner_crate: "fgdb-chronicle",
            owner_status: "planned",
            consumer_crates: &["fgdb", "fgdb-server"],
        }
    }

    const fn static_evidence_pin() -> EvidenceBindingContractPin {
        EvidenceBindingContractPin {
            row_id: "a01:evidence:bootstrap-frame-root-slot-static-contract",
            target_row_id: TARGET_ROW_ID,
            target_source_key: TARGET_SOURCE_KEY,
            evidence_id: "static-contract",
            phase: "static",
            status: "live",
            owner_bead_id: "fgdb-verification-owner-fixture",
            checker_ids: &["appendix_a_catalog_closure"],
            scenario_ids: &["g0_identity_e2e"],
            event_ids: &["appendix_closure_checked"],
            gate_ids: &["G0"],
        }
    }

    const fn runtime_evidence_pin() -> EvidenceBindingContractPin {
        EvidenceBindingContractPin {
            row_id: "a01:evidence:bootstrap-frame-root-slot-runtime-contract",
            target_row_id: TARGET_ROW_ID,
            target_source_key: TARGET_SOURCE_KEY,
            evidence_id: "runtime-contract",
            phase: "runtime",
            status: "planned",
            owner_bead_id: "fgdb-verification-owner-fixture",
            checker_ids: &["appendix_a_catalog_closure"],
            scenario_ids: &["g0_identity_e2e"],
            event_ids: &["appendix_closure_checked"],
            gate_ids: &["G0"],
        }
    }

    fn schema(generic_signature: &str) -> SchemaCandidate {
        SchemaCandidate {
            key: SchemaCandidateKey {
                family: "RecoveryBridgeSpec".to_owned(),
                generic_signature: generic_signature.to_owned(),
            },
            owner_statuses: vec![SchemaOwnerStatus::ConfirmedTopLevel],
            definition_kinds: vec![DefinitionKind::InlineRecord],
            expression_sha256s: vec!["fixture".to_owned()],
            body_conflict: false,
            locations: Vec::new(),
        }
    }

    fn ambiguity(kind: AmbiguityKind, affected_source_keys: &[&str]) -> AmbiguityCandidate {
        AmbiguityCandidate {
            key: AmbiguityKey {
                kind,
                schema_family: None,
                path: None,
                raw_sha256: "0".repeat(64),
                affected_source_key_count: affected_source_keys.len(),
                affected_source_keys_sha256: "0".repeat(64),
                reason: "fixture".to_owned(),
            },
            raw: "fixture".to_owned(),
            affected_source_keys: affected_source_keys
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            locations: Vec::new(),
        }
    }

    fn empty_transcripts() -> CensusTranscripts {
        let digest = || TranscriptDigest {
            rows: 0,
            sha256: String::new(),
        };
        CensusTranscripts {
            schemas: digest(),
            fields: digest(),
            unions: digest(),
            arms: digest(),
            ambiguities: digest(),
        }
    }

    fn field_candidate(owner: &str, path: &str, stable_name: &str) -> FieldCandidate {
        generic_field_candidate(owner, owner, path, stable_name)
    }

    fn generic_field_candidate(
        family: &str,
        owner: &str,
        path: &str,
        stable_name: &str,
    ) -> FieldCandidate {
        FieldCandidate {
            key: FieldCandidateKey {
                schema_family: family.to_owned(),
                schema_owner: owner.to_owned(),
                path: path.to_owned(),
                stable_name: stable_name.to_owned(),
            },
            exact_types: Vec::new(),
            cardinalities: Vec::new(),
            type_conflict: false,
            ambiguous: false,
            locations: Vec::new(),
        }
    }

    fn census_with_slice(
        slice_id: &str,
        fields: Vec<FieldCandidate>,
        ambiguities: Vec<AmbiguityCandidate>,
    ) -> AppendixSourceCensus {
        census_with_slice_rows(slice_id, fields, Vec::new(), Vec::new(), ambiguities)
    }

    fn census_with_slice_rows(
        slice_id: &str,
        fields: Vec<FieldCandidate>,
        unions: Vec<UnionCandidate>,
        arms: Vec<ArmCandidate>,
        ambiguities: Vec<AmbiguityCandidate>,
    ) -> AppendixSourceCensus {
        AppendixSourceCensus {
            source_start_line: 1,
            source_end_line: 1,
            source_byte_count: 0,
            source_sha256: String::new(),
            slices: vec![SliceSourceCensus {
                slice_id: slice_id.to_owned(),
                start_line: 1,
                end_line: 1,
                source_byte_count: 0,
                source_sha256: String::new(),
                schemas: Vec::new(),
                fields: fields.clone(),
                unions: unions.clone(),
                arms: arms.clone(),
                ambiguities,
                counts: CensusCounts::default(),
                transcripts: empty_transcripts(),
            }],
            schemas: Vec::new(),
            fields,
            unions,
            arms,
            ambiguities: Vec::new(),
            counts: CensusCounts::default(),
            transcripts: empty_transcripts(),
        }
    }

    fn union_candidate(owner: &str, union_path: &str) -> UnionCandidate {
        UnionCandidate {
            key: crate::appendix_source::UnionCandidateKey {
                schema_family: owner.to_owned(),
                schema_owner: owner.to_owned(),
                union_path: union_path.to_owned(),
            },
            occurrence_count: 1,
            arm_names: Vec::new(),
            arm_name_sets: Vec::new(),
            arm_set_conflict: false,
            parsed_arm_count: 0,
            unparsed_arm_count: 0,
            locations: Vec::new(),
            evidence_lines: Vec::new(),
        }
    }

    fn arm_candidate(owner: &str, union_path: &str, arm_name: &str) -> ArmCandidate {
        ArmCandidate {
            key: crate::appendix_source::ArmCandidateKey {
                schema_family: owner.to_owned(),
                schema_owner: owner.to_owned(),
                union_path: union_path.to_owned(),
                arm_name: arm_name.to_owned(),
            },
            payload_sha256s: Vec::new(),
            payload_conflict: false,
            locations: Vec::new(),
        }
    }

    fn arm_target(slice_id: &str, owner: &str, union_path: &str, arm_name: &str) -> Target {
        let source_key = format!("arm|{owner}|{union_path}|{arm_name}");
        let digest = sha256_hex(source_key.as_bytes());
        Target {
            row_id: format!("{slice_id}:target:union-arm-fixture-{}", &digest[..16]),
            target_row_id: format!("{slice_id}:union-arm:fixture-{}", &digest[..16]),
            slice_id: slice_id.to_owned(),
            source_key,
            target_kind: "union-arm".to_owned(),
            definition_status: "declared".to_owned(),
        }
    }

    fn catalog_with_complete_a01() -> Catalog {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        catalog
            .slices
            .iter_mut()
            .find(|slice| slice.id == "a01")
            .expect("a01 slice exists")
            .definition_status = "complete".to_owned();
        catalog
    }

    fn uncovered_field_violations(violations: &[Violation]) -> Vec<&Violation> {
        violations
            .iter()
            .filter(|violation| violation.code == "source_complete_census_uncovered")
            .collect()
    }

    #[test]
    fn arm_interior_census_field_requires_a_covering_arm_target() {
        let census = census_with_slice(
            "a01",
            vec![field_candidate(
                "FixtureState",
                "FixtureState.phase.Started.begun_ref",
                "begun_ref",
            )],
            Vec::new(),
        );

        let bare = catalog_with_complete_a01();
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&bare, &census, &mut violations);
        assert_eq!(
            uncovered_field_violations(&violations).len(),
            1,
            "an arm-interior census field without a covering arm target must fail closed: {violations:?}"
        );

        let mut covered = catalog_with_complete_a01();
        covered.targets.push(arm_target(
            "a01",
            "FixtureState",
            "FixtureState.phase",
            "Started",
        ));
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&covered, &census, &mut violations);
        assert!(
            uncovered_field_violations(&violations).is_empty(),
            "the union-arm payload contract covers its interior fields: {violations:?}"
        );
    }

    #[test]
    fn wire_interior_census_field_is_covered_by_its_targeted_wire_row() {
        let census = census_with_slice(
            "a01",
            vec![
                field_candidate("StrongRef", "StrongRef.oid", "oid"),
                field_candidate(
                    "NotARegisteredWireType",
                    "NotARegisteredWireType.oid",
                    "oid",
                ),
            ],
            Vec::new(),
        );
        let catalog = catalog_with_complete_a01();
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&catalog, &census, &mut violations);
        let uncovered = uncovered_field_violations(&violations);
        assert_eq!(
            uncovered.len(),
            1,
            "only the unregistered host may stay uncovered: {violations:?}"
        );
        assert!(
            uncovered[0].msg.contains("NotARegisteredWireType"),
            "the targeted wire envelope covers its interior fields: {violations:?}"
        );
    }

    #[test]
    fn wire_coverage_matches_the_generic_free_family_symbol() {
        let census = census_with_slice(
            "a01",
            vec![
                generic_field_candidate(
                    "StrongCiphertextRef",
                    "StrongCiphertextRef<T>",
                    "StrongCiphertextRef<T>.ciphertext_digest",
                    "ciphertext_digest",
                ),
                generic_field_candidate(
                    "NotAWireFamily",
                    "NotAWireFamily<T>",
                    "NotAWireFamily<T>.value",
                    "value",
                ),
            ],
            Vec::new(),
        );
        let catalog = catalog_with_complete_a01();
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&catalog, &census, &mut violations);
        let uncovered = uncovered_field_violations(&violations);
        assert_eq!(
            uncovered.len(),
            1,
            "one wire row commits the envelope for every expansion of its family: {violations:?}"
        );
        assert!(
            uncovered[0].msg.contains("NotAWireFamily"),
            "a generic family without a wire row stays uncovered: {violations:?}"
        );
    }

    #[test]
    fn nested_union_and_arm_census_keys_are_covered_by_the_targeted_parent_arm() {
        let census = census_with_slice_rows(
            "a01",
            Vec::new(),
            vec![union_candidate(
                "FixtureState",
                "FixtureState.phase.Started.mode",
            )],
            vec![arm_candidate(
                "FixtureState",
                "FixtureState.phase.Started.mode",
                "Fast",
            )],
            Vec::new(),
        );

        let bare = catalog_with_complete_a01();
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&bare, &census, &mut violations);
        assert_eq!(
            uncovered_field_violations(&violations).len(),
            2,
            "a nested union and its arm without a covering parent-arm target must fail closed: {violations:?}"
        );

        let mut covered = catalog_with_complete_a01();
        covered.targets.push(arm_target(
            "a01",
            "FixtureState",
            "FixtureState.phase",
            "Started",
        ));
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&covered, &census, &mut violations);
        assert!(
            uncovered_field_violations(&violations).is_empty(),
            "the parent arm's payload contract commits nested unions and arms: {violations:?}"
        );
    }

    #[test]
    fn wire_interior_union_and_arm_census_keys_are_covered_by_the_wire_envelope() {
        let census = census_with_slice_rows(
            "a01",
            Vec::new(),
            vec![
                union_candidate("ConsensusDomain", "ConsensusDomain.group_role"),
                union_candidate("NotARegisteredWireType", "NotARegisteredWireType.mode"),
            ],
            vec![
                arm_candidate("ConsensusDomain", "ConsensusDomain.group_role", "Shard"),
                arm_candidate(
                    "NotARegisteredWireType",
                    "NotARegisteredWireType.mode",
                    "Fast",
                ),
            ],
            Vec::new(),
        );
        let catalog = catalog_with_complete_a01();
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&catalog, &census, &mut violations);
        let uncovered = uncovered_field_violations(&violations);
        assert_eq!(
            uncovered.len(),
            2,
            "only the unregistered host's union and arm stay uncovered: {violations:?}"
        );
        assert!(
            uncovered
                .iter()
                .all(|violation| violation.msg.contains("NotARegisteredWireType")),
            "the targeted wire envelope covers its interior unions and arms: {violations:?}"
        );
    }

    #[test]
    fn flat_census_field_still_requires_a_field_target() {
        let census = census_with_slice(
            "a01",
            vec![field_candidate(
                "FixtureState",
                "FixtureState.plain_value",
                "plain_value",
            )],
            Vec::new(),
        );
        let mut catalog = catalog_with_complete_a01();
        catalog.targets.push(arm_target(
            "a01",
            "FixtureState",
            "FixtureState.phase",
            "Started",
        ));
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&catalog, &census, &mut violations);
        assert_eq!(
            uncovered_field_violations(&violations).len(),
            1,
            "a flat census field is never arm/wire-covered and still requires a field target: {violations:?}"
        );
    }

    #[test]
    fn identity_collapsed_arm_fields_fail_closed_per_uncovered_arm() {
        // The a01 collision shape: one (schema, stable_name) pair occurring
        // under two different arm paths.  Field-row identity cannot represent
        // both; each key must be covered by its own arm contract.
        let census = census_with_slice(
            "a01",
            vec![
                field_candidate(
                    "FixtureState",
                    "FixtureState.phase.Started.command_ref",
                    "command_ref",
                ),
                field_candidate(
                    "FixtureState",
                    "FixtureState.phase.Finished.command_ref",
                    "command_ref",
                ),
            ],
            Vec::new(),
        );
        let mut catalog = catalog_with_complete_a01();
        catalog.targets.push(arm_target(
            "a01",
            "FixtureState",
            "FixtureState.phase",
            "Started",
        ));
        let mut violations = Vec::new();
        verify_complete_field_census_coverage(&catalog, &census, &mut violations);
        let uncovered = uncovered_field_violations(&violations);
        assert_eq!(
            uncovered.len(),
            1,
            "each collapsed key needs its own covering arm: {violations:?}"
        );
        assert!(
            uncovered[0].msg.contains("Finished"),
            "the covered arm must be the targeted one: {violations:?}"
        );
    }

    #[test]
    fn adjudication_projection_accepts_arm_covered_keys_and_rejects_not_a_durable_over_them() {
        let arm_key = "field|FixtureState|FixtureState.phase.Started.begun_ref|begun_ref";
        let candidate = ambiguity(AmbiguityKind::UnownedStructuralFragment, &[arm_key]);
        let census = census_with_slice(
            "a01",
            vec![field_candidate(
                "FixtureState",
                "FixtureState.phase.Started.begun_ref",
                "begun_ref",
            )],
            vec![candidate.clone()],
        );
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        catalog.ambiguity_adjudications.clear();
        catalog.targets.push(arm_target(
            "a01",
            "FixtureState",
            "FixtureState.phase",
            "Started",
        ));
        let adjudication = AmbiguityAdjudication {
            row_id: "a01:ambiguity-adjudication:fixture".to_owned(),
            slice_id: "a01".to_owned(),
            ambiguity_source_key: candidate.key.source_key(),
            source_locations: Vec::new(),
            resolution: "maps-to-source".to_owned(),
            resolved_source_keys: vec![arm_key.to_owned()],
            rationale: "fixture".to_owned(),
        };
        catalog.ambiguity_adjudications.push(adjudication.clone());
        let mut violations = Vec::new();
        verify_ambiguity_adjudications(&catalog, &census, &mut violations);
        assert!(
            !violations.iter().any(
                |violation| violation.code == "source_ambiguity_resolution_projection_mismatch"
            ),
            "maps-to-source over an arm-covered key is projected through the arm contract: {violations:?}"
        );

        catalog.ambiguity_adjudications.clear();
        let mut contradictory = adjudication;
        contradictory.resolution = "not-a-durable-schema".to_owned();
        catalog.ambiguity_adjudications.push(contradictory);
        let mut violations = Vec::new();
        verify_ambiguity_adjudications(&catalog, &census, &mut violations);
        assert!(
            violations.iter().any(
                |violation| violation.code == "source_ambiguity_resolution_projection_mismatch"
            ),
            "not-a-durable-schema over an arm-covered key is contradictory: {violations:?}"
        );
    }

    #[test]
    fn per_formal_expansion_binding_is_exact_and_source_derived() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let row_id = "a19:expansion-binding:logical-kind-recovery-bridge-spec-parameter-1-role";
        let rationale = "Appendix source instantiates exactly Local and Meta";
        catalog.expansion_bindings.push(ExpansionBinding {
            row_id: row_id.to_owned(),
            target_row_id: "a19:logical-kind:recovery-bridge-spec".to_owned(),
            parameter_ordinal: 1,
            formal: "Role".to_owned(),
            formal_class: "role".to_owned(),
            values: vec!["Local".to_owned(), "Meta".to_owned()],
            rationale: rationale.to_owned(),
        });
        let annotation = Annotation {
            row_id: "a19:annotation:logical-kind-recovery-bridge-spec".to_owned(),
            target_row_id: "a19:logical-kind:recovery-bridge-spec".to_owned(),
            exact_type: "RecoveryBridgeSpec".to_owned(),
            cardinality: "one".to_owned(),
            layout: "canonical".to_owned(),
            role: "Local".to_owned(),
            posture: "recovery".to_owned(),
            authority: "recovery".to_owned(),
            locality: "local".to_owned(),
            generic_expansions: Vec::new(),
            role_expansions: vec!["Local".to_owned(), "Meta".to_owned()],
            reference_semantics: "embedded".to_owned(),
            target_schema_ids: Vec::new(),
            construction_order: "source-before-bridge".to_owned(),
            retention_and_cut_rule: "retain-through-recovery".to_owned(),
            digest_recipe: "canonical-fields".to_owned(),
            redaction_class: "authority-metadata".to_owned(),
            resource_bounds: "bounded-by-source-manifest".to_owned(),
            compatibility: "v1".to_owned(),
        };
        let contract = [ExpansionBindingContractPin {
            row_id,
            target_row_id: "a19:logical-kind:recovery-bridge-spec",
            target_source_key: "top|RecoveryBridgeSpec<Role>",
            parameter_ordinal: 1,
            formal: "Role",
            formal_class: "role",
            values: &["Local", "Meta"],
            rationale,
        }];
        let schemas = vec![schema("<Role>"), schema("<Local>"), schema("<Meta>")];
        assert!(top_level_annotation_expansions_match_with(
            &contract,
            &catalog,
            &annotation,
            &schemas[0],
            &schemas,
        ));

        catalog.expansion_bindings[0].values = vec!["Local".to_owned(), "Shard".to_owned()];
        assert!(
            !top_level_annotation_expansions_match_with(
                &contract,
                &catalog,
                &annotation,
                &schemas[0],
                &schemas,
            ),
            "cross-formal or arbitrary expansion values must not self-authorize"
        );
    }

    #[test]
    fn expansion_dimensions_distinguish_bounds_from_concrete_source_values() {
        let bounded =
            expansion_dimensions("<Role:AuthorityOwningRole>", ["<Role:AuthorityOwningRole>"])
                .expect("bounded formal is a supported source signature");
        assert_eq!(
            bounded,
            vec![ExpansionDimension {
                parameter_ordinal: 1,
                explicit_formal: Some("Role".to_owned()),
                source_values: BTreeSet::new(),
            }],
            "a trait bound is not a concrete expansion value"
        );

        let arbitrary_bounded = expansion_dimensions("<Scope:Trait>", ["<Scope:Trait>"])
            .expect("a bound explicitly declares its formal name");
        assert_eq!(
            arbitrary_bounded,
            vec![ExpansionDimension {
                parameter_ordinal: 1,
                explicit_formal: Some("Scope".to_owned()),
                source_values: BTreeSet::new(),
            }],
            "bound formals must not depend on the conventional short-name vocabulary"
        );

        let constrained =
            expansion_dimensions("<Role:Local|Meta|Shard>", ["<Role:Local|Meta|Shard>"])
                .expect("closed role alternatives are a supported source signature");
        assert_eq!(
            constrained,
            vec![ExpansionDimension {
                parameter_ordinal: 1,
                explicit_formal: Some("Role".to_owned()),
                source_values: ["Local", "Meta", "Shard"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
            "closed alternatives after a formal are concrete source values"
        );

        let arbitrary_constrained = expansion_dimensions("<Scope:A|B>", ["<Scope:A|B>"])
            .expect("an arbitrary formal may have closed concrete alternatives");
        assert_eq!(
            arbitrary_constrained,
            vec![ExpansionDimension {
                parameter_ordinal: 1,
                explicit_formal: Some("Scope".to_owned()),
                source_values: ["A", "B"].into_iter().map(str::to_owned).collect(),
            }]
        );

        let concrete = expansion_dimensions("<Local>", ["<Local>", "<Meta>", "<Shard>"])
            .expect("concrete-only family is a supported source signature");
        assert_eq!(
            concrete,
            vec![ExpansionDimension {
                parameter_ordinal: 1,
                explicit_formal: None,
                source_values: ["Local", "Meta", "Shard"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }]
        );

        let anchored_on_concrete = expansion_dimensions("<Local>", ["<Role>", "<Local>", "<Meta>"])
            .expect("a concrete anchor inherits the family formal");
        assert_eq!(
            anchored_on_concrete,
            vec![ExpansionDimension {
                parameter_ordinal: 1,
                explicit_formal: Some("Role".to_owned()),
                source_values: ["Local", "Meta"].into_iter().map(str::to_owned).collect(),
            }],
            "binding identity must come from the whole source family, not the selected occurrence"
        );
    }

    #[test]
    fn parameter_ordinals_disambiguate_identical_concrete_only_dimensions() {
        let dimensions = expansion_dimensions("<Local,Local>", ["<Local,Local>", "<Meta,Meta>"])
            .expect("repeated concrete-only dimensions are supported");
        assert_eq!(
            dimensions
                .iter()
                .map(|dimension| dimension.parameter_ordinal)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            dimensions[0].source_values, dimensions[1].source_values,
            "the regression requires value-identical anonymous dimensions"
        );

        let mut bindings = vec![
            ExpansionBinding {
                row_id: "a19:expansion-binding:logical-kind-recovery-bridge-spec-parameter-1-role"
                    .to_owned(),
                target_row_id: "a19:logical-kind:recovery-bridge-spec".to_owned(),
                parameter_ordinal: 1,
                formal: "Role".to_owned(),
                formal_class: "role".to_owned(),
                values: vec!["Local".to_owned(), "Meta".to_owned()],
                rationale: "The first source parameter has two concrete roles".to_owned(),
            },
            ExpansionBinding {
                row_id: "a19:expansion-binding:logical-kind-recovery-bridge-spec-parameter-2-role"
                    .to_owned(),
                target_row_id: "a19:logical-kind:recovery-bridge-spec".to_owned(),
                parameter_ordinal: 2,
                formal: "Role".to_owned(),
                formal_class: "role".to_owned(),
                values: vec!["Local".to_owned(), "Meta".to_owned()],
                rationale: "The second source parameter has the same two concrete roles".to_owned(),
            },
        ];
        let binding_refs: Vec<_> = bindings.iter().collect();
        assert!(
            expansion_bindings_match_dimensions(&binding_refs, &dimensions),
            "source position must make equal anonymous dimensions inhabitable"
        );

        bindings[1].parameter_ordinal = 1;
        let duplicate_ordinal_refs: Vec<_> = bindings.iter().collect();
        assert!(
            !expansion_bindings_match_dimensions(&duplicate_ordinal_refs, &dimensions),
            "one source parameter ordinal cannot discharge two dimensions"
        );

        bindings[1].parameter_ordinal = 2;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let target_row_id = "a19:logical-kind:recovery-bridge-spec";
        let mut target = catalog
            .targets
            .iter()
            .find(|target| target.target_row_id == target_row_id)
            .cloned()
            .expect("RecoveryBridgeSpec target");
        let mut selected = catalog
            .top_level_candidates
            .iter()
            .find(|candidate| candidate.source_key == target.source_key)
            .cloned()
            .expect("RecoveryBridgeSpec source candidate");
        selected.generic_signature = "<Local,Local>".to_owned();
        selected.source_key = "top|RecoveryBridgeSpec<Local,Local>".to_owned();
        target.source_key.clone_from(&selected.source_key);
        let mut peer = selected.clone();
        peer.row_id.push_str("-meta-meta");
        peer.generic_signature = "<Meta,Meta>".to_owned();
        peer.source_key = "top|RecoveryBridgeSpec<Meta,Meta>".to_owned();
        catalog.targets = vec![target];
        catalog.top_level_candidates = vec![selected, peer];
        catalog.expansion_bindings = bindings;

        let projection_targets =
            BTreeMap::from([(target_row_id.to_owned(), "logical-kind".to_owned())]);
        let candidate_by_key: BTreeMap<&str, &TopLevelCandidate> = catalog
            .top_level_candidates
            .iter()
            .map(|candidate| (candidate.source_key.as_str(), candidate))
            .collect();
        let mut all_row_ids = BTreeSet::new();
        let mut violations = Vec::new();
        validate_expansion_binding_rows(
            &catalog,
            &projection_targets,
            &candidate_by_key,
            &mut all_row_ids,
            &mut violations,
        );
        assert!(
            violations.is_empty(),
            "ordinal-distinguished anonymous dimensions must validate end to end: {violations:?}"
        );
    }

    #[test]
    fn expansion_row_validation_accepts_an_arbitrary_bound_formal() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let target_row_id = "a19:logical-kind:recovery-bridge-spec";
        let mut target = catalog
            .targets
            .iter()
            .find(|target| target.target_row_id == target_row_id)
            .cloned()
            .expect("RecoveryBridgeSpec target");
        let mut candidate = catalog
            .top_level_candidates
            .iter()
            .find(|candidate| candidate.source_key == target.source_key)
            .cloned()
            .expect("RecoveryBridgeSpec source candidate");
        let source_key = "top|RecoveryBridgeSpec<Scope:Trait>".to_owned();
        target.source_key.clone_from(&source_key);
        candidate.source_key = source_key;
        candidate.generic_signature = "<Scope:Trait>".to_owned();
        catalog.targets = vec![target];
        catalog.top_level_candidates = vec![candidate];
        catalog.expansion_bindings = vec![ExpansionBinding {
            row_id: "a19:expansion-binding:logical-kind-recovery-bridge-spec-parameter-1-scope"
                .to_owned(),
            target_row_id: target_row_id.to_owned(),
            parameter_ordinal: 1,
            formal: "Scope".to_owned(),
            formal_class: "generic".to_owned(),
            values: vec!["Local".to_owned()],
            rationale: "Appendix source binds the Scope formal to Local".to_owned(),
        }];

        let projection_targets =
            BTreeMap::from([(target_row_id.to_owned(), "logical_object_kinds".to_owned())]);
        let candidate_by_key: BTreeMap<&str, &TopLevelCandidate> = catalog
            .top_level_candidates
            .iter()
            .map(|candidate| (candidate.source_key.as_str(), candidate))
            .collect();
        let mut all_row_ids = BTreeSet::new();
        let mut violations = Vec::new();
        validate_expansion_binding_rows(
            &catalog,
            &projection_targets,
            &candidate_by_key,
            &mut all_row_ids,
            &mut violations,
        );
        assert!(
            violations.is_empty(),
            "an explicit arbitrary bound formal must validate end to end: {violations:?}"
        );
    }

    #[test]
    fn annotation_formals_follow_the_whole_family_for_a_concrete_anchor() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let target_row_id = "a19:logical-kind:recovery-bridge-spec";
        let mut target = catalog
            .targets
            .iter()
            .find(|target| target.target_row_id == target_row_id)
            .cloned()
            .expect("RecoveryBridgeSpec target");
        let mut selected = catalog
            .top_level_candidates
            .iter()
            .find(|candidate| candidate.source_key == target.source_key)
            .cloned()
            .expect("RecoveryBridgeSpec source candidate");
        selected.generic_signature = "<Local>".to_owned();
        selected.source_key = "top|RecoveryBridgeSpec<Local>".to_owned();
        target.source_key.clone_from(&selected.source_key);
        let mut formal_peer = selected.clone();
        formal_peer.row_id.push_str("-scope-formal");
        formal_peer.generic_signature = "<Scope:Trait>".to_owned();
        formal_peer.source_key = "top|RecoveryBridgeSpec<Scope:Trait>".to_owned();
        let candidates = vec![selected, formal_peer];
        let targets = BTreeMap::from([(target.target_row_id.as_str(), &target)]);
        let candidates_by_key: BTreeMap<&str, &TopLevelCandidate> = candidates
            .iter()
            .map(|candidate| (candidate.source_key.as_str(), candidate))
            .collect();
        let annotation = Annotation {
            row_id: "a19:annotation:logical-kind-recovery-bridge-spec".to_owned(),
            target_row_id: target_row_id.to_owned(),
            exact_type: "RecoveryBridgeSpec<Local>".to_owned(),
            cardinality: "one".to_owned(),
            layout: "canonical".to_owned(),
            role: "Scope".to_owned(),
            posture: "recovery".to_owned(),
            authority: "recovery".to_owned(),
            locality: "local".to_owned(),
            generic_expansions: vec!["Local".to_owned()],
            role_expansions: Vec::new(),
            reference_semantics: "embedded".to_owned(),
            target_schema_ids: Vec::new(),
            construction_order: "source-before-bridge".to_owned(),
            retention_and_cut_rule: "retain-through-recovery".to_owned(),
            digest_recipe: "canonical-fields".to_owned(),
            redaction_class: "authority-metadata".to_owned(),
            resource_bounds: "bounded-by-source-manifest".to_owned(),
            compatibility: "v1".to_owned(),
        };

        let formals =
            annotation_generic_formals(&annotation, &targets, &candidates_by_key, &candidates);
        assert!(
            formals.contains(annotation.role.as_str()),
            "a concrete selected occurrence must not hide a bound formal present in its source family"
        );
    }

    #[test]
    fn approved_expansion_coverage_crosses_source_slices_through_one_target() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let target_row_id = "a19:logical-kind:recovery-bridge-spec";
        let target_source_key = "top|RecoveryBridgeSpec<Role>";
        let expanded_source_key = "top|RecoveryBridgeSpec<Local|Meta>";
        catalog
            .top_level_candidates
            .iter_mut()
            .find(|candidate| candidate.source_key == expanded_source_key)
            .expect("released concrete RecoveryBridgeSpec occurrence exists")
            .slice_id = "a07".to_owned();
        let row_id = "a19:expansion-binding:logical-kind-recovery-bridge-spec-parameter-1-role";
        let rationale = "Appendix source instantiates exactly Local and Meta";
        catalog.expansion_bindings.push(ExpansionBinding {
            row_id: row_id.to_owned(),
            target_row_id: target_row_id.to_owned(),
            parameter_ordinal: 1,
            formal: "Role".to_owned(),
            formal_class: "role".to_owned(),
            values: vec!["Local".to_owned(), "Meta".to_owned()],
            rationale: rationale.to_owned(),
        });
        let contract = [ExpansionBindingContractPin {
            row_id,
            target_row_id,
            target_source_key,
            parameter_ordinal: 1,
            formal: "Role",
            formal_class: "role",
            values: &["Local", "Meta"],
            rationale,
        }];

        let coverage = approved_top_level_source_coverage_with(&contract, &catalog);
        for source_key in [target_source_key, expanded_source_key] {
            assert_eq!(
                coverage
                    .get(source_key)
                    .map(|target| target.target_row_id.as_str()),
                Some(target_row_id),
                "one independently pinned target must cover every exact family occurrence"
            );
        }
        let (a07_keys, a07_targets) = top_level_coverage_for_slice(&catalog, &coverage, "a07");
        // a07 owns landed top-level targets of its own, so the completing slice's
        // key set is its real coverage PLUS the reassigned occurrence. Derived from
        // the catalog rather than assumed empty, so this stays exact as a07 grows.
        let mut expected_a07_keys: Vec<&str> = catalog
            .targets
            .iter()
            .filter(|target| target.slice_id == "a07" && target.source_key.starts_with("top|"))
            .map(|target| target.source_key.as_str())
            .collect();
        expected_a07_keys.push(expanded_source_key);
        expected_a07_keys.sort_unstable();
        expected_a07_keys.dedup();
        assert_eq!(
            a07_keys, expected_a07_keys,
            "the completing source slice must count the exact cross-slice occurrence"
        );
        let mut expected_a07_targets: Vec<&str> = catalog
            .targets
            .iter()
            .filter(|target| target.slice_id == "a07" && target.source_key.starts_with("top|"))
            .map(|target| target.target_row_id.as_str())
            .collect();
        expected_a07_targets.push(target_row_id);
        expected_a07_targets.sort_unstable();
        expected_a07_targets.dedup();
        assert_eq!(
            a07_targets.keys().copied().collect::<Vec<_>>(),
            expected_a07_targets
        );

        let unapproved = approved_top_level_source_coverage_with(&[], &catalog);
        assert!(unapproved.contains_key(target_source_key));
        assert!(!unapproved.contains_key(expanded_source_key));

        catalog
            .top_level_candidates
            .iter_mut()
            .find(|candidate| candidate.source_key == expanded_source_key)
            .expect("released concrete RecoveryBridgeSpec occurrence exists")
            .identity_class = "physical".to_owned();
        let mixed_class = approved_top_level_source_coverage_with(&contract, &catalog);
        assert!(mixed_class.contains_key(target_source_key));
        assert!(!mixed_class.contains_key(expanded_source_key));

        catalog
            .top_level_candidates
            .iter_mut()
            .find(|candidate| candidate.source_key == expanded_source_key)
            .expect("released concrete RecoveryBridgeSpec occurrence exists")
            .identity_class = "logical".to_owned();
        let shadow_target_row_id = "a07:logical-kind:recovery-bridge-spec-shadow";
        catalog.targets.push(Target {
            row_id: "a07:target:logical-kind-recovery-bridge-spec-shadow".to_owned(),
            target_row_id: shadow_target_row_id.to_owned(),
            slice_id: "a07".to_owned(),
            source_key: expanded_source_key.to_owned(),
            target_kind: "logical-kind".to_owned(),
            definition_status: "complete".to_owned(),
        });
        let shadow_row_id =
            "a07:expansion-binding:logical-kind-recovery-bridge-spec-shadow-parameter-1-role";
        catalog.expansion_bindings.push(ExpansionBinding {
            row_id: shadow_row_id.to_owned(),
            target_row_id: shadow_target_row_id.to_owned(),
            parameter_ordinal: 1,
            formal: "Role".to_owned(),
            formal_class: "role".to_owned(),
            values: vec!["Local".to_owned(), "Meta".to_owned()],
            rationale: rationale.to_owned(),
        });
        let duplicate_contract = [
            contract[0],
            ExpansionBindingContractPin {
                row_id: shadow_row_id,
                target_row_id: shadow_target_row_id,
                target_source_key: expanded_source_key,
                parameter_ordinal: 1,
                formal: "Role",
                formal_class: "role",
                values: &["Local", "Meta"],
                rationale,
            },
        ];
        let duplicate = approved_top_level_source_coverage_with(&duplicate_contract, &catalog);
        assert!(!duplicate.contains_key(target_source_key));
        assert!(!duplicate.contains_key(expanded_source_key));
    }

    #[test]
    fn final_ambiguity_resolution_requires_the_exact_parser_owned_relation() {
        let source_key = "field|Record|Record.value|value";
        let candidate = ambiguity(AmbiguityKind::FieldTypeAmbiguous, &[source_key]);
        let mut row = AmbiguityAdjudication {
            row_id: "a02:ambiguity-adjudication:fixture".to_owned(),
            slice_id: "a02".to_owned(),
            ambiguity_source_key: candidate.key.source_key(),
            source_locations: vec!["a02:1".to_owned()],
            resolution: "maps-to-source".to_owned(),
            resolved_source_keys: vec![source_key.to_owned()],
            rationale: "The parser identified this exact field candidate".to_owned(),
        };
        assert!(final_ambiguity_resolution_matches(&row, &candidate));

        row.resolved_source_keys = vec!["field|Record|Record.other|other".to_owned()];
        assert!(
            !final_ambiguity_resolution_matches(&row, &candidate),
            "same-family but unrelated source keys must not discharge an ambiguity"
        );

        let ownerless = ambiguity(AmbiguityKind::UnownedStructuralFragment, &[]);
        row.resolution = "not-a-durable-schema".to_owned();
        row.resolved_source_keys.clear();
        assert!(final_ambiguity_resolution_matches(&row, &ownerless));

        let lexical = ambiguity(AmbiguityKind::UnterminatedInlineCode, &[]);
        assert!(
            !final_ambiguity_resolution_matches(&row, &lexical),
            "lexically unterminated source must remain a parser/source repair instead of closing as an empty rejection"
        );
    }

    #[test]
    fn nonzero_raw_ambiguity_pin_closes_only_with_exact_final_adjudication() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let source_key = "ambiguity|ambiguous-schema-owner|Sharded||0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef|fixture";
        let row_id = "a20:ambiguity-adjudication:fe317e2f4f78c1a778d4bb278a220758595a0e3de1ebf15174148546ff93f13c";
        let rationale = "Sharded is the target_posture union arm, not a schema";
        catalog.ambiguity_adjudications.push(AmbiguityAdjudication {
            row_id: row_id.to_owned(),
            slice_id: "a20".to_owned(),
            ambiguity_source_key: source_key.to_owned(),
            source_locations: vec!["a20:2575".to_owned()],
            resolution: "not-a-durable-schema".to_owned(),
            resolved_source_keys: vec!["top|Sharded".to_owned()],
            rationale: rationale.to_owned(),
        });
        let pin = [AmbiguityAdjudicationContractPin {
            row_id,
            slice_id: "a20",
            ambiguity_source_key: source_key,
            source_locations: &["a20:2575"],
            resolution: "not-a-durable-schema",
            resolved_source_keys: &["top|Sharded"],
            rationale,
        }];
        let keys = approved_final_ambiguity_keys_with(&pin, &catalog, "a20");
        let raw_count = 1;
        let raw_sha256 = sha256_hex(format!("{source_key}\n").as_bytes());
        let mut violations = Vec::new();
        validate_census_pin(
            "a20",
            "complete_ambiguity_adjudication",
            raw_count,
            &raw_sha256,
            keys,
            &mut violations,
        );
        assert!(
            violations.is_empty(),
            "nonzero raw ambiguity pin did not close with its exact final adjudication: {violations:?}"
        );

        catalog
            .ambiguity_adjudications
            .last_mut()
            .expect("synthetic adjudication was pushed above")
            .resolution = "needs-parser-fix".to_owned();
        let keys = approved_final_ambiguity_keys_with(&pin, &catalog, "a20");
        let mut violations = Vec::new();
        validate_census_pin(
            "a20",
            "complete_ambiguity_adjudication",
            raw_count,
            &raw_sha256,
            keys,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "slice_census_pin_mismatch"),
            "nonfinal ambiguity state incorrectly counted as resolved"
        );
    }

    #[test]
    fn readable_annotation_contract_exercises_nonempty_reciprocal_paths() {
        let catalog = catalog_with_annotation();
        let contract = [annotation_pin()];
        let mut violations = Vec::new();
        validate_readable_annotation_contract_with(
            &catalog,
            &contract,
            contract.len(),
            &mut violations,
        );
        assert!(
            violations.is_empty(),
            "exact readable annotation failed: {violations:?}"
        );
        assert_eq!(
            approved_annotation_counts_with(&catalog, &contract).get(TARGET_ROW_ID),
            Some(&1)
        );
        assert!(
            approved_annotation_counts_with(&catalog, &[]).is_empty(),
            "unapproved annotations must not satisfy complete-slice counts"
        );
    }

    #[test]
    fn readable_annotation_contract_rejects_mismatch_missing_duplicate_and_count_drift() {
        let mut catalog = catalog_with_annotation();
        let contract = [annotation_pin()];

        catalog.annotations[0].posture = "durable".to_owned();
        let mut violations = Vec::new();
        validate_readable_annotation_contract_with(
            &catalog,
            &contract,
            contract.len(),
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| { violation.code == "catalog_annotation_contract_mismatch" })
        );

        catalog.annotations.clear();
        let duplicate = [annotation_pin(), annotation_pin()];
        let mut violations = Vec::new();
        validate_readable_annotation_contract_with(
            &catalog,
            &duplicate,
            duplicate.len(),
            &mut violations,
        );
        for expected in [
            "catalog_annotation_contract_ambiguous",
            "catalog_annotation_contract_missing",
        ] {
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.code == expected),
                "missing annotation reciprocal branch {expected}: {violations:?}"
            );
        }

        let mut violations = Vec::new();
        validate_readable_annotation_contract_with(&catalog, &contract, 0, &mut violations);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "catalog_annotation_contract_pin_inconsistent"
            })
        );
    }

    #[test]
    fn readable_binding_contract_exercises_nonempty_reciprocal_paths() {
        let catalog = catalog_with_bindings();
        let semantic = [semantic_pin()];
        let evidence = [static_evidence_pin(), runtime_evidence_pin()];
        let mut violations = Vec::new();
        validate_readable_binding_contract_with(
            &catalog,
            &semantic,
            &evidence,
            semantic.len(),
            evidence.len(),
            &mut violations,
        );
        assert!(
            violations.is_empty(),
            "exact readable reciprocal bindings failed: {violations:?}"
        );

        let counts = approved_binding_counts_with(&catalog, &semantic, &evidence);
        assert_eq!(counts.semantic.get(TARGET_ROW_ID), Some(&1));
        assert_eq!(counts.static_live.get(TARGET_ROW_ID), Some(&1));
        assert_eq!(counts.runtime.get(TARGET_ROW_ID), Some(&1));
        assert_eq!(
            approved_binding_counts_with(&catalog, &[], &[]),
            ApprovedBindingCounts::default(),
            "unapproved rows must not satisfy complete-slice counts"
        );
    }

    #[test]
    fn readable_binding_contract_rejects_mismatch_missing_duplicate_and_count_drift() {
        let mut catalog = catalog_with_bindings();
        let semantic = [semantic_pin()];
        let evidence = [static_evidence_pin(), runtime_evidence_pin()];

        catalog.semantic_bindings[0].owner_crate = "fgdb-warden".to_owned();
        catalog.evidence[0].event_ids = vec!["appendix_source_manifest".to_owned()];
        let mut violations = Vec::new();
        validate_readable_binding_contract_with(
            &catalog,
            &semantic,
            &evidence,
            semantic.len(),
            evidence.len(),
            &mut violations,
        );
        assert!(
            violations.iter().any(|violation| {
                violation.code == "catalog_semantic_binding_contract_mismatch"
            })
        );
        assert!(
            violations.iter().any(|violation| {
                violation.code == "catalog_evidence_binding_contract_mismatch"
            })
        );

        catalog.semantic_bindings.clear();
        let duplicate_semantic = [semantic_pin(), semantic_pin()];
        let mut violations = Vec::new();
        validate_readable_binding_contract_with(
            &catalog,
            &duplicate_semantic,
            &evidence,
            duplicate_semantic.len(),
            evidence.len(),
            &mut violations,
        );
        for expected in [
            "catalog_semantic_binding_contract_ambiguous",
            "catalog_semantic_binding_contract_missing",
        ] {
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.code == expected),
                "missing reciprocal branch {expected}: {violations:?}"
            );
        }

        let mut violations = Vec::new();
        validate_readable_binding_contract_with(
            &catalog,
            &semantic,
            &evidence,
            0,
            evidence.len(),
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| { violation.code == "catalog_binding_contract_pin_inconsistent" })
        );
    }

    #[test]
    fn live_repository_bindings_require_existing_checker_artifacts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let workspace = workspace_package_names(&root).expect("workspace packages resolve");
        assert!(workspace.contains("fgdb-types"));
        assert!(
            !workspace.contains("fgdb-warden"),
            "planned crates must not masquerade as present implementation owners"
        );
        let checkers = load_appendix_checker_index(&root).expect("checker index loads");
        let checker_by_id: BTreeMap<&str, &model::Checker> = checkers
            .iter()
            .map(|checker| (checker.symbol.as_str(), checker))
            .collect();
        let root_without_artifacts = root.join("registries");

        let mut violations = Vec::new();
        let prover = crate::liveness::Prover::new(&root_without_artifacts);
        validate_scenario_registry(&prover, &checker_by_id, &catalog, &mut violations);
        assert!(
            violations
                .iter()
                .any(|violation| { violation.code == "catalog_scenario_checker_artifact_missing" }),
            "a live scenario checker with no artifact was accepted: {violations:?}"
        );

        let checker_ids = vec!["appendix_a_catalog_closure".to_owned()];
        let mut violations = Vec::new();
        validate_checker_bindings(
            &prover,
            "fixture",
            "live",
            &checker_ids,
            CheckerBindingCodes {
                unresolved: "unresolved",
                not_live: "not_live",
                artifact_missing: "artifact_missing",
            },
            &checker_by_id,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "artifact_missing"),
            "live evidence accepted a missing checker artifact: {violations:?}"
        );

        let mut tampered_checkers = checkers.clone();
        tampered_checkers
            .iter_mut()
            .find(|checker| checker.symbol == "appendix_a_catalog_source")
            .expect("Appendix source checker")
            .artifact = "Cargo.toml".to_owned();
        let tampered_by_id: BTreeMap<&str, &model::Checker> = tampered_checkers
            .iter()
            .map(|checker| (checker.symbol.as_str(), checker))
            .collect();
        let mut violations = Vec::new();
        validate_maintenance_checker_registry(&tampered_by_id, &mut violations);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "catalog_maintenance_checker_registry_drift"
            }),
            "a maintenance checker was rebound to an unrelated existing artifact: {violations:?}"
        );
    }

    #[test]
    fn a04_manifest_raft_owner_shells_are_exact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let expected = [
            (
                "CertificateAttemptPlan",
                "certificate-attempt-plan",
                "top|CertificateAttemptPlan",
                0x0265,
                10,
                "true",
                16_777_216,
                true,
            ),
            (
                "CertificateSignerLock",
                "certificate-signer-lock",
                "top|CertificateSignerLock",
                0x0269,
                12,
                "true",
                16_777_216,
                true,
            ),
            (
                "CertificateSignatureShare",
                "certificate-signature-share",
                "top|CertificateSignatureShare",
                0x052b,
                14,
                "true",
                16_777_216,
                false,
            ),
            (
                "InitialProtocolStateRecipe",
                "initial-protocol-state-recipe",
                "top|InitialProtocolStateRecipe<Role>",
                0x02d7,
                30,
                "true",
                16_777_216,
                true,
            ),
            (
                "RaftConsensusCutProjection",
                "raft-consensus-cut-projection",
                "top|RaftConsensusCutProjection<Role>",
                0x03a1,
                40,
                "true",
                16_777_216,
                true,
            ),
            (
                "RaftHardState",
                "raft-hard-state",
                "top|RaftHardState",
                0x03a2,
                40,
                "true",
                16_777_216,
                true,
            ),
            (
                "RaftLogSegment",
                "raft-log-segment",
                "top|RaftLogSegment",
                0x052a,
                40,
                "true",
                1_073_741_824,
                false,
            ),
            (
                "RaftSnapshot",
                "raft-snapshot",
                "top|RaftSnapshot",
                0x03a3,
                40,
                "true",
                1_073_741_824,
                true,
            ),
            (
                "RaftStateRoot",
                "raft-state-root",
                "top|RaftStateRoot<Role>",
                0x03a4,
                40,
                "true",
                16_777_216,
                true,
            ),
            (
                "RemoteConfigurationTrustRoot",
                "remote-configuration-trust-root",
                "top|RemoteConfigurationTrustRoot",
                0x03ae,
                40,
                "true",
                16_777_216,
                true,
            ),
            (
                "RemoteRetentionConsumerRoot",
                "remote-retention-consumer-root",
                "top|RemoteRetentionConsumerRoot",
                0x03b6,
                40,
                "true",
                16_777_216,
                true,
            ),
            (
                "RemoteRetentionObligationRoot",
                "remote-retention-obligation-root",
                "top|RemoteRetentionObligationRoot",
                0x03bb,
                40,
                "true",
                16_777_216,
                true,
            ),
            (
                "RemoteTrustCompactionPrecondition",
                "remote-trust-compaction-precondition",
                "top|RemoteTrustCompactionPrecondition",
                0x03c2,
                40,
                "true",
                16_777_216,
                true,
            ),
            (
                "ShardProtocolState",
                "shard-protocol-state",
                "top|ShardProtocolState",
                0x046b,
                40,
                "role-shard",
                16_777_216,
                true,
            ),
            (
                "TopologyState",
                "topology-state",
                "top|TopologyState",
                0x04aa,
                30,
                "role-meta",
                16_777_216,
                true,
            ),
            (
                "ValidatedRemoteConfigurationAnchor",
                "validated-remote-configuration-anchor",
                "top|ValidatedRemoteConfigurationAnchor",
                0x04b7,
                // 40 -> 14 (fgdb-oicl): RemoteAuthorityConfigurationEvidence@14 carries
                // ValidatedCheckpointSuccessor{predecessor_anchor_ref:StrongRef<
                // ValidatedRemoteConfigurationAnchor>} (a04:1398), so the anchor is the
                // ceiling-bound target of an already-frozen referrer and 40 was above it.
                14,
                "true",
                16_777_216,
                true,
            ),
        ];

        for (name, slug, source_key, code, order, role, max_size, reserved_code) in expected {
            let logical = catalog
                .identity
                .logical
                .iter()
                .find(|logical| logical.name == name)
                .expect("a04 logical owner must exist");
            assert_eq!(logical.object_kind, code, "{name} code");
            assert_eq!(logical.status, "reserved", "{name} lifecycle");
            assert_eq!(
                logical.construction_order, order,
                "{name} construction order"
            );
            assert_eq!(logical.role_predicate, role, "{name} role");
            assert_eq!(logical.max_size_bytes, max_size, "{name} size ceiling");
            assert_eq!(
                logical.golden_corpus,
                format!("corpus/logical/{}/", slug.replace('-', "_")),
                "{name} corpus"
            );

            let candidates = catalog
                .top_level_candidates
                .iter()
                .filter(|candidate| candidate.source_key == source_key)
                .collect::<Vec<_>>();
            assert_eq!(candidates.len(), 1, "{name} source candidate is unique");
            assert_eq!(candidates[0].identity_class, "logical", "{name} class");

            let targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key == source_key)
                .collect::<Vec<_>>();
            assert_eq!(targets.len(), 1, "{name} source target is unique");
            assert_eq!(
                targets[0].row_id,
                format!("a04:target:logical-kind-{slug}"),
                "{name} target row"
            );
            assert_eq!(
                targets[0].target_row_id,
                format!("a04:logical-kind:{slug}"),
                "{name} target owner"
            );
            assert_eq!(targets[0].target_kind, "logical-kind", "{name} target kind");
            assert_eq!(
                targets[0].definition_status, "declared",
                "{name} definition status"
            );

            let reservations = catalog
                .reservations
                .iter()
                .filter(|reservation| reservation.symbol == name)
                .collect::<Vec<_>>();
            if reserved_code {
                assert_eq!(reservations.len(), 1, "{name} reservation is unique");
                assert_eq!(
                    reservations[0].row_id,
                    format!("a04:reservation:{slug}"),
                    "{name} reservation row"
                );
                assert_eq!(
                    reservations[0].code_reservation,
                    format!("0x{code:04x}"),
                    "{name} reserved code"
                );
                assert_eq!(
                    reservations[0].disposition, "existing",
                    "{name} reservation disposition"
                );
            } else {
                assert!(
                    reservations.is_empty(),
                    "{name} is a direct unreserved mint"
                );
            }
        }
    }

    #[test]
    fn a04_embedded_wire_owner_shells_are_exact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let expected = [
            (
                "AdvanceRemoteConfigurationEvidenceSpec",
                "advance-remote-configuration-evidence-spec",
                "top|AdvanceRemoteConfigurationEvidenceSpec",
                0x0068,
                "record",
                "canonical role-valid remote configuration evidence CAS spec record",
                &["AdvanceRemoteConfigurationEvidenceSpec"][..],
                16_777_216,
            ),
            (
                "AuthorityLocalCertificateHeader",
                "authority-local-certificate-header",
                "top|AuthorityLocalCertificateHeader",
                0x0069,
                "record",
                "canonical authority-local certificate transcript header",
                &["*"][..],
                16_777_216,
            ),
            (
                "CertificateAttemptAbandonSpec",
                "certificate-attempt-abandon-spec",
                "top|CertificateAttemptAbandonSpec",
                0x006a,
                "record",
                "canonical certificate-attempt abandonment spec record",
                &["CertificateAttemptAbandonSpec"][..],
                16_777_216,
            ),
            (
                "CertificateUnsignedBodyDigest",
                "certificate-unsigned-body-digest",
                "top|CertificateUnsignedBodyDigest<T>",
                0x006b,
                "record",
                "canonical domain-separated certificate unsigned-body digest family",
                &["*"][..],
                32,
            ),
            (
                "ImportedCertificateDomain",
                "imported-certificate-domain",
                "top|ImportedCertificateDomain",
                0x006c,
                "record",
                "canonical imported certificate authority-domain header",
                &["*"][..],
                16_777_216,
            ),
            (
                "RaftMaintenanceCommand",
                "raft-maintenance-command",
                "top|RaftMaintenanceCommand",
                0x006d,
                "union",
                "canonical role-tagged Raft maintenance command union",
                &["RaftMaintenanceCommand"][..],
                16_777_216,
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "top|RemoteRetentionControlSpec",
                0x006e,
                "union",
                "canonical role-valid remote-retention control union",
                &["RemoteRetentionControlSpec"][..],
                16_777_216,
            ),
            (
                "RetirementLeaseDescriptor",
                "retirement-lease-descriptor",
                "top|RetirementLeaseDescriptor",
                0x006f,
                "record",
                "canonical retired-local physical-generation lease descriptor",
                &["RootManifest"][..],
                16_777_216,
            ),
            (
                "ValidateRemoteConfigurationAnchorSpec",
                "validate-remote-configuration-anchor-spec",
                "top|ValidateRemoteConfigurationAnchorSpec",
                0x0070,
                "record",
                "canonical remote configuration anchor validation spec record",
                &["ValidateRemoteConfigurationAnchorSpec"][..],
                16_777_216,
            ),
            (
                "WeakAuthorityAppliedIdentity",
                "weak-authority-applied-identity",
                "top|WeakAuthorityAppliedIdentity",
                0x0071,
                "union",
                "canonical nonretaining authority applied-identity union",
                &["ValidatedRemoteConfigurationAnchor"][..],
                16_777_216,
            ),
        ];

        for (name, slug, source_key, code, kind, context, containers, max_size) in expected {
            let wire = catalog
                .identity
                .wire
                .iter()
                .find(|wire| wire.name.eq(name))
                .expect("a04 wire owner must exist");
            assert_eq!(wire.wire_type_id, code, "{name} code");
            assert_eq!(wire.kind, kind, "{name} kind");
            assert_eq!(wire.status, "reserved", "{name} lifecycle");
            assert_eq!(wire.encoding_context, context, "{name} encoding context");
            assert_eq!(
                wire.allowed_containing_schemas
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                containers,
                "{name} containing-schema closure"
            );
            assert_eq!(wire.max_size_bytes, max_size, "{name} size ceiling");
            assert_eq!(wire.containing_union, None, "{name} is an owner");
            assert_eq!(wire.wire_tag, None, "{name} has no variant tag");

            let candidates = catalog
                .top_level_candidates
                .iter()
                .filter(|candidate| candidate.source_key.eq(source_key))
                .collect::<Vec<_>>();
            assert_eq!(candidates.len(), 1, "{name} source candidate is unique");
            assert_eq!(candidates[0].identity_class, "wire", "{name} class");

            let targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key.eq(source_key))
                .collect::<Vec<_>>();
            assert_eq!(targets.len(), 1, "{name} source target is unique");
            assert_eq!(
                targets[0].row_id,
                format!("a04:target:wire-type-{slug}"),
                "{name} target row"
            );
            assert_eq!(
                targets[0].target_row_id,
                format!("a04:wire-type:{slug}"),
                "{name} target owner"
            );
            assert_eq!(targets[0].target_kind, "wire-type", "{name} target kind");
            assert_eq!(
                targets[0].definition_status, "declared",
                "{name} definition status"
            );
            assert!(
                !catalog
                    .reservations
                    .iter()
                    .any(|reservation| reservation.symbol.eq(name)),
                "{name} is a direct wire mint, not a logical reservation"
            );
        }
    }

    #[test]
    fn a04_embedded_wire_union_contracts_are_exact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let expected_unions = [
            (
                "RaftMaintenanceCommand",
                "a04:union:raft-maintenance-command-5db5de01fbae1e54",
                3,
            ),
            (
                "RemoteRetentionControlSpec",
                "a04:union:remote-retention-control-spec-2e20a44d05d16f80",
                8,
            ),
        ];
        for (owner, row_id, arm_count) in expected_unions {
            let unions = catalog
                .identity
                .ordinary_unions
                .iter()
                .filter(|union| {
                    union.union_name.eq(owner)
                        && union.containing_schema.eq(owner)
                        && union.union_path.eq(owner)
                })
                .collect::<Vec<_>>();
            assert_eq!(unions.len(), 1, "{owner} union is unique");
            let union = unions[0];
            assert_eq!(union.tag_wire_type, "u8", "{owner} tag type");
            assert_eq!(union.encoding_context, "closed-tagged", "{owner} encoding");
            assert_eq!(
                union
                    .allowed_containing_schemas
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                [owner],
                "{owner} containing-schema closure"
            );
            assert_eq!(union.role_predicate, "true", "{owner} role predicate");
            assert_eq!(union.version_status, "reserved", "{owner} lifecycle");
            assert_eq!(union.max_size_bytes, 16_777_216, "{owner} size ceiling");
            assert_eq!(union.arms.len(), arm_count, "{owner} arm closure");
            assert!(
                catalog
                    .projection_rows
                    .iter()
                    .any(|row| row.row_id.eq(row_id) && row.row_kind.eq("union"))
            );

            let source_key = format!("union|{owner}|{owner}");
            let targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key.eq(&source_key))
                .collect::<Vec<_>>();
            assert_eq!(targets.len(), 1, "{owner} union target is unique");
            assert_eq!(targets[0].target_row_id, row_id, "{owner} target owner");
            assert_eq!(targets[0].target_kind, "union", "{owner} target kind");
        }

        let expected_arms = [
            (
                "RaftMaintenanceCommand",
                "raft-maintenance-command",
                "Local",
                "local",
                0x0001,
                0x00fe,
                "88172dd60ba1cca1155af932fa43e982bd44b698ffc476327bca25be57cfde45",
                "a04:union-arm:raft-maintenance-command-local-166e0ce93e239283",
            ),
            (
                "RaftMaintenanceCommand",
                "raft-maintenance-command",
                "Meta",
                "meta",
                0x0002,
                0x00ff,
                "eeafae78b778f68cf3fc34d68663b35b7316e7c0c004eb5c11d982a1eef74637",
                "a04:union-arm:raft-maintenance-command-meta-a986852dd35ce52b",
            ),
            (
                "RaftMaintenanceCommand",
                "raft-maintenance-command",
                "Shard",
                "shard",
                0x0003,
                0x0100,
                "259d58930b54ee5f66887b101a2b467696d4c806190710d77e0f550e0c9188aa",
                "a04:union-arm:raft-maintenance-command-shard-6092bf9775cd5dfc",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "AcquireGrant",
                "acquire_grant",
                0x0001,
                0x0101,
                "3d2eca5b05c39a339a55544db4ef38c8627286e12bc46eedc20c7e2dc27bdfe8",
                "a04:union-arm:remote-retention-control-spec-acquire-grant-f3b09b90f1d2c6bb",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "RegisterConsumerGrant",
                "register_consumer_grant",
                0x0002,
                0x0102,
                "c3a9c04dc72c1b5714934dbe21da5265c3623af0fae006d27d60ee4755d38397",
                "a04:union-arm:remote-retention-control-spec-register-consumer-grant-154402bf251f51ae",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "RequestConsumerRelease",
                "request_consumer_release",
                0x0003,
                0x0103,
                "c47004ec1bc102c2efa2b24e1c0521030366f897f240645d9c5249e3b823cbbb",
                "a04:union-arm:remote-retention-control-spec-request-consumer-release-0699968f2848f0b1",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "PublishConsumerReleaseEvidence",
                "publish_consumer_release_evidence",
                0x0004,
                0x0104,
                "13fafe1d4a2ca66becac7f0e6f89afa0b956f291c2e3776728131c0d9ea95760",
                "a04:union-arm:remote-retention-control-spec-publish-consumer-release-evidence-86544bfa99c7cb1d",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "ApplyAuthorityRelease",
                "apply_authority_release",
                0x0005,
                0x0105,
                "3b71a528ee85ba804999da14c3d1bf4972dfd96d176449a7c8a07042fd417319",
                "a04:union-arm:remote-retention-control-spec-apply-authority-release-e95b41c059245ab6",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "PublishAuthorityReleaseAck",
                "publish_authority_release_ack",
                0x0006,
                0x0106,
                "168e14cf2b5827497239ef905be8e957dbd6ea100e3553899364c0a9089cfa28",
                "a04:union-arm:remote-retention-control-spec-publish-authority-release-ack-8a598a86d04eb5ff",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "ConsumeReleaseAck",
                "consume_release_ack",
                0x0007,
                0x0107,
                "e661b30eae0fbdd05b5d42fa408e9247196112736fbb3ddeff69c21fea3e0a84",
                "a04:union-arm:remote-retention-control-spec-consume-release-ack-9b078d8cbfd19eb7",
            ),
            (
                "RemoteRetentionControlSpec",
                "remote-retention-control-spec",
                "AdoptLegacyAuthorityTransfer",
                "adopt_legacy_authority_transfer",
                0x0008,
                0x0108,
                "4c46a602c0e8e0b7d889479e1e1653fd8100270a9d673acc3fde6ba13924664a",
                "a04:union-arm:remote-retention-control-spec-adopt-legacy-authority-transfer-c66ff0c93c82d6eb",
            ),
        ];
        for (owner, owner_slug, source_name, stable_name, tag, code, digest, arm_row_id) in
            expected_arms
        {
            let union = catalog
                .identity
                .ordinary_unions
                .iter()
                .find(|union| union.union_name.eq(owner))
                .expect("a04 ordinary union exists");
            let arm = union
                .arms
                .iter()
                .find(|arm| arm.source_arm_name.eq(source_name))
                .expect("a04 ordinary-union arm exists");
            assert_eq!(arm.arm_tag, tag, "{owner}.{source_name} tag");
            assert_eq!(arm.stable_name, stable_name, "{owner}.{source_name} name");
            assert_eq!(arm.payload_kind, "inline-record");
            assert_eq!(arm.payload_sha256.as_deref(), Some(digest));
            assert_eq!(arm.role_predicate, "true");
            assert_eq!(arm.version_status, "reserved");
            assert_eq!(arm.max_size_bytes, 16_777_216);
            assert!(
                catalog
                    .projection_rows
                    .iter()
                    .any(|row| row.row_id.eq(arm_row_id) && row.row_kind.eq("union-arm"))
            );

            let wire_name = format!("{owner}.{stable_name}");
            let variants = catalog
                .identity
                .wire
                .iter()
                .filter(|wire| wire.name.eq(&wire_name))
                .collect::<Vec<_>>();
            assert_eq!(variants.len(), 1, "{wire_name} variant is unique");
            let variant = variants[0];
            assert_eq!(variant.wire_type_id, code, "{wire_name} code");
            assert_eq!(variant.kind, "union_variant");
            assert_eq!(variant.containing_union.as_deref(), Some(owner));
            assert_eq!(variant.wire_tag, Some(tag));
            assert_eq!(variant.status, "reserved");
            assert_eq!(
                variant.encoding_context,
                format!("arm {source_name} of closed union {owner}")
            );
            assert_eq!(
                variant
                    .allowed_containing_schemas
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                [owner]
            );
            assert_eq!(variant.max_size_bytes, 16_777_216);

            let source_key = format!("arm|{owner}|{owner}|{source_name}");
            let targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key.eq(&source_key))
                .collect::<Vec<_>>();
            assert_eq!(targets.len(), 2, "{wire_name} has arm and wire targets");
            assert!(targets.iter().any(|target| {
                target.target_kind.eq("union-arm") && target.target_row_id.eq(arm_row_id)
            }));
            assert!(targets.iter().any(|target| {
                target.target_kind.eq("wire-type")
                    && target.target_row_id.eq(&format!(
                        "a04:wire-type:{owner_slug}-{}",
                        stable_name.replace('_', "-")
                    ))
            }));
        }
    }

    #[test]
    fn a04_weak_authority_applied_identity_union_is_exact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let owner = "WeakAuthorityAppliedIdentity";
        let owner_slug = "weak-authority-applied-identity";
        let union_row_id = "a04:union:weak-authority-applied-identity-8f52cac0fe558262";

        let wire_owners = catalog
            .identity
            .wire
            .iter()
            .filter(|wire| wire.name.eq(owner))
            .collect::<Vec<_>>();
        assert_eq!(wire_owners.len(), 1, "{owner} wire owner is unique");
        assert_eq!(
            wire_owners[0]
                .allowed_containing_schemas
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["ValidatedRemoteConfigurationAnchor"]
        );

        let unions = catalog
            .identity
            .ordinary_unions
            .iter()
            .filter(|union| {
                union.union_name.eq(owner)
                    && union.containing_schema.eq(owner)
                    && union.union_path.eq(owner)
            })
            .collect::<Vec<_>>();
        assert_eq!(unions.len(), 1, "{owner} union is unique");
        let union = unions[0];
        assert_eq!(union.tag_wire_type, "u8");
        assert_eq!(union.encoding_context, "closed-tagged");
        assert_eq!(
            union
                .allowed_containing_schemas
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["ValidatedRemoteConfigurationAnchor"]
        );
        assert_eq!(union.role_predicate, "true");
        assert_eq!(union.version_status, "reserved");
        assert_eq!(union.max_size_bytes, 16_777_216);
        assert_eq!(union.arms.len(), 3);
        assert!(
            catalog
                .projection_rows
                .iter()
                .any(|row| row.row_id.eq(union_row_id) && row.row_kind.eq("union"))
        );

        let union_source_key = format!("union|{owner}|{owner}");
        let union_targets = catalog
            .targets
            .iter()
            .filter(|target| target.source_key.eq(&union_source_key))
            .collect::<Vec<_>>();
        assert_eq!(union_targets.len(), 1, "{owner} union target is unique");
        assert_eq!(union_targets[0].target_row_id, union_row_id);
        assert_eq!(union_targets[0].target_kind, "union");

        let expected_arms = [
            (
                "Local",
                "local",
                0x0001,
                0x00e2,
                "aa2e8bed1584d985ed1707bcca5ba309adc7c194ef60244601d164e4a54e93f9",
                "a04:union-arm:weak-authority-applied-identity-local-9cfe069854acb028",
            ),
            (
                "Meta",
                "meta",
                0x0002,
                0x00e3,
                "d66d4a4c937cd5cc1e401ba3399b5044c4773a7692bae0ee04d9262b6d482865",
                "a04:union-arm:weak-authority-applied-identity-meta-6eccd808689854c6",
            ),
            (
                "Shard",
                "shard",
                0x0003,
                0x00e4,
                "1126f8d358d2d7cac029d698aa0e66457118484f25bcc95c6f1562a676652509",
                "a04:union-arm:weak-authority-applied-identity-shard-09caf1a4f7e5f7dc",
            ),
        ];
        for (source_name, stable_name, tag, code, digest, arm_row_id) in expected_arms {
            let arm = union
                .arms
                .iter()
                .find(|arm| arm.source_arm_name.eq(source_name))
                .expect("WeakAuthorityAppliedIdentity arm exists");
            assert_eq!(arm.arm_tag, tag, "{source_name} tag");
            assert_eq!(arm.stable_name, stable_name, "{source_name} stable name");
            assert_eq!(arm.payload_kind, "inline-record");
            assert_eq!(arm.payload_sha256.as_deref(), Some(digest));
            assert_eq!(arm.role_predicate, "true");
            assert_eq!(arm.version_status, "reserved");
            assert_eq!(arm.max_size_bytes, 16_777_216);
            assert!(
                catalog
                    .projection_rows
                    .iter()
                    .any(|row| row.row_id.eq(arm_row_id) && row.row_kind.eq("union-arm"))
            );

            let wire_name = format!("{owner}.{stable_name}");
            let variants = catalog
                .identity
                .wire
                .iter()
                .filter(|wire| wire.name.eq(&wire_name))
                .collect::<Vec<_>>();
            assert_eq!(variants.len(), 1, "{wire_name} variant is unique");
            let variant = variants[0];
            assert_eq!(variant.wire_type_id, code, "{wire_name} code");
            assert_eq!(variant.kind, "union_variant");
            assert_eq!(variant.containing_union.as_deref(), Some(owner));
            assert_eq!(variant.wire_tag, Some(tag));
            assert_eq!(variant.status, "reserved");
            assert_eq!(
                variant.encoding_context,
                format!("arm {source_name} of closed union {owner}")
            );
            assert_eq!(
                variant
                    .allowed_containing_schemas
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                [owner]
            );
            assert_eq!(variant.max_size_bytes, 16_777_216);

            let arm_source_key = format!("arm|{owner}|{owner}|{source_name}");
            let arm_targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key.eq(&arm_source_key))
                .collect::<Vec<_>>();
            assert_eq!(arm_targets.len(), 2, "{wire_name} has arm and wire targets");
            assert!(arm_targets.iter().any(|target| {
                target.target_kind.eq("union-arm") && target.target_row_id.eq(arm_row_id)
            }));
            assert!(arm_targets.iter().any(|target| {
                target.target_kind.eq("wire-type")
                    && target
                        .target_row_id
                        .eq(&format!("a04:wire-type:{owner_slug}-{stable_name}"))
            }));
            assert!(
                !catalog
                    .reservations
                    .iter()
                    .any(|reservation| reservation.symbol.eq(&wire_name)),
                "{wire_name} is a direct wire mint"
            );
        }

        assert!(
            catalog
                .identity
                .fields
                .iter()
                .all(|field| field.containing_schema != owner),
            "{owner} remains a value union and does not become a durable-field owner"
        );
    }

    #[test]
    fn a04_unanimous_precedent_field_contracts_are_exact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let expected_fields = [
            (
                "TopologyState",
                "applied_control_ref",
                0x000d,
                "AppliedControlRef",
                "optional",
                "role-meta",
                49,
                30,
                "a04:field:topology-state-applied-control-ref",
                "field|TopologyState|TopologyState.applied_control_ref|applied_control_ref",
            ),
            (
                "ValidatedRemoteConfigurationAnchor",
                "consumer_applied_identity",
                0x000c,
                "WeakAuthorityAppliedIdentity",
                "one",
                "true",
                16_777_216,
                // moves with its containing kind, 40 -> 14 (fgdb-oicl)
                14,
                "a04:field:validated-remote-configuration-anchor-consumer-applied-identity",
                "field|ValidatedRemoteConfigurationAnchor|ValidatedRemoteConfigurationAnchor.consumer_applied_identity|consumer_applied_identity",
            ),
        ];
        for (
            schema,
            stable_name,
            field_tag,
            exact_wire_type,
            cardinality,
            role_predicate,
            max_size_bytes,
            construction_order,
            row_id,
            source_key,
        ) in expected_fields
        {
            let fields = catalog
                .identity
                .fields
                .iter()
                .filter(|field| {
                    field.containing_schema.eq(schema) && field.stable_name.eq(stable_name)
                })
                .collect::<Vec<_>>();
            assert_eq!(fields.len(), 1, "{schema}.{stable_name} field is unique");
            let field = fields[0];
            assert_eq!(field.field_tag, field_tag);
            assert_eq!(field.exact_wire_type, exact_wire_type);
            assert_eq!(field.cardinality, cardinality);
            assert_eq!(field.identity_class, "inline");
            assert_eq!(field.reference_semantics, "none");
            // Per-entry, not a shared literal: a field's order must EQUAL its
            // containing kind's, and these two owners no longer share one.
            assert_eq!(
                field.construction_order, construction_order,
                "{schema}.{stable_name} construction order"
            );
            assert_eq!(field.role_predicate, role_predicate);
            assert_eq!(field.version_status, "reserved");
            assert_eq!(field.max_size_bytes, max_size_bytes);
            assert!(
                catalog
                    .projection_rows
                    .iter()
                    .any(|row| row.row_id.eq(row_id) && row.row_kind.eq("field"))
            );

            let targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key.eq(source_key))
                .collect::<Vec<_>>();
            assert_eq!(targets.len(), 1, "{schema}.{stable_name} target is unique");
            assert_eq!(targets[0].target_row_id, row_id);
            assert_eq!(targets[0].target_kind, "field");
            assert_eq!(targets[0].definition_status, "declared");
        }
    }

    #[test]
    fn a04_source_ordered_embedded_union_targets_are_exact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let expected = [
            (
                "CertificateAttemptAbandonSpecExpectedLedgerState",
                "CertificateAttemptAbandonSpec",
                "CertificateAttemptAbandonSpec.expected_ledger_state",
                &[("Planned", 0x0001), ("Collecting", 0x0002)][..],
            ),
            (
                "ConfigurationStateForm",
                "ConfigurationState",
                "ConfigurationState.form",
                &[("Stable", 0x0001), ("Joint", 0x0002)][..],
            ),
            (
                "ConfigurationStateGroupRole",
                "ConfigurationState",
                "ConfigurationState.group_role",
                &[("Local", 0x0001), ("Meta", 0x0002), ("Shard", 0x0003)][..],
            ),
            (
                "InitialProtocolStateRecipeInheritedOrEmptyAuditQueueRecipe",
                "InitialProtocolStateRecipe<Role>",
                "InitialProtocolStateRecipe<Role>.inherited_or_empty_audit_queue_recipe",
                &[("LocalMetaOnly", 0x0001), ("ShardInapplicable", 0x0002)][..],
            ),
            (
                "InitialProtocolStateRecipeSourceKind",
                "InitialProtocolStateRecipe<Role>",
                "InitialProtocolStateRecipe<Role>.source_kind",
                &[
                    ("FreshGenesis", 0x0001),
                    ("Restore", 0x0002),
                    ("RoleTransition", 0x0003),
                    ("Takeover", 0x0004),
                ][..],
            ),
            (
                "TopologyStateForm",
                "TopologyState",
                "TopologyState.form",
                &[("Stable", 0x0001), ("Joint", 0x0002)][..],
            ),
            (
                "TopologyStatePartitionScheme",
                "TopologyState",
                "TopologyState.partition_scheme",
                &[("SourceRange", 0x0001), ("HubVertexCut", 0x0002)][..],
            ),
            (
                "TopologyStateSortedShardsRecordState",
                "TopologyState",
                "TopologyState.sorted_shards.record.state",
                &[
                    ("Active", 0x0001),
                    ("Joining", 0x0002),
                    ("Draining", 0x0003),
                ][..],
            ),
        ];

        for (name, containing_schema, union_path, expected_arms) in expected {
            let matches = catalog
                .identity
                .ordinary_unions
                .iter()
                .filter(|union| union.union_name == name)
                .collect::<Vec<_>>();
            assert_eq!(matches.len(), 1, "{name} union must be unique");
            let union = matches[0];
            assert_eq!(union.containing_schema, containing_schema);
            assert_eq!(union.union_path, union_path);
            assert_eq!(union.field_tag, None);
            assert_eq!(union.tag_wire_type, "u8");
            assert_eq!(union.encoding_context, "closed-tagged");
            assert!(
                !identity::ordinary_union_has_top_level_shape(union),
                "{name} is field-level, including on a generic host"
            );
            assert!(
                !identity::BUILTIN_WIRE_TYPES.contains(&name)
                    && !catalog.identity.wire.iter().any(|wire| wire.name == name),
                "{name} must not claim the top-level wire-backed collision exception"
            );

            let actual_arms = union
                .arms
                .iter()
                .map(|arm| (arm.source_arm_name.as_str(), arm.arm_tag))
                .collect::<Vec<_>>();
            assert_eq!(
                actual_arms, expected_arms,
                "{name} tags must follow Appendix A source spelling order"
            );

            let union_source_key = format!("union|{containing_schema}|{union_path}");
            let union_targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key == union_source_key)
                .collect::<Vec<_>>();
            assert_eq!(union_targets.len(), 1, "{name} union target must be unique");
            assert_eq!(union_targets[0].target_kind, "union");
            assert_eq!(union_targets[0].definition_status, "declared");
            assert!(catalog.projection_rows.iter().any(|row| {
                row.row_id == union_targets[0].target_row_id && row.row_kind == "union"
            }));

            for (source_arm_name, _) in expected_arms {
                let source_key = format!("arm|{containing_schema}|{union_path}|{source_arm_name}");
                let targets = catalog
                    .targets
                    .iter()
                    .filter(|target| target.source_key == source_key)
                    .collect::<Vec<_>>();
                assert_eq!(
                    targets.len(),
                    1,
                    "{name}.{source_arm_name} target must be unique"
                );
                assert_eq!(targets[0].target_kind, "union-arm");
                assert_eq!(targets[0].definition_status, "declared");
                assert!(catalog.projection_rows.iter().any(|row| {
                    row.row_id == targets[0].target_row_id && row.row_kind == "union-arm"
                }));
            }
        }
    }

    #[test]
    fn a04_source_ordered_logical_map_unions_are_exact() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let expected = [
            (
                "RemoteConfigurationTrustRoot",
                &[("CurrentEvidence", 0x0001), ("ValidatedAnchor", 0x0002)][..],
            ),
            (
                "RemoteRetentionConsumerRoot",
                &[
                    ("Acquired", 0x0001),
                    ("AuthorityTransferPending", 0x0002),
                    ("AuthorityTransferAdopted", 0x0003),
                    ("ReleaseRequested", 0x0004),
                    ("ReleaseCertified", 0x0005),
                    ("Acknowledged", 0x0006),
                ][..],
            ),
            (
                "RemoteRetentionObligationRoot",
                &[
                    ("Active", 0x0001),
                    ("TransferredOut", 0x0002),
                    ("TransferredInPending", 0x0003),
                    ("TransferredIn", 0x0004),
                    ("ReleaseApplied", 0x0005),
                    ("AckPublished", 0x0006),
                ][..],
            ),
        ];

        for (name, expected_arms) in expected {
            let matches = catalog
                .identity
                .ordinary_unions
                .iter()
                .filter(|union| union.union_name == name)
                .collect::<Vec<_>>();
            assert_eq!(matches.len(), 1, "{name} union must be unique");
            let union = matches[0];
            assert_eq!(union.containing_schema, name);
            assert_eq!(union.union_path, name);
            assert_eq!(union.field_tag, None);
            assert_eq!(union.tag_wire_type, "u8");
            assert_eq!(union.encoding_context, "closed-tagged");
            assert!(
                identity::ordinary_union_has_top_level_shape(union),
                "{name} is a whole-schema logical union"
            );
            assert!(
                catalog
                    .identity
                    .logical
                    .iter()
                    .any(|logical| logical.name == name),
                "{name} resolves through its exact logical parent"
            );
            assert!(
                !identity::BUILTIN_WIRE_TYPES.contains(&name)
                    && !catalog.identity.wire.iter().any(|wire| wire.name == name),
                "{name} must be logical-backed rather than claiming the wire collision exception"
            );
            assert!(
                catalog
                    .identity
                    .fields
                    .iter()
                    .all(|field| field.exact_wire_type != name),
                "{name} has no manufactured anchoring field row"
            );

            let actual_arms = union
                .arms
                .iter()
                .map(|arm| (arm.source_arm_name.as_str(), arm.arm_tag))
                .collect::<Vec<_>>();
            assert_eq!(
                actual_arms, expected_arms,
                "{name} tags must follow Appendix A source spelling order"
            );

            let union_source_key = format!("union|{name}|{name}");
            let union_targets = catalog
                .targets
                .iter()
                .filter(|target| target.source_key == union_source_key)
                .collect::<Vec<_>>();
            assert_eq!(union_targets.len(), 1, "{name} union target must be unique");
            assert_eq!(union_targets[0].target_kind, "union");
            assert_eq!(union_targets[0].definition_status, "declared");

            for (source_arm_name, _) in expected_arms {
                let source_key = format!("arm|{name}|{name}|{source_arm_name}");
                let targets = catalog
                    .targets
                    .iter()
                    .filter(|target| target.source_key == source_key)
                    .collect::<Vec<_>>();
                assert_eq!(
                    targets.len(),
                    1,
                    "{name}.{source_arm_name} target must be unique"
                );
                assert_eq!(targets[0].target_kind, "union-arm");
                assert_eq!(targets[0].definition_status, "declared");
            }
        }
    }
}
