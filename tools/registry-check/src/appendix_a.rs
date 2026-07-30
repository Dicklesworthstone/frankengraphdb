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
pub const APPENDIX_BYTE_COUNT: i64 = 1_025_645;
pub const APPENDIX_SHA256: &str =
    "74369512ac477bc7ec913b67c06612d516f495841f83737913859c1307ba5719";
pub const APPENDIX_HEADING: &str = "## Appendix A — On-Disk Object Formats (normative contract)";
pub const NEXT_HEADING: &str = "## Appendix B — Graph Intent Log (the semantic vocabulary)";
pub const EXPECTED_PROJECTION_ROW_COUNT: usize = 3719;
pub const EXPECTED_PROJECTION_ROW_IDS_SHA256: &str =
    "ada9473b796d9b496a4a468451def82f9705f55049e6329adf5c37300f31b0d3";
pub const EXPECTED_PROJECTION_FALLBACK_COUNT: usize = 135;
pub const EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256: &str =
    "8a0cb7da4e2d8bfd20335634d6670e18bb66349ec1a303448402489a52450235";
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
pub const EXPECTED_AMBIGUITY_ADJUDICATION_COUNT: usize = 455;
pub const EXPECTED_AMBIGUITY_ADJUDICATION_SHA256: &str =
    "53f282596c615b683d9112a3b35a97bf8cb665a606b686c926fd4f5cbdf3c3c2";
pub const EXPECTED_TYPE_RESERVATION_COUNT: usize = 813;
pub const EXPECTED_EXISTING_TYPE_RESERVATION_COUNT: usize = 446;
pub const EXPECTED_RESERVED_TYPE_RESERVATION_COUNT: usize = 367;
pub const EXPECTED_RESERVATION_HIGH_WATER: u16 = 0x051d;
pub const EXPECTED_RESERVATION_ASSIGNMENT_SHA256: &str =
    "53b4c274c45c3bc878618b6018e1746429a609376f3f874300c13c6abd775e0d";
pub const EXPECTED_REFERENCE_TARGET_IDS_SHA256: &str =
    "84276b6d97342e9ec1619424ddacb5b429e98e1862e03359afc837b65bb3392e";
pub const EXPECTED_REFERENCE_OCCURRENCE_COUNT: usize = 2_454;
pub const EXPECTED_REFERENCE_OCCURRENCE_SHA256: &str =
    "64535886e6dbb525694d6676b315397b959291e2901b9bcd456ae0e61861d4d3";
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
    source_locations: &'static [&'static str],
    resolution: &'static str,
    resolved_source_keys: &'static [&'static str],
    /// `sha256(catalog_row.rationale)`, NOT the prose.
    ///
    /// THE PROSE STAYS REACHABLE, and that is condition 1 of the fgdb-n061
    /// authorisation: `row_id` names the catalog row and this digest proves
    /// which bytes were approved, so any rationale is retrievable byte-exactly
    /// with `rg -A8 '<row_id>' registries/appendix_a_catalog.toml`. Nothing is
    /// deleted from the repository -- the text stays at its source, in a file
    /// this change does not touch, and digest + row reconstructs any row.
    ///
    /// WHY A DIGEST AND NOT A COPY. The 450 literals here were byte-identical
    /// to their catalog rows, 450 of 450, zero paraphrase -- so they carried no
    /// independent content, and they were the LAST place an existence query
    /// about Appendix A resolved against a copy of Appendix A. Every
    /// `registered` law name in registries/laws.toml had ZERO occurrences in
    /// this file outside them: the most-cited one returned 767 hits, every one
    /// a copy, and the plan anchor shared by FG-LAW-01 and FG-LAW-02 was 76
    /// hits and 100% copy. A first law sweep read three laws as RESOLVES and
    /// all three were false for exactly that reason
    /// (fgdb-checker-mirrors-subject-prose-23u1, fgdb-n061).
    ///
    /// THE LAW NAMES AND THE ANCHOR ARE DELIBERATELY NOT SPELLED HERE. Written
    /// out, this very comment would put them back into the file and a law
    /// existence query would resolve against it -- one hit instead of 767, but
    /// the same defect, in the paragraph explaining the defect. Cite laws by
    /// their registry ID; names and anchors live in registries/laws.toml.
    ///
    /// WHAT THIS FORECLOSES, stated because it is real: a reviewer approving a
    /// digest bump can no longer read the justification at the point of
    /// authorization, and `rg` archaeology for adjudication reasoning must move
    /// to the catalog. Detection is preserved exactly -- see the fail-closed
    /// digest guard in `validate_readable_ambiguity_contract` -- but legibility
    /// here is not.
    rationale_sha256: &'static str,
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
static AMBIGUITY_ADJUDICATION_CONTRACT: [AmbiguityAdjudicationContractPin; 455] = [
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9902cb5d9fadf41a985fd54c1bc021af6ff2e124af9886e02fb808aac5c05459",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.authority_ledger_floor|authority_ledger_floor",
        ],
        rationale_sha256: "ab97a2012e7b50e10a04e6ec801a59129177de98e9df7bb72cfa5ddf8130a594",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:99a87928b4e9051fadedb901f4799986579d307add86f64e1c8848d530e53adf",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|CertifiedRemoteStrongRef<T>"],
        rationale_sha256: "66c0c7c9250af0c34214109009f0a19bd5a9197320a84287215d33ad5cbc7e93",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b73053d5a89314ce34bf5ab28ab0942c5ba8aa5c2d1cd43a6f59ff4449e15438",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalGlobalCommandRef"],
        rationale_sha256: "29e26c29e0acbb16b27bc301a50fa13c1869a736a8b208a8f4b932a5cfe2e681",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6b485c80a37d34cd7e268be5fa2499117ce1c88914eaafab9bc9ee53e32cc15f",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalGlobalTxnInputRef"],
        rationale_sha256: "3145df2620ce4efafa06845d7dc29300e1d1b155b40fec47ea32f0a9115e7574",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c1de29a1f04f3d29608d42035d829d168499200bf1449172de752428a74f6ba4",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["arm|ConditionalMarkerRef|ConditionalMarkerRef.axis|Branch"],
        rationale_sha256: "f5ba091475c4827c33b169e71c833c20a010d98ce7eef650573fc58ed27f46f7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e5067c1188355a4aeedc045cd474f780b8f80e01a0e129dcfd0569e5dbf960c0",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalShardCommandRef"],
        rationale_sha256: "0820bdfbca4dee2bffdbf4d5f0de4f8944648ac9964d444110e07380c5ae9812",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6a6f71c5287f6e68eedbe69fa907319d95baf3c892ae49eb331e73f76a5a81bb",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ExportLeaf<T>"],
        rationale_sha256: "dbb68e882a60337791f7b9cec4ad0567124af551aea4b54c3da529c571130510",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f6b057d813024d9cdae86474e26e70f832b8b56ea96997303e2ea6e8d9fb180f",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|MarkerRef"],
        rationale_sha256: "b45d1ade8a35432b5b5fa98cc9db0cc909248513f5e73c670ab6a31d00487cfd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:19071118724e502558c8001fc247894ad3c6e95f24063c3c06a7259543443905",
        slice_id: "a01",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale_sha256: "935be12365d4c15805b4eba48da75ec974244b6837e29ee4a4c554031a191ef3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c4d2564bf7c395c7b349e663138fcb4c1e4361690d3c26044b1aecf73e43ec0e",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteReleaseSummaryEntry"],
        rationale_sha256: "39ba6d12845450a916a20d8fd62c87d97cae6f5633ecd59febaada0db90ac1c3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0da0b826f748cf4bd8faa497654351a2a9764542ea6982b41924de7afa6d745f",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionAckPublishRecord"],
        rationale_sha256: "c4e00b28f3ae46be5105fece2eef83e96fbdfebdbc3cb97b91fc5d513b4d9e40",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f91e77715cb9aae0faef9408747017b3a72f0d8d8c57bc1ab44771bba3169884",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionConsumeAckRecord"],
        rationale_sha256: "ff61a276ccb88875005cb59ce7889d1efdae1ef9aef79a74bc605f10f268c35a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4286033216d0e30f33f3289adca37f9ae9dbd4cdcfb89adbc1b94aa8cf488b43",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionGrantEvidence"],
        rationale_sha256: "5f1fef62e08e9566c79958f04af6fd6844a3b54a1cce12efdfdcc42e56db3709",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ca4727cb8f2c1151bad56af9e8998591d000a55bf9cf95566c5ddcafb4911df2",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionGrantRecord"],
        rationale_sha256: "539dfdd0f27bfb1285c6021953e9766a6283b48a7039703fb692dca8e6df22ac",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9201ddc13a840cea2d41c1df2285130c96caaf6c76b5d87456ebd7632f93fb5d",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseAckCertificate"],
        rationale_sha256: "0b8529905d9d23841676feb269c2cd8c1d2b8082201a9250c49de8b5568b4e00",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3e6f76af12b99912355abdcc8a766637447f32aa917a064eb14bfce535122caa",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseApplySpec"],
        rationale_sha256: "14eb7dd99b2890966941567ab163fc18afa4cb94d57c467759c6693ce826436e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:64ced526a660b98827cbc3ef997b177b68c4a84a15985fafd6640a432f68a5d3",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestCertificate"],
        rationale_sha256: "6039c1ce43200f20ced820c0dfdbc7fdd0d361b21121d3d8fb0ad4e9bfc43edf",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7a55234f5fdc43b6974252edcd0de7eba52e436ac217f10f8401ef6885ae9941",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestRecord"],
        rationale_sha256: "22ea1cd4dc029ecff6ea9d322e372ab48780b85214bc412fba152ba945eb62a3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:18d5436cd38b00236a2ca12e02bdeac86602803425abe2d1fa455996f2ad7f59",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestSpec"],
        rationale_sha256: "dc25623c31405c6ce029232fdbbf5e34e7cd23d4eb621cbd4dc7f35f0bbd0a5c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e120bc8a45d1cffd3c567b730d1c1c94e3efc66dfdec7c758fc8a7a7ab7bd8af",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseTombstone"],
        rationale_sha256: "2c5f8983d5139853e849f15576f77bf6e749093e2169a59290be68b33b02ab63",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:159ccf72cd3fb33feaaa8a683be064682e50c25785f7cbe598da6b0be0087f92",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|StrongCiphertextRef<T>"],
        rationale_sha256: "1bf7bcfd737f3177e0dfe676302b0f98df4ffde13e3198a5f1833133ac8c7e77",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b88b270c8e81a91838a8ad22b084d5f62869bfc4017064ffb7017275d923a751",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|StrongGlobalCommandRef"],
        rationale_sha256: "6149dd3569bf6268a79878d0bcbd551c91d7d3fc4003678b4dd5ccb0021bc734",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f2ff70af4b775f5145f4f900f742808ffade01b29c77ed78fafb5b4338eb7c37",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.export_projection_version|export_projection_version",
        ],
        rationale_sha256: "9392833ece86ae837e4a6539c971ba7b419da39f2d492cd2b8bbb2259a25530d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f415e1d2a5f705c55cb0e824abed15b2718e4379274f36ef4173e7fbcdc07b56",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|StrongShardCommandRef"],
        rationale_sha256: "eb460ade5fb692fb1cf31f7eb32e8fa1554ce09cc294fdf1214caa298cc98054",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c7ce81e2e7f285a53c0c12aead439bc99a21ae05d08f9e8cdae9acf3a09e857d",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|WeakGlobalCommandIdentity"],
        rationale_sha256: "87b79edcdf266c94943351e7ad024e5bda63559288f9adf78f3f89df83346032",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:1268173b9b0e90db9b8c6ff9e5fecbccc41c62c0f6eca9a75bcc274bc78c89df",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|WeakShardCommandIdentity"],
        rationale_sha256: "2f03caca764bea0dd2b3190eb5b7c4dfd1840af893b9d776478c5dc59cc16f29",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c3451eba691ae2bb32b935e0e2f4f563b7ab458f675c0418e8af0b3a7a86b418",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ConditionalGlobalTxnInputRef"],
        rationale_sha256: "dd55d453ca8b828bacd495201126387577f2625ea0bae86aa51d36163b2e535b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:83653995aca02322485f58cf8cc3a4937305ef9f84d50c6659fc2cd9004e136e",
        slice_id: "a01",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale_sha256: "72cf8e168afba6c4d14c5e72b66f85b75880d5e197da0ad61cc9a0ac71fc2193",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:892b85a96dad0e9766ca9fbef78fc37c5df29d469861d7b1bc9d6b1f7c567182",
        slice_id: "a01",
        source_locations: &["a01:1392"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|CanonicalScalarProfile"],
        rationale_sha256: "758c821f827b4fc93ff4d57b66f306a1bb63db065d0b67941cc07be4020cd71e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:608b425da6fb9c8cda3d49a78aae9a3e8c02fc48b30e613e1b2c417b202ae14c",
        slice_id: "a01",
        source_locations: &["a01:1390", "a21:2649"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|None"],
        rationale_sha256: "677bd1b25242a8bd229d0a6d046a392226321240c2afeda4ab0e0ebc84577023",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:5c7c00068e6786930898a4cd7ca0936d1be398fa90428449bc501db193221292",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PayloadAvailabilityCertificateRef<T>"],
        rationale_sha256: "e88634777959f2290b42c00d4af2e85c76a7451f56b0a247f44cb4b4c609c7e2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:05d7e3bb322be80fda931743566a01b05d3b38cf82f7b0d5c40fd940d655af1c",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|RemoteConfigurationRef"],
        rationale_sha256: "19ecb5ac139665e53b3a74c721df0119772a6f4df18321abe28a038cb121a0a7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:36c38b8690c34ce658b11fd0ddde6ac14aa37b8c84284910f9de561091d317e3",
        slice_id: "a01",
        source_locations: &["a01:1402", "a04:1578"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|RemoteGrantTargetRef"],
        rationale_sha256: "6893d396ade13f58f8ad7aaf724fd1512bc94090382e718765bc79a3208093d3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bf3f4910c7babb04019eba3e8a9d5ff90e67cf04fb39ccb54ac7192b1d4ff437",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.adoption_log_prefix_digest|adoption_log_prefix_digest",
        ],
        rationale_sha256: "3321edfdd65d7494e9d20045d240f6d0478d132fa67a53a7b28f5c2226b3c758",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9ef85f201456d54979f092bb31b1777aaaf90d831e425af9b6701f465bb99d80",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.canonical_configuration_bytes|canonical_configuration_bytes",
        ],
        rationale_sha256: "97f8ea7531ed3659382382a9e5312ea555ee0716806d26d07e0505f20a2b8a31",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:44d2f6bcfdaa7e6ac3780a200d27f10a33a0b638fb0615f3c96f5e98d64c6592",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_adoption_raft_index|configuration_adoption_raft_index",
        ],
        rationale_sha256: "a0faeebb69d6e272b4d68f2f0ec38eefecfa06591b61abae8d149ef4464934f4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:af7e299a09a52c513493942368959abef28db73ce3341a288576b8eb4b53c0f4",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_canonical_digest|configuration_canonical_digest",
        ],
        rationale_sha256: "05a217513581d9bdfe70c541079650d8ba1a625b8825f4bf8074a2485428687c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:86202e40010f1afc8012891816b8808b7e6c8ce542ab9892b8cf0b01af0dd23d",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_oid|configuration_oid",
        ],
        rationale_sha256: "031c9fd08dd7bc23a6bf18eb52e0c7e968c4965faa26e170774386fdf1749462",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bf26b9e5234ca109bef539c1a4c98e58925e1c2e3dd5e123f0b75fc633ef523e",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.configuration_quorum_signatures|configuration_quorum_signatures",
        ],
        rationale_sha256: "655eec53b0fec38ad594469977c47a140402608fd4a462d0674dd68d4041d737",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0cbbda898da10fa9b89f900be61c7b9aed7bbd0934366e1e71f3abdde07956a0",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.member_verification_key_set|member_verification_key_set",
        ],
        rationale_sha256: "5cb149dbba5dff9cd468ab3e63cf0d27ed3925b198220e2f27ddb4e0e66e9cd7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ee8990906ae0c1ecb94acbbe2f5723f319918c316d350f7108484833b54ba629",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.minimum_configuration_retention_floor|minimum_configuration_retention_floor",
        ],
        rationale_sha256: "db9cf5c2dd7045aca7fd1712be8d51f88166ab48d1552764ce3e0fe3d1d62d0f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e9dc734b30ce92280487bf83e234b3face8e3ea47b93f841bf472a2ef76643a2",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.payload_predicate_digest|payload_predicate_digest",
        ],
        rationale_sha256: "4a5ca3491f48b5c078cb650cfe910a91d543a32e7e0f4c9f74cdd407b2696113",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:785c16b82f46561a50e849315dd7e84c669b4f3b703746b32b4963ed6625b54e",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.signer_epoch|signer_epoch",
        ],
        rationale_sha256: "bed2a2d917f1766bd161285ac9531268a5e50cc29f3b3737f194dbaf707b86a0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:89368ae55192e51984ac23f81f0afa52c478e1ce606f91c776060f4c8a595396",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.authority_quorum_signatures|authority_quorum_signatures",
        ],
        rationale_sha256: "6e30c560b948d51d2cfcd42efde7a676d52e6b0f4e02ea6a3c86b3a2d2b37b49",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:550459906367220aa9ba71ed7b8aab0f60f321bb09c75879e1d3deec8fb0f15d",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.authority_retention_floor|authority_retention_floor",
        ],
        rationale_sha256: "bcebb1398f02ff87383dfcc64beda7fe1f37bb1fc08077affbb4219474f813cf",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0780dfaa7be7e082803da9d4d1d980f09c6ab855346e8a7f8638b7c5daea7ea9",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale_sha256: "11699b54cc40afa5556ae450264f6abfe2410a4b66f515015a3796b5234f916a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:088c4ff91992149430ae731d5ff92818988720134060669d7f25967c8e35e59f",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.encoding_placement_coverage|encoding_placement_coverage",
        ],
        rationale_sha256: "7cdc78eb20e4a57a07c648325ef34b508e2aae4b8e7c451700a41a4a06290f3a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:330ec263173ef0a9576a1913c6ba0487bf4138413b87bd18ecf4f7ed4b08fb49",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.failure_domains|failure_domains",
        ],
        rationale_sha256: "b0fc22126d5903741e558d74ac3bd59c42f002212461ba8c0e2ea0ec24de94f2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a3d03272c5e918952e4ac5c7fa89e97a8e29c5540a63623ff0ea776fea86e0ee",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.payload_predicate_digest|payload_predicate_digest",
        ],
        rationale_sha256: "9f40f4f1d8ec3004298be30eb3f2a6eba62ee995743c052bf4da97c1951a4c93",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:8891ee1b2dce0bcac481ecf3bb37e10b1194281a42f9f16bc24838fb04f87454",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.receipt_set_commitment|receipt_set_commitment",
        ],
        rationale_sha256: "b95f0ac158d4f5f8c01f079654903c2a2b0674cdaedbae03c9cb26e2528037d2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:dadff94a138b7c32a653efd353155f880adab9d0a9d5bd1442b5faed58225b0c",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.signer_epoch|signer_epoch",
        ],
        rationale_sha256: "2c0db974abf0a7bfd9bafa54a771ddcd02447b841d9298b9674bebf56009fc95",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0b2170ebc0cdcae1b0a8fc5ae73c50e6539cbe87246be6773e4eca53e8c24b7f",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemotePayloadAvailabilityEvidence|RemotePayloadAvailabilityEvidence.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale_sha256: "7005bcb7eeed5d054203a46a61da9ca8b2dca3da2bdd8443bb43c7adab63337f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:aba1b29d59bfb1146158d7c01d9f17f701b1a2f9aceaca04c3e15583a52481b3",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.ack_digest|ack_digest",
        ],
        rationale_sha256: "27e715c2b9bc6c269a97a469d604027f1f9becd50117e0ee8fbafd977213786f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ba383604b0f04fa552de5ca7b52083a58cec2bb816d9d50668b4cee84b8cb40e",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.object_specific_scalar_projection|object_specific_scalar_projection",
        ],
        rationale_sha256: "47ad7a3338ba6aee2064d2162b40e3a5b877a93c61ffe40390506e30f22caabf",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ba4b4e426fd4324114eaaad337e442b5ce7d6e038a34db56d1418c705e15954e",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.ack_leaf_identity|ack_leaf_identity",
        ],
        rationale_sha256: "8653639e0bb34b65564dcd4080fd82b9839edef17373b8bea3ddf59e4c8e1ff3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:23f9c08803b086cdbfc8c97b6f9659e8bb5c6e6a8c9cf04032f5f5d28e079408",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.authority_domain|authority_domain",
        ],
        rationale_sha256: "1d84a9c5268998457c2ca7d29b8ab617775c9b4c4ea257d829ce7e7d09182521",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7d1a0cc415b4e6a6170783944fabb87802c0c72103c47fae6f2c414de670e118",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.authority_order_index|authority_order_index",
        ],
        rationale_sha256: "20c58a89825916d899c1569830a64e8f5913e115e7548646893f76c18d1670fe",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3ed46e16d278ca8758eb8f03cda81ec00cda011ecabf571c6efd9b1ced1f858d",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.consumer_domain|consumer_domain",
        ],
        rationale_sha256: "7a107a46976a8282a5bc9ab1cdfd3a14a187e04b1b110acf6ca3ec70c5b34257",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0a140168b441efaf4eba40cc6f5f32b10863296cf590c9a62de21e0694f64a5f",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.grant_id|grant_id",
        ],
        rationale_sha256: "7a3d8582263e2fced3d83a5d2aab4ebeace6f443ccb7b2dbdb0be615093be7f1",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:63373197a5998d375086fa33282c053b58e6273df62146964f86434530696359",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.permanent_release_proof_floor|permanent_release_proof_floor",
        ],
        rationale_sha256: "3587de87b9235cd655f996d02087df158055dc2798f518a8681d9d3e18ac867c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9b5ce7da11e6c3031a9e2ff2d7f3f2c8868508ee8ee79cfe77c289a072e22a74",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.published_at_order_index|published_at_order_index",
        ],
        rationale_sha256: "659ea440a4010d0d41d80ae593f2b2faeb162383ff9a647685079cf891f8b371",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d010489d0119524a9d50b49a62f4f9944d175fac9a6ee4ad8de8128784e34969",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.release_nonce|release_nonce",
        ],
        rationale_sha256: "55b3dd593d92780a76c768ff784154429eef7c4921e6180cf5d8e76a45f058e9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a8344545b02b95a838fadd3e5bbb725c3d60093f1ac88f08a2cc9acfcecff955",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteReleaseSummaryEntry|RemoteReleaseSummaryEntry.target_identity|target_identity",
        ],
        rationale_sha256: "73ce077933637fdbe1652d162baec342f63d26143511a3ff72300f82667eb308",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:104955772015586008e43b5d3d99bd835f456ec5c11f29fce79c03c941ad0be3",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionAckPublishRecord|RemoteRetentionAckPublishRecord.summary_key|summary_key",
        ],
        rationale_sha256: "25cf3a2f8094c770181aca57a47426e78daa41539f7b2ff799be8b11bf6d20d4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c63e45723d33f104675de8ed3e9a8417545aa6209c6ef981a9b04c56fcca5bd0",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionConsumeAckRecord|RemoteRetentionConsumeAckRecord.summary_key|summary_key",
        ],
        rationale_sha256: "dcb662b44664ce0be39957ddc249e420f444f29ad64864f3b6de80d98cf9f5bb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3228dd0b5dd8875265298f3a724ef85adbebeb35fcbb5d05df62e87b91c40f82",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.authority_order_index|authority_order_index",
        ],
        rationale_sha256: "45c27a1be9f3437ad4a006ac73b2eafeb688fa6a32a389e7884f8f96abef7b9a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f18419d17e8a08e7609f35ebbc6f4c09735a946a02c5e7512a3ab0406f72f8cd",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.authority_quorum_signatures|authority_quorum_signatures",
        ],
        rationale_sha256: "47fda35cd213e49000d663e58856ce47e93b968e634ab0c4dffc89d9400f94f4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:fbd189543ad2fee10893b87f6f45d238a17c00595c70c1b415e5ab6dfd125b9a",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.grant_id|grant_id",
        ],
        rationale_sha256: "1ff3a5f9fbd04056dbbcf15b1a5c3becc9007e368939a19cdc7e6c7d032ba260",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d6336ec6c39141df42c4ed61b1cac308f22a269f73b8ff4aa55106247f19ce93",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.grant_nonce|grant_nonce",
        ],
        rationale_sha256: "dd4ecfc341e057cd4e20bd372d0bc85e5dcc9b0ab0ab721ff6f1d33c2b090296",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ee01aff30d2078379b503b7895ae8be464a00a752466263025dfbd0a45fdb667",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.minimum_authority_checkpoint_floor|minimum_authority_checkpoint_floor",
        ],
        rationale_sha256: "9c64b1ecb2676bbeff369469eaad93060c5e11943f723db9965bdec3489c335f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a816a6e2f7d5f4db12015423d9bea5c670a3e072257be3f0931e3ed61d49bee7",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.signer_epoch|signer_epoch",
        ],
        rationale_sha256: "29c8973cde57336f7f13dc09722ef676f6c33961aa8c95057a25a6fc3a343480",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:86987e71c410029e676e72048d9e14928105ac680c68d7b8dd9b3fa3a1e5c49d",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantEvidence|RemoteRetentionGrantEvidence.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale_sha256: "244c3f374e118bc2cb2212ff08b9ef80c3ef6f1edbf893c9e9a100eea84917c6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0ea0c37a2094412a8669dca8c447980a4970f76b3485a3a4cdedc96d530f9740",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.grant_id|grant_id",
        ],
        rationale_sha256: "ca5812053c8e2a0bf76547e356d845b07f39219e4627dc4c86e45746c67b3c4f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d470f3413f5c4faa0a2bf88552faa881c88f926d15a1d7bbbe4ce71f53817a5d",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.grant_nonce|grant_nonce",
        ],
        rationale_sha256: "d968a7e2e97abc05e79f6b4084e00d84f251231426ebb4db09f13e3794dbd947",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e14ae8c12903b4309edf9249c9fe2bf44de6671a7cce5cd73ae3e05ff8478495",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.minimum_authority_checkpoint_floor|minimum_authority_checkpoint_floor",
        ],
        rationale_sha256: "4f3ba258a34f4b61e778a94da5c792c102e9fee99ab68edaff1232e5130c8e6c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ec608cc085dc6c92eb129bd6aaeaac5f75c75069834e27f23ce68b9733e6f445",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionGrantSpec|RemoteRetentionGrantSpec.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale_sha256: "6300c726a480d15e43ce8b254a55143f0d302419d168d0e0c12a8f6f984fe808",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:38e1b1a453a2d78b3cd9b61fb722eb5dbee4e3ef16190d31191f4024a26a3d9e",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.authority_order_index|authority_order_index",
        ],
        rationale_sha256: "6c6133af6e618a98a1239ae88045fce92496231d44bbc612ececf3a9c9c2ec53",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:41ca748dc55b6431eaf3918bbe1b9a9734df2d1ca956d7e6f852cfb95efe5197",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.authority_state_root_digest|authority_state_root_digest",
        ],
        rationale_sha256: "28f29e6ad36c95f5c48df0fa2444de4b76155bb132cb867923ab9449c15e21db",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7d5b5b36c4658a32b106702e723141f57156946818eca7b8170d5acbe23674ee",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.grant_id|grant_id",
        ],
        rationale_sha256: "1cb5a8eef01f25a2a132a96aef0574b98d75e88ad1eb15eb6d13b0c9c377667b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ab27fb38ceb289b9a170a10a6a37c35180aea44221f333402b340661c320a043",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.quorum_signatures|quorum_signatures",
        ],
        rationale_sha256: "de3800c42428f8d79731efab64cc182cc0d069a28afce2bb74265a3f56119b44",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4bff13c8b469f1d738a8680dbae3d6c5f816043ad51d34dd0bd69416985ff533",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseAckCertificate|RemoteRetentionReleaseAckCertificate.release_nonce|release_nonce",
        ],
        rationale_sha256: "6386d34608e607b7daf91490d867cfef1686a8e28696f718c7fa8cbc96a59908",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:2d325be7f8f0ffbecc4dcd60c205c8760a79e9e0f90ddbded5e1c491881921b5",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.expected_active_grant_digest|expected_active_grant_digest",
        ],
        rationale_sha256: "a5fa5f82ff21358de5b120ceea7109bec1a32bbe19649384f56bdf600fef3f9d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:edc4e45136d059b93d4d936f23332275c3cb4bde7ea64a96fe190c8170a56355",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.successor_transfer_proof|successor_transfer_proof",
        ],
        rationale_sha256: "f6503c2dd41650a407dcd444b0d167f9de464ae589bd103ca2d8d82c45dc5660",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ad98d0bc880733e386ee6412e07437998406030d9bdb0efaff2e3793cb529ad4",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseApplySpec|RemoteRetentionReleaseApplySpec.verified_consumer_no_reference_floor|verified_consumer_no_reference_floor",
        ],
        rationale_sha256: "ef49287887c99c27f5946eebb35c791245e6e810ae4244df88303c11a9c4b22b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6f795daeb8ab9c6f9256b4c88ddb79c7fe051c84ffd60b6c0d97a9e9cf557467",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.complete_consumer_root_digest|complete_consumer_root_digest",
        ],
        rationale_sha256: "4891fa7ba3d2bc9619df93d1a14863faa8e4661194820ce17cade744ccd65e1f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d0d66f4d6ea6017ab754904e8928b724aa730a3d5dc0354290c5e0f370981533",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.consumer_no_reference_floor_digest|consumer_no_reference_floor_digest",
        ],
        rationale_sha256: "c902ce1341f01ec7399624fde341fcfdb884ad5c77b5eb331faa5be0cf62651d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b16fd118a0a64392b8ae28941eaaaf3b910196151732a38fe44b7fba9d08cc54",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.quorum_signatures|quorum_signatures",
        ],
        rationale_sha256: "a1a99a22301696bc69f3bf428a7f611d61013c5ed8ccdb4cf566089320d46afc",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a5377c0f69a2ceeaea82196dd3cfdfe3b5bc4106771c28224f4a6a90a1c46aae",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.release_nonce|release_nonce",
        ],
        rationale_sha256: "c600bddc9b555e3d1ed51a42311467ae4b5711d207d3bda117ec4191e569cbc2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f08db86581e0fafa4b2e38638a61d8ecda6371c9da72cdb671f1b34212da455f",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestCertificate|RemoteRetentionReleaseRequestCertificate.successor_grant_identity|successor_grant_identity",
        ],
        rationale_sha256: "5f28e7ec94c3fc01220ba44260653ff70c2d191f47d59a6dab5d285df62bf811",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4bbdb01f00f659a0e01412ae5d5cbaa2ddbc312ce44a5997149d1a2cd6d4ce0f",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestRecord|RemoteRetentionReleaseRequestRecord.consumer_no_reference_floor_digest|consumer_no_reference_floor_digest",
        ],
        rationale_sha256: "a93329ecf52ee34e30c5b4a550aefa5fab433f2c866bc6acd4888fbdc2212946",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:05940c088f9ba416398714a189357ee97c8e2c7e728a68eb8f4bb9291e8e7c13",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.complete_consumer_root_digest|complete_consumer_root_digest",
        ],
        rationale_sha256: "047833d8b25340a40e9062fcaec97eebd297d42db47975e70a7e3c6c3cf59a0a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cb107c3b25092a752db12aa072dceaae7e10ce08c8500f44d0e944ec974e7da6",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.consumer_checkpoint_floor|consumer_checkpoint_floor",
        ],
        rationale_sha256: "7a948a0493f53397b3bbbf9e66a704f0dfef0c69ed357f46c4b845d353bf9990",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:08913cde0840a5415b20c38f4728fca9d06781a479e6bcaf770b368c5488df0f",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseRequestSpec|RemoteRetentionReleaseRequestSpec.release_nonce|release_nonce",
        ],
        rationale_sha256: "9ecf8e96afbb34e7abfba6836fb0dc34946face2824b3f5c651d895f7e630f9e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ce182745bb770c96a671b0eba846d4d9a672cefa40a51f10cc88575445bd0e3c",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseTombstone|RemoteRetentionReleaseTombstone.authority_order_index|authority_order_index",
        ],
        rationale_sha256: "6b671300418b840236a0b01cacea3e8d2909473f2b5870a891287f5c34cb5e9c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:259538a996b4f52d0906e85b5e35436eee1012e4ada44589405094738a8b2725",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteRetentionReleaseTombstone|RemoteRetentionReleaseTombstone.release_nonce|release_nonce",
        ],
        rationale_sha256: "6b1edd4d203286f262dbbf66f703f4591c5763c5e928a501c154f1f27768ebcb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0cea5eb3bef0bc9ab4c17b1671ce03661e717d8ec0742dffff4e0566a2255868",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustArtifact|RootAuthorityTrustArtifact.canonical_root_authority_signature_set|canonical_root_authority_signature_set",
        ],
        rationale_sha256: "d7e86d04d764526569fae4210a6bff3ac5e16bb7ba348eb2fd10d38435112bf0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:47fbac16db79678402e6382624522139d902f97907bd56096457c3c53d502918",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.canonical_genesis_or_transition_bytes|canonical_genesis_or_transition_bytes",
        ],
        rationale_sha256: "a2467cdc887a3c721fdcefafd4570ff1ec775d17a8062c797e41fbcfa569a79c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b458a33eb43d02f3b156bc7d4539c5ebbb3740aa8e13dc2d369d5d203d0873ef",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.expected_root_verification_key_set_digest|expected_root_verification_key_set_digest",
        ],
        rationale_sha256: "05ad7f5dadb5272061284a3a1fc759b7269be384dc814ce0bcfd714b482dbc03",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:caddd2e243775866bb52f8da1e62fa89adabfb26042229cc138a2f2eb194b950",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.externally_pinned_root_policy_id|externally_pinned_root_policy_id",
        ],
        rationale_sha256: "2d0a3e0a0a01926cffba563c588d0df24d4d4e87a82141fb8991075cbe919247",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3fce4db02d1cb690e1e7204de1499f3ed1982f8f8eaecd41e1629b4c5403375a",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.source_identity_or_transition_continuity_commitment|source_identity_or_transition_continuity_commitment",
        ],
        rationale_sha256: "d7538ba52117c2cc5af263cfe0250a866d5510660794a09cfcfbbb2bda5e1ff3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0cfced7abb4163ebdcf4ffead214a475ffd662cb43bf1bb54c9848ce3cd137e6",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.target_configuration_canonical_digest|target_configuration_canonical_digest",
        ],
        rationale_sha256: "b4c8ed30c5adaef4d383c1631e7dff3e081c70d6f41f31a8e71fae2dc7944a30",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ef9a226efe47962957214937e4f1158545bb53682355abfc8ee4b464438e32e4",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.target_configuration_oid|target_configuration_oid",
        ],
        rationale_sha256: "1a3f386cf78ded5dd6ca6b239047dfead99754b2d711b246dcb20cda08a45af8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:396e4c7dcfc6962ef4e1b741b23543a382260c4385a4430104792dd47f60108a",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RootAuthorityTrustBody|RootAuthorityTrustBody.threshold|threshold",
        ],
        rationale_sha256: "bc5ee1747f0b34286eb1485d4f56dcabb1295558411f077e80f1e603b5a53f41",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ad518b83fc93d2e002e29f0b04c6997a3f4f7db95c0332b3396c97abdddbabce",
        slice_id: "a01",
        source_locations: &["a01:1425"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|RootSlot|RootSlot.reserved_zeroes|reserved_zeroes"],
        rationale_sha256: "2602f2dacef1ebf993d4f4f8efe62378dc0b2922e8190330be3c7469d14848fb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:09e59fe9e8d42990d61d08b6b8f2c7edb2526c89f0fdb20fae7745ef014a81e8",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &[],
        rationale_sha256: "7c46f389f893ee26a2b9469361b0c698cabdb27525a93e5aca71d27e56a1dd1a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:34577cda100fc597ce5020921e7520ccf2ff9ea71a5bee91bcfed896e09733cc",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &[],
        rationale_sha256: "dfa8ed796defbb7da16f3923d30453e60409ea5fe4dca0817492535acbf0a984",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0d9fa91b6888b1d850b7dc8d59eabdf2aa50b4782f6cfe5d9abec75bd9127586",
        slice_id: "a01",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale_sha256: "532406fb6b4a41c8485a97f234cb4e870b76e49786af0c5cdb46c3852035dd38",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d7a9a4eb5a85dfb74c358f357b30941729f34bcffd4a4a80e5acbe984df5ca50",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseAckCertificate"],
        rationale_sha256: "df9f6bf1289869c8c2794c1081361ce6707a107b3ae68f7a8440d8757366dc63",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b309ff017e04d9e2ad7b7d57dd82659a085c5ed58fb994ea08f5ca857aeb8b80",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|RemoteRetentionReleaseRequestCertificate"],
        rationale_sha256: "1f6b4648c21dcac4cddc9e3fb9f5c3e658bb42736a545a623b7145a1a9f025a5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ba8e5e4bfced370e72e8c5f2de3110ca3176fc081dcbdf0ebbf5070c8109f914",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.LocalControl.logical_command_seq|logical_command_seq",
        ],
        rationale_sha256: "9ddeeeadc8843b8104f6a705a80218d9ff1b84965069aed6d0d9c5677d3b0433",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:da6796d82ec8f0ad13cb7e98a3dd5027e081d6166fb25010f34fc1d3f942face",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.LocalTxn.logical_command_seq|logical_command_seq",
        ],
        rationale_sha256: "b3e6d66e2f5aa0f413a86b672fa463b4369e049477c594cffa0c9df445e27b73",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cd5263e4e5a623d18abf05fd873322bd2aed0ecf4101650b392c8ac0dfe83342",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.MetaControl.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale_sha256: "c75a3db98724dbf7943f3ed7fefe76f194b4152c135fae9704df411c97f84fad",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:86c7e179248aaf7991f43a1f3327c09353cf813cc01b8b33a36e4fa7bc70c63d",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedAbortRef|AppliedAbortRef.MetaTxn.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale_sha256: "c3f7137eef4a414a58a29330dee97f135213d5de68ebf7d6d8818324f997f13a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:5a6fd0f3b82c7f0e25cc7f2e54a6979556e86845159ade0fc5db84f20a68ce39",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedControlRef|AppliedControlRef.Local.logical_command_seq|logical_command_seq",
        ],
        rationale_sha256: "ef5b8167841fca5587fb7f64a1c5a0f288f68781c6679da5b6643c895d6056f4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:2d3a238643d53101c5c0b1b76309f7842bac2fde143198374ce80f7a28460922",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AppliedControlRef|AppliedControlRef.Meta.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale_sha256: "9d33e114bfe63bddded04cfa70f8d220d770f424b38fdfaf6ebb183999a6c8b6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4cd9ff504a8f45737e9059b60893933afd7d86625b7e0c02f1f076120253317b",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuditCandidateRef|AuditCandidateRef.Local.blocked_after_logical_command_seq|blocked_after_logical_command_seq",
        ],
        rationale_sha256: "361bbc63240de8867b43e7acddaef5762ce346bf65b9661a96574f099b66171c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:000a7ee23e3de7a2a40e0eeaed2ea1b2597bf1afc47a0fa82604885626570e48",
        slice_id: "a01",
        source_locations: &["a01:1408"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuditCandidateRef|AuditCandidateRef.Meta.blocked_after_global_logical_command_seq|blocked_after_global_logical_command_seq",
        ],
        rationale_sha256: "84ad58a6b85752c7158f93e8345b6b18660991d51010807becb6c6d37ae69713",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3f8943e11fe38023572621016d3bd2736d76845c6fa86ca01ab11c963ac3d295",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuthorityAppliedRef|AuthorityAppliedRef.Local.logical_command_seq|logical_command_seq",
        ],
        rationale_sha256: "fb93b4bc28574ac8687514cfe94220570c3190e71274cf43f7a7dd861c05b30b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cd8a157343d76e480a778f55bb074c26f2b450fe8d1a019afcb01160605d0736",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuthorityAppliedRef|AuthorityAppliedRef.Meta.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale_sha256: "63232aec44075c6b6bb1657abc7d5b2a94c994e34a32c0a62f8d3bb73bfafed9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:645cd3190c29d6877e5b52fbd9a7eb2d12617be3015867f30ebf73aff4d632f6",
        slice_id: "a01",
        source_locations: &["a01:1404"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuthorityAppliedRef|AuthorityAppliedRef.Shard.shard_raft_index|shard_raft_index",
        ],
        rationale_sha256: "59ffc270027d73971d4a74a87762d1839862d4dc869170cc474c83a5480b2746",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:4798d0e4d5005fcd185c72d5e213ed7330f55d1af1d633542b7eab63fe45cf84",
        slice_id: "a01",
        source_locations: &["a01:1402"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CertifiedRemoteStrongRef<T>|CertifiedRemoteStrongRef<T>.target_closure_inventory_digest|target_closure_inventory_digest",
        ],
        rationale_sha256: "b47c6ab1bab2a66fae378658085854e17dcd6fe260609c007031ef8e4b287b24",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:8d3b56aa3a767ea542872bc1ba3fd0ab477a7ed2ca662c337ee1bf8949583e7d",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalCommandRef|ConditionalCommandRef.command_ref|command_ref",
        ],
        rationale_sha256: "879d01c794bf43ab1265519d1e14ea46c179fa21f19367ac912096463c0e28bb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:35bc46ce1fd0d6c8b7488a044ef386504bc03fbd5d1ec2b770e86ab030a492d5",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalCoordinateRef|ConditionalCoordinateRef.branch|branch",
        ],
        rationale_sha256: "9d93098aa0e04fbe0d979d204495a49c8b30cd27fb03514a974002d06af79331",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:8bb17f7c3ce721f479db1a83c8e7dda855ec98c2eb641e4e187851f8c7c82723",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalCoordinateRef|ConditionalCoordinateRef.graph|graph",
        ],
        rationale_sha256: "ca1fbd0716d42f408ae9cbd47da4f1b6c3e27387a27192bfd977bc4831bce1ad",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:71b82ee0c32114b11f3406e639c01a44934215169bc01a02acd1b77d779ffe60",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|ConditionalCoordinateRef|ConditionalCoordinateRef.oid|oid"],
        rationale_sha256: "1fd85acb8e20c4682303156b10a8363a8bb76f91c2527ed09ec33fd7afdb3b59",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ce9bbf77be4d878c886864f08776365de232040b4d71c98b3322860c517e225c",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalCommandRef|ConditionalGlobalCommandRef.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale_sha256: "a2bbb760c3a0510c82f63f954d447af9ca89fcfe4cf876091ff77d7319cb7dc0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:91b4ecfbd59471f33fe1b99f790946e8559c552ac794ffb308e1dd109479fff3",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalCommandRef|ConditionalGlobalCommandRef.record_oid|record_oid",
        ],
        rationale_sha256: "134f01a7b292a431c1eea7296dee3dbba58b22cd8a8aa30816ddc20ead5f0c77",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:fef8d53fe305b0feaba6773e11a19e89f6adbe510b9f58cb35907fed8fdda0ce",
        slice_id: "a01",
        source_locations: &["a01:1406", "a11:1962"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef.assigned_global_logical_command_seq|assigned_global_logical_command_seq",
        ],
        rationale_sha256: "db324c993cfbb138be99b71bfbd942e207f427ee90f99f869c064d3a8684161d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:55674524425476cfb59b374d5d338c633a98488fde9d76d879b29e5b956b138c",
        slice_id: "a01",
        source_locations: &["a01:1400"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ExportLeaf<T>|ExportLeaf<T>.quorum_signatures|quorum_signatures",
        ],
        rationale_sha256: "efc089ac4a4c0397de2806e0df6781b38cc322a73c4d50ee51ff4ac22bdd658b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:56ab0b78574dcc4cc8ada1e27d64228256633d327386f25f3e31c79b082b93d3",
        slice_id: "a01",
        source_locations: &["a01:1406", "a11:1962"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalGlobalTxnInputRef|ConditionalGlobalTxnInputRef.command_oid|command_oid",
        ],
        rationale_sha256: "82cdb061b5cfcd45db0f865e1cd6b9a622d3d2f86c0d26097d62a63c23ecdb36",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:c9eacf0a0499722ac0abbf874b419638446536d586ed009714f01c2fe685713e",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalMarkerRef|ConditionalMarkerRef.marker_ref|marker_ref",
        ],
        rationale_sha256: "ec198011cbfa04f4e657fddba7f260246d843ab4ada35d9693cb9fc7003a88c0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:287525289d2fc48c53af7935b122ec99d01330bf6bc8e7acb3484a796c7b2dc1",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalShardCommandRef|ConditionalShardCommandRef.record_oid|record_oid",
        ],
        rationale_sha256: "131da73affe4c6b869091e31a043930876cdda519ef623af04075f60e5fdcf6c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:27f66e75e71751e7776eb0bf7f45d44cc30ae3153b29daccd9d65a3d1341a0df",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalShardCommandRef|ConditionalShardCommandRef.shard_id|shard_id",
        ],
        rationale_sha256: "599b03c338d53705ddb64d6037670fc13a8fb6fd6b811debb820a1eec188c29b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:448fd4c780118dedd261ff04c0d3fdbb837394d31386becb8935f9bb5a7477f2",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConditionalShardCommandRef|ConditionalShardCommandRef.shard_raft_index|shard_raft_index",
        ],
        rationale_sha256: "bb9fd527faa11b628dea2e470cf12b06dfe7083cd429f2d5ac2c64b4a7f54b04",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:51fde1eb845f5b657f68fc1295946c8f2077008b1f8310d772b449ee0959a974",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConsensusDomain|ConsensusDomain.cluster_incarnation|cluster_incarnation",
        ],
        rationale_sha256: "1522023f70bcc43fff1c506a9bee05eb566b077900692848b807d8b1568633d5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:06e4b97e0d741edd0bb8d618d48df3daf7f39f7ff5f2b1aa0d2571b1cede5c2d",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|ConsensusDomain|ConsensusDomain.database_id|database_id"],
        rationale_sha256: "d538a7cc25d6e50c870ef8b97ceef777eaa77f86eeeedce7191f501375d1cd2a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bc4a928f57e4004abc66ef087f1484f9d14c2fa3ba3f740e52124e669b091bf2",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConsensusDomain|ConsensusDomain.database_security_namespace_id|database_security_namespace_id",
        ],
        rationale_sha256: "46a27de3adb75ae64a393b259eb8670bde0861c7134a7c50768e767bd733db8a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e03210167c369f94d93dc4c8253d36d29ac78011864a802f5843834bf284c4fb",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|ConsensusDomain|ConsensusDomain.group_id|group_id"],
        rationale_sha256: "b13c064732562bbc675ef728bb7e9e54aa770178e27ec3af788c9922563bfa7f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:afbc344d70d6cc6795f928f6378d234d4c01c0738c49b479437ecd307b85b863",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConsensusDomain|ConsensusDomain.group_role.Shard.shard_id|shard_id",
        ],
        rationale_sha256: "c6830c9d5c07914f36af3cf7c63c62d4c3b725b4806bdffa24d343027fce256b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:5d0a9654f53322e956d33010ca7df5afc44cb67dbde765dcef51425db02dac13",
        slice_id: "a01",
        source_locations: &["a01:1443", "a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.encoding_id|encoding_id",
        ],
        rationale_sha256: "5d9ceae808ce92c1a975dc82de58dd612ec0d8d1edb774d3818149e5265240b5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d24695012d4044e09c759f8614fa9a032db2cbec2fc3c93e77ab749ce5a608d8",
        slice_id: "a01",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.failure_domain_policy_id|failure_domain_policy_id",
        ],
        rationale_sha256: "228188975e7d888e3e1c9c3b00c90f7d8adec3dc6acc2a4c9d11be49af2a940a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:43d2f0e65a4dc39f329e611ac733251f6188cb5cc544e93beef46e135474beab",
        slice_id: "a01",
        source_locations: &["a01:1443"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.root_placement_epoch|root_placement_epoch",
        ],
        rationale_sha256: "1f25cdc4c4879cb7b06287c0c9f58e92595eb9b4982a86451697aef9b49604b8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e1c52d6dcbd8885faf4eba1c8406864266ea081cd420b677c5467c4a97d9100b",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.final_retirement_floor_digest|final_retirement_floor_digest",
        ],
        rationale_sha256: "561a5d9616c2e70a8cc5ee78d51b861de3d29d6286edef3ddb12206916958a67",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:404a1f5ffca0b239e6d6304f019a320bfbb2f771e8d8f75075ea4aa41d31fbc6",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.joint_transition_transcript|joint_transition_transcript",
        ],
        rationale_sha256: "47781e05d21c6f5116cdeaefffc3ee4a0333cc4ec7a25de467e788f96d1d7f20",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6150ede1e1b9372211d8fcedffd089371e78f6ef8bdf839499417b25f27d9e22",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.new_configuration_quorum_signatures|new_configuration_quorum_signatures",
        ],
        rationale_sha256: "10f7e3518f34b10fece788335349504c7b6292f5406df19a1142d282dc9424c5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:af2b5edb93577dd590386a385295f80a7ae01d42de718097a430f50357d49612",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.Successor.old_configuration_quorum_signatures|old_configuration_quorum_signatures",
        ],
        rationale_sha256: "749b7fa989f192c7f4812415a50e98139073bdd73a00333b7deb951fb77b2738",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:299ee60ab1337573ea4994c60bdbe86a53c9be1ecf08fd2f189e0a4f025be75e",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.final_retirement_floor_digest|final_retirement_floor_digest",
        ],
        rationale_sha256: "f499a7eb9dbe01ef99398d25219ca3037e16e257776df36f1295c283372afc3b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3543ce46e3d146757ff29ca8552c7c2c03766d34c11e9c4b2e70e47446a29da2",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.joint_transition_transcript|joint_transition_transcript",
        ],
        rationale_sha256: "bba24db734ecbcc9476d82117845a6e7cfbebc27aa42241385b33335c2bde922",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9bac05f943c14519aa377b05808e57bfa117279607768581f79fb2720b221f99",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.new_configuration_quorum_signatures|new_configuration_quorum_signatures",
        ],
        rationale_sha256: "8dab59da54955f980bd0f270fcb46a1a7ea82fccf995d2240542c79454a860a4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:08cbc807047e544cd6c1e2598630e187feb142c28fc295cc0a7696e8d36dc68f",
        slice_id: "a01",
        source_locations: &["a01:1398"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteAuthorityConfigurationEvidence|RemoteAuthorityConfigurationEvidence.trust_transition.ValidatedCheckpointSuccessor.old_configuration_quorum_signatures|old_configuration_quorum_signatures",
        ],
        rationale_sha256: "ed8afd912cfda48219da80f500d6bf3061b46784c20b4934b8c63823a7bc49ca",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:107590809bb6e12167b2f2dd3e8f10051a169c3a28357753e2ddaeb18d7deb20",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteObjectIdentity|RemoteObjectIdentity.canonical_digest|canonical_digest",
        ],
        rationale_sha256: "96128449ef9d01470a3bff175a2a8a41f8e3b5d689b342ba2340183fb24dfdbd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bd6a0a183874f88db3ea3ec0302152530c28df45a9f5dd4897784ea8a85ec3a4",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteObjectIdentity|RemoteObjectIdentity.object_kind|object_kind",
        ],
        rationale_sha256: "97984b0edd24f51bf726aed05adc1eff3478b80e70e4e34e60d5bebb695851fd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b5662f79036ce591940334355f520642a72b0634d4a15befe4513b36e25d90ba",
        slice_id: "a01",
        source_locations: &["a01:1396"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RemoteObjectIdentity|RemoteObjectIdentity.object_oid|object_oid",
        ],
        rationale_sha256: "1ddef0fef2e9ff2e1b2cb105091f4a47d74ba0b79f021c8cd54bf6489a8ac6df",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6047adc7965b7865e94cb7668b9a58beefe4bc5ccad991358a95c9be0bc29aab",
        slice_id: "a01",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Meta.migration_nonce|migration_nonce",
        ],
        rationale_sha256: "baad8a4a8b67ebf7739535afce3e2bff6dd28980a15df5dadf8394de1ae803ae",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:20a8a347b7c249c90e09deab30ed8ff464b44114e205bbef6f1852afb27419c5",
        slice_id: "a01",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Meta.phase.Complete.seal_release_state|seal_release_state",
        ],
        rationale_sha256: "c775448de53e7ea8fdb1b9cca1d78b2b57df1a8b15e893bee0cf9281051ce705",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:ae9bc8dd611d9feec022771782f885fbc37718ed23bab487b4189f9841d51822",
        slice_id: "a01",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Meta.target_service_visibility_epoch|target_service_visibility_epoch",
        ],
        rationale_sha256: "630c939c56f204d8b86502c1db018fb199f4c4cea0624450aa537b1a47b03594",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6fa3a8b63ee9d48d3658e670f75400ea12f03518bd4fea540eda6f1d8dc62f94",
        slice_id: "a01",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.migration_nonce|migration_nonce",
        ],
        rationale_sha256: "21f4df4344ea91a4208af2a21d918d7bf0093f96396b4dd6b7dfa4a3fe7e6b66",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:530ab398113ef1b97858212d423b792e306d68ce291da23fdb3ae00831158d3a",
        slice_id: "a01",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.phase.Complete.seal_release_state|seal_release_state",
        ],
        rationale_sha256: "16788aeea876d42baceb64142198bb173ca95406197ae429ceef4e77c69796bf",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0dbcee60eff18a6b83fbfca6710c56cef320a2b09b9eacdd495000f524007939",
        slice_id: "a01",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.shard_id|shard_id",
        ],
        rationale_sha256: "32a40e29101904e460bf50b90db61288f7fd2fcd46caef93476402562d1f1fce",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:2b3f0d0dc9a723abcaf65c604e43b62aa250900a9806fc691b61ad743557b433",
        slice_id: "a01",
        source_locations: &["a01:1390"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTransitionActivationState|RoleTransitionActivationState.Shard.target_service_visibility_epoch|target_service_visibility_epoch",
        ],
        rationale_sha256: "54c637206eb1e2dcde1342e00914cffb4d2950b4b3a71fae0e15fc7490ec16eb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cb5825c40682074cd88d59caa2f46d67c9b6115be7ebbe0208b97da0b45d74c9",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.ciphertext_digest|ciphertext_digest",
        ],
        rationale_sha256: "6fe5cd945c6a3c1f5f06d8aea045fc1abe4655149222a4611240610258065c2f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:b7504277054b04c1cf2ceed042250c26449c0452fd4d7c8e99ea4b9f75bdb846",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.ciphertext_id|ciphertext_id",
        ],
        rationale_sha256: "7f596b70a70eb4fdbf7c715942d9299489eb7cb5e3d9ec34694443352604f2f2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:0d955780da7823b64d2ac4832293cb252f40d4f7d23421df903e14e41dc3cab5",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.dek_id|dek_id",
        ],
        rationale_sha256: "c17e17d8a2356f138728f86e4796b179ae3239914b7c705f8df0737b58451618",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a584b1e1a58dc60fff87a8bae2f09810790db317ac3222eccc0058ee781b6f6b",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.logical_oid|logical_oid",
        ],
        rationale_sha256: "7470b899cda8a48d40fba4817cf0c0eef0673afc7af19fcde2da0d8ebfb04cb9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:1e315e718c1dc57fdb221a62568115435bbd13f2273a677b758dcd6bc444e86d",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.protected_length|protected_length",
        ],
        rationale_sha256: "2cf51930e39b4c9fb212b45dd4746d56c4483a48fb6f1950e9127de134f50668",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:652c475ecabc6c9b76ce0f9c6fc7e5158ce7db6433cbdfd4edd2e8013d9fcb04",
        slice_id: "a01",
        source_locations: &["a01:1410"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongCiphertextRef<T>|StrongCiphertextRef<T>.recoverability_profile|recoverability_profile",
        ],
        rationale_sha256: "f796575e05d500347de2f4e0709dac5266ccaa876091be436f82861e7c419ff7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:bf14b1da59063849fedae6811820b7eac779d226996884d50707d7a33b9016e4",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|StrongCommandRef|StrongCommandRef.command_ref|command_ref"],
        rationale_sha256: "f18b450788697bf5e669f51b56f32e72d812cb9c5992c94a5b28f26977915eae",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6eb7a154015a5376477a8c84497a17f9adcb3359e3f533d0547e2cc9c40a8799",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongGlobalCommandRef|StrongGlobalCommandRef.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale_sha256: "7e81d86efeb1667bb556bf9dddd31a69d3f7ab6af9fed23b668f387d1b498cee",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:acf6c339daf3a326175b85aabd84678e241367cdc7b5c946db9157b194a6ab34",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongGlobalCommandRef|StrongGlobalCommandRef.record_oid|record_oid",
        ],
        rationale_sha256: "a8ab3495cd7c0db32013791027ea6859be4912e6e8744f4d88e3692b0216effb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:cd64b0149d18dc9934a66167c0937515a7618074d137a31ebb79344501932118",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|StrongMarkerRef|StrongMarkerRef.marker_ref|marker_ref"],
        rationale_sha256: "bfffb13326dacb23757b336fff689aa084f8cf622baf89861da75d1369e97812",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:3f8f3176a7718116cb6f1545bd688ad47df7968959d783ff0267d18843369182",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|StrongRef|StrongRef.oid|oid"],
        rationale_sha256: "9cf98dd7c7ea5e313bf4edea057a6f62d5c64841e48d5ddfc73e69853c70450f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:a6fb8ede356a5dcb049d10c0e248c928bc56e5eaedc09be55230e95f73c77310",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongShardCommandRef|StrongShardCommandRef.record_oid|record_oid",
        ],
        rationale_sha256: "c6d7d40af83ee3fd253f9aaa898663a35fc2240e1bd9a89a8d18500c3a3561e1",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6b31b163a80c3768fbba1b6cdbfd63504b7796fd719f38e655aa60e1c4c7ed20",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongShardCommandRef|StrongShardCommandRef.shard_id|shard_id",
        ],
        rationale_sha256: "447b5184b9c78bc7088cdd1786803176eb9521849a95b4e986000e081a0ef967",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:f84e8e64b2b57ad03afd6a8b703c0a8b7ed1979e0ecad941a601b2f9d4d39077",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|StrongShardCommandRef|StrongShardCommandRef.shard_raft_index|shard_raft_index",
        ],
        rationale_sha256: "09d4e56cc5bb381b45f72c526ba91ba767aef231d9b8f3839d8c978ec0cbf140",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:50cbfe932bcfbb0429e3ec0a3f025fff8b69d37555f229b96b6bff6191c7bdb3",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|WeakDigest|WeakDigest.digest|digest"],
        rationale_sha256: "df848358fba0f041e21a4426489c29ba1fa434ecc4eb21d6506b4f9426781692",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:139510a77fde9ead91fa0ec2678ea00991c4e70d1df84f8ab78848677f222a67",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakGlobalCommandIdentity|WeakGlobalCommandIdentity.global_logical_command_seq|global_logical_command_seq",
        ],
        rationale_sha256: "cb553d2c7443612dba1d067f45436740a34faed13f32164f88d09817b7960d39",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:7cad9b9c94e36c6ae8838f509fc3020a94487b0f21ebdd0de8116fbb71d64a82",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakGlobalCommandIdentity|WeakGlobalCommandIdentity.record_oid|record_oid",
        ],
        rationale_sha256: "c35439ba83b7ecd1b358be4e8505d78e9eccbeeb96743dcbe7e368bbbddccd1c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:9f08c1c75203bd3827179ad5f4a0310ca62537ca1fdc04fa423a491b8b53bf7d",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakMarkerIdentity|WeakMarkerIdentity.commit_seq|commit_seq",
        ],
        rationale_sha256: "d00350d8b9a0dc5106f3c467c1f061e72d991caa900cc7170fcbea98794fac4e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:e838c4e8c16f22e447b9f945df464fd6d6cbcccf0c9eaf87d67d9aaffd2dfc19",
        slice_id: "a01",
        source_locations: &["a01:1394"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakMarkerIdentity|WeakMarkerIdentity.marker_oid|marker_oid",
        ],
        rationale_sha256: "2ebd598cf713a51d493c27cc22124493fb393feb673c41dab3b125cb803b9247",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:71357f9cccbb9858750afa743c8c43e767f73a60ed20110a01be6f4aa2558826",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakShardCommandIdentity|WeakShardCommandIdentity.record_oid|record_oid",
        ],
        rationale_sha256: "6cc9226d342a63bd949a6d344acaa489d19efdddb3dace036938aa01cfef2602",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:d8cf9ed9c05e68dcf47540f49cf5e166b3e8a251f9c64c5f62afd135b057d125",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakShardCommandIdentity|WeakShardCommandIdentity.shard_id|shard_id",
        ],
        rationale_sha256: "652c7cbc9b1449d1e2abed0cec7db7ab17236610dd60d8959e92f1a5da3451eb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a01:ambiguity-adjudication:6025faae5c1c51f71fecd00802a3336e4a8d6697a2073a9bab4340fbc38bea46",
        slice_id: "a01",
        source_locations: &["a01:1406"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|WeakShardCommandIdentity|WeakShardCommandIdentity.shard_raft_index|shard_raft_index",
        ],
        rationale_sha256: "398a2e78216d60cb50769edc47c7409f984b19dea61a294d870835d554945074",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:460202d74810b05f3b369b22e66dd2f8b4a2ee578ae502a4359404a71187455e",
        slice_id: "a16",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Local.current_root_manifest_and_slot_identity|current_root_manifest_and_slot_identity",
        ],
        rationale_sha256: "7eb064525c178d613203be3f80a97b24b50c95395829c4c435dbe031ed61b2c6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d84bc693fde08ebdb0e53e54492f602de34d43c66e1286a0c750b8c2eee55df8",
        slice_id: "a16",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Local.writer_fence_epoch|writer_fence_epoch",
        ],
        rationale_sha256: "1b40770365e2d8e2704e29a00e841d67256817abc7df84d92ce862c5cfe2e330",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:87dfaf6e9cc2b8bbde832f18efa9fa3fb950f304d9dd594b8d9e3aa230e9a2d9",
        slice_id: "a16",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Meta.current_root_manifest_and_slot_identity|current_root_manifest_and_slot_identity",
        ],
        rationale_sha256: "cedb083bd71c8416e469437cee6088d0463eb174d6f653ce3d0067faabc27ebd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:40f683d4ca2d305185386c3a0779f4a2d359d5d3164c77de96ce2995d4237908",
        slice_id: "a16",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Meta.writer_fence_epoch|writer_fence_epoch",
        ],
        rationale_sha256: "f9397457496da8a94a35ae3d26fe50f4de9714f68bd032611aeaa5efb4eba989",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ce48b3992c5f79c6151433b0ac626110de8d73a50c9bebde4be1cbcb81201af7",
        slice_id: "a16",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Shard.current_root_manifest_and_slot_identity|current_root_manifest_and_slot_identity",
        ],
        rationale_sha256: "d81e287e9a21f1c243fd4926901a7939d03864ba689f1bb3083a8ec14b04a8ee",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f90c20349a918a0e970ca304cc68f78df1783ea71ac6293d72f2fcc573aa80e2",
        slice_id: "a16",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Shard.source_meta_prefix_and_configuration|source_meta_prefix_and_configuration",
        ],
        rationale_sha256: "d55a25329823a6929209738cef04a04df5444309d86831e1602a22ca0363efbd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b7eb51a05d5214237786f8e4895601cd889cd204a9fdbbcc964daaa2d3b8d58b",
        slice_id: "a16",
        source_locations: &["a16:2225"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ContinuityAuthorityCurrentBasis<Role>|ContinuityAuthorityCurrentBasis<Role>.Shard.writer_fence_epoch|writer_fence_epoch",
        ],
        rationale_sha256: "947b6987525f9858fbba24b95ffb5f662487c3f5f52e970383cf15ea582a1311",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0c0b4a16ce9bdbeccf7ec454b8e52e722e171bcf812d4c4413aeb99efccfa625",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.archive_authority_signature|archive_authority_signature",
        ],
        rationale_sha256: "64c4b31bf006cd397320f1500eafc0265371d04fb7a8f86910f60805c4700fa1",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b2dd91a2b492e9de178a0dbc850077f422ae4397974f295f38128ee4d3be64a",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.checked_domain_mapping|checked_domain_mapping",
        ],
        rationale_sha256: "6bd13f975fd51468391ee4f4feebca33454e883524a51523b8d11286ca92b0a7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:55f3b93319481ae5515b4979cbc26b3e5cf0fc60b472b45749710d0654a7ac77",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.dual_signed_transition_proof|dual_signed_transition_proof",
        ],
        rationale_sha256: "5c7b0fc39b1bac5e2f6c4664362889489048883fe1eae68fa0b9c8ca13fa7adf",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4604a342b681ed605409625ef352daad3d54bb49066723f90210c5ace15c52dd",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.lease_identity|lease_identity",
        ],
        rationale_sha256: "6aeb8246e88dc181ddf41b89a0ea31d7f484d47c4e824cd9981f81c29f11ac9b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:134d0d4d4be731d4cf4487b042d933a79b058435563c7e9211e3f67d19dc2bfe",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.no_gap_coverage_proof|no_gap_coverage_proof",
        ],
        rationale_sha256: "a0ba4c5ca27308fdf4c0064a623fb8644251192a465d373a7ca70884e01f8c7e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e71935c154a5a543fea65cb5dcf070475bffd2fb48f6867e2aeda5bbf4bec6b5",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.old_and_new_profile_identities|old_and_new_profile_identities",
        ],
        rationale_sha256: "d1f0909156a03f43a1b7d75db31916597a86c221212a9156ebb343d7ed1bf340",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:49fe4ea025fa3b41520aa2c736d9cc92c2ef522a87438d53b4eddce5c15404c8",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.CrossEpochHandoff.prior_and_successor_generations|prior_and_successor_generations",
        ],
        rationale_sha256: "c42a43e39138561558f3021c42c3ac375de5dab7a027f0cedc4d5eb5c4db005c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f87519086a8f536dc713ab1365a4bfb83c62192a8d0b5ce8cf81771d2963fe76",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.archive_authority_signature|archive_authority_signature",
        ],
        rationale_sha256: "c8a53560cd3612848b166fae9c20d3d6f6cf47e53c64c08c2e424e44d8c6ba09",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8d2ffdea256c376ad5b0006705b27ca6b51ccba5006c908129895cacd22ab53c",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.checked_nonshrinking_bounds_and_counter|checked_nonshrinking_bounds_and_counter",
        ],
        rationale_sha256: "b6efeaa86c50e5a8985eb08187a3636bd57a35a0affd33ddf2794d69b2f781d5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fa0cd7bad70d2a11070d4ac7aa07c6e9594bb9a2b1b1420b8b80bfc7457471de",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.lease_identity|lease_identity",
        ],
        rationale_sha256: "902a6355b382f229699dfee6787e6e335e4526c28c3e82a896e1281ed2b142ab",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:829355fb5b65191691f8664c311fc856ec13d034e717af269cea446ad14c4216",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_bounds|prior_and_successor_bounds",
        ],
        rationale_sha256: "4acb63b2f6db45f0aaa557942d3b38156152d3a1a0eda5450cfc8a3dfeb1b25b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8ebc993bcd786c4f58842b7f4a0482a6606d5a09b3569a869089a37f43916d4a",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.prior_and_successor_generations|prior_and_successor_generations",
        ],
        rationale_sha256: "d6e204ebb0a3a275f655f7f4e0ec97ccc29dcdc00999fbb64c0bad0a16e3ebae",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:85dbeb0516fa252e9be5a5de75cd32741741e2125a82a450fc8729828675599e",
        slice_id: "a16",
        source_locations: &["a16:2235"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LeaseWindowSuccessorProof|LeaseWindowSuccessorProof.SameEpochNonshrinking.same_profile_domain_epoch|same_profile_domain_epoch",
        ],
        rationale_sha256: "4b5b62b175ce1d4515563a331c82a54951dd3ab97ec8d14f6159a1976ab514af",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:631e05ec22d5bd0bd47535c6abea08a9afbf9bc2da7cf2fa67d12ee696cdfcb7",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claim_id|claim_id",
        ],
        rationale_sha256: "4d788848db1c871efbf585e576c70b99530618a0bfcd54e21f4d3046dd760d12",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:684dd613cf8d1b1d36038b3422de453aef92d6186e5e77525f46313e3752d1cb",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.claimant_identity|claimant_identity",
        ],
        rationale_sha256: "cbb1d4e6ecef9d88316e4b5b37e62b230a63cffb913ae13437b4787310ef7ea4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:351b5f727acb44a2cc22dcf886346f73878a40a476ba55b19266bc15b0ead3b8",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.directory_bound_creation_evidence_recipe_digest|directory_bound_creation_evidence_recipe_digest",
        ],
        rationale_sha256: "b2bccb81528ed06d1e76e88a07417555853d0140ecfad4773c981f052112eefc",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d51096cf7233f6066f047ab25034303514696d9f1745cd29920e9630706a3a7d",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.filesystem_profile_id|filesystem_profile_id",
        ],
        rationale_sha256: "2ef4e158b2bce3735b1b29343f267a67e3436d2fac71983e44114cdf99b00d36",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:db72c08571d804730fdad3aa7a4fee4ad481a4e9670b5c46439cd44392a7c819",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.fixed_local_continuity_recipe_and_digest|fixed_local_continuity_recipe_and_digest",
        ],
        rationale_sha256: "6108f41cf3dabb14b92d259c36fa1a75c7d49207d5d721d6d4e5a01f4fd5a6b4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:59e07a8fce6020b3269432fb6b921c8987cb1938688030616a2f5a342f2a62d5",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.mount_device_directory_and_target_manifest_inode_identity|mount_device_directory_and_target_manifest_inode_identity",
        ],
        rationale_sha256: "74250f78704f2db0d54692f0a421e968faab503fb4d2ad6d1dcdd487a88c04b4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:66fda3d55f20ffbd14f2a44e4671916cb011e2911f4415fc6b7878dbc49d8df2",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.parent_directory_durability_recipe|parent_directory_durability_recipe",
        ],
        rationale_sha256: "bd06383d68139d7254d6cff79323d58b831dc97dd8b8e16344b0da1daeaf4c2f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:710de412e21d880e486ff97a204979df9b5f70a5b586e886d210c20c4617b4e6",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.target_manifest_staging_inode_creation_nonce_digest|target_manifest_staging_inode_creation_nonce_digest",
        ],
        rationale_sha256: "03a56fcf2fba046727447c31b3e7d385dde92e52d5722224e5223349a86730cc",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3cb12c928a14326b5ba0c022926f4558be38a6ff547679081daf0d3e498163d4",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.whole_inode_fence_identity|whole_inode_fence_identity",
        ],
        rationale_sha256: "73a06a3266bf7eda8fafde3ef40856a0e47ca93978fc613840f5ce065eb56e7d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:31ee4abcd164b58e192c76a563d2ddb710c49d224988e73ec9356854643b5400",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.DirectoryBound.zero_existing_root_slot_proof_recipe|zero_existing_root_slot_proof_recipe",
        ],
        rationale_sha256: "8ecd464887220923eebb457481d3931cfa0bb580f678f722432dcf9f92841460",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3c53ffd2cdd6a62ddcbd58a34a2a90572ba753ab1df0b4b3e713f47ca11af4e3",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.enter_clone_reserved_operation_recipe|enter_clone_reserved_operation_recipe",
        ],
        rationale_sha256: "2d0bad10018d45c0557f1b8f1f7e4bca183f7097de58bfa35d723dae81c17230",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9412f6cc59223ce1a66a34d89a55e685ef653105e6973d6c11215986ca6b9ca0",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.expected_continuity_predecessor_digest_and_cas_version|expected_continuity_predecessor_digest_and_cas_version",
        ],
        rationale_sha256: "58fa2ee7fb1eb8152eb86926cc4cbe69ef44616147823913917413d2e85530c9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8c4b21b6111a8db073ffe5a7a4eedc10e207ce1654f25df674e05a2de987125a",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.continuity_profile.ExternalCas.recovery_only_lease_recipe|recovery_only_lease_recipe",
        ],
        rationale_sha256: "bc3cded0731c99c2185d1b433d5c18d2a0b5360de816c4f59b2242962f800175",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:01d951dd11d6fdaa8d31623cd9fd3539f67d2fcfbbdf285096fe10e57dc62baa",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.expected_reserved_head_digest_and_cas_version|expected_reserved_head_digest_and_cas_version",
        ],
        rationale_sha256: "bb7158b6bbc771e29112876f3d3d5f2a13c54b6ca9512cfea60d09f660bdf5ba",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:1c12d7fe225198c1beef3b67c21b25d50ce94eb39c141f0000aed764d117f1f6",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.identity_reservation_id|identity_reservation_id",
        ],
        rationale_sha256: "55a2197d96367676bc61bbb30f4e36132e3b3f52078c4fdd880c88dcbd4b0bc8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f6359a728522f27ef940fb7dd42aaf5347f9d0c6abe54b411fd0830de7ff3166",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.recovery_writer_authority_digest|recovery_writer_authority_digest",
        ],
        rationale_sha256: "860ff90be02477e880f0ccb185337ee9cf193ff183d2ff005bbed942ee8189a0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:cdaa27bf799002729bb11c5025f5d7e89547de30c3d1a4d0d837d82f9033eda4",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.service_visibility_epoch|service_visibility_epoch",
        ],
        rationale_sha256: "c5a2285cc806a6c8e4c7592845bfcd81adc102cbd06fabf9c7bd223cb9a51f23",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0560e2a9ebe97b9c931583bcbf356f270ce83d735769b1d8b38f2868a68ec1c1",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_database_and_security_identity|target_database_and_security_identity",
        ],
        rationale_sha256: "16f607e23a7b52ca7d13bc1fa8258e9500de88579b0946e3f96bb9c7a771b679",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:6343dff659cb84cb85d6b0f03a664cd385bd0c8157c51f995279634d6d013393",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.CloneNewIdentity.target_incarnation|target_incarnation",
        ],
        rationale_sha256: "66f33586dc9a2939e3802b7c77b087f3692e0fdc9782e2a1b7e75f40cbd5591e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fa8524d7c15de0f18cd494343c8fcb407f67fa565408d7adbe01ef387b1b7a93",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.expected_operational_or_fence_predecessor_digest_and_cas_version|expected_operational_or_fence_predecessor_digest_and_cas_version",
        ],
        rationale_sha256: "dfda0f7790d25015b64480fbe2522ebe59615a0455349b4fa2ed639c8ede5401",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e8bf7c31fe0a21de42bdf6e964bcf7763272c4fb6b7baedbab94cbee0e13750e",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.planned_target_incarnation|planned_target_incarnation",
        ],
        rationale_sha256: "3ca4c6841b2be52f553ce0af0af78f15a40fd816c24e4886f6fccb77ef1b7e84",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b5b4e394a7eedc0c7561d3bd52ecaa6bf8a74810759f105c9e56e85f6a012d6",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.prior_lease_fence_plan_commitment|prior_lease_fence_plan_commitment",
        ],
        rationale_sha256: "13db865b611d9d9bf03cb84d51442007db6062d9b29f915c4ab2ffb3b2b12bf5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:44c745c9b2fed3ed6f450d0c31b581e73b86745cdf4d0533e4092c82e0ce5365",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.recovery_writer_authority_digest|recovery_writer_authority_digest",
        ],
        rationale_sha256: "f620d01de21925752abcfa32b01cfcf8713ffb1d401a9f3894a47cb17ef750e1",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9e773b8e490ee9a2dc1d72cd77ac727e2912b017c4009938b3e0ee2515f5f6f1",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.restore_id|restore_id",
        ],
        rationale_sha256: "ad66acb1950192c80a5ae063e5d14b9b8274426ea2a2452ee14668cb049a0dc6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4cac0529f2c92be614e26bf98adfa7d72d02a2da52f9875469ed1f10a5e71217",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.service_visibility_epoch|service_visibility_epoch",
        ],
        rationale_sha256: "80ae3e1f2342e3d76ce485a911cc07212b5d3546a0a9f1b64e959667c371cf13",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f32accebd587a2bdf96b2c104ac49bbd63086b664565bf3632aa97cd46327239",
        slice_id: "a16",
        source_locations: &["a16:2245"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreClaimedTargetAuthorityRecipe|RestoreClaimedTargetAuthorityRecipe.RecoverSameIdentity.source_backup_identity|source_backup_identity",
        ],
        rationale_sha256: "777728aaf3be0c443b06c5fcff32c98550a30f7f6a64b93f7c7f3dbd1a307557",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:eed1694e00ba2fe12597870d419b8963e8ab25f286676fac24006a93fbbf3354",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.distinct_namespace_proof|distinct_namespace_proof",
        ],
        rationale_sha256: "549d5fd532b355cc6b6d8c082251c33b69d617e635f950006bcac5c9f204895f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8b134d2ad5fc8b1cd4eef543a1b695b76ead1ab868350cb12a1814eaf13fcee0",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.plan_digest|plan_digest",
        ],
        rationale_sha256: "e3897d252c2f56ee01b24bb128cb2c75a1e4f2582b7cc6455428bbdc8fe9bcd4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0ebce0aff84aa60ca53776c1924be86270c3f530e73ccc305f4289d3e2282d66",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_k_oid_source_open_only_commitment|source_k_oid_source_open_only_commitment",
        ],
        rationale_sha256: "8c766269c99caf3c600c574b13c8fb05aa264162120611342e8432605f4c7037",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f79e5b28c484822b5d2c57cedff5b89df5eacaab7356c3354811dc31d886164b",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.source_key_nontransplant_proof|source_key_nontransplant_proof",
        ],
        rationale_sha256: "06701e513379f45194217798b6195af59e1b1db7acd61d86a789ff4f49d62fe8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4b15f18ca33a6bbd9618b4d86f6433d4edd1017e5aee708689ecffdf6fa71353",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.ClonedFreshIdentity.target_k_oid_commitment|target_k_oid_commitment",
        ],
        rationale_sha256: "029ebd78f13dd453876f7086a3e320587d43329e00585ff1c8673c8f634f8b37",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:aa8fc751eab932565dddac78db025e788a38646928ef0d5fdaf24f1e33f2f562",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.identical_namespace_proof|identical_namespace_proof",
        ],
        rationale_sha256: "837e14ef7d4aa91a80a4ab3ce167545d058f2b965b34841cccd6fe3610fe65f5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4f9b9e06323cb762ffcac15cf6a71b19ae9f4bc981caf5ffbf4acbdb5d8c7509",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.key_equality_proof|key_equality_proof",
        ],
        rationale_sha256: "83d25093919bf05908a5c5db7aeb65fe94f5419098a506240c68acea2a7c35dd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:a06189964678768435533d90ee460e107015578db70cd22f78b6b892f3f56974",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.plan_digest|plan_digest",
        ],
        rationale_sha256: "131e25d9b5b5b9ac75212fb00e5062e467a8dac7a1322534c98e01be6d21da02",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9560b9c40af5f326f253491aafff7807c693ff33021e16b3b17b466c128224d7",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.source_k_oid_commitment|source_k_oid_commitment",
        ],
        rationale_sha256: "293072de75a5d5d05a0e27823b6777302e95f9ca8d888559dcc32d7d9386c1b3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:733122f0708df91db6da6ba6643b262794c139f839bf00461e87caee8c9e32e8",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.target_rewrapped_k_oid_commitment|target_rewrapped_k_oid_commitment",
        ],
        rationale_sha256: "5a2f04ec39512d62400376469c086bd5ce9f3db2d42aa6e0d5e79ce29aeebd7d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2f946d8a8ebb11b3383ac3c166032aa77d9e95a55135417bea7fd2ee2ac13b34",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyDispositionEvidence|RestoreIdentityKeyDispositionEvidence.PreservedSameIdentity.zero_plaintext_persistence_proof|zero_plaintext_persistence_proof",
        ],
        rationale_sha256: "f15d224d5332f3a0f14bcabea2f8420a7446583fcd81c283ac0c83119375c4d7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f2a2e6ed9f1043d3703e55e55632f3435d305711b0cc99e87d1e620885911483",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.create_target_k_oid_operation_recipe|create_target_k_oid_operation_recipe",
        ],
        rationale_sha256: "fea4e3c149aa438419423943a281ef6cded597c327bc1c67c964839f1e606e4b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f3d13d2ca5b4dd2c2112c77da17209411b75ce69ccd1af10cbce488650a94d46",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.distinct_namespace_basis|distinct_namespace_basis",
        ],
        rationale_sha256: "1cb4140d7f1c7f927e8c8f672e2498e04583d27007468bd3e248bb10e7b4bc7d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b0f6e5b9b71c069852886e98c69a257f258d0d87bf695be73b35d6cb8c223713",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.recover_source_k_oid_for_decode_operation_recipe|recover_source_k_oid_for_decode_operation_recipe",
        ],
        rationale_sha256: "e4615dbce9c06c75c9459d7197cc66b87af6806c587c6ad771c1abb60089b494",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:30dc167c4493d4c51e751a3d7e77af70c9c9a0e457eaf00ba57fff01dc0d235d",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.required_source_key_nontransplant_proof_profile|required_source_key_nontransplant_proof_profile",
        ],
        rationale_sha256: "0a3909675268381d337de1523dffc757246600d7b53dcc07c83222175528755a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:425f9c6202f370aebe6ca60eb1c8ceed11acfec76fd0d431e97a1ffd9684f5bb",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.source_k_oid_source_open_only_commitment|source_k_oid_source_open_only_commitment",
        ],
        rationale_sha256: "de766bcae44442e4efdc18226a52fdc7464efaf1bc8f05962cf13996b8add03e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4408fe8489bf3b8a56e92eddbd6a52f360583c3715cba3b3f83fb452245dd9aa",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.CloneFreshIdentity.target_k_oid_recipient_commitment|target_k_oid_recipient_commitment",
        ],
        rationale_sha256: "dafa380aafebaaabb5e211a30602e09817e0a82471319d95bc13179ecb0bc6ce",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:428f4a28cda62c0d2fe400327ee5371384d435132bac156a1160119d2ba2adf9",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.identical_namespace_basis|identical_namespace_basis",
        ],
        rationale_sha256: "b8c6fc9126f1c93e7392f0aab8bc3d2fedf74f3dd4edb765a0d23b61fa169fc1",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:5808d0f941f3989cdc6937d82303a4f64634837f2be819f4ae0151e111a5b1de",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.recover_and_rewrap_operation_recipe|recover_and_rewrap_operation_recipe",
        ],
        rationale_sha256: "f5d602edd33f74a3dc8f7af2eebbb73a3858459d68400d5d96a2f395abe48fb1",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:31cc6283f91ff4bf1c4175dfa89f4f612aa184914b4412d0aa7375a884ae6199",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.required_key_equality_proof_profile|required_key_equality_proof_profile",
        ],
        rationale_sha256: "36e8c9ecfbc8f46d6566ac82be062809550cc864cea12f794ebfc22d7d670839",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:a3cf39ce48c28bd30f39483bfc16e055768701adb3ccdfb88281f1aa8f2811cd",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.required_zero_plaintext_persistence_proof_profile|required_zero_plaintext_persistence_proof_profile",
        ],
        rationale_sha256: "54b04b0344aee24e8cff099a20ce84b3de2ca92cca028f0b976b6ca51706ed26",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:60b0d02f6527985a2ede2d69f167c9ba7a85fc7b418df1e308f9bc9ae5ad4ebb",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.source_k_oid_identity_and_commitment|source_k_oid_identity_and_commitment",
        ],
        rationale_sha256: "feb68a98a948f2d610ec36c67917a6e12b722bb1e688fc0e32d72d62e9758c83",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:30eb8375a7e592e229c0fd3c2610ca145d0b62e57e3f247d2234b443b3f7b01f",
        slice_id: "a16",
        source_locations: &["a16:2243"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreIdentityKeyPlan|RestoreIdentityKeyPlan.PreserveSameIdentity.target_rewrap_recipient_commitment|target_rewrap_recipient_commitment",
        ],
        rationale_sha256: "e8c602f03d8be85af205a18f86fbe9e92a0d84bf3a0101e0d616b0865cb79905",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:51e1d45595605b09416540d8f2f7bfc577e100c7cfc34d62c3f8d31ea3b4cdf5",
        slice_id: "a16",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.configuration_set_digest|configuration_set_digest",
        ],
        rationale_sha256: "f3a0bee368e15dcfa3182f8538ce31f7d97184a5db00e424e476b3d12caf5edd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:63fc7261e030989938c367bdb25963327b60d9ea1d9d24dbb85d7d2dbd41618e",
        slice_id: "a16",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Local.root_manifest_and_slot_cut|root_manifest_and_slot_cut",
        ],
        rationale_sha256: "014c06a64be8afdc117cfffb6ec0d519c9e06334f82919271299a2ba5566b00b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:216034541841b4dc0a18c6a9056d541edf9220e6baa68bccd57d9ce2d8447dbe",
        slice_id: "a16",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.configuration_set_digest|configuration_set_digest",
        ],
        rationale_sha256: "8690d587f3e3c33ee6ba87f89f2dd9b0255e152830b1efeb8fe47b7f91dbe831",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:80e7fb9081083f4cc82c9ab584105b308e38afb8978ba93b8c0e9b4a5fa010e7",
        slice_id: "a16",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.exact_current_and_prospective_shard_configuration_commitment|exact_current_and_prospective_shard_configuration_commitment",
        ],
        rationale_sha256: "6e6a1f30fd6e69181d5bfce0072d1e2cc267b94152a223153bc66d6751701537",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:dbb7edba8ea7628b68f176f2fdcf74df26e3460a1f61f937b6ccbe9ce860dd87",
        slice_id: "a16",
        source_locations: &["a16:2203"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>|RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>.Meta.root_manifest_and_slot_cut|root_manifest_and_slot_cut",
        ],
        rationale_sha256: "60d80aa01228b01641efc8f94796194ebc24344552bb2b7079d6180666f1652c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:6dd465c2c940014a013bc003a9e661deadc21487ac6313f05a50cbecea9097d9",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Local.checkpoint_ref|checkpoint_ref",
        ],
        rationale_sha256: "8d0565e61764ed0a45186621750f92398927f681cb8a9e57364317282edcada2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:902d267153d8579147f02b8f03e991d7c32f9d7209fff31de61ba342ed72baf4",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Local.config_payload_floor_ref|config_payload_floor_ref",
        ],
        rationale_sha256: "2fad40677cba10e5b344f92de01e7f12082efc2b103b1082f415b0c7a65ae131",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:483a3d514dff97d1044b68ac3509def420cffaa13437bc8473568f75218b61ac",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Local.configuration_ref|configuration_ref",
        ],
        rationale_sha256: "108e60b349f8a2a95c6ce3cf76151e67f6fb89890e9f64696c9933d134f39cf8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:1f20479cfd8941a0d1df5dcfce62960e4f5ef9dd4e3d41b65abd770d83777805",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.configuration_ref|configuration_ref",
        ],
        rationale_sha256: "e83db05e938ade011368c9b9a080213fd62c213d20bbcdad9902db24870d3533",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:88ec0918d92724ea23c58210e0e16c0c04742f08c40f324c78c8a8f139dee4e6",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.global_checkpoint_ref|global_checkpoint_ref",
        ],
        rationale_sha256: "152ff623e48433640b470cf76346d0288cfa1303fc5c1679a0cf6a358bb2a5b0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b15497009f309c87fbd8f21e8d7d8c6242e4ab3b226e7da1e20b5ea465f1348c",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.meta_config_payload_floor_ref|meta_config_payload_floor_ref",
        ],
        rationale_sha256: "1440827f3f8f3e59525cf78825670889a8903a26873e466927866e950ee69923",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:14ebc1cef7254371a9290cabc9931aa7b0a06c7cb57527b89d15186776e7ec50",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.shutdown_receipt_ref|shutdown_receipt_ref",
        ],
        rationale_sha256: "69d72fc98972a98a4aaa46f874d6933e09f473fe1bc211ea2527964c33e92954",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e91e24a51392d6954d25959e9414a9ad41fbb3a557250c1c659af72b8d65fe35",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeAuthorityRetirementFloorSet<Role>|RoleTimeAuthorityRetirementFloorSet<Role>.Meta.verifier_retirement_floor_ref|verifier_retirement_floor_ref",
        ],
        rationale_sha256: "312362c5749562f69386ca63b3ff1a2a0d9f9841e981f44c8df8813fd6607256",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b16ef0d3622c032199d593c58ccb018188ef9ba2bb7f11a09fc8be0dc80bf2f3",
        slice_id: "a16",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Local.closure_digest|closure_digest",
        ],
        rationale_sha256: "517864b6a0aefa05a36e3c86ffd74ecc7da2eda9697e5a7fa0c5e10b4595e352",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:139736010760224ea01da42ec72d64e00f649760b94b82519832db88c20c87c4",
        slice_id: "a16",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.aggregate_maximum_expiry_derivation_proof_ref|aggregate_maximum_expiry_derivation_proof_ref",
        ],
        rationale_sha256: "83a2965b37dc67afc0ab0214ae6fbeaed9bba1ce39d7aae553700f6988a29f05",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ff4db66414518f7f1180ae56f9e0e1f62eff4eea91062e2e1a4e6021735c4525",
        slice_id: "a16",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.closure_digest|closure_digest",
        ],
        rationale_sha256: "f7b6b311a9e001642c9202fe6484a3b73b763e078120795f4d87814e0a01ad42",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:198d323e4bcdc1582896d21ce07fa233d15a72464c6d5234a96b053e4af44aba",
        slice_id: "a16",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>|RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>.Meta.configured_group_inventory_bijection_proof_ref|configured_group_inventory_bijection_proof_ref",
        ],
        rationale_sha256: "ea0e796e79794bb505aadc69febb2a99354f131bd5461280b6ca6954a75161fc",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:36978d672f27a9404f4f2140e7de7b1f7d3cba5ba7395ba6247431031c873127",
        slice_id: "a16",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeIssuanceReservationClosure<Role>|RoleTimeIssuanceReservationClosure<Role>.Local.own_complete_bijection_proof_ref|own_complete_bijection_proof_ref",
        ],
        rationale_sha256: "9815b3be3b9f64a7144e6f815c96bafaf169112498f5f50ccd9449e578c851da",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e6fe389daff9216daf5eae4a5e9f60371cfee06c2285bc5d94e43ae3d62cf9f2",
        slice_id: "a16",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeIssuanceReservationClosure<Role>|RoleTimeIssuanceReservationClosure<Role>.Meta.configured_group_certificate_bijection_proof_ref|configured_group_certificate_bijection_proof_ref",
        ],
        rationale_sha256: "3c9a7b397eb311fa6246c4fba53ebca999cbfc41ead1b3b4dfb0ac9b64ed5430",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:22afadae2a28ad5464ce9a4779f1939a81035f3992249a1a2a7f9c7eddb44e07",
        slice_id: "a16",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RoleTimeIssuanceReservationClosure<Role>|RoleTimeIssuanceReservationClosure<Role>.Meta.own_group_certificate_ref|own_group_certificate_ref",
        ],
        rationale_sha256: "a365920b731954708c045ef1b10d1a3635d31f41d60bbdede41cb3e5d9553870",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8fd0bb570ac876054b677e9705401329de512801ee439276dc674a407df02f8e",
        slice_id: "a16",
        source_locations: &["a16:2239"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Bootstrap.bootstrap_projection_identity_and_digest|bootstrap_projection_identity_and_digest",
        ],
        rationale_sha256: "26f1fbc24939fcd699f4bbdc110ef2ebeba37c573b467bb429099f721e74429e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:42d2a629736e07406ef6cec496247a08477fd4f24713e6b7f51b0756b4f5df63",
        slice_id: "a16",
        source_locations: &["a16:2239"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Bootstrap.source_lease_projection_payload_identity_and_digest|source_lease_projection_payload_identity_and_digest",
        ],
        rationale_sha256: "b436c746843cdeea4a5086791cd3b0f30059b9a8016ee83d59f393e199c9605b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3662fa006e86638ee2ff3041186200f8ef75e915a62142622b013db772c927b7",
        slice_id: "a16",
        source_locations: &["a16:2239"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardRestoreSourceLeaseProjectionSource|ShardRestoreSourceLeaseProjectionSource.Refresh.typed_meta_projection_payload_identity_and_digest|typed_meta_projection_payload_identity_and_digest",
        ],
        rationale_sha256: "9a49497601c2aaec9361ab7f594125531654a37ba3399a188fef06b21c1ee507",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:6e7c4fbf5db3c3cf58224f222c31bb95b20241840a3ce0bb89ccb3e664407b8d",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.current_configuration_writer_fence_and_publication_commitment|current_configuration_writer_fence_and_publication_commitment",
        ],
        rationale_sha256: "ca12351bb8e298097279ff8782d8c6eb92ecacd8aa56018437cc38b22c08cbc7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f84e5724e1fd386f4850343edec20569c16d3b71309a61c85c66fb582b071114",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.exact_profile_registry_and_transcript_match_digest|exact_profile_registry_and_transcript_match_digest",
        ],
        rationale_sha256: "430ee543d3f64f7a8198fe32626730b4ff176db60f219a1adb071bb0cb4d3e6f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:eb4c402fb998455475a495a575802f161a3caf327e654e1e2a2c6f0b08b55f15",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.imported_at_maintenance_identity|imported_at_maintenance_identity",
        ],
        rationale_sha256: "b97cfed9816fbc5ad77b64d06e86eba9ad8eaf64e7adee757434bf74e49741f2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b88dd4fe39941051f81018d2005d684b7e0a9beec3622d0a86fce28dbb6f690",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Local.role_and_group|role_and_group",
        ],
        rationale_sha256: "3623488b9a5ab9e394f1b2dedf1ef6b067b61fdb9d03875278e44268f5544280",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8b5c7f1c7920620c2661b436a184d102ac583a25c8f6c251e25854579a6cde97",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.current_configuration_writer_fence_and_publication_commitment|current_configuration_writer_fence_and_publication_commitment",
        ],
        rationale_sha256: "f492412c1dc4ed133d9d35ec694e9facaf3705ac73d3a3746c091242d16be14a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:77cad945943965f6823afc4ce110982fc90b3f1b7bb55d835b30d45bf74ef876",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.exact_profile_registry_and_transcript_match_digest|exact_profile_registry_and_transcript_match_digest",
        ],
        rationale_sha256: "4b659628163e339c109405584f45e1f3adeef667bec52c0c87daa84b2d4af494",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:bbe2ed9b1b6f16c3558175619522d9be6d442f099cc198c0d6b5e8ebcaa55cc0",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.imported_at_maintenance_identity|imported_at_maintenance_identity",
        ],
        rationale_sha256: "301b0500e6db1bb44a51357412d2a15a8779d1c7f9370c39bb913ab44d7fce38",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:4b38e383af6cd3b3152ee3fd60ab247bd488ac0fd1921055815d572c6406d0de",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Meta.role_and_group|role_and_group",
        ],
        rationale_sha256: "0086228e3f37a68bf595df614efe2c66961c2112ba10b685736d9fe843231ebf",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8682f99fbb0811dea1a3c1ff717c7f6e4692127e146efa48a985de59e93214e2",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.current_configuration_writer_fence_and_publication_commitment|current_configuration_writer_fence_and_publication_commitment",
        ],
        rationale_sha256: "52caf5606b8bfbfb8725dddc06b28a6c311a4ad358bdfaaba119a03ceb79da0d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:53da05c0fc82cf4701fb292f9907292c9088e287c6779e836840352920925ddc",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.exact_profile_projection_and_transcript_match_digest|exact_profile_projection_and_transcript_match_digest",
        ],
        rationale_sha256: "aef89d283e3de5742defc0c8a32ccc42a24902a886ab27a103272ef6372211d7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:300d344bf5abdbd2ba3786dcb2f2101f0f44a7aef6794b192d392bc0d5f8e2ad",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.imported_at_maintenance_identity|imported_at_maintenance_identity",
        ],
        rationale_sha256: "0a63e8af78d3f482bfaad9101ff1321c8cb5980f43eacabac93ef728d2ee6367",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:60b6f049cbffd66661578e441fc181a08f56c8a19c041eea7846ee8e40b70c48",
        slice_id: "a16",
        source_locations: &["a16:2165"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeAuthorityObservationImport|TimeAuthorityObservationImport.Shard.role_and_group|role_and_group",
        ],
        rationale_sha256: "ae28e301a9d38109042cdc7a377c8feca27d86d607f242b973f90becf96f47a9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0e6c35f78172a67ce5d4652dfa17cca9cfd5c6d68b64ab0aa9f0dccb40305910",
        slice_id: "a16",
        source_locations: &["a16:2201"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectDisposition<Role>|TimeSubjectDisposition<Role>.Reissued.successor_subject_key|successor_subject_key",
        ],
        rationale_sha256: "14cf8ff5380574e3d75deb6c566d04c5d4102faf8d36344524e4d30287278ba7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:c39a49d58a1e4f8383dcd973f24f1542a19b8b3a35edcfbb27e8ad08faade723",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.portable_expiry_attestation_identity_and_digest|portable_expiry_attestation_identity_and_digest",
        ],
        rationale_sha256: "d11b712b3a38266e12ee91cdb47055f9c1b773e7cec528dc7a8fe2388ed8f9a6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9e665016cb7a45f5ae6e4d68bb6455ff508789fe83d9e40fe4b0a599dd71bad6",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.subject_body_and_profile_digest|subject_body_and_profile_digest",
        ],
        rationale_sha256: "bfb06f624e3d632b226ee268103756b1ef54986c3f32fdfdd86b58582c0605c9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:439880508061ce9206720a2ce11022658a71030cc4f8d519a52415f1aa558123",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.subject_key|subject_key",
        ],
        rationale_sha256: "b02312eafeb341f79784f4767fb97450496fca7315c8f0343c8b944356593a98",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:1df3c1bc531c4e2e02d5d3114aaf64b9f3a26aad276f789f75bde2445bca7cc7",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Expired.terminal_authority_commitment|terminal_authority_commitment",
        ],
        rationale_sha256: "8378242c3a9f62379abd8fcf075909628dc2781e8c03ac7a20e45e02db927ff6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3235f879a698f52d63298366de7f6f35bea2adef2d55058df0350648a94af466",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.nonwidening_proof_digest|nonwidening_proof_digest",
        ],
        rationale_sha256: "a8167d49ccfc4efb9e019862e574bea1b4008ec28f2ada059bfa0a11f0933a47",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:403123812a10492300994071d83a66ac3b7e056829d79bdc5ddd69eb9d110d10",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_body_and_profile_digest|old_body_and_profile_digest",
        ],
        rationale_sha256: "140adc605dd05d54993d3c242d7d4c7223e6d40f967752f9b3d7f9e0c28b34b3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2b515598a6fe76493393ac44fdc64bf6208f036442ef977ba94d33b6a110bdfa",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_identity_tombstone_digest|old_identity_tombstone_digest",
        ],
        rationale_sha256: "924c025df56faf144e236fe48fce758936461f9e084f9f2fd67232eb00784a03",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:2236fed6acd8eee82cfb48df29a034f919dd8aad900b0e20c6cdf98611ffec4a",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.old_subject_key|old_subject_key",
        ],
        rationale_sha256: "1619a6b69a27211266edd0b31219e6cca080cb7fee60db07c93999e6684e3a1d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ac681d7d9469b0042e8a114098420c4e08f9725dfffcd261b70a2c135d5b2f3f",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Reissued.successor_subject_key_and_public_commitment|successor_subject_key_and_public_commitment",
        ],
        rationale_sha256: "75864caa2e77fc810fd5e8937e61c1db0c1e8f594ada7558316d3fb5c55e2c5e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:64b787a35f64549ba2fe85e0c0b1ef8290bd992e6f1e28c1432f3af64ea89e86",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.subject_body_and_profile_digest|subject_body_and_profile_digest",
        ],
        rationale_sha256: "112fc5a053975d97cc6313ee3f3a47ed6417e70afbe5f0df30ef9bd3738e4a81",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ec3ed5bd31064ce1d87e694da563bfefd7d5838d8b4b1b8a617a8dbabb76d4df",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.subject_key|subject_key",
        ],
        rationale_sha256: "bc6a8ffb0e2e089b4cc5a2598ea50b76224b299f8f583dc0ceed3f833d8f3c94",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:090cb31de86f72015dffe5b58eddba0fb6ae766050c7d28ee911059708895582",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.terminal_authority_commitment|terminal_authority_commitment",
        ],
        rationale_sha256: "e965108ff7d339914016dad9f87a7a3ed37bce6718164fb276f1c9416a845a1a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:412f94e68cbd3fafba3f239ad4edae688817333f62d5379772e577a74fb150ff",
        slice_id: "a16",
        source_locations: &["a16:2209"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectTerminalProjection|TimeSubjectTerminalProjection.Terminal.typed_terminal_evidence_identity_and_digest|typed_terminal_evidence_identity_and_digest",
        ],
        rationale_sha256: "43e3ac7083954bb5282fa4f2b85eeb886afd10fa7387ac280d1bd6bf8145d0f2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fe273b4d125585f91d7775d7061204c113cf295fbd53b607ec0e0cade19f25d5",
        slice_id: "a16",
        source_locations: &["a16:2215"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ShardTimeAuthorityRetirementAck"],
        rationale_sha256: "ec32c2afc70e32fb487291d640ee93cec6adb3df0b466e2b16e1f9788ac8f593",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:0799a80b9503af44762c89c9bd28c21ce8615b3e50872f14c3d718cfb6b855ff",
        slice_id: "a16",
        source_locations: &["a16:2205"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ShardTimeAuthorityRetirementFloor"],
        rationale_sha256: "d67492564d081a7dbe9b622aea95cb0d3e504a168b80b89865338bac61b7b2f6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:89eb7fcff73cdc05f717b5f8888b49680d0d11e5d8b7ac63fbaec93d118bfc84",
        slice_id: "a16",
        source_locations: &["a16:2197"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ShardTimeBoundSubjectInventoryCertificate"],
        rationale_sha256: "5b0d030db5659ce056158e43252ce1920ed007805c166d87231a6e151a0ae1a6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e4cd82413f511826adaebd47c79e439c7cf26a366db7682136e3e70085ec8902",
        slice_id: "a16",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|CrossLogTransparencyFreshness"],
        rationale_sha256: "5397b2c13b92c6ff93eacfab51efbd325431fac7f5439115ca4568a461bcb015",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:b9347cd7b769ace4ffbb1db74e730bbdbb736dccd01fa43aabc5f762322d4d2b",
        slice_id: "a16",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|GroupTimeIssuanceQuiescenceCertificate"],
        rationale_sha256: "6aee4d5011b7f68baf9e8ab189f441a8c3ee1fb189f07c269694eed92bb39831",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:bbeb7610f005f870af91589e584a196b7b10d4898f7da64f0d8edc78dc7dbbdf",
        slice_id: "a16",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|KeyEnvelopeGrantWindow<Role>"],
        rationale_sha256: "b6ee3a210b354cc1cceb247347c625c911d7ad3129c52cafbf79b4a780d6bc2e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:994a81b46f241832d3170159b4c71dd7083d9fefe89aa659273a1c4426970fab",
        slice_id: "a16",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PayloadReceiptProposalFreshnessBasis"],
        rationale_sha256: "d5fff5d08e808b2539e66bb145ff4ad007d9943bff84e9c156e3824cceb03664",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:9c6c718123cf055dc9a456ace50120d2beaf8d0ba42cd7aabab639de7ecf9fd6",
        slice_id: "a16",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PriorIncarnationLeaseCohortWindow"],
        rationale_sha256: "fd0301a8de862f07836f1082f87e4a8553345db48ef4071a2a9cec8572f4845b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:40b07c775488fa9786d84a11088f6f57e4b9a5c95299ad4ffc143fc696574286",
        slice_id: "a16",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|ProtectedErrorReplayTimeBasis<Role>"],
        rationale_sha256: "00b0d197a69ebec3f2af7609c7295ec3ff952bcb61ca23146575fe709bcb7c22",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:eaa26b8f61958eeabd97319a5546c82f57bad2e255e80e863d3dcd1900482f97",
        slice_id: "a16",
        source_locations: &["a16:2185"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TimeAuthorityIssuanceClosingReceipt"],
        rationale_sha256: "b296a6070c3858f066a140e17af2ca2d22cf43d93112f12a79723be8362ee3a5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8f06d57bcaee0a663a16812a9981d7bd2478b69706dd7c042f70ed8b137e8231",
        slice_id: "a16",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TimeAuthorityRegistryTransitionReceipt"],
        rationale_sha256: "f5440c70e971644dc597b90ea0b089a0ecf65613403a1a4c5c65a208d35b7ece",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8382d602a7cfb3b34c9b3adc956a9aceaab94ee8e651d9eafad600a00a1f175a",
        slice_id: "a16",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TimeSubjectIssuanceReservation<Role>"],
        rationale_sha256: "1cb26594d908f2ad22f08e9e66e1383ba5a547dd7ec0cc9522a752c13525f7fc",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:a0148dfd5ecafa3bd5cdb3dbc1e191d682e5eeb8d6023b8bcd9e504b33ed85f6",
        slice_id: "a16",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|TransparencyCheckpointFreshnessBasis"],
        rationale_sha256: "f27ec62ca912bcfef33ffaf09348c7b982a2bbd899e81a2c5a98b76916d508b2",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f1c6073209ca7d229ae3f22e6b388d4775edd7625e3c0d7a08492cd3b091968d",
        slice_id: "a16",
        source_locations: &["a16:2171"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PriorIncarnationLeaseCohortWindow"],
        rationale_sha256: "59b3586f2f8b7f78bdfa165f54b638ab22e63e0f020e3e41232b35fce6cfb5f4",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d0fbe5b2a75940c2e66ca399d38b8efa3ca39e33d44bf639a2c965fba6ef4e0b",
        slice_id: "a16",
        source_locations: &["a16:2193"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|GroupTimeIssuanceQuiescenceCertificate"],
        rationale_sha256: "6a3f8f991d6607a7da0f23c4a7329b2ffbb2b4bbfaf87626337858a16069d438",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:f0d112a99c9db91ac5aacac17a50ab039e9d6dda70ea189fd0428ae7e5f6077e",
        slice_id: "a16",
        source_locations: &["a16:2173"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Expired.time_validation_evidence_ref|time_validation_evidence_ref",
        ],
        rationale_sha256: "c524232926b4c669e4cb5e9974ae6b445fb3b4f3416d7ba478eacf20cca136fc",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:27b38b207a4da96fce9484e5d14596a5c429122c2e0b1828fc0d5a30b9df4ba1",
        slice_id: "a16",
        source_locations: &["a16:2173"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>|MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>.state.Revoked.revocation_evidence_ref|revocation_evidence_ref",
        ],
        rationale_sha256: "2933a223502479180e785d5af08ef4fac172b35e26c6f27a4018815ada8fb8d0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:d3972d524264e6449e2528313a7976b508eac8acabf9593a4c8ce1ffe06f7c7c",
        slice_id: "a16",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.receipt_digest|receipt_digest",
        ],
        rationale_sha256: "5f8f10726d7d86a9e8d6078fbefc6b3034bf31b7ccfd4c79b7725fe8dd340324",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:73143cf7fd34135bee48b34d7acfe28e8d8b7d465f7045f6d3aa6db79174a6f2",
        slice_id: "a16",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_cas_version|returned_cas_version",
        ],
        rationale_sha256: "e932f6e53e7f3c32e13c2daa4b6a8f982d034e3b44ec77ac413e0c07d84a40ba",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:40fc6209f306a671ccd4db75f22833ab2aa76d13d8442ebc25674a04196cb132",
        slice_id: "a16",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.AlreadyApplied.returned_head_digest|returned_head_digest",
        ],
        rationale_sha256: "918bf06f538035be20fa7a56ac26b9bfaaff3d66e71d70eb873746f93d5ffe15",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8058b555f1a86c8409f8539532fc03036c349fa2f6fee639571fad438e13358b",
        slice_id: "a16",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.receipt_digest|receipt_digest",
        ],
        rationale_sha256: "87ebd74a7ab923c0dd5b9570a8e17cd89f9337127cd6f06b4b370500d892e4b0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e62694631633aa73ecfadb4ad6d290ed730a138cc9e521583c2200708582fc9b",
        slice_id: "a16",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_cas_version|returned_cas_version",
        ],
        rationale_sha256: "301ecef0e95281bc7273ec3ac6e8a61dc346f2e877f6ab179c1c14131edc07a8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:7cae16fadad1f0eaafdf3ebaded743a00bd755995d0a1d8db5d6a6d00507d90a",
        slice_id: "a16",
        source_locations: &["a16:2217"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PortableTimeAuthorityRegistryTransitionTerminalEvidence|PortableTimeAuthorityRegistryTransitionTerminalEvidence.terminal_disposition.Applied.returned_head_digest|returned_head_digest",
        ],
        rationale_sha256: "554aa5c01be9a3e723ebf20a24bf13091c8f72bb838bf65eff6d02f2cf95bd24",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:3f70b6d5956b91cbed5b83cf1c9a008e0582772e43187bbd4307c896c9a58b0f",
        slice_id: "a16",
        source_locations: &["a16:2237"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|RestoreSourceLeaseRecord<Role:AuthorityOwningRole>|RestoreSourceLeaseRecord<Role:AuthorityOwningRole>.record_kind.AcquireImported.prebootstrap_owner_digest|prebootstrap_owner_digest",
        ],
        rationale_sha256: "92bad281132177dfec1f17f5afa179bde0bf2585e9d5357efbbd0fb9eba42b31",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ee2a0d26ffa8fc46b9f93e2f68ff4faec17728d1615fa20416ac52779fb5fe24",
        slice_id: "a16",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Burned.typed_no_publication_proof_ref|typed_no_publication_proof_ref",
        ],
        rationale_sha256: "26a93fce4538f564caf9fee9e96dfc674961feb410a9ef5e897a064d17812c36",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:501aac0906acf541265937cca86f1d5e075cbecbad580e128e54fa76e93e0455",
        slice_id: "a16",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Published.publication_cut|publication_cut",
        ],
        rationale_sha256: "953a8215a73a7d7c6e85b37f27dbdc7b460dd874a5da4506768b0ee84348ba3e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:8762b46c4d850de631aa9c367c2e30c1f082d0306636f98323fdd1d17bfe7b52",
        slice_id: "a16",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Published.subject_identity|subject_identity",
        ],
        rationale_sha256: "6d66ad0d7b74e9b04ab8d4f3b155e568b33f68c27e2c9300c22ee924091c5d91",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:47ca97c9c1b146df0c47c3f7591362bdb4b898f8ed393ece13b85c505d8d74ca",
        slice_id: "a16",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.state.Published.subject_membership_proof_ref|subject_membership_proof_ref",
        ],
        rationale_sha256: "d3b600696b7b91cb87dc6a3794f1bfaa2690257add9e7420d93abef2fe80d513",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:fa23a8cbb7abc8dd79d1abab0b34316219cdace6a76e2bff9c4b1e0499a10650",
        slice_id: "a16",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Expired.expires_at|expires_at",
        ],
        rationale_sha256: "7a47c4f5903a3850b2d1e193276ece0e843288ce63262461bcf588ae882c81fa",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e3b83ea20d0ae098e66e43bf5c550af8f0ef4e5deb0611dff362dc221147dd87",
        slice_id: "a16",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.NotYetValid.not_before|not_before",
        ],
        rationale_sha256: "72221adbc9d51bff2c2248e18d4daafa1e2af17824adaad7f7e2f9e75714db19",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:130c006a13e9ca83c0077198adc547325eef648f40464ca11c5b7c361ee50b78",
        slice_id: "a16",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.expires_at|expires_at",
        ],
        rationale_sha256: "a6201f134624c935564416f476592c512839484d7cf22cdd8acd013f7fb64d8d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:ae09c78d9160d13ccf4f3c6dd11c1519b46645b455ad4719681f09b22eb4a015",
        slice_id: "a16",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.guard_deadline_local_monotonic_tick|guard_deadline_local_monotonic_tick",
        ],
        rationale_sha256: "acd606c5279c57efafb7b1925a98325b42dcc81358702f5ad6abbb0d875669f6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:92d41da0431b14a88135a2a3b5bbedbb281ab58af478f2ad2f04bcb383441774",
        slice_id: "a16",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.not_before|not_before",
        ],
        rationale_sha256: "f8512c104e90842329bc620dcb5fcaeaf042689fded89cfb9933ea78f7094253",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:e2c84603e3f6477231e1f9b44471df57d2ca0197a763b02659b10b0d5da10fa0",
        slice_id: "a16",
        source_locations: &["a16:2167"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeValidationEvidence|TimeValidationEvidence.classification.Usable.validated_process_incarnation_id|validated_process_incarnation_id",
        ],
        rationale_sha256: "329f1b272f51f7d66c2e4709c0a725e19e8bcd665064a880e4b07e5548bb9dc8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:db66247a27fb86a8603024209e2ddd3fe88d0af75ee5f1ee02c68f9788142a3c",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|PlacementDescriptorWithoutId"],
        rationale_sha256: "1e1256b68ef4ee9f68844c63884e3b5848dc0adfd6f5fba85cf59334a10ffbec",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:44e6b6852bdc2c0f3336cc4a9f866a0430e83a64baa491610e066c4e9e0a113f",
        slice_id: "a02",
        source_locations: &["a02:1447"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|FenceToken"],
        rationale_sha256: "811147cc642034731fd9e06f1c98ef7bb6a6b03ae5f8b8768266c73e4ac90224",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:b6f908b8122821b649d176365f4deb62078c49b328f0f9848bdec7b7dea770b2",
        slice_id: "a02",
        source_locations: &["a02:1447"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PendingFenceGuard"],
        rationale_sha256: "be90d2b7256f2b34091168d266bf0866b85ce290dcc2b0d16fa7d4781cf72bf9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:b7095531bad16e02ee36f522e449e2352c619acf383706f42d879b4794d93dc6",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|SymbolRecord"],
        rationale_sha256: "0818a611bdd35c92c9b6ca5ee6caf7b594fbe6db1d320fdf79b6dacd614e2156",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:4ccedc0588f7be74f4888666d503fde3bc7bc24371cf1ce1e3626a81d160c577",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.canonical_plaintext_len|canonical_plaintext_len",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:09e450db53ed431535948a0409580058cec35c34706c8d2e356e090ba0858523",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.codec_profile|codec_profile",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:2107f6bb310d1738e09dbc78172590eb86e893bdc1122828d6c89b4872d0dd4c",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.compressed_len|compressed_len",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:78bc7d81d3ea77a5f7cbafe8087902fe5408ae96fd0196ccc8853add6213b849",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.data_crypto_profile|data_crypto_profile",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:01b0e2f905955e3f08d85feea3eaa8ece8b3733508e8bb5f32659f2c5665d2b7",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.dek_id|dek_id",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:65f322cfd0c28b197696f849d53882d7a792fd2641775c105f7d6016bfcbdc67",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.logical_oid|logical_oid",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:aff152416fbd814b48c7e182decf54f4e4e8f31a7e39bf85bf7b2c8a35b39ed9",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_kind|object_kind",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:3b10032b12d68f801a7a16decacf694eed72525f8432d18257d23fa2974cb862",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_nonce_or_siv|object_nonce_or_siv",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:70dd1ce6a619845e395216c30b5cc24523f715d660652f6c59233b2f4a1cdb3b",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CipherDescriptorWithoutDigest|CipherDescriptorWithoutDigest.object_tag_len|object_tag_len",
        ],
        rationale_sha256: "6faf4376e87565480ba77bb182995707821110a48467f4134992fcb150791349",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:21b51c04904bd6d0f06771ddd3fcff1e8f7dc47b406c10d1d817b81ceee93536",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.ciphertext_digest|ciphertext_digest",
        ],
        rationale_sha256: "aa7ad6e148c18f5290c482dc22f3c0311b9da48f15169251bad90dabc6fe862d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:e30d0e52a9b56177e2817418658afed5827dadf120553f44103643ee0f1aae7d",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.ciphertext_id|ciphertext_id",
        ],
        rationale_sha256: "1c88d4b9f2758416ad7270079677bd950cbdacf7fc14c77ed4fa4ce96013f7c3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:0b3b432489be5ce60147873ff27823f6a116229dfdc512f64024a49580237c8c",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|CiphertextRecord|CiphertextRecord.descriptor|descriptor"],
        rationale_sha256: "3e0eb82b56a6fa86b1a8537dd005b7b83c4d9cb704980648e2a8aab54d98d882",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:7e38b5617e877b4c4d04aab2aceb9038d3998e272786e35c565ea11a61fc8402",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.object_tag_digest|object_tag_digest",
        ],
        rationale_sha256: "97c773dd8c86f5c7bbad94a38c3e63210d91656a6ecdbc01522fefe2e5d30be3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:0239498a2cba36ac46ba1927f4162f60e75399abfbb660542f8d307745300617",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|CiphertextRecord|CiphertextRecord.protected_length|protected_length",
        ],
        rationale_sha256: "8df12e6553ee88c8e4df247e9184fcc0530a34de215ce0e7200b4b45013d12cc",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:58efaec60718539e0d6fe9e4a1d057dd06ac699ceb1a3ab48e932808c2fa8133",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.ciphertext_id|ciphertext_id",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:c93e1bddb4c4026b3a846c187e67d3691154bb09a0d2c27549ed13d33c4faee3",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.fec_profile|fec_profile",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:36ce0f3f763b04170b64e17634530ae0b9f94d01676e993e5ed6cdf4f162bbf6",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.oti_common|oti_common",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:0fd7f1abd9e2c65b77a7b222d54c3da2786eafb0c0a0ef19dbd85113383b17ff",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.oti_scheme|oti_scheme",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:fc15e98ed9b83aca5947f32942eaef36bfe8ac25720614ccf5eccdd473462033",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.source_block_count|source_block_count",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:91902b26aa0a57dc07f52ad4d764ced91c8abdf107028a2ff451daf3f658b386",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.symbol_auth_profile|symbol_auth_profile",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:74c44cccf917099778495230c0ec66ff6391deb20367e75fccd7315f3ac6b929",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.symbol_size|symbol_size",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:e5a35dd4fe8de0fb93b60737c11309eea3fee0445f97b87965f72b1440a46590",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|EncodingDescriptorWithoutId|EncodingDescriptorWithoutId.transfer_length|transfer_length",
        ],
        rationale_sha256: "a444342f1ac92824b51897a98b156083d2dc22b3a560bc142fb4d3a9a6546ff0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:e950732c0153855d0e18984a9a8efc352d8856cc1fab0222138f4b42cc3bec9a",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.allowed_filesystems_and_mount_predicates|allowed_filesystems_and_mount_predicates",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:3ebc591c25f9c7eeab7a5df0a9850620fcfd9db848142c7c1de3a36f7915d444",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.cache_flush_assumption|cache_flush_assumption",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:4b63359434d412cb4e93da35ec045ceae13989d042b84d7ae6314386df91c871",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.directory_sync_rule|directory_sync_rule",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:8873904cf4efb6da4b32353b2091f387b40f93ceff6af9fcf79813cf5644c638",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.file_sync_rule|file_sync_rule",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:17d6d325b40c98e821df86f2214f8769f1605d56941c6ee69ea933198cb37433",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|FilesystemDurabilityProfile"],
        rationale_sha256: "b4bd78c4edfe946cf85a81a6dd5ee8f6a05efdfa5838cf598a5a55190370385a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:c7416022d96db48fb8420248771537b28d29cffb1e9428859c9fb36ea4853cfe",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|FilesystemDurabilityProfile"],
        rationale_sha256: "b4bd78c4edfe946cf85a81a6dd5ee8f6a05efdfa5838cf598a5a55190370385a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:eef13b4b7a2306d6fe972c7c2d821b7e62ba883e6ffb2a9e6fdd313323849073",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.owner_death_rule|owner_death_rule",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f647dad9e7de474a5854ee8f2254e916bee0d666b285d71f678368395354f05d",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.probe_suite_oid|probe_suite_oid",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:bc62521d65078a2781fe44d3efb08cd84e5d65e876b31ea864407d9332cd20fb",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.profile_id|profile_id",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:05ee487e0b125b8d10eb6d1c00f301fff764d0caa5dafe3ec9da98c6477584b9",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.profile_version|profile_version",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:99f8784a375d4d11bb423cc86fc89929a0674791e65daad040c44ae0e2415060",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.rename_no_replace_rule|rename_no_replace_rule",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:a846ae0b543e13fc45e6a5f69df06c09d4d13aef5289f4195c9e4490b97ccf67",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.required_lock_primitive|required_lock_primitive",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:418f3bc0a20a19de8b690a2bf879b4eb13452aca56deb8885b70a3840edba3af",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.slot_write_rule|slot_write_rule",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:010e36ee5de62c235893df457e3727287ec8a3c933e2350f364f259104b82659",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemDurabilityProfile|FilesystemDurabilityProfile.stable_inode_rule|stable_inode_rule",
        ],
        rationale_sha256: "984062c6fff6fda6abcf5bfa3c27f2a901f3de2da8021c3f475a5873d67a0921",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:dfc4ee027f44f5a6de4cbad5c1e1ea667efc9f0bb8caedc8f3c95090c0a1d3b1",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.canonical_mount_options|canonical_mount_options",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:59835471a2ee6ac1e9331216d06953e05c349a60ac9784235a02802c2514492c",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.checked_at|checked_at",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:55cf4f7028a45ef7d65a28a261fa7d7a3516b7d830592ae65bf7df05e455db6a",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.device_chain_digest|device_chain_digest",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:75f019a44ff22a05c88a94feab810d4ea5e37d5d09199e38e376efb9b35325e5",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.directory_device_inode|directory_device_inode",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:187c4435bf8b3e3c71d4c9991d9f9aa365586d2a8f5d222afd7f1973e98bdad7",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.filesystem_type|filesystem_type",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:58e8995d6dff4c106bde4b883c447df2fb5f23741ff4b27a13dffe03b4f5a846",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.manifest_device_inode|manifest_device_inode",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f0c77d4630f11fdfdb94e63f0eb32f9a5b62ec529eed748cb92e281f73c269d8",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.mount_id|mount_id",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:37dd75a6f44baa134b14e608eb509469c77653e9bf407141e9a2d456002e58dc",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.probe_suite_oid|probe_suite_oid",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:7493dcf2c9989d73978202b23a33520b3dae91d9b3322116b24d8c778479c089",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.profile_id|profile_id",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:c9a8f5c62ed9dfd6d2bb96b7d0041ea2070123fe3a64def0a1341d7f47a1b9b7",
        slice_id: "a02",
        source_locations: &["a02:1445"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|FilesystemInstanceRecord|FilesystemInstanceRecord.result_digest|result_digest",
        ],
        rationale_sha256: "6162e87889190dd36b7b29ba0350a257f8158f35cd622c6d900fa441798a4596",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:4589a1a610a23f057f6b5862ea7c6f0237c355edacd39c5d5b1795cf6b0cc5be",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.failure_domain_policy|failure_domain_policy",
        ],
        rationale_sha256: "f457f1396d29bafbf9f30ba7bf6415f9a63058c18581011e3139ee731782b48d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:1121451997661af6c018ac5cb8a92e35e459d9cea025b5dfb39f12bc16c38e20",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.location_form|location_form",
        ],
        rationale_sha256: "f457f1396d29bafbf9f30ba7bf6415f9a63058c18581011e3139ee731782b48d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:d6eb0a856c857d051ccc3db1bbbdfc5333456257cab21d9eec3372302d2a44aa",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PlacementDescriptorWithoutId|PlacementDescriptorWithoutId.placement_epoch|placement_epoch",
        ],
        rationale_sha256: "f457f1396d29bafbf9f30ba7bf6415f9a63058c18581011e3139ee731782b48d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:200f2d59a98b49a305f74582c258b6b32316c146f35b4b436ec9620d319c2b52",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|PlacementRecord|PlacementRecord.descriptor|descriptor"],
        rationale_sha256: "0614a0ea68d0649f8180fce15da38b0c3c091c68914e77c6ad137b7503106cba",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f7c039dd333ca9b1bd1e78736d84f3afe200de8795e7791698012f017f5f5f5a",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|PlacementRecord|PlacementRecord.placement_id|placement_id"],
        rationale_sha256: "d6153f198c3358c43ad20e84bfbb7a286fb597c8ec27b543612dc276f801b453",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:34cf0a3cb4495aaffdd79a0489e1b6a8caa672ad4ed267b7e32b2c42f13e59b1",
        slice_id: "a09",
        source_locations: &["a09:1904"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|LocalFinalCertificationReserveSpec"],
        rationale_sha256: "100b797bde0d25ff732446e2a30aa65d565e594829017f41d01b37cd1c7508c0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:31cc7fb171ebf8e60218481acbf4ae4a9a46c47d0bf7e2636d2c44c14dbd5dcc",
        slice_id: "a09",
        source_locations: &["a09:1904"],
        resolution: "maps-to-source",
        resolved_source_keys: &["top|LocalFinalCertificationReserveSpec"],
        rationale_sha256: "053de58ad70257991c3a1819a3bca0e9999d4774230070364860b48d4a11975b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a03:ambiguity-adjudication:8c725fa15a9bc362be089cd1c4a8a996d6f4e20664fcac5528a2df62b880ef17",
        slice_id: "a03",
        source_locations: &["a03:1524"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnOutcomeRecord|TxnOutcomeRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "684219a2bbf8179cf5554fa9ed38e1bbae8138b7b42eaf7a1bc3c9b184577001",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a06:ambiguity-adjudication:8723ace590fa1a79389ba18bd5aab08bcc1d3ebe7eb3a17528e939386f05cd0c",
        slice_id: "a06",
        source_locations: &["a06:1698"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|MetaPreparedCommandRecord|MetaPreparedCommandRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "fdeaf89b1f397d6a285015c14d413694c660ae6bb2e493a5d983059fa1b78ad7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a06:ambiguity-adjudication:d29cf70d19793cee02c2ad22e8df60d5ad6dd3357ba46123cc85f4d5728315f9",
        slice_id: "a06",
        source_locations: &["a06:1700"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardPreparedPayloadRecord|ShardPreparedPayloadRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "dc98ecbb2a9cbda48801ebbcb424659d07d48d5172378c54ddf905e968796a85",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a10:ambiguity-adjudication:a2ec9c9e4687c66ca7527a460cb9a9629e7e9297e0e9d39a307025de544f061f",
        slice_id: "a10",
        source_locations: &["a10:1922"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|PreparedCommitRecord|PreparedCommitRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "9c00a2d310b0606c80a400957bf50826a0f0b27c5c37ff9bcc9e6b14bb3d4a26",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a16:ambiguity-adjudication:733086702e6b63491e3b2a762e92e10d4e93ed043291c93f73edc4769a46c3d2",
        slice_id: "a16",
        source_locations: &["a16:2191"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TimeSubjectIssuanceReservation<Role>|TimeSubjectIssuanceReservation<Role>.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "1d623bcee74387ab454300c2327fda8ee0cd0287c93d82e332ee23490c7b907c",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a11:ambiguity-adjudication:98bfc2c6ef27ede75714946eaa38ebe9b92541fbd58e123d1e2f8a793ce2d21a",
        slice_id: "a11",
        source_locations: &["a11:1932"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|InternalBaselineDigest"],
        rationale_sha256: "9d321bfec8544d9f640cc3c34d4ba5c1ccdd3677dac5635703d735d66f8c0e90",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a11:ambiguity-adjudication:d93a7697a2040a94c9e91b9234c0c9ddf0f411dd9a4f6b67e3273e9205b2be4e",
        slice_id: "a11",
        source_locations: &["a11:1932"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PublicBaselineDigest"],
        rationale_sha256: "bf3b35bf268173e2585908a1db81f501212c6139971826fe80529dbf4e79fd99",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a11:ambiguity-adjudication:4a22f9a9db5d5ed1f2f62f6512dc1e2bcbeee6d916c46c48d39ef610cf35c613",
        slice_id: "a11",
        source_locations: &["a11:1934"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|PublicDeliveryDigest"],
        rationale_sha256: "400611262bd5c2098c72401fd92b9d1bbc69ac7d13e549e3e303afdb1c2a6f70",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:7989140375531b62a0427337b27000ebb3a319364975a0a7de7635fd2990b18a",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.database_security_namespace_id|database_security_namespace_id",
        ],
        rationale_sha256: "b023d753e48e5e6c7fac0bcf3a3fc38ef17ddd2ef54b6469b1b2af67afcfd8a9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:5eb46aa4d18d045fbab17d48d4fa4c2e04fe241b6339c21e60007af58b89dfef",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.allocation_epoch|allocation_epoch",
        ],
        rationale_sha256: "6755049dde800bd98eabbe4d2240ebe581adc2beadbc0770e831ed0e7e7be5e5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:5a108c87bcaf7af97c68b4de5140f580e0595077c32b9dc2101ce3020e0386ef",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.predecessor_digest|predecessor_digest",
        ],
        rationale_sha256: "e5c3b4b9d7ad9f63d143e7221f65bb431d186cd2d1ae5243456f176203614165",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:0fc76e8406b9a839f450c938eaff50007c961724ba28ce1f64977219c65d0d15",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.external_registry_id|external_registry_id",
        ],
        rationale_sha256: "29aa734cf76d5717f0f0b6d36c686c4cfd7b0b0bd66f2a57f48213c632511c59",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:7b44d5b03543471edf15dc99e30e657583e45f02cabaf48e320af54c665eb518",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.cas_version|cas_version",
        ],
        rationale_sha256: "602ab23ff92a524ac2cdf6c6f1ed14fdb5d8a4683a40b752dff25bb49e02cb10",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:35c1e0d07f3eb18d887676c63f91a58a9ee0a9a4009183bcd7ec016c3b660932",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.status|status",
        ],
        rationale_sha256: "0f6a030703c692299c5a0cb634ce464ea0bf690a59a34af02d61860e8dde53c5",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:afddde1229bb99bc5b43ecf670fb764381aad05dd58da905f2068c9f7cf78f0b",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.issued_at|issued_at",
        ],
        rationale_sha256: "3f8a7775d3b3bb4cceb8730361d28ced0e81c83cec04d2495a174acdee1162a9",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:f6fead0d3b3c1a3cd04821d4626ca07675c108902deda2d4d50aea669073edf7",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.signer_set_epoch|signer_set_epoch",
        ],
        rationale_sha256: "374c06988202ff70e5595116f4393515bfbbe8e93e175ea44d1751f58f3fff00",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:3bc7da5d29eaeb33f284b3e0664cbfdcce0645a313b045f01ba12501ed2c26a8",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdentityContinuityRecord|IdentityContinuityRecord.threshold_signatures|threshold_signatures",
        ],
        rationale_sha256: "155a4d3d27a5976ccecf90f96fe3ff3329971d0b2c92e97319c138182c293574",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:a56e96e992fa004d53648fff60577968c42e7eb1e76e061ec17ad916d8d3199b",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.lease_id|lease_id",
        ],
        rationale_sha256: "b3af50c450b6fe9b563bddbfdae1dc30e87bc0c8bddd1b8e7b5a1934733812f0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:3cbeed9a9ea13d29a959320cb4c972acdcc149cb61c03b43b64284e6ba669799",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.graph|graph",
        ],
        rationale_sha256: "1b457c30add76f582fdd439b235a1e3e1d561197eb302e585baa4285f506b53e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:a5247158af450d3874c3381f2ebb1a4bad5b01381a5db492e66e7ba3dbbd42cc",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.element_kind|element_kind",
        ],
        rationale_sha256: "287e6a9d340725f5dc0045ef6e6540074eea295e155c4937e175abac98d81ac7",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:9a3fd9fdb09fd9fd8034325657de2cadf652ec3713ec11fe97391773744c5249",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.allocation_epoch|allocation_epoch",
        ],
        rationale_sha256: "a14959de0bc60523d8c2bdaaf4c02ed36a166db8b39cbf53d6db8087bfa9674f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:99f471783470d1394c5e9b58102a4cbe715c18f5353e9784a913513b859235a7",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.partition|partition",
        ],
        rationale_sha256: "d55897a92eec462e2f66ba6d3240e78a6a8c30fbca35053705b69d3a1c185fbb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:24a3d803731ca838564933d4c189d3a334877983cff3b22faf085f146d49bfc4",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.half_open_range|half_open_range",
        ],
        rationale_sha256: "25e54a37ab165d559889ecb73118e43dbfe9e10f2f9da257664777d7804ffc86",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:ba4f137cd4482c720db3387c1f579bdd42a41c65b935a93ac146d42d369a3db5",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.time_authority_profile_oid|time_authority_profile_oid",
        ],
        rationale_sha256: "6cf8550618130562371e3727eca1d3c4f7f344fa108bbe922290a4a57bfa87a6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:f92ecc970f0808864f6330f4bea378f455e73f83eac39266660daf596c273b56",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.issuance_not_before|issuance_not_before",
        ],
        rationale_sha256: "bd68cab0af4387f7cb216d20c54023ddcd2a7768b87c1d76aa0ed5faa28b2888",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:e980cabf89166a1b611e2908e02485c320159dfb33dfb8b56bb34ad71c463614",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.issuance_not_after|issuance_not_after",
        ],
        rationale_sha256: "e0fdac1496a41a0dff27c995820935e0fe8c4acc1c21fcdab4b1ec84b312f206",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:8df2a4c61a0630de9609946d1278eead5147d6512dbc752ce2fc5adc9f277e13",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.permanent_spent_commitment|permanent_spent_commitment",
        ],
        rationale_sha256: "36cc747a40d2fd894a1aaa869398c4edf92974f541cc5ee020d6aadab0137c7d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:4f3f680f6528cfc80f55800a442a3251b4075724849f4cd6740839215556c8bc",
        slice_id: "a09",
        source_locations: &["a09:1892"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|IdRangeLease<Role:AuthorityOwningRole>|IdRangeLease<Role:AuthorityOwningRole>.body_digest|body_digest",
        ],
        rationale_sha256: "913c63f4a6d1d447c19ac2db81f893f7da233b4110a64517c0cc43cdbbd28dd0",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:8de057a6c84c235bec83c7ba8237e7f991be0809d8863d347cffcf7932c6ed56",
        slice_id: "a09",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.generation|generation",
        ],
        rationale_sha256: "9a7579d06dfdd05d73252cd36893075934b17835b73f5beb757124ffae234672",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:dbc2d9bca1bafc5c870e00da7218b31231b1c64558f67415d710bf1f07c6464d",
        slice_id: "a09",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.attempt_identity|attempt_identity",
        ],
        rationale_sha256: "94017e0f49094e957debf88a1faecd0cf10cf584c9e03ea67bddfd3aa0518a83",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:4576279cf232c7845c55021c16aaf532138f3ef656c0e1d5532ad1bb978b191a",
        slice_id: "a09",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.through_statement_seq|through_statement_seq",
        ],
        rationale_sha256: "a47915473cdb336f723f51bd00ef71a0af3fd54b54daf497ad3d98f886a4bead",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:43c3e5daf2b3bbd201ad393a918221d7137fd173c7abe6529878909c6e449b02",
        slice_id: "a09",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.sorted_spent_commitments|sorted_spent_commitments",
        ],
        rationale_sha256: "32cfed27fe9159be1d27a097634da61ee13d1c0e3c657ddc0e99c27acea3af4d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a09:ambiguity-adjudication:a654ba46e405b504506c1ecaf92c7fe0f0e8de107a6da5f174150750abd2da5d",
        slice_id: "a09",
        source_locations: &["a09:1900"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|TxnAllocationBindingRoot|TxnAllocationBindingRoot.root_digest|root_digest",
        ],
        rationale_sha256: "ce30bfdf59ad976630a7bc5eab38a36be0c36f381c83131abd213633dc37fabd",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:9fcf021dbaea4ffe1daf6fb1ceac10df49ebc45b5b43054c0c5cf93eb4471643",
        slice_id: "a15",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.key_identity|key_identity",
        ],
        rationale_sha256: "1d4aaaba46a14e8bc922862dad5c285b11a11d9dfcdb32e64c74e88b3dd97fb6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:c4b54a063bd35e654e0c35628c05ea2005055123fe1fbb308e31054cd4be76b7",
        slice_id: "a15",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.complete_target_set_digest|complete_target_set_digest",
        ],
        rationale_sha256: "0e8e4c307ae1d8704c2c8d9deb04ef7170795959db42aeca8bfec86e9282d0a8",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:b62b76d8465a74e44a868d3ea1cabd3eb3b9ca6e970d47ea69f35e21c7cd5e29",
        slice_id: "a15",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.expected_prospective_configuration_set_digest|expected_prospective_configuration_set_digest",
        ],
        rationale_sha256: "e36ee193d425c68982746ed33431af8f43458def8ec8471f2635f22ec842600d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:9d87b7faa3ae557cc0b0eb4aecf03219a98f5758e0e5607b1fa28c5d7eb3d55f",
        slice_id: "a15",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.expected_state_conditions|expected_state_conditions",
        ],
        rationale_sha256: "7f6fcca585f5599607b942d5c858ff611a5524ab35a90a1af9213fbf3da8e5fb",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a15:ambiguity-adjudication:d14591312dc555272847ff5149204b97bfc8f5ff4ceef5736d45d6b91bf71dda",
        slice_id: "a15",
        source_locations: &["a15:2059"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyDestroyProposal|KeyDestroyProposal.terminal_audit_gate|terminal_audit_gate",
        ],
        rationale_sha256: "337a77ec5a9549f29ad8dbdad943c6a1520c28fa725f3373adf508944a34688f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:670d6a3c6e1369d3bc38d6f3076252157264e695e05b76984089286770501c7b",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.encoded_len|encoded_len",
        ],
        rationale_sha256: "9f96d4ee6bf656de9518acf82f0cfcb18e6adb9d5662a6919e2dd1e684387801",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:cecf9025af9662200ebec0ca362365459431e220b7d0b989415630838a42ce3d",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.failure_domain_id|failure_domain_id",
        ],
        rationale_sha256: "39d9df7c37031760e78860c8cae1154bc47322132fa978fd304176b0b53b0f57",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:1add14f68841c209d9602d82b0d4974f1c7e0307bdc4e70a11b7805ab03369fb",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &["field|LocationForm|LocationForm.ContiguousSpan.offset|offset"],
        rationale_sha256: "6939774b7bee697154a70d7671ffee52048ebe693a4abcf0fd6fd6bed2377a0a",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:cf1828242c9de970216388ce83137d5debc49b7b633a229d89e52d5409e41d76",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.segment_id|segment_id",
        ],
        rationale_sha256: "3907bd704c495b38d1456d4c2a20c6ca4055aeaf463ac9d2fca097d0d53bb5cf",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:d73d40486b24cb361fcb957a65f0ceecc6a2c33196af11ea8ce9c8ef527936a7",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.ContiguousSpan.symbol_inventory_digest|symbol_inventory_digest",
        ],
        rationale_sha256: "471ed99089c490d13b2e94d289f8fd82a5c5bd066b9099cee490efccad1af70f",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:f889bf772824c1a9353bbd3b25ee290a6aca8bba2a931a9fc09f3018a0ff3355",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.Explicit.failure_domains|failure_domains",
        ],
        rationale_sha256: "fd08346f1160fa65ac779a822218b8eb7cd8294668d7da958fbe4b62b4dc0def",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a02:ambiguity-adjudication:3186d0debf2fb0589fc0c814cd95c8fec9e6c4c4e3b1a3e09b8f284bee7515ae",
        slice_id: "a02",
        source_locations: &["a02:1449"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocationForm|LocationForm.Explicit.sorted_symbol_inventory_and_locators|sorted_symbol_inventory_and_locators",
        ],
        rationale_sha256: "9a280d366754c9da1f8755cfe69bf07e3511c7f1f7ee5e21aa2d5c1920761998",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a07:ambiguity-adjudication:ee23aced90506d99111b719ae0f8486df181ed161f5ec8a12c8214f574341d65",
        slice_id: "a07",
        source_locations: &["a07:1780"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|GlobalTxnOutcomePreparationRecord|GlobalTxnOutcomePreparationRecord.expected_registered_outcome_digest|expected_registered_outcome_digest",
        ],
        rationale_sha256: "4158c0b849c684fc61f061eda6a7aad019851041ce38e15ba6a33faca642ce7e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:37232bd950b2c30115d0e2e9a2c861fbf52ee2e33dfeff50914c944c05927b86",
        slice_id: "a08",
        source_locations: &["a08:1838"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|NoTerminalSignatureOrOrderProof|NoTerminalSignatureOrOrderProof.freeze_digest|freeze_digest",
        ],
        rationale_sha256: "23b56cdd10ebbcc4af2d84becf45667ae5088bcf52283e1fcad7ea4363afb317",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a13:ambiguity-adjudication:3b42cf9567870731386d634a72ce4198def5da7fa1007e561b11c740bd67e521",
        slice_id: "a13",
        source_locations: &["a13:2006"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyEnvelopeNode|KeyEnvelopeNode.inherited_roots.record.source_root_digest|source_root_digest",
        ],
        rationale_sha256: "754e4dc4bd9aa571f1b3d506d7b5ddd399258d6cf9d84b68d06cfc17b2dc9a05",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a13:ambiguity-adjudication:3d70fb474e157bb474917cb69259eb2374ba0bc450888830e9b9d4790efa4da3",
        slice_id: "a13",
        source_locations: &["a13:2006"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyEnvelopeNode|KeyEnvelopeNode.inherited_roots.record.source_root_ciphertext_digest|source_root_ciphertext_digest",
        ],
        rationale_sha256: "67bd17cb4b6968d527560d880ce0f8aba67144dcc2dcd93bad16140e9a996234",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a03:ambiguity-adjudication:60ad7a389bdf6b49899267136a03d8d6dd1a05b01b06a859ed2a2daa3ca63872",
        slice_id: "a03",
        source_locations: &["a03:1486"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|LocalTxnWorkspaceGeneration|LocalTxnWorkspaceGeneration.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "b79ad52b480455a4a25b48fe3b1617b5bbba018b1a42e52ecf4db0e91b457aa6",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a07:ambiguity-adjudication:2de6b2d328c5ebd2cd67822daa04d16386a948037599f4d4b2de7f5cdd49bd89",
        slice_id: "a07",
        source_locations: &["a07:1782"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|GlobalTxnOutcomeRecord|GlobalTxnOutcomeRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "431ec5fbd30c890ef949dfcbdcba655f29980d4138c69c39ab403ee7493d99ec",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a07:ambiguity-adjudication:81afa6cb323423720d6188e180dfd25d3e6feefee35f8c726995bbfc0f88fcbe",
        slice_id: "a07",
        source_locations: &["a07:1720"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|GlobalTxnWorkspaceGeneration|GlobalTxnWorkspaceGeneration.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "d9d5c3215b43b05e2e9374a979cd8de407cfa3f766689b485b3986c94b9450a3",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:5e6422da03c18143ac7c23a26b0a04992d0a8fa2bccab560c67f15c966ad4244",
        slice_id: "a08",
        source_locations: &["a08:1850"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuditTerminalAttemptRecord|AuditTerminalAttemptRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "aeca066a8b61a2aa7dace7be20bfbddd1d80fc2d567d54e20d3252fbe84f0587",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:8b267282f9b1651ea6b6af6c075314f90f11513814db27e6ff5c3f0817c28f63",
        slice_id: "a08",
        source_locations: &["a08:1832"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuditTerminalFreezeRecord|AuditTerminalFreezeRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "4c52a65b3c41f7ebc128ebf8250538fb960eba61fd1d38726f2378c77a760d09",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:362433ba168c3c4c4b453872a778843309bbcf7d6cf4b7682f39291ba488de18",
        slice_id: "a08",
        source_locations: &["a08:1842"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|AuditTerminalSigningPlan|AuditTerminalSigningPlan.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "aae2fe85be44fd8a744b4c93c999d1cd35b74c96f747b9776d8785a914070c8d",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:da107d6726829e70ab937238f9611c7724e392ab0377ca9f1b24e8998f107359",
        slice_id: "a08",
        source_locations: &["a08:1856"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ConstraintReservationRecord|ConstraintReservationRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "7d9f5680a21c3aee8f9a74379563b831a29cc240af5278a2e18ce9b1be78b6ae",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:fecce341a8c91acf77a729277e6ab34a2304e63fbb997817e3afc16c71c31703",
        slice_id: "a08",
        source_locations: &["a08:1858"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|MetaConstraintReservationRecord|MetaConstraintReservationRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "6a9f6a7530d082e43a533edb5bc17269f60ce5ce93757200543992780036001e",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a08:ambiguity-adjudication:6628dd4541c84c2ed0068e1cc6629df1167e7b6d53c58da3a43e34946d1d2016",
        slice_id: "a08",
        source_locations: &["a08:1854"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|ShardPrepareRecord|ShardPrepareRecord.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "56c37291396c1e91a7019c93135bf20d6de1b0019dec50c0e3765f340e4668ab",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a13:ambiguity-adjudication:bee0b07c2bcd39653dc4f186ca535b17c3e92d1df298f0695b4f2b211f5cc2db",
        slice_id: "a13",
        source_locations: &["a13:2027"],
        resolution: "maps-to-source",
        resolved_source_keys: &[
            "field|KeyEnvelopeGrantRecord<Role:AuthorityOwningRole>|KeyEnvelopeGrantRecord<Role:AuthorityOwningRole>.nonretaining_predecessor_digest|nonretaining_predecessor_digest",
        ],
        rationale_sha256: "3810555a449ca3b41f582767c776ad09b69e6b4478fb31405cf029eb986953ad",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a14:ambiguity-adjudication:1de8f2d9c62b0e47c8eb4a5a86a59d68aba9c9d016b8e5b6b5fb9e9e31d686c0",
        slice_id: "a14",
        source_locations: &["a14:2051"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|GcIrreversibleDispatchGuard"],
        rationale_sha256: "ea0069268d6dfd0c719bfe79c82d519180bcf0ac5bcd1e17163a74d4e807eb23",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a14:ambiguity-adjudication:2c72169ab8552322c9326b333646ffbf6a3bd9cc23e84d7647ade80a6ebc68d0",
        slice_id: "a14",
        source_locations: &["a14:2041"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|RetiredLocal"],
        rationale_sha256: "0a5f7542efebcde95c4f2fe14286204695473c3d5a883ce705fc21c0bb06b425",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a14:ambiguity-adjudication:2eb676b7aa2eb0af2c3fda973cc90532720a8e5d1adc7876693d376fd37f9383",
        slice_id: "a14",
        source_locations: &["a14:2039"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|RegisteredStrongRef"],
        rationale_sha256: "e8160251b05a4ec7acd90d97737d64d2f5a00e70b4f0dc639e85950e13f464fa",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a14:ambiguity-adjudication:3d80f045a0983b8ca77fd53a8327bcd88286839b395f20f78a77dc5001081e67",
        slice_id: "a14",
        source_locations: &["a14:2043"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|CurrentCheckpointObservation"],
        rationale_sha256: "0706ca5c4e537022d8016e6257116783d6ba2586cdafb35e9cf0f1ac8556380b",
    },
    AmbiguityAdjudicationContractPin {
        row_id: "a14:ambiguity-adjudication:95eef7c080056b2c1c15358eefed88f9657bff7fbb56976704f5761451d314d8",
        slice_id: "a14",
        source_locations: &["a14:2039"],
        resolution: "not-a-durable-schema",
        resolved_source_keys: &["top|MandatoryInventoryClassRegistry"],
        rationale_sha256: "c0d0e6354b66fd71dfcf92e2735558953e2e2487c6fd3d3affd28322e66c8be1",
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
        byte_count: 23_138,
        sha256: "2a580707963042340045a835e3dc3c2e3d9eb57d5ed187638cc2ff0256ca7b44",
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
        byte_count: 69_107,
        sha256: "b2219214103512f64e26081eb311d884838be4e6232ef51605ab13d4aa87e92e",
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
        byte_count: 87_440,
        sha256: "38ddc4284df07564660542d61265405133252a7ba37560646b45f62ff8e7ca78",
    },
    SlicePin {
        ordinal: 8,
        id: "a08",
        bead_id: "fgdb-a08-w12-lifecycle-pr7j",
        title: "Appendix A exact catalog: W12 retention, compaction, reconfiguration, GC, and topology formats",
        start_line: 1791,
        end_line: 1889,
        line_count: 99,
        byte_count: 92_153,
        sha256: "12c687981de6c7b05f675e8c47368744b95cb556731a9a4b509d20a8540d45db",
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
        byte_count: 17_350,
        sha256: "a5d2c59bc69c7fab13ad5b98a4df9cdbf5daa350787722f810d2b728380a278e",
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
        byte_count: 80_982,
        sha256: "98978263199392f940c0c64afc073aca367a309ce2d52910019c6be3ba6e3e14",
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
        byte_count: 97_542,
        sha256: "13426a61fb328d31b0fac83459032d7b9d82a7aef34b05452640bf2c45a55fe3",
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
        byte_count: 105_478,
        sha256: "81eb98b270c2fb8fbed5f8b5a8dbdf3a776a24927e8316b407a9d6fc72f9e342",
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
    if !concatenated.as_slice().eq(appendix) {
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
    verify_census_construction_dag(catalog, &census, out);
    Some(census)
}

/// One census reference the construction-DAG law deliberately does not enforce,
/// together with the VERDICT that licenses it.
///
/// The verdict field is mandatory and is the whole point of the table: a waiver
/// that should be a repair is a documented lie, so the two cases are named
/// separately and an `Erratum` row carries the repair that retires it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CensusDagVerdict {
    /// Both re-ordering windows are empty, derived from BOTH reference
    /// directions: the owner is pinned from above by what retains it and the
    /// target is pinned from below by what it retains. No ordering repair
    /// exists, so the shape or the reference strength is the open question.
    Exception,
    /// A repair IS available and this waiver is temporary. `repair` states it
    /// exactly so the row can be retired rather than inherited.
    ///
    /// UNCONSTRUCTED SINCE fgdb-dbta, which retired the last Erratum row
    /// (CommitCommand.capsule_ref -> CommittedEffectCapsule, repaired 30 -> 10).
    /// The variant is KEPT, not deleted: `Exception` and `Erratum` are the two
    /// verdicts this table exists to keep apart -- "there is no ordering repair"
    /// versus "there is one and here it is" -- and the contract test below asserts
    /// exactly that distinction. Errata are temporary by construction, so the next
    /// one repopulates this variant; deleting it would delete the ability to say a
    /// waiver is temporary at all.
    ///
    /// `expect`, not `allow`: the moment a row constructs `Erratum` again this
    /// expectation goes unfulfilled and the build tells whoever added it to remove
    /// the attribute, rather than silently carrying a stale suppression.
    #[expect(dead_code, reason = "no Erratum row since fgdb-dbta; see doc comment")]
    Erratum,
}

/// The four metadata fields below are read ONLY by the `#[cfg(test)]` contract
/// test that enforces waiver quality (an `Erratum` must carry its repair, an
/// `Exception` must not). `verify_census_construction_dag` itself reads only
/// `owner`/`stable_name`/`target`, so in a non-test build the rest are dead.
///
/// They are KEPT because the contract test is the whole reason a waiver may be
/// written at all: a row without measured evidence and a named owning bead is an
/// inherited excuse rather than a licensed exception. `expect` rather than
/// `allow`, so that if live code ever starts reading one the suppression is
/// reported as unfulfilled instead of lingering.
#[derive(Debug, Clone, Copy)]
struct CensusDagWaiver {
    owner: &'static str,
    stable_name: &'static str,
    target: &'static str,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by the cfg(test) waiver-contract test")
    )]
    verdict: CensusDagVerdict,
    /// The measured window, both directions, that produced the verdict.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by the cfg(test) waiver-contract test")
    )]
    evidence: &'static str,
    /// For an `Erratum`, the exact repair that retires this row.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by the cfg(test) waiver-contract test")
    )]
    repair: &'static str,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by the cfg(test) waiver-contract test")
    )]
    owning_bead: &'static str,
}

/// The complete waiver set for the census-level construction DAG (fgdb-owlp).
///
/// Measured at HEAD over the funnel documented on `verify_census_construction_dag`.
/// The bead proposed waiving 14 future-result witnesses plus 19 self-edges; every
/// one of those was re-derived and all but these two are gone — seven were
/// resolved by re-ordering (`construction_order` is not pinned), and the self-edge
/// population went to zero once arm-path references were excluded, because an arm
/// payload member can never legally become an enforced field row.
const CENSUS_DAG_WAIVERS: &[CensusDagWaiver] = &[
    // The Erratum waiver that lived here — CommitCommand.capsule_ref ->
    // CommittedEffectCapsule — is RETIRED (fgdb-dbta). Its repair landed: the
    // target moved 30 -> 10, which is the single value its window admits.
    // Constraints, all three, so the window is checkable at a glance:
    //     CommitCommand@10 -> CEC                    =>  CEC <= 10
    //     CommitMarker@30  -> CEC                    =>  CEC <= 30
    //     CEC -> AuthorizationDecisionRecord@10      =>  CEC >= 10
    //     window = [10, 10]
    // The upper bound comes from CommitCommand through a landed REFERENCE-UNION
    // ARM, not from a plain field target_schema_id — a reader that walks only
    // [[field]] rows sees CommitMarker@30 alone, computes [10, 30], and concludes
    // 30 is legal. It is not, and that misreading is what kept this waiver alive.
    CensusDagWaiver {
        owner: "CommitCommand",
        stable_name: "final_certification_reservation_ref",
        target: "LocalFinalCertificationReservation",
        verdict: CensusDagVerdict::Exception,
        evidence: "both windows empty: the owner is pinned <= 10 by CommitCapsule@10, and the \
                   target is pinned >= 20 because it retains LocalFinalCertificationReserveSpec@20 \
                   while it would need <= 10. No ordering repair exists, so this is a \
                   reference-strength or shape question, not an order one",
        repair: "",
        owning_bead: "fgdb-a10-command-delta-ooy1",
    },
];

/// LAW: the construction DAG must be checked over CENSUS references, not only
/// over landed `[[field]]` rows (fgdb-owlp).
///
/// `validate_identity` enforces `dag_self_edge`, `dag_future_result` and
/// `dag_cycle` over rows that EXIST. A census strong reference with no row yet
/// contributes no edge, so a slice can pass every check while already carrying an
/// unsatisfiable ordering — and it only detonates when someone does the honest
/// field-body modelling, by which time the orders are frozen and the window may be
/// empty. Checking the census surfaces the contradiction at MINT time, while the
/// order is still free to choose.
///
/// The scope is derived, not assumed, and every narrowing below is a measured
/// non-obligation rather than a silent skip:
///   * RETAINING only. Strength comes from `registered_reference_definition_semantics`
///     — the same table `identity::declared_field_reference_semantics` consults, so a
///     wrapper cannot be retaining on one artifact and not the other. 13 of the 17
///     registered `reference_wrapper` types are retaining (7 strong + 6 conditional);
///     `locator` and the three `*Identity` wrappers impose no ordering obligation,
///     which is the a01 identity-is-not-reachability law used as an admission rule.
///   * FLAT census paths only. A candidate deeper than `{Owner}.{stable_name}` is an
///     arm PAYLOAD member; `validate_identity` accepts a `[[field]]` row on a union
///     owner, so such a row can never legally land, and counting it here would report
///     violations no correct modelling can produce (783 of 1950 are arm-path).
///   * A bare wrapper with no concrete target in the source spelling names no target,
///     so no edge is derivable from it.
///   * An owner or target that is not a registered logical kind has no order to
///     compare; the law cannot rule on what is not yet minted.
///
/// The COMPLETENESS GUARD fails CLOSED: a registered `reference_wrapper` whose
/// strength the shared table does not classify is a VIOLATION, never a skip. Zero
/// such wrappers exist today, which is exactly why the guard must be present — the
/// next wrapper added without a strength lands silently otherwise.
fn verify_census_construction_dag(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    let id = &catalog.identity;
    let wrapper_kind: BTreeMap<&str, &str> = id
        .wire
        .iter()
        .map(|wire| (wire.name.as_str(), wire.kind.as_str()))
        .collect();
    let order: BTreeMap<&str, i64> = id
        .logical
        .iter()
        .map(|kind| (kind.name.as_str(), kind.construction_order))
        .collect();
    let landed: BTreeSet<(&str, &str)> = id
        .fields
        .iter()
        .map(|field| {
            (
                identity::generic_free_family(&field.containing_schema),
                field.stable_name.as_str(),
            )
        })
        .collect();
    let waived: BTreeSet<(&str, &str, &str)> = CENSUS_DAG_WAIVERS
        .iter()
        .map(|w| (w.owner, w.stable_name, w.target))
        .collect();

    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for slice in &census.slices {
        for field in &slice.fields {
            let Some(exact) = field.exact_types.first() else {
                continue;
            };
            let (family, bracket_target) = census_reference_carrier(exact);
            let row_id = format!("{}#{}", field.key.schema_owner, field.key.stable_name);
            if wrapper_kind.get(family).copied() != Some("reference_wrapper") {
                // CARRIER CLOSURE GUARD — fails CLOSED on an unrecognised carrier.
                //
                // The bracket carrier below was silently dropped for the whole life
                // of this law because the family split ran on the raw spelling, so
                // `[StrongRef<T>]` yielded the family `"[StrongRef"`, missed the
                // lookup, and `continue`d BEFORE reaching any guard. 139 census
                // references died there, 2 of them violating. A skip that happens
                // before the guard is not a narrowing, it is a hole.
                //
                // So: if the spelling MENTIONS a registered reference_wrapper at a
                // flat path and no carrier shape above explained it, that is a new
                // carrier and it must be classified rather than dropped. The only
                // licensed exception is an inline AGGREGATE spelling (a sum `|` or
                // a record `{...}`), whose references live in member/arm payloads
                // that are their own census candidates at deeper paths -- 81 of
                // them today, already covered by the arm-payload narrowing.
                if field.key.path == format!("{}.{}", field.key.schema_owner, field.key.stable_name)
                    && !exact.contains('|')
                    && !exact.contains('{')
                    && let Some(mentioned) = wrapper_kind.iter().find_map(|(name, kind)| {
                        (*kind == "reference_wrapper" && exact.contains(&format!("{name}<")))
                            .then_some(*name)
                    })
                {
                    out.push(Violation::new(
                        "census_reference_carrier_unrecognised",
                        row_id,
                        format!(
                            "source spelling {exact:?} mentions registered reference_wrapper \
                             {mentioned:?} but no recognised carrier shape (plain, bracket array, \
                             or bracket map) derives a target from it: teach the carrier \
                             derivation this shape rather than letting the edge vanish"
                        ),
                    ));
                }
                continue;
            }
            // COMPLETENESS GUARD — an unclassified wrapper fails CLOSED.
            let Some(semantics) = census_reference_strength(family) else {
                out.push(Violation::new(
                    "census_reference_wrapper_unclassified",
                    row_id,
                    format!(
                        "registered reference_wrapper {family:?} has no strength in the shared \
                         reference-definition table, so the construction DAG cannot tell whether \
                         it retains its target: classify it rather than letting it pass"
                    ),
                ));
                continue;
            };
            if !matches!(semantics, "strong" | "conditional" | "weak_digest") {
                continue;
            }
            if field.key.path != format!("{}.{}", field.key.schema_owner, field.key.stable_name) {
                continue;
            }
            let owner = identity::generic_free_family(&field.key.schema_owner);
            if landed.contains(&(owner, field.key.stable_name.as_str())) {
                continue;
            }
            let Some(target) = bracket_target.map(identity::generic_free_family) else {
                continue;
            };
            let (Some(&owner_order), Some(&target_order)) = (order.get(owner), order.get(target))
            else {
                continue;
            };
            if waived.contains(&(owner, field.key.stable_name.as_str(), target)) {
                continue;
            }
            if owner == target {
                out.push(Violation::new(
                    "census_dag_self_edge",
                    row_id,
                    format!(
                        "source reference {owner:?}.{} retains {target:?}: a schema may not \
                         reference itself, and the landed-row law cannot see this until a field \
                         body lands",
                        field.key.stable_name
                    ),
                ));
                continue;
            }
            if target_order > owner_order {
                out.push(Violation::new(
                    "census_dag_future_result",
                    row_id,
                    format!(
                        "source reference {owner:?}@{owner_order} retains {target:?}@{target_order}: \
                         a future result is never referenceable, and the order is still free to \
                         change only until a field body lands"
                    ),
                ));
            }
            edges.entry(owner).or_default().insert(target);
        }
    }
    // dag_cycle is a SEPARATE law from dag_future_result: a graph can be free of
    // strict future edges and still carry a cycle among equal-order kinds, which no
    // amount of re-ordering repairs.
    if let Some(cycle) = identity::find_construction_cycle(&edges) {
        out.push(Violation::new(
            "census_dag_cycle",
            cycle.first().copied().unwrap_or(""),
            format!("source construction-DAG cycle among census references: {cycle:?}"),
        ));
    }
}

/// Derive `(wrapper family, concrete target)` from ONE source spelling, across
/// every carrier shape a reference is written in (fgdb-h1al).
///
/// A reference reaches its target through more than one spelling, and the law
/// previously split `'<'` on the raw text, which recognised exactly one of them:
///
///   plain          `StrongRef<T>`                    -> family `StrongRef`
///   bracket array  `[StrongRef<T>]`                  -> family `"[StrongRef"`   MISSED
///   bracket map    `[k -> StrongRef<T>]`             -> family `"[k -> StrongRef"` MISSED
///
/// The two missed forms are how the census records a MANY-cardinality reference,
/// so the law was blind to the plural case while enforcing the singular one. That
/// is measured, not inferred: 384 of 11773 census field candidates are bracket
/// spellings, 139 of those name a registered `reference_wrapper`, and 2 of those
/// were violating the DAG in silence.
///
/// The map form takes the spelling after the LAST `->` because the retaining
/// member of `[key -> Ref<T>]` is the VALUE; the key is an identity, not a
/// reference. `->` is honoured only INSIDE a bracket: no flat spelling in the
/// census names a registered wrapper across an arrow, so widening the flat path
/// too would be unmeasured scope.
///
/// A bare wrapper with no `<...>` yields `None` for the target and therefore no
/// edge, which is the pre-existing "names no target" narrowing, unchanged.
fn census_reference_carrier(exact: &str) -> (&str, Option<&str>) {
    let mut spelling = exact.trim();
    if let Some(inner) = spelling
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        spelling = inner.trim();
        if let Some(arrow) = spelling.rfind("->") {
            spelling = spelling[arrow + 2..].trim();
        }
    }
    let family = spelling.split('<').next().unwrap_or(spelling);
    let target = spelling
        .strip_prefix(family)
        .and_then(|rest| rest.strip_prefix('<'))
        .and_then(|rest| rest.strip_suffix('>'))
        .map(str::trim);
    (family, target)
}

/// The strength the construction DAG assigns a reference wrapper, composed exactly
/// as `identity::declared_field_reference_semantics` composes it so the two
/// artifacts cannot drift apart.
fn census_reference_strength(family: &str) -> Option<&'static str> {
    match registered_reference_definition_semantics(family) {
        Some("identity") => Some("none"),
        Some(other) => Some(other),
        None => match family {
            "WeakGlobalCommandIdentity" | "WeakMarkerIdentity" | "WeakShardCommandIdentity" => {
                Some("none")
            }
            _ => None,
        },
    }
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
    // Collection-element interiors (fgdb-k3sa).  A repeated field's elements are
    // spelled `<owner>.<field>.record.<member>` in the census, and those member
    // fields are projected through the repeated field's own contract exactly as
    // an arm payload is projected through its arm.  Before this, a collection
    // element was a THIRD carrier shape that neither the arm nor the wire branch
    // knew, so re-keying an element member onto its minted element kind dropped
    // its census key out of `projected_source_keys` entirely.
    //
    // Built by RECONSTRUCTION from the typed identity row, never by parsing the
    // source key: `|` is the key separator and is also legal inside a generic
    // signature (fgdb-tfow), so a `split('|')` with a fixed part count silently
    // skips a generic owner — a fail-open in a coverage set.
    //
    // Deliberately narrower than the arm branch: only census FIELDS are covered
    // this way.  A union or arm nested under `.record.` still needs its own
    // contract, which is the fail-closed direction.
    let mut record_prefixes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for field in &catalog.identity.fields {
        if field.cardinality != "many" {
            continue;
        }
        record_prefixes
            .entry(field.containing_schema.as_str())
            .or_default()
            .push(format!(
                "{}.{}.record.",
                field.containing_schema, field.stable_name
            ));
    }
    let arm_prefix_covers = |owner: &str, container_path: &str| {
        arm_prefixes.get(owner).is_some_and(|prefixes| {
            prefixes
                .iter()
                .any(|prefix| container_path.starts_with(prefix.as_str()))
        })
    };
    let record_prefix_covers = |owner: &str, container_path: &str| {
        record_prefixes.get(owner).is_some_and(|prefixes| {
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
            || record_prefix_covers(field.key.schema_owner.as_str(), &field.key.path)
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

/// One licence for the empty domain of the complete-slice census law, together
/// with the repair that retires it (fgdb-complete-census-law-vacuous-twice-54jf).
///
/// The two metadata fields are read ONLY by the `#[cfg(test)]` contract test
/// that enforces licence quality: a row without measured evidence and a named
/// repair is an inherited excuse rather than a licensed vacuity. `expect` rather
/// than `allow`, so that if live code ever starts reading one the suppression is
/// reported as unfulfilled instead of lingering.
#[derive(Debug, Clone, Copy)]
struct CompleteCensusDomainWaiver {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by the cfg(test) licence-contract test")
    )]
    evidence: &'static str,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by the cfg(test) licence-contract test")
    )]
    repair: &'static str,
    owning_bead: &'static str,
}

/// The complete licence set for an EMPTY complete-slice domain.
///
/// Exactly one row while no slice has ever been completed. Emptying this table
/// turns the vacuity into `source_complete_census_domain_vacuous`; completing
/// any slice turns the row into `source_complete_census_domain_waiver_stale`.
/// One of the three states is licensed at a time, so a law with no subjects can
/// no longer report the same clean zero as a law with all of them.
const COMPLETE_CENSUS_DOMAIN_WAIVERS: &[CompleteCensusDomainWaiver] =
    &[CompleteCensusDomainWaiver {
        evidence: "measured at HEAD: 0 of 21 slices carry definition_status = \"complete\"; all 21 \
                   are \"declared\", so the coverage loop body never executes. The predicate is \
                   live, not dead: forcing one slice complete yields 82 (a20), 328 (a03) or 720 \
                   (a18) violations and forcing all 21 yields 8616",
        repair: "mark the first slice complete once its interior residue reaches zero AND its \
                 census universe is certified; this row must be deleted in the same change",
        owning_bead: "fgdb-complete-census-law-vacuous-twice-54jf",
    }];

/// Slices whose census universe has been shown to hold the source it quantifies
/// over, and which may therefore CLAIM `definition_status = "complete"`.
///
/// EMPTY, and the emptiness is the enforcement rather than a second vacuity: a
/// slice marked complete while absent from this list is a violation naming the
/// universe, so the coverage certificate cannot be issued over a census known to
/// drop source. `fgdb-qh3r` recovered the owner-bound supplemental posture
/// unions in a18 and a20, but did not certify either slice's whole census. The
/// separate a03 anonymous-common-header ownership gap remains tracked by
/// `fgdb-ckb9`. Add a slice here only with the measurement that its census emits
/// every member its source spells.
const CENSUS_UNIVERSE_CERTIFIED_SLICES: &[&str] = &[];

/// The complete-slice field census law (fgdb-z35a): every census field key of
/// a complete slice must be covered by exactly one verified contract — a
/// field target, an approved not-a-durable-schema adjudication, or a covering
/// arm/wire interior contract.  The covered classes are census-derived, so
/// this equality lives in the source pass; a catalog-only sha-equality pin
/// cannot express them.  Extra targeted keys are rejected independently by
/// `verify_structural_target_source_keys`, and adjudicated key sets are
/// byte-matched to the census, so one-directional coverage completeness here
/// closes full set equality.
///
/// # It was vacuous twice over, and both narrowings are now checked
///
/// `fgdb-complete-census-law-vacuous-twice-54jf`.
///
/// NARROWING 1 — AN EMPTY DOMAIN. The loop is gated on `definition_status ==
/// "complete"` and every one of the 21 slices is `declared`, so the body never
/// executed and the law reported zero because it evaluated nothing. That is the
/// purest vacuity: every instrument reading the output saw the clean zero a
/// fully covered appendix would produce. `COMPLETE_CENSUS_DOMAIN_WAIVERS` now
/// licenses that state explicitly and fails in BOTH directions around it.
///
/// NARROWING 2 — THE UNIVERSE IS THE CENSUS, WHICH DROPS SOURCE. The keys are
/// `source_slice.fields/.unions/.arms`: census OUTPUT, not source. A member the
/// census never emitted is not in the universe and cannot be flagged, so the
/// certificate is completeness relative to whatever the census happened to see.
/// The reverse direction is already closed — `source_target_key_missing` rejects
/// a targeted key the census lacks — and what remains is a member absent from
/// BOTH, which no reader in this crate can see. So the CLAIM is gated instead:
/// see `CENSUS_UNIVERSE_CERTIFIED_SLICES`.
fn verify_complete_field_census_coverage(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    out: &mut Vec<Violation>,
) {
    verify_complete_field_census_coverage_with(
        catalog,
        census,
        COMPLETE_CENSUS_DOMAIN_WAIVERS,
        CENSUS_UNIVERSE_CERTIFIED_SLICES,
        out,
    );
}

/// The body, with the two ledgers injected so a test can mutate the input each
/// vacuity hides and observe the law go red.
fn verify_complete_field_census_coverage_with(
    catalog: &Catalog,
    census: &AppendixSourceCensus,
    domain_waivers: &[CompleteCensusDomainWaiver],
    certified_slices: &[&str],
    out: &mut Vec<Violation>,
) {
    let complete_slice_count = catalog
        .slices
        .iter()
        .filter(|slice| slice.definition_status == "complete")
        .count();
    match (complete_slice_count, domain_waivers.first()) {
        (0, None) => out.push(Violation::new(
            "source_complete_census_domain_vacuous",
            "source_manifest",
            format!(
                "the complete-slice census law evaluated 0 of {} slices, so its zero certifies \
                 nothing; license the empty domain or complete a slice",
                catalog.slices.len()
            ),
        )),
        (0, Some(_)) => {}
        (_, Some(waiver)) => out.push(Violation::new(
            "source_complete_census_domain_waiver_stale",
            "source_manifest",
            format!(
                "{complete_slice_count} slices are complete, so the empty-domain licence owned by \
                 {} no longer licenses anything and must be deleted",
                waiver.owning_bead
            ),
        )),
        (_, None) => {}
    }
    for certified in certified_slices {
        if !catalog
            .slices
            .iter()
            .any(|slice| slice.id.as_str().eq(*certified))
        {
            out.push(Violation::new(
                "source_complete_census_certification_stale",
                *certified,
                "a certified census universe names a slice the catalog does not carry",
            ));
        }
    }

    let covered = covered_interior_keys(catalog, census);
    for slice in catalog
        .slices
        .iter()
        .filter(|slice| slice.definition_status == "complete")
    {
        if !certified_slices.contains(&slice.id.as_str()) {
            out.push(Violation::new(
                "source_complete_census_universe_uncertified",
                &slice.id,
                "this slice claims completeness while its census universe is uncertified: the law \
                 below quantifies over census output, so a member the census dropped cannot be \
                 flagged and the certificate would be complete only relative to what was seen",
            ));
        }
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
    if !manifest.target_count.eq(&target_count)
        || manifest.target_ids_sha256 != census.target_ids_sha256
        || !manifest.occurrence_count.eq(&occurrence_count)
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
            match &identity {
                Some((suffix, _)) => {
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
                // COMPLETENESS GUARD, and it must stay.  `projection_row_identity`
                // returns None for any catalog key its match does not name, and
                // this arm used to be an `if let` with no else: a projection row
                // kind added tomorrow would have had its row_id silently
                // UNCHECKED while every other law kept passing, which reads as
                // enforced.  Measured 2026-07-27: 15 catalog row kinds carry a
                // row_id over 10320 rows and all 15 are covered today, so this
                // guard is not currently reachable from the shipped catalog --
                // that is exactly when a fail-open is cheapest to close and
                // hardest to notice.  Registering a new kind in
                // `projection_row_identity` is the fix; suppressing this code is
                // not.
                None => violations.push(Violation::new(
                    "catalog_row_id_derivation_unregistered",
                    &row_id,
                    format!(
                        "catalog row kind {catalog_key:?} has no row_id derivation in projection_row_identity; register one rather than leaving the row unchecked"
                    ),
                )),
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
    if !manifest.target_count.eq(&target_count)
        || !manifest.target_ids_sha256.eq(&target_ids_sha256)
        || target_count != i64::try_from(EXPECTED_TYPE_RESERVATION_COUNT).unwrap_or(i64::MAX)
        || !manifest
            .target_ids_sha256
            .eq(EXPECTED_REFERENCE_TARGET_IDS_SHA256)
        || manifest.occurrence_count
            != i64::try_from(EXPECTED_REFERENCE_OCCURRENCE_COUNT).unwrap_or(i64::MAX)
        || !manifest
            .occurrence_transcript_sha256
            .eq(EXPECTED_REFERENCE_OCCURRENCE_SHA256)
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
    if !manifest.target_count.eq(&target_count)
        || target_count != i64::try_from(EXPECTED_PROJECTION_ROW_COUNT).unwrap_or(i64::MAX)
        || !manifest
            .projection_fallback_count
            .eq(&projection_fallback_count)
        || projection_fallback_count
            != i64::try_from(EXPECTED_PROJECTION_FALLBACK_COUNT).unwrap_or(i64::MAX)
        || !manifest
            .target_source_assignment_sha256
            .eq(&assignment_sha256)
        || !manifest
            .target_source_assignment_sha256
            .eq(EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256)
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
        // The pin no longer carries a copy of `ambiguity_source_key`. It does
        // not need one: `row_id` IS that field's digest, by the derivation law
        // `validate_ambiguity_adjudication_rows` enforces, and the pin above was
        // found by `row_id` equality. Re-deriving it HERE rather than leaning on
        // that other validator keeps this an INDEPENDENT check -- the derivation
        // law pushes a violation, it does not abort, so a caller reaching this
        // function cannot assume it ran. Owner-authorised removal of 450 mirrored
        // literals (fgdb-checker-mirrors-subject-prose-23u1, candidate 2): the
        // copy answered existence queries about its own subject, and 766 of the
        // file's 1264 distinct 64-hex digests appeared ONLY inside it.
        && pin.row_id
            == format!(
                "{}:ambiguity-adjudication:{}",
                row.slice_id,
                sha256_hex(row.ambiguity_source_key.as_bytes())
            )
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
        // The pin no longer carries a copy of `rationale`; it carries the
        // digest of the bytes it approved, which is equivalent for this
        // function's only use. All eight callers read the result as a BOOLEAN
        // APPROVAL PREDICATE and then read the CATALOG row's fields -- no call
        // site anywhere reads a pinned rationale as text (fgdb-n061).
        && sha256_hex(row.rationale.as_bytes()) == pin.rationale_sha256
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
    // CONDITION 2 OF THE fgdb-n061 AUTHORISATION: a guard that FAILS CLOSED
    // when a pin's rationale digest and the catalog's rationale have drifted.
    //
    // WHY IT IS SEPARATE FROM THE MATCH ABOVE, which compares the same two
    // values. `..._contract_mismatch` is ONE code covering six fields, so a
    // rationale drift and a resolution drift are indistinguishable in the
    // report -- and a rationale drift is the one failure this change makes
    // possible that was not possible before. It gets its own code so it is
    // NAMED when it happens. It is checked here rather than only inside the
    // approval predicate because that predicate is a BOOLEAN: a caller that
    // never invokes it never asks the question. This loop asks it for every
    // row, unconditionally.
    //
    // Both directions fail closed. A malformed digest is its own violation
    // rather than a value that happens not to match, because a pin carrying
    // `""` or a truncated digest would otherwise report as an ordinary drift
    // and send a reader looking at the prose instead of at the pin.
    //
    // MUTATION-PROVEN 2026-07-27 in a quiet root, each alone, with a clean
    // control at 0 violations / exit 0:
    //   weaken one catalog rationale ("derived from" -> "ASSUMED")
    //       -> catalog_ambiguity_rationale_digest_mismatch, exit 1
    //   truncate one pin digest to "deadbeef"
    //       -> catalog_ambiguity_rationale_digest_malformed, exit 1
    for row in &catalog.ambiguity_adjudications {
        let Some(pin) = AMBIGUITY_ADJUDICATION_CONTRACT
            .iter()
            .find(|pin| pin.row_id == row.row_id)
        else {
            continue;
        };
        if pin.rationale_sha256.len() != 64
            || !pin
                .rationale_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            out.push(Violation::new(
                "catalog_ambiguity_rationale_digest_malformed",
                &row.row_id,
                "pinned rationale digest is not 64 lowercase hex characters",
            ));
            continue;
        }
        if sha256_hex(row.rationale.as_bytes()) != pin.rationale_sha256 {
            out.push(Violation::new(
                "catalog_ambiguity_rationale_digest_mismatch",
                &row.row_id,
                "catalog rationale does not hash to its pinned digest; the prose \
                 and the approval that covered it have drifted",
            ));
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
    if !computed_lines.eq(&Some(manifest.line_count)) {
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
        "AbandonedRestoreTerminalPinBasisRef"
        | "CanonicalBootstrapOpenersRef"
        | "CanonicalVerifiedObjectInventoryRef"
        | "CertifiedRemoteStrongRef"
        | "ClaimedReservationUseRef"
        | "ExternalCasRestoreServicePromotionManifestRef"
        | "ExternalCasRestoreServicePromotionReceiptRef"
        | "LocalRestoreStateRef"
        | "MetaRestoreStateRef"
        | "OperationalRestoreTerminalPinBasisRef"
        | "RegisteredStrongRef"
        | "RemoteConfigurationRef"
        | "ReservedReservationUseRef"
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
        if let Some(value) = &row.construction_relation {
            write_string(&mut out, "construction_relation", value);
        }
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

    /// The completeness guard on the derived-row_id law.
    ///
    /// `projection_row_identity` names ten catalog keys and returns None for
    /// everything else.  Before this control the caller matched that None with a
    /// bare `if let Some`, so a projection row kind nobody had registered was
    /// skipped in silence while the surrounding laws stayed green.  The positive
    /// half proves a registered kind is still checked; the negative half proves
    /// an unregistered one is a violation rather than a skip.
    #[test]
    fn appendix_unregistered_projection_row_kind_fails_closed() {
        let document = r#"
[[union]]
slice_id = "a01"
row_id = "a01:union:whatever"
union_name = "Probe"
containing_schema = "Probe"
union_path = "Probe"

[[probe_kind]]
slice_id = "a01"
row_id = "a01:probe-kind:whatever"
name = "Probe"
"#;
        let root = toml::parse(document).expect("completeness fixture parses");

        // Positive control: a REGISTERED kind still reaches the derivation law,
        // so the negative below cannot pass for the wrong reason.
        let mut registered = Vec::new();
        let mut registered_meta = Vec::new();
        catalog_projection_rows(
            &root,
            "union",
            "durable_fields",
            "union",
            &mut registered_meta,
            &mut registered,
        )
        .expect("union rows");
        assert_eq!(
            registered
                .iter()
                .filter(|violation| violation.code == "catalog_row_id_derived_mismatch")
                .count(),
            1,
            "a registered kind with a hand-written row_id must still fail: {registered:?}"
        );
        assert!(
            !registered
                .iter()
                .any(|violation| violation.code == "catalog_row_id_derivation_unregistered"),
            "a registered kind must not report itself unregistered: {registered:?}"
        );

        // The guard itself: an unregistered kind must be a violation, not a skip.
        let mut unregistered = Vec::new();
        let mut unregistered_meta = Vec::new();
        catalog_projection_rows(
            &root,
            "probe_kind",
            "durable_fields",
            "probe-kind",
            &mut unregistered_meta,
            &mut unregistered,
        )
        .expect("probe rows");
        assert_eq!(
            unregistered
                .iter()
                .filter(|violation| violation.code == "catalog_row_id_derivation_unregistered")
                .count(),
            1,
            "an unregistered projection row kind must fail closed: {unregistered:?}"
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

    // --- census construction DAG (fgdb-owlp) --------------------------------

    fn typed_field_candidate(
        owner: &str,
        path: &str,
        stable_name: &str,
        exact: &str,
    ) -> FieldCandidate {
        let mut candidate = field_candidate(owner, path, stable_name);
        candidate.exact_types = vec![exact.to_owned()];
        candidate
    }

    fn census_dag_codes(fields: Vec<FieldCandidate>) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let census = census_with_slice("a10", fields, Vec::new());
        let mut violations = Vec::new();
        verify_census_construction_dag(&catalog, &census, &mut violations);
        violations
            .into_iter()
            .map(|violation| violation.code)
            .collect()
    }

    /// The waiver metadata is load-bearing, not decoration: an `Erratum` is a
    /// temporary row that must carry the repair retiring it, and an `Exception`
    /// must NOT carry one, because "there is a fix" and "there is no fix" are the
    /// two verdicts the table exists to keep apart.
    #[test]
    fn census_dag_waivers_state_a_verdict_and_an_erratum_states_its_repair() {
        assert!(
            !CENSUS_DAG_WAIVERS.is_empty(),
            "non-vacuity: an empty waiver table would make the checks below meaningless"
        );
        for waiver in CENSUS_DAG_WAIVERS {
            assert!(
                !waiver.evidence.is_empty(),
                "{}.{} must state the measured window that licenses it",
                waiver.owner,
                waiver.stable_name
            );
            assert!(
                waiver.owning_bead.starts_with("fgdb-"),
                "{}.{} must name the bead that owns the ruling",
                waiver.owner,
                waiver.stable_name
            );
            match waiver.verdict {
                CensusDagVerdict::Erratum => assert!(
                    !waiver.repair.is_empty(),
                    "{}.{} is an ERRATUM, so it must state the repair that retires it — a waiver \
                     that should be a fix is a documented lie",
                    waiver.owner,
                    waiver.stable_name
                ),
                CensusDagVerdict::Exception => assert!(
                    waiver.repair.is_empty(),
                    "{}.{} is an EXCEPTION, so it must not claim a repair exists",
                    waiver.owner,
                    waiver.stable_name
                ),
            }
        }
    }

    /// CONTROL, firing direction: an edge the waiver table does NOT cover must be
    /// named.
    ///
    /// The probe used to retain CommittedEffectCapsule, which worked only while
    /// CEC sat at 30 above CommitCommand@10 — i.e. the fixture depended on the
    /// very erratum fgdb-dbta repaired. With CEC now at 10 that edge is legal and
    /// the control had nothing to find, which is a fixture that decayed into a
    /// pass rather than a law that changed. It now retains CommitMarker@30, which
    /// is above CommitCommand@10 for a reason no repair is going to remove.
    #[test]
    fn census_dag_names_a_future_result_the_waiver_does_not_cover() {
        let codes = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.unwaived_probe_ref",
            "unwaived_probe_ref",
            "StrongRef<CommitMarker>",
        )]);
        assert!(
            codes.contains(&"census_dag_future_result".to_owned()),
            "an uncovered future-result edge must be named, got {codes:?}"
        );
    }

    /// fgdb-h1al: the SAME edge in the bracket-array spelling must be seen. Before
    /// the carrier derivation existed, `[StrongRef<T>]` split to the family
    /// `"[StrongRef"`, missed the wrapper lookup and `continue`d before any guard --
    /// so the plural spelling of a reference was unenforced while the singular one
    /// was enforced. 139 census references were dropped that way, 2 of them
    /// violating.
    #[test]
    fn census_dag_sees_the_bracket_array_carrier() {
        let codes = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.unwaived_bracket_probe_refs",
            "unwaived_bracket_probe_refs",
            "[StrongRef<CommitMarker>]",
        )]);
        assert!(
            codes.contains(&"census_dag_future_result".to_owned()),
            "a bracket-array future-result edge must be named, got {codes:?}"
        );
    }

    /// fgdb-h1al: and in the bracket-MAP spelling, whose retaining member is the
    /// VALUE after the last `->`. Four census candidates use this form and every
    /// one of them retains a real target.
    #[test]
    fn census_dag_sees_the_bracket_map_carrier() {
        let codes = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.unwaived_map_probe_entries",
            "unwaived_map_probe_entries",
            "[probe_id -> StrongRef<CommitMarker>]",
        )]);
        assert!(
            codes.contains(&"census_dag_future_result".to_owned()),
            "a bracket-map future-result edge must be named, got {codes:?}"
        );
    }

    /// fgdb-h1al COMPLETENESS GUARD, failing direction: a spelling that mentions a
    /// registered wrapper but that no carrier shape explains is a VIOLATION, never
    /// a silent skip. This is the guard that would have caught the bracket hole on
    /// the day it was written.
    #[test]
    fn census_dag_guard_fires_on_an_unrecognised_carrier() {
        let codes = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.unrecognised_carrier_probe",
            "unrecognised_carrier_probe",
            "Vec<StrongRef<CommitMarker>>",
        )]);
        assert!(
            codes.contains(&"census_reference_carrier_unrecognised".to_owned()),
            "an unrecognised carrier mentioning a registered wrapper must fail CLOSED, got {codes:?}"
        );
    }

    /// CONTROL for the guard, passing direction, so it is not merely always-on: an
    /// inline AGGREGATE spelling carries its references in member/arm payloads,
    /// which are their own census candidates at deeper paths. 81 candidates are in
    /// this class and none of them owes a top-level edge.
    #[test]
    fn census_dag_guard_licenses_an_inline_aggregate_spelling() {
        let codes = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.inline_aggregate_probe",
            "inline_aggregate_probe",
            "None|Some{marker_ref:StrongRef<CommitMarker>}",
        )]);
        assert!(
            !codes.contains(&"census_reference_carrier_unrecognised".to_owned()),
            "an inline aggregate must not fire the carrier guard, got {codes:?}"
        );
    }

    /// CONTROL, passing direction: the same edge under its WAIVED field name is
    /// accepted, so the table is what licenses it rather than the law being blind.
    #[test]
    fn census_dag_accepts_the_waived_spelling_of_the_same_edge() {
        let codes = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.capsule_ref",
            "capsule_ref",
            "StrongRef<CommittedEffectCapsule>",
        )]);
        assert!(codes.is_empty(), "the waived edge must pass, got {codes:?}");
    }

    #[test]
    fn census_dag_names_a_self_edge_and_ignores_an_arm_path_one() {
        let flat = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.self_probe_ref",
            "self_probe_ref",
            "StrongRef<CommitCommand>",
        )]);
        assert!(
            flat.contains(&"census_dag_self_edge".to_owned()),
            "a flat self-edge must be named, got {flat:?}"
        );
        // An arm payload member can never legally become an enforced field row, so
        // reporting it would be a violation no correct modelling could produce.
        let arm = census_dag_codes(vec![typed_field_candidate(
            "CommitCommand",
            "CommitCommand.state.Started.self_probe_ref",
            "self_probe_ref",
            "StrongRef<CommitCommand>",
        )]);
        assert!(
            arm.is_empty(),
            "an arm-path reference must not be ruled on, got {arm:?}"
        );
    }

    /// CONTROL for the COMPLETENESS GUARD. A registered `reference_wrapper` whose
    /// strength the shared table does not classify must FAIL CLOSED. No such
    /// wrapper exists today, so the guard is proved by adding one.
    #[test]
    fn census_dag_completeness_guard_fails_closed_on_an_unclassified_wrapper() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let mut wrapper = catalog
            .identity
            .wire
            .iter()
            .find(|wire| wire.name == "StrongRef")
            .expect("StrongRef is registered")
            .clone();
        wrapper.name = "UnclassifiedProbeRef".to_owned();
        catalog.identity.wire.push(wrapper);
        assert!(
            census_reference_strength("UnclassifiedProbeRef").is_none(),
            "the fixture wrapper must be genuinely unclassified"
        );
        let census = census_with_slice(
            "a10",
            vec![typed_field_candidate(
                "CommitCommand",
                "CommitCommand.unclassified_probe_ref",
                "unclassified_probe_ref",
                "UnclassifiedProbeRef<CommittedEffectCapsule>",
            )],
            Vec::new(),
        );
        let mut violations = Vec::new();
        verify_census_construction_dag(&catalog, &census, &mut violations);
        let codes: Vec<&str> = violations.iter().map(|v| v.code.as_str()).collect();
        assert!(
            codes.contains(&"census_reference_wrapper_unclassified"),
            "an unclassified wrapper must fail closed rather than be skipped, got {codes:?}"
        );
    }

    /// The real Appendix source must be clean under the law as landed, reached
    /// through the REAL entry point rather than by calling the check directly —
    /// which also proves the law is actually wired into `verify_source`. This is
    /// the statement that the waiver set is COMPLETE at HEAD, and it is what goes
    /// red when a new latent violation is authored.
    #[test]
    fn census_dag_is_clean_over_the_real_appendix_source() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let source = fs::read(root.join(PLAN_PATH)).expect("plan reads");
        let census_dag: Vec<(String, String)> = verify_source(&catalog, &source)
            .into_iter()
            .filter(|violation| {
                violation.code.starts_with("census_dag_")
                    || violation.code == "census_reference_wrapper_unclassified"
            })
            .map(|violation| (violation.code, violation.msg))
            .collect();
        assert!(
            census_dag.is_empty(),
            "the census construction DAG must be clean at HEAD under the landed waiver set, got {census_dag:?}"
        );
    }

    fn uncovered_field_violations(violations: &[Violation]) -> Vec<&Violation> {
        violations
            .iter()
            .filter(|violation| violation.code == "source_complete_census_uncovered")
            .collect()
    }

    // --- fgdb-complete-census-law-vacuous-twice-54jf ------------------------
    //
    // The law below was vacuous in two independent ways at once, and a vacuous
    // law is indistinguishable at every instrument from one that ran and
    // passed. Each red-proof here mutates THE INPUT THE VACUITY HID and asserts
    // the law goes red, and each is paired with a conformant control on the
    // same subject so that a test which cannot fail is not mistaken for one
    // that did.

    /// The REAL catalog and its REAL structural census, built exactly the way
    /// `verify_source` builds them, so a red-proof runs against the production
    /// subject rather than a fixture that agrees with itself.
    fn real_catalog_and_census() -> (Catalog, AppendixSourceCensus) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        let source = fs::read(root.join(PLAN_PATH)).expect("plan reads");
        let line_spans = source_line_spans(&source);
        let manifest = &catalog.source_manifest;
        let appendix = extract_lines(&source, &line_spans, manifest.start_line, manifest.end_line)
            .expect("appendix range");
        let mut out = Vec::new();
        let census =
            verify_structural_source_census(&catalog, appendix, &mut out).expect("census builds");
        assert!(
            out.is_empty(),
            "the real structural census must be clean before a red-proof runs on it, got {out:?}"
        );
        (catalog, census)
    }

    fn codes(violations: &[Violation]) -> Vec<&str> {
        violations
            .iter()
            .map(|violation| violation.code.as_str())
            .collect()
    }

    fn run_coverage(
        catalog: &Catalog,
        census: &AppendixSourceCensus,
        waivers: &[CompleteCensusDomainWaiver],
        certified: &[&str],
    ) -> Vec<Violation> {
        let mut violations = Vec::new();
        verify_complete_field_census_coverage_with(
            catalog,
            census,
            waivers,
            certified,
            &mut violations,
        );
        violations
    }

    fn with_complete_slice(catalog: &Catalog, slice_id: &str) -> Catalog {
        let mut forced = catalog.clone();
        let mut found = false;
        for slice in &mut forced.slices {
            if slice.id == slice_id {
                slice.definition_status = "complete".to_owned();
                found = true;
            }
        }
        assert!(found, "the red-proof subject slice {slice_id} must exist");
        forced
    }

    /// A licence for a vacuity is only a licence if it carries what makes it
    /// one: the measurement that says the domain is empty today and the exact
    /// change that retires the row. Without both it is an inherited excuse.
    #[test]
    fn empty_domain_licence_carries_its_evidence_and_its_repair() {
        assert_eq!(
            COMPLETE_CENSUS_DOMAIN_WAIVERS.len(),
            1,
            "exactly one licence while no slice has ever been completed"
        );
        for waiver in COMPLETE_CENSUS_DOMAIN_WAIVERS {
            assert!(
                waiver.evidence.contains("0 of 21"),
                "the licence must state the measured domain, got {:?}",
                waiver.evidence
            );
            assert!(
                !waiver.repair.is_empty(),
                "a licensed vacuity must name the change that retires it"
            );
            assert!(
                waiver.owning_bead.starts_with("fgdb-"),
                "the licence must name an owning bead, got {:?}",
                waiver.owning_bead
            );
        }
    }

    /// NARROWING 1, RED-PROOF. The input this vacuity hid is the empty domain
    /// itself: at HEAD the law evaluates 0 of 21 slices and reports the same
    /// zero a fully covered appendix would. Withdraw the licence and the real
    /// tree goes red.
    #[test]
    fn an_unlicensed_empty_domain_is_red_on_the_real_catalog() {
        let (catalog, census) = real_catalog_and_census();
        assert_eq!(
            catalog
                .slices
                .iter()
                .filter(|slice| slice.definition_status == "complete")
                .count(),
            0,
            "the premise of this red-proof is that the domain is empty at HEAD"
        );

        let unlicensed = run_coverage(&catalog, &census, &[], &[]);
        assert!(
            codes(&unlicensed).contains(&"source_complete_census_domain_vacuous"),
            "an unlicensed empty domain must be a violation, got {:?}",
            codes(&unlicensed)
        );

        // CONFORMANT CONTROL, same subject: the landed licence makes it green,
        // so the red above is the licence's absence and not a broken law.
        let licensed = run_coverage(&catalog, &census, COMPLETE_CENSUS_DOMAIN_WAIVERS, &[]);
        assert!(
            licensed.is_empty(),
            "the landed licence must leave the real tree clean, got {:?}",
            codes(&licensed)
        );
    }

    /// NARROWING 1, THE OTHER DIRECTION. A licence that outlives the state it
    /// licenses is how a zero goes back to meaning nothing, so completing any
    /// slice must retire it in the same change.
    #[test]
    fn completing_a_slice_retires_the_empty_domain_licence() {
        let (catalog, census) = real_catalog_and_census();
        let forced = with_complete_slice(&catalog, "a20");
        let violations = run_coverage(&forced, &census, COMPLETE_CENSUS_DOMAIN_WAIVERS, &["a20"]);
        assert!(
            codes(&violations).contains(&"source_complete_census_domain_waiver_stale"),
            "a non-empty domain must retire the licence, got {:?}",
            codes(&violations)
        );
    }

    /// NARROWING 2, RED-PROOF. The input this vacuity hid is a slice CLAIMING
    /// completeness over a universe that is census output rather than source.
    /// Marking a real slice complete must be red on the universe, and the
    /// coverage body must still run — a gate that swaps one law for another
    /// buys nothing.
    #[test]
    fn an_uncertified_census_universe_blocks_the_completeness_claim() {
        let (catalog, census) = real_catalog_and_census();
        let forced = with_complete_slice(&catalog, "a20");

        let uncertified = run_coverage(&forced, &census, &[], &[]);
        assert!(
            codes(&uncertified).contains(&"source_complete_census_universe_uncertified"),
            "an uncertified universe must block the claim, got {:?}",
            codes(&uncertified)
        );
        assert_eq!(
            uncovered_field_violations(&uncertified).len(),
            74,
            "the coverage law must still evaluate a20's 74 uncovered keys after fgdb-peyc \
             covers the BODY union, both arms, and all five arm-interior members"
        );

        // CONFORMANT CONTROL: certify the universe and only the universe code
        // disappears. The 74 stay, so the certification gates the CLAIM and
        // does not weaken the coverage law it guards.
        let certified = run_coverage(&forced, &census, &[], &["a20"]);
        assert!(
            !codes(&certified).contains(&"source_complete_census_universe_uncertified"),
            "certifying the universe must clear exactly that code, got {:?}",
            codes(&certified)
        );
        assert_eq!(
            uncovered_field_violations(&certified).len(),
            74,
            "certification must not change what the coverage law finds"
        );
    }

    /// The certification list is itself checked, so it cannot rot into a list
    /// of names that certify nothing.
    #[test]
    fn a_certification_naming_no_slice_is_stale() {
        let (catalog, census) = real_catalog_and_census();
        let violations = run_coverage(&catalog, &census, COMPLETE_CENSUS_DOMAIN_WAIVERS, &["a99"]);
        assert!(
            codes(&violations).contains(&"source_complete_census_certification_stale"),
            "a certification for an absent slice must be a violation, got {:?}",
            codes(&violations)
        );
    }

    /// The certified list is EMPTY at HEAD and that emptiness is load-bearing:
    /// `fgdb-qh3r` repaired two owner-bound unions but did not produce a
    /// source-complete measurement for any whole slice; `fgdb-ckb9` tracks the
    /// remaining known a03 ownership gap. If a slice is ever certified, this
    /// test is the one that forces the certifier to state the measurement.
    #[test]
    fn the_certified_universe_list_is_empty_without_a_source_complete_measurement() {
        assert!(
            CENSUS_UNIVERSE_CERTIFIED_SLICES.is_empty(),
            "certifying a census universe requires the measurement that its census emits every \
             member its source spells; remaining known gaps include fgdb-ckb9, got \
             {CENSUS_UNIVERSE_CERTIFIED_SLICES:?}"
        );
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
        // No `ambiguity_source_key` here: the pin no longer carries one, and this
        // fixture is the proof that it need not. `row_id` above IS
        // sha256(source_key) -- the fixture already satisfied the derivation law
        // before that law was made load-bearing here, so the match still binds
        // this pin to this key and nothing weakened.
        let pin = [AmbiguityAdjudicationContractPin {
            row_id,
            slice_id: "a20",
            source_locations: &["a20:2575"],
            resolution: "not-a-durable-schema",
            resolved_source_keys: &["top|Sharded"],
            // The digest, not the prose -- the same law the 450 landed pins
            // follow. This site is why the `--test` leg of the quiet-root
            // recipe is mandatory: the lib's own `#[cfg(test)]` module is
            // compiled by NO other leg, and this was the only construction in
            // the tree still building the pin with a `rationale` field. It was
            // caught by that leg and by nothing else (fgdb-n061).
            rationale_sha256: Box::leak(sha256_hex(rationale.as_bytes()).into_boxed_str()),
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
