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
    /// How many clauses the spine holds at all.
    pub spine_clauses: usize,
    /// How many clauses a manifest enabling EVERY atom the spine names would
    /// reach: the run's own non-vacuity control. See [`ClosureReport::licensed`].
    pub saturated_reachable: usize,
}

impl ClosureReport {
    pub fn ok(&self) -> bool {
        self.absent.is_empty()
    }

    /// Is a "closure satisfied" conclusion from this run LICENSED?
    ///
    /// The pre-Genesis manifest enables nothing, so the closure reaches zero
    /// clauses and every reachable clause is trivially live. Measured on the
    /// real tree: `clauses=20 reachable=0 live=0 absent=0 ok=true`, with 20 of
    /// 20 clauses non-live. The contract — Appendix F, "Every reachable clause
    /// must be live" — held only because nothing was reachable, and the run
    /// reported that as a plain pass.
    ///
    /// That is the looks-exactly-like-a-pass family: the same green bar would
    /// appear if the predicate evaluator had silently stopped matching, if the
    /// dependency closure had stopped expanding, or if the spine had been
    /// emptied. `unsafe_ledger` answers this with [`scanner_fixture`], whose
    /// known site count licenses every zero-site result in the same run; the
    /// closure had no analogue.
    ///
    /// This is that analogue, and it needs no fixture registry: saturate the
    /// manifest with every atom the spine's own predicates name and check that
    /// the machinery reaches something. A zero result then means "this manifest
    /// enables nothing", which is a fact about the manifest — not "this
    /// compiler reaches nothing", which is a broken checker.
    ///
    /// [`scanner_fixture`]: crate::unsafe_ledger::scanner_fixture
    pub fn licensed(&self) -> bool {
        self.spine_clauses == 0 || self.saturated_reachable > 0
    }
}

/// Every capability atom the spine's activation predicates name.
///
/// An unparsable predicate contributes nothing here; `validate` reports it
/// separately, and [`compute`] already treats it as reachable.
pub fn spine_atoms(r: &Registries) -> BTreeSet<String> {
    let mut atoms = BTreeSet::new();
    for inv in &r.invariants.invariants {
        for c in &inv.clauses {
            if let Ok(expr) = predicate::parse(&c.activation_predicate) {
                predicate::atoms(&expr, &mut atoms);
            }
        }
    }
    atoms
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

    let reachable = reachable_under(
        &clause_pred,
        &clause_deps,
        &clause_status,
        &invariant_clauses,
        &enabled,
    );
    // The run's own control, computed from the same machinery over the same
    // spine: what a manifest enabling EVERYTHING would reach. Without it a
    // reachable set of zero is indistinguishable from a closure compiler that
    // has stopped working. See `ClosureReport::licensed`.
    let saturated_reachable = reachable_under(
        &clause_pred,
        &clause_deps,
        &clause_status,
        &invariant_clauses,
        &spine_atoms(r),
    )
    .len();

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
        spine_clauses: clause_status.len(),
        saturated_reachable,
    }
}

/// The reachable set under one set of enabled atoms: seed by predicate, then
/// close transitively over clause dependencies.
///
/// Factored out so the control in [`compute`] runs the SAME walk over the SAME
/// spine as the verdict it licenses. A control computed by a second
/// implementation would only prove that the second implementation works.
fn reachable_under(
    clause_pred: &BTreeMap<String, String>,
    clause_deps: &BTreeMap<String, Vec<String>>,
    clause_status: &BTreeMap<String, String>,
    invariant_clauses: &BTreeMap<String, Vec<String>>,
    enabled: &BTreeSet<String>,
) -> BTreeSet<String> {
    // Seed: clauses whose predicate evaluates true. An unparsable predicate
    // is treated as reachable (conservative: validation already reported it;
    // the closure must not silently drop the clause).
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for (key, pred_text) in clause_pred {
        let active = match predicate::parse(pred_text) {
            Ok(expr) => predicate::eval(&expr, enabled),
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
    reachable
}
