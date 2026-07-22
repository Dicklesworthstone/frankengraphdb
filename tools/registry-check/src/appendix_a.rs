//! Canonical Appendix A catalog, source verifier, and identity projections.
//!
//! The catalog is the one authoring surface.  Its typed projection rows are
//! parsed through the same strict models used by the six checked-in consumer
//! registries; deterministic rendering and byte comparison prevent those
//! projections from becoming independent authorities.

use crate::hash::sha256_hex;
use crate::identity::{self, IdentityRegistries};
use crate::toml::{self, Table, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

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

pub const APPENDIX_START_LINE: i64 = 1384;
pub const APPENDIX_END_LINE: i64 = 2654;
pub const APPENDIX_LINE_COUNT: i64 = 1271;
pub const APPENDIX_BYTE_COUNT: i64 = 950_365;
pub const APPENDIX_SHA256: &str =
    "db855855638b3df0001ccd2a7f0801943bb33ed59291421497149f71196201ff";
pub const APPENDIX_HEADING: &str = "## Appendix A — On-Disk Object Formats (normative contract)";
pub const NEXT_HEADING: &str = "## Appendix B — Graph Intent Log (the semantic vocabulary)";
pub const EXPECTED_PROJECTION_ROW_COUNT: usize = 128;
pub const EXPECTED_PROJECTION_ROW_IDS_SHA256: &str =
    "6b848d7420a156e55f05618ac350f1ff551f4cbb34271678bb5798b957edfc09";
pub const EXPECTED_TYPE_RESERVATION_COUNT: usize = 716;
pub const EXPECTED_EXISTING_TYPE_RESERVATION_COUNT: usize = 14;
pub const EXPECTED_RESERVED_TYPE_RESERVATION_COUNT: usize = 702;
pub const EXPECTED_RESERVATION_HIGH_WATER: u16 = 0x04bd;
pub const EXPECTED_RESERVATION_ASSIGNMENT_SHA256: &str =
    "44f11e32e50ca5bc2d6174f9b407211e70a1c46e3f44a8eb9f4a5b0b38e663d2";
pub const EXPECTED_SOURCE_FAMILY_SHA256: &str =
    "1582c908562899402a96d30e10af6ebf359d31ec0368747a272b181ec8d40e08";
pub const EXPECTED_SOURCE_LOCATION_PAIR_COUNT: usize = 1_724;
pub const EXPECTED_SOURCE_LOCATION_SHA256: &str =
    "c6c82e080dc495e27b21c1582fa24504d842a41d301ea2da3197bb09bf96b3c0";
pub const EXPECTED_DEFINED_SOURCE_FAMILY_COUNT: usize = 458;
pub const EXPECTED_EXTERNAL_SOURCE_FAMILY_COUNT: usize = 257;
pub const EXPECTED_DEFINED_SOURCE_FAMILY_SHA256: &str =
    "420bf64c5fedd5a33f23d1fed0199da68590c9c78bdb37805e44d689c63c84cd";
pub const EXPECTED_DEFINITION_LOCATION_SHA256: &str =
    "f5dd3106ed7570fe538e56b9b357465c0a996b9e76fdf21cfce59b349139cd55";
pub const EXPECTED_EXTERNAL_SOURCE_FAMILY_SHA256: &str =
    "d388bc9f2a8006e8c19f280254bfd14db119de428d236cdc90685a91dc0ba302";
pub const EXPECTED_REFERENCE_ONLY_SOURCE_FAMILY_SHA256: &str =
    "bffbdf3d10a4ac39029a9807aa3c81df1c586f7de93accc3e549324e038c2268";
pub const EXPECTED_SOURCE_CENSUS_TRANSCRIPT_SHA256: &str =
    "15f86ef2816b4b770f8d4b3f896c5c0d1f08736f073abe65374dde11466bae35";
pub const EXPECTED_SCAFFOLD_METADATA_BYTE_COUNT: usize = 845_375;
pub const EXPECTED_SCAFFOLD_METADATA_SHA256: &str =
    "02e3a35443b9ef0d80bc67a66682eb5b4aff5229486381bdfa5f45300f0d99f4";
pub const EXPECTED_G0_PROJECTION_ROW_COUNT: usize = 35;
pub const EXPECTED_G0_PROJECTION_ROW_IDS_SHA256: &str =
    "ff344794c0f061e83016f9f4844591a75d07bff597d439258d2b2632fc810d61";
pub const EXPECTED_SLICE_PROJECTION_CLASSES_SHA256: &str =
    "1bf2a60d904083bc19a196b6dc86c67f57c33009031460a5f7be2b32c10146fd";
pub const MAINTENANCE_PROOF_ROW_ID: &str = "catalog:maintenance-proof:appendix-a";
pub const MAINTENANCE_OWNER_BEAD: &str = "fgdb-appendix-a-catalog-scaffold-gvvf";
pub const MAINTENANCE_OWNER_CRATE: &str = "registry-check";

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

const ROOT_KEYS: [&str; 22] = [
    "schema_version",
    "catalog",
    "source_manifest",
    "reference_manifest",
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
pub struct StrongRefCensus {
    pub families: BTreeMap<String, Vec<String>>,
    pub family_sha256: String,
    pub location_pair_count: usize,
    pub location_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionCensus {
    pub first_locations: BTreeMap<String, String>,
    pub family_sha256: String,
    pub location_sha256: String,
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
        start_line: 1384,
        end_line: 1440,
        line_count: 57,
        byte_count: 23_172,
        sha256: "102b572835f29cfa6b8ec5d22a5a2ef9a9c9cd8d0998f4136a914b031812b25b",
    },
    SlicePin {
        ordinal: 2,
        id: "a02",
        bead_id: "fgdb-a02-filesystem-cipher-dsi3",
        title: "Appendix A exact catalog: Filesystem, cipher, encoding, placement, and symbols",
        start_line: 1441,
        end_line: 1459,
        line_count: 19,
        byte_count: 5_157,
        sha256: "f11543ab928994a39eeeee6e7154e75375e94f82a2e2b1640401d70c27a330d2",
    },
    SlicePin {
        ordinal: 3,
        id: "a03",
        bead_id: "fgdb-a03-local-state-txn-rxjg",
        title: "Appendix A exact catalog: Local logical state and durable transaction formats",
        start_line: 1460,
        end_line: 1537,
        line_count: 78,
        byte_count: 66_780,
        sha256: "c7602e569223eed2b230de305ed40f16f11f52670f97b2e5ca94fef3bf4824e6",
    },
    SlicePin {
        ordinal: 4,
        id: "a04",
        bead_id: "fgdb-a04-manifest-raft-4tgi",
        title: "Appendix A exact catalog: RootManifest, configuration, Raft, and cross-group trust prelude",
        start_line: 1538,
        end_line: 1583,
        line_count: 46,
        byte_count: 30_907,
        sha256: "ecd43f46a9ffd2be372922bf81bf589ab625778eef10d5d13828aa5939b37c2d",
    },
    SlicePin {
        ordinal: 5,
        id: "a05",
        bead_id: "fgdb-a05-w12-role-transition-wjj2",
        title: "Appendix A exact catalog: W12 Genesis, role transition, and activation formats",
        start_line: 1584,
        end_line: 1652,
        line_count: 69,
        byte_count: 59_030,
        sha256: "8f11565a0d29d115bbd4919b3ccbd808a2b0319ebf635c380b599a7010c8856d",
    },
    SlicePin {
        ordinal: 6,
        id: "a06",
        bead_id: "fgdb-a06-w12-core-zdzx",
        title: "Appendix A exact catalog: W12 Meta and Shard semantic core formats",
        start_line: 1653,
        end_line: 1694,
        line_count: 42,
        byte_count: 38_061,
        sha256: "18baef688b553fcb72987e546f95138b598f32a1389eeb72426a5f925684496e",
    },
    SlicePin {
        ordinal: 7,
        id: "a07",
        bead_id: "fgdb-a07-w12-txn-results-yt4z",
        title: "Appendix A exact catalog: W12 transaction, statement, result, and outcome formats",
        start_line: 1695,
        end_line: 1784,
        line_count: 90,
        byte_count: 86_794,
        sha256: "386e56f68638c6054ebc7cc30f9c33c0a708f88fb100eab6b21cc425f5b6056a",
    },
    SlicePin {
        ordinal: 8,
        id: "a08",
        bead_id: "fgdb-a08-w12-lifecycle-pr7j",
        title: "Appendix A exact catalog: W12 retention, compaction, reconfiguration, GC, and topology formats",
        start_line: 1785,
        end_line: 1883,
        line_count: 99,
        byte_count: 92_121,
        sha256: "9bee4c412d4ebb7df1d274528aa0b8e82033d1fb052db3b1a7e08bf4461f2481",
    },
    SlicePin {
        ordinal: 9,
        id: "a09",
        bead_id: "fgdb-a09-storage-identity-02tl",
        title: "Appendix A exact catalog: Strata run, identity continuity, allocator, and lease formats",
        start_line: 1884,
        end_line: 1903,
        line_count: 20,
        byte_count: 12_328,
        sha256: "eea5d9f7257bfefee5cae1077bbe3f17d4948267736dcd79e24d530f2a1873df",
    },
    SlicePin {
        ordinal: 10,
        id: "a10",
        bead_id: "fgdb-a10-command-delta-ooy1",
        title: "Appendix A exact catalog: Committed effects, commands, and logical delta formats",
        start_line: 1904,
        end_line: 1925,
        line_count: 22,
        byte_count: 16_579,
        sha256: "7bad384e377f49ef7d102eefda083260874790f6af47f85d0b344ea4c0854e9e",
    },
    SlicePin {
        ordinal: 11,
        id: "a11",
        bead_id: "fgdb-a11-delivery-markers-sdh6",
        title: "Appendix A exact catalog: Delivery cursors, envelopes, markers, and physical batching",
        start_line: 1926,
        end_line: 1957,
        line_count: 32,
        byte_count: 7_290,
        sha256: "19d0983f080d27922bf566a2973504bf2b60fc75b3902ecbe0e1c8396015bdaf",
    },
    SlicePin {
        ordinal: 12,
        id: "a12",
        bead_id: "fgdb-a12-checkpoint-resources-m9jz",
        title: "Appendix A exact catalog: Checkpoint, retention, constraint, and resource formats",
        start_line: 1958,
        end_line: 1993,
        line_count: 36,
        byte_count: 19_488,
        sha256: "1d9f07d6ccc7c5feb548224d9e5f38ef216143c1dfd63f95ebcf6e84907b76c6",
    },
    SlicePin {
        ordinal: 13,
        id: "a13",
        bead_id: "fgdb-a13-branch-merge-g2ko",
        title: "Appendix A exact catalog: Branch manifest, key grants, retirement, and merge formats",
        start_line: 1994,
        end_line: 2028,
        line_count: 35,
        byte_count: 17_149,
        sha256: "1901c5bda19eb47aba710870dc0bd87c2184b4142f5ed51d6b8db732401031f8",
    },
    SlicePin {
        ordinal: 14,
        id: "a14",
        bead_id: "fgdb-a14-ha-payload-gc-jb82",
        title: "Appendix A exact catalog: Payload availability, configuration floors, and GC epoch formats",
        start_line: 2029,
        end_line: 2050,
        line_count: 22,
        byte_count: 17_540,
        sha256: "de90db8cd87f7b9c4b168ed9357580ffb8e9c64f60ef643ba48f872daf566e93",
    },
    SlicePin {
        ordinal: 15,
        id: "a15",
        bead_id: "fgdb-a15-key-backup-n77c",
        title: "Appendix A exact catalog: Key destruction, backup, publication, and release formats",
        start_line: 2051,
        end_line: 2150,
        line_count: 100,
        byte_count: 79_596,
        sha256: "d68ac8d8b85c3bf56b836b34430d4fac668418d07f186cdcb01c6e4838ac828e",
    },
    SlicePin {
        ordinal: 16,
        id: "a16",
        bead_id: "fgdb-a16-time-authority-ytub",
        title: "Appendix A exact catalog: Rollback-protected authority-time formats and rotation",
        start_line: 2151,
        end_line: 2240,
        line_count: 90,
        byte_count: 69_768,
        sha256: "9fc751b96c0539ad956995a3e0bde2fe71e58fe4103aa80cbce9d1c687273427",
    },
    SlicePin {
        ordinal: 17,
        id: "a17",
        bead_id: "fgdb-a17-restore-prebootstrap-hy9w",
        title: "Appendix A exact catalog: Restore prebootstrap journal and source acquisition formats",
        start_line: 2241,
        end_line: 2342,
        line_count: 102,
        byte_count: 72_597,
        sha256: "660aaee44fbc117b6f49156c9f95ec3e1843d9ae171e54f4e08daf435c456cd5",
    },
    SlicePin {
        ordinal: 18,
        id: "a18",
        bead_id: "fgdb-a18-restore-registry-exjt",
        title: "Appendix A exact catalog: Restore registry, cleanup, terminal history, and abandonment formats",
        start_line: 2343,
        end_line: 2452,
        line_count: 110,
        byte_count: 94_976,
        sha256: "5fc84607d338774d06f4dcd1a0aed6f48165f427c7da4716b90d4bbbc949161c",
    },
    SlicePin {
        ordinal: 19,
        id: "a19",
        bead_id: "fgdb-a19-restore-readiness-fd0j",
        title: "Appendix A exact catalog: Restore lease barrier, reservations, bridge, and readiness formats",
        start_line: 2453,
        end_line: 2568,
        line_count: 116,
        byte_count: 77_017,
        sha256: "65c5015fa2243b33579d5b1b6d78ac3e0d55f9b0fcc10c482bbc02a2ebd4d9c0",
    },
    SlicePin {
        ordinal: 20,
        id: "a20",
        bead_id: "fgdb-a20-restore-promotion-ivsp",
        title: "Appendix A exact catalog: Restore promotion, independent reopen, completion, and release formats",
        start_line: 2569,
        end_line: 2602,
        line_count: 34,
        byte_count: 22_805,
        sha256: "6f1b942c046041d3ecefb159e0e86b30a673f03ca86b44dac921ad98ef07a064",
    },
    SlicePin {
        ordinal: 21,
        id: "a21",
        bead_id: "fgdb-a21-replay-security-ye0o",
        title: "Appendix A exact catalog: Replay, authorization, capability, DP, audit, and transparency formats",
        start_line: 2603,
        end_line: 2654,
        line_count: 52,
        byte_count: 41_210,
        sha256: "8149f67df01ff6f93810e9e419faef9f890710a524dcdda0580ae352561fcaa0",
    },
];

/// Parse one canonical catalog from the repository's strict TOML subset.
pub fn parse_catalog(text: &str) -> Result<Catalog, Vec<Violation>> {
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
    parse_catalog(text)
}

/// Load the canonical repository catalog and verify its pinned plan source.
pub fn load_and_verify(repo_root: &Path) -> Result<Catalog, Vec<Violation>> {
    let catalog = load_catalog_file(&repo_root.join(CATALOG_PATH))?;
    let source_path = repo_root.join(&catalog.source_manifest.plan_path);
    let source = fs::read(&source_path).map_err(|error| {
        vec![Violation::new(
            "source_read",
            "source_manifest",
            format!("cannot read {}: {error}", source_path.display()),
        )]
    })?;
    let violations = appendix_a_catalog_source(&catalog, &source);
    if violations.is_empty() {
        Ok(catalog)
    } else {
        Err(violations)
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

pub fn generated_scaffold_metadata(
    catalog: &Catalog,
    source: &[u8],
) -> Result<String, Vec<Violation>> {
    let strong_refs = strong_ref_census(source);
    let definitions = definition_census(source, &strong_refs);
    let mut violations = Vec::new();
    if strong_refs.families.len() != EXPECTED_TYPE_RESERVATION_COUNT
        || strong_refs.family_sha256 != EXPECTED_SOURCE_FAMILY_SHA256
        || strong_refs.location_pair_count != EXPECTED_SOURCE_LOCATION_PAIR_COUNT
        || strong_refs.location_sha256 != EXPECTED_SOURCE_LOCATION_SHA256
    {
        violations.push(Violation::new(
            "source_family_census_drift",
            "source_manifest",
            "cannot generate metadata from a source whose StrongRef census differs from the release pins",
        ));
    }
    if definitions.first_locations.len() != EXPECTED_DEFINED_SOURCE_FAMILY_COUNT
        || definitions.family_sha256 != EXPECTED_DEFINED_SOURCE_FAMILY_SHA256
        || definitions.location_sha256 != EXPECTED_DEFINITION_LOCATION_SHA256
    {
        violations.push(Violation::new(
            "source_definition_census_drift",
            "source_manifest",
            "cannot generate metadata from a source whose definition census differs from the release pins",
        ));
    }
    if !violations.is_empty() {
        return Err(violations);
    }

    let logical_codes: BTreeMap<&str, u16> = catalog
        .identity
        .logical
        .iter()
        .filter_map(|row| {
            u16::try_from(row.object_kind)
                .ok()
                .map(|code| (row.name.as_str(), code))
        })
        .collect();
    let reserved_codes =
        stable_reservation_codes(&catalog.reservations, &strong_refs.families, &logical_codes)?;

    let mut owner_by_family = BTreeMap::new();
    for (family, locations) in &strong_refs.families {
        let location = definitions
            .first_locations
            .get(family)
            .or_else(|| locations.first())
            .ok_or_else(|| {
                vec![Violation::new(
                    "catalog_reservation_owner_missing",
                    family,
                    "family has neither a definition nor a reference location",
                )]
            })?;
        let scope = location
            .split_once(':')
            .map(|parts| parts.0)
            .ok_or_else(|| {
                vec![Violation::new(
                    "catalog_source_location_invalid",
                    family,
                    format!("cannot derive owner slice from {location:?}"),
                )]
            })?;
        owner_by_family.insert(family.as_str(), scope);
    }

    let mut out = String::new();
    writeln!(
        &mut out,
        "\n# ============================================================================="
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# Generated G0 Appendix-A reference-target reservation census."
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# StrongRef families: {EXPECTED_TYPE_RESERVATION_COUNT}; family sha256: {EXPECTED_SOURCE_FAMILY_SHA256}"
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# Definition classifier: {EXPECTED_DEFINED_SOURCE_FAMILY_COUNT} Appendix definitions; sha256: {EXPECTED_DEFINED_SOURCE_FAMILY_SHA256}"
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# ============================================================================="
    )
    .expect("writing to String cannot fail");

    for slice in &catalog.slices {
        writeln!(
            &mut out,
            "\n# -----------------------------------------------------------------------------"
        )
        .expect("writing to String cannot fail");
        writeln!(
            &mut out,
            "# {} — {}",
            slice.id.to_ascii_uppercase(),
            slice.title
        )
        .expect("writing to String cannot fail");
        writeln!(
            &mut out,
            "# -----------------------------------------------------------------------------"
        )
        .expect("writing to String cannot fail");

        for (family, locations) in strong_refs
            .families
            .iter()
            .filter(|(family, _)| owner_by_family.get(family.as_str()) == Some(&slice.id.as_str()))
        {
            let Some(code) = logical_codes
                .get(family.as_str())
                .copied()
                .or_else(|| reserved_codes.get(family).copied())
            else {
                return Err(vec![Violation::new(
                    "catalog_reservation_allocation_missing",
                    family,
                    "type family has neither a released logical code nor a preserved reservation",
                )]);
            };
            let reservation_disposition = if logical_codes.contains_key(family.as_str()) {
                "existing"
            } else {
                "reserved"
            };
            write_reservation_row(&mut out, &slice.id, family, code, reservation_disposition);
            let source_disposition = if definitions.first_locations.contains_key(family) {
                "appendix-structural-definition"
            } else {
                "reference-only"
            };
            write_source_disposition_row(
                &mut out,
                &slice.id,
                family,
                source_disposition,
                locations,
                None,
            )?;
        }
    }

    writeln!(
        &mut out,
        "\n# -----------------------------------------------------------------------------"
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# G0 — released identity rows defined outside the pinned Appendix slice"
    )
    .expect("writing to String cannot fail");
    writeln!(
        &mut out,
        "# -----------------------------------------------------------------------------"
    )
    .expect("writing to String cannot fail");
    let mut g0_rows: Vec<&ProjectionRowMeta> = catalog
        .projection_rows
        .iter()
        .filter(|row| row.slice_id == "g0")
        .collect();
    g0_rows.sort_by_key(|row| row.row_id.as_str());
    for row in &g0_rows {
        let Some(file) = PROJECTION_FILES
            .iter()
            .find(|(registry, _)| *registry == row.projection)
            .map(|(_, file)| format!("registries/{file}"))
        else {
            return Err(vec![Violation::new(
                "catalog_projection_registry_unknown",
                &row.row_id,
                "projection row names an unknown registry",
            )]);
        };
        write_source_disposition_row(
            &mut out,
            "g0",
            &row.canonical_symbol,
            "projection-source",
            &[file],
            Some(&row.row_id),
        )?;
    }
    Ok(out)
}

fn stable_reservation_codes(
    reservations: &[Reservation],
    families: &BTreeMap<String, Vec<String>>,
    logical_codes: &BTreeMap<&str, u16>,
) -> Result<BTreeMap<String, u16>, Vec<Violation>> {
    let mut existing_by_symbol = BTreeMap::new();
    let mut used_codes = BTreeMap::new();
    let mut violations = Vec::new();

    for row in reservations {
        let Some(code) = parse_code_reservation(&row.code_reservation) else {
            violations.push(Violation::new(
                "catalog_reservation_code_invalid",
                &row.row_id,
                "cannot preserve an invalid released reservation code",
            ));
            continue;
        };
        if existing_by_symbol
            .insert(row.symbol.clone(), code)
            .is_some()
        {
            violations.push(Violation::new(
                "catalog_reservation_duplicate",
                &row.row_id,
                "cannot allocate from duplicate released reservation symbols",
            ));
        }
        if let Some(previous) = used_codes.insert(code, row.symbol.as_str()) {
            violations.push(Violation::new(
                "catalog_reservation_code_duplicate",
                &row.row_id,
                format!("released reservation code collides with {previous:?}"),
            ));
        }
    }
    for (symbol, code) in logical_codes {
        if let Some(previous) = used_codes.insert(*code, *symbol) {
            if previous != *symbol {
                violations.push(Violation::new(
                    "catalog_reservation_code_collision",
                    *symbol,
                    format!("logical assignment collides with {previous:?}"),
                ));
            }
        }
        if let Some(reserved) = existing_by_symbol.get(*symbol)
            && reserved != code
        {
            violations.push(Violation::new(
                "catalog_reservation_promotion_drift",
                *symbol,
                "a promoted logical row must retain its released reservation code",
            ));
        }
    }
    if !violations.is_empty() {
        return Err(violations);
    }

    let mut next_code = used_codes
        .keys()
        .copied()
        .filter(|code| (0x0200..=0x7fff).contains(code))
        .max()
        .map_or(Some(0x0200), |code| code.checked_add(1));
    let mut assigned = BTreeMap::new();
    for family in families.keys() {
        if logical_codes.contains_key(family.as_str()) {
            continue;
        }
        if let Some(code) = existing_by_symbol.get(family).copied() {
            assigned.insert(family.clone(), code);
            continue;
        }
        let Some(code) = next_code.filter(|code| *code <= 0x7fff) else {
            return Err(vec![Violation::new(
                "catalog_reservation_space_exhausted",
                family,
                "no permanent core reservation code remains",
            )]);
        };
        assigned.insert(family.clone(), code);
        next_code = code.checked_add(1);
    }
    Ok(assigned)
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

fn write_reservation_row(
    out: &mut String,
    slice_id: &str,
    family: &str,
    code: u16,
    disposition: &str,
) {
    writeln!(out, "\n[[reservation]]").expect("writing to String cannot fail");
    write_string(
        out,
        "row_id",
        &format!("{slice_id}:reservation:{}", lower_kebab(family)),
    );
    write_string(out, "slice_id", slice_id);
    write_string(out, "symbol", family);
    write_string(out, "row_kind", "logical-kind");
    write_string(out, "identity_class", "logical");
    write_string(out, "code_reservation", &format!("0x{code:04x}"));
    write_string(out, "disposition", disposition);
}

fn write_source_disposition_row(
    out: &mut String,
    slice_id: &str,
    symbol: &str,
    disposition: &str,
    locations: &[String],
    g0_target_row_id: Option<&str>,
) -> Result<(), Vec<Violation>> {
    writeln!(out, "\n[[source_symbol_disposition]]").expect("writing to String cannot fail");
    let row_id = if let Some(target_row_id) = g0_target_row_id {
        g0_disposition_row_id(target_row_id).ok_or_else(|| {
            vec![Violation::new(
                "catalog_row_id_invalid",
                target_row_id,
                "G0 target row ID does not have the closed three-part grammar",
            )]
        })?
    } else {
        format!(
            "{slice_id}:source-symbol-disposition:{}",
            lower_kebab(symbol)
        )
    };
    write_string(out, "row_id", &row_id);
    write_string(out, "slice_id", slice_id);
    write_string(out, "symbol", symbol);
    write_string(out, "disposition", disposition);
    write_string_array(out, "source_locations", locations);
    Ok(())
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
    verify_source_census(catalog, source, &line_spans, &mut out);

    sort_violations(&mut out);
    out
}

/// Extract every concrete ordinary/CertifiedRemote StrongRef target family
/// and its exact Appendix-A source lines. Top-level union shorthand is
/// expanded; generic arguments and variant selectors normalize to the owning
/// family. RegisteredStrongRef and explicit metavariables are not StrongRef
/// schema families.
pub fn strong_ref_census(source: &[u8]) -> StrongRefCensus {
    let spans = source_line_spans(source);
    let mut locations: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line_number in APPENDIX_START_LINE..=APPENDIX_END_LINE {
        let Some(line) = extract_lines(source, &spans, line_number, line_number) else {
            continue;
        };
        for family in strong_ref_families_on_line(line) {
            if let Some(slice) = SLICE_PINS
                .iter()
                .find(|slice| (slice.start_line..=slice.end_line).contains(&line_number))
            {
                locations
                    .entry(family)
                    .or_default()
                    .insert(format!("{}:{line_number}", slice.id));
            }
        }
    }
    let families: BTreeMap<String, Vec<String>> = locations
        .into_iter()
        .map(|(family, locations)| (family, locations.into_iter().collect()))
        .collect();
    let mut family_transcript = String::new();
    let mut location_transcript = String::new();
    let mut location_pair_count = 0usize;
    for (family, locations) in &families {
        writeln!(&mut family_transcript, "{family}").expect("writing to String cannot fail");
        location_pair_count += locations.len();
        writeln!(&mut location_transcript, "{family}|{}", locations.join(","))
            .expect("writing to String cannot fail");
    }
    StrongRefCensus {
        families,
        family_sha256: sha256_hex(family_transcript.as_bytes()),
        location_pair_count,
        location_sha256: sha256_hex(location_transcript.as_bytes()),
    }
}

/// Classify the StrongRef families that Appendix A defines, using the pinned
/// prose grammar documented by the scaffold Bead. Mentions and field uses do
/// not count: a definition requires an inline body/alias, an explicit
/// definitional verb after a standalone code span, or a fenced declaration.
pub fn definition_census(source: &[u8], strong_refs: &StrongRefCensus) -> DefinitionCensus {
    let spans = source_line_spans(source);
    let known: BTreeSet<&str> = strong_refs.families.keys().map(String::as_str).collect();
    let mut first_lines: BTreeMap<String, i64> = BTreeMap::new();
    for line_number in APPENDIX_START_LINE..=APPENDIX_END_LINE {
        let Some(raw_line) = extract_lines(source, &spans, line_number, line_number) else {
            continue;
        };
        let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
        let mut candidates = code_span_definition_candidates(line);
        if let Some(family) = fenced_definition_candidate(line) {
            candidates.insert(family);
        }
        for family in candidates {
            if known.contains(family.as_str()) {
                first_lines.entry(family).or_insert(line_number);
            }
        }
    }

    let first_locations: BTreeMap<String, String> = first_lines
        .into_iter()
        .filter_map(|(family, line)| {
            SLICE_PINS
                .iter()
                .find(|slice| (slice.start_line..=slice.end_line).contains(&line))
                .map(|slice| (family, format!("{}:{line}", slice.id)))
        })
        .collect();
    let mut family_transcript = String::new();
    let mut location_transcript = String::new();
    for (family, location) in &first_locations {
        writeln!(&mut family_transcript, "{family}").expect("writing to String cannot fail");
        writeln!(&mut location_transcript, "{family}|{location}")
            .expect("writing to String cannot fail");
    }
    DefinitionCensus {
        first_locations,
        family_sha256: sha256_hex(family_transcript.as_bytes()),
        location_sha256: sha256_hex(location_transcript.as_bytes()),
    }
}

fn code_span_definition_candidates(line: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut cursor = 0usize;
    while let Some(relative_open) = line[cursor..].iter().position(|byte| *byte == b'`') {
        let open = cursor + relative_open;
        let Some(relative_close) = line[open + 1..].iter().position(|byte| *byte == b'`') else {
            break;
        };
        let close = open + 1 + relative_close;
        let span = &line[open + 1..close];
        if let Some((family, consumed)) = definition_family_prefix(span) {
            let rest = &span[consumed..];
            if rest
                .iter()
                .copied()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|byte| matches!(byte, b'{' | b'='))
            {
                out.insert(family.clone());
            }
            if rest.is_empty()
                && line.get(close + 1).is_some_and(u8::is_ascii_whitespace)
                && following_word(&line[close + 1..])
                    .is_some_and(|word| DEFINITION_VERBS.contains(&word))
            {
                out.insert(family);
            }
        }
        cursor = close + 1;
    }
    out
}

const DEFINITION_VERBS: [&[u8]; 21] = [
    b"is",
    b"are",
    b"has",
    b"contains",
    b"maps",
    b"uses",
    b"adds",
    b"becomes",
    b"remains",
    b"means",
    b"binds",
    b"names",
    b"carries",
    b"defines",
    b"commits",
    b"records",
    b"stores",
    b"holds",
    b"encodes",
    b"owns",
    b"selects",
];

fn definition_family_prefix(bytes: &[u8]) -> Option<(String, usize)> {
    let first = *bytes.first()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    let mut cursor = 1usize;
    while bytes.get(cursor).is_some_and(|byte| identifier_byte(*byte)) {
        cursor += 1;
    }
    let family = std::str::from_utf8(&bytes[..cursor]).ok()?.to_owned();
    if bytes.get(cursor) == Some(&b'<') {
        let close = bytes[cursor + 1..].iter().position(|byte| *byte == b'>')? + cursor + 1;
        if bytes[cursor + 1..close].contains(&b'>') {
            return None;
        }
        cursor = close + 1;
    }
    Some((family, cursor))
}

fn following_word(bytes: &[u8]) -> Option<&[u8]> {
    let start = bytes.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let end = bytes[start..]
        .iter()
        .position(|byte| !byte.is_ascii_alphabetic())
        .map_or(bytes.len(), |offset| start + offset);
    (end > start).then_some(&bytes[start..end])
}

fn fenced_definition_candidate(line: &[u8]) -> Option<String> {
    let (family, consumed) = definition_family_prefix(line)?;
    line[consumed..]
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| byte == b'{')
        .then_some(family)
}

fn verify_source_census(
    catalog: &Catalog,
    source: &[u8],
    _spans: &[(usize, usize)],
    out: &mut Vec<Violation>,
) {
    let census = strong_ref_census(source);
    if census.families.len() != EXPECTED_TYPE_RESERVATION_COUNT
        || census.family_sha256 != EXPECTED_SOURCE_FAMILY_SHA256
    {
        out.push(Violation::new(
            "source_family_census_drift",
            "source_manifest",
            format!(
                "expected {EXPECTED_TYPE_RESERVATION_COUNT} StrongRef families with sha256 {EXPECTED_SOURCE_FAMILY_SHA256}; found {} with sha256 {}",
                census.families.len(), census.family_sha256
            ),
        ));
    }
    if census.location_pair_count != EXPECTED_SOURCE_LOCATION_PAIR_COUNT
        || census.location_sha256 != EXPECTED_SOURCE_LOCATION_SHA256
    {
        out.push(Violation::new(
            "source_location_census_drift",
            "source_manifest",
            format!(
                "expected {EXPECTED_SOURCE_LOCATION_PAIR_COUNT} family/location pairs with sha256 {EXPECTED_SOURCE_LOCATION_SHA256}; found {} with sha256 {}",
                census.location_pair_count, census.location_sha256
            ),
        ));
    }
    let definitions = definition_census(source, &census);
    if definitions.first_locations.len() != EXPECTED_DEFINED_SOURCE_FAMILY_COUNT
        || definitions.family_sha256 != EXPECTED_DEFINED_SOURCE_FAMILY_SHA256
        || definitions.location_sha256 != EXPECTED_DEFINITION_LOCATION_SHA256
    {
        out.push(Violation::new(
            "source_definition_census_drift",
            "source_manifest",
            format!(
                "expected {EXPECTED_DEFINED_SOURCE_FAMILY_COUNT} defined families with family sha256 {EXPECTED_DEFINED_SOURCE_FAMILY_SHA256} and location sha256 {EXPECTED_DEFINITION_LOCATION_SHA256}; found {} with family sha256 {} and location sha256 {}",
                definitions.first_locations.len(),
                definitions.family_sha256,
                definitions.location_sha256
            ),
        ));
    }

    let catalog_rows: BTreeMap<&str, &SourceSymbolDisposition> = catalog
        .source_symbol_dispositions
        .iter()
        .filter(|row| row.slice_id != "g0")
        .map(|row| (row.symbol.as_str(), row))
        .collect();
    for (family, expected_locations) in &census.families {
        match catalog_rows.get(family.as_str()).copied() {
            Some(row) if row.source_locations == *expected_locations => {}
            Some(row) => out.push(Violation::new(
                "source_location_census_mismatch",
                &row.row_id,
                format!(
                    "source locations for {family:?} must be {:?}, found {:?}",
                    expected_locations, row.source_locations
                ),
            )),
            None => out.push(Violation::new(
                "source_family_census_missing",
                family,
                "StrongRef family has no catalog source disposition",
            )),
        }
    }
    for (family, row) in catalog_rows {
        if !census.families.contains_key(family) {
            out.push(Violation::new(
                "source_family_census_orphan",
                &row.row_id,
                format!("catalog family {family:?} is absent from the pinned Appendix source"),
            ));
        }
    }
}

fn strong_ref_families_on_line(line: &[u8]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for open in line
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'<').then_some(index))
    {
        let mut token_start = open;
        while token_start > 0 && identifier_byte(line[token_start - 1]) {
            token_start -= 1;
        }
        let wrapper = &line[token_start..open];
        match wrapper {
            b"StrongRef" | b"CertifiedRemoteStrongRef" => {}
            _ => continue,
        }
        let Some(close) = matching_angle(line, open) else {
            continue;
        };
        for alternative in split_top_level(&line[open + 1..close], b'|') {
            if let Some(family) = leading_family(alternative)
                && !matches!(family.as_str(), "T" | "Enum" | "ExactRegisteredInput")
            {
                out.insert(family);
            }
        }
    }
    out
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

fn split_top_level(bytes: &[u8], delimiter: u8) -> Vec<&[u8]> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().copied().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            value if value == delimiter && depth == 0 => {
                parts.push(&bytes[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&bytes[start..]);
    parts
}

fn leading_family(bytes: &[u8]) -> Option<String> {
    let start = bytes.iter().position(|byte| identifier_byte(*byte))?;
    let end = bytes[start..]
        .iter()
        .position(|byte| !identifier_byte(*byte))
        .map_or(bytes.len(), |offset| start + offset);
    let family = std::str::from_utf8(&bytes[start..end]).ok()?;
    (!family.is_empty()).then(|| family.to_owned())
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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
        || manifest.target_count <= 0
        || manifest.occurrence_count <= 0
        || !valid_sha256_hex(&manifest.target_ids_sha256)
        || !valid_sha256_hex(&manifest.occurrence_transcript_sha256)
    {
        out.push(Violation::new(
            "reference_manifest_mismatch",
            "reference_manifest",
            format!(
                "reference manifest must match {target_count} sorted reservation targets/{target_ids_sha256} and carry a positive, lowercase-SHA-256 occurrence pin"
            ),
        ));
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
        if !valid_type_family(&row.symbol) || !valid_generic_signature(&row.generic_signature) {
            out.push(Violation::new(
                "catalog_candidate_symbol_invalid",
                &row.row_id,
                "symbol must be one concrete type family and generic_signature must be empty or one balanced angle-bracket suffix",
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

    let mut annotation_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in &catalog.annotations {
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
        validate_concrete_expansions(&row.row_id, &row.generic_expansions, out);
        validate_concrete_expansions(&row.row_id, &row.role_expansions, out);
        validate_concrete_expansions(&row.row_id, &row.target_schema_ids, out);
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

    let mut binding_counts: BTreeMap<&str, usize> = BTreeMap::new();
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
        *binding_counts
            .entry(row.target_row_id.as_str())
            .or_default() += 1;
        validate_semantic_binding(row, &slice_map, out);
    }

    let mut static_live_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut runtime_counts: BTreeMap<&str, usize> = BTreeMap::new();
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
        if row.phase == "static" && row.status == "live" && row.gate_ids.iter().any(|id| id == "G0")
        {
            *static_live_counts
                .entry(row.target_row_id.as_str())
                .or_default() += 1;
        }
        if row.phase == "runtime" {
            *runtime_counts
                .entry(row.target_row_id.as_str())
                .or_default() += 1;
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
    if row.slice_id == "g0" {
        let expected = format!(
            "projection|{}|{}",
            projection.projection, projection.canonical_symbol
        );
        if row.source_key != expected {
            out.push(Violation::new(
                "catalog_target_source_identity_mismatch",
                &row.row_id,
                format!("g0 source_key must be {expected:?}"),
            ));
        }
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
                | "outside-structural-definition"
                | "outside-ambiguous-structure"
                | "outside-name-only"
                | "reference-only"
        ) {
            out.push(Violation::new(
                "catalog_disposition_invalid",
                &row.row_id,
                "reference target requires one truthful structural/name/reference-only disposition",
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
mod tests {
    use super::*;

    fn reservation(symbol: &str, code: &str) -> Reservation {
        Reservation {
            row_id: format!("a01:reservation:{}", lower_kebab(symbol)),
            slice_id: "a01".to_owned(),
            symbol: symbol.to_owned(),
            row_kind: "logical-kind".to_owned(),
            identity_class: "logical".to_owned(),
            code_reservation: code.to_owned(),
            disposition: "reserved".to_owned(),
        }
    }

    #[test]
    fn appendix_a_released_reservations_survive_lexically_earlier_insertions_and_promotion() {
        let reservations = [
            reservation("AlphaFamily", "0x0200"),
            reservation("BetaFamily", "0x0201"),
        ];
        let families = BTreeMap::from([
            ("AardvarkFamily".to_owned(), Vec::new()),
            ("AlphaFamily".to_owned(), Vec::new()),
            ("BetaFamily".to_owned(), Vec::new()),
        ]);

        let empty_logical = BTreeMap::new();
        let assigned = stable_reservation_codes(&reservations, &families, &empty_logical)
            .expect("stable allocation succeeds");
        assert_eq!(assigned["AlphaFamily"], 0x0200);
        assert_eq!(assigned["BetaFamily"], 0x0201);
        assert_eq!(assigned["AardvarkFamily"], 0x0202);

        let promoted = BTreeMap::from([("AlphaFamily", 0x0200)]);
        let assigned = stable_reservation_codes(&reservations, &families, &promoted)
            .expect("promotion retaining the released code succeeds");
        assert!(!assigned.contains_key("AlphaFamily"));
        assert_eq!(assigned["BetaFamily"], 0x0201);
        assert_eq!(assigned["AardvarkFamily"], 0x0202);

        let drifted = BTreeMap::from([("AlphaFamily", 0x0300)]);
        let violations = stable_reservation_codes(&reservations, &families, &drifted)
            .expect_err("promotion with a different code must fail");
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "catalog_reservation_promotion_drift")
        );
    }
}
