//! Cross-registry validation: the claim constitution's CI teeth.
//!
//! One run reports *every* violation (never first-failure-only), each with a
//! stable code so negative fixtures can assert the exact defect class:
//!
//!   class_not_allowed        — claim class illegal for its carrier registry
//!   waiver_present           — clause waiver is anything but "forbidden"
//!   missing_checker          — checker/negative-test symbol not in checker_index
//!   unregistered_dependency  — clause depends on an unregistered clause/ID
//!   dependency_cycle         — clause dependency DAG has a cycle
//!   class_escalation         — weaker claim class justifies a stronger one
//!   unregistered_justifier   — justified_by names an unregistered row
//!   proof_lane_unresolved    — proof-class clause without a resolvable lane
//!   twenty_id_violation      — the twenty-ID spine set is wrong
//!   hash_mismatch            — twenty-ID table hash pin does not match
//!   bad_field                — enum/shape violation on a row field
//!   artifact_missing         — a "live"/"checked" row's artifact is absent

use crate::hash::id_table_hash;
use crate::model::{Clause, Registries};
use crate::predicate;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub code: String,
    pub registry: String,
    pub row_id: String,
    pub msg: String,
}

impl Violation {
    fn new(code: &str, registry: &str, row_id: &str, msg: impl Into<String>) -> Self {
        Violation {
            code: code.into(),
            registry: registry.into(),
            row_id: row_id.into(),
            msg: msg.into(),
        }
    }
}

/// The canonical claim classes and ranks (must match constitution.toml).
pub const CANONICAL_CLASSES: [(&str, i64); 6] = [
    ("invariant", 6),
    ("proof", 5),
    ("bounded_model", 4),
    ("statistical", 3),
    ("slo", 2),
    ("benchmark", 1),
];

pub fn class_rank(name: &str) -> Option<i64> {
    CANONICAL_CLASSES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, r)| *r)
}

/// The twenty-ID spine, in registry order.
pub fn expected_invariant_ids() -> Vec<String> {
    (1..=20).map(|i| format!("FG-INV-{i:02}")).collect()
}

fn validate_constitution(r: &Registries, out: &mut Vec<Violation>) {
    let reg = "constitution";
    // Exactly the six canonical classes with canonical ranks.
    let mut seen = BTreeMap::new();
    for c in &r.constitution.claim_classes {
        seen.insert(c.name.clone(), c.rank);
    }
    for (name, rank) in CANONICAL_CLASSES {
        match seen.get(name) {
            None => out.push(Violation::new(
                "bad_field",
                reg,
                name,
                "canonical claim class missing",
            )),
            Some(&r2) if r2 != rank => out.push(Violation::new(
                "bad_field",
                reg,
                name,
                format!("claim class rank {r2} != canonical {rank}"),
            )),
            _ => {}
        }
    }
    if r.constitution.claim_classes.len() != 6 {
        out.push(Violation::new(
            "bad_field",
            reg,
            "claim_class",
            format!(
                "expected exactly 6 claim classes, found {}",
                r.constitution.claim_classes.len()
            ),
        ));
    }
    // Twelve constraints FG-CON-01..12, in order.
    let expected: Vec<String> = (1..=12).map(|i| format!("FG-CON-{i:02}")).collect();
    let actual: Vec<String> = r
        .constitution
        .constraints
        .iter()
        .map(|c| c.id.clone())
        .collect();
    if actual != expected {
        out.push(Violation::new(
            "bad_field",
            reg,
            "constraint",
            format!("expected constraints {expected:?}, found {actual:?}"),
        ));
    }
    // Six bets B1..B6, in order.
    let expected: Vec<String> = (1..=6).map(|i| format!("B{i}")).collect();
    let actual: Vec<String> = r.constitution.bets.iter().map(|b| b.id.clone()).collect();
    if actual != expected {
        out.push(Violation::new(
            "bad_field",
            reg,
            "bet",
            format!("expected bets {expected:?}, found {actual:?}"),
        ));
    }
    for c in &r.constitution.constraints {
        if c.statement.trim().is_empty() {
            out.push(Violation::new("bad_field", reg, &c.id, "empty statement"));
        }
    }
}

/// All registered claim-row IDs with their class ranks, across registries.
/// Clause keys map to their clause's class rank.
fn rank_index(r: &Registries) -> BTreeMap<String, i64> {
    let mut idx = BTreeMap::new();
    for inv in &r.invariants.invariants {
        // A top-level invariant ID stands for its exact safety/liveness
        // statement: invariant class.
        idx.insert(inv.id.clone(), 6);
        for cl in &inv.clauses {
            if let Some(rank) = class_rank(&cl.claim_class) {
                idx.insert(cl.key.clone(), rank);
            }
        }
    }
    for row in &r.evidence.rows {
        if let Some(rank) = class_rank(&row.claim_class) {
            idx.insert(row.id.clone(), rank);
        }
    }
    for row in &r.slo.rows {
        if let Some(rank) = class_rank(&row.claim_class) {
            idx.insert(row.id.clone(), rank);
        }
    }
    idx
}

/// Check one clause's `justified_by` edges against the class lattice.
/// Exposed for the `claims_class_lattice_narrowing` property test.
pub fn check_justification(
    clause_id: &str,
    clause_class: &str,
    justified_by: &[String],
    ranks: &BTreeMap<String, i64>,
    registry: &str,
    out: &mut Vec<Violation>,
) {
    let Some(clause_rank) = class_rank(clause_class) else {
        // class_not_allowed is reported by the carrier check; nothing to do.
        return;
    };
    for j in justified_by {
        match ranks.get(j) {
            None => out.push(Violation::new(
                "unregistered_justifier",
                registry,
                clause_id,
                format!("justified_by names unregistered row {j:?}"),
            )),
            Some(&jr) if jr < clause_rank => out.push(Violation::new(
                "class_escalation",
                registry,
                clause_id,
                format!(
                    "row {j:?} (rank {jr}) cannot justify class {clause_class:?} (rank {clause_rank}): a weaker claim class may inform policy but may not enforce or justify a stronger one"
                ),
            )),
            _ => {}
        }
    }
}

fn validate_clause(
    r: &Registries,
    clause: &Clause,
    invariant_id: &str,
    clause_keys: &BTreeSet<String>,
    invariant_ids: &BTreeSet<String>,
    ranks: &BTreeMap<String, i64>,
    out: &mut Vec<Violation>,
) {
    let reg = "invariants";
    let id = &clause.key;
    if !clause.key.starts_with(invariant_id) {
        out.push(Violation::new(
            "bad_field",
            reg,
            id,
            format!("clause key must be scoped under its invariant ID {invariant_id:?}"),
        ));
    }
    if !r
        .invariants
        .allowed_claim_classes
        .contains(&clause.claim_class)
    {
        out.push(Violation::new(
            "class_not_allowed",
            reg,
            id,
            format!(
                "claim class {:?} is not allowed in invariants.toml (allowed: {:?}); statistical and empirical claims live in evidence.toml/slo.toml",
                clause.claim_class, r.invariants.allowed_claim_classes
            ),
        ));
    }
    if clause.waiver != "forbidden" {
        out.push(Violation::new(
            "waiver_present",
            reg,
            id,
            format!(
                "waiver is {:?}; every clause must carry the literal waiver = \"forbidden\"",
                clause.waiver
            ),
        ));
    }
    if !matches!(clause.status.as_str(), "live" | "stub" | "dormant") {
        out.push(Violation::new(
            "bad_field",
            reg,
            id,
            format!("status {:?} not in {{live, stub, dormant}}", clause.status),
        ));
    }
    if !matches!(clause.first_gate.as_str(), "G0" | "G1" | "G2" | "G3" | "G4") {
        out.push(Violation::new(
            "bad_field",
            reg,
            id,
            format!("first_gate {:?} not in {{G0..G4}}", clause.first_gate),
        ));
    }
    if clause.exact_statement.trim().is_empty() {
        out.push(Violation::new(
            "bad_field",
            reg,
            id,
            "empty exact_statement",
        ));
    }
    if clause.owner.trim().is_empty() {
        out.push(Violation::new("bad_field", reg, id, "empty owner"));
    }
    if let Err(e) = predicate::parse(&clause.activation_predicate) {
        out.push(Violation::new(
            "bad_field",
            reg,
            id,
            format!("invalid activation_predicate: {e}"),
        ));
    }
    // Checker/negative-test symbols must resolve in checker_index.toml.
    let symbols: BTreeSet<&str> = r.checker_index.iter().map(|c| c.symbol.as_str()).collect();
    for (field, symbol) in [
        ("checker_entrypoint", &clause.checker_entrypoint),
        ("negative_test_entrypoint", &clause.negative_test_entrypoint),
    ] {
        if !symbols.contains(symbol.as_str()) {
            out.push(Violation::new(
                "missing_checker",
                reg,
                id,
                format!("{field} {symbol:?} does not resolve in checker_index.toml"),
            ));
        }
    }
    // Dependencies must be registered clause keys or top-level FG-INV IDs.
    for dep in &clause.dependencies {
        if !clause_keys.contains(dep) && !invariant_ids.contains(dep) {
            out.push(Violation::new(
                "unregistered_dependency",
                reg,
                id,
                format!("dependency {dep:?} is not a registered clause or invariant ID"),
            ));
        }
    }
    // Proof-class clauses must bind a resolvable proof lane.
    if clause.claim_class == "proof" {
        match &clause.proof_lane {
            None => out.push(Violation::new(
                "proof_lane_unresolved",
                reg,
                id,
                "proof-class clause without a proof_lane",
            )),
            Some(lane_id) => {
                if !r.proof_lanes.iter().any(|l| &l.id == lane_id) {
                    out.push(Violation::new(
                        "proof_lane_unresolved",
                        reg,
                        id,
                        format!("proof_lane {lane_id:?} does not resolve in proof_lanes.toml"),
                    ));
                }
            }
        }
    } else if let Some(lane_id) = &clause.proof_lane {
        // Non-proof clauses may cite a lane (e.g. bounded_model → TLA+),
        // but it must still resolve.
        if !r.proof_lanes.iter().any(|l| &l.id == lane_id) {
            out.push(Violation::new(
                "proof_lane_unresolved",
                reg,
                id,
                format!("proof_lane {lane_id:?} does not resolve in proof_lanes.toml"),
            ));
        }
    }
    check_justification(
        id,
        &clause.claim_class,
        &clause.justified_by,
        ranks,
        reg,
        out,
    );
}

fn validate_invariants(r: &Registries, out: &mut Vec<Violation>) {
    let reg = "invariants";
    // Carrier discipline.
    let expected_allowed = ["invariant", "proof", "bounded_model"];
    if r.invariants.allowed_claim_classes != expected_allowed {
        out.push(Violation::new(
            "bad_field",
            reg,
            "registry",
            format!(
                "allowed_claim_classes must be {expected_allowed:?}, found {:?}",
                r.invariants.allowed_claim_classes
            ),
        ));
    }
    if r.invariants.waiver_policy != "forbidden" {
        out.push(Violation::new(
            "waiver_present",
            reg,
            "registry",
            format!(
                "waiver_policy is {:?}; the registry-level policy is the literal \"forbidden\"",
                r.invariants.waiver_policy
            ),
        ));
    }
    // Exactly the twenty-ID spine, in order.
    let expected = expected_invariant_ids();
    let actual: Vec<String> = r
        .invariants
        .invariants
        .iter()
        .map(|i| i.id.clone())
        .collect();
    if actual != expected {
        let expected_set: BTreeSet<&String> = expected.iter().collect();
        let actual_set: BTreeSet<&String> = actual.iter().collect();
        let missing: Vec<&&String> = expected_set.difference(&actual_set).collect();
        let extra: Vec<&&String> = actual_set.difference(&expected_set).collect();
        out.push(Violation::new(
            "twenty_id_violation",
            reg,
            "registry",
            format!(
                "the invariant spine must be exactly FG-INV-01..FG-INV-20 in order; missing: {missing:?}, extra: {extra:?}, actual order: {actual:?}"
            ),
        ));
    }
    // Hash pin (recompute over the *actual* table so a stale pin is caught
    // even when the ID set is correct).
    let recomputed = id_table_hash(&actual);
    if recomputed != r.invariants.twenty_id_hash {
        out.push(Violation::new(
            "hash_mismatch",
            reg,
            "registry",
            format!(
                "twenty_id_hash pin {:?} != recomputed {:?}; id table: {actual:?}",
                r.invariants.twenty_id_hash, recomputed
            ),
        ));
    }
    // Clause-level checks.
    let clause_keys: BTreeSet<String> = r
        .invariants
        .invariants
        .iter()
        .flat_map(|i| i.clauses.iter().map(|c| c.key.clone()))
        .collect();
    let invariant_ids: BTreeSet<String> = r
        .invariants
        .invariants
        .iter()
        .map(|i| i.id.clone())
        .collect();
    let ranks = rank_index(r);
    let mut seen_keys = BTreeSet::new();
    for inv in &r.invariants.invariants {
        for clause in &inv.clauses {
            if !seen_keys.insert(clause.key.clone()) {
                out.push(Violation::new(
                    "bad_field",
                    reg,
                    &clause.key,
                    "duplicate clause key",
                ));
            }
            validate_clause(
                r,
                clause,
                &inv.id,
                &clause_keys,
                &invariant_ids,
                &ranks,
                out,
            );
        }
    }
    // Dependency DAG acyclicity over clause keys (an FG-INV target expands
    // to all clauses of that invariant).
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for inv in &r.invariants.invariants {
        for clause in &inv.clauses {
            let mut targets = Vec::new();
            for dep in &clause.dependencies {
                if clause_keys.contains(dep) {
                    targets.push(dep.clone());
                } else if invariant_ids.contains(dep)
                    && let Some(dep_inv) = r.invariants.invariants.iter().find(|i| &i.id == dep)
                {
                    targets.extend(dep_inv.clauses.iter().map(|c| c.key.clone()));
                }
            }
            edges.insert(clause.key.clone(), targets);
        }
    }
    if let Some(cycle) = find_cycle(&edges) {
        out.push(Violation::new(
            "dependency_cycle",
            reg,
            cycle.first().map(String::as_str).unwrap_or(""),
            format!("clause dependency cycle: {cycle:?}"),
        ));
    }
}

/// Iterative three-color DFS cycle detection; returns one cycle if present.
fn find_cycle(edges: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: BTreeMap<&str, Color> =
        edges.keys().map(|k| (k.as_str(), Color::White)).collect();
    for start in edges.keys() {
        if color.get(start.as_str()) != Some(&Color::White) {
            continue;
        }
        // Stack of (node, next-child-index), plus the gray path for reporting.
        let mut stack: Vec<(&str, usize)> = vec![(start.as_str(), 0)];
        color.insert(start.as_str(), Color::Gray);
        while let Some(&(node, idx)) = stack.last() {
            let children = edges.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if idx < children.len() {
                if let Some(frame) = stack.last_mut() {
                    frame.1 += 1;
                }
                let child = children[idx].as_str();
                match color.get(child) {
                    Some(Color::Gray) => {
                        // Found a cycle: report the gray path from child.
                        let mut cycle: Vec<String> =
                            stack.iter().map(|(n, _)| (*n).to_string()).collect();
                        if let Some(pos) = cycle.iter().position(|n| n == child) {
                            cycle.drain(..pos);
                        }
                        cycle.push(child.to_string());
                        return Some(cycle);
                    }
                    Some(Color::White) => {
                        color.insert(child, Color::Gray);
                        stack.push((child, 0));
                    }
                    _ => {}
                }
            } else {
                color.insert(node, Color::Black);
                stack.pop();
            }
        }
    }
    None
}

fn validate_evidence(r: &Registries, out: &mut Vec<Violation>) {
    let reg = "evidence";
    if r.evidence.allowed_claim_classes != ["statistical"] {
        out.push(Violation::new(
            "bad_field",
            reg,
            "registry",
            format!(
                "allowed_claim_classes must be [\"statistical\"], found {:?}",
                r.evidence.allowed_claim_classes
            ),
        ));
    }
    let mut seen = BTreeSet::new();
    for row in &r.evidence.rows {
        if !seen.insert(row.id.clone()) {
            out.push(Violation::new("bad_field", reg, &row.id, "duplicate id"));
        }
        if !(id_matches(&row.id, "FG-CAL-") || id_matches(&row.id, "FG-EVID-")) {
            out.push(Violation::new(
                "bad_field",
                reg,
                &row.id,
                "id must match FG-CAL-NN or FG-EVID-NN",
            ));
        }
        if row.claim_class != "statistical" {
            out.push(Violation::new(
                "class_not_allowed",
                reg,
                &row.id,
                format!(
                    "claim class {:?} is not allowed in evidence.toml (only \"statistical\")",
                    row.claim_class
                ),
            ));
        }
        if row.required_disclosures.is_empty() {
            out.push(Violation::new(
                "bad_field",
                reg,
                &row.id,
                "required_disclosures must be non-empty",
            ));
        }
    }
}

fn validate_slo(r: &Registries, out: &mut Vec<Violation>) {
    let reg = "slo";
    let expected_allowed = ["slo", "benchmark", "bounded_model"];
    if r.slo.allowed_claim_classes != expected_allowed {
        out.push(Violation::new(
            "bad_field",
            reg,
            "registry",
            format!(
                "allowed_claim_classes must be {expected_allowed:?}, found {:?}",
                r.slo.allowed_claim_classes
            ),
        ));
    }
    let mut seen = BTreeSet::new();
    for row in &r.slo.rows {
        if !seen.insert(row.id.clone()) {
            out.push(Violation::new("bad_field", reg, &row.id, "duplicate id"));
        }
        if !r.slo.allowed_claim_classes.contains(&row.claim_class) {
            out.push(Violation::new(
                "class_not_allowed",
                reg,
                &row.id,
                format!(
                    "claim class {:?} is not allowed in slo.toml (allowed: {:?})",
                    row.claim_class, r.slo.allowed_claim_classes
                ),
            ));
        }
        if id_matches(&row.id, "FG-CFG-") {
            // Configuration-model claims: bounded_model, never invariants
            // (§15.0, Appendix G).
            if row.claim_class != "bounded_model"
                || row.kind.as_deref() != Some("configuration_model")
            {
                out.push(Violation::new(
                    "bad_field",
                    reg,
                    &row.id,
                    "FG-CFG rows must be claim_class = \"bounded_model\" with kind = \"configuration_model\"",
                ));
            }
        }
        if matches!(row.claim_class.as_str(), "slo" | "benchmark")
            && (row.operation_class.is_none() || row.posture.is_none() || row.audit_class.is_none())
        {
            // Appendix G: every µs/throughput budget is keyed
            // {operation_class, posture, audit_class}.
            out.push(Violation::new(
                "bad_field",
                reg,
                &row.id,
                "slo/benchmark rows must be keyed {operation_class, posture, audit_class}",
            ));
        }
        if row.required_disclosures.is_empty() {
            out.push(Violation::new(
                "bad_field",
                reg,
                &row.id,
                "required_disclosures must be non-empty",
            ));
        }
    }
}

fn validate_proof_lanes(r: &Registries, root: &Path, out: &mut Vec<Violation>) {
    let reg = "proof_lanes";
    let mut seen = BTreeSet::new();
    for lane in &r.proof_lanes {
        if !seen.insert(lane.id.clone()) {
            out.push(Violation::new("bad_field", reg, &lane.id, "duplicate id"));
        }
        if !matches!(lane.lane.as_str(), "lean" | "tlaplus") {
            out.push(Violation::new(
                "bad_field",
                reg,
                &lane.id,
                format!("lane {:?} not in {{lean, tlaplus}}", lane.lane),
            ));
        }
        if lane.model_scope.trim().is_empty() {
            out.push(Violation::new(
                "bad_field",
                reg,
                &lane.id,
                "empty model_scope: a proof-lane manifest must state exactly what is and is not proven",
            ));
        }
        match lane.status.as_str() {
            "declared" => {}
            "checked" => {
                if !root.join(&lane.artifact).is_file() {
                    out.push(Violation::new(
                        "artifact_missing",
                        reg,
                        &lane.id,
                        format!(
                            "status is \"checked\" but artifact {:?} does not exist",
                            lane.artifact
                        ),
                    ));
                }
            }
            other => out.push(Violation::new(
                "bad_field",
                reg,
                &lane.id,
                format!("status {other:?} not in {{declared, checked}}"),
            )),
        }
    }
}

fn validate_checker_index(r: &Registries, root: &Path, out: &mut Vec<Violation>) {
    let reg = "checker_index";
    let mut seen = BTreeSet::new();
    for c in &r.checker_index {
        if !seen.insert(c.symbol.clone()) {
            out.push(Violation::new(
                "bad_field",
                reg,
                &c.symbol,
                "duplicate symbol",
            ));
        }
        if !matches!(c.kind.as_str(), "cargo-test" | "script" | "binary" | "stub") {
            out.push(Violation::new(
                "bad_field",
                reg,
                &c.symbol,
                format!(
                    "kind {:?} not in {{cargo-test, script, binary, stub}}",
                    c.kind
                ),
            ));
        }
        match c.status.as_str() {
            "stub" => {}
            "live" => {
                if !root.join(&c.artifact).is_file() {
                    out.push(Violation::new(
                        "artifact_missing",
                        reg,
                        &c.symbol,
                        format!(
                            "status is \"live\" but artifact {:?} does not exist",
                            c.artifact
                        ),
                    ));
                }
            }
            other => out.push(Violation::new(
                "bad_field",
                reg,
                &c.symbol,
                format!("status {other:?} not in {{live, stub}}"),
            )),
        }
    }
}

fn id_matches(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|rest| rest.len() == 2 && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Run every check. `root` is the repository root (artifact resolution).
pub fn validate_all(r: &Registries, root: &Path) -> Vec<Violation> {
    let mut out = Vec::new();
    validate_constitution(r, &mut out);
    validate_invariants(r, &mut out);
    validate_evidence(r, &mut out);
    validate_slo(r, &mut out);
    validate_proof_lanes(r, root, &mut out);
    validate_checker_index(r, root, &mut out);
    out
}
