//! threat.rs — the G0 threat/trust model registry (fgdb-g0-threat-model-nqd).
//!
//! Loads `registries/threat_model.toml`, validates it, expands its three
//! matrices, generates `docs/THREAT_AND_TRUST_MODEL.md`, and scans the
//! generated normative text for claims that exceed the trust matrix.
//!
//! The three laws that live here and nowhere else:
//!
//!   1. **Exposure completeness.** Every (actor, asset) cell is dispositioned
//!      exactly once, naming a registered assumption. A missing cell is the
//!      failure this exists to catch — an unstated exposure reads exactly like
//!      a defended one.
//!   2. **Narrowing-only attenuation.** The authority lattice's order IS the
//!      macaroon attenuation law. Every dimension declares one narrowing
//!      operator; every prohibition names a negative fixture.
//!   3. **Posture product-space closure.** Every cell of the declared product
//!      space is registered, deferred with a named owner bead, or excluded by
//!      a named exclusion law. A posture list nobody can prove complete is a
//!      posture list that silently drops a deployment.
//!
//! Std-only by constitution: the closed dependency universe (FG-CON-01)
//! applies to the tooling that enforces it.

use crate::hash::{fnv1a64, sha256_hex};
use crate::toml::{
    ReadError, Table, Value, get_int, get_str, get_str_array, get_table, get_table_array, parse,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;
pub const REGISTRY_NAME: &str = "threat_model";
pub const REGISTRY_PATH: &str = "registries/threat_model.toml";
pub const DOCUMENT_PATH: &str = "docs/THREAT_AND_TRUST_MODEL.md";
pub const REPLAY_COMMAND: &str = "cargo run -p registry-check --bin threat-check -- --root .";
pub const PLAN_PATH: &str = "COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md";

/// The eight actors of §12.1, in source order. Frozen here so a rename or a
/// dropped actor is a compile-adjacent failure rather than a quiet edit.
pub const ACTOR_ORDER: [&str; 8] = [
    "untrusted_client",
    "mutually_hostile_tenant",
    "honest_but_curious_storage",
    "crash_fault_replica",
    "malicious_or_stale_donor",
    "independent_transparency_witness",
    "trusted_embedded_host_code",
    "compromised_operator_or_server",
];

/// The nine stable identities of §12.1, in source order.
pub const IDENTITY_ORDER: [&str; 9] = [
    "TenantId",
    "PrincipalId",
    "IssuerId",
    "TokenId",
    "DatabaseId",
    "SecurityPolicyEpoch",
    "RevocationIndex",
    "DecisionPolicyEpoch",
    "KeyEpoch",
];

/// The closed set of sixteen operation classes, in §12.1 source order.
/// Source order, not alphabetical: the census sorts, the source spells.
pub const OPERATION_CLASS_ORDER: [&str; 16] = [
    "Read",
    "Mutate",
    "Ddl",
    "Subscribe",
    "Analytics",
    "Replay",
    "ExecuteProcedure",
    "InstallModule",
    "ExternalIo",
    "Export",
    "Backup",
    "Restore",
    "Observe",
    "Admin",
    "KeyManage",
    "Replicate",
];

/// The eleven external authorities of §16 item 8, in source order.
pub const EXTERNAL_AUTHORITY_ORDER: [&str; 11] = [
    "identity_allocation_continuity",
    "cluster_incarnation",
    "time_authority",
    "audit_continuity",
    "dp_registry",
    "archive_grant",
    "reservation",
    "catalog",
    "restore_dispatch_journal",
    "transparency_witness",
    "kms_hsm",
];

pub const ALLOWED_TRUST_CLASSES: [&str; 7] = [
    "untrusted",
    "hostile",
    "honest_but_curious",
    "crash_fault",
    "potentially_malicious",
    "independent_verifier",
    "trusted",
];

pub const ALLOWED_IDENTITY_KINDS: [&str; 4] = [
    "stable_identity",
    "security_epoch",
    "adaptive_epoch",
    "monotone_index",
];

pub const ALLOWED_EPOCH_DOMAINS: [&str; 3] = ["security", "adaptive", "none"];

pub const ALLOWED_NARROWING_OPERATORS: [&str; 8] = [
    "fixed",
    "intersect",
    "raise_only",
    "lower_only",
    "append_only",
    "restrict_disclosure_only",
    "restrict_binding_only",
    "current_state_monotone",
];

pub const ALLOWED_CLAIM_CLASSES: [&str; 6] = [
    "invariant",
    "proof",
    "bounded_model",
    "statistical",
    "slo",
    "benchmark",
];

pub const ALLOWED_FOOTPRINT_DECLARATIONS: [&str; 2] = ["complete", "empty"];
pub const ALLOWED_DISPOSITIONS: [&str; 3] = ["defended", "conditional", "undefended"];
pub const ALLOWED_ATTENUATION_CLASSES: [&str; 2] = ["permitted", "prohibited"];
pub const ALLOWED_OPERATION_CLASS_BASES: [&str; 2] = ["named_in_source", "trigger_site_only"];
pub const ALLOWED_POSTURE_STATUSES: [&str; 1] = ["frozen"];

pub const SERVICE_CLASS_AXIS: [&str; 2] = ["Operational", "ArchiveReadOnly"];
pub const ROLE_POSTURE_AXIS: [&str; 2] = ["Local", "Sharded"];
pub const CONTINUITY_PROFILE_AXIS: [&str; 3] = ["DirectoryBound", "ExternalCas", "NotApplicable"];

// -----------------------------------------------------------------------------
// Model
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RegistryHeader {
    pub name: String,
    pub document_path: String,
    pub replay_command: String,
    pub bound_invariants: Vec<String>,
    pub bound_evidence: Vec<String>,
    pub actor_count: usize,
    pub asset_count: usize,
    pub assumption_count: usize,
    pub out_of_scope_count: usize,
    pub identity_count: usize,
    pub operation_class_count: usize,
    pub authority_dimension_count: usize,
    pub presentation_binding_count: usize,
    pub binding_transition_count: usize,
    pub attenuation_law_count: usize,
    pub external_authority_count: usize,
    pub posture_count: usize,
    pub deferred_posture_count: usize,
    pub exclusion_law_count: usize,
    pub footprint_cell_count: usize,
    pub exposure_cell_count: usize,
    pub id_table_hash: String,
    pub semantic_contract_hash: String,
}

#[derive(Debug, Clone)]
pub struct SourceBlock {
    pub id: String,
    pub plan_path: String,
    pub plan_start_line: usize,
    pub plan_end_line: usize,
    pub line_count: usize,
    pub byte_count: usize,
    pub fnv1a64: String,
    pub covers: String,
}

#[derive(Debug, Clone)]
pub struct Actor {
    pub id: String,
    pub title: String,
    pub source_order: usize,
    pub trust_class: String,
    pub inside_trust_boundary: bool,
    pub summary: String,
    pub source_anchor: String,
    pub defended_assets: Vec<String>,
    pub conditional_assets: Vec<String>,
    pub undefended_assets: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub id: String,
    pub title: String,
    pub source_order: usize,
    pub summary: String,
    pub primary_claim_class: String,
    pub primary_claim_ref: String,
}

#[derive(Debug, Clone)]
pub struct Assumption {
    pub id: String,
    pub statement: String,
    pub bounds: String,
    pub source_anchor: String,
}

#[derive(Debug, Clone)]
pub struct OutOfScope {
    pub id: String,
    pub title: String,
    pub statement: String,
    pub rationale: String,
    pub rejection_anchor: String,
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub source_order: usize,
    pub kind: String,
    pub epoch_domain: String,
    pub rust_newtype: String,
    pub wire_tag: String,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct OperationClass {
    pub name: String,
    pub ordinal: usize,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct AuthorityDimension {
    pub id: String,
    pub source_order: usize,
    pub narrowing_operator: String,
    pub summary: String,
    pub source_anchor: String,
}

#[derive(Debug, Clone)]
pub struct PresentationBinding {
    pub name: String,
    pub rank: usize,
    pub summary: String,
    pub source_anchor: String,
}

#[derive(Debug, Clone)]
pub struct BindingTransition {
    pub from: String,
    pub to: String,
    pub legal: bool,
    pub law: String,
}

#[derive(Debug, Clone)]
pub struct AttenuationLaw {
    pub id: String,
    pub class: String,
    pub mutation: String,
    pub statement: String,
    pub dimension_ids: Vec<String>,
    pub negative_fixture: String,
    pub source_anchor: String,
}

#[derive(Debug, Clone)]
pub struct ExternalAuthority {
    pub id: String,
    pub title: String,
    pub source_order: usize,
    pub record_kinds: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct ProductSpace {
    pub service_class_axis: Vec<String>,
    pub role_posture_axis: Vec<String>,
    pub continuity_profile_axis: Vec<String>,
    pub cell_count: usize,
}

#[derive(Debug, Clone)]
pub struct ExclusionLaw {
    pub id: String,
    pub statement: String,
    pub rationale: String,
    pub source_anchor: String,
}

#[derive(Debug, Clone)]
pub struct Posture {
    pub id: String,
    pub title: String,
    pub service_class: String,
    pub role_posture: String,
    pub continuity_profile: String,
    pub status: String,
    pub footprint_declaration: String,
    pub empty_justification: String,
    pub source_anchor: String,
}

#[derive(Debug, Clone)]
pub struct DeferredPosture {
    pub id: String,
    pub title: String,
    pub service_class: String,
    pub role_posture: String,
    pub continuity_profile: String,
    pub owner_bead: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Footprint {
    pub posture_id: String,
    pub authority_id: String,
    pub on_synchronous_path: bool,
    pub touch_count: usize,
    pub sync_path_position: String,
    pub operation_class_basis: String,
    pub operation_classes: Vec<String>,
    pub deferred_binding_owner: String,
}

#[derive(Debug, Clone)]
pub struct ClaimScanRule {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub qualifiers: Vec<String>,
    pub trust_matrix_conflict: String,
    pub severity: String,
}

#[derive(Debug, Clone)]
pub struct ThreatRegistry {
    pub schema_version: i64,
    pub registry: RegistryHeader,
    pub source_blocks: Vec<SourceBlock>,
    pub actors: Vec<Actor>,
    pub assets: Vec<Asset>,
    pub assumptions: Vec<Assumption>,
    pub out_of_scope: Vec<OutOfScope>,
    pub identities: Vec<Identity>,
    pub operation_classes: Vec<OperationClass>,
    pub authority_dimensions: Vec<AuthorityDimension>,
    pub presentation_bindings: Vec<PresentationBinding>,
    pub binding_transitions: Vec<BindingTransition>,
    pub attenuation_laws: Vec<AttenuationLaw>,
    pub external_authorities: Vec<ExternalAuthority>,
    pub product_space: ProductSpace,
    pub exclusion_laws: Vec<ExclusionLaw>,
    pub postures: Vec<Posture>,
    pub deferred_postures: Vec<DeferredPosture>,
    pub footprints: Vec<Footprint>,
    pub claim_scan_rules: Vec<ClaimScanRule>,
}

#[derive(Debug, Clone)]
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
    fn from(error: ReadError) -> Self {
        LoadError {
            path: error.path,
            message: error.msg,
        }
    }
}

// -----------------------------------------------------------------------------
// Loading
// -----------------------------------------------------------------------------

fn exact_keys(table: &Table, allowed: &[&str], ctx: &str) -> Result<(), ReadError> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ReadError {
                path: format!("{ctx}.{key}"),
                msg: "unknown key (the registry schema denies unknown keys)".into(),
            });
        }
    }
    Ok(())
}

fn usize_field(table: &Table, key: &str, ctx: &str) -> Result<usize, ReadError> {
    let raw = get_int(table, key, ctx)?;
    usize::try_from(raw).map_err(|_| ReadError {
        path: format!("{ctx}.{key}"),
        msg: format!("expected a non-negative integer, found {raw}"),
    })
}

fn bool_field(table: &Table, key: &str, ctx: &str) -> Result<bool, ReadError> {
    match table.get(key) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected boolean".into(),
        }),
        None => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "missing required key".into(),
        }),
    }
}

fn header_from(table: &Table) -> Result<RegistryHeader, ReadError> {
    let ctx = "threat_model.toml.registry";
    exact_keys(
        table,
        &[
            "name",
            "document_path",
            "replay_command",
            "bound_invariants",
            "bound_evidence",
            "actor_count",
            "asset_count",
            "assumption_count",
            "out_of_scope_count",
            "identity_count",
            "operation_class_count",
            "authority_dimension_count",
            "presentation_binding_count",
            "binding_transition_count",
            "attenuation_law_count",
            "external_authority_count",
            "posture_count",
            "deferred_posture_count",
            "exclusion_law_count",
            "footprint_cell_count",
            "exposure_cell_count",
            "id_table_hash",
            "semantic_contract_hash",
        ],
        ctx,
    )?;
    Ok(RegistryHeader {
        name: get_str(table, "name", ctx)?,
        document_path: get_str(table, "document_path", ctx)?,
        replay_command: get_str(table, "replay_command", ctx)?,
        bound_invariants: get_str_array(table, "bound_invariants", ctx)?,
        bound_evidence: get_str_array(table, "bound_evidence", ctx)?,
        actor_count: usize_field(table, "actor_count", ctx)?,
        asset_count: usize_field(table, "asset_count", ctx)?,
        assumption_count: usize_field(table, "assumption_count", ctx)?,
        out_of_scope_count: usize_field(table, "out_of_scope_count", ctx)?,
        identity_count: usize_field(table, "identity_count", ctx)?,
        operation_class_count: usize_field(table, "operation_class_count", ctx)?,
        authority_dimension_count: usize_field(table, "authority_dimension_count", ctx)?,
        presentation_binding_count: usize_field(table, "presentation_binding_count", ctx)?,
        binding_transition_count: usize_field(table, "binding_transition_count", ctx)?,
        attenuation_law_count: usize_field(table, "attenuation_law_count", ctx)?,
        external_authority_count: usize_field(table, "external_authority_count", ctx)?,
        posture_count: usize_field(table, "posture_count", ctx)?,
        deferred_posture_count: usize_field(table, "deferred_posture_count", ctx)?,
        exclusion_law_count: usize_field(table, "exclusion_law_count", ctx)?,
        footprint_cell_count: usize_field(table, "footprint_cell_count", ctx)?,
        exposure_cell_count: usize_field(table, "exposure_cell_count", ctx)?,
        id_table_hash: get_str(table, "id_table_hash", ctx)?,
        semantic_contract_hash: get_str(table, "semantic_contract_hash", ctx)?,
    })
}

fn source_block_from(table: &Table, index: usize) -> Result<SourceBlock, ReadError> {
    let ctx = format!("threat_model.toml.source_block[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "plan_path",
            "plan_start_line",
            "plan_end_line",
            "line_count",
            "byte_count",
            "fnv1a64",
            "covers",
        ],
        &ctx,
    )?;
    Ok(SourceBlock {
        id: get_str(table, "id", &ctx)?,
        plan_path: get_str(table, "plan_path", &ctx)?,
        plan_start_line: usize_field(table, "plan_start_line", &ctx)?,
        plan_end_line: usize_field(table, "plan_end_line", &ctx)?,
        line_count: usize_field(table, "line_count", &ctx)?,
        byte_count: usize_field(table, "byte_count", &ctx)?,
        fnv1a64: get_str(table, "fnv1a64", &ctx)?,
        covers: get_str(table, "covers", &ctx)?,
    })
}

fn actor_from(table: &Table, index: usize) -> Result<Actor, ReadError> {
    let ctx = format!("threat_model.toml.actor[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "title",
            "source_order",
            "trust_class",
            "inside_trust_boundary",
            "summary",
            "source_anchor",
            "defended_assets",
            "conditional_assets",
            "undefended_assets",
        ],
        &ctx,
    )?;
    Ok(Actor {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        source_order: usize_field(table, "source_order", &ctx)?,
        trust_class: get_str(table, "trust_class", &ctx)?,
        inside_trust_boundary: bool_field(table, "inside_trust_boundary", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
        defended_assets: get_str_array(table, "defended_assets", &ctx)?,
        conditional_assets: get_str_array(table, "conditional_assets", &ctx)?,
        undefended_assets: get_str_array(table, "undefended_assets", &ctx)?,
    })
}

fn asset_from(table: &Table, index: usize) -> Result<Asset, ReadError> {
    let ctx = format!("threat_model.toml.asset[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "title",
            "source_order",
            "summary",
            "primary_claim_class",
            "primary_claim_ref",
        ],
        &ctx,
    )?;
    Ok(Asset {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        source_order: usize_field(table, "source_order", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
        primary_claim_class: get_str(table, "primary_claim_class", &ctx)?,
        primary_claim_ref: get_str(table, "primary_claim_ref", &ctx)?,
    })
}

fn assumption_from(table: &Table, index: usize) -> Result<Assumption, ReadError> {
    let ctx = format!("threat_model.toml.assumption[{index}]");
    exact_keys(table, &["id", "statement", "bounds", "source_anchor"], &ctx)?;
    Ok(Assumption {
        id: get_str(table, "id", &ctx)?,
        statement: get_str(table, "statement", &ctx)?,
        bounds: get_str(table, "bounds", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
    })
}

fn out_of_scope_from(table: &Table, index: usize) -> Result<OutOfScope, ReadError> {
    let ctx = format!("threat_model.toml.out_of_scope[{index}]");
    exact_keys(
        table,
        &["id", "title", "statement", "rationale", "rejection_anchor"],
        &ctx,
    )?;
    Ok(OutOfScope {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        statement: get_str(table, "statement", &ctx)?,
        rationale: get_str(table, "rationale", &ctx)?,
        rejection_anchor: get_str(table, "rejection_anchor", &ctx)?,
    })
}

fn identity_from(table: &Table, index: usize) -> Result<Identity, ReadError> {
    let ctx = format!("threat_model.toml.identity[{index}]");
    exact_keys(
        table,
        &[
            "name",
            "source_order",
            "kind",
            "epoch_domain",
            "rust_newtype",
            "wire_tag",
            "summary",
        ],
        &ctx,
    )?;
    Ok(Identity {
        name: get_str(table, "name", &ctx)?,
        source_order: usize_field(table, "source_order", &ctx)?,
        kind: get_str(table, "kind", &ctx)?,
        epoch_domain: get_str(table, "epoch_domain", &ctx)?,
        rust_newtype: get_str(table, "rust_newtype", &ctx)?,
        wire_tag: get_str(table, "wire_tag", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
    })
}

fn operation_class_from(table: &Table, index: usize) -> Result<OperationClass, ReadError> {
    let ctx = format!("threat_model.toml.operation_class[{index}]");
    exact_keys(table, &["name", "ordinal", "summary"], &ctx)?;
    Ok(OperationClass {
        name: get_str(table, "name", &ctx)?,
        ordinal: usize_field(table, "ordinal", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
    })
}

fn authority_dimension_from(table: &Table, index: usize) -> Result<AuthorityDimension, ReadError> {
    let ctx = format!("threat_model.toml.authority_dimension[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "source_order",
            "narrowing_operator",
            "summary",
            "source_anchor",
        ],
        &ctx,
    )?;
    Ok(AuthorityDimension {
        id: get_str(table, "id", &ctx)?,
        source_order: usize_field(table, "source_order", &ctx)?,
        narrowing_operator: get_str(table, "narrowing_operator", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
    })
}

fn presentation_binding_from(table: &Table, index: usize) -> Result<PresentationBinding, ReadError> {
    let ctx = format!("threat_model.toml.presentation_binding[{index}]");
    exact_keys(table, &["name", "rank", "summary", "source_anchor"], &ctx)?;
    Ok(PresentationBinding {
        name: get_str(table, "name", &ctx)?,
        rank: usize_field(table, "rank", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
    })
}

fn binding_transition_from(table: &Table, index: usize) -> Result<BindingTransition, ReadError> {
    let ctx = format!("threat_model.toml.binding_transition[{index}]");
    exact_keys(table, &["from", "to", "legal", "law"], &ctx)?;
    Ok(BindingTransition {
        from: get_str(table, "from", &ctx)?,
        to: get_str(table, "to", &ctx)?,
        legal: bool_field(table, "legal", &ctx)?,
        law: get_str(table, "law", &ctx)?,
    })
}

fn attenuation_law_from(table: &Table, index: usize) -> Result<AttenuationLaw, ReadError> {
    let ctx = format!("threat_model.toml.attenuation_law[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "class",
            "mutation",
            "statement",
            "dimension_ids",
            "negative_fixture",
            "source_anchor",
        ],
        &ctx,
    )?;
    Ok(AttenuationLaw {
        id: get_str(table, "id", &ctx)?,
        class: get_str(table, "class", &ctx)?,
        mutation: get_str(table, "mutation", &ctx)?,
        statement: get_str(table, "statement", &ctx)?,
        dimension_ids: get_str_array(table, "dimension_ids", &ctx)?,
        negative_fixture: get_str(table, "negative_fixture", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
    })
}

fn external_authority_from(table: &Table, index: usize) -> Result<ExternalAuthority, ReadError> {
    let ctx = format!("threat_model.toml.external_authority[{index}]");
    exact_keys(
        table,
        &["id", "title", "source_order", "record_kinds", "summary"],
        &ctx,
    )?;
    Ok(ExternalAuthority {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        source_order: usize_field(table, "source_order", &ctx)?,
        record_kinds: get_str_array(table, "record_kinds", &ctx)?,
        summary: get_str(table, "summary", &ctx)?,
    })
}

fn product_space_from(table: &Table) -> Result<ProductSpace, ReadError> {
    let ctx = "threat_model.toml.posture_product_space";
    exact_keys(
        table,
        &[
            "service_class_axis",
            "role_posture_axis",
            "continuity_profile_axis",
            "cell_count",
        ],
        ctx,
    )?;
    Ok(ProductSpace {
        service_class_axis: get_str_array(table, "service_class_axis", ctx)?,
        role_posture_axis: get_str_array(table, "role_posture_axis", ctx)?,
        continuity_profile_axis: get_str_array(table, "continuity_profile_axis", ctx)?,
        cell_count: usize_field(table, "cell_count", ctx)?,
    })
}

fn exclusion_law_from(table: &Table, index: usize) -> Result<ExclusionLaw, ReadError> {
    let ctx = format!("threat_model.toml.exclusion_law[{index}]");
    exact_keys(
        table,
        &["id", "statement", "rationale", "source_anchor"],
        &ctx,
    )?;
    Ok(ExclusionLaw {
        id: get_str(table, "id", &ctx)?,
        statement: get_str(table, "statement", &ctx)?,
        rationale: get_str(table, "rationale", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
    })
}

fn posture_from(table: &Table, index: usize) -> Result<Posture, ReadError> {
    let ctx = format!("threat_model.toml.posture[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "title",
            "service_class",
            "role_posture",
            "continuity_profile",
            "status",
            "footprint_declaration",
            "empty_justification",
            "source_anchor",
        ],
        &ctx,
    )?;
    Ok(Posture {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        service_class: get_str(table, "service_class", &ctx)?,
        role_posture: get_str(table, "role_posture", &ctx)?,
        continuity_profile: get_str(table, "continuity_profile", &ctx)?,
        status: get_str(table, "status", &ctx)?,
        footprint_declaration: get_str(table, "footprint_declaration", &ctx)?,
        empty_justification: get_str(table, "empty_justification", &ctx)?,
        source_anchor: get_str(table, "source_anchor", &ctx)?,
    })
}

fn deferred_posture_from(table: &Table, index: usize) -> Result<DeferredPosture, ReadError> {
    let ctx = format!("threat_model.toml.deferred_posture[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "title",
            "service_class",
            "role_posture",
            "continuity_profile",
            "owner_bead",
            "reason",
        ],
        &ctx,
    )?;
    Ok(DeferredPosture {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        service_class: get_str(table, "service_class", &ctx)?,
        role_posture: get_str(table, "role_posture", &ctx)?,
        continuity_profile: get_str(table, "continuity_profile", &ctx)?,
        owner_bead: get_str(table, "owner_bead", &ctx)?,
        reason: get_str(table, "reason", &ctx)?,
    })
}

fn footprint_from(table: &Table, index: usize) -> Result<Footprint, ReadError> {
    let ctx = format!("threat_model.toml.footprint[{index}]");
    exact_keys(
        table,
        &[
            "posture_id",
            "authority_id",
            "on_synchronous_path",
            "touch_count",
            "sync_path_position",
            "operation_class_basis",
            "operation_classes",
            "deferred_binding_owner",
        ],
        &ctx,
    )?;
    Ok(Footprint {
        posture_id: get_str(table, "posture_id", &ctx)?,
        authority_id: get_str(table, "authority_id", &ctx)?,
        on_synchronous_path: bool_field(table, "on_synchronous_path", &ctx)?,
        touch_count: usize_field(table, "touch_count", &ctx)?,
        sync_path_position: get_str(table, "sync_path_position", &ctx)?,
        operation_class_basis: get_str(table, "operation_class_basis", &ctx)?,
        operation_classes: get_str_array(table, "operation_classes", &ctx)?,
        deferred_binding_owner: get_str(table, "deferred_binding_owner", &ctx)?,
    })
}

fn claim_scan_rule_from(table: &Table, index: usize) -> Result<ClaimScanRule, ReadError> {
    let ctx = format!("threat_model.toml.claim_scan_rule[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "subject",
            "predicate",
            "qualifiers",
            "trust_matrix_conflict",
            "severity",
        ],
        &ctx,
    )?;
    Ok(ClaimScanRule {
        id: get_str(table, "id", &ctx)?,
        subject: get_str(table, "subject", &ctx)?,
        predicate: get_str(table, "predicate", &ctx)?,
        qualifiers: get_str_array(table, "qualifiers", &ctx)?,
        trust_matrix_conflict: get_str(table, "trust_matrix_conflict", &ctx)?,
        severity: get_str(table, "severity", &ctx)?,
    })
}

fn rows<T>(
    root: &Table,
    key: &str,
    build: impl Fn(&Table, usize) -> Result<T, ReadError>,
) -> Result<Vec<T>, ReadError> {
    let tables = get_table_array(root, key, "threat_model.toml")?;
    let mut out = Vec::with_capacity(tables.len());
    for (index, table) in tables.into_iter().enumerate() {
        out.push(build(table, index)?);
    }
    Ok(out)
}

pub fn threat_from(root: &Table) -> Result<ThreatRegistry, ReadError> {
    exact_keys(
        root,
        &[
            "schema_version",
            "registry",
            "source_block",
            "actor",
            "asset",
            "assumption",
            "out_of_scope",
            "identity",
            "operation_class",
            "authority_dimension",
            "presentation_binding",
            "binding_transition",
            "attenuation_law",
            "external_authority",
            "posture_product_space",
            "exclusion_law",
            "posture",
            "deferred_posture",
            "footprint",
            "claim_scan_rule",
        ],
        "threat_model.toml",
    )?;
    Ok(ThreatRegistry {
        schema_version: get_int(root, "schema_version", "threat_model.toml")?,
        registry: header_from(get_table(root, "registry", "threat_model.toml")?)?,
        source_blocks: rows(root, "source_block", source_block_from)?,
        actors: rows(root, "actor", actor_from)?,
        assets: rows(root, "asset", asset_from)?,
        assumptions: rows(root, "assumption", assumption_from)?,
        out_of_scope: rows(root, "out_of_scope", out_of_scope_from)?,
        identities: rows(root, "identity", identity_from)?,
        operation_classes: rows(root, "operation_class", operation_class_from)?,
        authority_dimensions: rows(root, "authority_dimension", authority_dimension_from)?,
        presentation_bindings: rows(root, "presentation_binding", presentation_binding_from)?,
        binding_transitions: rows(root, "binding_transition", binding_transition_from)?,
        attenuation_laws: rows(root, "attenuation_law", attenuation_law_from)?,
        external_authorities: rows(root, "external_authority", external_authority_from)?,
        product_space: product_space_from(get_table(
            root,
            "posture_product_space",
            "threat_model.toml",
        )?)?,
        exclusion_laws: rows(root, "exclusion_law", exclusion_law_from)?,
        postures: rows(root, "posture", posture_from)?,
        deferred_postures: rows(root, "deferred_posture", deferred_posture_from)?,
        footprints: rows(root, "footprint", footprint_from)?,
        claim_scan_rules: rows(root, "claim_scan_rule", claim_scan_rule_from)?,
    })
}

pub fn parse_threat(text: &str) -> Result<ThreatRegistry, LoadError> {
    let table = parse(text).map_err(|error| LoadError {
        path: REGISTRY_PATH.into(),
        message: error.to_string(),
    })?;
    threat_from(&table).map_err(LoadError::from)
}

pub fn load_threat(path: &Path) -> Result<ThreatRegistry, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    parse_threat(&text).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.message,
    })
}

pub fn load_from_repo(root: &Path) -> Result<ThreatRegistry, LoadError> {
    load_threat(&root.join(REGISTRY_PATH))
}

// -----------------------------------------------------------------------------
// Violations
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: String,
    pub subject: String,
    pub source_anchor: String,
    pub message: String,
}

impl Violation {
    fn new(
        code: impl Into<String>,
        subject: impl Into<String>,
        source_anchor: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Violation {
            code: code.into(),
            subject: subject.into(),
            source_anchor: source_anchor.into(),
            message: message.into(),
        }
    }
}

// -----------------------------------------------------------------------------
// Expansions: the three matrices
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureCell {
    pub actor_id: String,
    pub asset_id: String,
    pub disposition: String,
    pub assumption_id: String,
}

fn split_binding(entry: &str) -> Option<(&str, &str)> {
    let (asset, assumption) = entry.split_once(':')?;
    if asset.is_empty() || assumption.is_empty() {
        return None;
    }
    Some((asset, assumption))
}

/// Expand the actor x asset matrix. Malformed entries are dropped here and
/// reported as violations by `validate_threat`; this function is the shape the
/// document and the event stream consume.
pub fn expand_exposures(registry: &ThreatRegistry) -> Vec<ExposureCell> {
    let mut out = Vec::new();
    let mut actors: Vec<&Actor> = registry.actors.iter().collect();
    actors.sort_by_key(|actor| actor.source_order);
    for actor in actors {
        for (disposition, entries) in [
            ("defended", &actor.defended_assets),
            ("conditional", &actor.conditional_assets),
            ("undefended", &actor.undefended_assets),
        ] {
            for entry in entries {
                if let Some((asset, assumption)) = split_binding(entry) {
                    out.push(ExposureCell {
                        actor_id: actor.id.clone(),
                        asset_id: asset.to_string(),
                        disposition: disposition.to_string(),
                        assumption_id: assumption.to_string(),
                    });
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductCell {
    pub service_class: String,
    pub role_posture: String,
    pub continuity_profile: String,
    /// "registered" | "deferred" | "excluded"
    pub resolution: String,
    /// posture id, deferred posture id, or the exclusion law id
    pub resolved_by: String,
}

/// The exclusion laws, evaluated. Returns the id of the first law that excludes
/// this cell, in registered order, or `None` when the cell is admissible.
fn excluded_by(law_ids: &[String], cell: (&str, &str, &str)) -> Option<String> {
    let (service_class, role_posture, continuity_profile) = cell;
    for id in law_ids {
        let excluded = match id.as_str() {
            // PX-1: Sharded implies ExternalCas.
            "PX-1" => role_posture == "Sharded" && continuity_profile != "ExternalCas",
            // PX-2: ArchiveReadOnly implies NotApplicable.
            "PX-2" => service_class == "ArchiveReadOnly" && continuity_profile != "NotApplicable",
            // PX-3: Operational implies a real continuity profile.
            "PX-3" => service_class == "Operational" && continuity_profile == "NotApplicable",
            _ => false,
        };
        if excluded {
            return Some(id.clone());
        }
    }
    None
}

/// Enumerate every cell of the declared product space and resolve it. This is
/// the law that makes the posture set provably complete rather than a list.
pub fn expand_product_space(registry: &ThreatRegistry) -> Vec<ProductCell> {
    let law_ids: Vec<String> = registry
        .exclusion_laws
        .iter()
        .map(|law| law.id.clone())
        .collect();
    let mut out = Vec::new();
    for service_class in &registry.product_space.service_class_axis {
        for role_posture in &registry.product_space.role_posture_axis {
            for continuity_profile in &registry.product_space.continuity_profile_axis {
                let key = (
                    service_class.as_str(),
                    role_posture.as_str(),
                    continuity_profile.as_str(),
                );
                let (resolution, resolved_by) = if let Some(law) = excluded_by(&law_ids, key) {
                    ("excluded".to_string(), law)
                } else if let Some(posture) = registry.postures.iter().find(|posture| {
                    (
                        posture.service_class.as_str(),
                        posture.role_posture.as_str(),
                        posture.continuity_profile.as_str(),
                    ) == key
                }) {
                    ("registered".to_string(), posture.id.clone())
                } else if let Some(deferred) = registry.deferred_postures.iter().find(|posture| {
                    (
                        posture.service_class.as_str(),
                        posture.role_posture.as_str(),
                        posture.continuity_profile.as_str(),
                    ) == key
                }) {
                    ("deferred".to_string(), deferred.id.clone())
                } else {
                    ("unresolved".to_string(), String::new())
                };
                out.push(ProductCell {
                    service_class: service_class.clone(),
                    role_posture: role_posture.clone(),
                    continuity_profile: continuity_profile.clone(),
                    resolution,
                    resolved_by,
                });
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintCell {
    pub posture_id: String,
    pub authority_id: String,
    /// "present" | "empty_declared" | "missing"
    pub status: String,
    pub on_synchronous_path: bool,
    pub touch_count: usize,
    pub sync_path_position: String,
}

/// Expand the registered-posture x eleven-authority matrix. Every cell is
/// logged, including the empty-declared ones: the bead's completeness law is
/// over cells, not over rows that happen to exist.
pub fn expand_footprint(registry: &ThreatRegistry) -> Vec<FootprintCell> {
    let mut out = Vec::new();
    let mut postures: Vec<&Posture> = registry.postures.iter().collect();
    postures.sort_by(|left, right| left.id.cmp(&right.id));
    let mut authorities: Vec<&ExternalAuthority> = registry.external_authorities.iter().collect();
    authorities.sort_by_key(|authority| authority.source_order);
    for posture in postures {
        for authority in &authorities {
            let row = registry
                .footprints
                .iter()
                .find(|row| row.posture_id == posture.id && row.authority_id == authority.id);
            let cell = match (posture.footprint_declaration.as_str(), row) {
                ("empty", None) => FootprintCell {
                    posture_id: posture.id.clone(),
                    authority_id: authority.id.clone(),
                    status: "empty_declared".into(),
                    on_synchronous_path: false,
                    touch_count: 0,
                    sync_path_position: String::new(),
                },
                (_, Some(row)) => FootprintCell {
                    posture_id: posture.id.clone(),
                    authority_id: authority.id.clone(),
                    status: "present".into(),
                    on_synchronous_path: row.on_synchronous_path,
                    touch_count: row.touch_count,
                    sync_path_position: row.sync_path_position.clone(),
                },
                (_, None) => FootprintCell {
                    posture_id: posture.id.clone(),
                    authority_id: authority.id.clone(),
                    status: "missing".into(),
                    on_synchronous_path: false,
                    touch_count: 0,
                    sync_path_position: String::new(),
                },
            };
            out.push(cell);
        }
    }
    out
}

// -----------------------------------------------------------------------------
// Source blocks
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SourceBlockCheck {
    pub id: String,
    pub line_count: usize,
    pub byte_count: usize,
    pub fnv1a64: String,
    pub outcome: String,
}

fn safe_repo_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains("..")
        && !path.contains('\\')
        && !path.contains('\0')
}

fn read_repo_text(root: &Path, relative: &str) -> Result<String, String> {
    if !safe_repo_relative(relative) {
        return Err(format!("unsafe repo-relative path {relative:?}"));
    }
    let path: PathBuf = root.join(relative);
    fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Extract an inclusive 1-based line range, keeping every line's terminator.
/// Deliberately byte-exact: a threat model that paraphrases its source is not
/// a threat model, and the only way to know it did not is to compare bytes.
pub fn line_range(text: &str, start: usize, end: usize) -> Result<String, String> {
    if start == 0 || end < start {
        return Err(format!("invalid line range {start}..{end}"));
    }
    let mut out = String::new();
    let mut seen = 0usize;
    for line in text.split_inclusive('\n') {
        seen += 1;
        if seen >= start && seen <= end {
            out.push_str(line);
        }
        if seen > end {
            break;
        }
    }
    if seen < end {
        return Err(format!(
            "line range {start}..{end} exceeds the document ({seen} lines)"
        ));
    }
    Ok(out)
}

fn canonical_fnv(bytes: &[u8]) -> String {
    format!("0x{:016x}", fnv1a64(bytes))
}

pub fn source_block_text(block: &SourceBlock, root: &Path) -> Result<String, String> {
    let plan = read_repo_text(root, &block.plan_path)?;
    line_range(&plan, block.plan_start_line, block.plan_end_line)
}

pub fn check_source_blocks(
    registry: &ThreatRegistry,
    root: &Path,
) -> Vec<Result<SourceBlockCheck, String>> {
    let mut blocks: Vec<&SourceBlock> = registry.source_blocks.iter().collect();
    blocks.sort_by(|left, right| left.id.cmp(&right.id));
    blocks
        .into_iter()
        .map(|block| {
            let text = source_block_text(block, root)?;
            let line_count = block.plan_end_line - block.plan_start_line + 1;
            let hash = canonical_fnv(text.as_bytes());
            let matches = block.line_count == line_count
                && block.byte_count == text.len()
                && block.fnv1a64 == hash;
            Ok(SourceBlockCheck {
                id: block.id.clone(),
                line_count,
                byte_count: text.len(),
                fnv1a64: hash,
                outcome: if matches { "pass" } else { "fail" }.into(),
            })
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Hashes
// -----------------------------------------------------------------------------

fn hash_of(items: &[String]) -> String {
    let mut transcript = String::new();
    for item in items {
        transcript.push_str(item);
        transcript.push('\n');
    }
    format!("fnv1a64:{:016x}", fnv1a64(transcript.as_bytes()))
}

/// Every stable id the registry declares, sorted. Moves on any addition,
/// removal, or rename.
pub fn id_table(registry: &ThreatRegistry) -> Vec<String> {
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for actor in &registry.actors {
        ids.insert(format!("actor:{}", actor.id));
    }
    for asset in &registry.assets {
        ids.insert(format!("asset:{}", asset.id));
    }
    for assumption in &registry.assumptions {
        ids.insert(format!("assumption:{}", assumption.id));
    }
    for row in &registry.out_of_scope {
        ids.insert(format!("out_of_scope:{}", row.id));
    }
    for identity in &registry.identities {
        ids.insert(format!("identity:{}", identity.name));
    }
    for class in &registry.operation_classes {
        ids.insert(format!("operation_class:{}", class.name));
    }
    for dimension in &registry.authority_dimensions {
        ids.insert(format!("authority_dimension:{}", dimension.id));
    }
    for binding in &registry.presentation_bindings {
        ids.insert(format!("presentation_binding:{}", binding.name));
    }
    for law in &registry.attenuation_laws {
        ids.insert(format!("attenuation_law:{}", law.id));
    }
    for authority in &registry.external_authorities {
        ids.insert(format!("external_authority:{}", authority.id));
    }
    for law in &registry.exclusion_laws {
        ids.insert(format!("exclusion_law:{}", law.id));
    }
    for posture in &registry.postures {
        ids.insert(format!("posture:{}", posture.id));
    }
    for posture in &registry.deferred_postures {
        ids.insert(format!("deferred_posture:{}", posture.id));
    }
    for rule in &registry.claim_scan_rules {
        ids.insert(format!("claim_scan_rule:{}", rule.id));
    }
    ids.into_iter().collect()
}

pub fn recompute_id_table_hash(registry: &ThreatRegistry) -> String {
    hash_of(&id_table(registry))
}

/// The semantic contract: every decision this registry freezes that a
/// downstream consumer could read as normative. Deliberately excludes prose
/// (summaries, rationales) so a copy edit does not read as a contract change,
/// and deliberately includes every narrowing operator, transition legality,
/// exposure disposition, and footprint position, because those are the
/// contract.
pub fn recompute_semantic_contract_hash(registry: &ThreatRegistry) -> String {
    let mut lines: Vec<String> = Vec::new();
    for actor in &registry.actors {
        lines.push(format!(
            "actor|{}|{}|{}|{}",
            actor.source_order, actor.id, actor.trust_class, actor.inside_trust_boundary
        ));
    }
    for cell in expand_exposures(registry) {
        lines.push(format!(
            "exposure|{}|{}|{}|{}",
            cell.actor_id, cell.asset_id, cell.disposition, cell.assumption_id
        ));
    }
    for asset in &registry.assets {
        lines.push(format!(
            "asset|{}|{}|{}|{}",
            asset.source_order, asset.id, asset.primary_claim_class, asset.primary_claim_ref
        ));
    }
    for row in &registry.out_of_scope {
        lines.push(format!("out_of_scope|{}|{}", row.id, row.rejection_anchor));
    }
    for identity in &registry.identities {
        lines.push(format!(
            "identity|{}|{}|{}|{}|{}|{}",
            identity.source_order,
            identity.name,
            identity.kind,
            identity.epoch_domain,
            identity.rust_newtype,
            identity.wire_tag
        ));
    }
    for class in &registry.operation_classes {
        lines.push(format!("operation_class|{}|{}", class.ordinal, class.name));
    }
    for dimension in &registry.authority_dimensions {
        lines.push(format!(
            "authority_dimension|{}|{}|{}",
            dimension.source_order, dimension.id, dimension.narrowing_operator
        ));
    }
    for binding in &registry.presentation_bindings {
        lines.push(format!(
            "presentation_binding|{}|{}",
            binding.rank, binding.name
        ));
    }
    for transition in &registry.binding_transitions {
        lines.push(format!(
            "binding_transition|{}|{}|{}|{}",
            transition.from, transition.to, transition.legal, transition.law
        ));
    }
    for law in &registry.attenuation_laws {
        lines.push(format!(
            "attenuation_law|{}|{}|{}|{}|{}",
            law.id,
            law.class,
            law.mutation,
            law.dimension_ids.join(","),
            law.negative_fixture
        ));
    }
    for authority in &registry.external_authorities {
        lines.push(format!(
            "external_authority|{}|{}|{}",
            authority.source_order,
            authority.id,
            authority.record_kinds.join(",")
        ));
    }
    for law in &registry.exclusion_laws {
        lines.push(format!("exclusion_law|{}|{}", law.id, law.statement));
    }
    for cell in expand_product_space(registry) {
        lines.push(format!(
            "product_cell|{}|{}|{}|{}|{}",
            cell.service_class,
            cell.role_posture,
            cell.continuity_profile,
            cell.resolution,
            cell.resolved_by
        ));
    }
    for cell in expand_footprint(registry) {
        lines.push(format!(
            "footprint|{}|{}|{}|{}|{}",
            cell.posture_id,
            cell.authority_id,
            cell.status,
            cell.on_synchronous_path,
            cell.touch_count
        ));
    }
    for rule in &registry.claim_scan_rules {
        lines.push(format!(
            "claim_scan_rule|{}|{}|{}|{}",
            rule.id,
            rule.subject,
            rule.predicate,
            rule.qualifiers.join(",")
        ));
    }
    hash_of(&lines)
}

// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

fn check_enum(
    value: &str,
    allowed: &[&str],
    code: &str,
    subject: &str,
    anchor: &str,
    field: &str,
    violations: &mut Vec<Violation>,
) {
    if !allowed.contains(&value) {
        violations.push(Violation::new(
            code,
            subject,
            anchor,
            format!("{field} {value:?} is outside the closed set {allowed:?}"),
        ));
    }
}

fn check_sequence(
    label: &str,
    actual: &[String],
    expected: &[&str],
    code: &str,
    violations: &mut Vec<Violation>,
) {
    let expected_owned: Vec<String> = expected.iter().map(|value| value.to_string()).collect();
    if actual != expected_owned.as_slice() {
        violations.push(Violation::new(
            code,
            label,
            "§12.1",
            format!(
                "{label} must be exactly the source-order sequence {expected:?}, found {actual:?}"
            ),
        ));
    }
}

/// Every ordered vocabulary must carry a dense, collision-free 1..n ordinal.
///
/// WHY this is separate from the sequence check: `check_sequence` sorts by
/// ordinal and compares names, and a stable sort leaves a DUPLICATED ordinal in
/// its original position — so two rows sharing an ordinal produce the correct
/// name sequence and the sequence check passes. The suite caught exactly that
/// (Read and Mutate both at 2 validated clean), which is the same
/// looks-exactly-like-a-pass family as a vacuous fixture.
fn check_dense_order(
    label: &str,
    orders: &[usize],
    anchor: &str,
    violations: &mut Vec<Violation>,
) {
    let mut sorted = orders.to_vec();
    sorted.sort_unstable();
    for window in sorted.windows(2) {
        if window[0] == window[1] {
            violations.push(Violation::new(
                "ordinal_collision",
                label,
                anchor,
                format!("ordinal {} is used by more than one row", window[0]),
            ));
            return;
        }
    }
    for (index, order) in sorted.iter().enumerate() {
        if *order != index + 1 {
            violations.push(Violation::new(
                "ordinal_gap",
                label,
                anchor,
                format!(
                    "ordinals must be a dense 1..{} sequence; found {order} at position {}",
                    sorted.len(),
                    index + 1
                ),
            ));
            return;
        }
    }
}

fn check_count(
    label: &str,
    declared: usize,
    actual: usize,
    anchor: &str,
    violations: &mut Vec<Violation>,
) {
    if declared != actual {
        violations.push(Violation::new(
            "count_drift",
            label,
            anchor,
            format!("declared {label} = {declared}, registry holds {actual}"),
        ));
    }
}

fn validate_header(registry: &ThreatRegistry, violations: &mut Vec<Violation>) {
    if registry.schema_version != SCHEMA_VERSION {
        violations.push(Violation::new(
            "schema_version",
            "<registry>",
            "§19 G0",
            format!(
                "schema_version must be {SCHEMA_VERSION}, found {}",
                registry.schema_version
            ),
        ));
    }
    if registry.registry.name != REGISTRY_NAME {
        violations.push(Violation::new(
            "registry_name",
            "<registry>",
            "§19 G0",
            format!("registry name must be {REGISTRY_NAME:?}"),
        ));
    }
    if registry.registry.document_path != DOCUMENT_PATH {
        violations.push(Violation::new(
            "document_path",
            "<registry>",
            "§19 G0",
            format!("document_path must be {DOCUMENT_PATH:?}"),
        ));
    }
    if registry.registry.replay_command != REPLAY_COMMAND {
        violations.push(Violation::new(
            "replay_command",
            "<registry>",
            "§19 G0",
            "replay_command must name the threat-check binary".to_string(),
        ));
    }
    let header = &registry.registry;
    check_count(
        "actor_count",
        header.actor_count,
        registry.actors.len(),
        "§12.1",
        violations,
    );
    check_count(
        "asset_count",
        header.asset_count,
        registry.assets.len(),
        "§12.1",
        violations,
    );
    check_count(
        "assumption_count",
        header.assumption_count,
        registry.assumptions.len(),
        "§12.1",
        violations,
    );
    check_count(
        "out_of_scope_count",
        header.out_of_scope_count,
        registry.out_of_scope.len(),
        "§3.4",
        violations,
    );
    check_count(
        "identity_count",
        header.identity_count,
        registry.identities.len(),
        "§12.1",
        violations,
    );
    check_count(
        "operation_class_count",
        header.operation_class_count,
        registry.operation_classes.len(),
        "§12.1",
        violations,
    );
    check_count(
        "authority_dimension_count",
        header.authority_dimension_count,
        registry.authority_dimensions.len(),
        "§12.1",
        violations,
    );
    check_count(
        "presentation_binding_count",
        header.presentation_binding_count,
        registry.presentation_bindings.len(),
        "§12.2",
        violations,
    );
    check_count(
        "binding_transition_count",
        header.binding_transition_count,
        registry.binding_transitions.len(),
        "§12.2",
        violations,
    );
    check_count(
        "attenuation_law_count",
        header.attenuation_law_count,
        registry.attenuation_laws.len(),
        "§12.2",
        violations,
    );
    check_count(
        "external_authority_count",
        header.external_authority_count,
        registry.external_authorities.len(),
        "§16.8",
        violations,
    );
    check_count(
        "posture_count",
        header.posture_count,
        registry.postures.len(),
        "§16.8",
        violations,
    );
    check_count(
        "deferred_posture_count",
        header.deferred_posture_count,
        registry.deferred_postures.len(),
        "§16.8",
        violations,
    );
    check_count(
        "exclusion_law_count",
        header.exclusion_law_count,
        registry.exclusion_laws.len(),
        "§5.1",
        violations,
    );
    check_count(
        "footprint_cell_count",
        header.footprint_cell_count,
        registry.footprints.len(),
        "§16.8",
        violations,
    );
    check_count(
        "exposure_cell_count",
        header.exposure_cell_count,
        expand_exposures(registry).len(),
        "§12.1",
        violations,
    );

    let id_hash = recompute_id_table_hash(registry);
    if header.id_table_hash != id_hash {
        violations.push(Violation::new(
            "id_table_hash",
            "<registry>",
            "§19 G0",
            format!(
                "declared id_table_hash {} != recomputed {id_hash}",
                header.id_table_hash
            ),
        ));
    }
    let semantic_hash = recompute_semantic_contract_hash(registry);
    if header.semantic_contract_hash != semantic_hash {
        violations.push(Violation::new(
            "semantic_contract_hash",
            "<registry>",
            "§19 G0",
            format!(
                "declared semantic_contract_hash {} != recomputed {semantic_hash}",
                header.semantic_contract_hash
            ),
        ));
    }
}

/// Law 1 — exposure completeness. Every (actor, asset) cell is dispositioned
/// exactly once and names a registered assumption.
fn validate_exposures(registry: &ThreatRegistry, violations: &mut Vec<Violation>) {
    let asset_ids: BTreeSet<&str> = registry
        .assets
        .iter()
        .map(|asset| asset.id.as_str())
        .collect();
    let assumption_ids: BTreeSet<&str> = registry
        .assumptions
        .iter()
        .map(|assumption| assumption.id.as_str())
        .collect();
    for actor in &registry.actors {
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for (disposition, entries) in [
            ("defended", &actor.defended_assets),
            ("conditional", &actor.conditional_assets),
            ("undefended", &actor.undefended_assets),
        ] {
            for entry in entries {
                let Some((asset, assumption)) = split_binding(entry) else {
                    violations.push(Violation::new(
                        "exposure_malformed",
                        &actor.id,
                        &actor.source_anchor,
                        format!(
                            "{disposition} entry {entry:?} must be \"<asset_id>:<assumption_id>\""
                        ),
                    ));
                    continue;
                };
                if !asset_ids.contains(asset) {
                    violations.push(Violation::new(
                        "exposure_unknown_asset",
                        &actor.id,
                        &actor.source_anchor,
                        format!("{disposition} entry names unregistered asset {asset:?}"),
                    ));
                }
                if !assumption_ids.contains(assumption) {
                    violations.push(Violation::new(
                        "exposure_unknown_assumption",
                        &actor.id,
                        &actor.source_anchor,
                        format!("{disposition} entry names unregistered assumption {assumption:?}"),
                    ));
                }
                *seen.entry(asset.to_string()).or_insert(0) += 1;
            }
        }
        for (asset, count) in &seen {
            if *count > 1 {
                violations.push(Violation::new(
                    "exposure_duplicated",
                    &actor.id,
                    &actor.source_anchor,
                    format!("asset {asset:?} is dispositioned {count} times; exactly one required"),
                ));
            }
        }
        for asset in &asset_ids {
            if !seen.contains_key(*asset) {
                violations.push(Violation::new(
                    "exposure_missing",
                    &actor.id,
                    &actor.source_anchor,
                    format!(
                        "asset {asset:?} has no disposition; an unstated exposure reads exactly like a defended one"
                    ),
                ));
            }
        }
    }
}

/// Law 2 — narrowing-only attenuation, plus the identity-distinctness law.
fn validate_authority_lattice(registry: &ThreatRegistry, violations: &mut Vec<Violation>) {
    // Identity vocabulary, in source order, with distinct epoch domains.
    let identity_names: Vec<String> = {
        let mut sorted: Vec<&Identity> = registry.identities.iter().collect();
        sorted.sort_by_key(|identity| identity.source_order);
        sorted.iter().map(|identity| identity.name.clone()).collect()
    };
    check_sequence(
        "identities",
        &identity_names,
        &IDENTITY_ORDER,
        "identity_order",
        violations,
    );
    check_dense_order(
        "identities",
        &registry
            .identities
            .iter()
            .map(|identity| identity.source_order)
            .collect::<Vec<_>>(),
        "§12.1",
        violations,
    );
    let mut newtypes: BTreeSet<&str> = BTreeSet::new();
    let mut wire_tags: BTreeSet<&str> = BTreeSet::new();
    for identity in &registry.identities {
        check_enum(
            &identity.kind,
            &ALLOWED_IDENTITY_KINDS,
            "identity_kind",
            &identity.name,
            "§12.1",
            "kind",
            violations,
        );
        check_enum(
            &identity.epoch_domain,
            &ALLOWED_EPOCH_DOMAINS,
            "identity_epoch_domain",
            &identity.name,
            "§12.1",
            "epoch_domain",
            violations,
        );
        if !newtypes.insert(identity.rust_newtype.as_str()) {
            violations.push(Violation::new(
                "identity_newtype_collision",
                &identity.name,
                "§12.1",
                format!(
                    "rust_newtype {:?} is shared with another identity; the epoch types must never unify",
                    identity.rust_newtype
                ),
            ));
        }
        if !wire_tags.insert(identity.wire_tag.as_str()) {
            violations.push(Violation::new(
                "identity_wire_tag_collision",
                &identity.name,
                "§12.1",
                format!("wire_tag {:?} is shared with another identity", identity.wire_tag),
            ));
        }
    }
    // The named law: security and adaptive epoch types are never comparable.
    let security_epoch = registry
        .identities
        .iter()
        .find(|identity| identity.name == "SecurityPolicyEpoch");
    let decision_epoch = registry
        .identities
        .iter()
        .find(|identity| identity.name == "DecisionPolicyEpoch");
    match (security_epoch, decision_epoch) {
        (Some(security), Some(decision)) => {
            if security.epoch_domain == decision.epoch_domain {
                violations.push(Violation::new(
                    "epoch_domains_unify",
                    "SecurityPolicyEpoch/DecisionPolicyEpoch",
                    "§12.1",
                    "the security and adaptive epoch types share an epoch_domain; they are never comparable or substitutable".to_string(),
                ));
            }
            if security.rust_newtype == decision.rust_newtype
                || security.wire_tag == decision.wire_tag
            {
                violations.push(Violation::new(
                    "epoch_identities_unify",
                    "SecurityPolicyEpoch/DecisionPolicyEpoch",
                    "§12.1",
                    "the security and adaptive epoch types must have distinct wire tags and distinct Rust newtypes".to_string(),
                ));
            }
        }
        _ => violations.push(Violation::new(
            "epoch_identity_absent",
            "SecurityPolicyEpoch/DecisionPolicyEpoch",
            "§12.1",
            "both SecurityPolicyEpoch and DecisionPolicyEpoch must be registered".to_string(),
        )),
    }

    // Operation classes: the closed set, in source order.
    let class_names: Vec<String> = {
        let mut sorted: Vec<&OperationClass> = registry.operation_classes.iter().collect();
        sorted.sort_by_key(|class| class.ordinal);
        sorted.iter().map(|class| class.name.clone()).collect()
    };
    check_sequence(
        "operation_classes",
        &class_names,
        &OPERATION_CLASS_ORDER,
        "operation_class_order",
        violations,
    );
    check_dense_order(
        "operation_classes",
        &registry
            .operation_classes
            .iter()
            .map(|class| class.ordinal)
            .collect::<Vec<_>>(),
        "§12.1",
        violations,
    );

    // Every dimension declares exactly one narrowing operator.
    check_dense_order(
        "authority_dimensions",
        &registry
            .authority_dimensions
            .iter()
            .map(|dimension| dimension.source_order)
            .collect::<Vec<_>>(),
        "§12.1",
        violations,
    );
    let dimension_ids: BTreeSet<&str> = registry
        .authority_dimensions
        .iter()
        .map(|dimension| dimension.id.as_str())
        .collect();
    for dimension in &registry.authority_dimensions {
        check_enum(
            &dimension.narrowing_operator,
            &ALLOWED_NARROWING_OPERATORS,
            "narrowing_operator",
            &dimension.id,
            &dimension.source_anchor,
            "narrowing_operator",
            violations,
        );
    }

    // Presentation bindings: dense ranks from zero.
    let mut ranks: Vec<usize> = registry
        .presentation_bindings
        .iter()
        .map(|binding| binding.rank)
        .collect();
    ranks.sort_unstable();
    let expected_ranks: Vec<usize> = (0..registry.presentation_bindings.len()).collect();
    if ranks != expected_ranks {
        violations.push(Violation::new(
            "binding_rank_not_dense",
            "<presentation_binding>",
            "§12.2",
            format!("presentation binding ranks must be dense from zero, found {ranks:?}"),
        ));
    }
    let rank_of: BTreeMap<&str, usize> = registry
        .presentation_bindings
        .iter()
        .map(|binding| (binding.name.as_str(), binding.rank))
        .collect();

    // The complete transition matrix: every (from, to) pair, legality derived
    // from the rank order rather than asserted per cell.
    let mut declared: BTreeSet<(String, String)> = BTreeSet::new();
    for transition in &registry.binding_transitions {
        if !declared.insert((transition.from.clone(), transition.to.clone())) {
            violations.push(Violation::new(
                "binding_transition_duplicated",
                format!("{}->{}", transition.from, transition.to),
                "§12.2",
                "the transition is declared more than once".to_string(),
            ));
        }
        let (Some(from), Some(to)) = (
            rank_of.get(transition.from.as_str()),
            rank_of.get(transition.to.as_str()),
        ) else {
            violations.push(Violation::new(
                "binding_transition_unknown_class",
                format!("{}->{}", transition.from, transition.to),
                "§12.2",
                "the transition names an unregistered presentation binding".to_string(),
            ));
            continue;
        };
        let derived_legal = to >= from;
        if transition.legal != derived_legal {
            violations.push(Violation::new(
                "binding_transition_legality",
                format!("{}->{}", transition.from, transition.to),
                "§12.2",
                format!(
                    "declared legal = {}, but the rank order derives {derived_legal}: a link may preserve or further restrict presentation binding, never weaken it",
                    transition.legal
                ),
            ));
        }
        let derived_law = if from == to {
            "preserve"
        } else if derived_legal {
            "further_restrict"
        } else {
            "weakened_binding"
        };
        if transition.law != derived_law {
            violations.push(Violation::new(
                "binding_transition_law",
                format!("{}->{}", transition.from, transition.to),
                "§12.2",
                format!("declared law {:?}, derived {derived_law:?}", transition.law),
            ));
        }
    }
    for from in &registry.presentation_bindings {
        for to in &registry.presentation_bindings {
            if !declared.contains(&(from.name.clone(), to.name.clone())) {
                violations.push(Violation::new(
                    "binding_transition_missing",
                    format!("{}->{}", from.name, to.name),
                    "§12.2",
                    "every cell of the narrowing transition matrix must be declared, including the illegal ones".to_string(),
                ));
            }
        }
    }

    // Attenuation laws: closed classes, resolvable dimensions, named fixtures,
    // and complete dimension coverage.
    let mut governed: BTreeSet<&str> = BTreeSet::new();
    let mut permitted = 0usize;
    let mut prohibited = 0usize;
    for law in &registry.attenuation_laws {
        check_enum(
            &law.class,
            &ALLOWED_ATTENUATION_CLASSES,
            "attenuation_class",
            &law.id,
            &law.source_anchor,
            "class",
            violations,
        );
        match law.class.as_str() {
            "permitted" => permitted += 1,
            "prohibited" => prohibited += 1,
            _ => {}
        }
        if law.dimension_ids.is_empty() {
            violations.push(Violation::new(
                "attenuation_law_ungoverned",
                &law.id,
                &law.source_anchor,
                "an attenuation law must govern at least one authority dimension".to_string(),
            ));
        }
        for dimension in &law.dimension_ids {
            if !dimension_ids.contains(dimension.as_str()) {
                violations.push(Violation::new(
                    "attenuation_law_unknown_dimension",
                    &law.id,
                    &law.source_anchor,
                    format!("names unregistered authority dimension {dimension:?}"),
                ));
            } else {
                governed.insert(dimension.as_str());
            }
        }
        // A prohibition and the operators it governs must agree. Without this
        // the two tables drift independently: `tenant` could be declared
        // `intersect` while ATT-X2 still claims the authority domain is fixed,
        // and only the contract hash would notice — a hash tells you something
        // moved, never that the model became unsound.
        if law.class == "prohibited" {
            let required: &[&str] = match law.mutation.as_str() {
                "change_authority_domain" | "move_cohorts" => &["fixed"],
                "widen_disclosure" => &["restrict_disclosure_only"],
                "weaken_presentation_binding" => &["restrict_binding_only"],
                "extend_time" => &["raise_only", "lower_only"],
                _ => &[],
            };
            if !required.is_empty() {
                for dimension_id in &law.dimension_ids {
                    let Some(dimension) = registry
                        .authority_dimensions
                        .iter()
                        .find(|dimension| &dimension.id == dimension_id)
                    else {
                        continue;
                    };
                    if !required.contains(&dimension.narrowing_operator.as_str()) {
                        violations.push(Violation::new(
                            "prohibition_operator_mismatch",
                            &law.id,
                            &law.source_anchor,
                            format!(
                                "prohibits {} but dimension {dimension_id} declares narrowing_operator {:?}, which permits it; expected one of {required:?}",
                                law.mutation, dimension.narrowing_operator
                            ),
                        ));
                    }
                }
            }
        }
        if law.negative_fixture.is_empty() {
            violations.push(Violation::new(
                "attenuation_law_no_fixture",
                &law.id,
                &law.source_anchor,
                "every attenuation law names the negative fixture that must reject its violation"
                    .to_string(),
            ));
        }
    }
    if permitted == 0 || prohibited == 0 {
        violations.push(Violation::new(
            "attenuation_law_one_sided",
            "<attenuation_law>",
            "§12.2",
            format!(
                "the law must state both what a link may do and what it cannot: {permitted} permitted, {prohibited} prohibited"
            ),
        ));
    }
    for dimension in &registry.authority_dimensions {
        // `current_state_monotone` dimensions are supplied by current state,
        // not narrowed by the chain, so no attenuation law governs them.
        if dimension.narrowing_operator == "current_state_monotone" {
            continue;
        }
        if !governed.contains(dimension.id.as_str()) {
            violations.push(Violation::new(
                "dimension_ungoverned",
                &dimension.id,
                &dimension.source_anchor,
                "no attenuation law governs this dimension; the lattice order would be undefined for it".to_string(),
            ));
        }
    }
}

/// Law 3 — posture product-space closure.
fn validate_postures(registry: &ThreatRegistry, violations: &mut Vec<Violation>) {
    let space = &registry.product_space;
    let axes: [(&str, &Vec<String>, &[&str]); 3] = [
        (
            "service_class_axis",
            &space.service_class_axis,
            &SERVICE_CLASS_AXIS,
        ),
        (
            "role_posture_axis",
            &space.role_posture_axis,
            &ROLE_POSTURE_AXIS,
        ),
        (
            "continuity_profile_axis",
            &space.continuity_profile_axis,
            &CONTINUITY_PROFILE_AXIS,
        ),
    ];
    for (label, actual, expected) in axes {
        check_sequence(label, actual, expected, "product_space_axis", violations);
    }
    let cells = expand_product_space(registry);
    if space.cell_count != cells.len() {
        violations.push(Violation::new(
            "product_space_cell_count",
            "<posture_product_space>",
            "§16.8",
            format!(
                "declared cell_count = {}, the axes produce {}",
                space.cell_count,
                cells.len()
            ),
        ));
    }
    for cell in &cells {
        if cell.resolution == "unresolved" {
            violations.push(Violation::new(
                "product_cell_unresolved",
                format!(
                    "{}/{}/{}",
                    cell.service_class, cell.role_posture, cell.continuity_profile
                ),
                "§16.8",
                "every admissible cell must be a registered posture or a deferred posture with a named owner bead".to_string(),
            ));
        }
    }
    // Every exclusion law must actually exclude something. A law that excludes
    // nothing is a law nobody can distinguish from an absent one.
    for law in &registry.exclusion_laws {
        if !cells
            .iter()
            .any(|cell| cell.resolution == "excluded" && cell.resolved_by == law.id)
        {
            violations.push(Violation::new(
                "exclusion_law_vacuous",
                &law.id,
                &law.source_anchor,
                "the exclusion law excludes no cell of the declared product space".to_string(),
            ));
        }
    }

    // A posture must land in an ADMISSIBLE cell. Without this the product-space
    // law is one-directional: it proves every admissible cell has a posture,
    // but not that every posture has an admissible cell, so a row contradicting
    // an exclusion law would sit in the registry unreferenced and unreported.
    let law_ids: Vec<String> = registry
        .exclusion_laws
        .iter()
        .map(|law| law.id.clone())
        .collect();
    let mut placements: Vec<(&str, &str, &str, &str)> = Vec::new();
    for posture in &registry.postures {
        placements.push((
            posture.id.as_str(),
            posture.service_class.as_str(),
            posture.role_posture.as_str(),
            posture.continuity_profile.as_str(),
        ));
    }
    for posture in &registry.deferred_postures {
        placements.push((
            posture.id.as_str(),
            posture.service_class.as_str(),
            posture.role_posture.as_str(),
            posture.continuity_profile.as_str(),
        ));
    }
    for (id, service_class, role_posture, continuity_profile) in placements {
        let on_axes = space.service_class_axis.iter().any(|v| v == service_class)
            && space.role_posture_axis.iter().any(|v| v == role_posture)
            && space
                .continuity_profile_axis
                .iter()
                .any(|v| v == continuity_profile);
        if !on_axes {
            violations.push(Violation::new(
                "posture_off_axis",
                id,
                "§16.8",
                format!(
                    "coordinates ({service_class}, {role_posture}, {continuity_profile}) are not on the declared product-space axes"
                ),
            ));
            continue;
        }
        if let Some(law) = excluded_by(&law_ids, (service_class, role_posture, continuity_profile)) {
            violations.push(Violation::new(
                "posture_excluded_cell",
                id,
                "§16.8",
                format!("occupies a cell excluded by {law}"),
            ));
        }
    }

    let mut posture_ids: BTreeSet<&str> = BTreeSet::new();
    for posture in &registry.postures {
        check_enum(
            &posture.status,
            &ALLOWED_POSTURE_STATUSES,
            "posture_status",
            &posture.id,
            &posture.source_anchor,
            "status",
            violations,
        );
        check_enum(
            &posture.footprint_declaration,
            &ALLOWED_FOOTPRINT_DECLARATIONS,
            "footprint_declaration",
            &posture.id,
            &posture.source_anchor,
            "footprint_declaration",
            violations,
        );
        if !posture_ids.insert(posture.id.as_str()) {
            violations.push(Violation::new(
                "posture_duplicated",
                &posture.id,
                &posture.source_anchor,
                "posture id is declared twice".to_string(),
            ));
        }
        match posture.footprint_declaration.as_str() {
            "empty" if posture.empty_justification.trim().is_empty() => {
                violations.push(Violation::new(
                    "posture_empty_unjustified",
                    &posture.id,
                    &posture.source_anchor,
                    "an empty footprint declaration must state why the posture consumes zero external authorities".to_string(),
                ));
            }
            "complete" if !posture.empty_justification.trim().is_empty() => {
                violations.push(Violation::new(
                    "posture_complete_with_justification",
                    &posture.id,
                    &posture.source_anchor,
                    "a complete footprint declaration carries no empty justification".to_string(),
                ));
            }
            _ => {}
        }
    }
    for posture in &registry.deferred_postures {
        if posture.owner_bead.trim().is_empty() {
            violations.push(Violation::new(
                "deferred_posture_unowned",
                &posture.id,
                "§16.8",
                "a deferred posture must name the bead that owns its footprint".to_string(),
            ));
        }
        if posture.reason.trim().is_empty() {
            violations.push(Violation::new(
                "deferred_posture_unexplained",
                &posture.id,
                "§16.8",
                "a deferred posture must state why its footprint is not frozen here".to_string(),
            ));
        }
        if posture_ids.contains(posture.id.as_str()) {
            violations.push(Violation::new(
                "deferred_posture_also_registered",
                &posture.id,
                "§16.8",
                "a posture cannot be both registered and deferred".to_string(),
            ));
        }
    }
}

/// The eleven-authority footprint completeness law.
fn validate_footprint(registry: &ThreatRegistry, violations: &mut Vec<Violation>) {
    let authority_order: Vec<String> = {
        let mut sorted: Vec<&ExternalAuthority> = registry.external_authorities.iter().collect();
        sorted.sort_by_key(|authority| authority.source_order);
        sorted
            .iter()
            .map(|authority| authority.id.clone())
            .collect()
    };
    check_sequence(
        "external_authorities",
        &authority_order,
        &EXTERNAL_AUTHORITY_ORDER,
        "external_authority_order",
        violations,
    );
    check_dense_order(
        "external_authorities",
        &registry
            .external_authorities
            .iter()
            .map(|authority| authority.source_order)
            .collect::<Vec<_>>(),
        "§16.8",
        violations,
    );

    let posture_ids: BTreeSet<&str> = registry
        .postures
        .iter()
        .map(|posture| posture.id.as_str())
        .collect();
    let authority_ids: BTreeSet<&str> = registry
        .external_authorities
        .iter()
        .map(|authority| authority.id.as_str())
        .collect();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for row in &registry.footprints {
        if !posture_ids.contains(row.posture_id.as_str()) {
            violations.push(Violation::new(
                "footprint_unknown_posture",
                format!("{}/{}", row.posture_id, row.authority_id),
                "§16.8",
                format!("names unregistered posture {:?}", row.posture_id),
            ));
        }
        if !authority_ids.contains(row.authority_id.as_str()) {
            violations.push(Violation::new(
                "footprint_unknown_authority",
                format!("{}/{}", row.posture_id, row.authority_id),
                "§16.8",
                format!("names unregistered authority {:?}", row.authority_id),
            ));
        }
        if !seen.insert((row.posture_id.clone(), row.authority_id.clone())) {
            violations.push(Violation::new(
                "footprint_duplicated",
                format!("{}/{}", row.posture_id, row.authority_id),
                "§16.8",
                "the cell is declared twice".to_string(),
            ));
        }
        check_enum(
            &row.operation_class_basis,
            &ALLOWED_OPERATION_CLASS_BASES,
            "operation_class_basis",
            format!("{}/{}", row.posture_id, row.authority_id).as_str(),
            "§16.8",
            "operation_class_basis",
            violations,
        );
        // The honesty law for this table. `trigger_site_only` means the plan
        // names where the authority sits but not which operation class the
        // realized path uses, so the cell must NOT name operation classes and
        // must name the bead that will bind them.
        match row.operation_class_basis.as_str() {
            "trigger_site_only" => {
                if !row.operation_classes.is_empty() {
                    violations.push(Violation::new(
                        "footprint_unsourced_operation_class",
                        format!("{}/{}", row.posture_id, row.authority_id),
                        "§16.8",
                        "operation_class_basis is trigger_site_only, so the cell may not name operation classes the source never named".to_string(),
                    ));
                }
                if row.deferred_binding_owner.trim().is_empty() {
                    violations.push(Violation::new(
                        "footprint_unowned_deferral",
                        format!("{}/{}", row.posture_id, row.authority_id),
                        "§16.8",
                        "a trigger-site-only cell must name the bead that binds its realized operation classes".to_string(),
                    ));
                }
            }
            "named_in_source" => {
                if row.operation_classes.is_empty() {
                    violations.push(Violation::new(
                        "footprint_empty_named_classes",
                        format!("{}/{}", row.posture_id, row.authority_id),
                        "§16.8",
                        "operation_class_basis is named_in_source but no operation class is named"
                            .to_string(),
                    ));
                }
                for class in &row.operation_classes {
                    if !OPERATION_CLASS_ORDER.contains(&class.as_str()) {
                        violations.push(Violation::new(
                            "footprint_unknown_operation_class",
                            format!("{}/{}", row.posture_id, row.authority_id),
                            "§12.1",
                            format!("names {class:?}, outside the closed set of sixteen"),
                        ));
                    }
                }
            }
            _ => {}
        }
        if row.on_synchronous_path && row.sync_path_position.trim().is_empty() {
            violations.push(Violation::new(
                "footprint_position_absent",
                format!("{}/{}", row.posture_id, row.authority_id),
                "§16.8",
                "a synchronous-path cell must state exactly where the authority sits".to_string(),
            ));
        }
        if row.on_synchronous_path && row.touch_count == 0 {
            violations.push(Violation::new(
                "footprint_zero_touches",
                format!("{}/{}", row.posture_id, row.authority_id),
                "§16.8",
                "a synchronous-path cell touches the authority at least once".to_string(),
            ));
        }
    }

    for cell in expand_footprint(registry) {
        if cell.status == "missing" {
            violations.push(Violation::new(
                "footprint_cell_missing",
                format!("{}/{}", cell.posture_id, cell.authority_id),
                "§16.8",
                "every registered posture declares a complete footprint row for all eleven authorities, or an explicit empty declaration".to_string(),
            ));
        }
    }
    // An empty-declared posture must carry no footprint rows at all: a
    // half-empty declaration is the drift this catches.
    for posture in &registry.postures {
        if posture.footprint_declaration == "empty"
            && registry
                .footprints
                .iter()
                .any(|row| row.posture_id == posture.id)
        {
            violations.push(Violation::new(
                "posture_empty_with_rows",
                &posture.id,
                &posture.source_anchor,
                "the posture declares an empty footprint but carries footprint rows".to_string(),
            ));
        }
    }
}

fn validate_actors_assets(registry: &ThreatRegistry, violations: &mut Vec<Violation>) {
    let actor_ids: Vec<String> = {
        let mut sorted: Vec<&Actor> = registry.actors.iter().collect();
        sorted.sort_by_key(|actor| actor.source_order);
        sorted.iter().map(|actor| actor.id.clone()).collect()
    };
    check_sequence("actors", &actor_ids, &ACTOR_ORDER, "actor_order", violations);
    check_dense_order(
        "actors",
        &registry
            .actors
            .iter()
            .map(|actor| actor.source_order)
            .collect::<Vec<_>>(),
        "§12.1",
        violations,
    );
    check_dense_order(
        "assets",
        &registry
            .assets
            .iter()
            .map(|asset| asset.source_order)
            .collect::<Vec<_>>(),
        "§12.1",
        violations,
    );
    for actor in &registry.actors {
        check_enum(
            &actor.trust_class,
            &ALLOWED_TRUST_CLASSES,
            "trust_class",
            &actor.id,
            &actor.source_anchor,
            "trust_class",
            violations,
        );
        if actor.summary.trim().is_empty() {
            violations.push(Violation::new(
                "actor_unsummarized",
                &actor.id,
                &actor.source_anchor,
                "every actor states what it can do".to_string(),
            ));
        }
    }
    for asset in &registry.assets {
        check_enum(
            &asset.primary_claim_class,
            &ALLOWED_CLAIM_CLASSES,
            "asset_claim_class",
            &asset.id,
            "§1.11",
            "primary_claim_class",
            violations,
        );
        if asset.primary_claim_ref.trim().is_empty() {
            violations.push(Violation::new(
                "asset_unbound_claim",
                &asset.id,
                "§1.11",
                "every asset names the registry row that carries its primary claim".to_string(),
            ));
        }
    }
    for assumption in &registry.assumptions {
        if assumption.bounds.trim().is_empty() {
            violations.push(Violation::new(
                "assumption_unbounded",
                &assumption.id,
                &assumption.source_anchor,
                "an assumption states what it grants AND what it does not".to_string(),
            ));
        }
    }
    for row in &registry.out_of_scope {
        if row.rationale.trim().is_empty() {
            violations.push(Violation::new(
                "out_of_scope_unreasoned",
                &row.id,
                &row.rejection_anchor,
                "a registered rejection carries its recorded reason".to_string(),
            ));
        }
    }
}

fn validate_source_blocks(registry: &ThreatRegistry, root: &Path, violations: &mut Vec<Violation>) {
    let mut blocks: Vec<&SourceBlock> = registry.source_blocks.iter().collect();
    blocks.sort_by(|left, right| left.id.cmp(&right.id));
    for (block, check) in blocks.iter().zip(check_source_blocks(registry, root)) {
        if block.plan_path != PLAN_PATH {
            violations.push(Violation::new(
                "source_block_path",
                &block.id,
                "§19 G0",
                format!("plan_path must be {PLAN_PATH:?}"),
            ));
        }
        match check {
            Ok(check) if check.outcome == "pass" => {}
            Ok(check) => violations.push(Violation::new(
                "source_block_drift",
                &block.id,
                "§19 G0",
                format!(
                    "declared line_count/byte_count/fnv1a64 = {}/{}/{}, recomputed {}/{}/{}",
                    block.line_count,
                    block.byte_count,
                    block.fnv1a64,
                    check.line_count,
                    check.byte_count,
                    check.fnv1a64
                ),
            )),
            Err(message) => violations.push(Violation::new(
                "source_block_unreadable",
                &block.id,
                "§19 G0",
                message,
            )),
        }
    }
}

pub fn validate_threat(registry: &ThreatRegistry, root: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    validate_header(registry, &mut violations);
    validate_actors_assets(registry, &mut violations);
    validate_exposures(registry, &mut violations);
    validate_authority_lattice(registry, &mut violations);
    validate_postures(registry, &mut violations);
    validate_footprint(registry, &mut violations);
    validate_source_blocks(registry, root, &mut violations);
    violations.sort_by(|left, right| {
        (&left.code, &left.subject, &left.message).cmp(&(&right.code, &right.subject, &right.message))
    });
    violations
}

// -----------------------------------------------------------------------------
// Claim scan
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimScanHit {
    pub rule_id: String,
    pub line: usize,
    pub sentence: String,
    pub trust_matrix_conflict: String,
}

fn sentences_with_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let mut current = String::new();
        for chunk in line.split_inclusive(['.', '!', '?']) {
            current.push_str(chunk);
            if chunk.ends_with(['.', '!', '?']) {
                out.push((index + 1, current.trim().to_string()));
                current = String::new();
            }
        }
        if !current.trim().is_empty() {
            out.push((index + 1, current.trim().to_string()));
        }
    }
    out
}

/// Scan normative text for sentences that assert more than the trust matrix
/// admits. This is LINT EVIDENCE, not a semantic noninterference proof — a
/// distinction the registry records and this doc comment repeats so a green
/// scan is never mistaken for a result about the system.
pub fn scan_claims(text: &str, registry: &ThreatRegistry) -> Vec<ClaimScanHit> {
    let mut hits = Vec::new();
    for (line, sentence) in sentences_with_lines(text) {
        let lowered = sentence.to_lowercase();
        for rule in &registry.claim_scan_rules {
            if !lowered.contains(&rule.subject.to_lowercase())
                || !lowered.contains(&rule.predicate.to_lowercase())
            {
                continue;
            }
            if rule
                .qualifiers
                .iter()
                .any(|qualifier| lowered.contains(&qualifier.to_lowercase()))
            {
                continue;
            }
            hits.push(ClaimScanHit {
                rule_id: rule.id.clone(),
                line,
                sentence: sentence.clone(),
                trust_matrix_conflict: rule.trust_matrix_conflict.clone(),
            });
        }
    }
    hits
}

// -----------------------------------------------------------------------------
// Document generation
// -----------------------------------------------------------------------------

fn heading(out: &mut String, level: usize, text: &str) {
    out.push('\n');
    for _ in 0..level {
        out.push('#');
    }
    out.push(' ');
    out.push_str(text);
    out.push_str("\n\n");
}

fn table_row(out: &mut String, cells: &[&str]) {
    out.push('|');
    for cell in cells {
        out.push(' ');
        out.push_str(cell);
        out.push_str(" |");
    }
    out.push('\n');
}

fn table_head(out: &mut String, headers: &[&str]) {
    table_row(out, headers);
    out.push('|');
    for _ in headers {
        out.push_str(" --- |");
    }
    out.push('\n');
}

/// Escape a cell for a GitHub-flavoured markdown table: pipes would split the
/// row, newlines would end it.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// Generate the published threat-model document. Deterministic: same registry
/// plus same plan bytes produce the same document, byte for byte.
pub fn generate_document(registry: &ThreatRegistry, root: &Path) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("<!-- GENERATED FILE — DO NOT EDIT BY HAND.\n");
    out.push_str("     Source: registries/threat_model.toml\n");
    out.push_str("     Regenerate: ");
    out.push_str(REPLAY_COMMAND);
    out.push_str(" --write\n");
    out.push_str("     Verify:     ");
    out.push_str(REPLAY_COMMAND);
    out.push_str("\n-->\n");
    out.push_str("\n# FrankenGraphDB — Threat and Trust Model\n\n");
    out.push_str(
        "This document is the frame in which every later security claim is scoped. It is generated from `registries/threat_model.toml`; the registry is the master, this rendering is a projection, and the checker fails if they disagree.\n\n",
    );
    out.push_str(
        "**What a reader should take from it.** The baseline trusts the executing database process and the active key boundary. A compromised server process can exfiltrate what it can decrypt, and no claim here contradicts that. Witnessed transparency and audit detect their scoped history and administrative attacks; they never undo disclosure. Ordinary Raft tolerates crash faults, not Byzantine replicas. Everything else below is the detail of those four sentences.\n",
    );

    heading(&mut out, 2, "1. Actors");
    out.push_str("The eight actors of §12.1, in source order. Each is considered adversarially: the disposition tables in §3 state what the model defends against this actor, not what the actor is expected to do.\n\n");
    table_head(
        &mut out,
        &["#", "Actor", "Trust class", "In boundary", "Summary"],
    );
    let mut actors: Vec<&Actor> = registry.actors.iter().collect();
    actors.sort_by_key(|actor| actor.source_order);
    for actor in &actors {
        table_row(
            &mut out,
            &[
                &actor.source_order.to_string(),
                &format!("`{}`", actor.id),
                &cell(&actor.trust_class),
                if actor.inside_trust_boundary {
                    "yes"
                } else {
                    "no"
                },
                &cell(&actor.summary),
            ],
        );
    }

    heading(&mut out, 2, "2. Protected assets");
    table_head(
        &mut out,
        &["#", "Asset", "Primary claim", "Class", "Summary"],
    );
    let mut assets: Vec<&Asset> = registry.assets.iter().collect();
    assets.sort_by_key(|asset| asset.source_order);
    for asset in &assets {
        table_row(
            &mut out,
            &[
                &asset.source_order.to_string(),
                &format!("`{}`", asset.id),
                &cell(&asset.primary_claim_ref),
                &cell(&asset.primary_claim_class),
                &cell(&asset.summary),
            ],
        );
    }

    heading(&mut out, 2, "3. The exposure matrix");
    out.push_str("Every actor-asset cell is dispositioned exactly once and names the assumption that carries it. `defended` means the model defends the asset against that actor; `conditional` means it does so only under the named assumption's stated bounds; `undefended` means it does not, and says so rather than leaving a gap a reader would fill with optimism.\n\n");
    let exposures = expand_exposures(registry);
    let by_cell: BTreeMap<(&str, &str), &ExposureCell> = exposures
        .iter()
        .map(|cell| ((cell.actor_id.as_str(), cell.asset_id.as_str()), cell))
        .collect();
    let mut headers: Vec<String> = vec!["Actor \\\\ Asset".to_string()];
    for asset in &assets {
        headers.push(format!("`{}`", asset.id));
    }
    let header_refs: Vec<&str> = headers.iter().map(String::as_str).collect();
    table_head(&mut out, &header_refs);
    for actor in &actors {
        let mut row: Vec<String> = vec![format!("`{}`", actor.id)];
        for asset in &assets {
            row.push(match by_cell.get(&(actor.id.as_str(), asset.id.as_str())) {
                Some(cell) => format!("{} ({})", cell.disposition, cell.assumption_id),
                None => "**MISSING**".to_string(),
            });
        }
        let row_refs: Vec<&str> = row.iter().map(String::as_str).collect();
        table_row(&mut out, &row_refs);
    }

    heading(&mut out, 2, "4. Trust assumptions");
    for assumption in &registry.assumptions {
        out.push_str(&format!(
            "- **{}** ({}) — {}\n  - *Bounds*: {}\n",
            assumption.id, assumption.source_anchor, assumption.statement, assumption.bounds
        ));
    }

    heading(&mut out, 2, "5. Registered out-of-scope failures");
    for row in &registry.out_of_scope {
        out.push_str(&format!(
            "- **{}** — {} ({})\n  - *Reason*: {}\n",
            row.id, row.statement, row.rejection_anchor, row.rationale
        ));
    }

    heading(&mut out, 2, "6. Stable security identities");
    table_head(
        &mut out,
        &["#", "Identity", "Kind", "Epoch domain", "Rust newtype", "Wire tag"],
    );
    let mut identities: Vec<&Identity> = registry.identities.iter().collect();
    identities.sort_by_key(|identity| identity.source_order);
    for identity in &identities {
        table_row(
            &mut out,
            &[
                &identity.source_order.to_string(),
                &format!("`{}`", identity.name),
                &cell(&identity.kind),
                &cell(&identity.epoch_domain),
                &format!("`{}`", identity.rust_newtype),
                &format!("`{}`", identity.wire_tag),
            ],
        );
    }
    out.push_str("\nThe security and adaptive epoch types have distinct wire tags and distinct Rust newtypes and are never comparable or substitutable. `SecurityPolicyEpoch` sits in the `security` epoch domain; `DecisionPolicyEpoch` sits in `adaptive`.\n");

    heading(&mut out, 2, "7. Operation classes");
    out.push_str("The closed set of sixteen, in §12.1 source order.\n\n");
    table_head(&mut out, &["#", "Class", "Summary"]);
    let mut classes: Vec<&OperationClass> = registry.operation_classes.iter().collect();
    classes.sort_by_key(|class| class.ordinal);
    for class in &classes {
        table_row(
            &mut out,
            &[
                &class.ordinal.to_string(),
                &format!("`{}`", class.name),
                &cell(&class.summary),
            ],
        );
    }

    heading(&mut out, 2, "8. The EffectiveAuthority lattice");
    out.push_str("The lattice has no independent order: its order is exactly the conjunction of these per-dimension narrowing operators under the attenuation law of §9.\n\n");
    table_head(
        &mut out,
        &["#", "Dimension", "Narrowing operator", "Source", "Summary"],
    );
    let mut dimensions: Vec<&AuthorityDimension> = registry.authority_dimensions.iter().collect();
    dimensions.sort_by_key(|dimension| dimension.source_order);
    for dimension in &dimensions {
        table_row(
            &mut out,
            &[
                &dimension.source_order.to_string(),
                &format!("`{}`", dimension.id),
                &format!("`{}`", dimension.narrowing_operator),
                &cell(&dimension.source_anchor),
                &cell(&dimension.summary),
            ],
        );
    }

    heading(&mut out, 2, "9. The attenuation law");
    table_head(
        &mut out,
        &["Law", "Class", "Statement", "Governs", "Negative fixture"],
    );
    for law in &registry.attenuation_laws {
        table_row(
            &mut out,
            &[
                &format!("`{}`", law.id),
                &cell(&law.class),
                &cell(&law.statement),
                &cell(&law.dimension_ids.join(", ")),
                &format!("`{}`", law.negative_fixture),
            ],
        );
    }

    heading(&mut out, 3, "9.1 Presentation binding narrowing");
    table_head(&mut out, &["Class", "Rank", "Summary"]);
    let mut bindings: Vec<&PresentationBinding> = registry.presentation_bindings.iter().collect();
    bindings.sort_by_key(|binding| binding.rank);
    for binding in &bindings {
        table_row(
            &mut out,
            &[
                &format!("`{}`", binding.name),
                &binding.rank.to_string(),
                &cell(&binding.summary),
            ],
        );
    }
    out.push_str("\nThe complete transition matrix. A link may preserve the binding or move to a strictly higher rank; every other cell is illegal and is declared here rather than merely absent.\n\n");
    let mut transition_headers: Vec<String> = vec!["from \\\\ to".to_string()];
    for binding in &bindings {
        transition_headers.push(format!("`{}`", binding.name));
    }
    let transition_refs: Vec<&str> = transition_headers.iter().map(String::as_str).collect();
    table_head(&mut out, &transition_refs);
    for from in &bindings {
        let mut row: Vec<String> = vec![format!("`{}`", from.name)];
        for to in &bindings {
            let found = registry
                .binding_transitions
                .iter()
                .find(|transition| transition.from == from.name && transition.to == to.name);
            row.push(match found {
                Some(transition) if transition.legal => format!("legal ({})", transition.law),
                Some(transition) => format!("**illegal** ({})", transition.law),
                None => "**MISSING**".to_string(),
            });
        }
        let row_refs: Vec<&str> = row.iter().map(String::as_str).collect();
        table_row(&mut out, &row_refs);
    }

    heading(&mut out, 2, "10. Postures and the product-space closure");
    out.push_str("A posture is an admissible cell of a declared product space, not a name on a list. Every cell below is registered, deferred to a named owner bead, or excluded by a named law.\n\n");
    table_head(&mut out, &["Law", "Statement", "Source", "Reason"]);
    for law in &registry.exclusion_laws {
        table_row(
            &mut out,
            &[
                &format!("`{}`", law.id),
                &cell(&law.statement),
                &cell(&law.source_anchor),
                &cell(&law.rationale),
            ],
        );
    }
    out.push('\n');
    table_head(
        &mut out,
        &[
            "Service class",
            "Role posture",
            "Continuity profile",
            "Resolution",
            "Resolved by",
        ],
    );
    for product in expand_product_space(registry) {
        table_row(
            &mut out,
            &[
                &cell(&product.service_class),
                &cell(&product.role_posture),
                &cell(&product.continuity_profile),
                &cell(&product.resolution),
                &format!("`{}`", product.resolved_by),
            ],
        );
    }
    out.push('\n');
    for posture in &registry.deferred_postures {
        out.push_str(&format!(
            "- **Deferred** `{}` — owner `{}`. {}\n",
            posture.id, posture.owner_bead, posture.reason
        ));
    }

    heading(&mut out, 2, "11. The external-authority footprint");
    out.push_str("Eleven authorities, in §16 item 8 source order.\n\n");
    table_head(&mut out, &["#", "Authority", "Records", "Summary"]);
    let mut authorities: Vec<&ExternalAuthority> = registry.external_authorities.iter().collect();
    authorities.sort_by_key(|authority| authority.source_order);
    for authority in &authorities {
        table_row(
            &mut out,
            &[
                &authority.source_order.to_string(),
                &format!("`{}`", authority.id),
                &cell(
                    &authority
                        .record_kinds
                        .iter()
                        .map(|kind| format!("`{kind}`"))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                &cell(&authority.summary),
            ],
        );
    }
    let mut postures: Vec<&Posture> = registry.postures.iter().collect();
    postures.sort_by(|left, right| left.id.cmp(&right.id));
    let footprint_cells = expand_footprint(registry);
    for posture in &postures {
        heading(&mut out, 3, &format!("11.x {}", posture.title));
        if posture.footprint_declaration == "empty" {
            out.push_str(&format!(
                "**Empty footprint declaration.** {}\n\nAll eleven authority cells are explicitly empty for this posture.\n",
                posture.empty_justification
            ));
            continue;
        }
        table_head(
            &mut out,
            &[
                "Authority",
                "Synchronous",
                "Touches",
                "Where on the path",
                "Operation-class basis",
            ],
        );
        for footprint in footprint_cells
            .iter()
            .filter(|footprint| footprint.posture_id == posture.id)
        {
            let row = registry.footprints.iter().find(|row| {
                row.posture_id == footprint.posture_id && row.authority_id == footprint.authority_id
            });
            table_row(
                &mut out,
                &[
                    &format!("`{}`", footprint.authority_id),
                    if footprint.on_synchronous_path {
                        "yes"
                    } else {
                        "no"
                    },
                    &footprint.touch_count.to_string(),
                    &cell(&footprint.sync_path_position),
                    &cell(
                        row.map(|row| row.operation_class_basis.as_str())
                            .unwrap_or("—"),
                    ),
                ],
            );
        }
        out.push_str(
            "\nEvery cell carries `operation_class_basis = trigger_site_only`: §16 item 8 states where each authority sits on the synchronous path but never names which of the sixteen operation classes the realized path uses. Binding realized operation classes is `fgdb-w9-authority-surface-gp9j`'s deliverable, measured against implemented paths rather than inferred here.\n",
        );
    }

    heading(&mut out, 2, "12. Checked source");
    out.push_str("The normative plan text this model is derived from, embedded verbatim. The checker re-reads both sides and fails on any drift.\n");
    let mut blocks: Vec<&SourceBlock> = registry.source_blocks.iter().collect();
    blocks.sort_by(|left, right| left.id.cmp(&right.id));
    for block in blocks {
        let text = source_block_text(block, root)?;
        out.push_str(&format!(
            "\n<!-- CHECKED-SOURCE-BEGIN id=\"{}\" -->\n",
            block.id
        ));
        out.push_str(&format!(
            "> Source: `{}` lines {}–{} — {}\n\n",
            block.plan_path, block.plan_start_line, block.plan_end_line, block.covers
        ));
        out.push_str(&text);
        out.push_str(&format!(
            "\n<!-- CHECKED-SOURCE-END id=\"{}\" -->\n",
            block.id
        ));
    }

    heading(&mut out, 2, "13. Provenance");
    out.push_str(&format!(
        "- Registry: `{REGISTRY_PATH}` (schema {SCHEMA_VERSION})\n- Replay: `{REPLAY_COMMAND}`\n- Bound invariants: {}\n- Bound evidence: {}\n- Identity-table hash: `{}`\n- Semantic-contract hash: `{}`\n",
        registry.registry.bound_invariants.join(", "),
        registry.registry.bound_evidence.join(", "),
        recompute_id_table_hash(registry),
        recompute_semantic_contract_hash(registry),
    ));
    Ok(out)
}

pub fn document_digest(text: &str) -> String {
    format!("sha256:{}", sha256_hex(text.as_bytes()))
}

/// Compare the generated document against the committed one.
pub fn check_document(registry: &ThreatRegistry, root: &Path) -> Result<bool, String> {
    let generated = generate_document(registry, root)?;
    let committed = read_repo_text(root, &registry.registry.document_path)?;
    Ok(generated == committed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(start: usize, end: usize) -> SourceBlock {
        SourceBlock {
            id: "b".into(),
            plan_path: PLAN_PATH.into(),
            plan_start_line: start,
            plan_end_line: end,
            line_count: end - start + 1,
            byte_count: 0,
            fnv1a64: String::new(),
            covers: String::new(),
        }
    }

    #[test]
    fn line_range_is_inclusive_and_keeps_terminators() {
        let text = "a\nb\nc\n";
        assert_eq!(line_range(text, 1, 1).unwrap(), "a\n");
        assert_eq!(line_range(text, 2, 3).unwrap(), "b\nc\n");
        assert_eq!(line_range(text, 1, 3).unwrap(), text);
    }

    #[test]
    fn line_range_rejects_out_of_bounds_and_inverted_ranges() {
        let text = "a\nb\n";
        assert!(line_range(text, 0, 1).is_err());
        assert!(line_range(text, 2, 1).is_err());
        assert!(line_range(text, 1, 9).is_err());
        let _ = block(1, 2);
    }

    #[test]
    fn split_binding_requires_both_halves() {
        assert_eq!(split_binding("a:b"), Some(("a", "b")));
        assert_eq!(split_binding(":b"), None);
        assert_eq!(split_binding("a:"), None);
        assert_eq!(split_binding("ab"), None);
    }

    #[test]
    fn sentence_splitter_tracks_lines() {
        let found = sentences_with_lines("one. two.\nthree");
        assert_eq!(found.len(), 3);
        assert_eq!(found[0], (1, "one.".to_string()));
        assert_eq!(found[1], (1, "two.".to_string()));
        assert_eq!(found[2], (2, "three".to_string()));
    }
}
