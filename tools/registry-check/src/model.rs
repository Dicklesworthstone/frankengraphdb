//! Typed model of the G0 claim registries, constructed from parsed TOML.
//!
//! Construction is strict: a missing or mistyped field is a typed
//! `ReadError`, and unknown claim classes / statuses are surfaced later by
//! the validator (so a single run reports every violation, not just the
//! first). The model layer only fails on *structural* impossibilities.

use crate::toml::{
    self, ReadError, Table, get_int, get_opt_str, get_opt_str_array, get_str, get_str_array,
    get_table, get_table_array,
};
use std::path::Path;

/// The six claim classes with their strength ranks (constitution.toml).
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimClass {
    pub name: String,
    pub rank: i64,
    pub definition: String,
    pub carriers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub id: String,
    pub title: String,
    pub statement: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bet {
    pub id: String,
    pub name: String,
    pub codename: String,
    pub statement: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Constitution {
    pub claim_classes: Vec<ClaimClass>,
    pub lattice_law: String,
    pub lattice_edge_rule: String,
    pub constraints: Vec<Constraint>,
    pub bets: Vec<Bet>,
}

/// A subordinate clause record under one of the twenty invariant IDs.
#[derive(Debug, Clone, PartialEq)]
pub struct Clause {
    pub key: String,
    pub claim_class: String,
    pub exact_statement: String,
    pub activation_predicate: String,
    pub dependencies: Vec<String>,
    pub checker_entrypoint: String,
    pub negative_test_entrypoint: String,
    pub model_or_proof_scope: String,
    pub proof_lane: Option<String>,
    pub owner: String,
    pub first_gate: String,
    pub status: String,
    pub waiver: String,
    pub justified_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Invariant {
    pub id: String,
    pub title: String,
    pub clauses: Vec<Clause>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InvariantRegistry {
    pub allowed_claim_classes: Vec<String>,
    pub waiver_policy: String,
    pub twenty_id_hash: String,
    pub invariants: Vec<Invariant>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceRow {
    pub id: String,
    pub claim_class: String,
    pub qualified_claim: String,
    pub required_disclosures: Vec<String>,
    pub binds_to: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceRegistry {
    pub allowed_claim_classes: Vec<String>,
    pub rows: Vec<EvidenceRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SloRow {
    pub id: String,
    pub claim_class: String,
    pub kind: Option<String>,
    pub qualified_claim: String,
    pub required_disclosures: Vec<String>,
    pub operation_class: Option<String>,
    pub posture: Option<String>,
    pub audit_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SloRegistry {
    pub allowed_claim_classes: Vec<String>,
    pub rows: Vec<SloRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub id: String,
    pub lane: String,
    pub model_scope: String,
    pub artifact: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Checker {
    pub symbol: String,
    pub kind: String,
    pub artifact: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub features: Vec<String>,
    pub postures: Vec<String>,
    pub roles: Vec<String>,
}

/// The complete registry set the validator operates on.
#[derive(Debug, Clone, PartialEq)]
pub struct Registries {
    pub constitution: Constitution,
    pub invariants: InvariantRegistry,
    pub evidence: EvidenceRegistry,
    pub slo: SloRegistry,
    pub proof_lanes: Vec<Lane>,
    pub checker_index: Vec<Checker>,
}

fn registry_name(root: &Table, expected: &str, file: &str) -> Result<(), ReadError> {
    let registry = get_table(root, "registry", file)?;
    let name = get_str(registry, "name", &format!("{file}.registry"))?;
    if name != expected {
        return Err(ReadError {
            path: format!("{file}.registry.name"),
            msg: format!("expected {expected:?}, found {name:?}"),
        });
    }
    Ok(())
}

pub fn constitution_from(root: &Table) -> Result<Constitution, ReadError> {
    registry_name(root, "constitution", "constitution.toml")?;
    let mut claim_classes = Vec::new();
    for (i, t) in get_table_array(root, "claim_class", "constitution.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("constitution.toml.claim_class[{i}]");
        claim_classes.push(ClaimClass {
            name: get_str(t, "name", &ctx)?,
            rank: get_int(t, "rank", &ctx)?,
            definition: get_str(t, "definition", &ctx)?,
            carriers: get_str_array(t, "carriers", &ctx)?,
        });
    }
    let lattice = get_table(root, "lattice", "constitution.toml")?;
    let mut constraints = Vec::new();
    for (i, t) in get_table_array(root, "constraint", "constitution.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("constitution.toml.constraint[{i}]");
        constraints.push(Constraint {
            id: get_str(t, "id", &ctx)?,
            title: get_str(t, "title", &ctx)?,
            statement: get_str(t, "statement", &ctx)?,
        });
    }
    let mut bets = Vec::new();
    for (i, t) in get_table_array(root, "bet", "constitution.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("constitution.toml.bet[{i}]");
        bets.push(Bet {
            id: get_str(t, "id", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            codename: get_str(t, "codename", &ctx)?,
            statement: get_str(t, "statement", &ctx)?,
        });
    }
    Ok(Constitution {
        claim_classes,
        lattice_law: get_str(lattice, "law", "constitution.toml.lattice")?,
        lattice_edge_rule: get_str(lattice, "edge_rule", "constitution.toml.lattice")?,
        constraints,
        bets,
    })
}

fn clause_from(t: &Table, ctx: &str) -> Result<Clause, ReadError> {
    Ok(Clause {
        key: get_str(t, "key", ctx)?,
        claim_class: get_str(t, "claim_class", ctx)?,
        exact_statement: get_str(t, "exact_statement", ctx)?,
        activation_predicate: get_str(t, "activation_predicate", ctx)?,
        dependencies: get_opt_str_array(t, "dependencies", ctx)?.unwrap_or_default(),
        checker_entrypoint: get_str(t, "checker_entrypoint", ctx)?,
        negative_test_entrypoint: get_str(t, "negative_test_entrypoint", ctx)?,
        model_or_proof_scope: get_str(t, "model_or_proof_scope", ctx)?,
        proof_lane: get_opt_str(t, "proof_lane", ctx)?,
        owner: get_str(t, "owner", ctx)?,
        first_gate: get_str(t, "first_gate", ctx)?,
        status: get_str(t, "status", ctx)?,
        waiver: get_str(t, "waiver", ctx)?,
        justified_by: get_opt_str_array(t, "justified_by", ctx)?.unwrap_or_default(),
    })
}

pub fn invariants_from(root: &Table) -> Result<InvariantRegistry, ReadError> {
    registry_name(root, "invariants", "invariants.toml")?;
    let registry = get_table(root, "registry", "invariants.toml")?;
    let mut invariants = Vec::new();
    for (i, t) in get_table_array(root, "invariant", "invariants.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("invariants.toml.invariant[{i}]");
        let id = get_str(t, "id", &ctx)?;
        let title = get_str(t, "title", &ctx)?;
        let mut clauses = Vec::new();
        for (j, ct) in get_table_array(t, "clause", &ctx)?.iter().enumerate() {
            clauses.push(clause_from(ct, &format!("{ctx}.clause[{j}]"))?);
        }
        invariants.push(Invariant { id, title, clauses });
    }
    Ok(InvariantRegistry {
        allowed_claim_classes: get_str_array(
            registry,
            "allowed_claim_classes",
            "invariants.toml.registry",
        )?,
        waiver_policy: get_str(registry, "waiver_policy", "invariants.toml.registry")?,
        twenty_id_hash: get_str(registry, "twenty_id_hash", "invariants.toml.registry")?,
        invariants,
    })
}

pub fn evidence_from(root: &Table) -> Result<EvidenceRegistry, ReadError> {
    registry_name(root, "evidence", "evidence.toml")?;
    let registry = get_table(root, "registry", "evidence.toml")?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "evidence", "evidence.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("evidence.toml.evidence[{i}]");
        rows.push(EvidenceRow {
            id: get_str(t, "id", &ctx)?,
            claim_class: get_str(t, "claim_class", &ctx)?,
            qualified_claim: get_str(t, "qualified_claim", &ctx)?,
            required_disclosures: get_str_array(t, "required_disclosures", &ctx)?,
            binds_to: get_opt_str_array(t, "binds_to", &ctx)?.unwrap_or_default(),
        });
    }
    Ok(EvidenceRegistry {
        allowed_claim_classes: get_str_array(
            registry,
            "allowed_claim_classes",
            "evidence.toml.registry",
        )?,
        rows,
    })
}

pub fn slo_from(root: &Table) -> Result<SloRegistry, ReadError> {
    registry_name(root, "slo", "slo.toml")?;
    let registry = get_table(root, "registry", "slo.toml")?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "slo", "slo.toml")?.iter().enumerate() {
        let ctx = format!("slo.toml.slo[{i}]");
        rows.push(SloRow {
            id: get_str(t, "id", &ctx)?,
            claim_class: get_str(t, "claim_class", &ctx)?,
            kind: get_opt_str(t, "kind", &ctx)?,
            qualified_claim: get_str(t, "qualified_claim", &ctx)?,
            required_disclosures: get_str_array(t, "required_disclosures", &ctx)?,
            operation_class: get_opt_str(t, "operation_class", &ctx)?,
            posture: get_opt_str(t, "posture", &ctx)?,
            audit_class: get_opt_str(t, "audit_class", &ctx)?,
        });
    }
    Ok(SloRegistry {
        allowed_claim_classes: get_str_array(
            registry,
            "allowed_claim_classes",
            "slo.toml.registry",
        )?,
        rows,
    })
}

pub fn proof_lanes_from(root: &Table) -> Result<Vec<Lane>, ReadError> {
    registry_name(root, "proof_lanes", "proof_lanes.toml")?;
    let mut lanes = Vec::new();
    for (i, t) in get_table_array(root, "lane", "proof_lanes.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("proof_lanes.toml.lane[{i}]");
        lanes.push(Lane {
            id: get_str(t, "id", &ctx)?,
            lane: get_str(t, "lane", &ctx)?,
            model_scope: get_str(t, "model_scope", &ctx)?,
            artifact: get_str(t, "artifact", &ctx)?,
            status: get_str(t, "status", &ctx)?,
        });
    }
    Ok(lanes)
}

pub fn checker_index_from(root: &Table) -> Result<Vec<Checker>, ReadError> {
    registry_name(root, "checker_index", "checker_index.toml")?;
    let mut checkers = Vec::new();
    for (i, t) in get_table_array(root, "checker", "checker_index.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("checker_index.toml.checker[{i}]");
        checkers.push(Checker {
            symbol: get_str(t, "symbol", &ctx)?,
            kind: get_str(t, "kind", &ctx)?,
            artifact: get_str(t, "artifact", &ctx)?,
            status: get_str(t, "status", &ctx)?,
        });
    }
    Ok(checkers)
}

pub fn manifest_from(root: &Table) -> Result<Manifest, ReadError> {
    let m = get_table(root, "manifest", "manifest")?;
    Ok(Manifest {
        name: get_str(m, "name", "manifest")?,
        features: get_str_array(m, "features", "manifest")?,
        postures: get_str_array(m, "postures", "manifest")?,
        roles: get_str_array(m, "roles", "manifest")?,
    })
}

/// A load failure: which file, and the parse/read error text.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadError {
    pub file: String,
    pub msg: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file, self.msg)
    }
}

impl std::error::Error for LoadError {}

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

/// Load all six registries from a `registries/` directory.
pub fn load_registries(dir: &Path) -> Result<Registries, LoadError> {
    let wrap = |file: &str, e: ReadError| LoadError {
        file: dir.join(file).display().to_string(),
        msg: e.to_string(),
    };
    Ok(Registries {
        constitution: constitution_from(&load_table(dir, "constitution.toml")?)
            .map_err(|e| wrap("constitution.toml", e))?,
        invariants: invariants_from(&load_table(dir, "invariants.toml")?)
            .map_err(|e| wrap("invariants.toml", e))?,
        evidence: evidence_from(&load_table(dir, "evidence.toml")?)
            .map_err(|e| wrap("evidence.toml", e))?,
        slo: slo_from(&load_table(dir, "slo.toml")?).map_err(|e| wrap("slo.toml", e))?,
        proof_lanes: proof_lanes_from(&load_table(dir, "proof_lanes.toml")?)
            .map_err(|e| wrap("proof_lanes.toml", e))?,
        checker_index: checker_index_from(&load_table(dir, "checker_index.toml")?)
            .map_err(|e| wrap("checker_index.toml", e))?,
    })
}

/// Load a capability manifest from a TOML file.
pub fn load_manifest(path: &Path) -> Result<Manifest, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|e| LoadError {
        file: path.display().to_string(),
        msg: format!("cannot read: {e}"),
    })?;
    let table = toml::parse(&text).map_err(|e| LoadError {
        file: path.display().to_string(),
        msg: e.to_string(),
    })?;
    manifest_from(&table).map_err(|e| LoadError {
        file: path.display().to_string(),
        msg: e.to_string(),
    })
}
