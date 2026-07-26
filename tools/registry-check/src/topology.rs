//! Workspace crate/layer topology checker (fgdb-g0-workspace-topology-1q9m).
//!
//! `registries/workspace_topology.toml` freezes plan §18.1/§18.2: every crate,
//! its layer, its unsafe policy, its activation status, its owner, the legal
//! dependency directions, and the build-here-versus-consume-from inventory with
//! per-row source evidence. This module is the gate.
//!
//! Three things here are worth knowing before reading the code.
//!
//! **The crate universe is derived, not transcribed.** [`parse_layer_table`]
//! parses the frozen §18.1 block and [`validate_topology`] compares the parse
//! against the registry in both directions. A crate the plan names and the
//! registry forgot fails; a crate the registry invents fails. Freezing a table
//! by retyping it is how transcription defects ship, and this appendix has
//! already produced several.
//!
//! **Inventory coverage is proved by residue.** [`decompose_inventory`] deletes
//! every registered capability phrase from the frozen §18.2 line and requires
//! what remains to be punctuation plus four registered rationale allowances.
//! An omitted capability leaves a residue token and names itself.
//!
//! **Cargo manifests are scanned, not TOML-parsed.** The registry parser in
//! [`crate::toml`] rejects inline tables by design, and every Cargo dependency
//! declaration is one, so [`scan_workspace`] is a small purpose-built manifest
//! reader. It reads what the closed-universe law needs — dependency name, real
//! package name, kind, path/git/rev, whether default features are disabled — and
//! treats anything it cannot understand as a typed error rather than a silence.
//!
//! Std-only by constitution, like every other checker in this crate: the closed
//! dependency universe applies to the tooling that enforces it.

use crate::hash::{fnv1a64, id_table_hash};
use crate::toml::{
    ReadError, Table, Value, get_int, get_str, get_str_array, get_table, get_table_array, parse,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

pub const REGISTRY_PATH: &str = "registries/workspace_topology.toml";
pub const REPLAY_COMMAND: &str = "cargo run -p registry-check --bin topology-check -- --root .";
pub const SCHEMA_VERSION: i64 = 1;

/// The fourteen layer titles of §18.1, in source order. Frozen here so a
/// renamed or dropped layer is a checker failure and not a quiet edit.
pub const LAYER_TITLES: [&str; 14] = [
    "Foundation",
    "Unsafe islands",
    "Chronicle",
    "Strata",
    "Txn + secure access",
    "Loom",
    "Ripple",
    "Beacon",
    "Prism",
    "Warden",
    "Surface/operations",
    "Aegis",
    "Composition",
    "Verification",
];

/// The three unsafe islands of §18.1 — the ONLY crates that may carry a policy
/// other than `forbid`. FG-CON-02 states this by enumeration, so it is checked
/// by enumeration.
pub const UNSAFE_ISLANDS: [&str; 3] = ["fgdb-unsafe-simd", "fgdb-unsafe-arena", "fgdb-unsafe-vfs"];

const ACTIVATION_STATUSES: [&str; 3] = ["active", "planned", "reserved"];
const UNSAFE_POLICIES: [&str; 2] = ["forbid", "deny_ledgered"];
const ROLE_BASES: [&str; 2] = ["plan_parenthetical", "layer_charter"];
const POSTURE_PARTICIPATIONS: [&str; 6] = [
    "all",
    "entry_embedded",
    "entry_server",
    "entry_cli",
    "packaging_boundary",
    "test_only",
];
const POSTURE_BASES: [&str; 5] = [
    "product_shape",
    "composition_entry",
    "packaging_boundary",
    "verification_layer",
    "source_named",
];
const OWNER_KINDS: [&str; 3] = ["workstream", "gate", "bead_family"];
const LINKAGES: [&str; 3] = ["linked", "linkable", "design_only"];
const DISPOSITIONS: [&str; 3] = ["build_here", "consume_from", "design_only"];
const ENDPOINT_KINDS: [&str; 3] = ["crate", "layer", "foundation"];
const POSTURE_STATUSES: [&str; 2] = ["live", "deferred"];

/// Characters allowed to remain after the §18.2 decomposition. Deliberately
/// tiny: a wide residue alphabet is how an omitted capability hides.
const RESIDUE_ALPHABET: [char; 5] = [' ', ',', '.', ';', '&'];

// -----------------------------------------------------------------------------
// Model
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RegistryHeader {
    pub name: String,
    pub document_path: String,
    pub replay_command: String,
    pub bound_constraints: Vec<String>,
    pub tooling_members: Vec<String>,
    pub unsafe_ledger_registry: String,
    pub workspace_manifest: String,
    pub workspace_unsafe_lint: String,
    pub toolchain_channel: String,
    pub embedded_source_blocks: Vec<String>,
    pub layer_count: usize,
    pub crate_count: usize,
    pub active_crate_count: usize,
    pub planned_crate_count: usize,
    pub reserved_crate_count: usize,
    pub owner_scope_count: usize,
    pub foundation_project_count: usize,
    pub capability_count: usize,
    pub build_here_count: usize,
    pub consume_from_count: usize,
    pub design_only_count: usize,
    pub residue_allowance_count: usize,
    pub required_dependency_count: usize,
    pub asset_evidence_gap_count: usize,
    pub forbidden_dependency_count: usize,
    pub dependency_narrowing_count: usize,
    pub posture_count: usize,
    pub source_block_count: usize,
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
pub struct LayerLaw {
    pub reciprocal_pair: Vec<String>,
    pub reciprocal_reason: String,
    pub crate_graph_must_be_acyclic: bool,
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub id: String,
    pub title: String,
    pub source_order: usize,
    pub allowed_outgoing_layers: Vec<String>,
    pub charter: String,
}

#[derive(Debug, Clone)]
pub struct OwnerScope {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub plan_anchor: String,
}

#[derive(Debug, Clone)]
pub struct FoundationProject {
    pub id: String,
    pub title: String,
    pub linkage: String,
    pub git_url: String,
    pub pinned_rev: String,
    pub package_prefixes: Vec<String>,
    pub known_members: Vec<String>,
    pub default_features_must_be_disabled: bool,
    pub default_features_basis: String,
    pub plan_anchor: String,
}

#[derive(Debug, Clone)]
pub struct ForbiddenDependency {
    pub id: String,
    pub selector: String,
    pub package_prefix: String,
    pub plan_anchor: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct CrateRow {
    pub name: String,
    pub layer: String,
    pub layer_position: usize,
    pub role: String,
    pub role_basis: String,
    pub unsafe_policy: String,
    pub activation_status: String,
    pub posture_participation: String,
    pub posture_basis: String,
    pub owner: String,
    pub owner_bead: String,
    pub manifest_dir: String,
}

#[derive(Debug, Clone)]
pub struct Posture {
    pub id: String,
    pub title: String,
    pub entry_crate: String,
    pub binary_name: String,
    pub plan_anchor: String,
    pub status: String,
    pub deferred_to: String,
}

#[derive(Debug, Clone)]
pub struct RequiredDependency {
    pub id: String,
    pub from: String,
    pub from_kind: String,
    pub to: String,
    pub to_kind: String,
    pub plan_anchor: String,
    pub source_marker: String,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct DependencyNarrowing {
    pub crate_name: String,
    pub allowed_layers: Vec<String>,
    pub allowed_crates: Vec<String>,
    pub allowed_foundation_projects: Vec<String>,
    pub plan_anchor: String,
    pub source_marker: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ResidueAllowance {
    pub id: String,
    pub text: String,
    pub reason: String,
}

/// A consume_from row whose capability the §2.1/§2.2 asset tables do not
/// enumerate. Registered one row at a time, and checked by ABSENCE: the gap is
/// only legal while the asset row really is missing.
#[derive(Debug, Clone)]
pub struct AssetEvidenceGap {
    pub capability_id: String,
    pub verified_absent_from: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Capability {
    pub id: String,
    pub disposition: String,
    pub source_phrase: String,
    pub owner_crate: String,
    pub foundation_project: String,
    pub foundation_asset: String,
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct TopologyRegistry {
    pub schema_version: i64,
    pub registry: RegistryHeader,
    pub source_blocks: Vec<SourceBlock>,
    pub layer_law: LayerLaw,
    pub layers: Vec<Layer>,
    pub owner_scopes: Vec<OwnerScope>,
    pub foundation_projects: Vec<FoundationProject>,
    pub forbidden_dependencies: Vec<ForbiddenDependency>,
    pub crates: Vec<CrateRow>,
    pub postures: Vec<Posture>,
    pub required_dependencies: Vec<RequiredDependency>,
    pub dependency_narrowings: Vec<DependencyNarrowing>,
    pub residue_allowances: Vec<ResidueAllowance>,
    pub asset_evidence_gaps: Vec<AssetEvidenceGap>,
    pub capabilities: Vec<Capability>,
}

impl TopologyRegistry {
    pub fn layer(&self, id: &str) -> Option<&Layer> {
        self.layers.iter().find(|layer| layer.id == id)
    }

    pub fn crate_row(&self, name: &str) -> Option<&CrateRow> {
        self.crates.iter().find(|row| row.name == name)
    }

    pub fn source_block(&self, id: &str) -> Option<&SourceBlock> {
        self.source_blocks.iter().find(|block| block.id == id)
    }

    pub fn foundation_project(&self, id: &str) -> Option<&FoundationProject> {
        self.foundation_projects
            .iter()
            .find(|project| project.id == id)
    }

    /// The crates of one layer, in `layer_position` order.
    pub fn layer_crates(&self, layer: &str) -> Vec<&CrateRow> {
        let mut rows: Vec<&CrateRow> = self.crates.iter().filter(|c| c.layer == layer).collect();
        rows.sort_by_key(|row| row.layer_position);
        rows
    }

    pub fn active_crates(&self) -> Vec<&CrateRow> {
        self.crates
            .iter()
            .filter(|row| row.activation_status == "active")
            .collect()
    }
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

fn rows<T>(
    root: &Table,
    key: &str,
    build: impl Fn(&Table, usize) -> Result<T, ReadError>,
) -> Result<Vec<T>, ReadError> {
    let tables = get_table_array(root, key, REGISTRY_PATH)?;
    let mut out = Vec::with_capacity(tables.len());
    for (index, table) in tables.into_iter().enumerate() {
        out.push(build(table, index)?);
    }
    Ok(out)
}

fn header_from(table: &Table) -> Result<RegistryHeader, ReadError> {
    let ctx = "workspace_topology.toml.registry";
    exact_keys(
        table,
        &[
            "name",
            "document_path",
            "replay_command",
            "bound_constraints",
            "tooling_members",
            "unsafe_ledger_registry",
            "workspace_manifest",
            "workspace_unsafe_lint",
            "toolchain_channel",
            "embedded_source_blocks",
            "layer_count",
            "crate_count",
            "active_crate_count",
            "planned_crate_count",
            "reserved_crate_count",
            "owner_scope_count",
            "foundation_project_count",
            "capability_count",
            "build_here_count",
            "consume_from_count",
            "design_only_count",
            "residue_allowance_count",
            "required_dependency_count",
            "asset_evidence_gap_count",
            "forbidden_dependency_count",
            "dependency_narrowing_count",
            "posture_count",
            "source_block_count",
            "id_table_hash",
            "semantic_contract_hash",
        ],
        ctx,
    )?;
    Ok(RegistryHeader {
        name: get_str(table, "name", ctx)?,
        document_path: get_str(table, "document_path", ctx)?,
        replay_command: get_str(table, "replay_command", ctx)?,
        bound_constraints: get_str_array(table, "bound_constraints", ctx)?,
        tooling_members: get_str_array(table, "tooling_members", ctx)?,
        unsafe_ledger_registry: get_str(table, "unsafe_ledger_registry", ctx)?,
        workspace_manifest: get_str(table, "workspace_manifest", ctx)?,
        workspace_unsafe_lint: get_str(table, "workspace_unsafe_lint", ctx)?,
        toolchain_channel: get_str(table, "toolchain_channel", ctx)?,
        embedded_source_blocks: get_str_array(table, "embedded_source_blocks", ctx)?,
        layer_count: usize_field(table, "layer_count", ctx)?,
        crate_count: usize_field(table, "crate_count", ctx)?,
        active_crate_count: usize_field(table, "active_crate_count", ctx)?,
        planned_crate_count: usize_field(table, "planned_crate_count", ctx)?,
        reserved_crate_count: usize_field(table, "reserved_crate_count", ctx)?,
        owner_scope_count: usize_field(table, "owner_scope_count", ctx)?,
        foundation_project_count: usize_field(table, "foundation_project_count", ctx)?,
        capability_count: usize_field(table, "capability_count", ctx)?,
        build_here_count: usize_field(table, "build_here_count", ctx)?,
        consume_from_count: usize_field(table, "consume_from_count", ctx)?,
        design_only_count: usize_field(table, "design_only_count", ctx)?,
        residue_allowance_count: usize_field(table, "residue_allowance_count", ctx)?,
        required_dependency_count: usize_field(table, "required_dependency_count", ctx)?,
        asset_evidence_gap_count: usize_field(table, "asset_evidence_gap_count", ctx)?,
        forbidden_dependency_count: usize_field(table, "forbidden_dependency_count", ctx)?,
        dependency_narrowing_count: usize_field(table, "dependency_narrowing_count", ctx)?,
        posture_count: usize_field(table, "posture_count", ctx)?,
        source_block_count: usize_field(table, "source_block_count", ctx)?,
        id_table_hash: get_str(table, "id_table_hash", ctx)?,
        semantic_contract_hash: get_str(table, "semantic_contract_hash", ctx)?,
    })
}

fn source_block_from(table: &Table, index: usize) -> Result<SourceBlock, ReadError> {
    let ctx = format!("workspace_topology.toml.source_block[{index}]");
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

fn layer_law_from(table: &Table) -> Result<LayerLaw, ReadError> {
    let ctx = "workspace_topology.toml.layer_law";
    exact_keys(
        table,
        &[
            "reciprocal_pair",
            "reciprocal_reason",
            "crate_graph_must_be_acyclic",
        ],
        ctx,
    )?;
    Ok(LayerLaw {
        reciprocal_pair: get_str_array(table, "reciprocal_pair", ctx)?,
        reciprocal_reason: get_str(table, "reciprocal_reason", ctx)?,
        crate_graph_must_be_acyclic: bool_field(table, "crate_graph_must_be_acyclic", ctx)?,
    })
}

fn layer_from(table: &Table, index: usize) -> Result<Layer, ReadError> {
    let ctx = format!("workspace_topology.toml.layer[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "title",
            "source_order",
            "allowed_outgoing_layers",
            "charter",
        ],
        &ctx,
    )?;
    Ok(Layer {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        source_order: usize_field(table, "source_order", &ctx)?,
        allowed_outgoing_layers: get_str_array(table, "allowed_outgoing_layers", &ctx)?,
        charter: get_str(table, "charter", &ctx)?,
    })
}

fn owner_scope_from(table: &Table, index: usize) -> Result<OwnerScope, ReadError> {
    let ctx = format!("workspace_topology.toml.owner_scope[{index}]");
    exact_keys(table, &["id", "kind", "title", "plan_anchor"], &ctx)?;
    Ok(OwnerScope {
        id: get_str(table, "id", &ctx)?,
        kind: get_str(table, "kind", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        plan_anchor: get_str(table, "plan_anchor", &ctx)?,
    })
}

fn foundation_project_from(table: &Table, index: usize) -> Result<FoundationProject, ReadError> {
    let ctx = format!("workspace_topology.toml.foundation_project[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "title",
            "linkage",
            "git_url",
            "pinned_rev",
            "package_prefixes",
            "known_members",
            "default_features_must_be_disabled",
            "default_features_basis",
            "plan_anchor",
        ],
        &ctx,
    )?;
    Ok(FoundationProject {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        linkage: get_str(table, "linkage", &ctx)?,
        git_url: get_str(table, "git_url", &ctx)?,
        pinned_rev: get_str(table, "pinned_rev", &ctx)?,
        package_prefixes: get_str_array(table, "package_prefixes", &ctx)?,
        known_members: get_str_array(table, "known_members", &ctx)?,
        default_features_must_be_disabled: bool_field(
            table,
            "default_features_must_be_disabled",
            &ctx,
        )?,
        default_features_basis: get_str(table, "default_features_basis", &ctx)?,
        plan_anchor: get_str(table, "plan_anchor", &ctx)?,
    })
}

fn forbidden_dependency_from(
    table: &Table,
    index: usize,
) -> Result<ForbiddenDependency, ReadError> {
    let ctx = format!("workspace_topology.toml.forbidden_dependency[{index}]");
    exact_keys(
        table,
        &["id", "selector", "package_prefix", "plan_anchor", "reason"],
        &ctx,
    )?;
    Ok(ForbiddenDependency {
        id: get_str(table, "id", &ctx)?,
        selector: get_str(table, "selector", &ctx)?,
        package_prefix: get_str(table, "package_prefix", &ctx)?,
        plan_anchor: get_str(table, "plan_anchor", &ctx)?,
        reason: get_str(table, "reason", &ctx)?,
    })
}

fn crate_from(table: &Table, index: usize) -> Result<CrateRow, ReadError> {
    let ctx = format!("workspace_topology.toml.crate[{index}]");
    exact_keys(
        table,
        &[
            "name",
            "layer",
            "layer_position",
            "role",
            "role_basis",
            "unsafe_policy",
            "activation_status",
            "posture_participation",
            "posture_basis",
            "owner",
            "owner_bead",
            "manifest_dir",
        ],
        &ctx,
    )?;
    Ok(CrateRow {
        name: get_str(table, "name", &ctx)?,
        layer: get_str(table, "layer", &ctx)?,
        layer_position: usize_field(table, "layer_position", &ctx)?,
        role: get_str(table, "role", &ctx)?,
        role_basis: get_str(table, "role_basis", &ctx)?,
        unsafe_policy: get_str(table, "unsafe_policy", &ctx)?,
        activation_status: get_str(table, "activation_status", &ctx)?,
        posture_participation: get_str(table, "posture_participation", &ctx)?,
        posture_basis: get_str(table, "posture_basis", &ctx)?,
        owner: get_str(table, "owner", &ctx)?,
        owner_bead: get_str(table, "owner_bead", &ctx)?,
        manifest_dir: get_str(table, "manifest_dir", &ctx)?,
    })
}

fn posture_from(table: &Table, index: usize) -> Result<Posture, ReadError> {
    let ctx = format!("workspace_topology.toml.posture[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "title",
            "entry_crate",
            "binary_name",
            "plan_anchor",
            "status",
            "deferred_to",
        ],
        &ctx,
    )?;
    Ok(Posture {
        id: get_str(table, "id", &ctx)?,
        title: get_str(table, "title", &ctx)?,
        entry_crate: get_str(table, "entry_crate", &ctx)?,
        binary_name: get_str(table, "binary_name", &ctx)?,
        plan_anchor: get_str(table, "plan_anchor", &ctx)?,
        status: get_str(table, "status", &ctx)?,
        deferred_to: get_str(table, "deferred_to", &ctx)?,
    })
}

fn required_dependency_from(table: &Table, index: usize) -> Result<RequiredDependency, ReadError> {
    let ctx = format!("workspace_topology.toml.required_dependency[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "from",
            "from_kind",
            "to",
            "to_kind",
            "plan_anchor",
            "source_marker",
            "note",
        ],
        &ctx,
    )?;
    Ok(RequiredDependency {
        id: get_str(table, "id", &ctx)?,
        from: get_str(table, "from", &ctx)?,
        from_kind: get_str(table, "from_kind", &ctx)?,
        to: get_str(table, "to", &ctx)?,
        to_kind: get_str(table, "to_kind", &ctx)?,
        plan_anchor: get_str(table, "plan_anchor", &ctx)?,
        source_marker: get_str(table, "source_marker", &ctx)?,
        note: get_str(table, "note", &ctx)?,
    })
}

fn dependency_narrowing_from(
    table: &Table,
    index: usize,
) -> Result<DependencyNarrowing, ReadError> {
    let ctx = format!("workspace_topology.toml.dependency_narrowing[{index}]");
    exact_keys(
        table,
        &[
            "crate_name",
            "allowed_layers",
            "allowed_crates",
            "allowed_foundation_projects",
            "plan_anchor",
            "source_marker",
            "reason",
        ],
        &ctx,
    )?;
    Ok(DependencyNarrowing {
        crate_name: get_str(table, "crate_name", &ctx)?,
        allowed_layers: get_str_array(table, "allowed_layers", &ctx)?,
        allowed_crates: get_str_array(table, "allowed_crates", &ctx)?,
        allowed_foundation_projects: get_str_array(table, "allowed_foundation_projects", &ctx)?,
        plan_anchor: get_str(table, "plan_anchor", &ctx)?,
        source_marker: get_str(table, "source_marker", &ctx)?,
        reason: get_str(table, "reason", &ctx)?,
    })
}

fn residue_allowance_from(table: &Table, index: usize) -> Result<ResidueAllowance, ReadError> {
    let ctx = format!("workspace_topology.toml.residue_allowance[{index}]");
    exact_keys(table, &["id", "text", "reason"], &ctx)?;
    Ok(ResidueAllowance {
        id: get_str(table, "id", &ctx)?,
        text: get_str(table, "text", &ctx)?,
        reason: get_str(table, "reason", &ctx)?,
    })
}

fn asset_evidence_gap_from(table: &Table, index: usize) -> Result<AssetEvidenceGap, ReadError> {
    let ctx = format!("workspace_topology.toml.asset_evidence_gap[{index}]");
    exact_keys(
        table,
        &["capability_id", "verified_absent_from", "reason"],
        &ctx,
    )?;
    Ok(AssetEvidenceGap {
        capability_id: get_str(table, "capability_id", &ctx)?,
        verified_absent_from: get_str(table, "verified_absent_from", &ctx)?,
        reason: get_str(table, "reason", &ctx)?,
    })
}

fn capability_from(table: &Table, index: usize) -> Result<Capability, ReadError> {
    let ctx = format!("workspace_topology.toml.capability[{index}]");
    exact_keys(
        table,
        &[
            "id",
            "disposition",
            "source_phrase",
            "owner_crate",
            "foundation_project",
            "foundation_asset",
            "note",
        ],
        &ctx,
    )?;
    Ok(Capability {
        id: get_str(table, "id", &ctx)?,
        disposition: get_str(table, "disposition", &ctx)?,
        source_phrase: get_str(table, "source_phrase", &ctx)?,
        owner_crate: get_str(table, "owner_crate", &ctx)?,
        foundation_project: get_str(table, "foundation_project", &ctx)?,
        foundation_asset: get_str(table, "foundation_asset", &ctx)?,
        note: get_str(table, "note", &ctx)?,
    })
}

pub fn topology_from(root: &Table) -> Result<TopologyRegistry, ReadError> {
    exact_keys(
        root,
        &[
            "schema_version",
            "registry",
            "source_block",
            "layer_law",
            "layer",
            "owner_scope",
            "foundation_project",
            "forbidden_dependency",
            "crate",
            "posture",
            "required_dependency",
            "dependency_narrowing",
            "residue_allowance",
            "asset_evidence_gap",
            "capability",
        ],
        "workspace_topology.toml",
    )?;
    Ok(TopologyRegistry {
        schema_version: get_int(root, "schema_version", "workspace_topology.toml")?,
        registry: header_from(get_table(root, "registry", "workspace_topology.toml")?)?,
        source_blocks: rows(root, "source_block", source_block_from)?,
        layer_law: layer_law_from(get_table(root, "layer_law", "workspace_topology.toml")?)?,
        layers: rows(root, "layer", layer_from)?,
        owner_scopes: rows(root, "owner_scope", owner_scope_from)?,
        foundation_projects: rows(root, "foundation_project", foundation_project_from)?,
        forbidden_dependencies: rows(root, "forbidden_dependency", forbidden_dependency_from)?,
        crates: rows(root, "crate", crate_from)?,
        postures: rows(root, "posture", posture_from)?,
        required_dependencies: rows(root, "required_dependency", required_dependency_from)?,
        dependency_narrowings: rows(root, "dependency_narrowing", dependency_narrowing_from)?,
        residue_allowances: rows(root, "residue_allowance", residue_allowance_from)?,
        asset_evidence_gaps: rows(root, "asset_evidence_gap", asset_evidence_gap_from)?,
        capabilities: rows(root, "capability", capability_from)?,
    })
}

pub fn parse_topology(text: &str) -> Result<TopologyRegistry, LoadError> {
    let table = parse(text).map_err(|error| LoadError {
        path: REGISTRY_PATH.into(),
        message: error.to_string(),
    })?;
    topology_from(&table).map_err(LoadError::from)
}

pub fn load_topology(path: &Path) -> Result<TopologyRegistry, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    parse_topology(&text).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.message,
    })
}

pub fn load_from_repo(root: &Path) -> Result<TopologyRegistry, LoadError> {
    load_topology(&root.join(REGISTRY_PATH))
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
// The live workspace scan
// -----------------------------------------------------------------------------

/// One dependency declaration, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestDependency {
    /// The key as written (may be a rename).
    pub key: String,
    /// The real package name: `package = "..."` when renamed, else the key.
    pub package: String,
    /// `dependencies` | `dev-dependencies` | `build-dependencies`.
    pub table: String,
    pub path: String,
    pub git: String,
    pub rev: String,
    /// True when the declaration carries `default-features = false`.
    pub default_features_disabled: bool,
}

#[derive(Debug, Clone)]
pub struct ScannedCrate {
    /// Repo-relative manifest directory, e.g. `crates/fgdb-types`.
    pub dir: String,
    pub package_name: String,
    pub dependencies: Vec<ManifestDependency>,
    pub lints_workspace: bool,
    pub root_path: String,
    pub root_forbids_unsafe: bool,
    /// `#![deny(unsafe_code)]` at the crate root. An island cannot inherit the
    /// workspace `forbid` — `forbid` cannot be lowered, so inheriting it would
    /// make every ledgered site uncompilable — so `deny` at the root is the
    /// ONLY thing standing between an island and unrestricted unsafe. It is
    /// therefore checked positively rather than assumed from the policy column.
    pub root_denies_unsafe: bool,
    /// Any `allow(unsafe_code)` / `expect(unsafe_code)` attribute anywhere in
    /// the crate's sources. The unsafe-boundary LEDGER owns site-level rows;
    /// this flag only answers "did an ordinary crate try to lower the policy".
    pub relaxes_unsafe: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceScan {
    /// `[workspace] members`, verbatim and sorted.
    pub members: Vec<String>,
    pub workspace_unsafe_lint: String,
    pub toolchain_channel: String,
    pub crates: Vec<ScannedCrate>,
}

impl WorkspaceScan {
    pub fn by_dir(&self, dir: &str) -> Option<&ScannedCrate> {
        self.crates.iter().find(|c| c.dir == dir)
    }

    pub fn by_package(&self, name: &str) -> Option<&ScannedCrate> {
        self.crates.iter().find(|c| c.package_name == name)
    }
}

/// Strip a `#` comment that is not inside a string literal.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => in_string = !in_string,
            b'#' if !in_string => return &line[..index],
            _ => {}
        }
    }
    line
}

fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Split an inline-table body on top-level commas (no nesting in Cargo
/// dependency declarations, but quotes must be respected).
fn split_inline_fields(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    for ch in body.chars() {
        match ch {
            '"' => {
                in_string = !in_string;
                current.push(ch);
            }
            ',' if !in_string => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn dependency_from_inline(
    key: &str,
    table: &str,
    body: &str,
) -> Result<ManifestDependency, String> {
    let mut dependency = ManifestDependency {
        key: key.to_string(),
        package: key.to_string(),
        table: table.to_string(),
        path: String::new(),
        git: String::new(),
        rev: String::new(),
        default_features_disabled: false,
    };
    for field in split_inline_fields(body) {
        let Some((name, value)) = field.split_once('=') else {
            return Err(format!(
                "dependency {key:?}: field {field:?} is not `key = value`"
            ));
        };
        let value = unquote(value);
        match name.trim() {
            "package" => dependency.package = value,
            "path" => dependency.path = value,
            "git" => dependency.git = value,
            "rev" | "tag" | "branch" => dependency.rev = value,
            "default-features" | "default_features" => {
                dependency.default_features_disabled = value == "false";
            }
            "version" | "features" | "optional" | "workspace" => {}
            other => {
                return Err(format!(
                    "dependency {key:?}: unsupported field {other:?} (the topology scanner reads a deliberate subset and fails closed)"
                ));
            }
        }
    }
    Ok(dependency)
}

/// Read one Cargo manifest into the shape the closed-universe law needs.
///
/// Deliberately NOT `crate::toml::parse`: that parser rejects inline tables by
/// design, and every git dependency in this workspace is one.
pub fn scan_manifest(dir: &str, text: &str) -> Result<ScannedCrate, String> {
    let mut package_name = String::new();
    let mut dependencies: Vec<ManifestDependency> = Vec::new();
    let mut lints_workspace = false;
    let mut section = String::new();
    // `[dependencies.foo]` form: the section itself names the dependency.
    let mut sub_dependency: Option<ManifestDependency> = None;
    let mut lib_path = String::new();

    let flush = |sub: &mut Option<ManifestDependency>, out: &mut Vec<ManifestDependency>| {
        if let Some(dependency) = sub.take() {
            out.push(dependency);
        }
    };

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let Some(header) = header.strip_suffix(']') else {
                return Err(format!(
                    "{dir}/Cargo.toml: unterminated section header {line:?}"
                ));
            };
            flush(&mut sub_dependency, &mut dependencies);
            let header = header.trim_start_matches('[').trim_end_matches(']');
            section = header.to_string();
            if let Some((table, name)) = header.rsplit_once('.')
                && table.ends_with("dependencies")
            {
                sub_dependency = Some(ManifestDependency {
                    key: name.to_string(),
                    package: name.to_string(),
                    table: table.rsplit('.').next().unwrap_or(table).to_string(),
                    path: String::new(),
                    git: String::new(),
                    rev: String::new(),
                    default_features_disabled: false,
                });
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "{dir}/Cargo.toml: line {line:?} is not `key = value`"
            ));
        };
        let key = key.trim();
        let value = value.trim();

        if let Some(dependency) = sub_dependency.as_mut() {
            match key {
                "package" => dependency.package = unquote(value),
                "path" => dependency.path = unquote(value),
                "git" => dependency.git = unquote(value),
                "rev" | "tag" | "branch" => dependency.rev = unquote(value),
                "default-features" | "default_features" => {
                    dependency.default_features_disabled = value == "false";
                }
                _ => {}
            }
            continue;
        }

        match section.as_str() {
            "package" if key == "name" => package_name = unquote(value),
            "lints" if key == "workspace" => lints_workspace = value == "true",
            "lib" if key == "path" => lib_path = unquote(value),
            table if table.ends_with("dependencies") => {
                let table = table.rsplit('.').next().unwrap_or(table).to_string();
                if let Some(body) = value.strip_prefix('{') {
                    let Some(body) = body.strip_suffix('}') else {
                        return Err(format!(
                            "{dir}/Cargo.toml: dependency {key:?} spans lines; the topology scanner reads single-line inline tables only"
                        ));
                    };
                    dependencies.push(dependency_from_inline(key, &table, body)?);
                } else {
                    dependencies.push(ManifestDependency {
                        key: key.to_string(),
                        package: key.to_string(),
                        table,
                        path: String::new(),
                        git: String::new(),
                        rev: String::new(),
                        default_features_disabled: false,
                    });
                }
            }
            _ => {}
        }
    }
    flush(&mut sub_dependency, &mut dependencies);
    dependencies.sort_by(|a, b| (&a.table, &a.key).cmp(&(&b.table, &b.key)));

    if package_name.is_empty() {
        return Err(format!("{dir}/Cargo.toml: [package] name is missing"));
    }
    let root_path = if lib_path.is_empty() {
        "src/lib.rs".to_string()
    } else {
        lib_path
    };
    Ok(ScannedCrate {
        dir: dir.to_string(),
        package_name,
        dependencies,
        lints_workspace,
        root_path,
        root_forbids_unsafe: false,
        root_denies_unsafe: false,
        relaxes_unsafe: false,
    })
}

fn collect_rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
    let mut entries = fs::read_dir(dir)
        .map_err(|error| format!("{}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Scan the live workspace: members, their manifests, and their unsafe posture.
pub fn scan_workspace(root: &Path) -> Result<WorkspaceScan, String> {
    let manifest_path = root.join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest_path)
        .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    let manifest =
        parse(&manifest_text).map_err(|error| format!("{}: {error}", manifest_path.display()))?;
    let workspace = get_table(&manifest, "workspace", "Cargo.toml").map_err(|e| e.to_string())?;
    let mut members =
        get_str_array(workspace, "members", "Cargo.toml.workspace").map_err(|e| e.to_string())?;
    members.sort();
    if members.iter().any(|member| member.contains('*')) {
        return Err(
            "Cargo.toml.workspace.members contains a glob; the topology law requires explicit members so a new directory cannot join the workspace silently".into(),
        );
    }

    let workspace_unsafe_lint = manifest
        .get("workspace")
        .and_then(|value| match value {
            Value::Table(table) => table.get("lints"),
            _ => None,
        })
        .and_then(|value| match value {
            Value::Table(table) => table.get("rust"),
            _ => None,
        })
        .and_then(|value| match value {
            Value::Table(table) => table.get("unsafe_code"),
            _ => None,
        })
        .and_then(|value| match value {
            Value::Str(text) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();

    let toolchain_path = root.join("rust-toolchain.toml");
    let toolchain_channel = match fs::read_to_string(&toolchain_path) {
        Ok(text) => parse(&text)
            .ok()
            .and_then(|table| match table.get("toolchain") {
                Some(Value::Table(toolchain)) => match toolchain.get("channel") {
                    Some(Value::Str(channel)) => Some(channel.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_default(),
        Err(_) => String::new(),
    };

    let mut crates = Vec::new();
    for member in &members {
        let member_dir = root.join(member);
        let member_manifest = member_dir.join("Cargo.toml");
        let text = fs::read_to_string(&member_manifest)
            .map_err(|error| format!("{}: {error}", member_manifest.display()))?;
        let mut scanned = scan_manifest(member, &text)?;
        let root_file = member_dir.join(&scanned.root_path);
        if let Ok(source) = fs::read_to_string(&root_file) {
            scanned.root_forbids_unsafe = source
                .lines()
                .any(|line| line.trim() == "#![forbid(unsafe_code)]");
            scanned.root_denies_unsafe = source
                .lines()
                .any(|line| line.trim() == "#![deny(unsafe_code)]");
        }
        let src_dir = member_dir.join("src");
        if src_dir.is_dir() {
            let mut sources = Vec::new();
            collect_rust_sources(&src_dir, &mut sources)?;
            for path in sources {
                let source = fs::read_to_string(&path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                // One reader for one fact. This was
                // `source.contains("allow(unsafe_code)")`, a second and weaker
                // reader of the question `unsafe_ledger::scan_sites` already
                // answered structurally, and the two disagreed on 5 of 10
                // attribute forms — every disagreement resolved against the
                // substring. It MISSED `allow(unsafe_code, clippy::x)` and
                // `allow( unsafe_code )` (the closing paren is not adjacent) and
                // `warn(unsafe_code)` (not in its vocabulary at all), and it
                // INVENTED a relaxation for `#[doc = "allow(unsafe_code)"]` and
                // for any comment quoting the rule — which this very crate's
                // sources do, so the substring already reported `registry-check`
                // as relaxing unsafe while the structural reader found zero
                // sites in it. That went unnoticed only because
                // `tools/registry-check` is a tooling member and Law 5 skips it.
                if !crate::unsafe_ledger::scan_sites(&path.display().to_string(), &source)
                    .is_empty()
                {
                    scanned.relaxes_unsafe = true;
                }
            }
        }
        crates.push(scanned);
    }

    Ok(WorkspaceScan {
        members,
        workspace_unsafe_lint,
        toolchain_channel,
        crates,
    })
}

// -----------------------------------------------------------------------------
// Derivations from the frozen plan blocks
// -----------------------------------------------------------------------------

/// Inclusive 1-based line range with every line's terminator kept. Byte-exact
/// on purpose: a registry that paraphrases its own source cannot be checked.
pub fn line_range(text: &str, start: usize, end: usize) -> Result<String, String> {
    if start == 0 || end < start {
        return Err(format!("invalid line range {start}..{end}"));
    }
    let mut out = String::new();
    let mut line_number = 0usize;
    for line in text.split_inclusive('\n') {
        line_number += 1;
        if line_number >= start && line_number <= end {
            out.push_str(line);
        }
        if line_number == end {
            return Ok(out);
        }
    }
    Err(format!(
        "line range {start}..{end} exceeds the file ({line_number} lines)"
    ))
}

pub fn source_block_text(block: &SourceBlock, root: &Path) -> Result<String, String> {
    let path = root.join(&block.plan_path);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    line_range(&text, block.plan_start_line, block.plan_end_line)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBlockCheck {
    pub id: String,
    pub line_count: usize,
    pub byte_count: usize,
    pub fnv1a64: String,
    pub outcome: &'static str,
}

pub fn check_source_blocks(
    registry: &TopologyRegistry,
    root: &Path,
) -> Vec<Result<SourceBlockCheck, String>> {
    let mut blocks: Vec<&SourceBlock> = registry.source_blocks.iter().collect();
    blocks.sort_by(|a, b| a.id.cmp(&b.id));
    blocks
        .into_iter()
        .map(|block| {
            let text = source_block_text(block, root)?;
            let line_count = text.lines().count();
            let byte_count = text.len();
            let digest = format!("0x{:016x}", fnv1a64(text.as_bytes()));
            let matches = line_count == block.line_count
                && byte_count == block.byte_count
                && digest == block.fnv1a64;
            Ok(SourceBlockCheck {
                id: block.id.clone(),
                line_count,
                byte_count,
                fnv1a64: digest,
                outcome: if matches { "pass" } else { "fail" },
            })
        })
        .collect()
}

/// One parsed row of the §18.1 table: the layer title and the backticked
/// `fgdb…` tokens in the order the source spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLayerRow {
    pub title: String,
    pub tokens: Vec<String>,
}

/// Is this a crate-shaped token — `fgdb` or `fgdb-<segments>`?
fn is_crate_token(token: &str) -> bool {
    if token == "fgdb" {
        return true;
    }
    let Some(rest) = token.strip_prefix("fgdb-") else {
        return false;
    };
    !rest.is_empty()
        && rest.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Parse the frozen §18.1 crate/layer table.
///
/// Markdown table, two columns. The crate universe is the set of backticked
/// crate-shaped tokens in column two, deduplicated per row keeping first
/// occurrence — the source cross-references other layers' crates inside
/// annotations (`fgdb-secure-view` in the Loom row, `fgdb-order` in Aegis), and
/// those mentions are references, not memberships.
pub fn parse_layer_table(block: &str) -> Result<Vec<ParsedLayerRow>, String> {
    let mut out: Vec<ParsedLayerRow> = Vec::new();
    for (index, line) in block.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with('|') {
            return Err(format!("§18.1 block line {} is not a table row", index + 1));
        }
        let cells: Vec<&str> = line
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if cells.len() != 2 {
            return Err(format!(
                "§18.1 block line {} has {} cells, expected 2",
                index + 1,
                cells.len()
            ));
        }
        if cells[0] == "Layer" || cells[0].starts_with("---") {
            continue;
        }
        let mut tokens: Vec<String> = Vec::new();
        let mut rest = cells[1];
        while let Some(open) = rest.find('`') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else {
                return Err(format!(
                    "§18.1 row {:?} has an unterminated backtick span",
                    cells[0]
                ));
            };
            let token = &after[..close];
            if is_crate_token(token) && !tokens.iter().any(|existing| existing == token) {
                tokens.push(token.to_string());
            }
            rest = &after[close + 1..];
        }
        out.push(ParsedLayerRow {
            title: cells[0].to_string(),
            tokens,
        });
    }
    Ok(out)
}

/// The result of decomposing the frozen §18.2 line against the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryCoverage {
    /// Phrases that did not occur exactly once when it was their turn.
    pub unresolved: Vec<(String, usize)>,
    /// What was left after every phrase and allowance was removed.
    pub residue: String,
    /// Residue characters outside the registered alphabet.
    pub illegal_residue: Vec<char>,
}

/// Delete every registered capability phrase and rationale allowance from the
/// §18.2 source line and report what is left.
///
/// Removal is order-sensitive and occurrence-checked: a phrase must occur
/// exactly once in the text remaining at its turn. That rules out the two ways
/// a substring decomposition lies — a phrase matching inside another phrase, and
/// a phrase matching in two places so that "covered" is ambiguous.
pub fn decompose_inventory(line: &str, registry: &TopologyRegistry) -> InventoryCoverage {
    let mut text = line.to_string();
    let mut unresolved = Vec::new();
    // Longest first: a phrase that contains another must be consumed before the
    // shorter one, or the shorter one matches inside it and the longer one is
    // then unfindable.
    let mut phrases: Vec<&str> = registry
        .capabilities
        .iter()
        .filter(|capability| capability.disposition != "design_only")
        .map(|capability| capability.source_phrase.as_str())
        .chain(
            registry
                .residue_allowances
                .iter()
                .map(|allowance| allowance.text.as_str()),
        )
        .collect();
    phrases.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    for phrase in phrases {
        let count = text.matches(phrase).count();
        if count == 1 {
            text = text.replacen(phrase, "", 1);
        } else {
            unresolved.push((phrase.to_string(), count));
        }
    }
    let illegal_residue: Vec<char> = {
        let mut seen: Vec<char> = text
            .chars()
            .filter(|c| !RESIDUE_ALPHABET.contains(c))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    };
    InventoryCoverage {
        unresolved,
        residue: text,
        illegal_residue,
    }
}

/// Resolve every `design_only` capability against the §2.3 donor table: nine
/// designs, nine table rows, one bijection.
pub fn design_bijection(block: &str, registry: &TopologyRegistry) -> Vec<Violation> {
    let mut violations = Vec::new();
    let rows: Vec<&str> = block
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .filter(|line| !line.starts_with("| frankensqlite design") && !line.starts_with("|---"))
        .collect();
    let designs: Vec<&Capability> = registry
        .capabilities
        .iter()
        .filter(|capability| capability.disposition == "design_only")
        .collect();
    if designs.len() != rows.len() {
        violations.push(Violation::new(
            "design_row_count_drift",
            "design_only",
            "§2.3",
            format!(
                "the donor table has {} rows and the registry declares {} design_only capabilities; the mapping must be a bijection",
                rows.len(),
                designs.len()
            ),
        ));
    }
    let mut claimed = vec![false; rows.len()];
    for design in &designs {
        let hits: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains(&design.source_phrase))
            .map(|(index, _)| index)
            .collect();
        match hits.len() {
            1 => {
                if claimed[hits[0]] {
                    violations.push(Violation::new(
                        "design_row_double_claim",
                        &design.id,
                        "§2.3",
                        format!(
                            "donor row {} is claimed by more than one design_only capability",
                            hits[0] + 1
                        ),
                    ));
                }
                claimed[hits[0]] = true;
            }
            0 => violations.push(Violation::new(
                "design_phrase_unresolved",
                &design.id,
                "§2.3",
                format!(
                    "source_phrase {:?} matches no row of the frozen donor table",
                    design.source_phrase
                ),
            )),
            n => violations.push(Violation::new(
                "design_phrase_ambiguous",
                &design.id,
                "§2.3",
                format!(
                    "source_phrase {:?} matches {n} donor rows; a design must name exactly one",
                    design.source_phrase
                ),
            )),
        }
    }
    for (index, taken) in claimed.iter().enumerate() {
        if !taken {
            violations.push(Violation::new(
                "design_row_unclaimed",
                format!("§2.3 row {}", index + 1),
                "§2.3",
                "no design_only capability claims this donor row; an unregistered donor design is an unbounded adoption claim".to_string(),
            ));
        }
    }
    violations
}

/// Every consume_from row's `foundation_asset` must resolve to exactly one line
/// of its project's frozen asset block. Many capabilities may share one asset
/// row (§2.1's distributed-protocol row supplies six), so this is many-to-one by
/// design — but it is never zero, and never ambiguous.
fn check_foundation_assets(
    registry: &TopologyRegistry,
    root: &Path,
    violations: &mut Vec<Violation>,
) {
    let blocks: BTreeMap<&str, &str> = BTreeMap::from([
        ("asupersync", "plan-asupersync-assets-v1"),
        ("franken_networkx", "plan-fnx-assets-v1"),
    ]);
    let mut texts: BTreeMap<&str, String> = BTreeMap::new();
    for (project, block_id) in &blocks {
        let Some(block) = registry.source_block(block_id) else {
            violations.push(Violation::new(
                "source_block_missing",
                *block_id,
                "§2.1/§2.2",
                "the consume_from evidence law needs this source block",
            ));
            continue;
        };
        match source_block_text(block, root) {
            Ok(text) => {
                texts.insert(project, text);
            }
            Err(message) => violations.push(Violation::new(
                "source_block_unreadable",
                *block_id,
                "§2.1/§2.2",
                message,
            )),
        }
    }
    for capability in &registry.capabilities {
        if capability.disposition != "consume_from" {
            continue;
        }
        let gap = registry
            .asset_evidence_gaps
            .iter()
            .find(|gap| gap.capability_id == capability.id);
        let Some(text) = texts.get(capability.foundation_project.as_str()) else {
            continue;
        };
        match gap {
            None => {
                let hits = text
                    .lines()
                    .filter(|line| line.contains(&capability.foundation_asset))
                    .count();
                if hits != 1 {
                    violations.push(Violation::new(
                        "foundation_asset_unresolved",
                        &capability.id,
                        "§2.1/§2.2",
                        format!(
                            "foundation_asset {:?} resolves to {hits} lines of the frozen {} asset block; exact package/source evidence must resolve to exactly one",
                            capability.foundation_asset, capability.foundation_project
                        ),
                    ));
                }
            }
            Some(gap) => {
                // The gap is legal only while the absence is real. A row that
                // could resolve must resolve.
                if !capability.foundation_asset.is_empty() {
                    violations.push(Violation::new(
                        "asset_gap_with_asset",
                        &capability.id,
                        "§2.1/§2.2",
                        "a registered evidence gap names no asset row: the gap IS the absence",
                    ));
                }
                let hits = text
                    .lines()
                    .filter(|line| line.contains(&capability.source_phrase))
                    .count();
                if hits != 0 {
                    violations.push(Violation::new(
                        "asset_gap_resolvable",
                        &capability.id,
                        &gap.verified_absent_from,
                        format!(
                            "the frozen asset block names this capability on {hits} line(s), so the registered evidence gap is stale; name the asset row instead"
                        ),
                    ));
                }
            }
        }
    }
    for gap in &registry.asset_evidence_gaps {
        match registry
            .capabilities
            .iter()
            .find(|capability| capability.id == gap.capability_id)
        {
            None => violations.push(Violation::new(
                "asset_gap_unresolved",
                &gap.capability_id,
                "§18.2",
                "an evidence gap names a capability that does not exist",
            )),
            Some(capability) if capability.disposition != "consume_from" => {
                violations.push(Violation::new(
                    "asset_gap_wrong_disposition",
                    &gap.capability_id,
                    "§18.2",
                    "only a consumed capability can lack a foundation asset row",
                ));
            }
            Some(_) => {}
        }
        if registry.source_block(&gap.verified_absent_from).is_none() {
            violations.push(Violation::new(
                "asset_gap_block_unresolved",
                &gap.capability_id,
                "§2.1/§2.2",
                format!(
                    "verified_absent_from {:?} is not a registered source block",
                    gap.verified_absent_from
                ),
            ));
        }
    }
}

/// Kahn's algorithm over the live crate graph. Returns the crates that remain
/// when no node has in-degree zero — i.e. every crate on or downstream of a
/// cycle, sorted.
pub fn crate_graph_cycle(scan: &WorkspaceScan) -> Vec<String> {
    let names: BTreeSet<&str> = scan
        .crates
        .iter()
        .map(|entry| entry.package_name.as_str())
        .collect();
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for entry in &scan.crates {
        let targets = edges.entry(entry.package_name.as_str()).or_default();
        for dependency in &entry.dependencies {
            if let Some(target) = names.get(dependency.package.as_str())
                && *target != entry.package_name.as_str()
            {
                targets.insert(*target);
            }
        }
    }
    let mut remaining: BTreeSet<&str> = names.clone();
    loop {
        let leaf = remaining
            .iter()
            .find(|name| {
                edges
                    .get(**name)
                    .map(|targets| targets.iter().all(|target| !remaining.contains(target)))
                    .unwrap_or(true)
            })
            .copied();
        match leaf {
            Some(name) => {
                remaining.remove(name);
            }
            None => break,
        }
    }
    remaining.into_iter().map(str::to_string).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostureClosure {
    pub posture_id: String,
    pub entry_crate: String,
    pub status: &'static str,
    pub closure: Vec<String>,
    pub illegal: Vec<String>,
}

/// Transitive closure of a posture's entry crate over the live graph, and the
/// crates in it whose declared participation forbids being there.
///
/// With no composition crate active every posture resolves to `deferred`. The
/// evaluator is therefore proved against synthetic graphs in the suite: a law
/// nobody has watched fire is a law nobody knows works.
pub fn posture_closures(registry: &TopologyRegistry, scan: &WorkspaceScan) -> Vec<PostureClosure> {
    registry
        .postures
        .iter()
        .map(|posture| {
            let Some(entry) = scan.by_package(&posture.entry_crate) else {
                return PostureClosure {
                    posture_id: posture.id.clone(),
                    entry_crate: posture.entry_crate.clone(),
                    status: "deferred",
                    closure: Vec::new(),
                    illegal: Vec::new(),
                };
            };
            let mut closure: BTreeSet<String> = BTreeSet::new();
            let mut frontier = vec![entry.package_name.clone()];
            while let Some(name) = frontier.pop() {
                if !closure.insert(name.clone()) {
                    continue;
                }
                if let Some(scanned) = scan.by_package(&name) {
                    for dependency in &scanned.dependencies {
                        if dependency.table == "dependencies"
                            && scan.by_package(&dependency.package).is_some()
                        {
                            frontier.push(dependency.package.clone());
                        }
                    }
                }
            }
            let illegal = closure
                .iter()
                .filter(|name| **name != posture.entry_crate)
                .filter(|name| {
                    registry
                        .crate_row(name)
                        .map(|row| {
                            row.posture_participation == "test_only"
                                || row.posture_participation == "packaging_boundary"
                                || row.posture_participation.starts_with("entry_")
                        })
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            PostureClosure {
                posture_id: posture.id.clone(),
                entry_crate: posture.entry_crate.clone(),
                status: "live",
                closure: closure.into_iter().collect(),
                illegal,
            }
        })
        .collect()
}

/// Resolve a required-dependency endpoint to the crate rows it admits.
fn endpoint_crates<'a>(
    registry: &'a TopologyRegistry,
    kind: &str,
    value: &str,
) -> Vec<&'a CrateRow> {
    match kind {
        "crate" => registry.crate_row(value).into_iter().collect(),
        "layer" => registry.layer_crates(value),
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredEdgeStatus {
    pub id: String,
    pub status: &'static str,
    pub detail: String,
}

/// Evaluate every named crate-level edge against the live graph. A row whose
/// endpoints are not both active is `deferred`, never `pass`.
pub fn required_edge_statuses(
    registry: &TopologyRegistry,
    scan: &WorkspaceScan,
) -> Vec<RequiredEdgeStatus> {
    registry
        .required_dependencies
        .iter()
        .map(|edge| {
            let sources: Vec<&CrateRow> = endpoint_crates(registry, &edge.from_kind, &edge.from)
                .into_iter()
                .filter(|row| row.activation_status == "active")
                .collect();
            if sources.is_empty() {
                return RequiredEdgeStatus {
                    id: edge.id.clone(),
                    status: "deferred",
                    detail: format!("no active crate on the {} side", edge.from_kind),
                };
            }
            if edge.to_kind == "foundation" {
                let Some(project) = registry.foundation_project(&edge.to) else {
                    return RequiredEdgeStatus {
                        id: edge.id.clone(),
                        status: "fail",
                        detail: format!("unknown foundation project {:?}", edge.to),
                    };
                };
                let satisfied = sources.iter().any(|row| {
                    scan.by_package(&row.name)
                        .map(|scanned| {
                            scanned.dependencies.iter().any(|dependency| {
                                project
                                    .package_prefixes
                                    .iter()
                                    .any(|prefix| dependency.package.starts_with(prefix))
                            })
                        })
                        .unwrap_or(false)
                });
                return RequiredEdgeStatus {
                    id: edge.id.clone(),
                    status: if satisfied { "pass" } else { "fail" },
                    detail: format!("{} -> {}", edge.from, project.id),
                };
            }
            let targets: Vec<&CrateRow> = endpoint_crates(registry, &edge.to_kind, &edge.to)
                .into_iter()
                .filter(|row| row.activation_status == "active")
                .collect();
            if targets.is_empty() {
                return RequiredEdgeStatus {
                    id: edge.id.clone(),
                    status: "deferred",
                    detail: format!("no active crate on the {} side", edge.to_kind),
                };
            }
            let satisfied = sources.iter().any(|source| {
                scan.by_package(&source.name)
                    .map(|scanned| {
                        scanned.dependencies.iter().any(|dependency| {
                            targets
                                .iter()
                                .any(|target| target.name == dependency.package)
                        })
                    })
                    .unwrap_or(false)
            });
            RequiredEdgeStatus {
                id: edge.id.clone(),
                status: if satisfied { "pass" } else { "fail" },
                detail: format!("{} -> {}", edge.from, edge.to),
            }
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Hash pins
// -----------------------------------------------------------------------------

/// Every stable id the registry declares, sorted. Moves on any addition,
/// removal, or rename.
pub fn id_table(registry: &TopologyRegistry) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    ids.extend(
        registry
            .source_blocks
            .iter()
            .map(|block| format!("source_block:{}", block.id)),
    );
    ids.extend(
        registry
            .layers
            .iter()
            .map(|layer| format!("layer:{}", layer.id)),
    );
    ids.extend(
        registry
            .owner_scopes
            .iter()
            .map(|scope| format!("owner_scope:{}", scope.id)),
    );
    ids.extend(
        registry
            .foundation_projects
            .iter()
            .map(|project| format!("foundation_project:{}", project.id)),
    );
    ids.extend(
        registry
            .forbidden_dependencies
            .iter()
            .map(|row| format!("forbidden_dependency:{}", row.id)),
    );
    ids.extend(
        registry
            .crates
            .iter()
            .map(|row| format!("crate:{}", row.name)),
    );
    ids.extend(
        registry
            .postures
            .iter()
            .map(|posture| format!("posture:{}", posture.id)),
    );
    ids.extend(
        registry
            .required_dependencies
            .iter()
            .map(|edge| format!("required_dependency:{}", edge.id)),
    );
    ids.extend(
        registry
            .dependency_narrowings
            .iter()
            .map(|row| format!("dependency_narrowing:{}", row.crate_name)),
    );
    ids.extend(
        registry
            .residue_allowances
            .iter()
            .map(|row| format!("residue_allowance:{}", row.id)),
    );
    ids.extend(
        registry
            .asset_evidence_gaps
            .iter()
            .map(|row| format!("asset_evidence_gap:{}", row.capability_id)),
    );
    ids.extend(
        registry
            .capabilities
            .iter()
            .map(|row| format!("capability:{}", row.id)),
    );
    ids.sort();
    ids
}

pub fn recompute_id_table_hash(registry: &TopologyRegistry) -> String {
    id_table_hash(&id_table(registry))
}

/// The semantic contract: every decision this registry freezes that a
/// downstream consumer could read as normative. Deliberately excludes prose
/// (charters, roles, notes, reasons) so a copy edit does not read as a contract
/// change, and deliberately includes every layer edge, activation status,
/// unsafe policy, posture participation, pinned revision, and capability
/// disposition, because those ARE the contract.
pub fn recompute_semantic_contract_hash(registry: &TopologyRegistry) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "workspace|{}|{}|{}|{}",
        registry.registry.workspace_unsafe_lint,
        registry.registry.toolchain_channel,
        registry.registry.tooling_members.join(","),
        registry.registry.unsafe_ledger_registry
    ));
    lines.push(format!(
        "layer_law|{}|{}",
        registry.layer_law.reciprocal_pair.join(","),
        registry.layer_law.crate_graph_must_be_acyclic
    ));
    for layer in &registry.layers {
        lines.push(format!(
            "layer|{}|{}|{}|{}",
            layer.id,
            layer.title,
            layer.source_order,
            layer.allowed_outgoing_layers.join(",")
        ));
    }
    for scope in &registry.owner_scopes {
        lines.push(format!("owner|{}|{}|{}", scope.id, scope.kind, scope.title));
    }
    for project in &registry.foundation_projects {
        lines.push(format!(
            "foundation|{}|{}|{}|{}|{}|{}",
            project.id,
            project.linkage,
            project.git_url,
            project.pinned_rev,
            project.package_prefixes.join(","),
            project.default_features_must_be_disabled
        ));
    }
    for row in &registry.forbidden_dependencies {
        lines.push(format!(
            "forbidden|{}|{}|{}",
            row.id, row.selector, row.package_prefix
        ));
    }
    for row in &registry.crates {
        lines.push(format!(
            "crate|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            row.name,
            row.layer,
            row.layer_position,
            row.unsafe_policy,
            row.activation_status,
            row.posture_participation,
            row.posture_basis,
            row.owner,
            row.manifest_dir
        ));
    }
    for posture in &registry.postures {
        lines.push(format!(
            "posture|{}|{}|{}|{}",
            posture.id, posture.entry_crate, posture.binary_name, posture.status
        ));
    }
    for edge in &registry.required_dependencies {
        lines.push(format!(
            "required|{}|{}|{}|{}|{}",
            edge.id, edge.from_kind, edge.from, edge.to_kind, edge.to
        ));
    }
    for row in &registry.dependency_narrowings {
        lines.push(format!(
            "narrowing|{}|{}|{}|{}",
            row.crate_name,
            row.allowed_layers.join(","),
            row.allowed_crates.join(","),
            row.allowed_foundation_projects.join(",")
        ));
    }
    for row in &registry.residue_allowances {
        lines.push(format!("allowance|{}|{}", row.id, row.text));
    }
    for row in &registry.asset_evidence_gaps {
        lines.push(format!(
            "asset_gap|{}|{}",
            row.capability_id, row.verified_absent_from
        ));
    }
    for row in &registry.capabilities {
        lines.push(format!(
            "capability|{}|{}|{}|{}|{}",
            row.id, row.disposition, row.source_phrase, row.owner_crate, row.foundation_project
        ));
    }
    lines.sort();
    format!("fnv1a64:{:016x}", fnv1a64(lines.join("\n").as_bytes()))
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

fn validate_header(registry: &TopologyRegistry, violations: &mut Vec<Violation>) {
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
    if registry.registry.name != "workspace_topology" {
        violations.push(Violation::new(
            "registry_name",
            &registry.registry.name,
            "§19 G0",
            "registry.name must be \"workspace_topology\"",
        ));
    }
    if registry.registry.replay_command != REPLAY_COMMAND {
        violations.push(Violation::new(
            "replay_command_drift",
            "registry.replay_command",
            "§19 G0",
            format!("replay_command must be {REPLAY_COMMAND:?}"),
        ));
    }
    let header = &registry.registry;
    check_count(
        "layer_count",
        header.layer_count,
        registry.layers.len(),
        "§18.1",
        violations,
    );
    check_count(
        "crate_count",
        header.crate_count,
        registry.crates.len(),
        "§18.1",
        violations,
    );
    check_count(
        "owner_scope_count",
        header.owner_scope_count,
        registry.owner_scopes.len(),
        "§19",
        violations,
    );
    check_count(
        "foundation_project_count",
        header.foundation_project_count,
        registry.foundation_projects.len(),
        "§1 constraint 1",
        violations,
    );
    check_count(
        "capability_count",
        header.capability_count,
        registry.capabilities.len(),
        "§18.2",
        violations,
    );
    check_count(
        "residue_allowance_count",
        header.residue_allowance_count,
        registry.residue_allowances.len(),
        "§18.2",
        violations,
    );
    check_count(
        "required_dependency_count",
        header.required_dependency_count,
        registry.required_dependencies.len(),
        "§18.1",
        violations,
    );
    check_count(
        "asset_evidence_gap_count",
        header.asset_evidence_gap_count,
        registry.asset_evidence_gaps.len(),
        "§2.1/§2.2",
        violations,
    );
    check_count(
        "forbidden_dependency_count",
        header.forbidden_dependency_count,
        registry.forbidden_dependencies.len(),
        "§1 constraint 1",
        violations,
    );
    check_count(
        "dependency_narrowing_count",
        header.dependency_narrowing_count,
        registry.dependency_narrowings.len(),
        "§15.2",
        violations,
    );
    check_count(
        "posture_count",
        header.posture_count,
        registry.postures.len(),
        "§1 constraint 5",
        violations,
    );
    check_count(
        "source_block_count",
        header.source_block_count,
        registry.source_blocks.len(),
        "§19 G0",
        violations,
    );
    for (label, declared, status) in [
        ("active_crate_count", header.active_crate_count, "active"),
        ("planned_crate_count", header.planned_crate_count, "planned"),
        (
            "reserved_crate_count",
            header.reserved_crate_count,
            "reserved",
        ),
    ] {
        let actual = registry
            .crates
            .iter()
            .filter(|row| row.activation_status == status)
            .count();
        check_count(label, declared, actual, "§18.1", violations);
    }
    for (label, declared, disposition) in [
        ("build_here_count", header.build_here_count, "build_here"),
        (
            "consume_from_count",
            header.consume_from_count,
            "consume_from",
        ),
        ("design_only_count", header.design_only_count, "design_only"),
    ] {
        let actual = registry
            .capabilities
            .iter()
            .filter(|row| row.disposition == disposition)
            .count();
        check_count(label, declared, actual, "§18.2", violations);
    }
    for id in &header.embedded_source_blocks {
        if registry.source_block(id).is_none() {
            violations.push(Violation::new(
                "embedded_block_unresolved",
                id,
                "§19 G0",
                "embedded_source_blocks names a source block that does not exist",
            ));
        }
    }
    let recomputed_ids = recompute_id_table_hash(registry);
    if recomputed_ids != header.id_table_hash {
        violations.push(Violation::new(
            "id_table_hash_drift",
            "registry.id_table_hash",
            "§19 G0",
            format!(
                "declared {}, recomputed {recomputed_ids}",
                header.id_table_hash
            ),
        ));
    }
    let recomputed_contract = recompute_semantic_contract_hash(registry);
    if recomputed_contract != header.semantic_contract_hash {
        violations.push(Violation::new(
            "semantic_contract_hash_drift",
            "registry.semantic_contract_hash",
            "§19 G0",
            format!(
                "declared {}, recomputed {recomputed_contract}",
                header.semantic_contract_hash
            ),
        ));
    }
}

fn validate_layers(registry: &TopologyRegistry, violations: &mut Vec<Violation>) {
    let mut by_order: Vec<&Layer> = registry.layers.iter().collect();
    by_order.sort_by_key(|layer| layer.source_order);
    for (index, layer) in by_order.iter().enumerate() {
        if layer.source_order != index + 1 {
            violations.push(Violation::new(
                "layer_order_not_dense",
                &layer.id,
                "§18.1",
                format!(
                    "layer source_order must be a dense 1..{} sequence; found {} at position {}",
                    registry.layers.len(),
                    layer.source_order,
                    index + 1
                ),
            ));
        }
    }
    let titles: Vec<&str> = by_order.iter().map(|layer| layer.title.as_str()).collect();
    if titles != LAYER_TITLES.to_vec() {
        violations.push(Violation::new(
            "layer_titles_drift",
            "layers",
            "§18.1",
            format!("layer titles must be exactly {LAYER_TITLES:?}, found {titles:?}"),
        ));
    }
    // The derived allowed-edge formula. Recomputed, not trusted: an edited row
    // that widened one layer by one entry is exactly the kind of change that
    // reads as noise in a diff.
    let islands = "unsafe_islands";
    for layer in &registry.layers {
        let mut expected: Vec<String> = registry
            .layers
            .iter()
            .filter(|other| other.source_order <= layer.source_order || other.id == islands)
            .map(|other| other.id.clone())
            .collect();
        expected.sort();
        let mut declared = layer.allowed_outgoing_layers.clone();
        declared.sort();
        if declared != expected {
            violations.push(Violation::new(
                "layer_allowed_edges_drift",
                &layer.id,
                "§18.1",
                format!(
                    "allowed_outgoing_layers must be the derived set {expected:?}, found {declared:?}"
                ),
            ));
        }
        for target in &layer.allowed_outgoing_layers {
            if registry.layer(target).is_none() {
                violations.push(Violation::new(
                    "layer_edge_unresolved",
                    &layer.id,
                    "§18.1",
                    format!("allowed_outgoing_layers names unknown layer {target:?}"),
                ));
            }
        }
    }
    if registry.layer_law.reciprocal_pair != vec!["foundation".to_string(), islands.to_string()] {
        violations.push(Violation::new(
            "reciprocal_pair_drift",
            "layer_law.reciprocal_pair",
            "§18.1",
            "the one registered reciprocal layer pair is [foundation, unsafe_islands]",
        ));
    }
    if !registry.layer_law.crate_graph_must_be_acyclic {
        violations.push(Violation::new(
            "acyclicity_disabled",
            "layer_law.crate_graph_must_be_acyclic",
            "§18.1",
            "the reciprocal layer pair is only sound while the CRATE graph is unconditionally acyclic",
        ));
    }
}

fn validate_crates(registry: &TopologyRegistry, violations: &mut Vec<Violation>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for row in &registry.crates {
        if !seen.insert(row.name.as_str()) {
            violations.push(Violation::new(
                "crate_duplicate",
                &row.name,
                "§18.1",
                "a crate may appear in exactly one row",
            ));
        }
        check_enum(
            &row.unsafe_policy,
            &UNSAFE_POLICIES,
            "bad_unsafe_policy",
            &row.name,
            "§1 constraint 2",
            "unsafe_policy",
            violations,
        );
        check_enum(
            &row.activation_status,
            &ACTIVATION_STATUSES,
            "bad_activation_status",
            &row.name,
            "§18.1",
            "activation_status",
            violations,
        );
        check_enum(
            &row.role_basis,
            &ROLE_BASES,
            "bad_role_basis",
            &row.name,
            "§18.1",
            "role_basis",
            violations,
        );
        check_enum(
            &row.posture_participation,
            &POSTURE_PARTICIPATIONS,
            "bad_posture_participation",
            &row.name,
            "§1 constraint 5",
            "posture_participation",
            violations,
        );
        check_enum(
            &row.posture_basis,
            &POSTURE_BASES,
            "bad_posture_basis",
            &row.name,
            "§1 constraint 5",
            "posture_basis",
            violations,
        );
        if registry.layer(&row.layer).is_none() {
            violations.push(Violation::new(
                "crate_layer_unresolved",
                &row.name,
                "§18.1",
                format!("layer {:?} is not a registered layer", row.layer),
            ));
        }
        if !registry
            .owner_scopes
            .iter()
            .any(|scope| scope.id == row.owner)
        {
            violations.push(Violation::new(
                "crate_owner_unresolved",
                &row.name,
                "§19",
                format!("owner {:?} is not a registered owner scope", row.owner),
            ));
        }
        // FG-CON-02 by enumeration: exactly the three named islands may relax.
        let is_island = UNSAFE_ISLANDS.contains(&row.name.as_str());
        if is_island && row.unsafe_policy != "deny_ledgered" {
            violations.push(Violation::new(
                "island_policy_drift",
                &row.name,
                "§1 constraint 2",
                "an unsafe island carries deny_ledgered",
            ));
        }
        if !is_island && row.unsafe_policy != "forbid" {
            violations.push(Violation::new(
                "unsafe_policy_relaxed",
                &row.name,
                "§1 constraint 2",
                "only fgdb-unsafe-simd/arena/vfs may carry a policy other than forbid",
            ));
        }
        if is_island && row.layer != "unsafe_islands" {
            violations.push(Violation::new(
                "island_layer_drift",
                &row.name,
                "§18.1",
                "an unsafe island belongs to the unsafe_islands layer",
            ));
        }
        match row.activation_status.as_str() {
            "active" => {
                if row.manifest_dir.is_empty() {
                    violations.push(Violation::new(
                        "active_without_manifest_dir",
                        &row.name,
                        "§18.1",
                        "an active crate declares its manifest directory",
                    ));
                }
                if row.owner_bead.is_empty() {
                    violations.push(Violation::new(
                        "active_without_owner_bead",
                        &row.name,
                        "§19",
                        "an active crate names the bead that landed it",
                    ));
                }
            }
            _ => {
                if !row.manifest_dir.is_empty() {
                    violations.push(Violation::new(
                        "inactive_with_manifest_dir",
                        &row.name,
                        "§18.1",
                        "a crate that is not active has no manifest directory: a crate appears only with its first real final-abstraction slice",
                    ));
                }
            }
        }
    }
    // Dense per-layer positions.
    for layer in &registry.layers {
        let rows = registry.layer_crates(&layer.id);
        for (index, row) in rows.iter().enumerate() {
            if row.layer_position != index + 1 {
                violations.push(Violation::new(
                    "layer_position_not_dense",
                    &row.name,
                    "§18.1",
                    format!(
                        "layer_position must be a dense 1..{} sequence within {}; found {} at position {}",
                        rows.len(),
                        layer.id,
                        row.layer_position,
                        index + 1
                    ),
                ));
            }
        }
    }
    // Posture entry crates: exactly one per posture, and the participation
    // column must agree with the posture that claims it.
    for posture in &registry.postures {
        check_enum(
            &posture.status,
            &POSTURE_STATUSES,
            "bad_posture_status",
            &posture.id,
            "§1 constraint 5",
            "status",
            violations,
        );
        let expected = format!("entry_{}", posture.id);
        match registry.crate_row(&posture.entry_crate) {
            None => violations.push(Violation::new(
                "posture_entry_unresolved",
                &posture.id,
                "§18.1 Composition",
                format!(
                    "entry_crate {:?} is not a registered crate",
                    posture.entry_crate
                ),
            )),
            Some(row) if row.posture_participation != expected => {
                violations.push(Violation::new(
                    "posture_entry_participation_drift",
                    &posture.id,
                    "§1 constraint 5",
                    format!(
                        "{} claims {} but that crate declares posture_participation {:?}",
                        posture.id, posture.entry_crate, row.posture_participation
                    ),
                ));
            }
            Some(_) => {}
        }
    }
    for row in &registry.crates {
        if let Some(posture) = row.posture_participation.strip_prefix("entry_") {
            let claims = registry
                .postures
                .iter()
                .filter(|candidate| candidate.entry_crate == row.name)
                .count();
            if claims != 1 {
                violations.push(Violation::new(
                    "entry_crate_unclaimed",
                    &row.name,
                    "§1 constraint 5",
                    format!(
                        "declares entry_{posture} but {claims} posture rows name it as entry_crate"
                    ),
                ));
            }
        }
    }
    for scope in &registry.owner_scopes {
        check_enum(
            &scope.kind,
            &OWNER_KINDS,
            "bad_owner_kind",
            &scope.id,
            "§19",
            "kind",
            violations,
        );
    }
    for project in &registry.foundation_projects {
        check_enum(
            &project.linkage,
            &LINKAGES,
            "bad_linkage",
            &project.id,
            "§1 constraint 1",
            "linkage",
            violations,
        );
        if project.linkage == "design_only" && !project.pinned_rev.is_empty() {
            violations.push(Violation::new(
                "design_only_pinned",
                &project.id,
                "§2.3",
                "a design-only project has no pinned revision: nothing links it",
            ));
        }
        if project.linkage != "design_only" && project.pinned_rev.is_empty() {
            violations.push(Violation::new(
                "linkable_without_pin",
                &project.id,
                "§1 constraint 1",
                "a linkable foundation project carries exactly one pinned revision",
            ));
        }
        if project.package_prefixes.is_empty() {
            violations.push(Violation::new(
                "foundation_without_prefix",
                &project.id,
                "§1 constraint 1",
                "package_prefixes is how the closed-universe law recognizes this project",
            ));
        }
    }
    for row in &registry.dependency_narrowings {
        if registry.crate_row(&row.crate_name).is_none() {
            violations.push(Violation::new(
                "narrowing_crate_unresolved",
                &row.crate_name,
                "§15.2",
                "a dependency narrowing names a registered crate",
            ));
        }
        for layer in &row.allowed_layers {
            if registry.layer(layer).is_none() {
                violations.push(Violation::new(
                    "narrowing_layer_unresolved",
                    &row.crate_name,
                    "§15.2",
                    format!("allowed_layers names unknown layer {layer:?}"),
                ));
            }
        }
        for name in &row.allowed_crates {
            if registry.crate_row(name).is_none() {
                violations.push(Violation::new(
                    "narrowing_exception_unresolved",
                    &row.crate_name,
                    "§15.2",
                    format!("allowed_crates names unknown crate {name:?}"),
                ));
            }
        }
    }
    for edge in &registry.required_dependencies {
        check_enum(
            &edge.from_kind,
            &ENDPOINT_KINDS,
            "bad_endpoint_kind",
            &edge.id,
            "§18.1",
            "from_kind",
            violations,
        );
        check_enum(
            &edge.to_kind,
            &ENDPOINT_KINDS,
            "bad_endpoint_kind",
            &edge.id,
            "§18.1",
            "to_kind",
            violations,
        );
        if edge.from_kind == "foundation" {
            violations.push(Violation::new(
                "foundation_source_endpoint",
                &edge.id,
                "§18.1",
                "a foundation project is never the SOURCE of a required edge: we depend on it, it does not depend on us",
            ));
        }
        let resolved = match edge.from_kind.as_str() {
            "crate" => registry.crate_row(&edge.from).is_some(),
            "layer" => registry.layer(&edge.from).is_some(),
            _ => false,
        };
        if !resolved {
            violations.push(Violation::new(
                "required_edge_endpoint_unresolved",
                &edge.id,
                "§18.1",
                format!(
                    "from {:?} does not resolve as a {}",
                    edge.from, edge.from_kind
                ),
            ));
        }
        let resolved_to = match edge.to_kind.as_str() {
            "crate" => registry.crate_row(&edge.to).is_some(),
            "layer" => registry.layer(&edge.to).is_some(),
            "foundation" => registry.foundation_project(&edge.to).is_some(),
            _ => false,
        };
        if !resolved_to {
            violations.push(Violation::new(
                "required_edge_endpoint_unresolved",
                &edge.id,
                "§18.1",
                format!("to {:?} does not resolve as a {}", edge.to, edge.to_kind),
            ));
        }
    }
    for capability in &registry.capabilities {
        check_enum(
            &capability.disposition,
            &DISPOSITIONS,
            "bad_disposition",
            &capability.id,
            "§18.2",
            "disposition",
            violations,
        );
        if !capability.owner_crate.is_empty()
            && registry.crate_row(&capability.owner_crate).is_none()
        {
            violations.push(Violation::new(
                "capability_owner_unresolved",
                &capability.id,
                "§18.2",
                format!(
                    "owner_crate {:?} is not a registered crate",
                    capability.owner_crate
                ),
            ));
        }
        match capability.disposition.as_str() {
            "build_here" => {
                if capability.owner_crate.is_empty() {
                    violations.push(Violation::new(
                        "build_here_without_owner",
                        &capability.id,
                        "§18.2",
                        "a capability we build names the crate that builds it",
                    ));
                }
                if !capability.foundation_project.is_empty() {
                    violations.push(Violation::new(
                        "build_here_with_foundation",
                        &capability.id,
                        "§18.2",
                        "a build_here capability names no foundation project: that is what makes the partition a partition",
                    ));
                }
            }
            "consume_from" => {
                if !capability.owner_crate.is_empty() {
                    violations.push(Violation::new(
                        "consume_from_with_owner",
                        &capability.id,
                        "§18.2",
                        "a consumed capability has no owning fgdb crate; an owner column here is the duplicate-subsystem failure in registry form",
                    ));
                }
                match registry.foundation_project(&capability.foundation_project) {
                    None => violations.push(Violation::new(
                        "consume_from_unresolved",
                        &capability.id,
                        "§18.2",
                        format!(
                            "foundation_project {:?} is not registered",
                            capability.foundation_project
                        ),
                    )),
                    Some(project) if project.linkage == "design_only" => {
                        violations.push(Violation::new(
                            "consume_from_design_only",
                            &capability.id,
                            "§2.3",
                            "a design-only project supplies designs, never a consumed capability",
                        ));
                    }
                    Some(_) => {}
                }
                let has_gap = registry
                    .asset_evidence_gaps
                    .iter()
                    .any(|gap| gap.capability_id == capability.id);
                if capability.foundation_asset.is_empty() && !has_gap {
                    violations.push(Violation::new(
                        "consume_from_without_asset",
                        &capability.id,
                        "§2.1/§2.2",
                        "exact package/source evidence is required: name the asset row, or register the absence as an asset_evidence_gap",
                    ));
                }
            }
            "design_only" => {
                if capability.foundation_project != "frankensqlite" {
                    violations.push(Violation::new(
                        "design_only_wrong_project",
                        &capability.id,
                        "§2.3",
                        "frankensqlite is the only design-only donor",
                    ));
                }
                if !capability.foundation_asset.is_empty() {
                    violations.push(Violation::new(
                        "design_only_with_asset",
                        &capability.id,
                        "§2.3",
                        "a design-only row names no consumed asset",
                    ));
                }
            }
            _ => {}
        }
    }
}

/// Law 1 — the crate universe is derived from the frozen §18.1 table.
fn validate_derived_universe(
    registry: &TopologyRegistry,
    root: &Path,
    violations: &mut Vec<Violation>,
) {
    let Some(block) = registry.source_block("plan-crate-layer-table-v1") else {
        violations.push(Violation::new(
            "source_block_missing",
            "plan-crate-layer-table-v1",
            "§18.1",
            "the crate universe is derived from this block; without it nothing is checked",
        ));
        return;
    };
    let text = match source_block_text(block, root) {
        Ok(text) => text,
        Err(message) => {
            violations.push(Violation::new(
                "source_block_unreadable",
                &block.id,
                "§18.1",
                message,
            ));
            return;
        }
    };
    let parsed = match parse_layer_table(&text) {
        Ok(parsed) => parsed,
        Err(message) => {
            violations.push(Violation::new(
                "layer_table_unparsable",
                &block.id,
                "§18.1",
                message,
            ));
            return;
        }
    };
    if parsed.len() != registry.layers.len() {
        violations.push(Violation::new(
            "layer_row_count_drift",
            "plan-crate-layer-table-v1",
            "§18.1",
            format!(
                "the frozen table has {} layer rows, the registry declares {}",
                parsed.len(),
                registry.layers.len()
            ),
        ));
    }
    let declared_names: BTreeSet<&str> = registry
        .crates
        .iter()
        .map(|row| row.name.as_str())
        .collect();
    let mut by_order: Vec<&Layer> = registry.layers.iter().collect();
    by_order.sort_by_key(|layer| layer.source_order);
    for (index, parsed_row) in parsed.iter().enumerate() {
        // Coverage: every crate-shaped token the plan spells is registered.
        for token in &parsed_row.tokens {
            if !declared_names.contains(token.as_str()) {
                violations.push(Violation::new(
                    "crate_unregistered",
                    token,
                    "§18.1",
                    format!(
                        "the §18.1 {:?} row names this crate and the registry has no row for it",
                        parsed_row.title
                    ),
                ));
            }
        }
        let Some(layer) = by_order.get(index) else {
            continue;
        };
        if layer.title != parsed_row.title {
            violations.push(Violation::new(
                "layer_title_position_drift",
                &layer.id,
                "§18.1",
                format!(
                    "source_order {} is {:?} in the registry and {:?} in the frozen table",
                    layer.source_order, layer.title, parsed_row.title
                ),
            ));
            continue;
        }
        // Partition + order: the tokens belonging to THIS layer, in source
        // order, must equal the registry's layer_position sequence.
        let mine: Vec<&str> = parsed_row
            .tokens
            .iter()
            .filter(|token| {
                registry
                    .crate_row(token)
                    .map(|row| row.layer == layer.id)
                    .unwrap_or(false)
            })
            .map(String::as_str)
            .collect();
        let declared: Vec<&str> = registry
            .layer_crates(&layer.id)
            .into_iter()
            .map(|row| row.name.as_str())
            .collect();
        if mine != declared {
            violations.push(Violation::new(
                "layer_membership_drift",
                &layer.id,
                "§18.1",
                format!(
                    "the frozen table spells {mine:?} for this layer; the registry declares {declared:?} (order comes from the source, never from a sorted census)"
                ),
            ));
        }
    }
    // A `plan_parenthetical` role must be verbatim in the frozen table.
    for row in &registry.crates {
        if row.role_basis == "plan_parenthetical" && !text.contains(&row.role) {
            violations.push(Violation::new(
                "role_not_verbatim",
                &row.name,
                "§18.1",
                format!(
                    "role_basis is plan_parenthetical but {:?} does not occur in the frozen table",
                    row.role
                ),
            ));
        }
    }
}

/// Law 4 — inventory partition and residue coverage.
fn validate_inventory(registry: &TopologyRegistry, root: &Path, violations: &mut Vec<Violation>) {
    let Some(block) = registry.source_block("plan-build-inventory-v1") else {
        violations.push(Violation::new(
            "source_block_missing",
            "plan-build-inventory-v1",
            "§18.2",
            "coverage is proved against this block",
        ));
        return;
    };
    match source_block_text(block, root) {
        Ok(text) => {
            let coverage = decompose_inventory(text.trim_end_matches('\n'), registry);
            for (phrase, count) in &coverage.unresolved {
                violations.push(Violation::new(
                    "capability_phrase_unresolved",
                    phrase,
                    "§18.2",
                    format!(
                        "occurs {count} times in the frozen §18.2 line at removal time; a decomposition is only a proof when every phrase occurs exactly once"
                    ),
                ));
            }
            if !coverage.illegal_residue.is_empty() {
                violations.push(Violation::new(
                    "inventory_coverage_incomplete",
                    "plan-build-inventory-v1",
                    "§18.2",
                    format!(
                        "residue {:?} is outside the registered alphabet {:?}; §18.2 names something this registry does not: {}",
                        coverage.illegal_residue,
                        RESIDUE_ALPHABET,
                        coverage.residue.trim()
                    ),
                ));
            }
        }
        Err(message) => violations.push(Violation::new(
            "source_block_unreadable",
            &block.id,
            "§18.2",
            message,
        )),
    }
    match registry.source_block("plan-frankensqlite-donor-v1") {
        Some(block) => match source_block_text(block, root) {
            Ok(text) => violations.extend(design_bijection(&text, registry)),
            Err(message) => violations.push(Violation::new(
                "source_block_unreadable",
                &block.id,
                "§2.3",
                message,
            )),
        },
        None => violations.push(Violation::new(
            "source_block_missing",
            "plan-frankensqlite-donor-v1",
            "§2.3",
            "the design bijection is proved against this block",
        )),
    }
    check_foundation_assets(registry, root, violations);
    // Ownership vocabulary: a workstream title must be verbatim in §19.
    if let Some(block) = registry.source_block("plan-workstream-table-v1")
        && let Ok(text) = source_block_text(block, root)
    {
        for scope in &registry.owner_scopes {
            if scope.kind == "workstream"
                && !text.contains(&format!("| {} | {} |", scope.id, scope.title))
            {
                violations.push(Violation::new(
                    "workstream_title_drift",
                    &scope.id,
                    "§19",
                    format!(
                        "the frozen workstream table does not spell `| {} | {} |`",
                        scope.id, scope.title
                    ),
                ));
            }
        }
    }
}

/// The island roster is declared twice — here as the `unsafe_islands` layer,
/// and in the unsafe-boundary ledger as a positive roster with its own status.
/// Two registries that both name the islands must not be able to disagree, so
/// the two rosters are checked as a bijection with agreeing statuses.
///
/// Ownership stays split: the ledger owns site rows and the site<->allow
/// bijection; this registry owns the crate-level policy column. Neither
/// restates the other, and neither can drift from it.
pub fn check_island_roster(
    registry: &TopologyRegistry,
    root: &Path,
    violations: &mut Vec<Violation>,
) {
    let path = root.join(&registry.registry.unsafe_ledger_registry);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            // Unreadable is a failure, never a skip: a roster nobody can read
            // agrees with everything.
            violations.push(Violation::new(
                "island_roster_unreadable",
                &registry.registry.unsafe_ledger_registry,
                "§1 constraint 2",
                format!("{}: {error}", path.display()),
            ));
            return;
        }
    };
    let table = match parse(&text) {
        Ok(table) => table,
        Err(error) => {
            violations.push(Violation::new(
                "island_roster_unparsable",
                &registry.registry.unsafe_ledger_registry,
                "§1 constraint 2",
                error.to_string(),
            ));
            return;
        }
    };
    let islands = match get_table_array(&table, "island", "unsafe_boundary_ledger.toml") {
        Ok(islands) => islands,
        Err(error) => {
            violations.push(Violation::new(
                "island_roster_unparsable",
                &registry.registry.unsafe_ledger_registry,
                "§1 constraint 2",
                error.to_string(),
            ));
            return;
        }
    };
    let mut rostered: BTreeMap<String, String> = BTreeMap::new();
    for island in islands {
        let name = get_str(island, "name", "unsafe_boundary_ledger.toml.island");
        let status = get_str(island, "status", "unsafe_boundary_ledger.toml.island");
        match (name, status) {
            (Ok(name), Ok(status)) => {
                rostered.insert(name, status);
            }
            _ => violations.push(Violation::new(
                "island_roster_unparsable",
                &registry.registry.unsafe_ledger_registry,
                "§1 constraint 2",
                "an [[island]] row is missing name or status",
            )),
        }
    }
    let declared: BTreeSet<&str> = registry
        .layer_crates("unsafe_islands")
        .into_iter()
        .map(|row| row.name.as_str())
        .collect();
    for name in rostered.keys() {
        if !declared.contains(name.as_str()) {
            violations.push(Violation::new(
                "island_roster_extra",
                name,
                "§18.1",
                "the unsafe-boundary ledger rosters an island this map does not place in the unsafe_islands layer",
            ));
        }
    }
    for name in &declared {
        let Some(status) = rostered.get(*name) else {
            violations.push(Violation::new(
                "island_roster_missing",
                *name,
                "§1 constraint 2",
                "an island of the unsafe_islands layer is absent from the unsafe-boundary ledger roster",
            ));
            continue;
        };
        let Some(row) = registry.crate_row(name) else {
            continue;
        };
        let expected = if row.activation_status == "active" {
            "present"
        } else {
            "planned"
        };
        if status != expected {
            violations.push(Violation::new(
                "island_status_disagreement",
                *name,
                "§1 constraint 2",
                format!(
                    "this map says activation_status={:?} (ledger status {expected:?}) and the ledger says {status:?}",
                    row.activation_status
                ),
            ));
        }
    }
}

/// Laws 2, 3 and 5 — the live tree.
fn validate_live_tree(registry: &TopologyRegistry, root: &Path, violations: &mut Vec<Violation>) {
    let scan = match scan_workspace(root) {
        Ok(scan) => scan,
        Err(message) => {
            violations.push(Violation::new(
                "workspace_scan_failed",
                "Cargo.toml",
                "§18.1",
                message,
            ));
            return;
        }
    };
    if scan.workspace_unsafe_lint != registry.registry.workspace_unsafe_lint {
        violations.push(Violation::new(
            "workspace_unsafe_lint_drift",
            "Cargo.toml",
            "§1 constraint 2",
            format!(
                "[workspace.lints.rust] unsafe_code is {:?}, the registry requires {:?}",
                scan.workspace_unsafe_lint, registry.registry.workspace_unsafe_lint
            ),
        ));
    }
    if scan.toolchain_channel != registry.registry.toolchain_channel {
        violations.push(Violation::new(
            "toolchain_channel_drift",
            "rust-toolchain.toml",
            "§18 toolchain",
            format!(
                "channel is {:?}, the registry pins {:?}",
                scan.toolchain_channel, registry.registry.toolchain_channel
            ),
        ));
    }

    // --- Law 2: activation bijection ---
    let tooling: BTreeSet<&str> = registry
        .registry
        .tooling_members
        .iter()
        .map(String::as_str)
        .collect();
    let mut active_dirs: BTreeMap<&str, &CrateRow> = BTreeMap::new();
    for row in registry.active_crates() {
        active_dirs.insert(row.manifest_dir.as_str(), row);
    }
    for member in &scan.members {
        if tooling.contains(member.as_str()) {
            continue;
        }
        if !active_dirs.contains_key(member.as_str()) {
            violations.push(Violation::new(
                "member_unregistered",
                member,
                "§18.1",
                "a workspace member is either an active crate row or a registered tooling member",
            ));
        }
    }
    for (dir, row) in &active_dirs {
        if !scan.members.iter().any(|member| member == dir) {
            violations.push(Violation::new(
                "active_not_a_member",
                &row.name,
                "§18.1",
                format!("active row declares {dir:?}, which is not a workspace member"),
            ));
        }
        match scan.by_dir(dir) {
            None => violations.push(Violation::new(
                "active_manifest_missing",
                &row.name,
                "§18.1",
                format!("no manifest at {dir}/Cargo.toml"),
            )),
            Some(scanned) => {
                if scanned.package_name != row.name {
                    violations.push(Violation::new(
                        "package_name_drift",
                        &row.name,
                        "§18.1",
                        format!(
                            "{dir}/Cargo.toml declares package {:?}",
                            scanned.package_name
                        ),
                    ));
                }
                // Inheritance is decided by the policy column, and BOTH
                // directions are errors. An ordinary crate that omits the
                // table escapes the workspace forbid invisibly. An island that
                // carries it inherits `forbid`, which cannot be lowered, so
                // every one of its ledgered sites stops compiling — the island
                // would be a boundary crate that can hold no boundary.
                if row.unsafe_policy == "deny_ledgered" {
                    if scanned.lints_workspace {
                        violations.push(Violation::new(
                            "island_inherits_forbid",
                            &row.name,
                            "§1 constraint 2",
                            "an unsafe island must NOT carry [lints] workspace = true: it would inherit unsafe_code = \"forbid\", forbid cannot be lowered, and no ledgered allow site could compile",
                        ));
                    }
                } else if !scanned.lints_workspace {
                    violations.push(Violation::new(
                        "lints_not_inherited",
                        &row.name,
                        "§1 constraint 2",
                        "an active crate inherits the workspace lints ([lints] workspace = true), or the forbid default does not reach it",
                    ));
                }
            }
        }
    }
    // A planned or reserved crate must not exist on disk yet.
    for row in &registry.crates {
        if row.activation_status == "active" {
            continue;
        }
        let candidate = root.join("crates").join(&row.name);
        if candidate.exists() {
            violations.push(Violation::new(
                "phantom_crate_directory",
                &row.name,
                "§18.1",
                format!(
                    "activation_status is {:?} but crates/{} exists; a crate appears only with its first real final-abstraction slice, never as an empty prototype",
                    row.activation_status, row.name
                ),
            ));
        }
    }
    for member in &scan.members {
        if tooling.contains(member.as_str()) {
            continue;
        }
        if let Some(scanned) = scan.by_dir(member)
            && registry.crate_row(&scanned.package_name).is_none()
        {
            violations.push(Violation::new(
                "crate_undeclared",
                &scanned.package_name,
                "§18.1",
                "the workspace holds a crate with no registry row; the map changes with the bead that needs it, not after the fact",
            ));
        }
    }

    // --- Law 5: unsafe policy against the live roots ---
    for row in registry.active_crates() {
        let Some(scanned) = scan.by_dir(&row.manifest_dir) else {
            continue;
        };
        if row.unsafe_policy == "forbid" {
            if !scanned.root_forbids_unsafe {
                violations.push(Violation::new(
                    "root_missing_forbid",
                    &row.name,
                    "§1 constraint 2",
                    format!(
                        "{}/{} does not carry #![forbid(unsafe_code)]",
                        row.manifest_dir, scanned.root_path
                    ),
                ));
            }
            if scanned.relaxes_unsafe {
                violations.push(Violation::new(
                    "ordinary_crate_relaxes_unsafe",
                    &row.name,
                    "§1 constraint 2",
                    "an allow/expect(unsafe_code) attribute appears in an ordinary crate; forbid cannot be lowered and only the three named islands may hold ledgered allows",
                ));
            }
        } else if !scanned.root_denies_unsafe {
            // The island half of Law 5, and it is not symmetric with the
            // ordinary half. An island's manifest deliberately omits the
            // workspace lint table, so if its root also loses
            // `#![deny(unsafe_code)]` NOTHING constrains unsafe in that crate
            // and every gate still passes: the ledger checker would keep
            // matching the sites that remain, and this map would keep
            // reporting deny_ledgered. The policy column is a claim about the
            // root, so the root is read.
            violations.push(Violation::new(
                "island_root_missing_deny",
                &row.name,
                "§1 constraint 2",
                format!(
                    "{}/{} does not carry #![deny(unsafe_code)]; an island that neither inherits forbid nor denies at its root is unconstrained",
                    row.manifest_dir, scanned.root_path
                ),
            ));
        }
    }

    // --- Law 3: closed universe, layer direction, narrowing ---
    let narrowings: BTreeMap<&str, &DependencyNarrowing> = registry
        .dependency_narrowings
        .iter()
        .map(|row| (row.crate_name.as_str(), row))
        .collect();
    for row in registry.active_crates() {
        let Some(scanned) = scan.by_dir(&row.manifest_dir) else {
            continue;
        };
        let Some(layer) = registry.layer(&row.layer) else {
            continue;
        };
        for dependency in &scanned.dependencies {
            let subject = format!("{} -> {}", row.name, dependency.package);
            if let Some(forbidden) = registry
                .forbidden_dependencies
                .iter()
                .find(|rule| dependency.package.starts_with(&rule.package_prefix))
            {
                violations.push(Violation::new(
                    "forbidden_dependency",
                    &subject,
                    &forbidden.plan_anchor,
                    format!("{} forbids it: {}", forbidden.id, forbidden.reason),
                ));
                continue;
            }
            if let Some(target) = registry.crate_row(&dependency.package) {
                if dependency.path.is_empty() {
                    violations.push(Violation::new(
                        "internal_dependency_not_path",
                        &subject,
                        "§18.1",
                        "a workspace crate is depended on by path, never by version or git",
                    ));
                }
                if target.activation_status != "active" {
                    violations.push(Violation::new(
                        "dependency_on_inactive_crate",
                        &subject,
                        "§18.1",
                        format!("{} is {}", target.name, target.activation_status),
                    ));
                }
                if !layer.allowed_outgoing_layers.contains(&target.layer) {
                    violations.push(Violation::new(
                        "layer_inversion",
                        &subject,
                        "§18.1",
                        format!(
                            "{} is in layer {} and {} may only depend on {:?}",
                            target.name, target.layer, layer.id, layer.allowed_outgoing_layers
                        ),
                    ));
                }
                if let Some(narrowing) = narrowings.get(row.name.as_str()) {
                    let allowed = narrowing.allowed_layers.contains(&target.layer)
                        || narrowing.allowed_crates.contains(&target.name);
                    if !allowed {
                        violations.push(Violation::new(
                            "narrowing_violated",
                            &subject,
                            &narrowing.plan_anchor,
                            format!(
                                "{} is narrowed to layers {:?} plus crates {:?}",
                                row.name, narrowing.allowed_layers, narrowing.allowed_crates
                            ),
                        ));
                    }
                }
                continue;
            }
            let project = registry.foundation_projects.iter().find(|project| {
                project
                    .package_prefixes
                    .iter()
                    .any(|prefix| dependency.package.starts_with(prefix))
            });
            match project {
                None => violations.push(Violation::new(
                    "external_dependency",
                    &subject,
                    "§1 constraint 1",
                    "the dependency universe is closed: core/alloc/std, the pinned nightly, asupersync, and the fnx-* crates. Everything else is built in-house.",
                )),
                Some(project) => {
                    if project.linkage == "design_only" {
                        violations.push(Violation::new(
                            "design_only_linked",
                            &subject,
                            &project.plan_anchor,
                            format!("{} is a design donor; its packages are never linked", project.id),
                        ));
                    }
                    if dependency.git != project.git_url {
                        violations.push(Violation::new(
                            "foundation_source_drift",
                            &subject,
                            &project.plan_anchor,
                            format!(
                                "git is {:?}, the registry pins {:?}",
                                dependency.git, project.git_url
                            ),
                        ));
                    }
                    if dependency.rev != project.pinned_rev {
                        violations.push(Violation::new(
                            "foundation_rev_drift",
                            &subject,
                            &project.plan_anchor,
                            format!(
                                "rev is {:?}, the registry pins {:?}; two revisions of one foundation is two versions of one capability",
                                dependency.rev, project.pinned_rev
                            ),
                        ));
                    }
                    if project.default_features_must_be_disabled
                        && !dependency.default_features_disabled
                    {
                        violations.push(Violation::new(
                            "default_feature_escape",
                            &subject,
                            &project.plan_anchor,
                            format!(
                                "{} requires default-features = false ({})",
                                project.id, project.default_features_basis
                            ),
                        ));
                    }
                    if let Some(narrowing) = narrowings.get(row.name.as_str())
                        && !narrowing.allowed_foundation_projects.contains(&project.id)
                    {
                        violations.push(Violation::new(
                            "narrowing_violated",
                            &subject,
                            &narrowing.plan_anchor,
                            format!(
                                "{} is narrowed to foundation projects {:?}",
                                row.name, narrowing.allowed_foundation_projects
                            ),
                        ));
                    }
                }
            }
        }
    }
    // Tooling members are std-only by the constraint they enforce.
    for member in &registry.registry.tooling_members {
        match scan.by_dir(member) {
            None => violations.push(Violation::new(
                "tooling_member_missing",
                member,
                "§18.1",
                "a registered tooling member must be a workspace member",
            )),
            Some(scanned) => {
                for dependency in &scanned.dependencies {
                    violations.push(Violation::new(
                        "tooling_dependency",
                        format!("{member} -> {}", dependency.package),
                        "§1 constraint 1",
                        "G0 constitutional tooling is std-only: the closed dependency universe applies to the tooling that enforces it",
                    ));
                }
            }
        }
    }

    // --- Crate-graph acyclicity, named edges, posture closures ---
    let cycle = crate_graph_cycle(&scan);
    if !cycle.is_empty() {
        violations.push(Violation::new(
            "crate_graph_cycle",
            cycle.join(", "),
            "§18.1",
            "the live crate graph is not acyclic; the one reciprocal LAYER pair is only sound while this holds",
        ));
    }
    for status in required_edge_statuses(registry, &scan) {
        if status.status == "fail" {
            let anchor = registry
                .required_dependencies
                .iter()
                .find(|edge| edge.id == status.id)
                .map(|edge| edge.plan_anchor.clone())
                .unwrap_or_default();
            violations.push(Violation::new(
                "required_edge_missing",
                &status.id,
                anchor,
                format!(
                    "both endpoints are active but the live graph has no such edge ({})",
                    status.detail
                ),
            ));
        }
    }
    for closure in posture_closures(registry, &scan) {
        for illegal in &closure.illegal {
            violations.push(Violation::new(
                "posture_closure_violation",
                format!("{} <- {illegal}", closure.posture_id),
                "§1 constraint 5",
                "a test-only, packaging-boundary, or foreign-entry crate appears in a shipped posture closure",
            ));
        }
    }
    for posture in &registry.postures {
        let live = scan.by_package(&posture.entry_crate).is_some();
        let declared_live = posture.status == "live";
        if live != declared_live {
            violations.push(Violation::new(
                "posture_status_drift",
                &posture.id,
                "§1 constraint 5",
                format!(
                    "status is {:?} but the entry crate {} is {}active in the workspace",
                    posture.status,
                    posture.entry_crate,
                    if live { "" } else { "not " }
                ),
            ));
        }
    }
}

pub fn validate_topology(registry: &TopologyRegistry, root: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    validate_header(registry, &mut violations);
    validate_layers(registry, &mut violations);
    validate_crates(registry, &mut violations);
    validate_derived_universe(registry, root, &mut violations);
    validate_inventory(registry, root, &mut violations);
    for (block, result) in registry
        .source_blocks
        .iter()
        .zip(check_source_blocks(registry, root))
    {
        match result {
            Ok(check) if check.outcome == "pass" => {}
            Ok(check) => violations.push(Violation::new(
                "source_block_drift",
                &check.id,
                &block.covers,
                format!(
                    "recomputed {} lines / {} bytes / {}",
                    check.line_count, check.byte_count, check.fnv1a64
                ),
            )),
            Err(message) => violations.push(Violation::new(
                "source_block_unreadable",
                &block.id,
                &block.covers,
                message,
            )),
        }
    }
    check_island_roster(registry, root, &mut violations);
    validate_live_tree(registry, root, &mut violations);
    violations
}

// -----------------------------------------------------------------------------
// Document generation
// -----------------------------------------------------------------------------

fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn dash(text: &str) -> String {
    if text.is_empty() {
        "—".to_string()
    } else {
        escape_cell(text)
    }
}

/// Generate the published topology document. Deterministic: the same registry
/// plus the same plan bytes produce the same document, byte for byte.
pub fn generate_document(registry: &TopologyRegistry, root: &Path) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("<!-- GENERATED FILE — DO NOT EDIT.\n");
    out.push_str("     Source: registries/workspace_topology.toml\n");
    out.push_str("     Regenerate: ");
    out.push_str(REPLAY_COMMAND);
    out.push_str(" --write\n");
    out.push_str("     Owner bead: fgdb-g0-workspace-topology-1q9m -->\n\n");
    out.push_str(
        "# Workspace Topology — Crates, Layers, and the Build-Versus-Consume Inventory\n\n",
    );
    out.push_str(
        "This document is generated from `registries/workspace_topology.toml` and checked \
byte-exact in CI. The registry is the master; this file is its rendering. Every plan excerpt \
below is embedded verbatim under an `fnv1a64` pin, so plan drift turns the gate red rather than \
silently invalidating the map.\n\n",
    );
    out.push_str(&format!(
        "* **Layers:** {}\n* **Crates:** {} ({} active, {} planned, {} reserved)\n* **Inventory rows:** {} ({} build-here, {} consume-from, {} design-only)\n* **Replay:** `{}`\n* **Constraints bound:** {}\n\n",
        registry.layers.len(),
        registry.crates.len(),
        registry.registry.active_crate_count,
        registry.registry.planned_crate_count,
        registry.registry.reserved_crate_count,
        registry.capabilities.len(),
        registry.registry.build_here_count,
        registry.registry.consume_from_count,
        registry.registry.design_only_count,
        registry.registry.replay_command,
        registry.registry.bound_constraints.join(", ")
    ));
    out.push_str("## What `activation_status` means\n\n");
    out.push_str(
        "`active` — the crate exists in the workspace with its first real final-abstraction \
slice. `planned` — the row is frozen and the directory **must not exist**; the checker fails on \
a planned crate with a directory just as it fails on a directory with no row. `reserved` — named \
by the plan as belonging to a later workstream only (`fgdb-shard`, W12).\n\n",
    );

    out.push_str("## Layers and legal dependency direction\n\n");
    out.push_str("| # | Layer | Id | May depend on | Charter |\n|---|---|---|---|---|\n");
    let mut by_order: Vec<&Layer> = registry.layers.iter().collect();
    by_order.sort_by_key(|layer| layer.source_order);
    for layer in &by_order {
        let mut allowed: Vec<String> = layer.allowed_outgoing_layers.clone();
        allowed.sort();
        out.push_str(&format!(
            "| {} | {} | `{}` | {} | {} |\n",
            layer.source_order,
            escape_cell(&layer.title),
            layer.id,
            allowed
                .iter()
                .map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", "),
            escape_cell(&layer.charter)
        ));
    }
    out.push_str(&format!(
        "\nThe allowed set is **derived**, not declared: `allowed(L) = {{ M : M.source_order <= L.source_order }} ∪ {{ unsafe_islands }}`, and the checker recomputes every row. The union term is the single registered exception to table order — {}\n\n",
        escape_cell(&registry.layer_law.reciprocal_reason)
    ));

    out.push_str("## The crate map\n\n");
    out.push_str(&format!(
        "Exactly three crates may carry `deny_ledgered`; every other row carries `forbid`, and every ACTIVE forbid-crate root is scanned for the attribute and for any attempt to lower it. The same three islands are rostered in [`{}`]({}), which owns the site-level ledger and the site↔allow bijection; this map owns the crate-level policy column. The two rosters are checked as a bijection with agreeing statuses, so neither registry can drift from the other.\n\n",
        registry.registry.unsafe_ledger_registry, registry.registry.unsafe_ledger_registry
    ));
    for layer in &by_order {
        let rows = registry.layer_crates(&layer.id);
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "### {}. {}\n\n| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |\n|---|---|---|---|---|---|---|---|\n",
            layer.source_order,
            escape_cell(&layer.title)
        ));
        for row in rows {
            out.push_str(&format!(
                "| {} | `{}` | {} | `{}` | {} | {} | {} | {} |\n",
                row.layer_position,
                row.name,
                row.activation_status,
                row.unsafe_policy,
                row.posture_participation,
                row.owner,
                dash(&row.owner_bead),
                escape_cell(&row.role)
            ));
        }
        out.push('\n');
    }

    out.push_str("## The three postures\n\n");
    out.push_str("| Posture | Entry crate | Binary | Status | Deferred to | Anchor |\n|---|---|---|---|---|---|\n");
    for posture in &registry.postures {
        out.push_str(&format!(
            "| {} | `{}` | {} | {} | {} | {} |\n",
            escape_cell(&posture.title),
            posture.entry_crate,
            dash(&posture.binary_name),
            posture.status,
            dash(&posture.deferred_to),
            escape_cell(&posture.plan_anchor)
        ));
    }
    out.push_str(
        "\nA posture closure is the transitive dependency set of its entry crate over the LIVE \
graph. A `test_only`, `packaging_boundary`, or foreign-entry crate inside a shipped closure is a \
violation. While every entry crate is `planned` the law reports `deferred` — never `pass` — and \
the closure evaluator is proved against synthetic graphs in the suite instead.\n\n",
    );

    out.push_str("## The legal external universe\n\n");
    out.push_str("| Project | Linkage | Pinned revision | Package prefixes | Default features | Anchor |\n|---|---|---|---|---|---|\n");
    for project in &registry.foundation_projects {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            escape_cell(&project.title),
            project.linkage,
            if project.pinned_rev.is_empty() {
                "—".to_string()
            } else {
                format!("`{}`", project.pinned_rev)
            },
            project
                .package_prefixes
                .iter()
                .map(|prefix| format!("`{prefix}`"))
                .collect::<Vec<_>>()
                .join(", "),
            if project.default_features_must_be_disabled {
                format!(
                    "must be disabled ({})",
                    escape_cell(&project.default_features_basis)
                )
            } else {
                format!(
                    "permitted ({})",
                    escape_cell(&project.default_features_basis)
                )
            },
            escape_cell(&project.plan_anchor)
        ));
    }
    out.push('\n');
    out.push_str("| Forbidden dependency | Selector | Prefix | Why |\n|---|---|---|---|\n");
    for row in &registry.forbidden_dependencies {
        out.push_str(&format!(
            "| `{}` | {} | `{}` | {} |\n",
            row.id,
            row.selector,
            row.package_prefix,
            escape_cell(&row.reason)
        ));
    }
    out.push('\n');

    out.push_str("## Named crate-level edges\n\n");
    out.push_str("| Edge | From | To | Source phrase | Why |\n|---|---|---|---|---|\n");
    for edge in &registry.required_dependencies {
        out.push_str(&format!(
            "| `{}` | {} `{}` | {} `{}` | {} | {} |\n",
            edge.id,
            edge.from_kind,
            edge.from,
            edge.to_kind,
            edge.to,
            escape_cell(&edge.source_marker),
            escape_cell(&edge.note)
        ));
    }
    out.push('\n');
    for row in &registry.dependency_narrowings {
        out.push_str(&format!(
            "**Narrowing — `{}`.** Layers {:?}, plus crates {:?}, plus foundation projects {:?} ({}). {}\n\n",
            row.crate_name,
            row.allowed_layers,
            row.allowed_crates,
            row.allowed_foundation_projects,
            escape_cell(&row.plan_anchor),
            escape_cell(&row.reason)
        ));
    }

    out.push_str("## Build here, or consume from a foundation\n\n");
    out.push_str(
        "Coverage of §18.2 is **proved by residue**: every phrase below is deleted from the \
frozen source line, and what remains must be punctuation plus the registered rationale \
allowances. A capability §18.2 names and this registry drops fails as leftover residue.\n\n",
    );
    out.push_str("### Built here\n\n| Capability | Owning crate | Note |\n|---|---|---|\n");
    for capability in registry
        .capabilities
        .iter()
        .filter(|row| row.disposition == "build_here")
    {
        out.push_str(&format!(
            "| {} | `{}` | {} |\n",
            escape_cell(&capability.source_phrase),
            capability.owner_crate,
            dash(&capability.note)
        ));
    }
    out.push_str("\n### Consumed from a foundation\n\n| Capability | Project | Asset (exact evidence) | Note |\n|---|---|---|---|\n");
    for capability in registry
        .capabilities
        .iter()
        .filter(|row| row.disposition == "consume_from")
    {
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            escape_cell(&capability.source_phrase),
            capability.foundation_project,
            escape_cell(&capability.foundation_asset),
            dash(&capability.note)
        ));
    }
    if !registry.asset_evidence_gaps.is_empty() {
        out.push_str("\n**Registered asset-evidence gaps.** A consumed capability normally names exactly one asset row of §2.1/§2.2. Where §18.2 attributes a capability the asset tables never enumerate, the absence is registered here and the checker verifies it: a gap whose asset row exists is itself a violation.\n\n");
        out.push_str("| Capability | Absent from | Finding |\n|---|---|---|\n");
        for gap in &registry.asset_evidence_gaps {
            out.push_str(&format!(
                "| `{}` | `{}` | {} |\n",
                gap.capability_id,
                gap.verified_absent_from,
                escape_cell(&gap.reason)
            ));
        }
    }
    out.push_str("\n### Adopted as design only (never linked)\n\n| frankensqlite design | Re-instantiated in | Note |\n|---|---|---|\n");
    for capability in registry
        .capabilities
        .iter()
        .filter(|row| row.disposition == "design_only")
    {
        out.push_str(&format!(
            "| {} | `{}` | {} |\n",
            escape_cell(&capability.source_phrase),
            capability.owner_crate,
            dash(&capability.note)
        ));
    }
    out.push('\n');
    out.push_str("| Residue allowance | Text | Why it is not a capability |\n|---|---|---|\n");
    for allowance in &registry.residue_allowances {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            allowance.id,
            escape_cell(&allowance.text),
            escape_cell(&allowance.reason)
        ));
    }
    out.push('\n');

    out.push_str("## Ownership vocabulary\n\n");
    out.push_str("| Id | Kind | Title | Crates | Anchor |\n|---|---|---|---|---|\n");
    for scope in &registry.owner_scopes {
        let count = registry
            .crates
            .iter()
            .filter(|row| row.owner == scope.id)
            .count();
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            scope.id,
            scope.kind,
            escape_cell(&scope.title),
            count,
            escape_cell(&scope.plan_anchor)
        ));
    }
    out.push('\n');

    out.push_str("## Checked plan sources\n\n");
    out.push_str("| Block | Plan lines | Lines | Bytes | fnv1a64 | Embedded | Covers |\n|---|---|---|---|---|---|---|\n");
    let mut blocks: Vec<&SourceBlock> = registry.source_blocks.iter().collect();
    blocks.sort_by_key(|block| block.plan_start_line);
    for block in &blocks {
        out.push_str(&format!(
            "| `{}` | {}–{} | {} | {} | `{}` | {} | {} |\n",
            block.id,
            block.plan_start_line,
            block.plan_end_line,
            block.line_count,
            block.byte_count,
            block.fnv1a64,
            if registry.registry.embedded_source_blocks.contains(&block.id) {
                "yes"
            } else {
                "pin only"
            },
            escape_cell(&block.covers)
        ));
    }
    out.push('\n');
    for id in &registry.registry.embedded_source_blocks {
        let Some(block) = registry.source_block(id) else {
            return Err(format!("embedded_source_blocks names unknown block {id:?}"));
        };
        let text = source_block_text(block, root)?;
        out.push_str(&format!("### {} — {}\n\n", block.id, block.covers));
        out.push_str(&format!("<!-- BEGIN {} -->\n", block.id));
        out.push_str(&text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("<!-- END {} -->\n\n", block.id));
    }

    out.push_str("## Pins\n\n");
    out.push_str(&format!(
        "* `id_table_hash` = `{}` — every stable id, sorted.\n* `semantic_contract_hash` = `{}` — every normative decision, prose excluded.\n",
        registry.registry.id_table_hash, registry.registry.semantic_contract_hash
    ));
    Ok(out)
}

pub fn document_digest(text: &str) -> String {
    format!("fnv1a64:{:016x}", fnv1a64(text.as_bytes()))
}

/// Compare the generated document against the committed one.
pub fn check_document(registry: &TopologyRegistry, root: &Path) -> Result<bool, String> {
    let generated = generate_document(registry, root)?;
    let path = root.join(&registry.registry.document_path);
    let committed = fs::read_to_string(&path).unwrap_or_default();
    Ok(committed == generated)
}
