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
//!   proof_lane_declared_while_clause_promoted
//!                            — a clause off "stub" cites a still-declared lane
//!   proof_lane_gate_*        — a "checked" lane's gate is undeclared, unresolved
//!                              or not itself live
//!   proof_lane_proves_nothing / proof_lane_admits_anything
//!                            — a "checked" lane's artifact cannot report a
//!                              failure (no proposition; `sorry`; no INVARIANT)
//!   proof_lane_system_unreadable
//!                            — a "checked" lane names a formal system no reader
//!                              here adjudicates (completeness guard)
//!   clause_promoted_without_live_checker
//!                            — a clause is enforced while an entrypoint is not a
//!                              live checker (promotion law)
//!   clause_negative_test_is_its_own_checker
//!                            — an enforced clause's negative test IS its checker
//!   enforcement_coverage_incomplete / _empty / _drift
//!                            — "every ID has a live checker" examined fewer ids
//!                              than exist, had nothing to examine, or disagrees
//!                              with the declared enforced counts
//!   checker_liveness_self_test_failed
//!                            — the liveness readers failed a known answer, so no
//!                              clean verdict anywhere below is licensed (control;
//!                              emitted ONCE for the whole run, since one reader
//!                              serves all three registries that ask it)
//!   twenty_id_violation      — the twenty-ID spine set is wrong
//!   hash_mismatch            — twenty-ID table hash pin does not match
//!   bad_field                — enum/shape violation on a row field
//!   artifact_missing         — a "live"/"checked" row's artifact is absent
//!   checker_*                — a "live" checker row is not registered, invoked
//!                              or capable of failing
//!   script_undeclared        — a scripts/**/*.sh is neither registered nor declared
//!   script_disposition_*     — a non-gate declaration is dangling or conflicting
//!   script_scan_empty        — the scripts/ scan found nothing (control)

use crate::hash::id_table_hash;
use crate::model::{Clause, Manifest, Registries, SCRIPT_ROLES, ScriptDisposition};
use crate::predicate;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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

/// The closed clause-status vocabulary, and — for each status — whether a
/// clause in it is ENFORCED.
///
/// One list, two consumers, on purpose
/// (`fgdb-clause-promotion-to-live-is-unguarded-nllh`). The status vocabulary
/// was previously spelled inline in a `matches!`, and the promotion law that
/// followed would have been a second `== "live"` beside it. Two spellings of one
/// vocabulary is how a status added later arrives enforced by nothing: the
/// schema check would reject it (or be widened to accept it) while the law that
/// gives `live` its meaning silently skips it. Adding a status here forces the
/// author to answer the only question that matters about it.
///
/// `dormant` is not enforced by design: `invariants.toml`'s header says "an
/// unimplemented or dormant clause forces its feature off and cannot count as
/// covered", so it is a stub that has been switched off rather than a promotion.
pub const CLAUSE_STATUS_ENFORCED: &[(&str, bool)] =
    &[("live", true), ("stub", false), ("dormant", false)];

/// Is `status` a registered clause status, and does it enforce?
///
/// `None` means the status is not in the vocabulary at all.
pub fn clause_status_is_enforced(status: &str) -> Option<bool> {
    CLAUSE_STATUS_ENFORCED
        .iter()
        .find(|(name, _)| *name == status)
        .map(|(_, enforced)| *enforced)
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

#[allow(clippy::too_many_arguments)]
fn validate_clause(
    r: &Registries,
    prover: &crate::liveness::Prover<'_>,
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
    // The clause status vocabulary, read from the ONE list that also decides
    // whether the promotion law below applies. See [`CLAUSE_STATUS_ENFORCED`]:
    // two copies of this vocabulary is how a new status would arrive enforced
    // by nothing.
    let enforced = clause_status_is_enforced(&clause.status);
    if enforced.is_none() {
        out.push(Violation::new(
            "bad_field",
            reg,
            id,
            format!(
                "status {:?} not in {{{}}}",
                clause.status,
                CLAUSE_STATUS_ENFORCED
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
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
    // THE PROMOTION LAW (`fgdb-clause-promotion-to-live-is-unguarded-nllh`).
    //
    // `invariants.toml`'s own header states it: "Workstream beads flip status
    // stub -> live in the same change that LANDS THE CHECKER, never before."
    // AGENTS.md rests every G1-G4 exit gate on it: "no subsystem ships against
    // an unenforced invariant ... cannot pass while any invariant it depends on
    // lacks a live checker in invariants.toml."
    //
    // Nothing implemented it. Measured before this was written: promoting a
    // shipped clause to `live`, changing nothing else, produced ZERO violations
    // — its checker stayed a `status = "stub"` row pointing at a crate that does
    // not exist. So did the degenerate case, a live clause whose
    // `negative_test_entrypoint` IS its `checker_entrypoint`. The whole law for
    // both fields was "the string resolves to a row".
    //
    // What `live` means for a CHECKER row is `crate::liveness`'s question, and
    // it is answered there rather than a third time here: this is the same
    // delegation `assess_lane` makes for proof lanes
    // (`fgdb-proof-lane-checked-is-only-file-existence-0f1l`), which is the same
    // reader `validate_checker_index` uses (`...-tl0o`). Three faces of one
    // fact, one reader.
    //
    // The negative test is held to the SAME bar as the checker and not a weaker
    // one, because its entire purpose is to prove the checker can go red — a
    // negative test that cannot itself fail proves nothing, which is exactly the
    // shape of every defect in this family.
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
    if enforced == Some(true) {
        for defect in prover.assess_clause(clause, &r.checker_index) {
            out.push(Violation::new(
                defect.kind.code(),
                reg,
                id,
                format!("clause status is {:?} but {}", clause.status, defect.detail),
            ));
        }
    }

    // Proof-class clauses must bind a resolvable proof lane; any clause that
    // cites one at all (e.g. bounded_model → TLA+) must have it resolve. ONE
    // lookup, because two copies of it is how the second law below came to be
    // checked by neither.
    let cited = clause
        .proof_lane
        .as_ref()
        .map(|lane_id| (lane_id, r.proof_lanes.iter().find(|l| &l.id == lane_id)));
    match cited {
        None => {
            if clause.claim_class == "proof" {
                out.push(Violation::new(
                    "proof_lane_unresolved",
                    reg,
                    id,
                    "proof-class clause without a proof_lane",
                ));
            }
        }
        Some((lane_id, None)) => out.push(Violation::new(
            "proof_lane_unresolved",
            reg,
            id,
            format!("proof_lane {lane_id:?} does not resolve in proof_lanes.toml"),
        )),
        Some((lane_id, Some(lane))) => {
            // proof_lanes.toml's header, second law: "A proof-class clause may
            // cite a declared lane ONLY WHILE ITS OWN STATUS IS \"stub\"." A
            // declared lane's artifact does not exist yet, so a clause promoted
            // off `stub` while citing one has been promoted against a proof
            // nobody has written. The registry stated this combination was
            // illegal and nothing rejected it
            // (`fgdb-proof-lane-checked-is-only-file-existence-0f1l`).
            if lane.status == "declared" && clause.status != "stub" {
                out.push(Violation::new(
                    "proof_lane_declared_while_clause_promoted",
                    reg,
                    id,
                    format!(
                        "clause status is {:?} while its proof_lane {lane_id:?} is still \
                         \"declared\" (artifact {:?} does not exist yet); a declared lane may \
                         be cited only while the citing clause is \"stub\"",
                        clause.status, lane.artifact
                    ),
                ));
            }
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

fn validate_invariants(
    r: &Registries,
    prover: &crate::liveness::Prover<'_>,
    out: &mut Vec<Violation>,
) {
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
                prover,
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

/// Schema and checkedness for the `[[lane]]` rows of
/// `registries/proof_lanes.toml`.
///
/// The checkedness half is [`crate::liveness`], not a second `is_file()` —
/// `status = "checked"` was `root.join(artifact).is_file()`, the pre-`tl0o`
/// checker read down to the missing path-safety guard, and the registry header's
/// own definition ("the artifact exists in-repo AND is CI-checked") had no
/// implementation of its second conjunct at all
/// (`fgdb-proof-lane-checked-is-only-file-existence-0f1l`). "Is CI-checked" is
/// the question `liveness` was written to answer, so the lane delegates rather
/// than re-deriving: it names its gate and `liveness` proves that gate live.
///
/// Same shape as [`validate_checker_index`]: the self-test is consulted FIRST,
/// because a reader that has stopped reading returns "no defects" for every row,
/// which is byte-identical to what a healthy registry returns.
/// AGENTS.md's hardest rule, made non-vacuous
/// (`fgdb-fginv-spine-zero-live-checkers-v05b`).
///
/// *Spec-First Workflow* item 2: **"CI cross-checks that every ID has a live
/// checker."** The hard rule under it: "no subsystem ships against an unenforced
/// invariant. A workstream exit gate (G1-G4) cannot pass while any invariant it
/// depends on lacks a live checker."
///
/// Every clause is `stub` and every entrypoint resolves to a `stub` row, so that
/// cross-check quantified over an EMPTY SET and passed. That is the purest form
/// of the family this file has spent the evening closing — not a reader that
/// answers wrongly, but a universally-quantified law with nothing to quantify
/// over, whose exit code is identical to a fully enforced spine's.
///
/// The fix is not "make the twenty live" — that is the verification programme,
/// not a law. It is to make the emptiness *accounted for*:
///
/// * the ledger examines EVERY id `expected_invariant_ids()` names and says so;
///   a law that examined fewer has not passed, it has stopped looking;
/// * a spine with no clauses at all is a violation, never a pass, exactly as
///   `script_scan_empty` is for the scripts/ scan;
/// * the enforced counts are DECLARED in the registry and checked in BOTH
///   directions — too few means a clause silently regressed, too many means one
///   was promoted without the gate review the doctrine requires.
///
/// "Is this clause's apparatus live" is not re-derived here: it is
/// `liveness::Prover::assess_clause`, the same reader the promotion law
/// (`...-nllh`) uses, which is itself `Prover::assess` — the reader `tl0o` built
/// and `0f1l` consumed. Four faces, one reader.
fn validate_enforcement_coverage(
    r: &Registries,
    prover: &crate::liveness::Prover<'_>,
    out: &mut Vec<Violation>,
) {
    let reg = "invariants";
    let mut accounted: Vec<String> = Vec::new();
    let mut clauses_total = 0usize;
    let mut enforced_clauses: Vec<String> = Vec::new();
    let mut enforced_invariants: Vec<String> = Vec::new();

    for invariant in &r.invariants.invariants {
        accounted.push(invariant.id.clone());
        // An invariant with no clauses enforces nothing: there is no apparatus
        // to be live. Seeding `true` here would let an emptied invariant count
        // as enforced, which is this bug one level down.
        let mut every_clause_enforced = !invariant.clauses.is_empty();
        for clause in &invariant.clauses {
            clauses_total += 1;
            let enforced = clause_status_is_enforced(&clause.status) == Some(true)
                && prover.assess_clause(clause, &r.checker_index).is_empty();
            if enforced {
                enforced_clauses.push(clause.key.clone());
            } else {
                every_clause_enforced = false;
            }
        }
        if every_clause_enforced {
            enforced_invariants.push(invariant.id.clone());
        }
    }

    // COMPLETENESS GUARD. The ledger must have accounted for the whole spine.
    // `expected_invariant_ids()` is the ONE reader for what the spine must be;
    // `validate_invariants` consumes it to check the registry, this consumes it
    // to check the LEDGER. Without it a spine that shrank to nothing reports
    // "0 enforced, 0 declared, pass" — a law that succeeded by having nothing to
    // check.
    let expected_ids = expected_invariant_ids();
    if accounted != expected_ids {
        out.push(Violation::new(
            "enforcement_coverage_incomplete",
            reg,
            "<coverage>",
            format!(
                "the enforcement ledger accounted for {} invariant ids {:?}, but the spine is \
                 {} ids {:?}; a coverage law that examined fewer rows than exist has not \
                 passed, it has stopped looking",
                accounted.len(),
                accounted,
                expected_ids.len(),
                expected_ids
            ),
        ));
    }

    // A law with nothing to check has NOT passed. Same instrument as
    // `script_scan_empty`: zero rows and a broken reader are indistinguishable
    // at the exit code, so zero is a violation rather than a pass.
    if clauses_total == 0 {
        out.push(Violation::new(
            "enforcement_coverage_empty",
            reg,
            "<coverage>",
            "the spine declares no clauses at all, so \"every ID has a live checker\" is \
             quantified over the empty set; a zero here cannot be distinguished from a \
             registry that failed to load, so it is a violation rather than a pass",
        ));
        return;
    }

    // The declared expectations, checked in BOTH directions.
    for (what, measured, declared, moved) in [
        (
            "clauses",
            enforced_clauses.len() as i64,
            r.invariants.expected_enforced_clauses,
            &enforced_clauses,
        ),
        (
            "invariant ids",
            enforced_invariants.len() as i64,
            r.invariants.expected_enforced_invariants,
            &enforced_invariants,
        ),
    ] {
        if measured != declared {
            out.push(Violation::new(
                "enforcement_coverage_drift",
                reg,
                "<coverage>",
                format!(
                    "registry declares {declared} enforced {what}, measured {measured} over \
                     {clauses_total} clauses in {} invariant ids; enforced now: {moved:?}. \
                     Too few means a clause regressed to stub; too many means one was \
                     promoted without the gate review the doctrine requires — either way the \
                     declaration and the tree disagree about what this project enforces",
                    accounted.len()
                ),
            ));
        }
    }
}

fn validate_proof_lanes(
    r: &Registries,
    prover: &crate::liveness::Prover<'_>,
    out: &mut Vec<Violation>,
) {
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
            // Both statuses are adjudicated: a `declared` lane still owes a safe
            // repository-relative artifact path, and that was never checked at
            // all. The rest of the reads apply only to `checked`, which the
            // prover decides — not this function, so there is one place that
            // knows what the word means.
            "declared" | "checked" => {
                for defect in prover.assess_lane(lane, &r.checker_index) {
                    out.push(Violation::new(
                        defect.kind.code(),
                        reg,
                        &lane.id,
                        format!("status is {:?} but {}", lane.status, defect.detail),
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

/// Schema and liveness for the `[[checker]]` rows of
/// `registries/checker_index.toml`.
///
/// The liveness half is [`crate::liveness`], not a second `is_file()` — see that
/// module's header. Two facts matter about the shape of this function:
///
/// * the self-test is consulted FIRST, and a broken reader is reported as a
///   violation rather than allowed to produce a clean sweep. A liveness reader
///   that has stopped reading returns "no defects" for every row, which is
///   byte-identical to what a healthy registry returns;
/// * a live row that is not actually live is reported per row, with the code
///   naming which of the three claims — registered, invoked, capable of failing
///   — it failed.
fn validate_checker_index(
    r: &Registries,
    prover: &crate::liveness::Prover<'_>,
    out: &mut Vec<Violation>,
) {
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
                for defect in prover.assess(c) {
                    out.push(Violation::new(
                        defect.kind.code(),
                        reg,
                        &c.symbol,
                        format!("status is \"live\" but {}", defect.detail),
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

/// Recursively collect every shell deliverable below `scripts/`.
///
/// This walk is deliberately filesystem-derived rather than Git-derived: an
/// untracked script is already absent from the Git-based shell lint and must
/// not disappear from this closure too. Every directory-entry error fails the
/// whole scan closed; silently dropping one child would make an unreadable
/// subtree indistinguishable from a fully declared one.
fn collect_shell_scripts(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let relative_dir = dir.strip_prefix(root).unwrap_or(dir);
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", relative_dir.display()))?;
    let mut entries = entries.collect::<Result<Vec<_>, _>>().map_err(|e| {
        format!(
            "cannot read a directory entry below {}: {e}",
            relative_dir.display()
        )
    })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "cannot classify shell-deliverable candidate {}: {e}",
                path.strip_prefix(root).unwrap_or(&path).display()
            )
        })?;
        if file_type.is_dir() {
            collect_shell_scripts(root, &path, out)?;
            continue;
        }
        if !entry.file_name().to_string_lossy().ends_with(".sh") {
            continue;
        }
        let relative = path.strip_prefix(root).map_err(|e| {
            format!(
                "scanned path {} escaped repository root {}: {e}",
                path.display(),
                root.display()
            )
        })?;
        out.push(relative.to_string_lossy().replace('\\', "/"));
    }
    Ok(())
}

/// Close `checker_index.toml` in the FILE -> ROW direction.
///
/// [`validate_checker_index`] closes row -> file: a `live` row's artifact must
/// exist. Nothing closed the other way, so a `scripts/**/*.sh` could carry every
/// signal of a gate — `set -euo pipefail`, pinned counts, PASS/FAIL counters —
/// while no runner and no registry knew it existed. Five top-level scripts did
/// (`fgdb-orphan-w1-e2e-gates-unregistered-unrun-vuq8`), holding six hard-pinned
/// magic numbers between them. Before `fgdb-fknh`, the same blind spot remained
/// below `scripts/lib/` and `scripts/git_hooks/`.
///
/// Since `scripts/check.sh` became registry-derived, registration is also what
/// makes a script RUN, so this is the law that decides whether a deliverable is
/// a gate at all.
///
/// The scan reads the filesystem rather than `git ls-files`: an UNTRACKED script
/// is exempt from the git-based shell lint, so it must not also be exempt here.
fn validate_script_closure(r: &Registries, root: &Path, out: &mut Vec<Violation>) {
    let reg = "checker_index";
    let dir = root.join("scripts");

    let mut on_disk = Vec::new();
    if let Err(e) = collect_shell_scripts(root, &dir, &mut on_disk) {
        out.push(Violation::new(
            "script_scan_failed",
            reg,
            "scripts/",
            format!("{e} — refusing to report recursive script closure as checked"),
        ));
        return;
    }
    on_disk.sort();

    // CONTROL. Every verdict below is a statement about a set this function
    // built by scanning a directory. If the scan comes back empty, the two
    // readings — "there are no scripts" and "the scanner is broken" — are
    // indistinguishable, and every "declared" verdict is then quantified over
    // nothing. `scripts/check.sh` is itself a script, so zero is never correct.
    if on_disk.is_empty() {
        out.push(Violation::new(
            "script_scan_empty",
            reg,
            "scripts/",
            "scanned scripts/ and found no *.sh at all; a zero result here cannot be \
             distinguished from a broken scan, so it is a violation rather than a pass",
        ));
        return;
    }

    let registered: BTreeSet<&str> = r
        .checker_index
        .iter()
        .map(|c| c.artifact.as_str())
        .filter(|a| a.starts_with("scripts/"))
        .collect();
    let declared: BTreeMap<&str, &ScriptDisposition> = r
        .script_dispositions
        .iter()
        .map(|d| (d.path.as_str(), d))
        .collect();

    for path in &on_disk {
        let is_registered = registered.contains(path.as_str());
        let disposition = declared.get(path.as_str()).copied();
        match (is_registered, disposition) {
            (false, None) => out.push(Violation::new(
                "script_undeclared",
                reg,
                path,
                "shell deliverable is neither a registered checker artifact nor a \
                 [[script_disposition]] row; register it, or say why it is not a gate",
            )),
            (true, Some(d)) => out.push(Violation::new(
                "script_disposition_conflict",
                reg,
                path,
                format!(
                    "is a registered checker artifact AND declared a non-gate {:?}; \
                     exactly one of the two must hold",
                    d.role
                ),
            )),
            _ => {}
        }
    }

    let present: BTreeSet<&str> = on_disk.iter().map(String::as_str).collect();
    for d in &r.script_dispositions {
        if !present.contains(d.path.as_str()) {
            out.push(Violation::new(
                "script_disposition_dangling",
                reg,
                &d.path,
                "[[script_disposition]] names a script that does not exist",
            ));
        }
        if !SCRIPT_ROLES.contains(&d.role.as_str()) {
            out.push(Violation::new(
                "bad_field",
                reg,
                &d.path,
                format!("role {:?} not in {SCRIPT_ROLES:?}", d.role),
            ));
        }
        if d.reason.trim().is_empty() {
            out.push(Violation::new(
                "bad_field",
                reg,
                &d.path,
                "a non-gate declaration requires a reason",
            ));
        }
    }
}

fn id_matches(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|rest| rest.len() == 2 && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// `logical_object_kinds.toml` rows with `status = "active"` and the arms of the
/// `active_logical_object_kinds!` invocation in `crates/fgdb-types/src/refs.rs`
/// must biject, by code AND by name.
///
/// WHY THIS LIVES HERE. The binding is real but was enforced *only* by two
/// `const _: () = assert!(...)` inside `fgdb-types`, so it fired at compile time
/// of a foundation crate and nowhere else. A pane could add an `active` row,
/// watch `registry-check all`, the identity suite, architecture-check and the G0
/// identity e2e all report green, and leave the whole workspace unable to build.
/// That is exactly what happened: 84418b2 added `DurableCapabilityValidationEvidence`
/// (0x028f) as active with no arm, taking the count 10 -> 11, and `main` stayed
/// broken for hours with every registry gate green.
///
/// The const assert also cannot say WHICH row is wrong — it only compares two
/// counts, so its diagnostic is a bare `assertion failed: count_bytes(...) == ...`.
/// This checker names the offending symbol in both directions.
fn validate_active_logical_kind_arms(root: &Path, out: &mut Vec<Violation>) {
    let reg = "logical_object_kinds";
    let refs_path = root.join("crates/fgdb-types/src/refs.rs");
    let toml_path = root.join("registries/logical_object_kinds.toml");

    // An unreadable input is a violation, never a skip: a checker that silently
    // does nothing is indistinguishable from one that passed.
    let Ok(refs_src) = std::fs::read_to_string(&refs_path) else {
        out.push(Violation::new(
            "active_logical_kind_source_unreadable",
            reg,
            "refs.rs",
            format!(
                "cannot read {}; refusing to report the arm binding as checked",
                refs_path.display()
            ),
        ));
        return;
    };
    let Ok(toml_src) = std::fs::read_to_string(&toml_path) else {
        out.push(Violation::new(
            "active_logical_kind_source_unreadable",
            reg,
            "logical_object_kinds.toml",
            format!("cannot read {}", toml_path.display()),
        ));
        return;
    };

    // Arms: `    Variant = 0x0001 => "Name",` inside the invocation block.
    //
    // The block boundaries and the arm parse are both read out of MASKED source
    // -- comments and literal contents blanked, every other byte left in place,
    // so the masked line and the raw line are byte-aligned and an offset in one
    // names the same column of the other. Only the arm's NAME is taken from the
    // raw line, bounded by where the masked line says the code ends.
    //
    // Reading raw text here was wrong in both directions at once. A commented-
    // out arm parsed as a live one, so the bijection this function exists to
    // enforce -- active registry row <-> real Rust arm -- was satisfied by text
    // the compiler ignores, which is precisely the failure it was written to
    // catch, reached from the other side. And a trailing comment on a live arm
    // put the comment's own text inside that arm's name, reporting
    // `active_logical_kind_name_mismatch` against a row that was correct: a
    // wrong diagnosis wearing the right verdict.
    //
    // `unsafe_ledger::mask_source` is the one reader for "which bytes of this
    // Rust source are live code". A second comment-stripper here is how a fixed
    // reader stays fixed while its twin rots.
    let masked = crate::unsafe_ledger::mask_source(&refs_src);
    let mut arms: BTreeMap<u32, String> = BTreeMap::new();
    let mut depth = 0usize;
    let mut found_macro = false;
    for (line, raw) in masked.text().lines().zip(refs_src.lines()) {
        let trimmed = line.trim();
        if depth == 0 {
            if trimmed.starts_with("active_logical_object_kinds!") && trimmed.ends_with('{') {
                depth = 1;
                found_macro = true;
            }
            continue;
        }
        // Brace DEPTH, not a bare `}`: the block used to end at the first line
        // that was exactly `}`, so any nested block inside the invocation closed
        // the arm set early and every arm below it read as missing.
        let opens = line.bytes().filter(|b| *b == b'{').count();
        let closes = line.bytes().filter(|b| *b == b'}').count();
        let after = (depth + opens).saturating_sub(closes);
        if after == 0 {
            depth = 0;
            continue;
        }
        depth = after;
        let Some(arrow) = line.find("=>") else {
            continue;
        };
        let Some((_variant, code_text)) = line[..arrow].split_once('=') else {
            continue;
        };
        let code_text = code_text.trim().trim_end_matches(',').trim();
        let Some(hex) = code_text.strip_prefix("0x") else {
            continue;
        };
        let Ok(code) = u32::from_str_radix(hex, 16) else {
            continue;
        };
        // Everything from the arrow to the last live byte of the line. A
        // trailing comment is blank in the masked line, so `trim_end` drops it
        // without ever letting it reach the name.
        let name = raw
            .get(arrow + 2..line.trim_end().len())
            .unwrap_or_default()
            .trim()
            .trim_end_matches(',')
            .trim()
            .trim_matches('"')
            .to_owned();
        if arms.insert(code, name).is_some() {
            out.push(Violation::new(
                "active_logical_kind_arm_duplicate",
                reg,
                &format!("0x{code:04x}"),
                "two arms declare the same object_kind code",
            ));
        }
    }
    if !found_macro {
        out.push(Violation::new(
            "active_logical_kind_macro_absent",
            reg,
            "refs.rs",
            "no `active_logical_object_kinds!` invocation found; the arm binding was NOT checked",
        ));
        return;
    }

    // Active rows out of the registry, read as DATA through the one TOML parser
    // this crate has.
    //
    // The line scan this replaced matched `object_kind = 0x`, `name = ` and
    // `status = ` as literal prefixes of a trimmed line, which is the same
    // substring-for-structure defect as the arm scanner above, one file over,
    // and it failed in the direction that manufactures work: `name='Beta'` in
    // TOML literal quotes, or `name="Beta"` with no spaces around the `=`, made
    // the row unreadable, so it dropped silently out of the active set and its
    // perfectly good Rust arm was reported as `arm_without_active_logical_kind`.
    // A trailing comment on `status` dropped EVERY row and the run reported
    // `active_logical_kind_none_parsed`. Every one of those is valid TOML that
    // `appendix_a` -- which generates this very file -- reads without trouble.
    let table = match crate::toml::parse(&toml_src) {
        Ok(t) => t,
        Err(e) => {
            out.push(Violation::new(
                "active_logical_kind_registry_unparseable",
                reg,
                "logical_object_kinds.toml",
                format!("cannot parse the registry, so the arm binding was NOT checked: {e}"),
            ));
            return;
        }
    };
    let rows = match crate::toml::get_table_array(&table, "kind", "logical_object_kinds.toml") {
        Ok(rows) => rows,
        Err(e) => {
            out.push(Violation::new(
                "active_logical_kind_registry_unparseable",
                reg,
                "logical_object_kinds.toml",
                format!("cannot read the [[kind]] rows, so the arm binding was NOT checked: {e}"),
            ));
            return;
        }
    };
    let mut active: BTreeMap<u32, String> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let ctx = format!("logical_object_kinds.toml.kind[{i}]");
        // A row this checker cannot read is a violation, never a silent drop:
        // dropping it is exactly how a live arm came to look orphaned.
        let (code, name, status) = match (
            crate::toml::get_int(row, "object_kind", &ctx),
            crate::toml::get_str(row, "name", &ctx),
            crate::toml::get_str(row, "status", &ctx),
        ) {
            (Ok(code), Ok(name), Ok(status)) => (code, name, status),
            _ => {
                out.push(Violation::new(
                    "active_logical_kind_row_unreadable",
                    reg,
                    &ctx,
                    "a [[kind]] row is missing object_kind, name or status; its arm binding \
                     cannot be checked and the row is not assumed inactive",
                ));
                continue;
            }
        };
        if status == "active" {
            active.insert(code as u32, name);
        }
    }

    if active.is_empty() {
        out.push(Violation::new(
            "active_logical_kind_none_parsed",
            reg,
            "logical_object_kinds.toml",
            "parsed zero active rows; the registry format changed and this check is vacuous",
        ));
        return;
    }

    // The foundation crate consumes this projection as raw bytes and requires
    // each active row's object_kind/name/status fields to be adjacent and in
    // that order.  The generator byte-compare alone cannot see a generator
    // change that preserves its own output, so keep the consumer contract
    // load-bearing here as well.
    for (code, name) in &active {
        let needle = format!("object_kind = 0x{code:04x}\nname = \"{name}\"\nstatus = \"active\"");
        if !toml_src.contains(&needle) {
            out.push(Violation::new(
                "logical_kind_projection_layout",
                reg,
                name,
                "active logical kind projection must keep object_kind, name, and status adjacent in that order for fgdb-types raw-byte consumers",
            ));
        }
    }

    for (c, n) in &active {
        match arms.get(c) {
            Some(arm_name) if arm_name == n => {}
            Some(arm_name) => out.push(Violation::new(
                "active_logical_kind_name_mismatch",
                reg,
                n,
                format!("row 0x{c:04x} is named {n:?} but its refs.rs arm is named {arm_name:?}"),
            )),
            None => out.push(Violation::new(
                "active_logical_kind_without_arm",
                reg,
                n,
                format!(
                    "row 0x{c:04x} {n:?} is status=\"active\" but no `active_logical_object_kinds!` \
                     arm declares it; fgdb-types will not compile. Either add the arm or make the \
                     row status=\"reserved\""
                ),
            )),
        }
    }
    for (c, n) in &arms {
        if !active.contains_key(c) {
            out.push(Violation::new(
                "arm_without_active_logical_kind",
                reg,
                n,
                format!("refs.rs arm 0x{c:04x} {n:?} has no status=\"active\" registry row"),
            ));
        }
    }
}

/// Every capability atom named by an `activation_predicate` must be declared in
/// the registry's `capability_atoms` vocabulary.
///
/// The atom space used to be OPEN, and that is not a cosmetic gap: an atom that
/// is merely misspelled evaluates false exactly as an unlanded capability does,
/// so `mvcc-visibilty` makes its clause unreachable forever. Measured before
/// this check existed: misspelling one atom shrank the reachable set from 20 to
/// 19 with no violation of any kind. In a tree where the other 19 clauses were
/// live, the misspelled one would have escaped enforcement under a green
/// verdict — permanently, and including after Genesis, because nothing in the
/// system could ever notice.
///
/// Closing the vocabulary is what makes a typo a validation error instead of a
/// silent absence. The same vocabulary is applied to capability manifests by
/// [`validate_manifest_atoms`].
fn validate_capability_atoms(r: &Registries, out: &mut Vec<Violation>) {
    let reg = "invariants";
    let declared: BTreeSet<&str> = r
        .invariants
        .capability_atoms
        .iter()
        .map(String::as_str)
        .collect();
    for inv in &r.invariants.invariants {
        for clause in &inv.clauses {
            let Ok(expr) = predicate::parse(&clause.activation_predicate) else {
                // Already reported as `bad_field`; nothing to say twice.
                continue;
            };
            let mut atoms = BTreeSet::new();
            predicate::atoms(&expr, &mut atoms);
            for atom in atoms {
                if !declared.contains(atom.as_str()) {
                    out.push(Violation::new(
                        "undeclared_capability_atom",
                        reg,
                        &clause.key,
                        format!(
                            "activation_predicate names capability atom {atom:?}, which is not \
                             in registry.capability_atoms. An undeclared atom is indistinguishable \
                             from a misspelled one: both evaluate false, so the clause is \
                             unreachable forever and no gate can say so"
                        ),
                    ));
                }
            }
        }
    }
}

/// Every atom a capability manifest enables must be declared in the same
/// vocabulary.
///
/// This is the other half of the typo class. A manifest naming
/// `mvcc-visibilty` silently enables nothing; the closure it produces is
/// smaller than the one the author believed they had asked for, and the gate
/// reports a pass over it.
pub fn validate_manifest_atoms(r: &Registries, manifest: &Manifest) -> Vec<Violation> {
    let declared: BTreeSet<&str> = r
        .invariants
        .capability_atoms
        .iter()
        .map(String::as_str)
        .collect();
    let mut out = Vec::new();
    for (field, atoms) in [
        ("features", &manifest.features),
        ("postures", &manifest.postures),
        ("roles", &manifest.roles),
    ] {
        for atom in atoms {
            if !declared.contains(atom.as_str()) {
                out.push(Violation::new(
                    "undeclared_manifest_atom",
                    "manifest",
                    &manifest.name,
                    format!(
                        "{field} names capability atom {atom:?}, which is not in \
                         invariants.toml registry.capability_atoms; it enables nothing, and a \
                         misspelling is indistinguishable from a capability that has not landed"
                    ),
                ));
            }
        }
    }
    out
}

/// Run every check. `root` is the repository root (artifact resolution).
pub fn validate_all(r: &Registries, root: &Path) -> Vec<Violation> {
    let mut out = Vec::new();
    // ONE prover for the whole sweep. Three registries now ask `liveness` the
    // same question — is this checker row live — and the prover exists to cache
    // the module-map read that answers it. Three provers would be three caches
    // of one fact, which is this bug family in its performance costume; it is
    // also why the licence below is consulted once rather than per registry.
    let prover = crate::liveness::Prover::new(root);
    // THE CONTROL, first, for the whole run. The liveness readers are
    // source-text readers, the layer where all four of this repository's "looks
    // exactly like a pass" tooling bugs lived. If they have stopped reading they
    // report "no defects" for every row, which is byte-identical to what a
    // healthy tree reports — so no clean verdict below is licensed until they
    // have answered a known question correctly.
    let self_test = crate::liveness::self_test();
    if !self_test.licensed() {
        out.push(Violation::new(
            "checker_liveness_self_test_failed",
            "checker_index",
            "<self-test>",
            format!(
                "the liveness readers got {} of {} known answers wrong ({}); refusing to \
                 report any checker row live, any proof lane checked, or any clause \
                 legally promoted",
                self_test.failures.len(),
                self_test.cases,
                self_test.failures.join(", ")
            ),
        ));
    }
    validate_constitution(r, &mut out);
    validate_invariants(r, &prover, &mut out);
    validate_enforcement_coverage(r, &prover, &mut out);
    validate_evidence(r, &mut out);
    validate_slo(r, &mut out);
    validate_proof_lanes(r, &prover, &mut out);
    validate_checker_index(r, &prover, &mut out);
    validate_script_closure(r, root, &mut out);
    validate_active_logical_kind_arms(root, &mut out);
    validate_capability_atoms(r, &mut out);
    out
}
