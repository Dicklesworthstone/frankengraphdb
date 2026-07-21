//! Activation-closure compilation (Appendix F preamble).
//!
//! Given a capability manifest, compute the set of clauses reachable under
//! it: a clause is reachable when its activation predicate evaluates true
//! over the manifest's enabled atoms, and reachability closes transitively
//! over clause dependencies (a dependency on a top-level FG-INV ID pulls in
//! all of that invariant's clauses). Every reachable clause must be live;
//! otherwise the corresponding capability is absent, and the report names
//! the exact clauses behind each absent capability.

use crate::model::{Manifest, Registries};
use crate::predicate;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq)]
pub struct ClosureReport {
    pub manifest: String,
    /// All clause keys reachable under the manifest.
    pub reachable: BTreeSet<String>,
    /// Reachable clauses with status = "live".
    pub live: BTreeSet<String>,
    /// Reachable clauses that are NOT live: each forces its capability off.
    pub absent: BTreeSet<String>,
    /// capability atom -> clause keys forcing it absent. Clauses whose
    /// predicate mentions no atom are attributed to "always".
    pub absent_capabilities: BTreeMap<String, BTreeSet<String>>,
}

impl ClosureReport {
    pub fn ok(&self) -> bool {
        self.absent.is_empty()
    }
}

pub fn compute(r: &Registries, manifest: &Manifest) -> ClosureReport {
    let enabled: BTreeSet<String> = manifest
        .features
        .iter()
        .chain(manifest.postures.iter())
        .chain(manifest.roles.iter())
        .cloned()
        .collect();

    // Index clauses and expand FG-INV dependency targets.
    let mut clause_status: BTreeMap<String, String> = BTreeMap::new();
    let mut clause_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut clause_pred: BTreeMap<String, String> = BTreeMap::new();
    let mut invariant_clauses: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for inv in &r.invariants.invariants {
        let keys: Vec<String> = inv.clauses.iter().map(|c| c.key.clone()).collect();
        invariant_clauses.insert(inv.id.clone(), keys);
        for c in &inv.clauses {
            clause_status.insert(c.key.clone(), c.status.clone());
            clause_deps.insert(c.key.clone(), c.dependencies.clone());
            clause_pred.insert(c.key.clone(), c.activation_predicate.clone());
        }
    }

    // Seed: clauses whose predicate evaluates true. An unparsable predicate
    // is treated as reachable (conservative: validation already reported it;
    // the closure must not silently drop the clause).
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for (key, pred_text) in &clause_pred {
        let active = match predicate::parse(pred_text) {
            Ok(expr) => predicate::eval(&expr, &enabled),
            Err(_) => true,
        };
        if active && reachable.insert(key.clone()) {
            queue.push_back(key.clone());
        }
    }
    // Transitive dependency closure.
    while let Some(key) = queue.pop_front() {
        let deps = clause_deps.get(&key).cloned().unwrap_or_default();
        for dep in deps {
            let targets: Vec<String> = if let Some(keys) = invariant_clauses.get(&dep) {
                keys.clone()
            } else {
                vec![dep]
            };
            for t in targets {
                if clause_status.contains_key(&t) && reachable.insert(t.clone()) {
                    queue.push_back(t);
                }
            }
        }
    }

    let mut live = BTreeSet::new();
    let mut absent = BTreeSet::new();
    let mut absent_capabilities: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for key in &reachable {
        if clause_status.get(key).map(String::as_str) == Some("live") {
            live.insert(key.clone());
        } else {
            absent.insert(key.clone());
            // Attribute to the capability atoms the clause's predicate names.
            let mut atoms = BTreeSet::new();
            if let Some(pred_text) = clause_pred.get(key)
                && let Ok(expr) = predicate::parse(pred_text)
            {
                predicate::atoms(&expr, &mut atoms);
            }
            if atoms.is_empty() {
                atoms.insert("always".to_string());
            }
            for atom in atoms {
                absent_capabilities
                    .entry(atom)
                    .or_default()
                    .insert(key.clone());
            }
        }
    }

    ClosureReport {
        manifest: manifest.name.clone(),
        reachable,
        live,
        absent,
        absent_capabilities,
    }
}
