//! Canonical Appendix A catalog, source verifier, and identity projections.
//!
//! The catalog is the one authoring surface.  Its typed projection rows are
//! parsed through the same strict models used by the six checked-in consumer
//! registries; deterministic rendering and byte comparison prevent those
//! projections from becoming independent authorities.

use crate::appendix_reference::{ReferenceTarget, census_plan_references};
use crate::appendix_source::{
    AppendixSourceCensus, FieldCandidate, SchemaCandidate, SchemaOwnerStatus, SourceSliceSpec,
    census_appendix_source,
};
use crate::hash::sha256_hex;
use crate::identity::{self, IdentityRegistries};
use crate::toml::{self, Table, Value};
use crate::{architecture, model};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path};

pub const CATALOG_SCHEMA_VERSION: i64 = 2;
pub const CATALOG_NAME: &str = "appendix_a_catalog";
pub const CATALOG_EPOCH: i64 = 2;
pub const ROW_ID_GRAMMAR_VERSION: i64 = 2;
pub const DIAGNOSTIC_VERSION: i64 = 1;
pub const CANONICAL_ORDER: &str =
    "source-key,projection-registry,assigned-code,containing-schema,field-tag,arm-tag,row-id";
pub const CATALOG_PATH: &str = "registries/appendix_a_catalog.toml";
pub const PLAN_PATH: &str = "COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md";
pub const SOURCE_ENCODING: &str = "utf-8-lf";
pub const HASH_ALGORITHM: &str = "sha256";

pub const APPENDIX_START_LINE: i64 = 1388;
pub const APPENDIX_END_LINE: i64 = 2728;
pub const APPENDIX_LINE_COUNT: i64 = 1341;
pub const APPENDIX_BYTE_COUNT: i64 = 1_020_717;
pub const APPENDIX_SHA256: &str =
    "71a48b67304f94568590f79c5b1c1ee4731819aee022c57fece78a7e72bce7f1";
pub const APPENDIX_HEADING: &str = "## Appendix A — On-Disk Object Formats (normative contract)";
pub const NEXT_HEADING: &str = "## Appendix B — Graph Intent Log (the semantic vocabulary)";
pub const EXPECTED_PROJECTION_ROW_COUNT: usize = 162;
pub const EXPECTED_PROJECTION_ROW_IDS_SHA256: &str =
    "df757bffcc05b0704b2296ad2725f35b2a016f79e380f6b67e809ed1ace1185b";
pub const EXPECTED_PROJECTION_FALLBACK_COUNT: usize = 81;
pub const EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256: &str =
    "fb328272b624b4f15339e8291feb2dfb22d6ecf6300df06d5173816c65115325";
pub const EXPECTED_ANNOTATION_COUNT: usize = 0;
pub const EXPECTED_ANNOTATION_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const EXPECTED_SEMANTIC_BINDING_COUNT: usize = 0;
pub const EXPECTED_SEMANTIC_BINDING_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const EXPECTED_EVIDENCE_BINDING_COUNT: usize = 0;
pub const EXPECTED_EVIDENCE_BINDING_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const EXPECTED_TYPE_RESERVATION_COUNT: usize = 813;
pub const EXPECTED_EXISTING_TYPE_RESERVATION_COUNT: usize = 15;
pub const EXPECTED_RESERVED_TYPE_RESERVATION_COUNT: usize = 798;
pub const EXPECTED_RESERVATION_HIGH_WATER: u16 = 0x051d;
pub const EXPECTED_RESERVATION_ASSIGNMENT_SHA256: &str =
    "a5b96b1dab40c38a3fc8e0f390564245f9a8b3285837cb3fde1bcf0003d44335";
pub const EXPECTED_REFERENCE_TARGET_IDS_SHA256: &str =
    "84276b6d97342e9ec1619424ddacb5b429e98e1862e03359afc837b65bb3392e";
pub const EXPECTED_REFERENCE_OCCURRENCE_COUNT: usize = 2_458;
pub const EXPECTED_REFERENCE_OCCURRENCE_SHA256: &str =
    "9878e84c7c72d0e098a66794ce56a00ffdfed62aaf251bc0d87efd665e0a630b";
pub const EXPECTED_G0_PROJECTION_ROW_COUNT: usize = 35;
pub const EXPECTED_G0_PROJECTION_ROW_IDS_SHA256: &str =
    "ff344794c0f061e83016f9f4844591a75d07bff597d439258d2b2632fc810d61";
pub const EXPECTED_SLICE_PROJECTION_CLASSES_SHA256: &str =
    "1bf2a60d904083bc19a196b6dc86c67f57c33009031460a5f7be2b32c10146fd";
pub const MAINTENANCE_PROOF_ROW_ID: &str = "catalog:maintenance-proof:appendix-a";
pub const MAINTENANCE_OWNER_BEAD: &str = "fgdb-appendix-a-catalog-scaffold-gvvf";
pub const MAINTENANCE_OWNER_CRATE: &str = "registry-check";

pub const APPENDIX_EVIDENCE_EVENT_IDS: [&str; 9] = [
    "appendix_closure_checked",
    "appendix_completed",
    "appendix_generation_completed",
    "appendix_projection_checked",
    "appendix_projection_generated",
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
    consumer_crates: &'static [&'static str],
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

// These independent, readable pins are deliberately empty while all A01-A21
// slices are declared. A slice may add completion metadata only by adding the
// exact reciprocal target/source/owner/evidence contract here in reviewed
// code; changing the opaque transcript digest alone is never authorization.
const SEMANTIC_BINDING_CONTRACT: [SemanticBindingContractPin; 0] = [];
const EVIDENCE_BINDING_CONTRACT: [EvidenceBindingContractPin; 0] = [];

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

const ROOT_KEYS: [&str; 23] = [
    "schema_version",
    "catalog",
    "source_manifest",
    "reference_manifest",
    "target_manifest",
    "maintenance_proof",
    "slice",
    "projection_epoch",
    "reservation",
    "logical_kind",
    "physical_kind",
    "bootstrap_frame",
    "prebootstrap_kind",
    "wire_type",
    "field",
    "reference_union",
    "reference_union_arm",
    "top_level_candidate",
    "target",
    "annotation",
    "semantic_binding",
    "evidence",
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
const SEMANTIC_BINDING_KEYS: [&str; 5] = [
    "row_id",
    "target_row_id",
    "owner_bead_id",
    "owner_crate",
    "consumer_crates",
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
    pub slices: Vec<Slice>,
    pub projection_epochs: BTreeMap<String, i64>,
    pub identity: IdentityRegistries,
    pub projection_rows: Vec<ProjectionRowMeta>,
    pub reservations: Vec<Reservation>,
    pub top_level_candidates: Vec<TopLevelCandidate>,
    pub targets: Vec<Target>,
    pub annotations: Vec<Annotation>,
    pub semantic_bindings: Vec<SemanticBinding>,
    pub evidence: Vec<EvidenceBinding>,
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
pub struct ProjectionRowMeta {
    pub projection: String,
    pub row_kind: String,
    pub slice_id: String,
    pub row_id: String,
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
    pub consumer_crates: Vec<String>,
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
        byte_count: 5_157,
        sha256: "f11543ab928994a39eeeee6e7154e75375e94f82a2e2b1640401d70c27a330d2",
    },
    SlicePin {
        ordinal: 3,
        id: "a03",
        bead_id: "fgdb-a03-local-state-txn-rxjg",
        title: "Appendix A exact catalog: Local logical state and durable transaction formats",
        start_line: 1464,
        end_line: 1543,
        line_count: 80,
        byte_count: 69_062,
        sha256: "9d761919535cdd27ad4783e5862ce119293973789e55db627ecc9d7bc0983db4",
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
        byte_count: 38_061,
        sha256: "18baef688b553fcb72987e546f95138b598f32a1389eeb72426a5f925684496e",
    },
    SlicePin {
        ordinal: 7,
        id: "a07",
        bead_id: "fgdb-a07-w12-txn-results-yt4z",
        title: "Appendix A exact catalog: W12 transaction, statement, result, and outcome formats",
        start_line: 1701,
        end_line: 1790,
        line_count: 90,
        byte_count: 87_339,
        sha256: "1b3a393d03ffbecec17b95f09be8b30067bd7803d1bc2e1176a83876a056f114",
    },
    SlicePin {
        ordinal: 8,
        id: "a08",
        bead_id: "fgdb-a08-w12-lifecycle-pr7j",
        title: "Appendix A exact catalog: W12 retention, compaction, reconfiguration, GC, and topology formats",
        start_line: 1791,
        end_line: 1889,
        line_count: 99,
        byte_count: 92_121,
        sha256: "9bee4c412d4ebb7df1d274528aa0b8e82033d1fb052db3b1a7e08bf4461f2481",
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
        byte_count: 16_579,
        sha256: "7bad384e377f49ef7d102eefda083260874790f6af47f85d0b344ea4c0854e9e",
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
        byte_count: 17_149,
        sha256: "1901c5bda19eb47aba710870dc0bd87c2184b4142f5ed51d6b8db732401031f8",
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
        byte_count: 79_596,
        sha256: "d68ac8d8b85c3bf56b836b34430d4fac668418d07f186cdcb01c6e4838ac828e",
    },
    SlicePin {
        ordinal: 16,
        id: "a16",
        bead_id: "fgdb-a16-time-authority-ytub",
        title: "Appendix A exact catalog: Rollback-protected authority-time formats and rotation",
        start_line: 2157,
        end_line: 2246,
        line_count: 90,
        byte_count: 69_768,
        sha256: "9fc751b96c0539ad956995a3e0bde2fe71e58fe4103aa80cbce9d1c687273427",
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
    let evidence = parse_evidence(&root, &mut violations);
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
        Some(evidence),
        Some(source_symbol_dispositions),
    ) = (
        reservations,
        top_level_candidates,
        targets,
        annotations,
        semantic_bindings,
        evidence,
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
        slices,
        projection_epochs,
        identity,
        projection_rows,
        reservations,
        top_level_candidates,
        targets,
        annotations,
        semantic_bindings,
        evidence,
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
    let bead_entries = match architecture::bead_provenance_index(&architecture, repo_root) {
        Ok(entries) => entries,
        Err(_) => {
            return vec![Violation::new(
                "catalog_repository_beads_unavailable",
                "repository_bindings",
                "cannot resolve the authoritative Beads index needed by Appendix metadata",
            )];
        }
    };
    let bead_ids: BTreeSet<&str> = bead_entries
        .iter()
        .map(|entry| entry.bead_id.as_str())
        .collect();
    let planned_crates: BTreeSet<&str> = architecture::PLANNED_CRATES.iter().copied().collect();
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
        if workspace_crates
            .as_ref()
            .is_some_and(|crates| !crates.contains(row.owner_crate.as_str()))
        {
            out.push(Violation::new(
                "catalog_semantic_owner_crate_unresolved",
                &row.row_id,
                "semantic owner_crate must resolve to an actual Cargo workspace package",
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
    validate_scenario_registry(repo_root, &checker_by_id, catalog, &mut out);
    validate_checker_bindings(
        repo_root,
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
            repo_root,
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
    let mut member_paths = Vec::new();
    for member in members {
        if let Some(parent) = member.strip_suffix("/*") {
            if !safe_repository_relative(parent) {
                return Err(format!("unsafe Cargo workspace member glob {member:?}"));
            }
            let mut children = fs::read_dir(repo_root.join(parent))
                .map_err(|error| format!("workspace member glob {member:?}: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("workspace member glob {member:?}: {error}"))?;
            children.sort_by_key(|entry| entry.file_name());
            member_paths.extend(
                children
                    .into_iter()
                    .filter(|child| child.path().join("Cargo.toml").is_file())
                    .map(|child| Path::new(parent).join(child.file_name())),
            );
        } else if safe_repository_relative(&member) {
            member_paths.push(Path::new(&member).to_owned());
        } else {
            return Err(format!("unsafe Cargo workspace member path {member:?}"));
        }
    }
    member_paths.sort();
    member_paths.dedup();

    let mut packages = BTreeSet::new();
    for member_path in member_paths {
        let manifest_path = repo_root.join(&member_path).join("Cargo.toml");
        let manifest_text = fs::read_to_string(&manifest_path)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        let manifest = toml::parse(&manifest_text)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        let package = toml::get_table(&manifest, "package", "workspace member Cargo.toml")
            .map_err(|error| error.to_string())?;
        let package_name = toml::get_str(package, "name", "workspace member Cargo.toml.package")
            .map_err(|error| error.to_string())?;
        if !packages.insert(package_name) {
            return Err("Cargo workspace contains duplicate package names".to_owned());
        }
    }
    Ok(packages)
}

fn safe_repository_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn live_checker_artifact_exists(repo_root: &Path, checker: &model::Checker) -> bool {
    safe_repository_relative(&checker.artifact) && repo_root.join(&checker.artifact).is_file()
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
    repo_root: &Path,
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
                checker.status == "live" && !live_checker_artifact_exists(repo_root, checker)
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
    repo_root: &Path,
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
                if evidence_status == "live"
                    && !live_checker_artifact_exists(repo_root, checker) =>
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
        append_contract_array(&mut transcript, &row.consumer_crates);
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
    verify_annotation_source_contracts(catalog, &census, out);
    Some(census)
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

    for annotation in &catalog.annotations {
        let Some(target) = target_by_projection
            .get(annotation.target_row_id.as_str())
            .copied()
        else {
            continue;
        };
        if let Some(field) = field_by_source_key.get(&target.source_key).copied() {
            let source_is_exact = !field.ambiguous
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
                top_level_annotation_expansions_match(annotation, schema, &census.schemas);
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
    annotation: &Annotation,
    schema: &SchemaCandidate,
    schemas: &[SchemaCandidate],
) -> bool {
    let formals = generic_formals_from_signature(&schema.key.generic_signature);
    let requires_role_expansions = formals.contains("Role");
    let requires_generic_expansions = formals.iter().any(|formal| formal != "Role");
    let (expected_role_expansions, expected_generic_expansions) =
        source_concrete_expansion_sets(schema, schemas);
    let actual_role_expansions: BTreeSet<&str> = annotation
        .role_expansions
        .iter()
        .map(String::as_str)
        .collect();
    let actual_generic_expansions: BTreeSet<&str> = annotation
        .generic_expansions
        .iter()
        .map(String::as_str)
        .collect();

    expansion_set_matches(
        &annotation.role_expansions,
        &actual_role_expansions,
        requires_role_expansions,
        &expected_role_expansions,
    ) && expansion_set_matches(
        &annotation.generic_expansions,
        &actual_generic_expansions,
        requires_generic_expansions,
        &expected_generic_expansions,
    )
}

fn expansion_set_matches(
    actual: &[String],
    actual_set: &BTreeSet<&str>,
    required: bool,
    expected: &BTreeSet<String>,
) -> bool {
    if !required {
        return actual.is_empty();
    }
    if actual.is_empty() || actual.len() != actual_set.len() {
        return false;
    }
    expected.is_empty()
        || (actual.len() == expected.len()
            && actual_set
                .iter()
                .copied()
                .eq(expected.iter().map(String::as_str)))
}

fn source_concrete_expansion_sets(
    schema: &SchemaCandidate,
    schemas: &[SchemaCandidate],
) -> (BTreeSet<String>, BTreeSet<String>) {
    let Some(formal_parameters) = generic_signature_parameters(&schema.key.generic_signature)
    else {
        return (BTreeSet::new(), BTreeSet::new());
    };
    let mut role_expansions = BTreeSet::new();
    let mut generic_expansions = BTreeSet::new();

    for (index, formal_parameter) in formal_parameters.iter().enumerate() {
        let Some(formal) = generic_parameter_formal(formal_parameter) else {
            continue;
        };
        if let Some((_, bound)) = formal_parameter.split_once(':')
            && bound.contains('|')
            && let Some(values) = concrete_parameter_alternatives(bound)
        {
            expansion_set_for_formal(formal, &mut role_expansions, &mut generic_expansions)
                .extend(values);
        }
        for candidate in schemas
            .iter()
            .filter(|candidate| candidate.key.family == schema.key.family)
        {
            let Some(candidate_parameters) =
                generic_signature_parameters(&candidate.key.generic_signature)
            else {
                continue;
            };
            if candidate_parameters.len() != formal_parameters.len() {
                continue;
            }
            let Some(values) = candidate_parameters
                .get(index)
                .and_then(|parameter| concrete_parameter_alternatives(parameter))
            else {
                continue;
            };
            expansion_set_for_formal(formal, &mut role_expansions, &mut generic_expansions)
                .extend(values);
        }
    }
    (role_expansions, generic_expansions)
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
    const KNOWN_FORMALS: [&str; 9] = [
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
    let formal = parameter
        .split_once(':')
        .map_or(parameter.trim(), |(formal, _)| formal.trim());
    KNOWN_FORMALS.contains(&formal).then_some(formal)
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
        Some((fields_epoch, fields, unions)),
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
            metadata.push(ProjectionRowMeta {
                projection: registry_name.to_owned(),
                row_kind: row_kind.to_owned(),
                slice_id,
                row_id,
                canonical_symbol: identity.map_or_else(String::new, |(_, symbol)| symbol),
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
            read_string_array(table, "consumer_crates", &context, violations),
        );
        if let (
            Some(row_id),
            Some(target_row_id),
            Some(owner_bead_id),
            Some(owner_crate),
            Some(consumer_crates),
        ) = values
        {
            rows.push(SemanticBinding {
                row_id,
                target_row_id,
                owner_bead_id,
                owner_crate,
                consumer_crates,
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
    validate_readable_binding_contract(catalog, out);
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
        && row
            .consumer_crates
            .iter()
            .map(String::as_str)
            .eq(pin.consumer_crates.iter().copied())
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
        let expected_row_id = format!(
            "{}:{}:{}",
            row.slice_id,
            row.row_kind,
            lower_kebab(&row.canonical_symbol)
        );
        if row.canonical_symbol.trim().is_empty() || row.row_id != expected_row_id {
            out.push(Violation::new(
                "catalog_row_id_derived_mismatch",
                &row.row_id,
                format!(
                    "projection row_id must derive from canonical symbol {:?}; expected {expected_row_id:?}",
                    row.canonical_symbol
                ),
            ));
        }
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

fn validate_catalog_metadata(catalog: &Catalog, out: &mut Vec<Violation>) {
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
            validate_target_source_identity(
                row,
                projection,
                candidate_by_key.get(row.source_key.as_str()).copied(),
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

    let ApprovedBindingCounts {
        semantic: binding_counts,
        static_live: static_live_counts,
        runtime: runtime_counts,
    } = approved_binding_counts(catalog);
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
    }

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
        if slice_targets.is_empty() {
            out.push(Violation::new(
                "complete_slice_target_missing",
                &slice.id,
                "complete slice has no source-backed targets",
            ));
        }
        let top_keys: Vec<&str> = slice_targets
            .iter()
            .filter(|row| row.source_key.starts_with("top|"))
            .map(|row| row.source_key.as_str())
            .collect();
        let field_keys: Vec<&str> = slice_targets
            .iter()
            .filter(|row| row.source_key.starts_with("field|"))
            .map(|row| row.source_key.as_str())
            .collect();
        let union_keys: Vec<&str> = slice_targets
            .iter()
            .filter(|row| row.source_key.starts_with("union|"))
            .map(|row| row.source_key.as_str())
            .collect();
        let arm_keys: Vec<&str> = slice_targets
            .iter()
            .filter(|row| row.source_key.starts_with("arm|"))
            .map(|row| row.source_key.as_str())
            .collect();
        validate_census_pin(
            &slice.id,
            "complete_top_level",
            slice.top_level_candidate_count,
            &slice.top_level_candidate_ids_sha256,
            top_keys,
            out,
        );
        validate_census_pin(
            &slice.id,
            "complete_field",
            slice.field_candidate_count,
            &slice.field_candidate_ids_sha256,
            field_keys,
            out,
        );
        validate_census_pin(
            &slice.id,
            "complete_union",
            slice.union_candidate_count,
            &slice.union_candidate_ids_sha256,
            union_keys,
            out,
        );
        validate_census_pin(
            &slice.id,
            "complete_arm",
            slice.arm_candidate_count,
            &slice.arm_candidate_ids_sha256,
            arm_keys,
            out,
        );
        if slice.ambiguity_count != 0 {
            out.push(Violation::new(
                "complete_slice_ambiguity",
                &slice.id,
                "complete slice must resolve every source-census ambiguity",
            ));
        }
        for row in &slice_targets {
            if row.definition_status != "complete" {
                out.push(Violation::new(
                    "complete_slice_target_declared",
                    &row.row_id,
                    "complete slice contains a target that is still declared",
                ));
            }
            if !row.source_key.starts_with("top|") && !row.source_key.starts_with("field|") {
                out.push(Violation::new(
                    "complete_slice_source_contract_unverified",
                    &row.target_row_id,
                    "complete target requires a source-reconciled top-level or field contract; union, arm, reference-only, and projection fallback targets remain declared until their exact source checker exists",
                ));
            }
            let annotation_count = annotation_counts
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

fn validate_target_source_identity(
    row: &Target,
    projection: &ProjectionRowMeta,
    top_candidate: Option<&TopLevelCandidate>,
    out: &mut Vec<Violation>,
) {
    let projection_source_key = format!(
        "projection|{}|{}",
        projection.projection, projection.canonical_symbol
    );
    if row.source_key == projection_source_key {
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
        "field" => {
            let mut parts = row.source_key.split('|');
            let source_matches = parts.next() == Some("field")
                && parts.next().zip(parts.next()).is_some_and(|(schema, _path)| {
                    parts.next().is_some_and(|stable_name| {
                        parts.next().is_none()
                            && projection.canonical_symbol == format!("{schema}.{stable_name}")
                    })
                });
            if !source_matches {
                out.push(Violation::new(
                    "catalog_target_source_identity_mismatch",
                    &row.row_id,
                    "durable-field projection must map to the same source schema and stable field name",
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
    const EVENTS: [&str; 3] = [
        "appendix_closure_checked",
        "appendix_projection_checked",
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
            "maintenance proof must exactly bind the scaffold owner, seven checked-in artifacts, three live checkers, G0 scenario/events, and G0",
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
    {
        out.push(Violation::new(
            "catalog_semantic_owner_invalid",
            &row.row_id,
            "semantic owner must be a non-maintenance implementation Bead and crate",
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
        return generic_formals_from_signature(&candidate.generic_signature);
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

fn generic_formals_from_signature(signature: &str) -> BTreeSet<String> {
    const KNOWN_FORMALS: [&str; 9] = [
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
        if !formal.contains('|')
            && (has_bound || KNOWN_FORMALS.contains(&formal))
            && !formal.is_empty()
            && formal
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
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

fn registered_reference_definition_semantics(family: &str) -> Option<&'static str> {
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

    const TARGET_ROW_ID: &str = "a01:bootstrap-frame:root-slot";
    const TARGET_SOURCE_KEY: &str = "top|RootSlot";

    fn catalog_with_bindings() -> Catalog {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut catalog = load_catalog_file(&root.join(CATALOG_PATH)).expect("catalog loads");
        catalog.semantic_bindings.push(SemanticBinding {
            row_id: "a01:semantic-binding:bootstrap-frame-root-slot".to_owned(),
            target_row_id: TARGET_ROW_ID.to_owned(),
            owner_bead_id: "fgdb-w2-owner-fixture".to_owned(),
            owner_crate: "fgdb-chronicle".to_owned(),
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

    const fn semantic_pin() -> SemanticBindingContractPin {
        SemanticBindingContractPin {
            row_id: "a01:semantic-binding:bootstrap-frame-root-slot",
            target_row_id: TARGET_ROW_ID,
            target_source_key: TARGET_SOURCE_KEY,
            owner_bead_id: "fgdb-w2-owner-fixture",
            owner_crate: "fgdb-chronicle",
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
        validate_scenario_registry(
            &root_without_artifacts,
            &checker_by_id,
            &catalog,
            &mut violations,
        );
        assert!(
            violations
                .iter()
                .any(|violation| { violation.code == "catalog_scenario_checker_artifact_missing" }),
            "a live scenario checker with no artifact was accepted: {violations:?}"
        );

        let checker_ids = vec!["appendix_a_catalog_closure".to_owned()];
        let mut violations = Vec::new();
        validate_checker_bindings(
            &root_without_artifacts,
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
}
