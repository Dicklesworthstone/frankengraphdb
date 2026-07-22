//! Architecture-decision registry model and validator.
//!
//! The architecture record is deliberately executable governance, not a
//! prose-only ADR collection.  This module validates the frozen source
//! excerpts, the closed decision vocabulary, stable identifiers, ownership
//! edges, and references into the live claim registries.  It remains std-only
//! so the checker obeys the same closed dependency universe it enforces.

use crate::hash::{fnv1a64, id_table_hash};
use crate::toml::{
    self, ReadError, Table, Value, get_int, get_opt_str, get_opt_str_array, get_str, get_str_array,
    get_table, get_table_array,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path};

pub const SCHEMA_VERSION: i64 = 2;
pub const REGISTRY_NAME: &str = "architecture_decisions";
pub const DECISION_ID_PREFIX: &str = "FG-ADR-";
pub const OWNERSHIP_SCOPE: &str = "all_beads_provenance";
pub const REPLAY_COMMAND: &str = "cargo run -p registry-check --bin architecture-check -- --root .";

pub const ALLOWED_CATEGORIES: [&str; 12] = [
    "thesis_bet",
    "constraint",
    "foundation_asupersync",
    "foundation_fnx",
    "foundation_frankensqlite",
    "foundation_gap",
    "sota_storage",
    "sota_query",
    "sota_incremental",
    "rejection",
    "calibration",
    "bibliography",
];

pub const ALLOWED_DISPOSITIONS: [&str; 8] = [
    "adopt",
    "adapt",
    "reject",
    "defer",
    "consume",
    "build",
    "test_only",
    "research_only",
];

pub const ALLOWED_RELATIONSHIP_KINDS: [&str; 6] = [
    "consume_as_is",
    "design_donor",
    "upstream_prerequisite",
    "build_in_house",
    "test_only_oracle",
    "research_only_citation",
];

pub const ALLOWED_STATUSES: [&str; 3] = ["frozen", "review_due", "superseded"];
pub const ALLOWED_VERIFICATION_ENTRYPOINT_STATUSES: [&str; 2] = ["live", "planned"];
pub const ALLOWED_VERIFICATION_EVIDENCE_SCOPES: [&str; 2] = ["governance", "implementation"];
pub const ALLOWED_EXTERNAL_REVIEW_OUTCOMES: [&str; 2] = ["current", "drift_detected"];

pub const REQUIRED_SOURCE_BLOCKS: [&str; 2] = [
    "plan-thesis-foundations-sota-v1",
    "plan-reviewed-bibliography-v1",
];

pub const BEAD_PROVENANCE_SOURCE_PATH: &str = ".beads/issues.jsonl";
pub const BEAD_RESOLUTION_PRECEDENCE: [&str; 4] =
    ["direct_owner", "bet_label", "exact_override", "family_rule"];
pub const ALLOWED_BET_LABELS: [&str; 6] = ["b1", "b2", "b3", "b4", "b5", "b6"];
pub const ALLOWED_FAMILY_MATCH_KINDS: [&str; 2] = ["prefix", "appendix_a"];
pub const PINNED_BEAD_COUNT: usize = 300;
pub const PINNED_DIRECT_OWNER_COUNT: usize = 98;
pub const PINNED_BET_LABEL_COUNT: usize = 157;
pub const PINNED_EXACT_OVERRIDE_COUNT: usize = 12;
pub const PINNED_FAMILY_RULE_COUNT: usize = 33;
pub const PINNED_BEAD_FAMILY_TABLE_COUNT: usize = 14;
pub const PINNED_BEAD_OVERRIDE_TABLE_COUNT: usize = 12;
pub const PINNED_BEAD_BINDING_HASH: &str = "fnv1a64:a4fe510c54bcffda";

pub const PLANNED_CRATES: [&str; 70] = [
    "fgdb-types",
    "fgdb-bigint",
    "fgdb-delta-types",
    "fgdb-claim",
    "fgdb-authz-types",
    "fgdb-policy",
    "fgdb-resource",
    "fgdb-codec",
    "fgdb-sketch",
    "fgdb-collections",
    "fgdb-crypto",
    "fgdb-calibrate",
    "fgdb-evidence",
    "fgdb-unsafe-simd",
    "fgdb-unsafe-arena",
    "fgdb-unsafe-vfs",
    "fgdb-ecs",
    "fgdb-order",
    "fgdb-chronicle",
    "fgdb-branch",
    "fgdb-keymgr",
    "fgdb-audit",
    "fgdb-backup",
    "fgdb-strata",
    "fgdb-props",
    "fgdb-buffer",
    "fgdb-scratch",
    "fgdb-txn",
    "fgdb-constraints",
    "fgdb-secure-view",
    "fgdb-gql",
    "fgdb-cypher",
    "fgdb-bind",
    "fgdb-algebra",
    "fgdb-planner",
    "fgdb-exec",
    "fgdb-linalg",
    "fgdb-datalog",
    "fgdb-ripple",
    "fgdb-views",
    "fgdb-subs",
    "fgdb-index-core",
    "fgdb-btree",
    "fgdb-fts",
    "fgdb-vector",
    "fgdb-pathidx",
    "fgdb-prism",
    "fgdb-warden",
    "fgdb-privacy",
    "fgdb-redaction",
    "fgdb-protocol",
    "fgdb-bolt",
    "fgdb-formats",
    "fgdb-udf-vm",
    "fgdb-observatory",
    "fgdb-system-graph",
    "fgdb-raft",
    "fgdb-repl",
    "fgdb-shard",
    "fgdb",
    "fgdb-server",
    "fgdb-cli",
    "fgdb-python",
    "fgdb-adbc",
    "fgdb-sim",
    "fgdb-reference",
    "fgdb-oracles",
    "fgdb-bench",
    "fgdb-conformance",
    "fgdb-fuzz",
];

pub const PINNED_DECISION_COUNT: usize = 256;
pub const PINNED_BIBLIOGRAPHY_COUNT: usize = 138;
pub const PINNED_EXTERNAL_REVIEW_DECISION_COUNT: usize = 64;

// Independent code pins keep a self-consistent registry edit from silently
// removing a row or inverting an adopt/reject decision.
pub const PINNED_DECISION_ID_HASH: &str = "fnv1a64:21402ba5834603dd";
pub const PINNED_BIBLIOGRAPHY_ID_HASH: &str = "fnv1a64:212896d82dc8caf7";
pub const PINNED_BIBLIOGRAPHY_ANCHOR_HASH: &str = "fnv1a64:35bc497bde8cd1d4";
pub const PINNED_SEMANTIC_CONTRACT_HASH: &str = "fnv1a64:f18d79ffa7a2e03d";
// Filled from the independently reviewed append-only source/review transcript.
// This is intentionally separate from `PINNED_SEMANTIC_CONTRACT_HASH`.
pub const PINNED_EXTERNAL_REVIEW_HISTORY_HASH: &str = "fnv1a64:5e15e3d99eec84ac";

/// All non-bibliography counts are deliberately pinned.  Bibliography is
/// normalized independently and its registry count is checked against the
/// actual number of normalized rows.
pub const PINNED_CATEGORY_COUNTS: [(&str, usize); 11] = [
    ("thesis_bet", 6),
    ("constraint", 12),
    ("foundation_asupersync", 16),
    ("foundation_fnx", 7),
    ("foundation_frankensqlite", 9),
    ("foundation_gap", 13),
    ("sota_storage", 7),
    ("sota_query", 7),
    ("sota_incremental", 5),
    ("rejection", 20),
    ("calibration", 16),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHeader {
    pub name: String,
    pub decision_id_prefix: String,
    pub ownership_scope: String,
    pub allowed_categories: Vec<String>,
    pub allowed_dispositions: Vec<String>,
    pub allowed_relationship_kinds: Vec<String>,
    pub allowed_statuses: Vec<String>,
    pub planned_crates: Vec<String>,
    pub required_source_blocks: Vec<String>,
    pub decision_count: usize,
    pub id_table_hash: String,
    pub external_review_history_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryCount {
    pub category: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBlock {
    pub id: String,
    pub document_path: String,
    pub start_marker: String,
    pub end_marker: String,
    pub plan_path: String,
    pub plan_start_line: usize,
    pub plan_end_line: usize,
    pub line_count: usize,
    pub byte_count: usize,
    pub fnv1a64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub rationale: String,
    pub assumptions: Vec<String>,
    pub no_claim_boundary: Vec<String>,
    pub verification_strategy: Vec<String>,
    pub negative_evidence_triggers: Vec<String>,
    pub review_policy: String,
    pub reviewed_at: String,
    pub review_after: String,
    pub check_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: String,
    pub category: String,
    pub stable_key: String,
    pub source_block: String,
    pub source_anchor: String,
    pub disposition: String,
    pub relationship_kind: String,
    pub summary: String,
    pub profile: String,
    pub owner_workstream: String,
    pub owner_beads: Vec<String>,
    pub owner_crates: Vec<String>,
    pub affected_bets: Vec<String>,
    pub affected_constraints: Vec<String>,
    pub affected_invariants: Vec<String>,
    pub affected_evidence: Vec<String>,
    pub affected_slos: Vec<String>,
    pub affected_cost_rows: Vec<String>,
    pub affected_format_rows: Vec<String>,
    pub verification_entrypoints: Vec<String>,
    pub checker_ids: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub status: String,
}

/// Registry-level declaration for a decision's verification label.  The
/// declaration separates executable evidence from a planned future gate: a
/// `live` row must resolve to a live checker-index artifact, while a `planned`
/// row is never counted as implementation evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationEntrypoint {
    pub entrypoint: String,
    pub status: String,
    pub evidence_scope: String,
    pub checker_id: Option<String>,
    pub package: Option<String>,
    pub target: Option<String>,
    pub selector: Option<String>,
    pub command_argv: Option<Vec<String>>,
}

/// Immutable, content-pinned external material consulted by an ADR review.
/// The checker does not fetch network content; `content_digest` authenticates
/// the exact bytes reviewed and `source_fingerprint` binds all source metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalReviewSource {
    pub id: String,
    pub uri: String,
    pub published_at: String,
    pub retrieved_at: String,
    pub content_digest: String,
    pub source_fingerprint: String,
}

/// One append-only review event in a decision-local linear chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalReview {
    pub id: String,
    pub decision_id: String,
    pub sequence: usize,
    pub predecessor: String,
    pub reviewed_at: String,
    pub claim_fingerprint: String,
    pub source_ids: Vec<String>,
    pub outcome: String,
    pub record_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadProvenanceConfig {
    pub source_path: String,
    pub resolution_precedence: Vec<String>,
    pub allowed_bet_labels: Vec<String>,
    pub bead_count: usize,
    pub direct_owner_count: usize,
    pub bet_label_count: usize,
    pub exact_override_count: usize,
    pub family_rule_count: usize,
    pub binding_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadFamily {
    pub id: String,
    pub match_kind: String,
    pub pattern: String,
    pub decision_ids: Vec<String>,
    pub expected_match_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadOverride {
    pub id: String,
    pub bead_id: String,
    pub decision_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchitectureRegistry {
    pub schema_version: i64,
    pub registry: RegistryHeader,
    pub category_counts: Vec<CategoryCount>,
    pub source_blocks: Vec<SourceBlock>,
    pub profiles: Vec<Profile>,
    pub decisions: Vec<Decision>,
    pub verification_entrypoints: Vec<VerificationEntrypoint>,
    pub external_review_sources: Vec<ExternalReviewSource>,
    pub external_reviews: Vec<ExternalReview>,
    pub bead_provenance: BeadProvenanceConfig,
    pub bead_families: Vec<BeadFamily>,
    pub bead_overrides: Vec<BeadOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for LoadError {}

impl From<ReadError> for LoadError {
    fn from(value: ReadError) -> Self {
        Self {
            path: value.path,
            message: value.msg,
        }
    }
}

fn read_error(path: impl Into<String>, message: impl Into<String>) -> ReadError {
    ReadError {
        path: path.into(),
        msg: message.into(),
    }
}

fn usize_field(table: &Table, key: &str, ctx: &str) -> Result<usize, ReadError> {
    let value = get_int(table, key, ctx)?;
    usize::try_from(value).map_err(|_| {
        read_error(
            format!("{ctx}.{key}"),
            format!("expected a non-negative platform-sized integer, found {value}"),
        )
    })
}

fn require_present(table: &Table, key: &str, ctx: &str) -> Result<(), ReadError> {
    if table.contains_key(key) {
        Ok(())
    } else {
        Err(read_error(format!("{ctx}.{key}"), "missing required key"))
    }
}

fn exact_keys(table: &Table, allowed: &[&str], ctx: &str) -> Result<(), ReadError> {
    let allowed: BTreeSet<&str> = allowed.iter().copied().collect();
    let unknown: Vec<&str> = table
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(read_error(
            ctx,
            format!("unknown key(s) in closed schema: {}", unknown.join(", ")),
        ))
    }
}

fn registry_header_from(table: &Table) -> Result<RegistryHeader, ReadError> {
    let ctx = "architecture_decisions.toml.registry";
    exact_keys(
        table,
        &[
            "name",
            "decision_id_prefix",
            "ownership_scope",
            "allowed_categories",
            "allowed_dispositions",
            "allowed_relationship_kinds",
            "allowed_statuses",
            "planned_crates",
            "required_source_blocks",
            "decision_count",
            "id_table_hash",
            "external_review_history_hash",
        ],
        ctx,
    )?;
    Ok(RegistryHeader {
        name: get_str(table, "name", ctx)?,
        decision_id_prefix: get_str(table, "decision_id_prefix", ctx)?,
        ownership_scope: get_str(table, "ownership_scope", ctx)?,
        allowed_categories: get_str_array(table, "allowed_categories", ctx)?,
        allowed_dispositions: get_str_array(table, "allowed_dispositions", ctx)?,
        allowed_relationship_kinds: get_str_array(table, "allowed_relationship_kinds", ctx)?,
        allowed_statuses: get_str_array(table, "allowed_statuses", ctx)?,
        planned_crates: get_str_array(table, "planned_crates", ctx)?,
        required_source_blocks: get_str_array(table, "required_source_blocks", ctx)?,
        decision_count: usize_field(table, "decision_count", ctx)?,
        id_table_hash: get_str(table, "id_table_hash", ctx)?,
        external_review_history_hash: get_str(table, "external_review_history_hash", ctx)?,
    })
}

fn category_count_from(table: &Table, index: usize) -> Result<CategoryCount, ReadError> {
    let ctx = format!("architecture_decisions.toml.category_count[{index}]");
    exact_keys(table, &["category", "count"], &ctx)?;
    Ok(CategoryCount {
        category: get_str(table, "category", &ctx)?,
        count: usize_field(table, "count", &ctx)?,
    })
}

fn source_block_from(table: &Table, index: usize) -> Result<SourceBlock, ReadError> {
    let ctx = format!("architecture_decisions.toml.source_block[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "document_path",
            "start_marker",
            "end_marker",
            "plan_path",
            "plan_start_line",
            "plan_end_line",
            "line_count",
            "byte_count",
            "fnv1a64",
        ],
        &ctx,
    )?;
    Ok(SourceBlock {
        id: get_str(table, "id", &ctx)?,
        document_path: get_str(table, "document_path", &ctx)?,
        start_marker: get_str(table, "start_marker", &ctx)?,
        end_marker: get_str(table, "end_marker", &ctx)?,
        plan_path: get_str(table, "plan_path", &ctx)?,
        plan_start_line: usize_field(table, "plan_start_line", &ctx)?,
        plan_end_line: usize_field(table, "plan_end_line", &ctx)?,
        line_count: usize_field(table, "line_count", &ctx)?,
        byte_count: usize_field(table, "byte_count", &ctx)?,
        fnv1a64: get_str(table, "fnv1a64", &ctx)?,
    })
}

fn profile_from(table: &Table, index: usize) -> Result<Profile, ReadError> {
    let ctx = format!("architecture_decisions.toml.profile[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "rationale",
            "assumptions",
            "no_claim_boundary",
            "verification_strategy",
            "negative_evidence_triggers",
            "review_policy",
            "reviewed_at",
            "review_after",
            "check_command",
        ],
        &ctx,
    )?;
    Ok(Profile {
        id: get_str(table, "id", &ctx)?,
        rationale: get_str(table, "rationale", &ctx)?,
        assumptions: get_str_array(table, "assumptions", &ctx)?,
        no_claim_boundary: get_str_array(table, "no_claim_boundary", &ctx)?,
        verification_strategy: get_str_array(table, "verification_strategy", &ctx)?,
        negative_evidence_triggers: get_str_array(table, "negative_evidence_triggers", &ctx)?,
        review_policy: get_str(table, "review_policy", &ctx)?,
        reviewed_at: get_str(table, "reviewed_at", &ctx)?,
        review_after: get_str(table, "review_after", &ctx)?,
        check_command: get_str(table, "check_command", &ctx)?,
    })
}

fn decision_from(table: &Table, index: usize) -> Result<Decision, ReadError> {
    let ctx = format!("architecture_decisions.toml.decision[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "category",
            "stable_key",
            "source_block",
            "source_anchor",
            "disposition",
            "relationship_kind",
            "summary",
            "profile",
            "owner_workstream",
            "owner_beads",
            "owner_crates",
            "affected_bets",
            "affected_constraints",
            "affected_invariants",
            "affected_evidence",
            "affected_slos",
            "affected_cost_rows",
            "affected_format_rows",
            "verification_entrypoints",
            "checker_ids",
            "evidence_ids",
            "status",
        ],
        &ctx,
    )?;
    Ok(Decision {
        id: get_str(table, "id", &ctx)?,
        category: get_str(table, "category", &ctx)?,
        stable_key: get_str(table, "stable_key", &ctx)?,
        source_block: get_str(table, "source_block", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
        disposition: get_str(table, "disposition", &ctx)?,
        relationship_kind: get_str(table, "relationship_kind", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
        profile: get_str(table, "profile", &ctx)?,
        owner_workstream: get_str(table, "owner_workstream", &ctx)?,
        owner_beads: get_str_array(table, "owner_beads", &ctx)?,
        owner_crates: get_str_array(table, "owner_crates", &ctx)?,
        affected_bets: get_str_array(table, "affected_bets", &ctx)?,
        affected_constraints: get_str_array(table, "affected_constraints", &ctx)?,
        affected_invariants: get_str_array(table, "affected_invariants", &ctx)?,
        affected_evidence: get_str_array(table, "affected_evidence", &ctx)?,
        affected_slos: get_str_array(table, "affected_slos", &ctx)?,
        affected_cost_rows: get_str_array(table, "affected_cost_rows", &ctx)?,
        affected_format_rows: get_str_array(table, "affected_format_rows", &ctx)?,
        verification_entrypoints: get_str_array(table, "verification_entrypoints", &ctx)?,
        checker_ids: get_str_array(table, "checker_ids", &ctx)?,
        evidence_ids: get_str_array(table, "evidence_ids", &ctx)?,
        status: get_str(table, "status", &ctx)?,
    })
}

fn verification_entrypoint_from(
    table: &Table,
    index: usize,
) -> Result<VerificationEntrypoint, ReadError> {
    let ctx = format!("architecture_decisions.toml.verification_entrypoint[{index}]");
    exact_keys(
        table,
        &[
            "entrypoint",
            "status",
            "evidence_scope",
            "checker_id",
            "package",
            "target",
            "selector",
            "command_argv",
        ],
        &ctx,
    )?;
    Ok(VerificationEntrypoint {
        entrypoint: get_str(table, "entrypoint", &ctx)?,
        status: get_str(table, "status", &ctx)?,
        evidence_scope: get_str(table, "evidence_scope", &ctx)?,
        checker_id: get_opt_str(table, "checker_id", &ctx)?,
        package: get_opt_str(table, "package", &ctx)?,
        target: get_opt_str(table, "target", &ctx)?,
        selector: get_opt_str(table, "selector", &ctx)?,
        command_argv: get_opt_str_array(table, "command_argv", &ctx)?,
    })
}

fn external_review_source_from(
    table: &Table,
    index: usize,
) -> Result<ExternalReviewSource, ReadError> {
    let ctx = format!("architecture_decisions.toml.external_review_source[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "uri",
            "published_at",
            "retrieved_at",
            "content_digest",
            "source_fingerprint",
        ],
        &ctx,
    )?;
    Ok(ExternalReviewSource {
        id: get_str(table, "id", &ctx)?,
        uri: get_str(table, "uri", &ctx)?,
        published_at: get_str(table, "published_at", &ctx)?,
        retrieved_at: get_str(table, "retrieved_at", &ctx)?,
        content_digest: get_str(table, "content_digest", &ctx)?,
        source_fingerprint: get_str(table, "source_fingerprint", &ctx)?,
    })
}

fn external_review_from(table: &Table, index: usize) -> Result<ExternalReview, ReadError> {
    let ctx = format!("architecture_decisions.toml.external_review[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "decision_id",
            "sequence",
            "predecessor",
            "reviewed_at",
            "claim_fingerprint",
            "source_ids",
            "outcome",
            "record_fingerprint",
        ],
        &ctx,
    )?;
    Ok(ExternalReview {
        id: get_str(table, "id", &ctx)?,
        decision_id: get_str(table, "decision_id", &ctx)?,
        sequence: usize_field(table, "sequence", &ctx)?,
        predecessor: get_str(table, "predecessor", &ctx)?,
        reviewed_at: get_str(table, "reviewed_at", &ctx)?,
        claim_fingerprint: get_str(table, "claim_fingerprint", &ctx)?,
        source_ids: get_str_array(table, "source_ids", &ctx)?,
        outcome: get_str(table, "outcome", &ctx)?,
        record_fingerprint: get_str(table, "record_fingerprint", &ctx)?,
    })
}

fn bead_provenance_from(table: &Table) -> Result<BeadProvenanceConfig, ReadError> {
    let ctx = "architecture_decisions.toml.bead_provenance";
    exact_keys(
        table,
        &[
            "source_path",
            "resolution_precedence",
            "allowed_bet_labels",
            "bead_count",
            "direct_owner_count",
            "bet_label_count",
            "exact_override_count",
            "family_rule_count",
            "binding_hash",
        ],
        ctx,
    )?;
    Ok(BeadProvenanceConfig {
        source_path: get_str(table, "source_path", ctx)?,
        resolution_precedence: get_str_array(table, "resolution_precedence", ctx)?,
        allowed_bet_labels: get_str_array(table, "allowed_bet_labels", ctx)?,
        bead_count: usize_field(table, "bead_count", ctx)?,
        direct_owner_count: usize_field(table, "direct_owner_count", ctx)?,
        bet_label_count: usize_field(table, "bet_label_count", ctx)?,
        exact_override_count: usize_field(table, "exact_override_count", ctx)?,
        family_rule_count: usize_field(table, "family_rule_count", ctx)?,
        binding_hash: get_str(table, "binding_hash", ctx)?,
    })
}

fn bead_family_from(table: &Table, index: usize) -> Result<BeadFamily, ReadError> {
    let ctx = format!("architecture_decisions.toml.bead_family[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "match_kind",
            "pattern",
            "decision_ids",
            "expected_match_count",
        ],
        &ctx,
    )?;
    Ok(BeadFamily {
        id: get_str(table, "id", &ctx)?,
        match_kind: get_str(table, "match_kind", &ctx)?,
        pattern: get_str(table, "pattern", &ctx)?,
        decision_ids: get_str_array(table, "decision_ids", &ctx)?,
        expected_match_count: usize_field(table, "expected_match_count", &ctx)?,
    })
}

fn bead_override_from(table: &Table, index: usize) -> Result<BeadOverride, ReadError> {
    let ctx = format!("architecture_decisions.toml.bead_override[{index}]");
    exact_keys(table, &["id", "bead_id", "decision_ids"], &ctx)?;
    Ok(BeadOverride {
        id: get_str(table, "id", &ctx)?,
        bead_id: get_str(table, "bead_id", &ctx)?,
        decision_ids: get_str_array(table, "decision_ids", &ctx)?,
    })
}

/// Construct the typed flattened schema from an already parsed TOML table.
pub fn architecture_from(root: &Table) -> Result<ArchitectureRegistry, ReadError> {
    exact_keys(
        root,
        &[
            "schema_version",
            "registry",
            "category_count",
            "source_block",
            "profile",
            "decision",
            "verification_entrypoint",
            "external_review_source",
            "external_review",
            "bead_provenance",
            "bead_family",
            "bead_override",
        ],
        "architecture_decisions.toml",
    )?;
    for key in [
        "category_count",
        "source_block",
        "profile",
        "decision",
        "verification_entrypoint",
        "external_review_source",
        "external_review",
        "bead_provenance",
        "bead_family",
        "bead_override",
    ] {
        require_present(root, key, "architecture_decisions.toml")?;
    }
    let registry =
        registry_header_from(get_table(root, "registry", "architecture_decisions.toml")?)?;
    let category_counts = get_table_array(root, "category_count", "architecture_decisions.toml")?
        .iter()
        .enumerate()
        .map(|(i, row)| category_count_from(row, i))
        .collect::<Result<Vec<_>, _>>()?;
    let source_blocks = get_table_array(root, "source_block", "architecture_decisions.toml")?
        .iter()
        .enumerate()
        .map(|(i, row)| source_block_from(row, i))
        .collect::<Result<Vec<_>, _>>()?;
    let profiles = get_table_array(root, "profile", "architecture_decisions.toml")?
        .iter()
        .enumerate()
        .map(|(i, row)| profile_from(row, i))
        .collect::<Result<Vec<_>, _>>()?;
    let decisions = get_table_array(root, "decision", "architecture_decisions.toml")?
        .iter()
        .enumerate()
        .map(|(i, row)| decision_from(row, i))
        .collect::<Result<Vec<_>, _>>()?;
    let verification_entrypoints = get_table_array(
        root,
        "verification_entrypoint",
        "architecture_decisions.toml",
    )?
    .iter()
    .enumerate()
    .map(|(i, row)| verification_entrypoint_from(row, i))
    .collect::<Result<Vec<_>, _>>()?;
    let external_review_sources = get_table_array(
        root,
        "external_review_source",
        "architecture_decisions.toml",
    )?
    .iter()
    .enumerate()
    .map(|(i, row)| external_review_source_from(row, i))
    .collect::<Result<Vec<_>, _>>()?;
    let external_reviews = get_table_array(root, "external_review", "architecture_decisions.toml")?
        .iter()
        .enumerate()
        .map(|(i, row)| external_review_from(row, i))
        .collect::<Result<Vec<_>, _>>()?;
    let bead_provenance = bead_provenance_from(get_table(
        root,
        "bead_provenance",
        "architecture_decisions.toml",
    )?)?;
    let bead_families = get_table_array(root, "bead_family", "architecture_decisions.toml")?
        .iter()
        .enumerate()
        .map(|(i, row)| bead_family_from(row, i))
        .collect::<Result<Vec<_>, _>>()?;
    let bead_overrides = get_table_array(root, "bead_override", "architecture_decisions.toml")?
        .iter()
        .enumerate()
        .map(|(i, row)| bead_override_from(row, i))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ArchitectureRegistry {
        schema_version: get_int(root, "schema_version", "architecture_decisions.toml")?,
        registry,
        category_counts,
        source_blocks,
        profiles,
        decisions,
        verification_entrypoints,
        external_review_sources,
        external_reviews,
        bead_provenance,
        bead_families,
        bead_overrides,
    })
}

/// Parse a registry string into the public model.
pub fn parse_architecture(text: &str) -> Result<ArchitectureRegistry, LoadError> {
    let root = toml::parse(text).map_err(|error| LoadError {
        path: "architecture_decisions.toml".into(),
        message: error.to_string(),
    })?;
    architecture_from(&root).map_err(LoadError::from)
}

/// Load the architecture registry at an explicit path.
pub fn load_architecture(path: &Path) -> Result<ArchitectureRegistry, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    parse_architecture(&text).map_err(|mut error| {
        error.path = format!("{} ({})", path.display(), error.path);
        error
    })
}

/// Load `registries/architecture_decisions.toml` below a repository root.
pub fn load_from_repo(root: &Path) -> Result<ArchitectureRegistry, LoadError> {
    load_architecture(&root.join("registries/architecture_decisions.toml"))
}

/// A stable, fully contextual diagnostic.  The same fields are emitted by the
/// standalone CLI for both pass rows and violations, which keeps CI logs
/// useful without exposing registry prose beyond the already-public summary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Violation {
    pub code: String,
    pub decision_id: String,
    pub relationship_kind: String,
    pub owner_bead: String,
    pub owner_crate: String,
    pub claim_class: String,
    pub checker_ids: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub status: String,
    pub contradiction_class: String,
    pub source_anchor: String,
    pub replay_command: String,
    pub message: String,
}

impl Violation {
    fn global(
        code: impl Into<String>,
        contradiction_class: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            decision_id: "<registry>".into(),
            relationship_kind: "<registry>".into(),
            owner_bead: String::new(),
            owner_crate: String::new(),
            claim_class: "architectural_decision".into(),
            checker_ids: Vec::new(),
            evidence_ids: Vec::new(),
            status: "invalid".into(),
            contradiction_class: contradiction_class.into(),
            source_anchor: "registries/architecture_decisions.toml".into(),
            replay_command: REPLAY_COMMAND.into(),
            message: message.into(),
        }
    }

    fn for_bead(
        code: impl Into<String>,
        bead_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let mut violation = Self::global(code, "bead_provenance", message);
        violation.owner_bead = bead_id.into();
        violation
    }

    fn for_decision(
        decision: &Decision,
        profiles: &BTreeMap<String, &Profile>,
        claim_class: &str,
        code: impl Into<String>,
        contradiction_class: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let replay_command = profiles
            .get(&decision.profile)
            .map(|profile| profile.check_command.clone())
            .unwrap_or_else(|| REPLAY_COMMAND.into());
        Self {
            code: code.into(),
            decision_id: decision.id.clone(),
            relationship_kind: decision.relationship_kind.clone(),
            owner_bead: decision.owner_beads.first().cloned().unwrap_or_default(),
            owner_crate: decision.owner_crates.first().cloned().unwrap_or_default(),
            claim_class: claim_class.into(),
            checker_ids: decision.checker_ids.clone(),
            evidence_ids: decision.evidence_ids.clone(),
            status: decision.status.clone(),
            contradiction_class: contradiction_class.into(),
            source_anchor: decision.source_anchor.clone(),
            replay_command,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBlockCheck {
    pub id: String,
    pub line_count: usize,
    pub byte_count: usize,
    pub fnv1a64: String,
    pub exact_match: bool,
    pub outcome: String,
}

#[derive(Debug, Clone, Default)]
struct ReferenceCatalog {
    bets: BTreeSet<String>,
    constraints: BTreeSet<String>,
    invariants: BTreeSet<String>,
    evidence: BTreeMap<String, String>,
    slos: BTreeMap<String, String>,
    checkers: BTreeMap<String, CheckerRecord>,
    cost_rows: Option<BTreeSet<String>>,
    format_rows: Option<BTreeSet<String>>,
}

#[derive(Debug, Clone)]
struct CheckerRecord {
    kind: String,
    artifact: String,
    status: String,
}

fn set_of(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn duplicates(values: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for value in values {
        if !seen.insert(value.clone()) {
            duplicates.insert(value.clone());
        }
    }
    duplicates.into_iter().collect()
}

fn blank_items(values: &[String]) -> Vec<usize> {
    values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| value.trim().is_empty().then_some(index))
        .collect()
}

fn canonical_fnv(bytes: &[u8]) -> String {
    format!("0x{:016x}", fnv1a64(bytes))
}

fn safe_repo_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn read_repo_bytes(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    if !safe_repo_relative(relative) {
        return Err(format!(
            "path {relative:?} is not a safe repository-relative path"
        ));
    }
    fs::read(root.join(relative)).map_err(|error| format!("{relative}: {error}"))
}

fn line_range(bytes: &[u8], start: usize, end: usize) -> Result<&[u8], String> {
    if start == 0 || end < start {
        return Err(format!("invalid inclusive line range {start}..={end}"));
    }
    let mut line = 1usize;
    let mut start_offset = (start == 1).then_some(0usize);
    let mut end_offset = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if line == end {
                end_offset = Some(index + 1);
                break;
            }
            line += 1;
            if line == start {
                start_offset = Some(index + 1);
            }
        }
    }
    let start_offset =
        start_offset.ok_or_else(|| format!("start line {start} exceeds document line count"))?;
    let end_offset = match end_offset {
        Some(offset) => offset,
        None if line == end && start_offset <= bytes.len() => bytes.len(),
        None => return Err(format!("end line {end} exceeds document line count")),
    };
    Ok(&bytes[start_offset..end_offset])
}

fn unique_marker_offset(haystack: &[u8], marker: &str) -> Result<usize, String> {
    if marker.is_empty() {
        return Err("marker is empty".into());
    }
    let marker = marker.as_bytes();
    let offsets: Vec<usize> = haystack
        .windows(marker.len())
        .enumerate()
        .filter_map(|(index, window)| (window == marker).then_some(index))
        .collect();
    match offsets.as_slice() {
        [offset] => Ok(*offset),
        [] => Err("marker does not occur in document".into()),
        _ => Err(format!("marker occurs {} times in document", offsets.len())),
    }
}

fn embedded_between_markers<'a>(
    document: &'a [u8],
    start_marker: &str,
    end_marker: &str,
) -> Result<&'a [u8], String> {
    let start = unique_marker_offset(document, start_marker)?;
    let end = unique_marker_offset(document, end_marker)?;
    let mut body_start = start + start_marker.len();
    if document.get(body_start..body_start + 2) == Some(b"\r\n") {
        body_start += 2;
    } else if document.get(body_start) == Some(&b'\n') {
        body_start += 1;
    }
    if end < body_start {
        return Err("end marker precedes start marker".into());
    }
    Ok(&document[body_start..end])
}

fn source_expected(block: &SourceBlock) -> Option<(usize, usize, usize, usize, &'static str)> {
    match block.id.as_str() {
        "plan-thesis-foundations-sota-v1" => Some((1, 184, 184, 46_176, "0xb09e44e4eec5c18a")),
        "plan-reviewed-bibliography-v1" => Some((3196, 3199, 4, 4_741, "0xba0fcc184882baec")),
        _ => None,
    }
}

fn source_check(block: &SourceBlock, root: &Path) -> Result<SourceBlockCheck, String> {
    let document = read_repo_bytes(root, &block.document_path)?;
    let plan = read_repo_bytes(root, &block.plan_path)?;
    let embedded = embedded_between_markers(&document, &block.start_marker, &block.end_marker)?;
    let expected = line_range(&plan, block.plan_start_line, block.plan_end_line)?;
    let line_count = block
        .plan_end_line
        .checked_sub(block.plan_start_line)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| "source line-count arithmetic overflow".to_string())?;
    let hash = canonical_fnv(embedded);
    let metadata_matches = block.line_count == line_count
        && block.byte_count == embedded.len()
        && block.fnv1a64 == hash;
    Ok(SourceBlockCheck {
        id: block.id.clone(),
        line_count,
        byte_count: embedded.len(),
        fnv1a64: hash,
        exact_match: embedded == expected,
        outcome: if metadata_matches && embedded == expected {
            "pass"
        } else {
            "fail"
        }
        .into(),
    })
}

/// Recompute byte/line/hash evidence for every declared source block.
pub fn check_source_blocks(
    registry: &ArchitectureRegistry,
    root: &Path,
) -> Vec<Result<SourceBlockCheck, String>> {
    let mut blocks: Vec<&SourceBlock> = registry.source_blocks.iter().collect();
    blocks.sort_by(|left, right| left.id.cmp(&right.id));
    blocks
        .into_iter()
        .map(|block| source_check(block, root))
        .collect()
}

fn read_toml(path: &Path) -> Result<Table, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    toml::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
}

#[derive(Debug)]
struct WorkspacePackage {
    relative_path: String,
    manifest: Table,
}

fn workspace_member_paths(root: &Path) -> Result<Vec<String>, String> {
    let manifest = read_toml(&root.join("Cargo.toml"))?;
    let workspace =
        get_table(&manifest, "workspace", "Cargo.toml").map_err(|error| error.to_string())?;
    let members = get_str_array(workspace, "members", "Cargo.toml.workspace")
        .map_err(|error| error.to_string())?;
    let mut paths = Vec::new();
    for member in members {
        if let Some(parent) = member.strip_suffix("/*") {
            if !safe_repo_relative(parent) {
                return Err(format!("unsafe Cargo workspace member glob {member:?}"));
            }
            let mut children = fs::read_dir(root.join(parent))
                .map_err(|error| format!("workspace member glob {member:?}: {error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("workspace member glob {member:?}: {error}"))?;
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                if child.path().join("Cargo.toml").is_file() {
                    let relative = Path::new(parent).join(child.file_name());
                    paths.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        } else if safe_repo_relative(&member) {
            paths.push(member);
        } else {
            return Err(format!("unsafe Cargo workspace member path {member:?}"));
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn resolve_workspace_package(root: &Path, name: &str) -> Result<Option<WorkspacePackage>, String> {
    let mut resolved = None;
    for relative_path in workspace_member_paths(root)? {
        let manifest = read_toml(&root.join(&relative_path).join("Cargo.toml"))?;
        let package = get_table(&manifest, "package", "workspace member Cargo.toml")
            .map_err(|error| error.to_string())?;
        let package_name = get_str(package, "name", "workspace member Cargo.toml.package")
            .map_err(|error| error.to_string())?;
        if package_name == name {
            if resolved.is_some() {
                return Err(format!(
                    "Cargo workspace contains duplicate package name {name:?}"
                ));
            }
            resolved = Some(WorkspacePackage {
                relative_path,
                manifest,
            });
        }
    }
    Ok(resolved)
}

fn cargo_test_artifact(package: &WorkspacePackage, target: &str) -> String {
    format!("{}/tests/{target}.rs", package.relative_path)
}

fn cargo_bin_artifact(
    root: &Path,
    package: &WorkspacePackage,
    target: &str,
) -> Result<Option<String>, String> {
    if let Some(Value::Array(rows)) = package.manifest.get("bin") {
        for (index, row) in rows.iter().enumerate() {
            let Value::Table(row) = row else {
                return Err(format!("Cargo.toml bin[{index}] is not a table"));
            };
            let ctx = format!("Cargo.toml.bin[{index}]");
            let name = get_str(row, "name", &ctx).map_err(|error| error.to_string())?;
            if name == target {
                let path = get_str(row, "path", &ctx).map_err(|error| error.to_string())?;
                if !safe_repo_relative(&path) {
                    return Err(format!("Cargo binary {target:?} has unsafe path {path:?}"));
                }
                return Ok(Some(format!("{}/{path}", package.relative_path)));
            }
        }
    }
    let conventional = format!("{}/src/bin/{target}.rs", package.relative_path);
    Ok(Some(conventional).filter(|path| root.join(path).is_file()))
}

fn rust_test_selector_count(source: &str, selector: &str) -> usize {
    let function = format!("fn {selector}");
    let lines: Vec<&str> = source.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            let line = line.trim_start();
            line.starts_with(&function) && line[function.len()..].trim_start().starts_with('(')
        })
        .filter(|(index, _)| {
            let start = index.saturating_sub(4);
            lines[start..*index]
                .iter()
                .any(|line| line.trim() == "#[test]")
        })
        .count()
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    path.is_file()
}

fn collect_rows(
    root: &Table,
    array_key: &str,
    id_key: &str,
    class_key: Option<&str>,
) -> Result<Vec<(String, Option<String>)>, String> {
    let rows = get_table_array(root, array_key, array_key).map_err(|error| error.to_string())?;
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let ctx = format!("{array_key}[{index}]");
            let id = get_str(row, id_key, &ctx).map_err(|error| error.to_string())?;
            let claim_class = class_key
                .map(|key| get_str(row, key, &ctx).map_err(|error| error.to_string()))
                .transpose()?;
            Ok((id, claim_class))
        })
        .collect()
}

fn collect_all_id_values(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Table(table) => {
            if let Some(Value::Str(id)) = table.get("id") {
                output.insert(id.clone());
            }
            for child in table.values() {
                collect_all_id_values(child, output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_all_id_values(item, output);
            }
        }
        Value::Str(_) | Value::Int(_) | Value::Bool(_) => {}
    }
}

fn optional_id_registry(
    root: &Path,
    candidates: &[&str],
) -> Result<Option<BTreeSet<String>>, String> {
    for relative in candidates {
        let path = root.join(relative);
        if path.exists() {
            let table = read_toml(&path)?;
            let mut ids = BTreeSet::new();
            collect_all_id_values(&Value::Table(table), &mut ids);
            return Ok(Some(ids));
        }
    }
    Ok(None)
}

fn load_reference_catalog(root: &Path) -> Result<ReferenceCatalog, String> {
    let constitution = read_toml(&root.join("registries/constitution.toml"))?;
    let invariants = read_toml(&root.join("registries/invariants.toml"))?;
    let evidence = read_toml(&root.join("registries/evidence.toml"))?;
    let slo = read_toml(&root.join("registries/slo.toml"))?;
    let checker_index = read_toml(&root.join("registries/checker_index.toml"))?;

    let bets = collect_rows(&constitution, "bet", "id", None)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let constraints = collect_rows(&constitution, "constraint", "id", None)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let invariants = collect_rows(&invariants, "invariant", "id", None)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let evidence = collect_rows(&evidence, "evidence", "id", Some("claim_class"))?
        .into_iter()
        .map(|(id, class)| (id, class.unwrap_or_default()))
        .collect();
    let slos = collect_rows(&slo, "slo", "id", Some("claim_class"))?
        .into_iter()
        .map(|(id, class)| (id, class.unwrap_or_default()))
        .collect();

    let mut checkers = BTreeMap::new();
    for (index, row) in get_table_array(&checker_index, "checker", "checker_index")
        .map_err(|error| error.to_string())?
        .iter()
        .enumerate()
    {
        let ctx = format!("checker[{index}]");
        let symbol = get_str(row, "symbol", &ctx).map_err(|error| error.to_string())?;
        let kind = get_str(row, "kind", &ctx).map_err(|error| error.to_string())?;
        let artifact = get_str(row, "artifact", &ctx).map_err(|error| error.to_string())?;
        let status = get_str(row, "status", &ctx).map_err(|error| error.to_string())?;
        if checkers
            .insert(
                symbol.clone(),
                CheckerRecord {
                    kind,
                    artifact,
                    status,
                },
            )
            .is_some()
        {
            return Err(format!(
                "checker_index.toml contains duplicate checker symbol {symbol:?}"
            ));
        }
    }

    let cost_rows = optional_id_registry(
        root,
        &[
            "registries/operation_costs.toml",
            "registries/operation_cost_registry.toml",
            "registries/costs.toml",
        ],
    )?;
    let format_rows = optional_id_registry(
        root,
        &[
            "registries/formats.toml",
            "registries/format_registry.toml",
            "registries/durable_formats.toml",
        ],
    )?;
    Ok(ReferenceCatalog {
        bets,
        constraints,
        invariants,
        evidence,
        slos,
        checkers,
        cost_rows,
        format_rows,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BeadRecord {
    id: String,
    status: String,
    labels: Vec<String>,
}

struct JsonCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> JsonCursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            offset: 0,
        }
    }

    fn skip_whitespace(&mut self) {
        while self
            .bytes
            .get(self.offset)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.offset += 1;
        }
    }

    fn consume(&mut self, expected: u8) -> Result<(), String> {
        self.skip_whitespace();
        match self.bytes.get(self.offset).copied() {
            Some(actual) if actual == expected => {
                self.offset += 1;
                Ok(())
            }
            Some(actual) => Err(format!(
                "expected JSON byte {:?} at offset {}, found {:?}",
                char::from(expected),
                self.offset,
                char::from(actual)
            )),
            None => Err(format!(
                "expected JSON byte {:?} at end of input",
                char::from(expected)
            )),
        }
    }

    fn hex_quad(&mut self) -> Result<u16, String> {
        let end = self
            .offset
            .checked_add(4)
            .ok_or_else(|| "JSON unicode escape offset overflow".to_string())?;
        let digits = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "truncated JSON unicode escape".to_string())?;
        let mut value = 0u16;
        for digit in digits {
            value = value
                .checked_mul(16)
                .and_then(|value| {
                    digit
                        .to_ascii_lowercase()
                        .checked_sub(b'0')
                        .filter(|value| *value <= 9)
                        .or_else(|| {
                            digit
                                .to_ascii_lowercase()
                                .checked_sub(b'a')
                                .filter(|value| *value <= 5)
                                .map(|value| value + 10)
                        })
                        .and_then(|digit| value.checked_add(u16::from(digit)))
                })
                .ok_or_else(|| "invalid JSON unicode escape".to_string())?;
        }
        self.offset = end;
        Ok(value)
    }

    fn string(&mut self) -> Result<String, String> {
        self.consume(b'"')?;
        let mut output = String::new();
        loop {
            let byte = self
                .bytes
                .get(self.offset)
                .copied()
                .ok_or_else(|| "unterminated JSON string".to_string())?;
            match byte {
                b'"' => {
                    self.offset += 1;
                    return Ok(output);
                }
                b'\\' => {
                    self.offset += 1;
                    let escape = self
                        .bytes
                        .get(self.offset)
                        .copied()
                        .ok_or_else(|| "unterminated JSON escape".to_string())?;
                    self.offset += 1;
                    match escape {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{0008}'),
                        b'f' => output.push('\u{000c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => {
                            let first = self.hex_quad()?;
                            let scalar = if (0xd800..=0xdbff).contains(&first) {
                                if self.bytes.get(self.offset..self.offset + 2) != Some(b"\\u") {
                                    return Err(
                                        "high surrogate is not followed by a low surrogate".into(),
                                    );
                                }
                                self.offset += 2;
                                let second = self.hex_quad()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err(
                                        "high surrogate is followed by an invalid low surrogate"
                                            .into(),
                                    );
                                }
                                0x1_0000
                                    + ((u32::from(first) - 0xd800) << 10)
                                    + (u32::from(second) - 0xdc00)
                            } else if (0xdc00..=0xdfff).contains(&first) {
                                return Err("unpaired low surrogate in JSON string".into());
                            } else {
                                u32::from(first)
                            };
                            output.push(
                                char::from_u32(scalar)
                                    .ok_or_else(|| "invalid JSON unicode scalar".to_string())?,
                            );
                        }
                        other => {
                            return Err(format!(
                                "invalid JSON escape \\{} at offset {}",
                                char::from(other),
                                self.offset - 1
                            ));
                        }
                    }
                }
                0x00..=0x1f => {
                    return Err(format!(
                        "unescaped control byte in JSON string at offset {}",
                        self.offset
                    ));
                }
                0x20..=0x7f => {
                    output.push(char::from(byte));
                    self.offset += 1;
                }
                _ => {
                    let rest = std::str::from_utf8(&self.bytes[self.offset..])
                        .map_err(|error| format!("invalid UTF-8 in JSON string: {error}"))?;
                    let character = rest
                        .chars()
                        .next()
                        .ok_or_else(|| "unterminated UTF-8 JSON string".to_string())?;
                    output.push(character);
                    self.offset += character.len_utf8();
                }
            }
        }
    }

    fn string_array(&mut self) -> Result<Vec<String>, String> {
        self.consume(b'[')?;
        let mut values = Vec::new();
        self.skip_whitespace();
        if self.bytes.get(self.offset) == Some(&b']') {
            self.offset += 1;
            return Ok(values);
        }
        loop {
            values.push(self.string()?);
            self.skip_whitespace();
            match self.bytes.get(self.offset) {
                Some(b',') => self.offset += 1,
                Some(b']') => {
                    self.offset += 1;
                    return Ok(values);
                }
                _ => {
                    return Err(format!(
                        "invalid JSON string array at offset {}",
                        self.offset
                    ));
                }
            }
        }
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), String> {
        if self.bytes.get(self.offset..self.offset + literal.len()) == Some(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            Err(format!("invalid JSON literal at offset {}", self.offset))
        }
    }

    fn number(&mut self) -> Result<(), String> {
        let start = self.offset;
        if self.bytes.get(self.offset) == Some(&b'-') {
            self.offset += 1;
        }
        match self.bytes.get(self.offset) {
            Some(b'0') => self.offset += 1,
            Some(b'1'..=b'9') => {
                self.offset += 1;
                while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                    self.offset += 1;
                }
            }
            _ => return Err(format!("invalid JSON number at offset {start}")),
        }
        if self.bytes.get(self.offset) == Some(&b'.') {
            self.offset += 1;
            let fraction = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if self.offset == fraction {
                return Err(format!("empty JSON fraction at offset {fraction}"));
            }
        }
        if self
            .bytes
            .get(self.offset)
            .is_some_and(|byte| matches!(byte, b'e' | b'E'))
        {
            self.offset += 1;
            if self
                .bytes
                .get(self.offset)
                .is_some_and(|byte| matches!(byte, b'+' | b'-'))
            {
                self.offset += 1;
            }
            let exponent = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if self.offset == exponent {
                return Err(format!("empty JSON exponent at offset {exponent}"));
            }
        }
        Ok(())
    }

    fn skip_value(&mut self, depth: usize) -> Result<(), String> {
        if depth > 128 {
            return Err("JSON nesting exceeds 128 levels".into());
        }
        self.skip_whitespace();
        match self.bytes.get(self.offset).copied() {
            Some(b'"') => self.string().map(|_| ()),
            Some(b'{') => {
                self.offset += 1;
                self.skip_whitespace();
                if self.bytes.get(self.offset) == Some(&b'}') {
                    self.offset += 1;
                    return Ok(());
                }
                loop {
                    self.string()?;
                    self.consume(b':')?;
                    self.skip_value(depth + 1)?;
                    self.skip_whitespace();
                    match self.bytes.get(self.offset) {
                        Some(b',') => self.offset += 1,
                        Some(b'}') => {
                            self.offset += 1;
                            return Ok(());
                        }
                        _ => {
                            return Err(format!("invalid JSON object at offset {}", self.offset));
                        }
                    }
                }
            }
            Some(b'[') => {
                self.offset += 1;
                self.skip_whitespace();
                if self.bytes.get(self.offset) == Some(&b']') {
                    self.offset += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value(depth + 1)?;
                    self.skip_whitespace();
                    match self.bytes.get(self.offset) {
                        Some(b',') => self.offset += 1,
                        Some(b']') => {
                            self.offset += 1;
                            return Ok(());
                        }
                        _ => {
                            return Err(format!("invalid JSON array at offset {}", self.offset));
                        }
                    }
                }
            }
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(other) => Err(format!(
                "invalid JSON value byte {:?} at offset {}",
                char::from(other),
                self.offset
            )),
            None => Err("missing JSON value".into()),
        }
    }
}

fn parse_bead_record(line: &str) -> Result<BeadRecord, String> {
    let mut cursor = JsonCursor::new(line);
    cursor.consume(b'{')?;
    let mut seen = BTreeSet::new();
    let mut id = None;
    let mut status = None;
    let mut labels = None;
    cursor.skip_whitespace();
    if cursor.bytes.get(cursor.offset) == Some(&b'}') {
        cursor.offset += 1;
    } else {
        loop {
            let key = cursor.string()?;
            if !seen.insert(key.clone()) {
                return Err(format!("duplicate top-level JSON key {key:?}"));
            }
            cursor.consume(b':')?;
            match key.as_str() {
                "id" => id = Some(cursor.string()?),
                "status" => status = Some(cursor.string()?),
                "labels" => labels = Some(cursor.string_array()?),
                _ => cursor.skip_value(0)?,
            }
            cursor.skip_whitespace();
            match cursor.bytes.get(cursor.offset) {
                Some(b',') => cursor.offset += 1,
                Some(b'}') => {
                    cursor.offset += 1;
                    break;
                }
                _ => {
                    return Err(format!(
                        "invalid top-level JSON object at offset {}",
                        cursor.offset
                    ));
                }
            }
        }
    }
    cursor.skip_whitespace();
    if cursor.offset != cursor.bytes.len() {
        return Err(format!(
            "trailing content after JSON object at offset {}",
            cursor.offset
        ));
    }
    Ok(BeadRecord {
        id: id.ok_or_else(|| "missing top-level string field \"id\"".to_string())?,
        status: status.ok_or_else(|| "missing top-level string field \"status\"".to_string())?,
        labels: labels.unwrap_or_default(),
    })
}

fn load_bead_records(root: &Path, source_path: &str) -> Result<Vec<BeadRecord>, String> {
    if source_path != BEAD_PROVENANCE_SOURCE_PATH || !safe_repo_relative(source_path) {
        return Err(format!(
            "bead provenance source path {source_path:?} is not the pinned safe path {BEAD_PROVENANCE_SOURCE_PATH:?}"
        ));
    }
    let path = root.join(source_path);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut records = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = parse_bead_record(line)
            .map_err(|error| format!("{}:{}: {error}", path.display(), index + 1))?;
        if !ids.insert(record.id.clone()) {
            return Err(format!(
                "{}:{}: duplicate bead id {:?}",
                path.display(),
                index + 1,
                record.id
            ));
        }
        records.push(record);
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(records)
}

fn claim_classes_for(decision: &Decision, catalog: &ReferenceCatalog) -> Vec<String> {
    let mut classes = BTreeSet::new();
    if !decision.affected_invariants.is_empty() {
        classes.insert("invariant".to_string());
    }
    for id in decision
        .affected_evidence
        .iter()
        .chain(decision.evidence_ids.iter())
    {
        if let Some(class) = catalog.evidence.get(id).or_else(|| catalog.slos.get(id))
            && !class.is_empty()
        {
            classes.insert(class.clone());
        }
    }
    for id in &decision.affected_slos {
        if let Some(class) = catalog.slos.get(id)
            && !class.is_empty()
        {
            classes.insert(class.clone());
        }
    }
    if classes.is_empty() {
        classes.insert("architectural_decision".into());
    }
    classes.into_iter().collect()
}

/// Derive the diagnostic claim-class field from live affected claim rows.
pub fn effective_claim_classes(
    registry: &ArchitectureRegistry,
    root: &Path,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let catalog = load_reference_catalog(root)?;
    Ok(registry
        .decisions
        .iter()
        .map(|decision| (decision.id.clone(), claim_classes_for(decision, &catalog)))
        .collect())
}

/// Deterministic reverse edge for explicit decision-owner declarations.  The
/// total implementation-bead mapping is exposed separately by
/// [`bead_provenance_index`].
pub fn owner_decision_index(registry: &ArchitectureRegistry) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for decision in &registry.decisions {
        if decision.status == "superseded" {
            continue;
        }
        for bead in &decision.owner_beads {
            index
                .entry(bead.clone())
                .or_default()
                .push(decision.id.clone());
        }
    }
    for decision_ids in index.values_mut() {
        decision_ids.sort();
        decision_ids.dedup();
    }
    index
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceIndexEntry {
    pub owner_kind: String,
    pub owner_id: String,
    pub decision_ids: Vec<String>,
    pub profile_ids: Vec<String>,
    pub rationales: Vec<String>,
}

type ProvenanceSets = (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>);

/// Generalized reciprocal provenance walk for every explicitly named owner
/// class.  Total Beads coverage additionally applies the registry-backed
/// label, override, and family rules in [`bead_provenance_index`].
pub fn provenance_index(registry: &ArchitectureRegistry) -> Vec<ProvenanceIndexEntry> {
    let profiles: BTreeMap<&str, &Profile> = registry
        .profiles
        .iter()
        .map(|profile| (profile.id.as_str(), profile))
        .collect();
    let mut index: BTreeMap<(String, String), ProvenanceSets> = BTreeMap::new();
    for decision in registry
        .decisions
        .iter()
        .filter(|decision| decision.status != "superseded")
    {
        for (owner_kind, owner_ids) in [
            ("bead", decision.owner_beads.as_slice()),
            ("crate", decision.owner_crates.as_slice()),
            ("checker", decision.checker_ids.as_slice()),
            ("evidence", decision.evidence_ids.as_slice()),
        ] {
            for owner_id in owner_ids {
                let entry = index
                    .entry((owner_kind.into(), owner_id.clone()))
                    .or_default();
                entry.0.insert(decision.id.clone());
                entry.1.insert(decision.profile.clone());
                if let Some(profile) = profiles.get(decision.profile.as_str()) {
                    entry.2.insert(profile.rationale.clone());
                }
            }
        }
    }
    index
        .into_iter()
        .map(
            |((owner_kind, owner_id), (decision_ids, profile_ids, rationales))| {
                ProvenanceIndexEntry {
                    owner_kind,
                    owner_id,
                    decision_ids: decision_ids.into_iter().collect(),
                    profile_ids: profile_ids.into_iter().collect(),
                    rationales: rationales.into_iter().collect(),
                }
            },
        )
        .collect()
}

/// Total, deterministic Beads-to-ADR provenance row.  The descriptive fields
/// are derived from the resolved decision/profile rows so robot consumers can
/// walk both directions without scraping prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadProvenanceEntry {
    pub bead_id: String,
    pub status: String,
    pub resolution_class: String,
    pub rule_id: String,
    pub decision_ids: Vec<String>,
    pub profile_ids: Vec<String>,
    pub summaries: Vec<String>,
    pub rationales: Vec<String>,
    pub source_anchors: Vec<String>,
    pub replay_commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BeadResolutionIssue {
    code: String,
    bead_id: String,
    message: String,
}

#[derive(Default)]
struct BeadResolution {
    entries: Vec<BeadProvenanceEntry>,
    issues: Vec<BeadResolutionIssue>,
    class_counts: BTreeMap<String, usize>,
    family_counts: BTreeMap<String, usize>,
}

fn appendix_a_bead(id: &str) -> bool {
    if id.starts_with("fgdb-appendix-a-catalog-") {
        return true;
    }
    let Some(suffix) = id.strip_prefix("fgdb-a") else {
        return false;
    };
    let Some(number) = suffix.get(0..2) else {
        return false;
    };
    suffix.as_bytes().get(2) == Some(&b'-')
        && number
            .parse::<u8>()
            .is_ok_and(|number| (1..=21).contains(&number))
}

fn family_matches(family: &BeadFamily, bead_id: &str) -> bool {
    match family.match_kind.as_str() {
        "prefix" => bead_id.starts_with(&family.pattern),
        "appendix_a" => {
            family.pattern == "fgdb-a01..a21|fgdb-appendix-a-catalog-" && appendix_a_bead(bead_id)
        }
        _ => false,
    }
}

fn looks_like_bet_label(label: &str) -> bool {
    label.strip_prefix('b').is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn provenance_entry(
    bead: &BeadRecord,
    resolution_class: &str,
    rule_id: String,
    decision_ids: BTreeSet<String>,
    decisions: &BTreeMap<&str, &Decision>,
    profiles: &BTreeMap<&str, &Profile>,
) -> BeadProvenanceEntry {
    let mut profile_ids = BTreeSet::new();
    let mut summaries = BTreeSet::new();
    let mut rationales = BTreeSet::new();
    let mut source_anchors = BTreeSet::new();
    let mut replay_commands = BTreeSet::new();
    for decision_id in &decision_ids {
        let Some(decision) = decisions.get(decision_id.as_str()) else {
            continue;
        };
        profile_ids.insert(decision.profile.clone());
        summaries.insert(decision.summary.clone());
        source_anchors.insert(decision.source_anchor.clone());
        if let Some(profile) = profiles.get(decision.profile.as_str()) {
            rationales.insert(profile.rationale.clone());
            replay_commands.insert(profile.check_command.clone());
        }
    }
    BeadProvenanceEntry {
        bead_id: bead.id.clone(),
        status: bead.status.clone(),
        resolution_class: resolution_class.into(),
        rule_id,
        decision_ids: decision_ids.into_iter().collect(),
        profile_ids: profile_ids.into_iter().collect(),
        summaries: summaries.into_iter().collect(),
        rationales: rationales.into_iter().collect(),
        source_anchors: source_anchors.into_iter().collect(),
        replay_commands: replay_commands.into_iter().collect(),
    }
}

fn resolve_bead_records(registry: &ArchitectureRegistry, beads: &[BeadRecord]) -> BeadResolution {
    let direct = owner_decision_index(registry);
    let decisions: BTreeMap<&str, &Decision> = registry
        .decisions
        .iter()
        .map(|decision| (decision.id.as_str(), decision))
        .collect();
    let profiles: BTreeMap<&str, &Profile> = registry
        .profiles
        .iter()
        .map(|profile| (profile.id.as_str(), profile))
        .collect();
    let overrides: BTreeMap<&str, &BeadOverride> = registry
        .bead_overrides
        .iter()
        .map(|rule| (rule.bead_id.as_str(), rule))
        .collect();
    let bead_ids: BTreeSet<&str> = beads.iter().map(|bead| bead.id.as_str()).collect();
    let mut result = BeadResolution::default();

    for rule in &registry.bead_overrides {
        if !bead_ids.contains(rule.bead_id.as_str()) {
            result.issues.push(BeadResolutionIssue {
                code: "bead_override_target_missing".into(),
                bead_id: rule.bead_id.clone(),
                message: format!(
                    "override {:?} targets absent bead {:?}",
                    rule.id, rule.bead_id
                ),
            });
        }
    }

    for bead in beads {
        let mut allowed_labels: Vec<String> = bead
            .labels
            .iter()
            .filter(|label| ALLOWED_BET_LABELS.contains(&label.as_str()))
            .cloned()
            .collect();
        allowed_labels.sort();
        allowed_labels.dedup();
        for label in &bead.labels {
            if looks_like_bet_label(label) && !ALLOWED_BET_LABELS.contains(&label.as_str()) {
                result.issues.push(BeadResolutionIssue {
                    code: "bead_bet_label_unknown".into(),
                    bead_id: bead.id.clone(),
                    message: format!("unknown architecture bet label {label:?}"),
                });
            }
        }

        let direct_ids = direct.get(&bead.id);
        let override_rule = overrides.get(bead.id.as_str()).copied();
        if override_rule.is_some() && (direct_ids.is_some() || !allowed_labels.is_empty()) {
            result.issues.push(BeadResolutionIssue {
                code: "bead_override_shadowed".into(),
                bead_id: bead.id.clone(),
                message: format!(
                    "override {:?} is shadowed by higher-precedence {}",
                    override_rule.map(|rule| rule.id.as_str()).unwrap_or(""),
                    if direct_ids.is_some() {
                        "direct_owner"
                    } else {
                        "bet_label"
                    }
                ),
            });
        }

        let (resolution_class, rule_id, decision_ids): (&str, String, BTreeSet<String>) =
            if let Some(ids) = direct_ids {
                (
                    "direct_owner",
                    "direct_owner".to_string(),
                    ids.iter().cloned().collect(),
                )
            } else if !allowed_labels.is_empty() {
                let decision_ids = allowed_labels
                    .iter()
                    .map(|label| format!("FG-ADR-BET-B{}", &label[1..]))
                    .collect();
                (
                    "bet_label",
                    format!("label:{}", allowed_labels.join("+")),
                    decision_ids,
                )
            } else if let Some(rule) = override_rule {
                (
                    "exact_override",
                    rule.id.clone(),
                    rule.decision_ids.iter().cloned().collect(),
                )
            } else {
                let matches: Vec<&BeadFamily> = registry
                    .bead_families
                    .iter()
                    .filter(|family| family_matches(family, &bead.id))
                    .collect();
                match matches.as_slice() {
                    [family] => (
                        "family_rule",
                        family.id.clone(),
                        family.decision_ids.iter().cloned().collect(),
                    ),
                    [] => {
                        result.issues.push(BeadResolutionIssue {
                        code: "bead_provenance_orphan".into(),
                        bead_id: bead.id.clone(),
                        message:
                            "bead has no direct owner, bet label, exact override, or family rule"
                                .into(),
                    });
                        continue;
                    }
                    _ => {
                        result.issues.push(BeadResolutionIssue {
                            code: "bead_family_ambiguous".into(),
                            bead_id: bead.id.clone(),
                            message: format!(
                                "bead matches multiple family rules {:?}",
                                matches
                                    .iter()
                                    .map(|family| family.id.as_str())
                                    .collect::<Vec<_>>()
                            ),
                        });
                        continue;
                    }
                }
            };

        for decision_id in &decision_ids {
            match decisions.get(decision_id.as_str()) {
                None => result.issues.push(BeadResolutionIssue {
                    code: "bead_provenance_decision_unresolved".into(),
                    bead_id: bead.id.clone(),
                    message: format!(
                        "{resolution_class} rule {rule_id:?} targets unknown decision {decision_id:?}"
                    ),
                }),
                Some(decision) if decision.status == "superseded" => {
                    result.issues.push(BeadResolutionIssue {
                        code: "bead_provenance_decision_superseded".into(),
                        bead_id: bead.id.clone(),
                        message: format!(
                            "{resolution_class} rule {rule_id:?} targets superseded decision {decision_id:?}"
                        ),
                    });
                }
                Some(_) => {}
            }
        }
        *result
            .class_counts
            .entry(resolution_class.into())
            .or_default() += 1;
        if resolution_class == "family_rule" {
            *result.family_counts.entry(rule_id.clone()).or_default() += 1;
        }
        result.entries.push(provenance_entry(
            bead,
            resolution_class,
            rule_id,
            decision_ids,
            &decisions,
            &profiles,
        ));
    }
    result
        .entries
        .sort_by(|left, right| left.bead_id.cmp(&right.bead_id));
    result.issues.sort_by(|left, right| {
        (&left.bead_id, &left.code, &left.message).cmp(&(
            &right.bead_id,
            &right.code,
            &right.message,
        ))
    });
    result
}

/// Resolve the complete Beads issue set using the declared precedence.  A
/// malformed JSONL row, orphan, ambiguity, shadowed override, or unresolved
/// decision target is returned as an error rather than silently omitted.
pub fn resolve_bead_provenance(
    registry: &ArchitectureRegistry,
    root: &Path,
) -> Result<Vec<BeadProvenanceEntry>, String> {
    let beads = load_bead_records(root, &registry.bead_provenance.source_path)?;
    let resolution = resolve_bead_records(registry, &beads);
    if resolution.issues.is_empty() {
        Ok(resolution.entries)
    } else {
        Err(resolution
            .issues
            .into_iter()
            .map(|issue| format!("{} {}: {}", issue.code, issue.bead_id, issue.message))
            .collect::<Vec<_>>()
            .join("; "))
    }
}

/// Public total provenance index named for robot/API consumers.
pub fn bead_provenance_index(
    registry: &ArchitectureRegistry,
    root: &Path,
) -> Result<Vec<BeadProvenanceEntry>, String> {
    resolve_bead_provenance(registry, root)
}

/// Canonical pin over total Beads-to-ADR resolution.  Bead lifecycle status
/// and derived prose are intentionally excluded; the stable binding identity,
/// rule, and sorted ADR targets are included.
pub fn recompute_bead_binding_hash(entries: &[BeadProvenanceEntry]) -> String {
    let mut entries: Vec<&BeadProvenanceEntry> = entries.iter().collect();
    entries.sort_by(|left, right| left.bead_id.cmp(&right.bead_id));
    let mut transcript = Vec::new();
    for entry in entries {
        transcript_field(&mut transcript, "bead.id", &entry.bead_id);
        transcript_field(
            &mut transcript,
            "bead.resolution_class",
            &entry.resolution_class,
        );
        transcript_field(&mut transcript, "bead.rule_id", &entry.rule_id);
        transcript_array(&mut transcript, "bead.decision_ids", &entry.decision_ids);
    }
    format!("fnv1a64:{:016x}", fnv1a64(&transcript))
}

fn valid_stable_id(id: &str, prefix: &str) -> bool {
    let Some(suffix) = id.strip_prefix(prefix) else {
        return false;
    };
    !suffix.is_empty()
        && !suffix.starts_with('-')
        && !suffix.ends_with('-')
        && !suffix.contains("--")
        && suffix.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '-'
        })
}

fn valid_stable_key(key: &str) -> bool {
    let mut characters = key.chars();
    matches!(characters.next(), Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit())
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_' | '.')
        })
}

fn valid_workstream(workstream: &str) -> bool {
    matches!(
        workstream,
        "G0" | "Verification" | "Performance" | "Cross-cutting"
    ) || workstream
        .strip_prefix('W')
        .and_then(|suffix| suffix.parse::<u8>().ok())
        .is_some_and(|number| (1..=12).contains(&number))
}

fn valid_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let parse = |range: std::ops::Range<usize>| -> Option<u32> {
        std::str::from_utf8(&bytes[range]).ok()?.parse().ok()
    };
    let Some(year) = parse(0..4) else {
        return false;
    };
    let Some(month) = parse(5..7) else {
        return false;
    };
    let Some(day) = parse(8..10) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

fn valid_review_after(value: &str) -> bool {
    valid_iso_date(value)
        || ["on-", "before-", "after-", "at-"].iter().any(|prefix| {
            value.strip_prefix(prefix).is_some_and(|suffix| {
                !suffix.is_empty()
                    && suffix.chars().all(|character| {
                        character.is_ascii_lowercase()
                            || character.is_ascii_digit()
                            || character == '-'
                    })
            })
        })
}

fn valid_verification_entrypoint(value: &str) -> bool {
    ["cargo-test:", "cargo-check:", "e2e:"]
        .iter()
        .any(|prefix| {
            value.strip_prefix(prefix).is_some_and(|label| {
                !label.is_empty()
                    && label.chars().all(|character| {
                        character.is_ascii_alphanumeric()
                            || matches!(character, '-' | '_' | ':' | '.')
                    })
            })
        })
}

fn verification_entrypoint_parts(value: &str) -> Option<(&str, &str)> {
    let (scheme, label) = value.split_once(':')?;
    valid_verification_entrypoint(value).then_some((scheme, label))
}

fn checker_kind_for_scheme(scheme: &str) -> Option<&'static str> {
    match scheme {
        "cargo-test" => Some("cargo-test"),
        "cargo-check" => Some("binary"),
        "e2e" => Some("script"),
        _ => None,
    }
}

fn valid_lower_hex(value: &str, digits: usize) -> bool {
    value.len() == digits
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_fnv_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("fnv1a64:")
        .is_some_and(|hex| valid_lower_hex(hex, 16))
}

fn valid_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| valid_lower_hex(hex, 64))
}

fn valid_external_source_uri(value: &str) -> bool {
    value.strip_prefix("https://").is_some_and(|rest| {
        !rest.is_empty() && !value.bytes().any(|byte| byte.is_ascii_whitespace())
    })
}

fn requires_external_review(decision: &Decision) -> bool {
    decision.status != "superseded"
        && (decision.category.starts_with("foundation_") || decision.category.starts_with("sota_"))
}

fn sorted_equal(actual: &[String], expected: &[&str]) -> bool {
    let actual: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
    let expected: BTreeSet<&str> = expected.iter().copied().collect();
    actual == expected
}

fn validate_string_array(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    field: &str,
    values: &[String],
    violations: &mut Vec<Violation>,
) {
    let blank = blank_items(values);
    if !blank.is_empty() {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "blank_array_item",
            "schema",
            format!("{field} contains blank item indexes {blank:?}"),
        ));
    }
    let duplicate = duplicates(values);
    if !duplicate.is_empty() {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "duplicate_array_item",
            "schema",
            format!("{field} contains duplicates {duplicate:?}"),
        ));
    }
}

fn validate_reference_set(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    field: &str,
    values: &[String],
    live: &BTreeSet<String>,
    violations: &mut Vec<Violation>,
) {
    for value in values {
        if !live.contains(value) {
            violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "unresolved_reference",
                "reference_closure",
                format!("{field} reference {value:?} does not resolve in its live registry"),
            ));
        }
    }
}

fn validate_reference_map(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    field: &str,
    values: &[String],
    live: &BTreeMap<String, String>,
    violations: &mut Vec<Violation>,
) {
    for value in values {
        if !live.contains_key(value) {
            violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "unresolved_reference",
                "reference_closure",
                format!("{field} reference {value:?} does not resolve in its live registry"),
            ));
        }
    }
}

fn validate_reserved_or_live(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    field: &str,
    values: &[String],
    live: Option<&BTreeSet<String>>,
    violations: &mut Vec<Violation>,
) {
    for value in values {
        if let Some(reserved) = value.strip_prefix("reserved:") {
            if reserved.trim().is_empty() {
                violations.push(Violation::for_decision(
                    decision,
                    profiles,
                    claim_class,
                    "empty_reserved_reference",
                    "reference_closure",
                    format!("{field} contains an empty reserved reference"),
                ));
            }
            continue;
        }
        match live {
            Some(ids) if ids.contains(value) => {}
            Some(_) => violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "unresolved_reference",
                "reference_closure",
                format!("{field} reference {value:?} does not resolve"),
            )),
            None => violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "unreserved_future_reference",
                "reference_closure",
                format!(
                    "{field} reference {value:?} targets a registry not yet live; prefix it with reserved:"
                ),
            )),
        }
    }
}

fn validate_bead_policy_shape(registry: &ArchitectureRegistry, violations: &mut Vec<Violation>) {
    let policy = &registry.bead_provenance;
    if policy.source_path != BEAD_PROVENANCE_SOURCE_PATH || !safe_repo_relative(&policy.source_path)
    {
        violations.push(Violation::global(
            "bead_source_path",
            "bead_provenance",
            format!(
                "bead provenance source_path {:?} must equal the safe pinned path {BEAD_PROVENANCE_SOURCE_PATH:?}",
                policy.source_path
            ),
        ));
    }
    if policy.resolution_precedence != BEAD_RESOLUTION_PRECEDENCE {
        violations.push(Violation::global(
            "bead_resolution_precedence",
            "bead_provenance",
            format!(
                "resolution_precedence {:?} must equal {:?}",
                policy.resolution_precedence, BEAD_RESOLUTION_PRECEDENCE
            ),
        ));
    }
    if policy.allowed_bet_labels != ALLOWED_BET_LABELS {
        violations.push(Violation::global(
            "bead_bet_label_set",
            "bead_provenance",
            format!(
                "allowed_bet_labels {:?} must equal {:?}",
                policy.allowed_bet_labels, ALLOWED_BET_LABELS
            ),
        ));
    }
    for (field, actual, expected) in [
        ("bead_count", policy.bead_count, PINNED_BEAD_COUNT),
        (
            "direct_owner_count",
            policy.direct_owner_count,
            PINNED_DIRECT_OWNER_COUNT,
        ),
        (
            "bet_label_count",
            policy.bet_label_count,
            PINNED_BET_LABEL_COUNT,
        ),
        (
            "exact_override_count",
            policy.exact_override_count,
            PINNED_EXACT_OVERRIDE_COUNT,
        ),
        (
            "family_rule_count",
            policy.family_rule_count,
            PINNED_FAMILY_RULE_COUNT,
        ),
    ] {
        if actual != expected {
            violations.push(Violation::global(
                "bead_count_pin",
                "bead_provenance",
                format!("{field} is {actual}, independently pinned value is {expected}"),
            ));
        }
    }
    let declared_total = policy
        .direct_owner_count
        .saturating_add(policy.bet_label_count)
        .saturating_add(policy.exact_override_count)
        .saturating_add(policy.family_rule_count);
    if declared_total != policy.bead_count {
        violations.push(Violation::global(
            "bead_class_count_total",
            "bead_provenance",
            format!(
                "declared resolution class counts sum to {declared_total}, bead_count is {}",
                policy.bead_count
            ),
        ));
    }
    if registry.bead_families.len() != PINNED_BEAD_FAMILY_TABLE_COUNT {
        violations.push(Violation::global(
            "bead_family_table_count",
            "bead_provenance",
            format!(
                "bead_family has {} rows, pinned row count is {PINNED_BEAD_FAMILY_TABLE_COUNT}",
                registry.bead_families.len()
            ),
        ));
    }
    if registry.bead_overrides.len() != PINNED_BEAD_OVERRIDE_TABLE_COUNT {
        violations.push(Violation::global(
            "bead_override_table_count",
            "bead_provenance",
            format!(
                "bead_override has {} rows, pinned row count is {PINNED_BEAD_OVERRIDE_TABLE_COUNT}",
                registry.bead_overrides.len()
            ),
        ));
    }

    let decisions: BTreeMap<&str, &Decision> = registry
        .decisions
        .iter()
        .map(|decision| (decision.id.as_str(), decision))
        .collect();
    let mut family_ids = BTreeSet::new();
    let mut family_patterns = BTreeSet::new();
    let mut expected_family_total = 0usize;
    for family in &registry.bead_families {
        if family.id.trim().is_empty() || !family_ids.insert(family.id.as_str()) {
            violations.push(Violation::global(
                "bead_family_id",
                "bead_provenance",
                format!("bead family ID {:?} is blank or duplicated", family.id),
            ));
        }
        if family.pattern.trim().is_empty() || !family_patterns.insert(family.pattern.as_str()) {
            violations.push(Violation::global(
                "bead_family_pattern",
                "bead_provenance",
                format!(
                    "bead family pattern {:?} is blank or duplicated",
                    family.pattern
                ),
            ));
        }
        if !ALLOWED_FAMILY_MATCH_KINDS.contains(&family.match_kind.as_str()) {
            violations.push(Violation::global(
                "bead_family_match_kind",
                "bead_provenance",
                format!(
                    "bead family {:?} match_kind {:?} is outside the closed enum",
                    family.id, family.match_kind
                ),
            ));
        }
        match family.match_kind.as_str() {
            "prefix" if !family.pattern.starts_with("fgdb-") || family.pattern.contains('|') => {
                violations.push(Violation::global(
                    "bead_family_prefix",
                    "bead_provenance",
                    format!(
                        "prefix family {:?} must use one literal fgdb- prefix",
                        family.id
                    ),
                ));
            }
            "appendix_a" if family.pattern != "fgdb-a01..a21|fgdb-appendix-a-catalog-" => {
                violations.push(Violation::global(
                    "bead_family_appendix_pattern",
                    "bead_provenance",
                    format!(
                        "appendix_a family {:?} must use the exact catalog sentinel",
                        family.id
                    ),
                ));
            }
            _ => {}
        }
        expected_family_total = expected_family_total.saturating_add(family.expected_match_count);
        if family.decision_ids.is_empty()
            || !blank_items(&family.decision_ids).is_empty()
            || !duplicates(&family.decision_ids).is_empty()
        {
            violations.push(Violation::global(
                "bead_family_decision_ids",
                "bead_provenance",
                format!(
                    "bead family {:?} requires nonblank unique decision_ids",
                    family.id
                ),
            ));
        }
        for decision_id in &family.decision_ids {
            match decisions.get(decision_id.as_str()) {
                None => violations.push(Violation::global(
                    "bead_rule_decision_unresolved",
                    "bead_provenance",
                    format!(
                        "bead family {:?} targets unknown decision {decision_id:?}",
                        family.id
                    ),
                )),
                Some(decision) if decision.status == "superseded" => {
                    violations.push(Violation::global(
                        "bead_rule_decision_superseded",
                        "bead_provenance",
                        format!(
                            "bead family {:?} targets superseded decision {decision_id:?}",
                            family.id
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
    }
    if expected_family_total != policy.family_rule_count {
        violations.push(Violation::global(
            "bead_family_expected_total",
            "bead_provenance",
            format!(
                "bead_family expected_match_count values sum to {expected_family_total}, declared family_rule_count is {}",
                policy.family_rule_count
            ),
        ));
    }

    let mut override_ids = BTreeSet::new();
    let mut override_beads = BTreeSet::new();
    for rule in &registry.bead_overrides {
        if rule.id.trim().is_empty() || !override_ids.insert(rule.id.as_str()) {
            violations.push(Violation::global(
                "bead_override_id",
                "bead_provenance",
                format!("bead override ID {:?} is blank or duplicated", rule.id),
            ));
        }
        if !rule.bead_id.starts_with("fgdb-") || !override_beads.insert(rule.bead_id.as_str()) {
            violations.push(Violation::for_bead(
                "bead_override_bead_id",
                &rule.bead_id,
                format!(
                    "override {:?} has a malformed or duplicate bead_id {:?}",
                    rule.id, rule.bead_id
                ),
            ));
        }
        if rule.decision_ids.is_empty()
            || !blank_items(&rule.decision_ids).is_empty()
            || !duplicates(&rule.decision_ids).is_empty()
        {
            violations.push(Violation::for_bead(
                "bead_override_decision_ids",
                &rule.bead_id,
                format!(
                    "bead override {:?} requires nonblank unique decision_ids",
                    rule.id
                ),
            ));
        }
        for decision_id in &rule.decision_ids {
            match decisions.get(decision_id.as_str()) {
                None => violations.push(Violation::for_bead(
                    "bead_rule_decision_unresolved",
                    &rule.bead_id,
                    format!(
                        "bead override {:?} targets unknown decision {decision_id:?}",
                        rule.id
                    ),
                )),
                Some(decision) if decision.status == "superseded" => {
                    violations.push(Violation::for_bead(
                        "bead_rule_decision_superseded",
                        &rule.bead_id,
                        format!(
                            "bead override {:?} targets superseded decision {decision_id:?}",
                            rule.id
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
    }
}

fn validate_bead_resolution(
    registry: &ArchitectureRegistry,
    beads: &[BeadRecord],
    violations: &mut Vec<Violation>,
) {
    let resolution = resolve_bead_records(registry, beads);
    for issue in resolution.issues {
        violations.push(Violation::for_bead(
            issue.code,
            issue.bead_id,
            issue.message,
        ));
    }
    if beads.len() != registry.bead_provenance.bead_count || beads.len() != PINNED_BEAD_COUNT {
        violations.push(Violation::global(
            "bead_source_count",
            "bead_provenance",
            format!(
                "{} has {} distinct records, declared count is {}, independently pinned count is {PINNED_BEAD_COUNT}",
                registry.bead_provenance.source_path,
                beads.len(),
                registry.bead_provenance.bead_count
            ),
        ));
    }
    if resolution.entries.len() != beads.len() {
        violations.push(Violation::global(
            "bead_provenance_not_total",
            "bead_provenance",
            format!(
                "resolved {} of {} Beads records",
                resolution.entries.len(),
                beads.len()
            ),
        ));
    }
    for (class, declared, pinned) in [
        (
            "direct_owner",
            registry.bead_provenance.direct_owner_count,
            PINNED_DIRECT_OWNER_COUNT,
        ),
        (
            "bet_label",
            registry.bead_provenance.bet_label_count,
            PINNED_BET_LABEL_COUNT,
        ),
        (
            "exact_override",
            registry.bead_provenance.exact_override_count,
            PINNED_EXACT_OVERRIDE_COUNT,
        ),
        (
            "family_rule",
            registry.bead_provenance.family_rule_count,
            PINNED_FAMILY_RULE_COUNT,
        ),
    ] {
        let actual = resolution.class_counts.get(class).copied().unwrap_or(0);
        if actual != declared || actual != pinned {
            violations.push(Violation::global(
                "bead_resolution_class_count",
                "bead_provenance",
                format!(
                    "resolution class {class:?} has {actual} rows, declared {declared}, pinned {pinned}"
                ),
            ));
        }
    }
    for family in &registry.bead_families {
        let actual = resolution
            .family_counts
            .get(&family.id)
            .copied()
            .unwrap_or(0);
        if actual != family.expected_match_count {
            violations.push(Violation::global(
                "bead_family_match_count",
                "bead_provenance",
                format!(
                    "family {:?} selected {actual} beads, expected {}",
                    family.id, family.expected_match_count
                ),
            ));
        }
    }
    let binding_hash = recompute_bead_binding_hash(&resolution.entries);
    if registry.bead_provenance.binding_hash != binding_hash {
        violations.push(Violation::global(
            "bead_binding_hash_mismatch",
            "bead_provenance",
            format!(
                "registry binding hash {:?}, recomputed {binding_hash:?}",
                registry.bead_provenance.binding_hash
            ),
        ));
    }
    if binding_hash != PINNED_BEAD_BINDING_HASH {
        violations.push(Violation::global(
            "independent_bead_binding_hash_mismatch",
            "bead_provenance",
            format!(
                "recomputed binding hash {binding_hash:?} differs from code pin {PINNED_BEAD_BINDING_HASH:?}"
            ),
        ));
    }
}

fn validate_header(registry: &ArchitectureRegistry, violations: &mut Vec<Violation>) {
    let header = &registry.registry;
    if registry.schema_version != SCHEMA_VERSION {
        violations.push(Violation::global(
            "schema_version",
            "schema",
            format!(
                "schema_version is {}, expected {SCHEMA_VERSION}",
                registry.schema_version
            ),
        ));
    }
    for (code, field, actual, expected) in [
        (
            "registry_name",
            "registry.name",
            header.name.as_str(),
            REGISTRY_NAME,
        ),
        (
            "decision_id_prefix",
            "registry.decision_id_prefix",
            header.decision_id_prefix.as_str(),
            DECISION_ID_PREFIX,
        ),
        (
            "ownership_scope",
            "registry.ownership_scope",
            header.ownership_scope.as_str(),
            OWNERSHIP_SCOPE,
        ),
    ] {
        if actual != expected {
            violations.push(Violation::global(
                code,
                "schema",
                format!("{field} is {actual:?}, expected {expected:?}"),
            ));
        }
    }
    for (field, values, expected) in [
        (
            "allowed_categories",
            header.allowed_categories.as_slice(),
            ALLOWED_CATEGORIES.as_slice(),
        ),
        (
            "allowed_dispositions",
            header.allowed_dispositions.as_slice(),
            ALLOWED_DISPOSITIONS.as_slice(),
        ),
        (
            "allowed_relationship_kinds",
            header.allowed_relationship_kinds.as_slice(),
            ALLOWED_RELATIONSHIP_KINDS.as_slice(),
        ),
        (
            "allowed_statuses",
            header.allowed_statuses.as_slice(),
            ALLOWED_STATUSES.as_slice(),
        ),
        (
            "required_source_blocks",
            header.required_source_blocks.as_slice(),
            REQUIRED_SOURCE_BLOCKS.as_slice(),
        ),
    ] {
        if !sorted_equal(values, expected) || !duplicates(values).is_empty() {
            violations.push(Violation::global(
                "closed_set_mismatch",
                "schema",
                format!("registry.{field} is not the exact required closed set"),
            ));
        }
    }
    let duplicate_crates = duplicates(&header.planned_crates);
    if !duplicate_crates.is_empty() {
        violations.push(Violation::global(
            "planned_crates_duplicate",
            "owner_closure",
            format!("planned_crates contains duplicates {duplicate_crates:?}"),
        ));
    }
    for planned_crate in &header.planned_crates {
        if planned_crate != "fgdb"
            && !planned_crate
                .strip_prefix("fgdb-")
                .is_some_and(|suffix| !suffix.is_empty())
        {
            violations.push(Violation::global(
                "planned_crate_name",
                "owner_closure",
                format!("planned crate {planned_crate:?} is outside the fgdb crate universe"),
            ));
        }
    }
    let actual_crates: BTreeSet<&str> = header.planned_crates.iter().map(String::as_str).collect();
    let expected_crates: BTreeSet<&str> = PLANNED_CRATES.iter().copied().collect();
    if actual_crates != expected_crates {
        let missing: Vec<&str> = expected_crates
            .difference(&actual_crates)
            .copied()
            .collect();
        let extra: Vec<&str> = actual_crates
            .difference(&expected_crates)
            .copied()
            .collect();
        violations.push(Violation::global(
            "planned_crates_pin",
            "owner_closure",
            format!("planned_crates differs from plan §18.1; missing={missing:?}, extra={extra:?}"),
        ));
    }
}

fn validate_category_counts(registry: &ArchitectureRegistry, violations: &mut Vec<Violation>) {
    let mut declared = BTreeMap::new();
    for row in &registry.category_counts {
        if declared.insert(row.category.clone(), row.count).is_some() {
            violations.push(Violation::global(
                "category_count_duplicate",
                "count_pin",
                format!("duplicate category_count row for {:?}", row.category),
            ));
        }
    }
    let mut actual = BTreeMap::<String, usize>::new();
    for decision in &registry.decisions {
        *actual.entry(decision.category.clone()).or_default() += 1;
    }
    for category in ALLOWED_CATEGORIES {
        let actual_count = actual.get(category).copied().unwrap_or(0);
        match declared.get(category) {
            Some(declared_count) if *declared_count == actual_count => {}
            Some(declared_count) => violations.push(Violation::global(
                "category_count_mismatch",
                "count_pin",
                format!(
                    "category {category:?} declares {declared_count} rows but has {actual_count}"
                ),
            )),
            None => violations.push(Violation::global(
                "category_count_missing",
                "count_pin",
                format!("category {category:?} has no category_count row"),
            )),
        }
    }
    for (category, expected) in PINNED_CATEGORY_COUNTS {
        let actual_count = actual.get(category).copied().unwrap_or(0);
        if actual_count != expected {
            violations.push(Violation::global(
                "pinned_category_count",
                "count_pin",
                format!(
                    "category {category:?} has {actual_count} rows, pinned count is {expected}"
                ),
            ));
        }
    }
    let allowed = set_of(&ALLOWED_CATEGORIES);
    for category in declared.keys().chain(actual.keys()) {
        if !allowed.contains(category) {
            violations.push(Violation::global(
                "unknown_category_count",
                "schema",
                format!("category count references unknown category {category:?}"),
            ));
        }
    }
    if registry.registry.decision_count != registry.decisions.len() {
        violations.push(Violation::global(
            "decision_count_mismatch",
            "count_pin",
            format!(
                "registry.decision_count is {}, actual rows are {}",
                registry.registry.decision_count,
                registry.decisions.len()
            ),
        ));
    }
    if registry.decisions.len() != PINNED_DECISION_COUNT {
        violations.push(Violation::global(
            "pinned_decision_count",
            "count_pin",
            format!(
                "decision table has {} rows, independently pinned count is {PINNED_DECISION_COUNT}",
                registry.decisions.len()
            ),
        ));
    }
    let bibliography_count = actual.get("bibliography").copied().unwrap_or(0);
    if bibliography_count != PINNED_BIBLIOGRAPHY_COUNT {
        violations.push(Violation::global(
            "pinned_bibliography_count",
            "count_pin",
            format!(
                "bibliography has {bibliography_count} normalized rows, independently pinned count is {PINNED_BIBLIOGRAPHY_COUNT}"
            ),
        ));
    }
}

fn transcript_field(transcript: &mut Vec<u8>, name: &str, value: &str) {
    transcript.extend_from_slice(name.as_bytes());
    transcript.push(b'=');
    transcript.extend_from_slice(value.len().to_string().as_bytes());
    transcript.push(b':');
    transcript.extend_from_slice(value.as_bytes());
    transcript.push(b'\n');
}

fn transcript_array(transcript: &mut Vec<u8>, name: &str, values: &[String]) {
    let mut values = values.to_vec();
    values.sort();
    transcript_field(
        transcript,
        &format!("{name}.len"),
        &values.len().to_string(),
    );
    for value in values {
        transcript_field(transcript, name, &value);
    }
}

/// Fingerprint the exact immutable metadata for one external source record.
pub fn recompute_external_review_source_fingerprint(source: &ExternalReviewSource) -> String {
    let mut transcript = Vec::new();
    for (name, value) in [
        ("external_review_source.id", source.id.as_str()),
        ("external_review_source.uri", source.uri.as_str()),
        (
            "external_review_source.published_at",
            source.published_at.as_str(),
        ),
        (
            "external_review_source.retrieved_at",
            source.retrieved_at.as_str(),
        ),
        (
            "external_review_source.content_digest",
            source.content_digest.as_str(),
        ),
    ] {
        transcript_field(&mut transcript, name, value);
    }
    format!("fnv1a64:{:016x}", fnv1a64(&transcript))
}

/// Fingerprint only the externally reviewed claim surface.  This transcript
/// is deliberately separate from the whole-registry semantic pin: changing a
/// foundation/SOTA claim and co-updating that pin still leaves the review tip
/// stale until a new review event is appended.
pub fn recompute_external_review_claim_fingerprint(
    decision: &Decision,
    profile: &Profile,
) -> String {
    let mut transcript = Vec::new();
    for (name, value) in [
        ("decision.id", decision.id.as_str()),
        ("decision.category", decision.category.as_str()),
        ("decision.stable_key", decision.stable_key.as_str()),
        ("decision.source_block", decision.source_block.as_str()),
        ("decision.source_anchor", decision.source_anchor.as_str()),
        ("decision.disposition", decision.disposition.as_str()),
        (
            "decision.relationship_kind",
            decision.relationship_kind.as_str(),
        ),
        ("decision.summary", decision.summary.as_str()),
        ("decision.profile", decision.profile.as_str()),
        ("profile.rationale", profile.rationale.as_str()),
    ] {
        transcript_field(&mut transcript, name, value);
    }
    for (name, values) in [
        ("profile.assumptions", profile.assumptions.as_slice()),
        (
            "profile.no_claim_boundary",
            profile.no_claim_boundary.as_slice(),
        ),
        (
            "profile.verification_strategy",
            profile.verification_strategy.as_slice(),
        ),
        (
            "profile.negative_evidence_triggers",
            profile.negative_evidence_triggers.as_slice(),
        ),
    ] {
        transcript_array(&mut transcript, name, values);
    }
    format!("fnv1a64:{:016x}", fnv1a64(&transcript))
}

fn external_review_record_fingerprint_with_predecessor(
    review: &ExternalReview,
    sources: &BTreeMap<String, &ExternalReviewSource>,
    predecessor_fingerprint: Option<&str>,
) -> Result<String, String> {
    let mut transcript = Vec::new();
    let sequence = review.sequence.to_string();
    for (name, value) in [
        ("external_review.id", review.id.as_str()),
        ("external_review.decision_id", review.decision_id.as_str()),
        ("external_review.sequence", sequence.as_str()),
        ("external_review.predecessor", review.predecessor.as_str()),
        ("external_review.reviewed_at", review.reviewed_at.as_str()),
        (
            "external_review.claim_fingerprint",
            review.claim_fingerprint.as_str(),
        ),
        ("external_review.outcome", review.outcome.as_str()),
        (
            "external_review.predecessor_fingerprint",
            predecessor_fingerprint.unwrap_or(""),
        ),
    ] {
        transcript_field(&mut transcript, name, value);
    }
    transcript_array(
        &mut transcript,
        "external_review.source_ids",
        &review.source_ids,
    );
    for source_id in &review.source_ids {
        let source = sources.get(source_id).ok_or_else(|| {
            format!(
                "review {:?} references unknown external source {source_id:?}",
                review.id
            )
        })?;
        transcript_field(
            &mut transcript,
            "external_review.source_fingerprint",
            &recompute_external_review_source_fingerprint(source),
        );
    }
    Ok(format!("fnv1a64:{:016x}", fnv1a64(&transcript)))
}

/// Fingerprint one review row using its declared predecessor and the
/// recomputed immutable source fingerprints from `registry`.
pub fn recompute_external_review_record_fingerprint(
    registry: &ArchitectureRegistry,
    review: &ExternalReview,
) -> Result<String, String> {
    let sources: BTreeMap<String, &ExternalReviewSource> = registry
        .external_review_sources
        .iter()
        .map(|source| (source.id.clone(), source))
        .collect();
    let predecessor_fingerprint = if review.predecessor.is_empty() {
        None
    } else {
        Some(
            registry
                .external_reviews
                .iter()
                .find(|candidate| candidate.id == review.predecessor)
                .ok_or_else(|| {
                    format!(
                        "review {:?} references unknown predecessor {:?}",
                        review.id, review.predecessor
                    )
                })?
                .record_fingerprint
                .as_str(),
        )
    };
    external_review_record_fingerprint_with_predecessor(review, &sources, predecessor_fingerprint)
}

/// Independent pin over every immutable external-source row and every review
/// chain record. The code pin makes rewriting old history distinguishable from
/// appending a newly reviewed tip, even when the general semantic pin changes.
pub fn recompute_external_review_history_hash(registry: &ArchitectureRegistry) -> String {
    let mut transcript = Vec::new();
    let mut sources: Vec<&ExternalReviewSource> = registry.external_review_sources.iter().collect();
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    for source in sources {
        transcript_field(&mut transcript, "external_review_source.id", &source.id);
        transcript_field(
            &mut transcript,
            "external_review_source.source_fingerprint",
            &source.source_fingerprint,
        );
    }
    let mut reviews: Vec<&ExternalReview> = registry.external_reviews.iter().collect();
    reviews.sort_by(|left, right| {
        (left.decision_id.as_str(), left.sequence, left.id.as_str()).cmp(&(
            right.decision_id.as_str(),
            right.sequence,
            right.id.as_str(),
        ))
    });
    for review in reviews {
        transcript_field(&mut transcript, "external_review.id", &review.id);
        transcript_field(
            &mut transcript,
            "external_review.decision_id",
            &review.decision_id,
        );
        transcript_field(
            &mut transcript,
            "external_review.sequence",
            &review.sequence.to_string(),
        );
        transcript_field(
            &mut transcript,
            "external_review.record_fingerprint",
            &review.record_fingerprint,
        );
    }
    format!("fnv1a64:{:016x}", fnv1a64(&transcript))
}

/// Canonical pin over the complete sorted decision-ID set.
pub fn recompute_decision_id_hash(registry: &ArchitectureRegistry) -> String {
    let mut ids: Vec<String> = registry
        .decisions
        .iter()
        .map(|decision| decision.id.clone())
        .collect();
    ids.sort();
    id_table_hash(&ids)
}

/// Canonical pin over the normalized bibliography row identity set.
pub fn recompute_bibliography_id_hash(registry: &ArchitectureRegistry) -> String {
    let mut ids: Vec<String> = registry
        .decisions
        .iter()
        .filter(|decision| decision.category == "bibliography")
        .map(|decision| decision.id.clone())
        .collect();
    ids.sort();
    id_table_hash(&ids)
}

/// Canonical pin over the normalized Appendix-E `(id, source_anchor)` set.
pub fn recompute_bibliography_anchor_hash(registry: &ArchitectureRegistry) -> String {
    let mut rows: Vec<(&str, &str)> = registry
        .decisions
        .iter()
        .filter(|decision| decision.category == "bibliography")
        .map(|decision| (decision.id.as_str(), decision.source_anchor.as_str()))
        .collect();
    rows.sort();
    let mut transcript = Vec::new();
    for (id, anchor) in rows {
        transcript_field(&mut transcript, "id", id);
        transcript_field(&mut transcript, "anchor", anchor);
    }
    format!("fnv1a64:{:016x}", fnv1a64(&transcript))
}

/// Canonical semantic-contract transcript.  Length framing prevents field or
/// delimiter ambiguity; row and set order are normalized.  This deliberately
/// covers more than the minimum ID/category/disposition tuple so a profile
/// claim widening cannot hide behind an unchanged decision ID.
pub fn recompute_semantic_contract_hash(registry: &ArchitectureRegistry) -> String {
    let mut transcript = Vec::new();
    let mut decisions: Vec<&Decision> = registry.decisions.iter().collect();
    decisions.sort_by(|left, right| left.id.cmp(&right.id));
    for decision in decisions {
        for (name, value) in [
            ("decision.id", decision.id.as_str()),
            ("decision.category", decision.category.as_str()),
            ("decision.stable_key", decision.stable_key.as_str()),
            ("decision.source_block", decision.source_block.as_str()),
            ("decision.source_anchor", decision.source_anchor.as_str()),
            ("decision.disposition", decision.disposition.as_str()),
            (
                "decision.relationship_kind",
                decision.relationship_kind.as_str(),
            ),
            ("decision.summary", decision.summary.as_str()),
            ("decision.profile", decision.profile.as_str()),
            (
                "decision.owner_workstream",
                decision.owner_workstream.as_str(),
            ),
            ("decision.status", decision.status.as_str()),
        ] {
            transcript_field(&mut transcript, name, value);
        }
        for (name, values) in [
            ("decision.owner_beads", decision.owner_beads.as_slice()),
            ("decision.owner_crates", decision.owner_crates.as_slice()),
            ("decision.affected_bets", decision.affected_bets.as_slice()),
            (
                "decision.affected_constraints",
                decision.affected_constraints.as_slice(),
            ),
            (
                "decision.affected_invariants",
                decision.affected_invariants.as_slice(),
            ),
            (
                "decision.affected_evidence",
                decision.affected_evidence.as_slice(),
            ),
            ("decision.affected_slos", decision.affected_slos.as_slice()),
            (
                "decision.affected_cost_rows",
                decision.affected_cost_rows.as_slice(),
            ),
            (
                "decision.affected_format_rows",
                decision.affected_format_rows.as_slice(),
            ),
            (
                "decision.verification_entrypoints",
                decision.verification_entrypoints.as_slice(),
            ),
            ("decision.checker_ids", decision.checker_ids.as_slice()),
            ("decision.evidence_ids", decision.evidence_ids.as_slice()),
        ] {
            transcript_array(&mut transcript, name, values);
        }
    }
    let mut profiles: Vec<&Profile> = registry.profiles.iter().collect();
    profiles.sort_by(|left, right| left.id.cmp(&right.id));
    for profile in profiles {
        for (name, value) in [
            ("profile.id", profile.id.as_str()),
            ("profile.rationale", profile.rationale.as_str()),
            ("profile.review_policy", profile.review_policy.as_str()),
            ("profile.reviewed_at", profile.reviewed_at.as_str()),
            ("profile.review_after", profile.review_after.as_str()),
            ("profile.check_command", profile.check_command.as_str()),
        ] {
            transcript_field(&mut transcript, name, value);
        }
        for (name, values) in [
            ("profile.assumptions", profile.assumptions.as_slice()),
            (
                "profile.no_claim_boundary",
                profile.no_claim_boundary.as_slice(),
            ),
            (
                "profile.verification_strategy",
                profile.verification_strategy.as_slice(),
            ),
            (
                "profile.negative_evidence_triggers",
                profile.negative_evidence_triggers.as_slice(),
            ),
        ] {
            transcript_array(&mut transcript, name, values);
        }
    }
    let policy = &registry.bead_provenance;
    transcript_field(
        &mut transcript,
        "bead_provenance.source_path",
        &policy.source_path,
    );
    for (name, value) in [
        ("bead_provenance.bead_count", policy.bead_count),
        (
            "bead_provenance.direct_owner_count",
            policy.direct_owner_count,
        ),
        ("bead_provenance.bet_label_count", policy.bet_label_count),
        (
            "bead_provenance.exact_override_count",
            policy.exact_override_count,
        ),
        (
            "bead_provenance.family_rule_count",
            policy.family_rule_count,
        ),
    ] {
        transcript_field(&mut transcript, name, &value.to_string());
    }
    transcript_field(
        &mut transcript,
        "bead_provenance.binding_hash",
        &policy.binding_hash,
    );
    transcript_array(
        &mut transcript,
        "bead_provenance.resolution_precedence",
        &policy.resolution_precedence,
    );
    transcript_array(
        &mut transcript,
        "bead_provenance.allowed_bet_labels",
        &policy.allowed_bet_labels,
    );
    let mut families: Vec<&BeadFamily> = registry.bead_families.iter().collect();
    families.sort_by(|left, right| left.id.cmp(&right.id));
    for family in families {
        for (name, value) in [
            ("bead_family.id", family.id.as_str()),
            ("bead_family.match_kind", family.match_kind.as_str()),
            ("bead_family.pattern", family.pattern.as_str()),
        ] {
            transcript_field(&mut transcript, name, value);
        }
        transcript_field(
            &mut transcript,
            "bead_family.expected_match_count",
            &family.expected_match_count.to_string(),
        );
        transcript_array(
            &mut transcript,
            "bead_family.decision_ids",
            &family.decision_ids,
        );
    }
    let mut overrides: Vec<&BeadOverride> = registry.bead_overrides.iter().collect();
    overrides.sort_by(|left, right| left.id.cmp(&right.id));
    for rule in overrides {
        transcript_field(&mut transcript, "bead_override.id", &rule.id);
        transcript_field(&mut transcript, "bead_override.bead_id", &rule.bead_id);
        transcript_array(
            &mut transcript,
            "bead_override.decision_ids",
            &rule.decision_ids,
        );
    }
    format!("fnv1a64:{:016x}", fnv1a64(&transcript))
}

fn validate_hash_and_ids(registry: &ArchitectureRegistry, violations: &mut Vec<Violation>) {
    let ids: Vec<String> = registry
        .decisions
        .iter()
        .map(|decision| decision.id.clone())
        .collect();
    let duplicate_ids = duplicates(&ids);
    if !duplicate_ids.is_empty() {
        violations.push(Violation::global(
            "decision_id_duplicate",
            "stable_identity",
            format!("duplicate decision IDs {duplicate_ids:?}"),
        ));
    }
    let stable_keys: Vec<String> = registry
        .decisions
        .iter()
        .map(|decision| decision.stable_key.clone())
        .collect();
    let duplicate_keys = duplicates(&stable_keys);
    if !duplicate_keys.is_empty() {
        violations.push(Violation::global(
            "stable_key_duplicate",
            "stable_identity",
            format!("duplicate stable keys {duplicate_keys:?}"),
        ));
    }
    let source_anchors: Vec<String> = registry
        .decisions
        .iter()
        .map(|decision| decision.source_anchor.clone())
        .collect();
    let duplicate_anchors = duplicates(&source_anchors);
    if !duplicate_anchors.is_empty() {
        violations.push(Violation::global(
            "source_anchor_duplicate",
            "source_integrity",
            format!("two or more decision IDs claim the same source anchor {duplicate_anchors:?}"),
        ));
    }
    let recomputed = recompute_decision_id_hash(registry);
    if registry.registry.id_table_hash != recomputed {
        violations.push(Violation::global(
            "id_table_hash_mismatch",
            "stable_identity",
            format!(
                "registry hash {:?}, recomputed {recomputed:?}",
                registry.registry.id_table_hash
            ),
        ));
    }
    if recomputed != PINNED_DECISION_ID_HASH {
        violations.push(Violation::global(
            "independent_id_hash_mismatch",
            "stable_identity",
            format!(
                "sorted decision ID hash {recomputed:?} differs from code pin {PINNED_DECISION_ID_HASH:?}"
            ),
        ));
    }
    let bibliography_hash = recompute_bibliography_id_hash(registry);
    if bibliography_hash != PINNED_BIBLIOGRAPHY_ID_HASH {
        violations.push(Violation::global(
            "bibliography_id_hash_mismatch",
            "stable_identity",
            format!(
                "bibliography ID hash {bibliography_hash:?} differs from code pin {PINNED_BIBLIOGRAPHY_ID_HASH:?}"
            ),
        ));
    }
    let bibliography_anchor_hash = recompute_bibliography_anchor_hash(registry);
    if bibliography_anchor_hash != PINNED_BIBLIOGRAPHY_ANCHOR_HASH {
        violations.push(Violation::global(
            "bibliography_anchor_hash_mismatch",
            "source_integrity",
            format!(
                "bibliography anchor hash {bibliography_anchor_hash:?} differs from code pin {PINNED_BIBLIOGRAPHY_ANCHOR_HASH:?}"
            ),
        ));
    }
    let semantic_hash = recompute_semantic_contract_hash(registry);
    if semantic_hash != PINNED_SEMANTIC_CONTRACT_HASH {
        violations.push(Violation::global(
            "semantic_contract_hash_mismatch",
            "semantic_contract",
            format!(
                "semantic contract hash {semantic_hash:?} differs from code pin {PINNED_SEMANTIC_CONTRACT_HASH:?}"
            ),
        ));
    }
}

fn validate_profiles<'a>(
    registry: &'a ArchitectureRegistry,
    violations: &mut Vec<Violation>,
) -> BTreeMap<String, &'a Profile> {
    let mut profiles = BTreeMap::new();
    for profile in &registry.profiles {
        if profiles.insert(profile.id.clone(), profile).is_some() {
            violations.push(Violation::global(
                "profile_id_duplicate",
                "profile_closure",
                format!("duplicate profile ID {:?}", profile.id),
            ));
        }
        for (field, value) in [
            ("id", profile.id.as_str()),
            ("rationale", profile.rationale.as_str()),
            ("review_policy", profile.review_policy.as_str()),
            ("reviewed_at", profile.reviewed_at.as_str()),
            ("review_after", profile.review_after.as_str()),
            ("check_command", profile.check_command.as_str()),
        ] {
            if value.trim().is_empty() {
                violations.push(Violation::global(
                    "profile_required_field",
                    "profile_closure",
                    format!("profile {:?} has blank {field}", profile.id),
                ));
            }
        }
        for (field, values) in [
            ("assumptions", profile.assumptions.as_slice()),
            ("no_claim_boundary", profile.no_claim_boundary.as_slice()),
            (
                "verification_strategy",
                profile.verification_strategy.as_slice(),
            ),
            (
                "negative_evidence_triggers",
                profile.negative_evidence_triggers.as_slice(),
            ),
        ] {
            if values.is_empty() {
                violations.push(Violation::global(
                    "profile_required_array",
                    "profile_closure",
                    format!("profile {:?} has empty {field}", profile.id),
                ));
            }
            if !blank_items(values).is_empty() || !duplicates(values).is_empty() {
                violations.push(Violation::global(
                    "profile_array_shape",
                    "profile_closure",
                    format!(
                        "profile {:?} has blank or duplicate {field} entries",
                        profile.id
                    ),
                ));
            }
        }
        if !valid_iso_date(&profile.reviewed_at) || !valid_review_after(&profile.review_after) {
            violations.push(Violation::global(
                "profile_review_date",
                "review_policy",
                format!(
                    "profile {:?} must use YYYY-MM-DD reviewed_at and a date or closed event trigger for review_after",
                    profile.id
                ),
            ));
        } else if valid_iso_date(&profile.review_after)
            && profile.review_after < profile.reviewed_at
        {
            violations.push(Violation::global(
                "profile_review_order",
                "review_policy",
                format!("profile {:?} review_after precedes reviewed_at", profile.id),
            ));
        }
        if profile.check_command != REPLAY_COMMAND {
            violations.push(Violation::global(
                "profile_check_command",
                "verification_closure",
                format!(
                    "profile {:?} check_command must be the exact supported replay command {REPLAY_COMMAND:?}",
                    profile.id
                ),
            ));
        }
    }
    let referenced: BTreeSet<&str> = registry
        .decisions
        .iter()
        .map(|decision| decision.profile.as_str())
        .collect();
    for id in profiles.keys() {
        if !referenced.contains(id.as_str()) {
            violations.push(Violation::global(
                "orphan_profile",
                "profile_closure",
                format!("profile {id:?} is not referenced by any decision"),
            ));
        }
    }
    profiles
}

fn validate_sources(registry: &ArchitectureRegistry, root: &Path, violations: &mut Vec<Violation>) {
    let mut ids = BTreeSet::new();
    for block in &registry.source_blocks {
        if !ids.insert(block.id.clone()) {
            violations.push(Violation::global(
                "source_block_duplicate",
                "source_integrity",
                format!("duplicate source block ID {:?}", block.id),
            ));
        }
        if block.document_path != "docs/ARCHITECTURE_DECISION_RECORD.md"
            || block.plan_path != "COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
        {
            violations.push(Violation::global(
                "source_path_mismatch",
                "source_integrity",
                format!(
                    "source block {:?} must bind the canonical ADR document to the master plan",
                    block.id
                ),
            ));
        }
        match source_expected(block) {
            Some((start, end, lines, bytes, hash)) => {
                if block.plan_start_line != start
                    || block.plan_end_line != end
                    || block.line_count != lines
                    || block.byte_count != bytes
                    || block.fnv1a64 != hash
                {
                    violations.push(Violation::global(
                        "source_metadata_pin",
                        "source_integrity",
                        format!(
                            "source block {:?} metadata differs from the pinned ({start}..={end}, {lines} lines, {bytes} bytes, {hash}) tuple",
                            block.id
                        ),
                    ));
                }
            }
            None => violations.push(Violation::global(
                "source_block_unknown",
                "source_integrity",
                format!("unrecognized source block {:?}", block.id),
            )),
        }
        let expected_start = format!("<!-- CHECKED-SOURCE-BEGIN id=\"{}\" -->", block.id);
        let expected_end = format!("<!-- CHECKED-SOURCE-END id=\"{}\" -->", block.id);
        if block.start_marker != expected_start || block.end_marker != expected_end {
            violations.push(Violation::global(
                "source_marker_pin",
                "source_integrity",
                format!(
                    "source block {:?} does not use its exact checked-source markers",
                    block.id
                ),
            ));
        }
        match source_check(block, root) {
            Ok(check) => {
                if check.outcome != "pass" {
                    violations.push(Violation::global(
                        "source_bytes_mismatch",
                        "source_integrity",
                        format!(
                            "source block {:?} failed exact byte/line/FNV validation ({} lines, {} bytes, {})",
                            block.id, check.line_count, check.byte_count, check.fnv1a64
                        ),
                    ));
                }
            }
            Err(error) => violations.push(Violation::global(
                "source_load_error",
                "source_integrity",
                format!("source block {:?}: {error}", block.id),
            )),
        }
    }
    let required = set_of(&REQUIRED_SOURCE_BLOCKS);
    if ids != required {
        violations.push(Violation::global(
            "source_block_set",
            "source_integrity",
            format!("source block IDs {ids:?} do not equal required set {required:?}"),
        ));
    }
}

fn validate_decision_shape(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    registry: &ArchitectureRegistry,
    violations: &mut Vec<Violation>,
) {
    for (field, value) in [
        ("id", decision.id.as_str()),
        ("category", decision.category.as_str()),
        ("stable_key", decision.stable_key.as_str()),
        ("source_block", decision.source_block.as_str()),
        ("source_anchor", decision.source_anchor.as_str()),
        ("disposition", decision.disposition.as_str()),
        ("relationship_kind", decision.relationship_kind.as_str()),
        ("summary", decision.summary.as_str()),
        ("profile", decision.profile.as_str()),
        ("status", decision.status.as_str()),
    ] {
        if value.trim().is_empty() {
            violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "required_field_blank",
                "schema",
                format!("decision field {field} is blank"),
            ));
        }
    }
    if !valid_stable_id(&decision.id, &registry.registry.decision_id_prefix) {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "decision_id_format",
            "stable_identity",
            format!(
                "ID must start with {:?} and use a nonempty uppercase ASCII segmented suffix",
                registry.registry.decision_id_prefix
            ),
        ));
    }
    if !valid_stable_key(&decision.stable_key) {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "stable_key_format",
            "stable_identity",
            "stable_key must use lowercase ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    for (field, values) in [
        ("owner_beads", decision.owner_beads.as_slice()),
        ("owner_crates", decision.owner_crates.as_slice()),
        ("affected_bets", decision.affected_bets.as_slice()),
        (
            "affected_constraints",
            decision.affected_constraints.as_slice(),
        ),
        (
            "affected_invariants",
            decision.affected_invariants.as_slice(),
        ),
        ("affected_evidence", decision.affected_evidence.as_slice()),
        ("affected_slos", decision.affected_slos.as_slice()),
        ("affected_cost_rows", decision.affected_cost_rows.as_slice()),
        (
            "affected_format_rows",
            decision.affected_format_rows.as_slice(),
        ),
        (
            "verification_entrypoints",
            decision.verification_entrypoints.as_slice(),
        ),
        ("checker_ids", decision.checker_ids.as_slice()),
        ("evidence_ids", decision.evidence_ids.as_slice()),
    ] {
        validate_string_array(decision, profiles, claim_class, field, values, violations);
    }
    for entrypoint in &decision.verification_entrypoints {
        if !valid_verification_entrypoint(entrypoint) {
            violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "verification_entrypoint_scheme",
                "verification_closure",
                format!(
                    "verification entrypoint {entrypoint:?} must use cargo-test:, cargo-check:, or e2e:"
                ),
            ));
        }
    }
    for (field, value, allowed) in [
        (
            "category",
            decision.category.as_str(),
            ALLOWED_CATEGORIES.as_slice(),
        ),
        (
            "disposition",
            decision.disposition.as_str(),
            ALLOWED_DISPOSITIONS.as_slice(),
        ),
        (
            "relationship_kind",
            decision.relationship_kind.as_str(),
            ALLOWED_RELATIONSHIP_KINDS.as_slice(),
        ),
        (
            "status",
            decision.status.as_str(),
            ALLOWED_STATUSES.as_slice(),
        ),
    ] {
        if !allowed.contains(&value) {
            violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "closed_enum",
                "schema",
                format!("{field} value {value:?} is outside the closed enum"),
            ));
        }
    }
    if !registry
        .source_blocks
        .iter()
        .any(|block| block.id == decision.source_block)
    {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "source_block_unresolved",
            "source_integrity",
            format!("source block {:?} is not declared", decision.source_block),
        ));
    }
    let expected_source = if decision.category == "bibliography" {
        "plan-reviewed-bibliography-v1"
    } else {
        "plan-thesis-foundations-sota-v1"
    };
    if decision.source_block != expected_source {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "category_source_mismatch",
            "source_integrity",
            format!(
                "category {:?} must use source block {expected_source:?}",
                decision.category
            ),
        ));
    }
    if !profiles.contains_key(&decision.profile) {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "profile_unresolved",
            "profile_closure",
            format!("profile {:?} is not declared", decision.profile),
        ));
    }
}

fn validate_disposition(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    violations: &mut Vec<Violation>,
) {
    let compatible = match decision.relationship_kind.as_str() {
        "consume_as_is" => matches!(decision.disposition.as_str(), "consume"),
        "design_donor" => matches!(decision.disposition.as_str(), "adapt" | "adopt" | "defer"),
        "upstream_prerequisite" => matches!(decision.disposition.as_str(), "defer" | "consume"),
        "build_in_house" => matches!(decision.disposition.as_str(), "build" | "adapt" | "adopt"),
        "test_only_oracle" => matches!(decision.disposition.as_str(), "test_only"),
        "research_only_citation" => {
            matches!(
                decision.disposition.as_str(),
                "research_only" | "reject" | "defer"
            )
        }
        _ => true,
    };
    if !compatible {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "disposition_relationship_conflict",
            "disposition",
            format!(
                "disposition {:?} is incompatible with relationship {:?}",
                decision.disposition, decision.relationship_kind
            ),
        ));
    }
    if decision.category == "rejection"
        && (!matches!(decision.disposition.as_str(), "reject" | "defer")
            || decision.relationship_kind != "research_only_citation")
    {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "frozen_rejection_changed",
            "rejected_architecture_reintroduced",
            "rejection rows must remain reject/defer + research_only_citation",
        ));
    }
    if decision.category == "bibliography"
        && (decision.disposition != "research_only"
            || decision.relationship_kind != "research_only_citation")
    {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "bibliography_promoted",
            "research_dependency_promotion",
            "bibliography rows must remain research_only + research_only_citation",
        ));
    }
    if decision.disposition == "research_only"
        && decision.relationship_kind != "research_only_citation"
    {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "research_disposition_conflict",
            "research_dependency_promotion",
            "research_only disposition requires research_only_citation relationship",
        ));
    }
    if decision.category == "bibliography"
        && (!decision.owner_crates.is_empty()
            || !decision.affected_cost_rows.is_empty()
            || !decision.affected_format_rows.is_empty())
    {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "research_dependency_promotion",
            "research_dependency_promotion",
            "bibliography citations cannot own runtime crates or cost/format rows",
        ));
    }
    if decision.relationship_kind == "research_only_citation"
        && matches!(
            decision.disposition.as_str(),
            "adopt" | "adapt" | "consume" | "build" | "test_only"
        )
    {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "research_dependency_promotion",
            "research_dependency_promotion",
            "research-only citation was promoted to a runtime/dependency disposition",
        ));
    }
}

fn validate_owners(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    registry: &ArchitectureRegistry,
    bead_ids: Option<&BTreeSet<String>>,
    violations: &mut Vec<Violation>,
) {
    let effective = decision.status != "superseded";
    if effective && decision.owner_beads.is_empty() {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "owner_bead_missing",
            "owner_closure",
            "effective decisions require at least one named owner bead",
        ));
    }
    if effective && !valid_workstream(&decision.owner_workstream) {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "owner_workstream_invalid",
            "owner_closure",
            format!(
                "owner_workstream {:?} is outside G0, W1..W12, Verification, Performance, or Cross-cutting",
                decision.owner_workstream
            ),
        ));
    }
    let runtime_or_build = decision.relationship_kind != "research_only_citation";
    if effective && runtime_or_build && decision.owner_crates.is_empty() {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "owner_crate_missing",
            "owner_closure",
            "effective runtime/build/test decisions require a concrete owner crate",
        ));
    }
    let planned: BTreeSet<&str> = registry
        .registry
        .planned_crates
        .iter()
        .map(String::as_str)
        .collect();
    for owner_crate in &decision.owner_crates {
        if !planned.contains(owner_crate.as_str()) {
            let mut violation = Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "owner_crate_unplanned",
                "owner_closure",
                format!("owner crate {owner_crate:?} is outside registry.planned_crates"),
            );
            violation.owner_crate = owner_crate.clone();
            violations.push(violation);
        }
    }
    if let Some(bead_ids) = bead_ids {
        for owner_bead in &decision.owner_beads {
            if !bead_ids.contains(owner_bead) {
                let mut violation = Violation::for_decision(
                    decision,
                    profiles,
                    claim_class,
                    "owner_bead_unresolved",
                    "owner_closure",
                    format!("owner bead {owner_bead:?} does not resolve in .beads/issues.jsonl"),
                );
                violation.owner_bead = owner_bead.clone();
                violations.push(violation);
            }
        }
    }
    if effective && decision.verification_entrypoints.is_empty() {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "verification_entrypoint_missing",
            "verification_closure",
            "effective decisions require at least one verification entrypoint",
        ));
    }
    if effective && decision.checker_ids.is_empty() {
        violations.push(Violation::for_decision(
            decision,
            profiles,
            claim_class,
            "checker_id_missing",
            "verification_closure",
            "effective decisions require at least one checker ID",
        ));
    }
}

fn validate_references(
    decision: &Decision,
    profiles: &BTreeMap<String, &Profile>,
    claim_class: &str,
    catalog: &ReferenceCatalog,
    root: &Path,
    violations: &mut Vec<Violation>,
) {
    validate_reference_set(
        decision,
        profiles,
        claim_class,
        "affected_bets",
        &decision.affected_bets,
        &catalog.bets,
        violations,
    );
    validate_reference_set(
        decision,
        profiles,
        claim_class,
        "affected_constraints",
        &decision.affected_constraints,
        &catalog.constraints,
        violations,
    );
    validate_reference_set(
        decision,
        profiles,
        claim_class,
        "affected_invariants",
        &decision.affected_invariants,
        &catalog.invariants,
        violations,
    );
    validate_reference_map(
        decision,
        profiles,
        claim_class,
        "affected_evidence",
        &decision.affected_evidence,
        &catalog.evidence,
        violations,
    );
    validate_reference_map(
        decision,
        profiles,
        claim_class,
        "affected_slos",
        &decision.affected_slos,
        &catalog.slos,
        violations,
    );
    validate_reserved_or_live(
        decision,
        profiles,
        claim_class,
        "affected_cost_rows",
        &decision.affected_cost_rows,
        catalog.cost_rows.as_ref(),
        violations,
    );
    validate_reserved_or_live(
        decision,
        profiles,
        claim_class,
        "affected_format_rows",
        &decision.affected_format_rows,
        catalog.format_rows.as_ref(),
        violations,
    );

    for evidence_id in &decision.evidence_ids {
        if !catalog.evidence.contains_key(evidence_id) && !catalog.slos.contains_key(evidence_id) {
            violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "evidence_id_unresolved",
                "evidence_closure",
                format!(
                    "evidence ID {evidence_id:?} does not resolve in evidence.toml or slo.toml"
                ),
            ));
        }
    }
    for checker_id in &decision.checker_ids {
        match catalog.checkers.get(checker_id) {
            None => violations.push(Violation::for_decision(
                decision,
                profiles,
                claim_class,
                "checker_id_unresolved",
                "verification_closure",
                format!("checker ID {checker_id:?} does not resolve in checker_index.toml"),
            )),
            Some(checker) => {
                if checker.status != "live" && decision.status != "superseded" {
                    violations.push(Violation::for_decision(
                        decision,
                        profiles,
                        claim_class,
                        "checker_not_live",
                        "verification_closure",
                        format!(
                            "effective decision uses checker {checker_id:?} with status {:?}",
                            checker.status
                        ),
                    ));
                }
                if checker.status == "live"
                    && (!safe_repo_relative(&checker.artifact)
                        || !root.join(&checker.artifact).is_file())
                {
                    violations.push(Violation::for_decision(
                        decision,
                        profiles,
                        claim_class,
                        "checker_artifact_missing",
                        "verification_closure",
                        format!(
                            "live checker {checker_id:?} artifact {:?} is absent or unsafe",
                            checker.artifact
                        ),
                    ));
                }
            }
        }
    }
}

fn live_verification_resolution_issues(
    declaration: &VerificationEntrypoint,
    catalog: &ReferenceCatalog,
    root: &Path,
) -> Vec<(&'static str, String)> {
    let mut issues = Vec::new();
    let Some((scheme, _)) = verification_entrypoint_parts(&declaration.entrypoint) else {
        issues.push((
            "verification_entrypoint_declaration_scheme",
            format!(
                "declared verification entrypoint {:?} has an invalid scheme or label",
                declaration.entrypoint
            ),
        ));
        return issues;
    };
    let Some(checker_id) = declaration.checker_id.as_deref() else {
        issues.push((
            "live_verification_checker_missing",
            format!(
                "live verification entrypoint {:?} has no checker_id",
                declaration.entrypoint
            ),
        ));
        return issues;
    };
    if checker_id != declaration.entrypoint {
        issues.push((
            "verification_entrypoint_checker_identity",
            format!(
                "live verification entrypoint {:?} must use a dedicated checker with the same full symbol, found {:?}",
                declaration.entrypoint, checker_id
            ),
        ));
    }
    let Some(checker) = catalog.checkers.get(checker_id) else {
        issues.push((
            "verification_entrypoint_checker_unresolved",
            format!(
                "live verification entrypoint {:?} checker {:?} does not resolve in checker_index.toml",
                declaration.entrypoint, checker_id
            ),
        ));
        return issues;
    };
    if checker.status != "live" {
        issues.push((
            "verification_entrypoint_checker_not_live",
            format!(
                "live verification entrypoint {:?} resolves to checker {:?} with status {:?}",
                declaration.entrypoint, checker_id, checker.status
            ),
        ));
    }
    let expected_kind =
        checker_kind_for_scheme(scheme).expect("valid verification scheme has a checker kind");
    if checker.kind != expected_kind {
        issues.push((
            "verification_entrypoint_kind_mismatch",
            format!(
                "verification entrypoint {:?} requires checker kind {:?}, but {:?} is {:?}",
                declaration.entrypoint, expected_kind, checker_id, checker.kind
            ),
        ));
    }
    if !safe_repo_relative(&checker.artifact) || !root.join(&checker.artifact).is_file() {
        issues.push((
            "verification_entrypoint_artifact_missing",
            format!(
                "live verification entrypoint {:?} checker {:?} artifact {:?} is absent or unsafe",
                declaration.entrypoint, checker_id, checker.artifact
            ),
        ));
    }

    match scheme {
        "cargo-test" => {
            let (Some(package_name), Some(target), Some(selector), Some(command_argv)) = (
                declaration.package.as_deref(),
                declaration.target.as_deref(),
                declaration.selector.as_deref(),
                declaration.command_argv.as_ref(),
            ) else {
                issues.push((
                    "verification_entrypoint_invocation_missing",
                    format!(
                        "live cargo-test entrypoint {:?} requires package, target, selector, and command_argv",
                        declaration.entrypoint
                    ),
                ));
                return issues;
            };
            match resolve_workspace_package(root, package_name) {
                Err(error) => issues.push(("verification_entrypoint_package", error)),
                Ok(None) => issues.push((
                    "verification_entrypoint_package",
                    format!(
                        "cargo-test entrypoint {:?} package {:?} is not a workspace member",
                        declaration.entrypoint, package_name
                    ),
                )),
                Ok(Some(package)) => {
                    let expected_artifact = cargo_test_artifact(&package, target);
                    if checker.artifact != expected_artifact {
                        issues.push((
                            "verification_entrypoint_target_mismatch",
                            format!(
                                "cargo-test entrypoint {:?} target resolves to {:?}, checker artifact is {:?}",
                                declaration.entrypoint, expected_artifact, checker.artifact
                            ),
                        ));
                    }
                    match fs::read_to_string(root.join(&expected_artifact)) {
                        Ok(source) => {
                            let count = rust_test_selector_count(&source, selector);
                            if count != 1 {
                                issues.push((
                                    "verification_entrypoint_selector",
                                    format!(
                                        "cargo-test entrypoint {:?} selector {:?} occurs as an exact #[test] function {count} times in {:?}",
                                        declaration.entrypoint, selector, expected_artifact
                                    ),
                                ));
                            }
                        }
                        Err(error) => issues.push((
                            "verification_entrypoint_target_missing",
                            format!(
                                "cargo-test entrypoint {:?} target {:?} cannot be read: {error}",
                                declaration.entrypoint, expected_artifact
                            ),
                        )),
                    }
                }
            }
            let expected_command = vec![
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                package_name.to_string(),
                "--test".to_string(),
                target.to_string(),
                selector.to_string(),
                "--".to_string(),
                "--exact".to_string(),
            ];
            if *command_argv != expected_command {
                issues.push((
                    "verification_entrypoint_command",
                    format!(
                        "cargo-test entrypoint {:?} command_argv must be {:?}",
                        declaration.entrypoint, expected_command
                    ),
                ));
            }
            if package_name == "registry-check"
                && target == "architecture_decisions"
                && declaration.evidence_scope != "governance"
            {
                issues.push((
                    "verification_entrypoint_scope_mismatch",
                    format!(
                        "the architecture_decisions registry-contract target is governance evidence, not {:?} evidence",
                        declaration.evidence_scope
                    ),
                ));
            }
        }
        "cargo-check" => {
            let (Some(package_name), Some(target), Some(command_argv)) = (
                declaration.package.as_deref(),
                declaration.target.as_deref(),
                declaration.command_argv.as_ref(),
            ) else {
                issues.push((
                    "verification_entrypoint_invocation_missing",
                    format!(
                        "live cargo-check entrypoint {:?} requires package, target, and command_argv",
                        declaration.entrypoint
                    ),
                ));
                return issues;
            };
            if declaration.selector.is_some() {
                issues.push((
                    "verification_entrypoint_invocation_shape",
                    format!(
                        "cargo-check entrypoint {:?} cannot declare a test selector",
                        declaration.entrypoint
                    ),
                ));
            }
            match resolve_workspace_package(root, package_name) {
                Err(error) => issues.push(("verification_entrypoint_package", error)),
                Ok(None) => issues.push((
                    "verification_entrypoint_package",
                    format!(
                        "cargo-check entrypoint {:?} package {:?} is not a workspace member",
                        declaration.entrypoint, package_name
                    ),
                )),
                Ok(Some(package)) => match cargo_bin_artifact(root, &package, target) {
                    Err(error) => issues.push(("verification_entrypoint_target_missing", error)),
                    Ok(None) => issues.push((
                        "verification_entrypoint_target_missing",
                        format!(
                            "cargo-check entrypoint {:?} binary target {:?} does not exist",
                            declaration.entrypoint, target
                        ),
                    )),
                    Ok(Some(expected_artifact)) if checker.artifact != expected_artifact => {
                        issues.push((
                            "verification_entrypoint_target_mismatch",
                            format!(
                                "cargo-check entrypoint {:?} target resolves to {:?}, checker artifact is {:?}",
                                declaration.entrypoint, expected_artifact, checker.artifact
                            ),
                        ));
                    }
                    Ok(Some(_)) => {}
                },
            }
            let expected_command = vec![
                "cargo".to_string(),
                "check".to_string(),
                "-p".to_string(),
                package_name.to_string(),
                "--bin".to_string(),
                target.to_string(),
            ];
            if *command_argv != expected_command {
                issues.push((
                    "verification_entrypoint_command",
                    format!(
                        "cargo-check entrypoint {:?} command_argv must be {:?}",
                        declaration.entrypoint, expected_command
                    ),
                ));
            }
        }
        "e2e" => {
            if declaration.package.is_some()
                || declaration.target.is_some()
                || declaration.selector.is_some()
            {
                issues.push((
                    "verification_entrypoint_invocation_shape",
                    format!(
                        "e2e entrypoint {:?} cannot declare Cargo package/target/selector fields",
                        declaration.entrypoint
                    ),
                ));
            }
            let Some(command_argv) = declaration.command_argv.as_ref() else {
                issues.push((
                    "verification_entrypoint_invocation_missing",
                    format!(
                        "live e2e entrypoint {:?} requires command_argv",
                        declaration.entrypoint
                    ),
                ));
                return issues;
            };
            let expected_command = vec![checker.artifact.clone()];
            if *command_argv != expected_command {
                issues.push((
                    "verification_entrypoint_command",
                    format!(
                        "e2e entrypoint {:?} command_argv must directly execute {:?}",
                        declaration.entrypoint, checker.artifact
                    ),
                ));
            }
            if safe_repo_relative(&checker.artifact)
                && !executable_file(&root.join(&checker.artifact))
            {
                issues.push((
                    "verification_entrypoint_script_not_executable",
                    format!(
                        "e2e entrypoint {:?} script {:?} is not executable",
                        declaration.entrypoint, checker.artifact
                    ),
                ));
            }
            if checker.artifact == "scripts/g0_architecture_decisions_e2e.sh"
                && declaration.evidence_scope != "governance"
            {
                issues.push((
                    "verification_entrypoint_scope_mismatch",
                    format!(
                        "the architecture-decision E2E is governance evidence, not {:?} evidence",
                        declaration.evidence_scope
                    ),
                ));
            }
        }
        _ => unreachable!("verification_entrypoint_parts rejects unknown schemes"),
    }
    issues
}

fn validate_verification_entrypoint_registry_basic(
    registry: &ArchitectureRegistry,
    profiles: &BTreeMap<String, &Profile>,
    catalog: &ReferenceCatalog,
    root: &Path,
    violations: &mut Vec<Violation>,
) {
    let mut declarations = BTreeMap::<&str, &VerificationEntrypoint>::new();
    let mut valid_live = BTreeSet::<&str>::new();

    for declaration in &registry.verification_entrypoints {
        if declarations
            .insert(declaration.entrypoint.as_str(), declaration)
            .is_some()
        {
            violations.push(Violation::global(
                "verification_entrypoint_declaration_duplicate",
                "verification_closure",
                format!(
                    "verification entrypoint {:?} is declared more than once",
                    declaration.entrypoint
                ),
            ));
        }
        let parts = verification_entrypoint_parts(&declaration.entrypoint);
        if parts.is_none() {
            violations.push(Violation::global(
                "verification_entrypoint_declaration_scheme",
                "verification_closure",
                format!(
                    "declared verification entrypoint {:?} has an invalid scheme or label",
                    declaration.entrypoint
                ),
            ));
        }
        if !ALLOWED_VERIFICATION_ENTRYPOINT_STATUSES.contains(&declaration.status.as_str()) {
            violations.push(Violation::global(
                "verification_entrypoint_status",
                "verification_closure",
                format!(
                    "verification entrypoint {:?} has invalid status {:?}",
                    declaration.entrypoint, declaration.status
                ),
            ));
            continue;
        }
        if declaration
            .checker_id
            .as_ref()
            .is_some_and(|checker_id| checker_id.trim().is_empty())
        {
            violations.push(Violation::global(
                "verification_entrypoint_checker_blank",
                "verification_closure",
                format!(
                    "verification entrypoint {:?} has a blank checker_id",
                    declaration.entrypoint
                ),
            ));
        }

        match declaration.status.as_str() {
            "live" => {
                let Some(checker_id) = declaration.checker_id.as_deref() else {
                    violations.push(Violation::global(
                        "live_verification_checker_missing",
                        "verification_closure",
                        format!(
                            "live verification entrypoint {:?} has no checker_id",
                            declaration.entrypoint
                        ),
                    ));
                    continue;
                };
                let Some(checker) = catalog.checkers.get(checker_id) else {
                    violations.push(Violation::global(
                        "verification_entrypoint_checker_unresolved",
                        "verification_closure",
                        format!(
                            "live verification entrypoint {:?} checker {:?} does not resolve in checker_index.toml",
                            declaration.entrypoint, checker_id
                        ),
                    ));
                    continue;
                };
                let mut resolves = true;
                if checker.status != "live" {
                    resolves = false;
                    violations.push(Violation::global(
                        "verification_entrypoint_checker_not_live",
                        "verification_closure",
                        format!(
                            "live verification entrypoint {:?} resolves to checker {:?} with status {:?}",
                            declaration.entrypoint, checker_id, checker.status
                        ),
                    ));
                }
                if let Some((scheme, _)) = parts {
                    let expected_kind = checker_kind_for_scheme(scheme)
                        .expect("valid verification scheme has a checker kind");
                    if checker.kind != expected_kind {
                        resolves = false;
                        violations.push(Violation::global(
                            "verification_entrypoint_kind_mismatch",
                            "verification_closure",
                            format!(
                                "verification entrypoint {:?} requires checker kind {:?}, but {:?} is {:?}",
                                declaration.entrypoint, expected_kind, checker_id, checker.kind
                            ),
                        ));
                    }
                } else {
                    resolves = false;
                }
                if !safe_repo_relative(&checker.artifact) || !root.join(&checker.artifact).is_file()
                {
                    resolves = false;
                    violations.push(Violation::global(
                        "verification_entrypoint_artifact_missing",
                        "verification_closure",
                        format!(
                            "live verification entrypoint {:?} checker {:?} artifact {:?} is absent or unsafe",
                            declaration.entrypoint, checker_id, checker.artifact
                        ),
                    ));
                }
                if resolves {
                    valid_live.insert(declaration.entrypoint.as_str());
                }
            }
            "planned" => {
                if let Some(checker_id) = declaration.checker_id.as_deref() {
                    match catalog.checkers.get(checker_id) {
                        None => violations.push(Violation::global(
                            "verification_entrypoint_checker_unresolved",
                            "verification_closure",
                            format!(
                                "planned verification entrypoint {:?} checker {:?} does not resolve in checker_index.toml",
                                declaration.entrypoint, checker_id
                            ),
                        )),
                        Some(checker) if checker.status == "live" => {
                            violations.push(Violation::global(
                                "planned_verification_checker_live",
                                "verification_closure",
                                format!(
                                    "planned verification entrypoint {:?} points at already-live checker {:?}",
                                    declaration.entrypoint, checker_id
                                ),
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
            _ => {}
        }
    }

    let mut referenced = BTreeSet::new();
    for decision in &registry.decisions {
        let claim_class = claim_classes_for(decision, catalog).join("+");
        let mut decision_has_live = false;
        for entrypoint in &decision.verification_entrypoints {
            referenced.insert(entrypoint.as_str());
            match declarations.get(entrypoint.as_str()) {
                None => violations.push(Violation::for_decision(
                    decision,
                    profiles,
                    &claim_class,
                    "verification_entrypoint_unresolved",
                    "verification_closure",
                    format!(
                        "verification entrypoint {entrypoint:?} has no registry-level declaration"
                    ),
                )),
                Some(_) if valid_live.contains(entrypoint.as_str()) => {
                    decision_has_live = true;
                }
                Some(_) => {}
            }
        }
        if decision.status != "superseded" && !decision_has_live {
            violations.push(Violation::for_decision(
                decision,
                profiles,
                &claim_class,
                "live_verification_entrypoint_missing",
                "verification_closure",
                "effective decisions require at least one declared entrypoint backed by a live checker artifact",
            ));
        }
    }
    for declaration in &registry.verification_entrypoints {
        if !referenced.contains(declaration.entrypoint.as_str()) {
            violations.push(Violation::global(
                "verification_entrypoint_declaration_orphan",
                "verification_closure",
                format!(
                    "verification entrypoint declaration {:?} is not referenced by any decision",
                    declaration.entrypoint
                ),
            ));
        }
    }
}

/// Return the decision's entrypoints that are currently backed by exact live
/// checker targets in the requested evidence scope. In particular, governance
/// checks can never be returned as implementation evidence.
pub fn resolved_live_entrypoints_for_scope(
    registry: &ArchitectureRegistry,
    root: &Path,
    decision_id: &str,
    evidence_scope: &str,
) -> Result<Vec<String>, String> {
    if !ALLOWED_VERIFICATION_EVIDENCE_SCOPES.contains(&evidence_scope) {
        return Err(format!(
            "unknown verification evidence scope {evidence_scope:?}"
        ));
    }
    let decision = registry
        .decisions
        .iter()
        .find(|decision| decision.id == decision_id)
        .ok_or_else(|| format!("unknown architecture decision {decision_id:?}"))?;
    let catalog = load_reference_catalog(root)?;
    let mut declarations = BTreeMap::new();
    for declaration in &registry.verification_entrypoints {
        if declarations
            .insert(declaration.entrypoint.as_str(), declaration)
            .is_some()
        {
            return Err(format!(
                "duplicate verification entrypoint declaration {:?}",
                declaration.entrypoint
            ));
        }
    }
    let mut resolved = Vec::new();
    for entrypoint in &decision.verification_entrypoints {
        let Some(declaration) = declarations.get(entrypoint.as_str()) else {
            continue;
        };
        if declaration.status == "live"
            && declaration.evidence_scope == evidence_scope
            && live_verification_resolution_issues(declaration, &catalog, root).is_empty()
        {
            resolved.push(entrypoint.clone());
        }
    }
    resolved.sort();
    resolved.dedup();
    Ok(resolved)
}

fn validate_verification_entrypoint_registry(
    registry: &ArchitectureRegistry,
    profiles: &BTreeMap<String, &Profile>,
    catalog: &ReferenceCatalog,
    root: &Path,
    violations: &mut Vec<Violation>,
) {
    validate_verification_entrypoint_registry_basic(registry, profiles, catalog, root, violations);

    let mut declarations = BTreeMap::<&str, &VerificationEntrypoint>::new();
    let mut checker_owners = BTreeMap::<&str, &str>::new();
    let mut valid_live_governance = BTreeSet::<&str>::new();
    for declaration in &registry.verification_entrypoints {
        declarations
            .entry(declaration.entrypoint.as_str())
            .or_insert(declaration);
        if !ALLOWED_VERIFICATION_EVIDENCE_SCOPES.contains(&declaration.evidence_scope.as_str()) {
            violations.push(Violation::global(
                "verification_entrypoint_evidence_scope",
                "verification_closure",
                format!(
                    "verification entrypoint {:?} has invalid evidence_scope {:?}",
                    declaration.entrypoint, declaration.evidence_scope
                ),
            ));
        }
        if let Some(checker_id) = declaration.checker_id.as_deref()
            && let Some(prior) = checker_owners.insert(checker_id, &declaration.entrypoint)
            && prior != declaration.entrypoint
        {
            violations.push(Violation::global(
                "verification_entrypoint_checker_reused",
                "verification_closure",
                format!(
                    "checker {checker_id:?} is shared by entrypoints {prior:?} and {:?}",
                    declaration.entrypoint
                ),
            ));
        }
        match declaration.status.as_str() {
            "live" => {
                let issues = live_verification_resolution_issues(declaration, catalog, root);
                if issues.is_empty() && declaration.evidence_scope == "governance" {
                    valid_live_governance.insert(declaration.entrypoint.as_str());
                }
                for (code, message) in issues {
                    violations.push(Violation::global(code, "verification_closure", message));
                }
            }
            "planned"
                if declaration.checker_id.is_some()
                    || declaration.package.is_some()
                    || declaration.target.is_some()
                    || declaration.selector.is_some()
                    || declaration.command_argv.is_some() =>
            {
                violations.push(Violation::global(
                    "planned_verification_invocation_present",
                    "verification_closure",
                    format!(
                        "planned verification entrypoint {:?} cannot carry checker or invocation metadata",
                        declaration.entrypoint
                    ),
                ));
            }
            "planned" => {}
            _ => {}
        }
    }

    for decision in registry
        .decisions
        .iter()
        .filter(|decision| decision.status != "superseded")
    {
        let has_live_governance = decision
            .verification_entrypoints
            .iter()
            .any(|entrypoint| valid_live_governance.contains(entrypoint.as_str()));
        if !has_live_governance {
            let claim_class = claim_classes_for(decision, catalog).join("+");
            violations.push(Violation::for_decision(
                decision,
                profiles,
                &claim_class,
                "live_governance_entrypoint_missing",
                "verification_closure",
                "effective decisions require a declared governance entrypoint backed by an exact live checker target",
            ));
        }
    }
}

fn validate_external_review_contract_into(
    registry: &ArchitectureRegistry,
    violations: &mut Vec<Violation>,
) {
    let profiles: BTreeMap<String, &Profile> = registry
        .profiles
        .iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect();
    let decisions: BTreeMap<String, &Decision> = registry
        .decisions
        .iter()
        .map(|decision| (decision.id.clone(), decision))
        .collect();
    let applicable: Vec<&Decision> = registry
        .decisions
        .iter()
        .filter(|decision| requires_external_review(decision))
        .collect();
    if applicable.len() != PINNED_EXTERNAL_REVIEW_DECISION_COUNT {
        violations.push(Violation::global(
            "external_review_applicable_count",
            "external_review_coverage",
            format!(
                "{} active foundation/SOTA decisions require external review, pinned count is {PINNED_EXTERNAL_REVIEW_DECISION_COUNT}",
                applicable.len()
            ),
        ));
    }

    let mut sources = BTreeMap::<String, &ExternalReviewSource>::new();
    for source in &registry.external_review_sources {
        if sources.insert(source.id.clone(), source).is_some() {
            violations.push(Violation::global(
                "external_review_source_id_duplicate",
                "external_review_source_integrity",
                format!("duplicate external review source ID {:?}", source.id),
            ));
        }
        if !valid_stable_id(&source.id, "FG-ADR-EXTSRC-") {
            violations.push(Violation::global(
                "external_review_source_id_format",
                "external_review_source_integrity",
                format!("external review source ID {:?} is malformed", source.id),
            ));
        }
        if !valid_external_source_uri(&source.uri) {
            violations.push(Violation::global(
                "external_review_source_uri",
                "external_review_source_integrity",
                format!(
                    "external review source {:?} URI must be a nonblank HTTPS URI without whitespace",
                    source.id
                ),
            ));
        }
        if !valid_iso_date(&source.published_at) || !valid_iso_date(&source.retrieved_at) {
            violations.push(Violation::global(
                "external_review_source_date",
                "external_review_source_integrity",
                format!(
                    "external review source {:?} has invalid published/retrieved dates {:?}/{:?}",
                    source.id, source.published_at, source.retrieved_at
                ),
            ));
        } else if source.published_at > source.retrieved_at {
            violations.push(Violation::global(
                "external_review_source_date_order",
                "external_review_source_integrity",
                format!(
                    "external review source {:?} was published after it was retrieved",
                    source.id
                ),
            ));
        }
        if !valid_sha256_digest(&source.content_digest) {
            violations.push(Violation::global(
                "external_review_source_digest",
                "external_review_source_integrity",
                format!(
                    "external review source {:?} content_digest must be sha256 plus 64 lowercase hex digits",
                    source.id
                ),
            ));
        }
        let expected = recompute_external_review_source_fingerprint(source);
        if !valid_fnv_fingerprint(&source.source_fingerprint)
            || source.source_fingerprint != expected
        {
            violations.push(Violation::global(
                "external_review_source_fingerprint",
                "external_review_source_integrity",
                format!(
                    "external review source {:?} fingerprint {:?} does not match recomputed {:?}",
                    source.id, source.source_fingerprint, expected
                ),
            ));
        }
    }

    let mut review_ids = BTreeSet::new();
    let mut groups = BTreeMap::<String, Vec<&ExternalReview>>::new();
    let mut referenced_sources = BTreeSet::new();
    for review in &registry.external_reviews {
        if !review_ids.insert(review.id.clone()) {
            violations.push(Violation::global(
                "external_review_id_duplicate",
                "external_review_linearity",
                format!("duplicate external review ID {:?}", review.id),
            ));
        }
        if !valid_stable_id(&review.id, "FG-ADR-REVIEW-") {
            violations.push(Violation::global(
                "external_review_id_format",
                "external_review_linearity",
                format!("external review ID {:?} is malformed", review.id),
            ));
        }
        let decision = decisions.get(&review.decision_id).copied();
        match decision {
            None => violations.push(Violation::global(
                "external_review_decision_unresolved",
                "external_review_coverage",
                format!(
                    "external review {:?} targets unknown decision {:?}",
                    review.id, review.decision_id
                ),
            )),
            Some(decision)
                if !(decision.category.starts_with("foundation_")
                    || decision.category.starts_with("sota_")) =>
            {
                violations.push(Violation::for_decision(
                    decision,
                    &profiles,
                    "architectural_decision",
                    "external_review_decision_scope",
                    "external_review_coverage",
                    format!(
                        "external review {:?} targets non-foundation/SOTA category {:?}",
                        review.id, decision.category
                    ),
                ));
            }
            Some(_) => {
                groups
                    .entry(review.decision_id.clone())
                    .or_default()
                    .push(review);
            }
        }
        if review.sequence == 0 {
            violations.push(Violation::global(
                "external_review_sequence",
                "external_review_linearity",
                format!("external review {:?} sequence must start at one", review.id),
            ));
        }
        if !valid_iso_date(&review.reviewed_at) {
            violations.push(Violation::global(
                "external_review_date",
                "external_review_linearity",
                format!(
                    "external review {:?} reviewed_at {:?} is not an ISO calendar date",
                    review.id, review.reviewed_at
                ),
            ));
        }
        if !valid_fnv_fingerprint(&review.claim_fingerprint) {
            violations.push(Violation::global(
                "external_review_claim_fingerprint_format",
                "external_review_claim_freshness",
                format!(
                    "external review {:?} claim_fingerprint is malformed",
                    review.id
                ),
            ));
        }
        if !ALLOWED_EXTERNAL_REVIEW_OUTCOMES.contains(&review.outcome.as_str()) {
            violations.push(Violation::global(
                "external_review_outcome",
                "external_review_claim_freshness",
                format!(
                    "external review {:?} outcome {:?} is outside the closed set",
                    review.id, review.outcome
                ),
            ));
        }
        if review.source_ids.is_empty()
            || !blank_items(&review.source_ids).is_empty()
            || !duplicates(&review.source_ids).is_empty()
            || !review.source_ids.windows(2).all(|pair| pair[0] < pair[1])
        {
            violations.push(Violation::global(
                "external_review_source_ids",
                "external_review_source_integrity",
                format!(
                    "external review {:?} requires nonblank, sorted, unique source_ids",
                    review.id
                ),
            ));
        }
        for source_id in &review.source_ids {
            referenced_sources.insert(source_id.clone());
            match sources.get(source_id) {
                None => violations.push(Violation::global(
                    "external_review_source_unresolved",
                    "external_review_source_integrity",
                    format!(
                        "external review {:?} references unknown source {:?}",
                        review.id, source_id
                    ),
                )),
                Some(source)
                    if valid_iso_date(&source.retrieved_at)
                        && valid_iso_date(&review.reviewed_at)
                        && source.retrieved_at > review.reviewed_at =>
                {
                    violations.push(Violation::global(
                        "external_review_source_after_review",
                        "external_review_source_integrity",
                        format!(
                            "external review {:?} predates retrieval of source {:?}",
                            review.id, source_id
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
        if !valid_fnv_fingerprint(&review.record_fingerprint) {
            violations.push(Violation::global(
                "external_review_record_fingerprint_format",
                "external_review_linearity",
                format!(
                    "external review {:?} record_fingerprint is malformed",
                    review.id
                ),
            ));
        }
    }

    for source in &registry.external_review_sources {
        if !referenced_sources.contains(&source.id) {
            violations.push(Violation::global(
                "external_review_source_orphan",
                "external_review_source_integrity",
                format!(
                    "external review source {:?} is not referenced by any review",
                    source.id
                ),
            ));
        }
    }

    for decision in applicable {
        let Some(chain) = groups.get_mut(&decision.id) else {
            violations.push(Violation::for_decision(
                decision,
                &profiles,
                "architectural_decision",
                "external_review_coverage_missing",
                "external_review_coverage",
                "active foundation/SOTA decision has no external-review chain",
            ));
            continue;
        };
        chain.sort_by(|left, right| {
            (left.sequence, left.id.as_str()).cmp(&(right.sequence, right.id.as_str()))
        });
        let mut previous: Option<&ExternalReview> = None;
        let mut previous_expected_fingerprint: Option<String> = None;
        for (index, review) in chain.iter().enumerate() {
            let expected_sequence = index + 1;
            if review.sequence != expected_sequence {
                violations.push(Violation::for_decision(
                    decision,
                    &profiles,
                    "architectural_decision",
                    "external_review_sequence",
                    "external_review_linearity",
                    format!(
                        "review {:?} has sequence {}, expected contiguous sequence {expected_sequence}",
                        review.id, review.sequence
                    ),
                ));
            }
            let expected_predecessor = previous.map_or("", |prior| prior.id.as_str());
            if review.predecessor != expected_predecessor {
                violations.push(Violation::for_decision(
                    decision,
                    &profiles,
                    "architectural_decision",
                    "external_review_predecessor",
                    "external_review_linearity",
                    format!(
                        "review {:?} predecessor {:?} does not equal prior review {:?}",
                        review.id, review.predecessor, expected_predecessor
                    ),
                ));
            }
            if previous.is_some_and(|prior| {
                valid_iso_date(&prior.reviewed_at)
                    && valid_iso_date(&review.reviewed_at)
                    && prior.reviewed_at > review.reviewed_at
            }) {
                violations.push(Violation::for_decision(
                    decision,
                    &profiles,
                    "architectural_decision",
                    "external_review_date_order",
                    "external_review_linearity",
                    format!(
                        "review {:?} predates its predecessor {:?}",
                        review.id, review.predecessor
                    ),
                ));
            }
            if let Ok(expected) = external_review_record_fingerprint_with_predecessor(
                review,
                &sources,
                previous_expected_fingerprint.as_deref(),
            ) {
                if review.record_fingerprint != expected {
                    violations.push(Violation::for_decision(
                        decision,
                        &profiles,
                        "architectural_decision",
                        "external_review_record_fingerprint",
                        "external_review_linearity",
                        format!(
                            "review {:?} fingerprint {:?} does not match recomputed {:?}",
                            review.id, review.record_fingerprint, expected
                        ),
                    ));
                }
                previous_expected_fingerprint = Some(expected);
            }
            previous = Some(review);
        }

        let tip = chain
            .last()
            .expect("external-review coverage rejected empty chains");
        if let Some(profile) = profiles.get(&decision.profile) {
            let expected = recompute_external_review_claim_fingerprint(decision, profile);
            if tip.claim_fingerprint != expected {
                violations.push(Violation::for_decision(
                    decision,
                    &profiles,
                    "architectural_decision",
                    "external_review_claim_stale",
                    "external_review_claim_freshness",
                    format!(
                        "review tip {:?} binds claim fingerprint {:?}, current claim recomputes to {:?}",
                        tip.id, tip.claim_fingerprint, expected
                    ),
                ));
            }
        }
        let expected_outcome = match decision.status.as_str() {
            "frozen" => Some("current"),
            "review_due" => Some("drift_detected"),
            _ => None,
        };
        if expected_outcome.is_some_and(|expected| tip.outcome != expected) {
            violations.push(Violation::for_decision(
                decision,
                &profiles,
                "architectural_decision",
                "external_review_tip_status",
                "external_review_claim_freshness",
                format!(
                    "decision status {:?} requires review tip outcome {:?}, found {:?}",
                    decision.status,
                    expected_outcome.unwrap_or_default(),
                    tip.outcome
                ),
            ));
        }
    }
    let history_hash = recompute_external_review_history_hash(registry);
    if registry.registry.external_review_history_hash != history_hash {
        violations.push(Violation::global(
            "external_review_history_hash_mismatch",
            "external_review_append_only",
            format!(
                "registry external-review history hash {:?} does not match recomputed {:?}",
                registry.registry.external_review_history_hash, history_hash
            ),
        ));
    }
    if history_hash != PINNED_EXTERNAL_REVIEW_HISTORY_HASH {
        violations.push(Violation::global(
            "independent_external_review_history_hash_mismatch",
            "external_review_append_only",
            format!(
                "external-review history hash {history_hash:?} differs from independent code pin {PINNED_EXTERNAL_REVIEW_HISTORY_HASH:?}"
            ),
        ));
    }
}

/// Validate the per-decision external-review source and append-only chain
/// contract independently of the whole-registry semantic pin.
pub fn validate_external_review_contract(registry: &ArchitectureRegistry) -> Vec<Violation> {
    let mut violations = Vec::new();
    validate_external_review_contract_into(registry, &mut violations);
    violations.sort();
    violations.dedup();
    violations
}

fn validate_typed_category_coverage(
    registry: &ArchitectureRegistry,
    catalog: &ReferenceCatalog,
    violations: &mut Vec<Violation>,
) {
    let thesis_refs: BTreeSet<String> = registry
        .decisions
        .iter()
        .filter(|decision| decision.category == "thesis_bet")
        .flat_map(|decision| decision.affected_bets.iter().cloned())
        .collect();
    if thesis_refs != catalog.bets {
        violations.push(Violation::global(
            "thesis_bet_coverage",
            "reference_closure",
            format!(
                "thesis_bet rows cover {thesis_refs:?}, live bet set is {:?}",
                catalog.bets
            ),
        ));
    }
    for decision in registry
        .decisions
        .iter()
        .filter(|decision| decision.category == "thesis_bet")
    {
        if decision.affected_bets.len() != 1 {
            violations.push(Violation::global(
                "thesis_bet_cardinality",
                "reference_closure",
                format!(
                    "thesis decision {:?} must bind exactly one B1..B6 row",
                    decision.id
                ),
            ));
        }
    }
    let constraint_refs: BTreeSet<String> = registry
        .decisions
        .iter()
        .filter(|decision| decision.category == "constraint")
        .flat_map(|decision| decision.affected_constraints.iter().cloned())
        .collect();
    if constraint_refs != catalog.constraints {
        violations.push(Violation::global(
            "constraint_coverage",
            "reference_closure",
            format!(
                "constraint rows cover {constraint_refs:?}, live constraint set is {:?}",
                catalog.constraints
            ),
        ));
    }
    for decision in registry
        .decisions
        .iter()
        .filter(|decision| decision.category == "constraint")
    {
        if decision.affected_constraints.len() != 1 {
            violations.push(Violation::global(
                "constraint_cardinality",
                "reference_closure",
                format!(
                    "constraint decision {:?} must bind exactly one FG-CON row",
                    decision.id
                ),
            ));
        }
    }
}

/// Validate every architecture contract.  Diagnostics are sorted by their
/// complete stable tuple so identical inputs always produce identical output.
pub fn validate_architecture(registry: &ArchitectureRegistry, root: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    validate_header(registry, &mut violations);
    validate_bead_policy_shape(registry, &mut violations);
    validate_category_counts(registry, &mut violations);
    validate_hash_and_ids(registry, &mut violations);
    let profiles = validate_profiles(registry, &mut violations);
    validate_sources(registry, root, &mut violations);
    validate_external_review_contract_into(registry, &mut violations);

    let catalog = match load_reference_catalog(root) {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            violations.push(Violation::global(
                "reference_catalog_load",
                "reference_closure",
                error,
            ));
            None
        }
    };
    let beads = match load_bead_records(root, &registry.bead_provenance.source_path) {
        Ok(records) => Some(records),
        Err(error) => {
            violations.push(Violation::global(
                "bead_index_load",
                "bead_provenance",
                error,
            ));
            None
        }
    };
    let bead_ids = beads.as_ref().map(|records| {
        records
            .iter()
            .map(|record| record.id.clone())
            .collect::<BTreeSet<_>>()
    });
    let default_catalog = ReferenceCatalog::default();
    let catalog_ref = catalog.as_ref().unwrap_or(&default_catalog);

    for decision in &registry.decisions {
        let claim_classes = claim_classes_for(decision, catalog_ref);
        let claim_class = claim_classes.join("+");
        validate_decision_shape(decision, &profiles, &claim_class, registry, &mut violations);
        validate_disposition(decision, &profiles, &claim_class, &mut violations);
        validate_owners(
            decision,
            &profiles,
            &claim_class,
            registry,
            bead_ids.as_ref(),
            &mut violations,
        );
        if let Some(catalog) = &catalog {
            validate_references(
                decision,
                &profiles,
                &claim_class,
                catalog,
                root,
                &mut violations,
            );
        }
    }
    if let Some(catalog) = &catalog {
        validate_verification_entrypoint_registry(
            registry,
            &profiles,
            catalog,
            root,
            &mut violations,
        );
        validate_typed_category_coverage(registry, catalog, &mut violations);
    }
    if let Some(beads) = &beads {
        validate_bead_resolution(registry, beads, &mut violations);
    }

    // Explicit owner edges retain their own reciprocal proof in addition to
    // the total Beads resolver above.
    if registry.registry.ownership_scope == OWNERSHIP_SCOPE {
        let index = owner_decision_index(registry);
        let effective_owner_edges: usize = registry
            .decisions
            .iter()
            .filter(|decision| decision.status != "superseded")
            .map(|decision| decision.owner_beads.len())
            .sum();
        let indexed_edges: usize = index.values().map(Vec::len).sum();
        if indexed_edges != effective_owner_edges {
            violations.push(Violation::global(
                "owner_reverse_index",
                "owner_closure",
                format!(
                    "reverse index has {indexed_edges} unique edges, decisions declare {effective_owner_edges}"
                ),
            ));
        }
        let provenance = provenance_index(registry);
        let declared_provenance_edges: usize = registry
            .decisions
            .iter()
            .filter(|decision| decision.status != "superseded")
            .map(|decision| {
                decision.owner_beads.len()
                    + decision.owner_crates.len()
                    + decision.checker_ids.len()
                    + decision.evidence_ids.len()
            })
            .sum();
        let indexed_provenance_edges: usize = provenance
            .iter()
            .map(|entry| entry.decision_ids.len())
            .sum();
        if declared_provenance_edges != indexed_provenance_edges {
            violations.push(Violation::global(
                "provenance_reverse_index",
                "owner_closure",
                format!(
                    "general provenance index has {indexed_provenance_edges} edges, decisions declare {declared_provenance_edges}"
                ),
            ));
        }
        for entry in provenance {
            if entry.profile_ids.is_empty() || entry.rationales.is_empty() {
                violations.push(Violation::global(
                    "provenance_rationale_missing",
                    "profile_closure",
                    format!(
                        "{} owner {:?} cannot walk back to a profile and rationale",
                        entry.owner_kind, entry.owner_id
                    ),
                ));
            }
        }
    }

    violations.sort();
    violations.dedup();
    violations
}
