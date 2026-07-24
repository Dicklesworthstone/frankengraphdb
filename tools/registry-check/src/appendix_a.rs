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

pub const CATALOG_SCHEMA_VERSION: i64 = 4;
pub const CATALOG_NAME: &str = "appendix_a_catalog";
pub const CATALOG_EPOCH: i64 = 4;
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
pub const APPENDIX_BYTE_COUNT: i64 = 1_020_717;
pub const APPENDIX_SHA256: &str =
    "71a48b67304f94568590f79c5b1c1ee4731819aee022c57fece78a7e72bce7f1";
pub const APPENDIX_HEADING: &str = "## Appendix A — On-Disk Object Formats (normative contract)";
pub const NEXT_HEADING: &str = "## Appendix B — Graph Intent Log (the semantic vocabulary)";
pub const EXPECTED_PROJECTION_ROW_COUNT: usize = 360;
pub const EXPECTED_PROJECTION_ROW_IDS_SHA256: &str =
    "3416a9dad5c442685108dd8ee3ae792d38a807b908dd77378af9a32ec71bec26";
pub const EXPECTED_PROJECTION_FALLBACK_COUNT: usize = 83;
pub const EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256: &str =
    "c7765d78764616eb16d99957ec1cb26e1863ea67285f5512ba0dd7cf2d093812";
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
pub const EXPECTED_AMBIGUITY_ADJUDICATION_COUNT: usize = 176;
pub const EXPECTED_AMBIGUITY_ADJUDICATION_SHA256: &str =
    "5c85988801f963fc8b38dfca7dc796bde19f995f8ddb4e208e2a8f581338fbbc";
pub const EXPECTED_TYPE_RESERVATION_COUNT: usize = 813;
pub const EXPECTED_EXISTING_TYPE_RESERVATION_COUNT: usize = 35;
pub const EXPECTED_RESERVED_TYPE_RESERVATION_COUNT: usize = 778;
pub const EXPECTED_RESERVATION_HIGH_WATER: u16 = 0x051d;
pub const EXPECTED_RESERVATION_ASSIGNMENT_SHA256: &str =
    "3ac84204a870fcf3862b1ad6f7878b48c645a871d48fb67a2552d249039b61c2";
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

// These independent, readable pins are deliberately empty while all A01-A21
// slices are declared. A slice may add completion metadata only by adding the
// exact reciprocal target/source/owner/evidence contract here in reviewed
// code; changing the opaque transcript digest alone is never authorization.
const SEMANTIC_BINDING_CONTRACT: [SemanticBindingContractPin; 0] = [];
const EXPANSION_BINDING_CONTRACT: [ExpansionBindingContractPin; 0] = [];
const EVIDENCE_BINDING_CONTRACT: [EvidenceBindingContractPin; 0] = [];
static AMBIGUITY_ADJUDICATION_CONTRACT: [AmbiguityAdjudicationContractPin; 176] = [
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

const ROOT_KEYS: [&str; 27] = [
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

fn workspace_exact_excludes(workspace: &Table) -> Result<BTreeSet<PathBuf>, String> {
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

fn workspace_member_paths(
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

fn safe_repository_relative(path: &str) -> bool {
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
            let top_level_source_key = format!("top|{}", union.union_name);
            match top_level_by_key.get(top_level_source_key.as_str()).copied() {
                Some(candidate)
                    if candidate.slice_id == target.slice_id
                        && candidate.symbol == union.union_name
                        && candidate.generic_signature.is_empty()
                        && candidate.source_kind == "confirmed" => {}
                _ => out.push(Violation::new(
                    "source_union_top_level_owner_mismatch",
                    &target.row_id,
                    "a wire-backed top-level ordinary union requires one same-slice confirmed top-level source candidate with the exact union name",
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
    validate_readable_binding_contract(catalog, out);
    validate_readable_expansion_contract(catalog, out);
    validate_readable_ambiguity_contract(catalog, out);
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
    validate_runtime_live_owner_coupling(catalog, out);
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
                || ordinary_union_wire_source_supported;
            if !source_contract_supported {
                out.push(Violation::new(
                    "complete_slice_source_contract_unverified",
                    &row.target_row_id,
                    "complete target requires a source-reconciled top-level, field, union, or arm contract; reference-only and projection fallback targets remain declared",
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

fn validate_target_source_identity(
    row: &Target,
    projection: &ProjectionRowMeta,
    top_candidate: Option<&TopLevelCandidate>,
    ordinary_union_wire_source: Option<&str>,
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
        "union" => {
            let mut parts = row.source_key.split('|');
            let source_matches = parts.next() == Some("union")
                && parts.next().zip(parts.next()).is_some_and(|(schema, path)| {
                    parts.next().is_none()
                        && projection.canonical_symbol == format!("{schema}.{path}")
                });
            if !source_matches {
                out.push(Violation::new(
                    "catalog_target_source_identity_mismatch",
                    &row.row_id,
                    "ordinary union projection must map to the exact source schema owner and union path",
                ));
            }
        }
        "union-arm" => {
            let mut parts = row.source_key.split('|');
            let source_matches = parts.next() == Some("arm")
                && parts
                    .next()
                    .zip(parts.next())
                    .zip(parts.next())
                    .is_some_and(|((schema, path), arm)| {
                        parts.next().is_none()
                            && projection.canonical_symbol == format!("{schema}.{path}.{arm}")
                    });
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
        assert_eq!(
            a07_keys,
            [expanded_source_key],
            "the completing source slice must count the exact cross-slice occurrence"
        );
        assert_eq!(
            a07_targets.keys().copied().collect::<Vec<_>>(),
            [target_row_id]
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
