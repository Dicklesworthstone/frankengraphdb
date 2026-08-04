//! **Model-generated histories, differentially tested against the oracle** (§15).
//!
//! The plan names this and nothing implemented it: "model-generated histories run
//! against both engines and compare snapshots, results, certificates, and permitted
//! abort outcomes". Its sibling file [`strata_oracle_differential`] runs the same
//! differential over HAND-WRITTEN fixtures. Those fixtures are good — they caught the
//! supersede-per-version defect, the forced-seal constraint and the cascade gap — but
//! a human writes a dozen, and the space is MVCC × parallel edges × cascades × block
//! cuts × sequence batching. Hand-written cases cannot cover that and everyone knows it.
//!
//! **THE GENERATOR IS INTENT-LEVEL, NOT EFFECT-LEVEL**, and this is the design
//! decision the whole file rests on. It generates *operations against live model
//! state* — never `DeltaRow`s or `AdjacencyEntry`s directly. An effect-level
//! generator spends most of its output on before-images no state ever had, so most
//! of its failures are unreachable-history false positives, and a tool whose
//! failures are usually spurious gets switched off. Every history this file emits is
//! one the system could actually reach.
//!
//! **VALIDITY IS MAINTAINED, NOT FILTERED.** The generator only ever proposes a step
//! that its model says is legal right now: an edge is added between two LIVE
//! vertices, a delete names a LIVE element, and no identity is ever reused — because
//! identities are permanently spent (plan §4.5, enforced since `fgdb-s50d`). Rejects
//! are therefore evidence of a GENERATOR defect, and [`common::try_build`] returns
//! them as values so the test can say so instead of dying in a stack trace.
//!
//! **SHRINKING IS NOT OPTIONAL.** A failing 200-step history is a curiosity; a
//! shrunk 3-step one is a bug report. Without shrinking this tool would be built,
//! would find something once, and would be abandoned.
//!
//! Branches are generated below by a **separate action language and model**.
//! A fork changes the history topology, so treating it as another row-shaped
//! `Step` would hide the parent boundary and make branch-aware shrinking
//! impossible. The branch differential independently folds a forest, then checks
//! current state, historical snapshots, properties, the enacted single-period
//! valid-time subset and its selector boundaries, origins, frontiers, and
//! inherited conflict windows against `ReferenceDatabase` after every action.

mod common;

use asupersync::lab::explorer::{DporExplorer, ExplorerConfig};
use asupersync::runtime::yield_now;
use asupersync::sync::Mutex as AsyncMutex;
use asupersync::{Budget, Cx, LabRuntime};
use common::{BRANCH, GRAPH, LABEL, PROP, REL, Step, check_agrees, try_build};
use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, ElementId, LogicalDeltaTemplate, PropertyKeyId, SchemaEpoch,
    ValidTimePeriod,
};
use fgdb_reference::intents::{Intent, MismatchPolicy, Statement};
use fgdb_reference::ssi::{DangerousStructure, TxnTrace, dangerous_structures};
use fgdb_reference::txn::{Transaction, TxnOutcome};
use fgdb_reference::{BranchOrigin, ConflictKey, ReferenceDatabase, ReferenceGraph};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, LogicalCommandSeq, ObjectId, VId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// A deterministic PRNG, in-house because the dependency universe is closed.
// ---------------------------------------------------------------------------

/// SplitMix64 (Steele–Lea–Flood 2014), the standard seeding generator.
///
/// Chosen because it is a *fixed* algorithm rather than a library detail: the
/// seed printed by a failure has to mean the same thing next week, and it has to
/// mean the same thing on a different machine. A generator whose stream can drift
/// under us turns every reported seed into a lie.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform over `0..n`. Modulo bias is irrelevant here — `n` is always tiny
    /// (a weight table or a small index) and the bias is far below anything that
    /// could shift which shapes get produced.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

// ---------------------------------------------------------------------------
// The model the generator maintains so it can only propose reachable steps.
// ---------------------------------------------------------------------------

/// Which interesting shapes a generated history actually contained.
///
/// **A GENERATOR THAT NEVER PRODUCES SHAPE X CANNOT TEST SHAPE X**, and it will
/// look exactly as green as one that does. So the shapes that broke things before
/// are counted, and a run that never reaches one of them fails as a COVERAGE bug
/// in the generator rather than passing quietly.
#[derive(Default, Debug, Clone, Copy)]
struct Coverage {
    parallel_edges: usize,
    edge_deletes: usize,
    vertex_cascades: usize,
    cascades_with_edges: usize,
    self_loops: usize,
    batched_commits: usize,
    multi_block: usize,
}

impl Coverage {
    fn merge(&mut self, other: &Coverage) {
        self.parallel_edges += other.parallel_edges;
        self.edge_deletes += other.edge_deletes;
        self.vertex_cascades += other.vertex_cascades;
        self.cascades_with_edges += other.cascades_with_edges;
        self.self_loops += other.self_loops;
        self.batched_commits += other.batched_commits;
        self.multi_block += other.multi_block;
    }
}

struct Model {
    live_vertices: Vec<u128>,
    /// `(eid, src, dst)` for every live edge.
    live_edges: Vec<(u128, u128, u128)>,
    /// Every vertex ever born, so the sweep can also assert that a RETIRED
    /// vertex answers empty rather than being quietly dropped from the check.
    all_vertices: Vec<u128>,
    /// Monotone identity minting. Never reused: a retired identity is spent
    /// forever, so reuse would be an unreachable history, not a hard test case.
    next_vid: u128,
    next_eid: u128,
}

impl Model {
    fn new() -> Self {
        Self {
            live_vertices: Vec::new(),
            live_edges: Vec::new(),
            all_vertices: Vec::new(),
            next_vid: 1,
            next_eid: 1_000,
        }
    }
}

/// One generated history plus the seed that produced it.
struct Generated {
    history: Vec<(u64, Step)>,
    seal_after: Vec<usize>,
    sources: Vec<u128>,
    last: u64,
    coverage: Coverage,
}

/// Generate one valid history.
///
/// The weights are deliberately skewed toward the shapes that have actually broken
/// this code — parallel edges between an already-used pair (the `fgdb-0trr` defect
/// class) and vertex cascades (which a mutation proved the fixtures were missing) —
/// rather than being uniform. A uniform generator spends its budget on the easy
/// middle of the space.
fn generate(seed: u64, steps: usize) -> Generated {
    let mut rng = SplitMix64(seed);
    let mut model = Model::new();
    let mut history: Vec<(u64, Step)> = Vec::new();
    let mut seal_after = Vec::new();
    let mut coverage = Coverage::default();
    let mut seq: u64 = 1;
    let mut pending_cut = false;

    for _ in 0..steps {
        if pending_cut {
            seal_after.push(history.len().saturating_sub(1));
            pending_cut = false;
        }
        // Sequence batching: several steps sharing one commit sequence is the
        // case where "same-version replace" logic lives, and it is where the
        // parallel-edge collapse in fgdb-0trr actually happened.
        if rng.chance(60) {
            seq += 1;
        } else if !history.is_empty() {
            coverage.batched_commits += 1;
        }

        let step = loop {
            // Weighted choice, re-rolled when the model says the pick is not
            // currently legal. Re-rolling rather than filtering keeps the
            // distribution readable: each arm states its own precondition.
            match rng.below(100) {
                // Create a vertex. Always legal.
                0..=29 => {
                    let vid = model.next_vid;
                    model.next_vid += 1;
                    model.live_vertices.push(vid);
                    model.all_vertices.push(vid);
                    break Step::CreateVertex(vid);
                }
                // Add an edge between two live vertices.
                30..=74 => {
                    if model.live_vertices.is_empty() {
                        continue;
                    }
                    let eid = model.next_eid;
                    let (src, dst) = if rng.chance(45) && !model.live_edges.is_empty() {
                        // DELIBERATE PARALLEL EDGE: reuse an existing (src, dst)
                        // with a fresh EId. This is the exact shape whose identity
                        // Strata used to drop at the format boundary.
                        let (_, s, d) = model.live_edges[rng.below(model.live_edges.len())];
                        coverage.parallel_edges += 1;
                        (s, d)
                    } else {
                        let s = model.live_vertices[rng.below(model.live_vertices.len())];
                        let d = model.live_vertices[rng.below(model.live_vertices.len())];
                        if s == d {
                            coverage.self_loops += 1;
                        }
                        (s, d)
                    };
                    model.next_eid += 1;
                    model.live_edges.push((eid, src, dst));
                    break Step::AddEdge { eid, src, dst };
                }
                // Delete a live edge.
                75..=89 => {
                    if model.live_edges.is_empty() {
                        continue;
                    }
                    let index = rng.below(model.live_edges.len());
                    let (eid, _, _) = model.live_edges.remove(index);
                    coverage.edge_deletes += 1;
                    break Step::DeleteEdge(eid);
                }
                // Delete a live vertex, cascading its incident edges.
                _ => {
                    if model.live_vertices.is_empty() {
                        continue;
                    }
                    let index = rng.below(model.live_vertices.len());
                    let vid = model.live_vertices.remove(index);
                    let before = model.live_edges.len();
                    // MIRROR THE CASCADE IN THE MODEL. If the model kept an edge
                    // the oracle just retired, every later step naming that edge
                    // would be an unreachable history and the generator would be
                    // manufacturing its own false failures.
                    model.live_edges.retain(|(_, s, d)| *s != vid && *d != vid);
                    coverage.vertex_cascades += 1;
                    if model.live_edges.len() != before {
                        coverage.cascades_with_edges += 1;
                    }
                    break Step::DeleteVertex(vid);
                }
            }
        };

        history.push((seq, step));
        // Decide the cut for the NEXT iteration, so the index recorded is the one
        // just pushed rather than one that does not exist yet.
        if rng.chance(18) {
            pending_cut = true;
        }
    }

    // BLOCK CUTS LAND ONLY ON COMMIT BOUNDARIES, and this is a correctness
    // constraint on the generator rather than a stylistic one.
    //
    // FOUND BY THIS GENERATOR ON ITS FIRST RUN (seed 523): cutting a block
    // BETWEEN an edge's creation and its deletion within ONE commit sequence made
    // the writer refuse the history with `RetiredBeforeCreated { created_at: 3,
    // retired_at: 3 }`. That refusal is CORRECT and documented — the same-commit
    // fold applies "only while the creation is still pending", and once the
    // creation has sealed the empty interval can no longer be folded away, so the
    // format's typed refusal is the honest answer to a pathological stream.
    //
    // But a commit is atomic, so a caller cutting a block in the middle of one is
    // not modelling anything a caller does; the writer reaches that state only via
    // its own 16M-entry ceiling. Generating it would spend the budget re-proving a
    // documented refusal while manufacturing "failures" that indict nothing.
    let seal_after = seal_after
        .into_iter()
        .filter(|&index| {
            history
                .get(index + 1)
                .is_none_or(|(next_seq, _)| *next_seq != history[index].0)
        })
        .collect::<Vec<_>>();
    coverage.multi_block += seal_after.len();

    Generated {
        history,
        seal_after,
        sources: model.all_vertices,
        last: seq,
        coverage,
    }
}

// ---------------------------------------------------------------------------
// Running one candidate, and shrinking a failure.
// ---------------------------------------------------------------------------

/// Run one history end to end. `Ok(())` means the two sides agreed everywhere.
///
/// A history the harness REFUSES is reported as a distinct error string rather
/// than being silently treated as a pass. Both are failures of this test, but they
/// are failures of different things — a refusal indicts the generator, a
/// disagreement indicts the engine — and collapsing them would let a generator
/// that proposes nothing legal look perfectly healthy.
fn run(case: &Generated) -> Result<(), String> {
    let (graph, blocks) = try_build(&case.history, &case.seal_after)?;
    check_agrees(&graph, &blocks, &case.sources, case.last)
}

/// Classify a failure so the shrinker can tell "still the same bug" from
/// "some other bug".
///
/// The step index is stripped, because removing a step renumbers the ones after
/// it: the SAME defect legitimately reports a different index in a shorter
/// history, and comparing the raw strings would make the shrinker refuse every
/// removal and silently return its input.
fn failure_kind(error: &str) -> &str {
    match error.find(": ") {
        Some(cut) if error.starts_with("step ") => &error[cut + 2..],
        _ => error,
    }
}

/// Greedy delta-debugging: drop one step at a time, keep the drop when THE SAME
/// failure survives.
///
/// Runs back to front so that removing a step never renumbers one still to be
/// tried. `seal_after` indices are remapped rather than dropped, because a block
/// cut is frequently the thing that makes a case fail at all — shrinking away the
/// cut would "fix" the counterexample and report a minimal history that passes.
///
/// **"THE SAME" IS LOAD-BEARING, AND THIS WAS A REAL BUG HERE.** The first version
/// accepted any `Err`, so on seed 27 it shrank a genuine disagreement down to
/// `[DeleteEdge(1000)]` — a one-step history that fails merely because it deletes
/// an edge that was never created. That is a perfect minimal counterexample to a
/// question nobody asked. A shrinker that may change the failure is worse than no
/// shrinker: it reports with full confidence, and it points somewhere else.
fn shrink(case: &Generated) -> Generated {
    let Err(original) = run(case) else {
        return Generated {
            sources: case.sources.clone(),
            last: case.last,
            history: case.history.clone(),
            seal_after: case.seal_after.clone(),
            coverage: Coverage::default(),
        };
    };
    let target = failure_kind(&original).to_string();
    shrink_preserving(case, &target)
}

fn shrink_preserving(case: &Generated, target: &str) -> Generated {
    let mut best_history = case.history.clone();
    let mut best_seals = case.seal_after.clone();

    let mut index = best_history.len();
    while index > 0 {
        index -= 1;

        let mut candidate_history = best_history.clone();
        candidate_history.remove(index);
        let candidate_seals: Vec<usize> = best_seals
            .iter()
            .filter(|&&s| s != index)
            .map(|&s| if s > index { s - 1 } else { s })
            .collect();

        // Re-derive the sweep inputs from the SMALLER history rather than reusing
        // the original's: a vertex whose creation was just removed must not stay
        // in the source list, or the shrunk case fails for a different reason
        // than the original did and the report points at the wrong thing.
        let candidate = Generated {
            sources: sources_of(&candidate_history),
            last: candidate_history.iter().map(|(s, _)| *s).max().unwrap_or(1),
            history: candidate_history,
            seal_after: candidate_seals,
            coverage: Coverage::default(),
        };

        // Keep the removal ONLY when the same defect still fires.
        if run(&candidate).is_err_and(|e| failure_kind(&e) == target) {
            best_history = candidate.history;
            best_seals = candidate.seal_after;
        }
    }

    Generated {
        sources: sources_of(&best_history),
        last: best_history.iter().map(|(s, _)| *s).max().unwrap_or(1),
        history: best_history,
        seal_after: best_seals,
        coverage: Coverage::default(),
    }
}

fn sources_of(history: &[(u64, Step)]) -> Vec<u128> {
    history
        .iter()
        .filter_map(|(_, step)| match step {
            Step::CreateVertex(vid) => Some(*vid),
            _ => None,
        })
        .collect()
}

/// Print the shrunk counterexample AS A COMPILABLE TEST CASE.
///
/// This is what converts a fuzz finding into a permanent law. A failure that
/// prints only a seed requires the reader to re-run the generator and trust it did
/// not change; a failure that prints the history can be pasted straight into
/// `strata_oracle_differential.rs` and kept forever.
fn report(seed: u64, case: &Generated, error: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n=== GENERATED DIFFERENTIAL FAILURE ===\nseed: {seed}\nerror: {error}\n\n\
         Paste this into strata_oracle_differential.rs as a permanent law:\n\n\
         #[test]\n\
         fn regression_seed_{seed}() {{\n    let history = [\n"
    ));
    for (seq, step) in &case.history {
        let rendered = match step {
            Step::CreateVertex(vid) => format!("Step::CreateVertex({vid})"),
            Step::AddEdge { eid, src, dst } => {
                format!("Step::AddEdge {{ eid: {eid}, src: {src}, dst: {dst} }}")
            }
            Step::DeleteEdge(eid) => format!("Step::DeleteEdge({eid})"),
            Step::DeleteVertex(vid) => format!("Step::DeleteVertex({vid})"),
        };
        out.push_str(&format!("        ({seq}u64, {rendered}),\n"));
    }
    out.push_str(&format!(
        "    ];\n    let (graph, blocks) = build(&history, &{:?});\n    \
         assert_agrees(&graph, &blocks, &{:?}, {});\n}}\n",
        case.seal_after, case.sources, case.last
    ));
    out
}

// ---------------------------------------------------------------------------
// The laws.
// ---------------------------------------------------------------------------

/// THE MAIN EVENT: many generated histories, every one of which must agree.
#[test]
fn generated_histories_agree_with_the_oracle() -> Result<(), String> {
    let mut coverage = Coverage::default();

    for seed in 0..240u64 {
        let case = generate(seed, 24);
        coverage.merge(&case.coverage);

        if let Err(original) = run(&case) {
            let minimal = shrink(&case);
            // Report the SHRUNK failure, but assert it is still the original one.
            // If shrinking ever changes the defect, the pasteable regression test
            // below would encode the wrong bug — so that possibility dies here
            // rather than in the reader's head.
            let minimal_error = run(&minimal)
                .err()
                .unwrap_or_else(|| "shrunk case no longer fails".to_string());
            assert_eq!(
                failure_kind(&minimal_error),
                failure_kind(&original),
                "seed {seed}: shrinking changed the defect ({original} -> {minimal_error})"
            );
            return Err(report(seed, &minimal, &minimal_error));
        }
    }

    // COVERAGE IS ASSERTED, NOT HOPED FOR. Each of these is a shape that has
    // actually broken this code or that no hand-written fixture reaches. A run
    // that produced none of one of them proves nothing about it, and this
    // assertion is what stops the generator quietly drifting into a tame
    // distribution as the weights get tuned.
    assert!(
        coverage.parallel_edges > 0,
        "generator never produced a parallel edge — the fgdb-0trr shape is untested: {coverage:?}"
    );
    assert!(
        coverage.cascades_with_edges > 0,
        "generator never cascaded a vertex that actually had edges: {coverage:?}"
    );
    assert!(
        coverage.edge_deletes > 0,
        "generator never deleted an edge: {coverage:?}"
    );
    assert!(
        coverage.batched_commits > 0,
        "generator never put two steps in one commit sequence: {coverage:?}"
    );
    assert!(
        coverage.multi_block > 0,
        "generator never cut a block, so every history fit in one: {coverage:?}"
    );
    assert!(
        coverage.self_loops > 0,
        "generator never produced a self-loop: {coverage:?}"
    );
    Ok(())
}

/// The generator must produce histories the harness ACCEPTS.
///
/// Separated from the law above on purpose. If the generator degraded into
/// proposing unreachable steps, `run` would return `Err` and the main test would
/// report a spurious engine disagreement — blaming Strata for a defect in this
/// file. This law names the real culprit.
#[test]
fn every_generated_history_is_reachable() -> Result<(), String> {
    for seed in 500..560u64 {
        let case = generate(seed, 20);
        if let Err(error) = try_build(&case.history, &case.seal_after) {
            return Err(format!(
                "seed {seed} produced an UNREACHABLE history — this is a generator defect, \
                 not an engine defect: {error}\n{:#?}",
                case.history
            ));
        }
    }
    Ok(())
}

/// The generator is DETERMINISTIC: one seed, one history, forever.
///
/// Without this, a reported seed is worthless — the whole reporting story assumes
/// re-running a seed reproduces the failure, and that assumption deserves a law
/// rather than a comment.
#[test]
fn a_seed_reproduces_its_history_exactly() {
    for seed in [7u64, 99, 4242] {
        let first = generate(seed, 30);
        let second = generate(seed, 30);
        assert_eq!(first.history, second.history, "seed {seed} drifted");
        assert_eq!(
            first.seal_after, second.seal_after,
            "seed {seed} seals drifted"
        );
    }
}

/// The shrinker must actually shrink, and must preserve the failure.
///
/// **THE CONTROL THIS TEST EXISTS FOR.** A shrinker that returns its input
/// unchanged passes any "the shrunk case still fails" check trivially. So this
/// drives shrinking with an injected, genuinely failing case and asserts BOTH that
/// the failure survives AND that the result got strictly smaller.
#[test]
fn shrinking_preserves_the_failure_and_reduces_the_history() {
    // A history that fails for a reason the harness itself reports: deleting an
    // edge that was never created is unreachable, so `try_build` refuses it.
    // Padding it with unrelated valid steps gives the shrinker something to remove.
    let mut history = vec![];
    for vid in 1..=6u128 {
        history.push((1u64, Step::CreateVertex(vid)));
    }
    history.push((2, Step::DeleteEdge(9_999)));
    for vid in 7..=12u128 {
        history.push((3u64, Step::CreateVertex(vid)));
    }

    let case = Generated {
        sources: sources_of(&history),
        last: 3,
        history,
        seal_after: vec![2, 5],
        coverage: Coverage::default(),
    };
    assert!(
        run(&case).is_err(),
        "the seeded case must fail to start with"
    );

    let original = run(&case).expect_err("the seeded case must fail");
    let minimal = shrink(&case);
    let shrunk = run(&minimal).expect_err("shrinking destroyed the failure it must preserve");
    // THE SAME failure, not merely SOME failure. Asserting only `is_err()` here
    // would have passed against the bug this control exists to catch.
    assert_eq!(
        failure_kind(&shrunk),
        failure_kind(&original),
        "shrinker changed which defect fires: {original} -> {shrunk}"
    );
    assert!(
        minimal.history.len() < case.history.len(),
        "shrinker returned {} steps from {} — it removed nothing",
        minimal.history.len(),
        case.history.len()
    );
    // The offending step must survive: it IS the failure.
    assert!(
        minimal
            .history
            .iter()
            .any(|(_, s)| matches!(s, Step::DeleteEdge(9_999))),
        "shrinker removed the very step that causes the failure: {:?}",
        minimal.history
    );
}

// ---------------------------------------------------------------------------
// A separate generator for branch forests.
// ---------------------------------------------------------------------------

/// Branching changes the shape of the generated object, not merely the set of
/// row operations. Keeping it in a separate language prevents a fork from being
/// mistaken for one more [`Step`] that a single-coordinate harness can apply.
/// `SetValidTime` deliberately models the current `Option<ValidTimePeriod>` row
/// contract; it is evidence for that enacted subset, not the plan's eventual
/// normalized multi-slice representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BranchStep {
    Graph(Step),
    SetProperty {
        elem: ElementId,
        after: Option<i64>,
    },
    SetValidTime {
        elem: ElementId,
        after: Option<ValidTimePeriod>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BranchAction {
    Write {
        logical_seq: u64,
        branch: u128,
        step: BranchStep,
    },
    Fork {
        parent: u128,
        child: u128,
        /// An observed logical-command position, or zero for the empty prefix.
        boundary: u64,
    },
}

#[derive(Clone, Debug)]
struct BranchGenerated {
    actions: Vec<BranchAction>,
    coverage: BranchCoverage,
}

/// Coverage over the dimensions that make a branch generator more than a
/// collection of single-branch histories.
#[derive(Clone, Copy, Debug, Default)]
struct BranchCoverage {
    current_forks: usize,
    historical_forks: usize,
    zero_forks: usize,
    nested_forks: usize,
    fork_branch_writes: usize,
    inherited_mutations: usize,
    divergent_siblings: usize,
    inherited_conflict_windows: usize,
    property_sets: usize,
    property_removals: usize,
    vertex_property_mutations: usize,
    edge_property_mutations: usize,
    inherited_property_mutations: usize,
    valid_time_sets: usize,
    valid_time_clears: usize,
    bounded_valid_times: usize,
    open_valid_times: usize,
    zero_length_valid_times: usize,
    vertex_valid_time_mutations: usize,
    edge_valid_time_mutations: usize,
    inherited_valid_time_mutations: usize,
}

impl BranchCoverage {
    fn merge(&mut self, other: &Self) {
        self.current_forks += other.current_forks;
        self.historical_forks += other.historical_forks;
        self.zero_forks += other.zero_forks;
        self.nested_forks += other.nested_forks;
        self.fork_branch_writes += other.fork_branch_writes;
        self.inherited_mutations += other.inherited_mutations;
        self.divergent_siblings += other.divergent_siblings;
        self.inherited_conflict_windows += other.inherited_conflict_windows;
        self.property_sets += other.property_sets;
        self.property_removals += other.property_removals;
        self.vertex_property_mutations += other.vertex_property_mutations;
        self.edge_property_mutations += other.edge_property_mutations;
        self.inherited_property_mutations += other.inherited_property_mutations;
        self.valid_time_sets += other.valid_time_sets;
        self.valid_time_clears += other.valid_time_clears;
        self.bounded_valid_times += other.bounded_valid_times;
        self.open_valid_times += other.open_valid_times;
        self.zero_length_valid_times += other.zero_length_valid_times;
        self.vertex_valid_time_mutations += other.vertex_valid_time_mutations;
        self.edge_valid_time_mutations += other.edge_valid_time_mutations;
        self.inherited_valid_time_mutations += other.inherited_valid_time_mutations;
    }
}

/// A deliberately plain graph used only by the branch oracle below.
///
/// It knows logical payloads, but no MVCC representation, version digest,
/// branch metadata, or `ReferenceGraph` implementation detail. That is enough
/// to answer what each branch should contain after recursively applying its
/// ancestor prefix and own commits.
#[derive(Clone, Debug, PartialEq, Eq)]
struct NaiveVertex {
    birth_ordinal: u64,
    props: BTreeMap<PropertyKeyId, CanonicalScalar>,
    valid_time: Option<ValidTimePeriod>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NaiveEdge {
    birth_ordinal: u64,
    src: u128,
    dst: u128,
    props: BTreeMap<PropertyKeyId, CanonicalScalar>,
    valid_time: Option<ValidTimePeriod>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NaiveGraph {
    vertices: BTreeMap<u128, NaiveVertex>,
    edges: BTreeMap<u128, NaiveEdge>,
}

impl NaiveGraph {
    fn apply(&mut self, step: BranchStep) -> Result<BTreeSet<ConflictKey>, String> {
        let mut conflicts = BTreeSet::new();
        match step {
            BranchStep::Graph(Step::CreateVertex(vid)) => {
                let birth_ordinal = u64::try_from(vid)
                    .map_err(|_| format!("vertex identity {vid} exceeds the birth domain"))?;
                let value = i64::try_from(vid)
                    .map_err(|_| format!("vertex identity {vid} exceeds the scalar domain"))?;
                if self
                    .vertices
                    .insert(
                        vid,
                        NaiveVertex {
                            birth_ordinal,
                            props: BTreeMap::from([(PROP, CanonicalScalar::Int(value))]),
                            valid_time: None,
                        },
                    )
                    .is_some()
                {
                    return Err(format!("CreateVertex({vid}) reuses a live identity"));
                }
                conflicts.insert(ConflictKey::Element(ElementId::Vertex(VId(vid))));
            }
            BranchStep::Graph(Step::AddEdge { eid, src, dst }) => {
                if !self.vertices.contains_key(&src) || !self.vertices.contains_key(&dst) {
                    return Err(format!(
                        "AddEdge({eid}) names a dead endpoint ({src}, {dst})"
                    ));
                }
                let birth_ordinal = u64::try_from(eid)
                    .map_err(|_| format!("edge identity {eid} exceeds the birth domain"))?;
                if self
                    .edges
                    .insert(
                        eid,
                        NaiveEdge {
                            birth_ordinal,
                            src,
                            dst,
                            props: BTreeMap::new(),
                            valid_time: None,
                        },
                    )
                    .is_some()
                {
                    return Err(format!("AddEdge({eid}) reuses a live identity"));
                }
                conflicts.insert(ConflictKey::Element(ElementId::Edge(EId(eid))));
            }
            BranchStep::Graph(Step::DeleteEdge(eid)) => {
                if self.edges.remove(&eid).is_none() {
                    return Err(format!("DeleteEdge({eid}) names a dead edge"));
                }
                conflicts.insert(ConflictKey::Element(ElementId::Edge(EId(eid))));
            }
            BranchStep::Graph(Step::DeleteVertex(vid)) => {
                if self.vertices.remove(&vid).is_none() {
                    return Err(format!("DeleteVertex({vid}) names a dead vertex"));
                }
                conflicts.insert(ConflictKey::Element(ElementId::Vertex(VId(vid))));
                let retired = self
                    .edges
                    .iter()
                    .filter_map(|(eid, edge)| (edge.src == vid || edge.dst == vid).then_some(*eid))
                    .collect::<Vec<_>>();
                for eid in retired {
                    self.edges.remove(&eid);
                    conflicts.insert(ConflictKey::Element(ElementId::Edge(EId(eid))));
                }
            }
            BranchStep::SetProperty { elem, after } => {
                let props = match elem {
                    ElementId::Vertex(vid) => {
                        &mut self
                            .vertices
                            .get_mut(&vid.0)
                            .ok_or_else(|| format!("SetProperty names dead vertex {}", vid.0))?
                            .props
                    }
                    ElementId::Edge(eid) => {
                        &mut self
                            .edges
                            .get_mut(&eid.0)
                            .ok_or_else(|| format!("SetProperty names dead edge {}", eid.0))?
                            .props
                    }
                };
                if let Some(value) = after {
                    props.insert(PROP, CanonicalScalar::Int(value));
                } else {
                    props.remove(&PROP);
                }
                conflicts.insert(ConflictKey::Element(elem));
            }
            BranchStep::SetValidTime { elem, after } => {
                if let Some(period) = after
                    && period
                        .end_micros
                        .is_some_and(|end| period.start_micros > end)
                {
                    return Err(format!("SetValidTime names an inverted period {period:?}"));
                }
                let valid_time = match elem {
                    ElementId::Vertex(vid) => {
                        &mut self
                            .vertices
                            .get_mut(&vid.0)
                            .ok_or_else(|| format!("SetValidTime names dead vertex {}", vid.0))?
                            .valid_time
                    }
                    ElementId::Edge(eid) => {
                        &mut self
                            .edges
                            .get_mut(&eid.0)
                            .ok_or_else(|| format!("SetValidTime names dead edge {}", eid.0))?
                            .valid_time
                    }
                };
                *valid_time = after;
                conflicts.insert(ConflictKey::Element(elem));
            }
        }
        Ok(conflicts)
    }

    fn live_elements(&self) -> Vec<ElementId> {
        self.vertices
            .keys()
            .map(|vid| ElementId::Vertex(VId(*vid)))
            .chain(self.edges.keys().map(|eid| ElementId::Edge(EId(*eid))))
            .collect()
    }

    fn property(&self, elem: ElementId) -> Option<&CanonicalScalar> {
        match elem {
            ElementId::Vertex(vid) => self.vertices.get(&vid.0)?.props.get(&PROP),
            ElementId::Edge(eid) => self.edges.get(&eid.0)?.props.get(&PROP),
        }
    }

    fn valid_time(&self, elem: ElementId) -> Option<ValidTimePeriod> {
        match elem {
            ElementId::Vertex(vid) => self.vertices.get(&vid.0)?.valid_time,
            ElementId::Edge(eid) => self.edges.get(&eid.0)?.valid_time,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NaiveOrigin {
    Genesis,
    Fork { parent: u128, boundary: u64 },
}

#[derive(Clone, Debug)]
struct NaiveCommit {
    commit_seq: u64,
    logical_seq: u64,
    step: BranchStep,
    conflicts: BTreeSet<ConflictKey>,
}

#[derive(Clone, Debug)]
struct NaiveBranch {
    origin: NaiveOrigin,
    commits: Vec<NaiveCommit>,
}

/// Independent, intentionally slow branch semantics.
///
/// Every read recursively folds the parent at the fork boundary and then this
/// branch's own records. There is no cached materialized branch state, so a bug
/// in `ReferenceDatabase`'s eager coordinate map cannot make both sides agree.
#[derive(Clone, Debug, Default)]
struct NaiveBranchDatabase {
    branches: BTreeMap<u128, NaiveBranch>,
    /// Global commit sequence paired with its independently advancing logical
    /// command position. Index `n - 1` is commit `n`.
    positions: Vec<u64>,
}

impl NaiveBranchDatabase {
    fn logical_frontier(&self) -> u64 {
        self.positions.last().copied().unwrap_or(0)
    }

    fn logical_for_commit(&self, commit_seq: u64) -> Option<u64> {
        if commit_seq == 0 {
            Some(0)
        } else {
            let index = usize::try_from(commit_seq - 1).ok()?;
            self.positions.get(index).copied()
        }
    }

    fn commit_frontier_at(&self, boundary: u64) -> u64 {
        self.positions
            .iter()
            .rposition(|logical| *logical <= boundary)
            .map_or(0, |index| {
                u64::try_from(index + 1).expect("model position count was admitted as u64")
            })
    }

    fn observed_boundaries(&self) -> Vec<u64> {
        let mut boundaries = Vec::with_capacity(self.positions.len() + 1);
        boundaries.push(0);
        boundaries.extend(self.positions.iter().copied());
        boundaries
    }

    fn branch_ids(&self) -> Vec<u128> {
        self.branches.keys().copied().collect()
    }

    fn branch_depth(&self, branch: u128) -> Result<usize, String> {
        let mut current = branch;
        let mut seen = BTreeSet::new();
        let mut depth = 0usize;
        loop {
            if !seen.insert(current) {
                return Err(format!("branch lineage cycles at {current}"));
            }
            let record = self
                .branches
                .get(&current)
                .ok_or_else(|| format!("branch {current} does not exist"))?;
            match record.origin {
                NaiveOrigin::Genesis => return Ok(depth),
                NaiveOrigin::Fork { parent, .. } => {
                    current = parent;
                    depth += 1;
                }
            }
        }
    }

    fn fork(&mut self, parent: u128, child: u128, boundary: u64) -> Result<(), String> {
        if parent == child {
            return Err(format!("branch {child} cannot fork from itself"));
        }
        if !self.branches.contains_key(&parent) {
            return Err(format!("parent branch {parent} does not exist"));
        }
        if self.branches.contains_key(&child) {
            return Err(format!("child branch {child} already exists"));
        }
        if boundary > self.logical_frontier() {
            return Err(format!(
                "fork boundary {boundary} exceeds logical frontier {}",
                self.logical_frontier()
            ));
        }
        if boundary != 0 && !self.positions.contains(&boundary) {
            return Err(format!("fork boundary {boundary} was never observed"));
        }
        // Materialize once as a validity check. The result is deliberately not
        // stored: every model read below must walk lineage afresh.
        self.materialize(parent, boundary)?;
        self.branches.insert(
            child,
            NaiveBranch {
                origin: NaiveOrigin::Fork { parent, boundary },
                commits: Vec::new(),
            },
        );
        Ok(())
    }

    fn apply_write(
        &mut self,
        branch: u128,
        logical_seq: u64,
        step: BranchStep,
    ) -> Result<u64, String> {
        if logical_seq <= self.logical_frontier() {
            return Err(format!(
                "logical sequence {logical_seq} does not advance {}",
                self.logical_frontier()
            ));
        }
        let mut state = if self.branches.contains_key(&branch) {
            self.materialize(branch, self.logical_frontier())?
        } else {
            NaiveGraph::default()
        };
        let conflicts = state.apply(step)?;
        let commit_seq = u64::try_from(self.positions.len())
            .map_err(|_| "model commit sequence exceeds u64".to_string())?
            .checked_add(1)
            .ok_or_else(|| "model commit sequence is exhausted".to_string())?;
        self.positions.push(logical_seq);
        self.branches
            .entry(branch)
            .or_insert_with(|| NaiveBranch {
                origin: NaiveOrigin::Genesis,
                commits: Vec::new(),
            })
            .commits
            .push(NaiveCommit {
                commit_seq,
                logical_seq,
                step,
                conflicts,
            });
        Ok(commit_seq)
    }

    fn materialize(&self, branch: u128, logical_high: u64) -> Result<NaiveGraph, String> {
        self.materialize_inner(branch, logical_high, &mut BTreeSet::new())
    }

    fn materialize_inner(
        &self,
        branch: u128,
        logical_high: u64,
        visiting: &mut BTreeSet<u128>,
    ) -> Result<NaiveGraph, String> {
        if !visiting.insert(branch) {
            return Err(format!("branch lineage cycles at {branch}"));
        }
        let record = self
            .branches
            .get(&branch)
            .ok_or_else(|| format!("branch {branch} does not exist"))?;
        let mut graph = match record.origin {
            NaiveOrigin::Genesis => NaiveGraph::default(),
            NaiveOrigin::Fork { parent, boundary } => {
                self.materialize_inner(parent, logical_high.min(boundary), visiting)?
            }
        };
        visiting.remove(&branch);
        for commit in &record.commits {
            if commit.logical_seq > logical_high {
                break;
            }
            graph.apply(commit.step)?;
        }
        Ok(graph)
    }

    fn applied_through(&self, branch: u128) -> Result<u64, String> {
        let record = self
            .branches
            .get(&branch)
            .ok_or_else(|| format!("branch {branch} does not exist"))?;
        let inherited = match record.origin {
            NaiveOrigin::Genesis => 0,
            NaiveOrigin::Fork { boundary, .. } => self.commit_frontier_at(boundary),
        };
        Ok(record
            .commits
            .last()
            .map_or(inherited, |commit| commit.commit_seq))
    }

    fn all_vertex_ids(&self) -> BTreeSet<u128> {
        self.branches
            .values()
            .flat_map(|branch| branch.commits.iter())
            .filter_map(|commit| match commit.step {
                BranchStep::Graph(Step::CreateVertex(vid)) => Some(vid),
                _ => None,
            })
            .collect()
    }

    fn all_edge_ids(&self) -> BTreeSet<u128> {
        self.branches
            .values()
            .flat_map(|branch| branch.commits.iter())
            .filter_map(|commit| match commit.step {
                BranchStep::Graph(Step::AddEdge { eid, .. }) => Some(eid),
                _ => None,
            })
            .collect()
    }

    fn branch_owns_vertex(&self, branch: u128, vid: u128) -> bool {
        self.branches.get(&branch).is_some_and(|record| {
            record
                .commits
                .iter()
                .any(|commit| commit.step == BranchStep::Graph(Step::CreateVertex(vid)))
        })
    }

    fn branch_owns_edge(&self, branch: u128, eid: u128) -> bool {
        self.branches.get(&branch).is_some_and(|record| {
            record.commits.iter().any(
                |commit| matches!(commit.step, BranchStep::Graph(Step::AddEdge { eid: found, .. }) if found == eid),
            )
        })
    }

    fn branch_owns_element(&self, branch: u128, elem: ElementId) -> bool {
        match elem {
            ElementId::Vertex(vid) => self.branch_owns_vertex(branch, vid.0),
            ElementId::Edge(eid) => self.branch_owns_edge(branch, eid.0),
        }
    }

    fn expected_conflicts_since(
        &self,
        branch: u128,
        since: u64,
    ) -> Result<BTreeSet<ConflictKey>, String> {
        let record = self
            .branches
            .get(&branch)
            .ok_or_else(|| format!("branch {branch} does not exist"))?;
        let mut conflicts = BTreeSet::new();
        let born_by_commit = record
            .commits
            .first()
            .is_some_and(|first| first.commit_seq > since);
        let born_by_origin = !record
            .commits
            .iter()
            .any(|commit| commit.commit_seq <= since);
        if born_by_commit || born_by_origin {
            conflicts.insert(ConflictKey::CoordinateExistence);
        }

        let commit_high = self.applied_through(branch)?;
        let logical_high = self
            .logical_for_commit(commit_high)
            .ok_or_else(|| format!("commit {commit_high} has no logical position"))?;
        for (ancestor, ancestor_commit_high, ancestor_logical_high) in
            self.lineage_caps(branch, commit_high, logical_high)?
        {
            let ancestor_record = self
                .branches
                .get(&ancestor)
                .ok_or_else(|| format!("ancestor branch {ancestor} disappeared"))?;
            for commit in &ancestor_record.commits {
                if commit.commit_seq <= since {
                    continue;
                }
                if commit.commit_seq > ancestor_commit_high
                    || commit.logical_seq > ancestor_logical_high
                {
                    break;
                }
                conflicts.extend(commit.conflicts.iter().copied());
            }
        }
        Ok(conflicts)
    }

    fn lineage_caps(
        &self,
        branch: u128,
        mut commit_high: u64,
        mut logical_high: u64,
    ) -> Result<Vec<(u128, u64, u64)>, String> {
        let mut chain = Vec::new();
        let mut current = branch;
        let mut seen = BTreeSet::new();
        loop {
            if !seen.insert(current) {
                return Err(format!("branch lineage cycles at {current}"));
            }
            chain.push((current, commit_high, logical_high));
            let record = self
                .branches
                .get(&current)
                .ok_or_else(|| format!("branch {current} does not exist"))?;
            match record.origin {
                NaiveOrigin::Genesis => break,
                NaiveOrigin::Fork { parent, boundary } => {
                    logical_high = logical_high.min(boundary);
                    commit_high = commit_high.min(self.commit_frontier_at(boundary));
                    current = parent;
                }
            }
        }
        chain.reverse();
        Ok(chain)
    }
}

fn generate_branch_forest(seed: u64, action_budget: usize) -> BranchGenerated {
    let mut rng = SplitMix64(seed);
    let mut model = NaiveBranchDatabase::default();
    let mut actions = Vec::with_capacity(action_budget);
    let mut coverage = BranchCoverage::default();
    let mut next_branch = 2u128;
    let mut next_vid = 1u128;
    let mut next_eid = 1_000u128;
    let mut next_property_value = 10_000i64;
    let mut logical_seq = 0u64;

    for _ in 0..action_budget {
        if !model.branches.is_empty() && model.branches.len() < 8 && rng.chance(30) {
            let branches = model.branch_ids();
            let parent = branches[rng.below(branches.len())];
            let child = next_branch;
            next_branch += 1;
            let boundaries = model.observed_boundaries();
            let boundary = if rng.chance(35) {
                model.logical_frontier()
            } else {
                boundaries[rng.below(boundaries.len())]
            };
            if boundary == model.logical_frontier() {
                coverage.current_forks += 1;
            } else {
                coverage.historical_forks += 1;
            }
            if boundary == 0 {
                coverage.zero_forks += 1;
            }
            if model.branch_depth(parent).expect("generated parent exists") > 0 {
                coverage.nested_forks += 1;
            }
            if boundary > 0
                && !model
                    .materialize(parent, boundary)
                    .expect("generated boundary materializes")
                    .vertices
                    .is_empty()
            {
                coverage.inherited_conflict_windows += 1;
            }
            model
                .fork(parent, child, boundary)
                .expect("generated fork is reachable");
            actions.push(BranchAction::Fork {
                parent,
                child,
                boundary,
            });
            continue;
        }

        let branch = if model.branches.is_empty() {
            1
        } else {
            let branches = model.branch_ids();
            branches[rng.below(branches.len())]
        };
        let state = if model.branches.contains_key(&branch) {
            model
                .materialize(branch, model.logical_frontier())
                .expect("generated branch materializes")
        } else {
            NaiveGraph::default()
        };
        let step = loop {
            match rng.below(100) {
                0..=21 => {
                    let vid = next_vid;
                    next_vid += 1;
                    break BranchStep::Graph(Step::CreateVertex(vid));
                }
                22..=44 => {
                    if state.vertices.is_empty() {
                        continue;
                    }
                    let vertices = state.vertices.keys().copied().collect::<Vec<_>>();
                    let src = vertices[rng.below(vertices.len())];
                    let dst = vertices[rng.below(vertices.len())];
                    let eid = next_eid;
                    next_eid += 1;
                    break BranchStep::Graph(Step::AddEdge { eid, src, dst });
                }
                45..=54 => {
                    if state.edges.is_empty() {
                        continue;
                    }
                    let edges = state.edges.keys().copied().collect::<Vec<_>>();
                    let eid = edges[rng.below(edges.len())];
                    if model.branches.contains_key(&branch) && !model.branch_owns_edge(branch, eid)
                    {
                        coverage.inherited_mutations += 1;
                    }
                    break BranchStep::Graph(Step::DeleteEdge(eid));
                }
                55..=64 => {
                    if state.vertices.is_empty() {
                        continue;
                    }
                    let vertices = state.vertices.keys().copied().collect::<Vec<_>>();
                    let vid = vertices[rng.below(vertices.len())];
                    if model.branches.contains_key(&branch)
                        && !model.branch_owns_vertex(branch, vid)
                    {
                        coverage.inherited_mutations += 1;
                    }
                    break BranchStep::Graph(Step::DeleteVertex(vid));
                }
                65..=81 => {
                    let elements = state.live_elements();
                    if elements.is_empty() {
                        continue;
                    }
                    let elem = elements[rng.below(elements.len())];
                    let inherited = model.branches.contains_key(&branch)
                        && !model.branch_owns_element(branch, elem);
                    let after = if state.property(elem).is_some() && rng.chance(35) {
                        coverage.property_removals += 1;
                        None
                    } else {
                        let value = next_property_value;
                        next_property_value = next_property_value
                            .checked_add(1)
                            .expect("small generated property domain");
                        coverage.property_sets += 1;
                        Some(value)
                    };
                    match elem {
                        ElementId::Vertex(_) => coverage.vertex_property_mutations += 1,
                        ElementId::Edge(_) => coverage.edge_property_mutations += 1,
                    }
                    if inherited {
                        coverage.inherited_mutations += 1;
                        coverage.inherited_property_mutations += 1;
                    }
                    break BranchStep::SetProperty { elem, after };
                }
                _ => {
                    let elements = state.live_elements();
                    if elements.is_empty() {
                        continue;
                    }
                    let elem = elements[rng.below(elements.len())];
                    let inherited = model.branches.contains_key(&branch)
                        && !model.branch_owns_element(branch, elem);
                    let current = state.valid_time(elem);
                    let after = if current.is_some() && rng.chance(30) {
                        coverage.valid_time_clears += 1;
                        None
                    } else {
                        let period = loop {
                            let start = i64::try_from(rng.below(65))
                                .expect("small generated time domain")
                                - 32;
                            let end_micros = if rng.chance(70) {
                                Some(
                                    start
                                        + i64::try_from(rng.below(9))
                                            .expect("small generated duration domain"),
                                )
                            } else {
                                None
                            };
                            let candidate = ValidTimePeriod {
                                start_micros: start,
                                end_micros,
                            };
                            if Some(candidate) != current {
                                break candidate;
                            }
                        };
                        coverage.valid_time_sets += 1;
                        if let Some(end) = period.end_micros {
                            coverage.bounded_valid_times += 1;
                            if end == period.start_micros {
                                coverage.zero_length_valid_times += 1;
                            }
                        } else {
                            coverage.open_valid_times += 1;
                        }
                        Some(period)
                    };
                    match elem {
                        ElementId::Vertex(_) => coverage.vertex_valid_time_mutations += 1,
                        ElementId::Edge(_) => coverage.edge_valid_time_mutations += 1,
                    }
                    if inherited {
                        coverage.inherited_mutations += 1;
                        coverage.inherited_valid_time_mutations += 1;
                    }
                    break BranchStep::SetValidTime { elem, after };
                }
            }
        };
        logical_seq += u64::try_from(1 + rng.below(4)).expect("small logical increment");
        if model
            .branches
            .get(&branch)
            .is_some_and(|record| matches!(record.origin, NaiveOrigin::Fork { .. }))
        {
            coverage.fork_branch_writes += 1;
        }
        model
            .apply_write(branch, logical_seq, step)
            .expect("generated write is reachable");
        actions.push(BranchAction::Write {
            logical_seq,
            branch,
            step,
        });
    }

    for (fork_index, action) in actions.iter().enumerate() {
        let BranchAction::Fork { parent, child, .. } = *action else {
            continue;
        };
        let suffix = &actions[fork_index + 1..];
        let parent_written = suffix.iter().any(|candidate| {
            matches!(candidate, BranchAction::Write { branch, .. } if *branch == parent)
        });
        let child_written = suffix.iter().any(
            |candidate| matches!(candidate, BranchAction::Write { branch, .. } if *branch == child),
        );
        if parent_written && child_written {
            coverage.divergent_siblings += 1;
        }
    }

    BranchGenerated { actions, coverage }
}

fn delta_row_for_branch(
    db: &ReferenceDatabase,
    branch: BranchId,
    step: BranchStep,
) -> Result<DeltaRow, String> {
    match step {
        BranchStep::Graph(Step::CreateVertex(vid)) => {
            let birth_ordinal = u64::try_from(vid)
                .map_err(|_| format!("vertex identity {vid} exceeds the birth-ordinal domain"))?;
            let value = i64::try_from(vid)
                .map_err(|_| format!("vertex identity {vid} exceeds the test scalar domain"))?;
            Ok(DeltaRow::CreateVertex {
                vid: VId(vid),
                birth_ordinal,
                labels: vec![LABEL],
                props: vec![(PROP, CanonicalScalar::Int(value))],
                valid_time: None,
            })
        }
        BranchStep::Graph(Step::AddEdge { eid, src, dst }) => Ok(DeltaRow::CreateEdge {
            eid: EId(eid),
            birth_ordinal: u64::try_from(eid)
                .map_err(|_| format!("edge identity {eid} exceeds the birth-ordinal domain"))?,
            src: VId(src),
            relation: REL,
            dst: VId(dst),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        }),
        BranchStep::Graph(Step::DeleteEdge(eid)) => {
            let graph = db
                .graph(GRAPH, branch)
                .ok_or_else(|| format!("DeleteEdge({eid}) names an absent branch"))?;
            let before_version = graph
                .element_version(ElementId::Edge(EId(eid)))
                .ok_or_else(|| format!("DeleteEdge({eid}) names a dead edge"))?;
            Ok(DeltaRow::DeleteEdge {
                eid: EId(eid),
                before_version,
            })
        }
        BranchStep::Graph(Step::DeleteVertex(vid)) => {
            let graph = db
                .graph(GRAPH, branch)
                .ok_or_else(|| format!("DeleteVertex({vid}) names an absent branch"))?;
            let before_version = graph
                .element_version(ElementId::Vertex(VId(vid)))
                .ok_or_else(|| format!("DeleteVertex({vid}) names a dead vertex"))?;
            Ok(DeltaRow::DeleteVertex {
                vid: VId(vid),
                before_version,
                sorted_retired_incident_edges: graph.incident_edges(VId(vid)),
            })
        }
        BranchStep::SetProperty { elem, after } => {
            let graph = db
                .graph(GRAPH, branch)
                .ok_or_else(|| format!("SetProperty({elem:?}) names an absent branch"))?;
            let before = match elem {
                ElementId::Vertex(vid) => graph
                    .vertex(vid)
                    .ok_or_else(|| format!("SetProperty names dead vertex {}", vid.0))?
                    .props
                    .get(&PROP)
                    .cloned(),
                ElementId::Edge(eid) => graph
                    .edge(eid)
                    .ok_or_else(|| format!("SetProperty names dead edge {}", eid.0))?
                    .props
                    .get(&PROP)
                    .cloned(),
            };
            Ok(DeltaRow::Property {
                elem,
                property: PROP,
                before,
                after: after.map(CanonicalScalar::Int),
            })
        }
        BranchStep::SetValidTime { elem, after } => {
            let graph = db
                .graph(GRAPH, branch)
                .ok_or_else(|| format!("SetValidTime({elem:?}) names an absent branch"))?;
            let before = match elem {
                ElementId::Vertex(vid) => {
                    graph
                        .vertex(vid)
                        .ok_or_else(|| format!("SetValidTime names dead vertex {}", vid.0))?
                        .valid_time
                }
                ElementId::Edge(eid) => {
                    graph
                        .edge(eid)
                        .ok_or_else(|| format!("SetValidTime names dead edge {}", eid.0))?
                        .valid_time
                }
            };
            Ok(DeltaRow::ValidTime {
                elem,
                contract_id: ObjectId([0x56; 32]),
                before,
                after,
            })
        }
    }
}

fn branch_template(branch: BranchId, row: DeltaRow) -> Result<LogicalDeltaTemplate, String> {
    LogicalDeltaTemplate::build(
        ObjectId([0x31; 32]),
        [0x42; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch,
            relation: REL,
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows: vec![row],
        }],
    )
    .map_err(|error| format!("branch template is not canonical: {error}"))
}

fn run_branch_forest(case: &BranchGenerated) -> Result<(), String> {
    run_branch_actions(&case.actions)
}

fn run_branch_actions(actions: &[BranchAction]) -> Result<(), String> {
    let mut subject = ReferenceDatabase::new();
    let mut model = NaiveBranchDatabase::default();

    for (index, action) in actions.iter().copied().enumerate() {
        match action {
            BranchAction::Write {
                logical_seq,
                branch,
                step,
            } => {
                let mut candidate = model.clone();
                let commit_seq = candidate
                    .apply_write(branch, logical_seq, step)
                    .map_err(|error| format!("action {index}: model refused write: {error}"))?;
                let branch_id = BranchId(branch);
                let row = delta_row_for_branch(&subject, branch_id, step)
                    .map_err(|error| format!("action {index}: {error}"))?;
                let template = branch_template(branch_id, row)
                    .map_err(|error| format!("action {index}: {error}"))?;
                subject
                    .apply_template(
                        &template,
                        CommitSeq(commit_seq),
                        LogicalCommandSeq(logical_seq),
                    )
                    .map_err(|error| {
                        format!("action {index}: reference refused reachable write: {error}")
                    })?;
                model = candidate;
            }
            BranchAction::Fork {
                parent,
                child,
                boundary,
            } => {
                let mut candidate = model.clone();
                candidate
                    .fork(parent, child, boundary)
                    .map_err(|error| format!("action {index}: model refused fork: {error}"))?;
                let result = if boundary == model.logical_frontier() {
                    subject.fork_branch(GRAPH, BranchId(parent), BranchId(child))
                } else {
                    subject.fork_branch_at(
                        GRAPH,
                        BranchId(parent),
                        BranchId(child),
                        LogicalCommandSeq(boundary),
                    )
                };
                result.map_err(|error| {
                    format!("action {index}: reference refused reachable fork: {error}")
                })?;
                model = candidate;
            }
        }
        check_branch_model(&subject, &model).map_err(|error| format!("action {index}: {error}"))?;
    }
    Ok(())
}

fn check_branch_model(
    subject: &ReferenceDatabase,
    model: &NaiveBranchDatabase,
) -> Result<(), String> {
    if subject.coordinate_count() != model.branches.len() {
        return Err(format!(
            "coordinate count differs: reference {} vs model {}",
            subject.coordinate_count(),
            model.branches.len()
        ));
    }
    let all_vertices = model.all_vertex_ids();
    let all_edges = model.all_edge_ids();

    for (branch, expected_branch) in &model.branches {
        let branch_id = BranchId(*branch);
        let expected_origin = match expected_branch.origin {
            NaiveOrigin::Genesis => BranchOrigin::Genesis,
            NaiveOrigin::Fork { parent, boundary } => BranchOrigin::Fork {
                parent_branch: BranchId(parent),
                fork_boundary: LogicalCommandSeq(boundary),
            },
        };
        if subject.branch_origin(GRAPH, branch_id) != Some(expected_origin) {
            return Err(format!(
                "branch {branch} origin differs: reference {:?} vs model {expected_origin:?}",
                subject.branch_origin(GRAPH, branch_id)
            ));
        }
        let expected_frontier = model.applied_through(*branch)?;
        if subject.applied_through(GRAPH, branch_id) != Some(CommitSeq(expected_frontier)) {
            return Err(format!(
                "branch {branch} frontier differs: reference {:?} vs model {expected_frontier}",
                subject.applied_through(GRAPH, branch_id)
            ));
        }
        if subject.recorded_commits(GRAPH, branch_id) != expected_branch.commits.len() {
            return Err(format!(
                "branch {branch} own-commit count differs: reference {} vs model {}",
                subject.recorded_commits(GRAPH, branch_id),
                expected_branch.commits.len()
            ));
        }

        let expected_current = model.materialize(*branch, model.logical_frontier())?;
        let actual_current = subject
            .graph(GRAPH, branch_id)
            .ok_or_else(|| format!("branch {branch} has no reference graph"))?;
        compare_branch_graph(
            actual_current,
            &expected_current,
            &all_vertices,
            &all_edges,
            &format!("branch {branch} current state"),
        )?;

        for high in 0..=expected_frontier {
            let snapshot = subject
                .snapshot_at(GRAPH, branch_id, CommitSeq(high))
                .map_err(|error| format!("branch {branch} snapshot {high} failed: {error}"))?;
            let actual = subject
                .read(&snapshot)
                .map_err(|error| format!("branch {branch} read {high} failed: {error}"))?;
            let logical_high = model
                .logical_for_commit(high)
                .ok_or_else(|| format!("commit {high} has no model logical position"))?;
            let expected = model.materialize(*branch, logical_high)?;
            compare_branch_graph(
                &actual,
                &expected,
                &all_vertices,
                &all_edges,
                &format!("branch {branch} at commit {high}"),
            )?;

            let actual_conflicts = subject
                .conflict_keys_since(GRAPH, branch_id, CommitSeq(high))
                .map_err(|error| {
                    format!("branch {branch} conflict window after {high} failed: {error}")
                })?;
            let expected_conflicts = model.expected_conflicts_since(*branch, high)?;
            if actual_conflicts != expected_conflicts {
                return Err(format!(
                    "branch {branch} conflicts after {high} differ: reference \
                     {actual_conflicts:?} vs model {expected_conflicts:?}"
                ));
            }
        }
    }
    Ok(())
}

fn compare_branch_graph(
    actual: &ReferenceGraph,
    expected: &NaiveGraph,
    all_vertices: &BTreeSet<u128>,
    all_edges: &BTreeSet<u128>,
    context: &str,
) -> Result<(), String> {
    if actual.vertex_count() != expected.vertices.len()
        || actual.edge_count() != expected.edges.len()
    {
        return Err(format!(
            "{context}: cardinality differs: reference ({}, {}) vs model ({}, {})",
            actual.vertex_count(),
            actual.edge_count(),
            expected.vertices.len(),
            expected.edges.len()
        ));
    }
    for vid in all_vertices {
        let expected_vertex = expected.vertices.get(vid);
        let found = actual.vertex(VId(*vid));
        if found.is_some() != expected_vertex.is_some() {
            return Err(format!(
                "{context}: vertex {vid} liveness differs: reference {} vs model {}",
                found.is_some(),
                expected_vertex.is_some()
            ));
        }
        if let (Some(vertex), Some(expected_vertex)) = (found, expected_vertex) {
            if vertex.birth_ordinal != expected_vertex.birth_ordinal
                || vertex.labels != BTreeSet::from([LABEL])
            {
                return Err(format!(
                    "{context}: vertex {vid} structural payload differs: {vertex:?}"
                ));
            }
            if vertex.props != expected_vertex.props {
                return Err(format!(
                    "{context}: vertex {vid} properties differ: reference {:?} vs model {:?}",
                    vertex.props, expected_vertex.props
                ));
            }
            if vertex.valid_time != expected_vertex.valid_time {
                return Err(format!(
                    "{context}: vertex {vid} valid time differs: reference {:?} vs model {:?}",
                    vertex.valid_time, expected_vertex.valid_time
                ));
            }
        }
        let expected_out = expected
            .edges
            .iter()
            .filter_map(|(eid, edge)| (edge.src == *vid).then_some(EId(*eid)))
            .collect::<Vec<_>>();
        if actual.out_edges(VId(*vid)) != expected_out {
            return Err(format!(
                "{context}: outgoing edges of {vid} differ: reference {:?} vs model {expected_out:?}",
                actual.out_edges(VId(*vid))
            ));
        }
    }
    for eid in all_edges {
        let expected_edge = expected.edges.get(eid);
        let found = actual.edge(EId(*eid));
        if found.is_some() != expected_edge.is_some() {
            return Err(format!(
                "{context}: edge {eid} liveness differs: reference {} vs model {}",
                found.is_some(),
                expected_edge.is_some()
            ));
        }
        if let (Some(edge), Some(expected_edge)) = (found, expected_edge) {
            if edge.birth_ordinal != expected_edge.birth_ordinal
                || edge.src != VId(expected_edge.src)
                || edge.dst != VId(expected_edge.dst)
                || edge.relation != REL
                || edge.canonical_key.is_some()
            {
                return Err(format!(
                    "{context}: edge {eid} structural payload differs: {edge:?}"
                ));
            }
            if edge.props != expected_edge.props {
                return Err(format!(
                    "{context}: edge {eid} properties differ: reference {:?} vs model {:?}",
                    edge.props, expected_edge.props
                ));
            }
            if edge.valid_time != expected_edge.valid_time {
                return Err(format!(
                    "{context}: edge {eid} valid time differs: reference {:?} vs model {:?}",
                    edge.valid_time, expected_edge.valid_time
                ));
            }
        }
    }
    compare_branch_temporal_projection(actual, expected, context)
}

fn period_contains(period: Option<ValidTimePeriod>, micros: i64) -> bool {
    period.is_none_or(|period| {
        period.start_micros <= micros && period.end_micros.is_none_or(|end| micros < end)
    })
}

fn naive_vertex_live_at(graph: &NaiveGraph, vid: u128, micros: i64) -> bool {
    graph
        .vertices
        .get(&vid)
        .is_some_and(|vertex| period_contains(vertex.valid_time, micros))
}

fn temporal_probe_instants(graph: &NaiveGraph) -> BTreeSet<i64> {
    let mut instants = BTreeSet::from([i64::MIN, -1, 0, 1, i64::MAX]);
    for period in graph
        .vertices
        .values()
        .filter_map(|vertex| vertex.valid_time)
        .chain(graph.edges.values().filter_map(|edge| edge.valid_time))
    {
        instants.insert(period.start_micros.saturating_sub(1));
        instants.insert(period.start_micros);
        instants.insert(period.start_micros.saturating_add(1));
        if let Some(end) = period.end_micros {
            instants.insert(end.saturating_sub(1));
            instants.insert(end);
            instants.insert(end.saturating_add(1));
        }
    }
    instants
}

fn compare_branch_temporal_projection(
    actual: &ReferenceGraph,
    expected: &NaiveGraph,
    context: &str,
) -> Result<(), String> {
    for micros in temporal_probe_instants(expected) {
        let expected_vertices = expected
            .vertices
            .iter()
            .filter_map(|(vid, vertex)| {
                period_contains(vertex.valid_time, micros).then_some(VId(*vid))
            })
            .collect::<Vec<_>>();
        let expected_edges = expected
            .edges
            .iter()
            .filter_map(|(eid, edge)| {
                (period_contains(edge.valid_time, micros)
                    && naive_vertex_live_at(expected, edge.src, micros)
                    && naive_vertex_live_at(expected, edge.dst, micros))
                .then_some(EId(*eid))
            })
            .collect::<Vec<_>>();
        if actual.vertices_as_of(micros) != expected_vertices {
            return Err(format!(
                "{context}: vertices at valid time {micros} differ: reference {:?} vs model {expected_vertices:?}",
                actual.vertices_as_of(micros)
            ));
        }
        if actual.edges_as_of(micros) != expected_edges {
            return Err(format!(
                "{context}: edges at valid time {micros} differ: reference {:?} vs model {expected_edges:?}",
                actual.edges_as_of(micros)
            ));
        }
        for vid in expected.vertices.keys() {
            let expected_neighbours = expected
                .edges
                .values()
                .filter_map(|edge| {
                    (edge.src == *vid
                        && period_contains(edge.valid_time, micros)
                        && naive_vertex_live_at(expected, edge.src, micros)
                        && naive_vertex_live_at(expected, edge.dst, micros))
                    .then_some(VId(edge.dst))
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let actual_neighbours = actual.neighbours_as_of(VId(*vid), REL, micros);
            if actual_neighbours != expected_neighbours {
                return Err(format!(
                    "{context}: neighbours of {vid} at valid time {micros} differ: reference {actual_neighbours:?} vs model {expected_neighbours:?}"
                ));
            }
        }
    }
    Ok(())
}

fn branch_failure_kind(error: &str) -> &str {
    match error.find(": ") {
        Some(cut) if error.starts_with("action ") => &error[cut + 2..],
        _ => error,
    }
}

fn shrink_branch_forest(case: &BranchGenerated) -> BranchGenerated {
    let Err(original) = run_branch_forest(case) else {
        return case.clone();
    };
    let target = branch_failure_kind(&original).to_string();
    let mut best = case.actions.clone();

    // First remove whole branch subtrees. A one-action shrinker cannot do this:
    // deleting only the fork leaves every descendant action unreachable.
    let children = best
        .iter()
        .filter_map(|action| match action {
            BranchAction::Fork { child, .. } => Some(*child),
            BranchAction::Write { .. } => None,
        })
        .collect::<Vec<_>>();
    for child in children.into_iter().rev() {
        let candidate = drop_branch_subtree(&best, child);
        if candidate.len() < best.len()
            && run_branch_actions(&candidate)
                .is_err_and(|error| branch_failure_kind(&error) == target)
        {
            best = candidate;
        }
    }

    // Then shorten ancestry without deleting the child. The boundary remains an
    // observed global position; only the redundant intermediate parent changes.
    for index in (0..best.len()).rev() {
        let Some(candidate) = lower_fork_depth_once(&best, index) else {
            continue;
        };
        if run_branch_actions(&candidate).is_err_and(|error| branch_failure_kind(&error) == target)
        {
            best = candidate;
        }
    }

    // Finally use ordinary delta debugging inside the surviving forest.
    for index in (0..best.len()).rev() {
        let mut candidate = best.clone();
        candidate.remove(index);
        if run_branch_actions(&candidate).is_err_and(|error| branch_failure_kind(&error) == target)
        {
            best = candidate;
        }
    }

    BranchGenerated {
        actions: best,
        coverage: BranchCoverage::default(),
    }
}

fn drop_branch_subtree(actions: &[BranchAction], root: u128) -> Vec<BranchAction> {
    let mut removed = BTreeSet::from([root]);
    loop {
        let before = removed.len();
        for action in actions {
            if let BranchAction::Fork { parent, child, .. } = *action
                && removed.contains(&parent)
            {
                removed.insert(child);
            }
        }
        if removed.len() == before {
            break;
        }
    }
    actions
        .iter()
        .copied()
        .filter(|action| match *action {
            BranchAction::Write { branch, .. } => !removed.contains(&branch),
            BranchAction::Fork { child, .. } => !removed.contains(&child),
        })
        .collect()
}

fn lower_fork_depth_once(actions: &[BranchAction], index: usize) -> Option<Vec<BranchAction>> {
    let BranchAction::Fork {
        parent,
        child,
        boundary,
    } = *actions.get(index)?
    else {
        return None;
    };
    let grandparent = actions[..index]
        .iter()
        .rev()
        .find_map(|action| match *action {
            BranchAction::Fork {
                parent: candidate,
                child: forked,
                ..
            } if forked == parent => Some(candidate),
            BranchAction::Write { .. } | BranchAction::Fork { .. } => None,
        })?;
    let mut lowered = actions.to_vec();
    lowered[index] = BranchAction::Fork {
        parent: grandparent,
        child,
        boundary,
    };
    Some(lowered)
}

fn report_branch_failure(seed: u64, case: &BranchGenerated, error: &str) -> String {
    let mut out = format!(
        "\n=== GENERATED BRANCH DIFFERENTIAL FAILURE ===\nseed: {seed}\nerror: {error}\n\n\
         Paste this into generated_histories.rs as a permanent law:\n\n\
         #[test]\nfn regression_branch_seed_{seed}() {{\n    let actions = [\n"
    );
    for action in &case.actions {
        match action {
            BranchAction::Write {
                logical_seq,
                branch,
                step,
            } => out.push_str(&format!(
                "        BranchAction::Write {{ logical_seq: {logical_seq}, branch: {branch}, step: {} }},\n",
                render_branch_step(*step)
            )),
            BranchAction::Fork {
                parent,
                child,
                boundary,
            } => out.push_str(&format!(
                "        BranchAction::Fork {{ parent: {parent}, child: {child}, boundary: {boundary} }},\n"
            )),
        }
    }
    out.push_str(
        "    ];\n    run_branch_actions(&actions).expect(\"branch history agrees\");\n}\n",
    );
    out
}

fn render_branch_step(step: BranchStep) -> String {
    match step {
        BranchStep::Graph(Step::CreateVertex(vid)) => {
            format!("BranchStep::Graph(Step::CreateVertex({vid}))")
        }
        BranchStep::Graph(Step::AddEdge { eid, src, dst }) => {
            format!("BranchStep::Graph(Step::AddEdge {{ eid: {eid}, src: {src}, dst: {dst} }})")
        }
        BranchStep::Graph(Step::DeleteEdge(eid)) => {
            format!("BranchStep::Graph(Step::DeleteEdge({eid}))")
        }
        BranchStep::Graph(Step::DeleteVertex(vid)) => {
            format!("BranchStep::Graph(Step::DeleteVertex({vid}))")
        }
        BranchStep::SetProperty { elem, after } => format!(
            "BranchStep::SetProperty {{ elem: {}, after: {after:?} }}",
            render_branch_element(elem)
        ),
        BranchStep::SetValidTime { elem, after } => {
            let rendered_after = after.map_or_else(
                || "None".to_string(),
                |period| {
                    format!(
                        "Some(ValidTimePeriod {{ start_micros: {}, end_micros: {:?} }})",
                        period.start_micros, period.end_micros
                    )
                },
            );
            format!(
                "BranchStep::SetValidTime {{ elem: {}, after: {rendered_after} }}",
                render_branch_element(elem)
            )
        }
    }
}

fn render_branch_element(elem: ElementId) -> String {
    match elem {
        ElementId::Vertex(vid) => format!("ElementId::Vertex(VId({}))", vid.0),
        ElementId::Edge(eid) => format!("ElementId::Edge(EId({}))", eid.0),
    }
}

/// A green differential needs a control proving that its independent side can
/// disagree. Substitute the parent's present for a historical boundary: both
/// graphs are internally valid, but the child gains one vertex it must not see.
#[test]
fn branch_differential_detects_current_state_substituted_for_a_historical_fork() {
    let mut subject = ReferenceDatabase::new();
    let mut model = NaiveBranchDatabase::default();
    for (logical_seq, vid) in [(1u64, 1u128), (2, 2)] {
        let step = BranchStep::Graph(Step::CreateVertex(vid));
        let commit_seq = model
            .apply_write(1, logical_seq, step)
            .expect("model accepts reachable write");
        let row = delta_row_for_branch(&subject, BranchId(1), step).expect("row builds");
        subject
            .apply_template(
                &branch_template(BranchId(1), row).expect("template builds"),
                CommitSeq(commit_seq),
                LogicalCommandSeq(logical_seq),
            )
            .expect("reference accepts reachable write");
    }

    model.fork(1, 2, 1).expect("model forks at history");
    subject
        .fork_branch(GRAPH, BranchId(1), BranchId(2))
        .expect("planted mutant forks at the present");

    let expected = model
        .materialize(2, model.logical_frontier())
        .expect("model child materializes");
    let error = compare_branch_graph(
        subject
            .graph(GRAPH, BranchId(2))
            .expect("mutant child exists"),
        &expected,
        &model.all_vertex_ids(),
        &model.all_edge_ids(),
        "planted current-for-history mutant",
    )
    .expect_err("the differential must detect the planted boundary substitution");
    assert!(
        error.contains("cardinality differs") || error.contains("vertex 2 liveness differs"),
        "the planted state divergence should be named, got {error}"
    );
}

/// Controls for the two new payload families. The valid-time arm calls the
/// selector oracle directly so exact payload comparison cannot mask a broken
/// half-open or endpoint-liveness projection.
#[test]
fn branch_differential_detects_dropped_property_and_valid_time_rows() {
    let create = BranchStep::Graph(Step::CreateVertex(1));
    let mut actual = ReferenceGraph::new();
    actual
        .apply_row(&DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![LABEL],
            props: vec![(PROP, CanonicalScalar::Int(1))],
            valid_time: None,
        })
        .expect("control vertex applies");

    let mut base = NaiveGraph::default();
    base.apply(create).expect("control model vertex applies");

    let mut property_expected = base.clone();
    property_expected
        .apply(BranchStep::SetProperty {
            elem: ElementId::Vertex(VId(1)),
            after: Some(42),
        })
        .expect("control property applies");
    let property_error = compare_branch_graph(
        &actual,
        &property_expected,
        &BTreeSet::from([1]),
        &BTreeSet::new(),
        "planted dropped-property mutant",
    )
    .expect_err("the differential must detect a dropped property row");
    assert!(
        property_error.contains("properties differ"),
        "the property divergence should be named, got {property_error}"
    );

    let mut temporal_expected = base;
    temporal_expected
        .apply(BranchStep::SetValidTime {
            elem: ElementId::Vertex(VId(1)),
            after: Some(ValidTimePeriod {
                start_micros: 10,
                end_micros: Some(20),
            }),
        })
        .expect("control valid time applies");
    let temporal_error = compare_branch_temporal_projection(
        &actual,
        &temporal_expected,
        "planted dropped-valid-time mutant",
    )
    .expect_err("the selector differential must detect a dropped valid-time row");
    assert!(
        temporal_error.contains("vertices at valid time"),
        "the selector divergence should be named, got {temporal_error}"
    );
}

#[test]
fn generated_branch_forests_match_the_naive_lineage_model() -> Result<(), String> {
    let mut coverage = BranchCoverage::default();
    for seed in 0..128u64 {
        let case = generate_branch_forest(seed, 32);
        coverage.merge(&case.coverage);
        if let Err(original) = run_branch_forest(&case) {
            let minimal = shrink_branch_forest(&case);
            let minimal_error = run_branch_forest(&minimal)
                .err()
                .unwrap_or_else(|| "shrunk branch case no longer fails".to_string());
            assert_eq!(
                branch_failure_kind(&minimal_error),
                branch_failure_kind(&original),
                "seed {seed}: branch shrinking changed the defect ({original} -> {minimal_error})"
            );
            return Err(report_branch_failure(seed, &minimal, &minimal_error));
        }
    }

    assert!(coverage.current_forks > 0, "no current fork: {coverage:?}");
    assert!(
        coverage.historical_forks > 0,
        "no historical fork: {coverage:?}"
    );
    assert!(
        coverage.zero_forks > 0,
        "no zero-boundary fork: {coverage:?}"
    );
    assert!(coverage.nested_forks > 0, "no nested fork: {coverage:?}");
    assert!(
        coverage.fork_branch_writes > 0,
        "no write to a forked branch: {coverage:?}"
    );
    assert!(
        coverage.inherited_mutations > 0,
        "no mutation of inherited state: {coverage:?}"
    );
    assert!(
        coverage.divergent_siblings > 0,
        "no parent/child divergence: {coverage:?}"
    );
    assert!(
        coverage.inherited_conflict_windows > 0,
        "no inherited conflict window: {coverage:?}"
    );
    assert!(coverage.property_sets > 0, "no property set: {coverage:?}");
    assert!(
        coverage.property_removals > 0,
        "no property removal: {coverage:?}"
    );
    assert!(
        coverage.vertex_property_mutations > 0,
        "no vertex property mutation: {coverage:?}"
    );
    assert!(
        coverage.edge_property_mutations > 0,
        "no edge property mutation: {coverage:?}"
    );
    assert!(
        coverage.inherited_property_mutations > 0,
        "no inherited property mutation: {coverage:?}"
    );
    assert!(
        coverage.valid_time_sets > 0,
        "no valid-time set: {coverage:?}"
    );
    assert!(
        coverage.valid_time_clears > 0,
        "no valid-time clear: {coverage:?}"
    );
    assert!(
        coverage.bounded_valid_times > 0,
        "no bounded valid-time assignment: {coverage:?}"
    );
    assert!(
        coverage.open_valid_times > 0,
        "no open valid-time assignment: {coverage:?}"
    );
    assert!(
        coverage.zero_length_valid_times > 0,
        "no zero-length valid-time assignment: {coverage:?}"
    );
    assert!(
        coverage.vertex_valid_time_mutations > 0,
        "no vertex valid-time mutation: {coverage:?}"
    );
    assert!(
        coverage.edge_valid_time_mutations > 0,
        "no edge valid-time mutation: {coverage:?}"
    );
    assert!(
        coverage.inherited_valid_time_mutations > 0,
        "no inherited valid-time mutation: {coverage:?}"
    );
    Ok(())
}

#[test]
fn every_generated_branch_forest_is_reachable() -> Result<(), String> {
    for seed in 500..560u64 {
        let case = generate_branch_forest(seed, 28);
        let mut model = NaiveBranchDatabase::default();
        for (index, action) in case.actions.iter().copied().enumerate() {
            let result = match action {
                BranchAction::Write {
                    logical_seq,
                    branch,
                    step,
                } => model.apply_write(branch, logical_seq, step).map(|_| ()),
                BranchAction::Fork {
                    parent,
                    child,
                    boundary,
                } => model.fork(parent, child, boundary),
            };
            result.map_err(|error| {
                format!("seed {seed} action {index} is unreachable: {error}: {action:?}")
            })?;
        }
    }
    Ok(())
}

#[test]
fn a_seed_reproduces_its_branch_forest_exactly() {
    for seed in [13u64, 101, 9_001] {
        assert_eq!(
            generate_branch_forest(seed, 36).actions,
            generate_branch_forest(seed, 36).actions,
            "branch seed {seed} drifted"
        );
    }
}

#[test]
fn branch_shrink_moves_drop_subtrees_and_lower_fork_depth() {
    let actions = vec![
        BranchAction::Write {
            logical_seq: 1,
            branch: 1,
            step: BranchStep::Graph(Step::CreateVertex(1)),
        },
        BranchAction::Fork {
            parent: 1,
            child: 2,
            boundary: 1,
        },
        BranchAction::Fork {
            parent: 2,
            child: 3,
            boundary: 1,
        },
        BranchAction::Write {
            logical_seq: 2,
            branch: 3,
            step: BranchStep::Graph(Step::CreateVertex(2)),
        },
        BranchAction::Fork {
            parent: 1,
            child: 4,
            boundary: 1,
        },
    ];

    let dropped = drop_branch_subtree(&actions, 2);
    assert!(
        dropped.iter().all(|action| match action {
            BranchAction::Write { branch, .. } => !matches!(*branch, 2 | 3),
            BranchAction::Fork { child, .. } => !matches!(*child, 2 | 3),
        }),
        "dropping branch 2 must also drop descendant 3: {dropped:?}"
    );
    assert!(
        dropped
            .iter()
            .any(|action| matches!(action, BranchAction::Fork { child: 4, .. })),
        "an unrelated sibling must survive: {dropped:?}"
    );

    let lowered = lower_fork_depth_once(&actions, 2).expect("branch 3 has a grandparent");
    assert_eq!(
        lowered[2],
        BranchAction::Fork {
            parent: 1,
            child: 3,
            boundary: 1,
        },
        "the depth move reparents one level without changing the child or boundary"
    );
}

#[test]
fn branch_shrinker_preserves_the_failure_and_reduces_the_forest() {
    let case = BranchGenerated {
        actions: vec![
            BranchAction::Write {
                logical_seq: 1,
                branch: 1,
                step: BranchStep::Graph(Step::CreateVertex(1)),
            },
            BranchAction::Fork {
                parent: 1,
                child: 2,
                boundary: 1,
            },
            BranchAction::Fork {
                parent: 2,
                child: 3,
                boundary: 1,
            },
            BranchAction::Write {
                logical_seq: 2,
                branch: 3,
                step: BranchStep::Graph(Step::DeleteEdge(9_999)),
            },
            BranchAction::Fork {
                parent: 1,
                child: 4,
                boundary: 1,
            },
        ],
        coverage: BranchCoverage::default(),
    };
    let original = run_branch_forest(&case).expect_err("the planted branch case must fail");
    let minimal = shrink_branch_forest(&case);
    let shrunk = run_branch_forest(&minimal).expect_err("the shrunk branch case must still fail");
    assert_eq!(
        branch_failure_kind(&shrunk),
        branch_failure_kind(&original),
        "branch shrinking changed which defect fires: {original} -> {shrunk}"
    );
    assert!(
        minimal.actions.len() < case.actions.len(),
        "branch shrinker returned {} actions from {}",
        minimal.actions.len(),
        case.actions.len()
    );
    assert!(
        minimal.actions.iter().any(|action| matches!(
            action,
            BranchAction::Write {
                step: BranchStep::Graph(Step::DeleteEdge(9_999)),
                ..
            }
        )),
        "branch shrinker removed the failing operation: {:?}",
        minimal.actions
    );
}

/// A successful conditional write whose replacement equals the current value is
/// still a semantic read. The reducer quite correctly emits no row for that
/// no-op, so the transaction layer must capture the observation separately from
/// its durable effects. Otherwise two guarded, disjoint writes lose both
/// dependency edges and the independent history checker sees a serial history.
#[test]
fn compare_and_set_noop_guards_are_present_in_transaction_traces() {
    const SEMANTICS: ObjectId = ObjectId([0x11; 32]);

    let create = |vid| {
        Statement::new(vec![Intent::CreateVertex {
            vid: VId(vid),
            labels: vec![LABEL],
            props: vec![(PROP, CanonicalScalar::Int(1))],
        }])
    };
    let set = |vid, value| {
        Statement::new(vec![Intent::SetProp {
            elem: ElementId::Vertex(VId(vid)),
            name: PROP,
            value: CanonicalScalar::Int(value),
        }])
    };
    let guard = |vid| {
        Statement::new(vec![Intent::CompareAndSet {
            elem: ElementId::Vertex(VId(vid)),
            name: PROP,
            expected: Some(CanonicalScalar::Int(1)),
            value: CanonicalScalar::Int(1),
            mismatch: MismatchPolicy::StatementError,
        }])
    };

    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, BRANCH).expect("genesis begins");
    seed.execute(&[create(1), create(2)])
        .expect("seed executes");
    assert!(
        seed.commit(&mut db, REL, SEMANTICS, CommitSeq(1), LogicalCommandSeq(10),)
            .expect("seed commit is evaluated")
            .is_committed()
    );

    let mut left = Transaction::begin(&db, GRAPH, BRANCH).expect("left begins");
    let mut right = Transaction::begin(&db, GRAPH, BRANCH).expect("right begins");
    left.execute(&[guard(2), set(1, 0)]).expect("left executes");
    right
        .execute(&[guard(1), set(2, 0)])
        .expect("right executes");

    let left_trace = left.trace(1).committed_at(CommitSeq(2));
    let right_trace = right.trace(2).committed_at(CommitSeq(3));
    assert!(
        left.commit(&mut db, REL, SEMANTICS, CommitSeq(2), LogicalCommandSeq(20),)
            .expect("left commit is evaluated")
            .is_committed()
    );
    assert!(
        right
            .commit(&mut db, REL, SEMANTICS, CommitSeq(3), LogicalCommandSeq(30),)
            .expect("right commit is evaluated")
            .is_committed()
    );

    assert_eq!(
        dangerous_structures(&[left_trace, right_trace]),
        vec![DangerousStructure {
            pivot: 2,
            incoming_from: 1,
            outgoing_to: 1,
        }],
        "conditional no-op guards must remain visible as transaction reads"
    );
}

// ---------------------------------------------------------------------------
// Transaction histories: a separate language and an independent SI model.
// ---------------------------------------------------------------------------

// SUBSET NOTE: this axis holds topology fixed and varies one integer property.
// It covers snapshot workspaces, explicit and CompareAndSet reads, FCW, abort,
// read-close and the enacted SSI trace law. It does not stand in for the future
// generated observation families for adjacency, range gaps or constraints.

const TXN_VERTICES: [u128; 3] = [1, 2, 3];
const TXN_SEMANTICS: ObjectId = ObjectId([0x31; 32]);
const ABORT_GUARD_VALUE: i64 = i64::MIN;

/// Transaction topology cannot be represented honestly as another graph
/// `Step`: begin captures a snapshot, actions address a private workspace, and
/// commit or abort ends that workspace. Keeping this language separate is what
/// lets the shrinker remove a whole transaction rather than leave orphaned
/// transaction-shaped `Step` arms behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TxnAction {
    Begin { tx: u8 },
    Read { tx: u8, vid: u128 },
    Guard { tx: u8, vid: u128, expected: i64 },
    Write { tx: u8, vid: u128, value: i64 },
    Abort { tx: u8, guard_vid: u128 },
    Commit { tx: u8 },
}

impl TxnAction {
    const fn transaction(self) -> u8 {
        match self {
            Self::Begin { tx }
            | Self::Read { tx, .. }
            | Self::Guard { tx, .. }
            | Self::Write { tx, .. }
            | Self::Abort { tx, .. }
            | Self::Commit { tx } => tx,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TxnCoverage {
    begins: usize,
    concurrent_begins: usize,
    reads: usize,
    read_own_writes: usize,
    guards: usize,
    writes: usize,
    disjoint_commits: usize,
    conflicts: usize,
    aborts_after_writes: usize,
    read_closes: usize,
    histories_with_structures: usize,
}

impl TxnCoverage {
    fn merge(&mut self, next: &Self) {
        self.begins += next.begins;
        self.concurrent_begins += next.concurrent_begins;
        self.reads += next.reads;
        self.read_own_writes += next.read_own_writes;
        self.guards += next.guards;
        self.writes += next.writes;
        self.disjoint_commits += next.disjoint_commits;
        self.conflicts += next.conflicts;
        self.aborts_after_writes += next.aborts_after_writes;
        self.read_closes += next.read_closes;
        self.histories_with_structures += next.histories_with_structures;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TxnGenerated {
    actions: Vec<TxnAction>,
    coverage: TxnCoverage,
}

#[derive(Clone, Debug)]
struct NaiveTransaction {
    snapshot_high: u64,
    workspace: BTreeMap<u128, i64>,
    reads: BTreeSet<ConflictKey>,
    write_vertices: BTreeSet<u128>,
    effect_count: usize,
    statement_failures: usize,
    next_statement: usize,
    mutation_capable: bool,
    aborted_at: Option<usize>,
}

impl NaiveTransaction {
    fn register_statement(&mut self) -> Result<usize, String> {
        let statement = self.next_statement;
        self.next_statement = statement
            .checked_add(1)
            .ok_or_else(|| "statement index exhausted".to_string())?;
        Ok(statement)
    }

    fn ensure_open(&self) -> Result<(), String> {
        if let Some(statement) = self.aborted_at {
            Err(format!(
                "transaction already aborted at statement {statement}"
            ))
        } else {
            Ok(())
        }
    }
}

/// The model deliberately does not reuse `TxnTrace`: matching field names are
/// not an independent oracle if both sides are assembled by the same helper.
#[derive(Clone, Debug, PartialEq, Eq)]
struct NaiveTxnTrace {
    id: usize,
    snapshot_high: u64,
    commit_seq: Option<u64>,
    reads: BTreeSet<ConflictKey>,
    writes: BTreeSet<ConflictKey>,
}

#[derive(Clone, Debug, PartialEq)]
enum TxnModelEvent {
    Began,
    Read(Option<i64>),
    Executed,
    Terminal(TxnOutcome),
}

#[derive(Clone, Debug)]
struct NaiveTxnDatabase {
    committed: BTreeMap<u128, i64>,
    frontier: u64,
    active: BTreeMap<u8, NaiveTransaction>,
    started: BTreeSet<u8>,
    committed_writes: Vec<(u64, BTreeSet<u128>)>,
    history: Vec<NaiveTxnTrace>,
}

impl NaiveTxnDatabase {
    fn seeded() -> Self {
        Self {
            committed: TXN_VERTICES.into_iter().map(|vid| (vid, 1)).collect(),
            frontier: 1,
            active: BTreeMap::new(),
            started: BTreeSet::new(),
            committed_writes: Vec::new(),
            history: Vec::new(),
        }
    }

    fn apply(&mut self, action: TxnAction) -> Result<TxnModelEvent, String> {
        match action {
            TxnAction::Begin { tx } => {
                if self.started.contains(&tx) {
                    return Err(format!("transaction {tx} began more than once"));
                }
                self.started.insert(tx);
                self.active.insert(
                    tx,
                    NaiveTransaction {
                        snapshot_high: self.frontier,
                        workspace: self.committed.clone(),
                        reads: BTreeSet::new(),
                        write_vertices: BTreeSet::new(),
                        effect_count: 0,
                        statement_failures: 0,
                        next_statement: 0,
                        mutation_capable: false,
                        aborted_at: None,
                    },
                );
                Ok(TxnModelEvent::Began)
            }
            TxnAction::Read { tx, vid } => {
                let transaction = self
                    .active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("read names inactive transaction {tx}"))?;
                transaction.ensure_open()?;
                transaction
                    .reads
                    .insert(ConflictKey::Element(ElementId::Vertex(VId(vid))));
                Ok(TxnModelEvent::Read(
                    transaction.workspace.get(&vid).copied(),
                ))
            }
            TxnAction::Guard { tx, vid, expected } => {
                let transaction = self
                    .active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("guard names inactive transaction {tx}"))?;
                transaction.ensure_open()?;
                transaction.register_statement()?;
                transaction
                    .reads
                    .insert(ConflictKey::Element(ElementId::Vertex(VId(vid))));
                if transaction.workspace.get(&vid).copied() == Some(expected) {
                    transaction.mutation_capable = true;
                } else {
                    transaction.statement_failures += 1;
                }
                Ok(TxnModelEvent::Executed)
            }
            TxnAction::Write { tx, vid, value } => {
                let transaction = self
                    .active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("write names inactive transaction {tx}"))?;
                transaction.ensure_open()?;
                transaction.register_statement()?;
                let before = transaction
                    .workspace
                    .get(&vid)
                    .copied()
                    .ok_or_else(|| format!("write names absent vertex {vid}"))?;
                transaction.mutation_capable = true;
                if before != value {
                    transaction.workspace.insert(vid, value);
                    transaction.write_vertices.insert(vid);
                    transaction.effect_count += 1;
                }
                Ok(TxnModelEvent::Executed)
            }
            TxnAction::Abort { tx, guard_vid } => {
                let transaction = self
                    .active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("abort names inactive transaction {tx}"))?;
                transaction.ensure_open()?;
                let statement = transaction.register_statement()?;
                transaction
                    .reads
                    .insert(ConflictKey::Element(ElementId::Vertex(VId(guard_vid))));
                if transaction.workspace.get(&guard_vid).copied() == Some(ABORT_GUARD_VALUE) {
                    return Err("abort guard unexpectedly matched the generated value".to_string());
                }
                transaction.aborted_at = Some(statement);
                Ok(TxnModelEvent::Executed)
            }
            TxnAction::Commit { tx } => self.commit(tx),
        }
    }

    fn commit(&mut self, tx: u8) -> Result<TxnModelEvent, String> {
        let transaction = self
            .active
            .remove(&tx)
            .ok_or_else(|| format!("commit names inactive transaction {tx}"))?;

        let mut writes = BTreeSet::new();
        for vid in &transaction.write_vertices {
            writes.insert(ConflictKey::Element(ElementId::Vertex(VId(*vid))));
        }
        let mut trace = NaiveTxnTrace {
            id: usize::from(tx),
            snapshot_high: transaction.snapshot_high,
            commit_seq: None,
            reads: transaction.reads.clone(),
            writes,
        };

        let outcome = if let Some(statement) = transaction.aborted_at {
            TxnOutcome::Aborted { statement }
        } else if !transaction.mutation_capable {
            TxnOutcome::ReadClosed {
                statement_failures: transaction.statement_failures,
            }
        } else {
            let mut conflicts = BTreeSet::new();
            for (seq, committed_writes) in &self.committed_writes {
                if *seq <= transaction.snapshot_high {
                    continue;
                }
                for vid in transaction.write_vertices.intersection(committed_writes) {
                    conflicts.insert(ConflictKey::Element(ElementId::Vertex(VId(*vid))));
                }
            }

            if conflicts.is_empty() {
                let commit_seq = self
                    .frontier
                    .checked_add(1)
                    .ok_or_else(|| "commit frontier exhausted".to_string())?;
                for vid in &transaction.write_vertices {
                    let value = transaction
                        .workspace
                        .get(vid)
                        .copied()
                        .ok_or_else(|| format!("workspace lost vertex {vid}"))?;
                    self.committed.insert(*vid, value);
                }
                self.frontier = commit_seq;
                self.committed_writes
                    .push((commit_seq, transaction.write_vertices.clone()));
                trace.commit_seq = Some(commit_seq);
                TxnOutcome::WriteCommitted {
                    commit_seq: CommitSeq(commit_seq),
                    effects: transaction.effect_count,
                    statement_failures: transaction.statement_failures,
                }
            } else {
                TxnOutcome::Conflicted {
                    conflicts: conflicts.into_iter().collect(),
                }
            }
        };

        self.history.push(trace);
        Ok(TxnModelEvent::Terminal(outcome))
    }
}

fn naive_antidependency(reader: &NaiveTxnTrace, writer: &NaiveTxnTrace) -> bool {
    if reader.id == writer.id {
        return false;
    }
    let (Some(reader_commit), Some(writer_commit)) = (reader.commit_seq, writer.commit_seq) else {
        return false;
    };
    if writer_commit <= reader.snapshot_high || reader_commit <= writer.snapshot_high {
        return false;
    }
    reader.reads.intersection(&writer.writes).next().is_some()
}

/// Brute-force statement of the history law. This intentionally does not call
/// the subject checker or share its private edge helper.
fn naive_dangerous_structures(history: &[NaiveTxnTrace]) -> Vec<DangerousStructure> {
    let mut found = BTreeSet::new();
    for pivot in history {
        let Some(pivot_commit) = pivot.commit_seq else {
            continue;
        };
        for incoming in history {
            if !naive_antidependency(incoming, pivot) {
                continue;
            }
            for outgoing in history {
                if !naive_antidependency(pivot, outgoing) {
                    continue;
                }
                let Some(outgoing_commit) = outgoing.commit_seq else {
                    continue;
                };
                if outgoing_commit >= pivot_commit {
                    continue;
                }
                found.insert(DangerousStructure {
                    pivot: pivot.id,
                    incoming_from: incoming.id,
                    outgoing_to: outgoing.id,
                });
            }
        }
    }
    found.into_iter().collect()
}

fn txn_create_statement(vid: u128) -> Statement {
    Statement::new(vec![Intent::CreateVertex {
        vid: VId(vid),
        labels: vec![LABEL],
        props: vec![(PROP, CanonicalScalar::Int(1))],
    }])
}

fn txn_write_statement(vid: u128, value: i64) -> Statement {
    Statement::new(vec![Intent::SetProp {
        elem: ElementId::Vertex(VId(vid)),
        name: PROP,
        value: CanonicalScalar::Int(value),
    }])
}

fn txn_guard_statement(vid: u128, expected: i64, mismatch: MismatchPolicy) -> Statement {
    Statement::new(vec![Intent::CompareAndSet {
        elem: ElementId::Vertex(VId(vid)),
        name: PROP,
        expected: Some(CanonicalScalar::Int(expected)),
        value: CanonicalScalar::Int(expected),
        mismatch,
    }])
}

fn seeded_transaction_subject() -> Result<ReferenceDatabase, String> {
    let mut db = ReferenceDatabase::new();
    let mut seed = Transaction::begin_genesis(&db, GRAPH, BRANCH)
        .map_err(|error| format!("seed begin: {error}"))?;
    let statements: Vec<_> = TXN_VERTICES.into_iter().map(txn_create_statement).collect();
    seed.execute(&statements)
        .map_err(|error| format!("seed execute: {error}"))?;
    let outcome = seed
        .commit(
            &mut db,
            REL,
            TXN_SEMANTICS,
            CommitSeq(1),
            LogicalCommandSeq(1),
        )
        .map_err(|error| format!("seed commit: {error}"))?;
    if outcome
        != (TxnOutcome::WriteCommitted {
            commit_seq: CommitSeq(1),
            effects: TXN_VERTICES.len(),
            statement_failures: 0,
        })
    {
        return Err(format!("seed outcome differs: {outcome:?}"));
    }
    Ok(db)
}

fn scalar_int(value: Option<CanonicalScalar>) -> Result<Option<i64>, String> {
    match value {
        None => Ok(None),
        Some(CanonicalScalar::Int(value)) => Ok(Some(value)),
        Some(other) => Err(format!("expected integer property, got {other:?}")),
    }
}

fn graph_property(graph: &ReferenceGraph, vid: u128) -> Result<Option<i64>, String> {
    scalar_int(
        graph
            .vertex(VId(vid))
            .and_then(|vertex| vertex.props.get(&PROP).cloned()),
    )
}

fn compare_transaction_state(
    subject_db: &ReferenceDatabase,
    subject_active: &BTreeMap<u8, Transaction>,
    subject_history: &[TxnTrace],
    model: &NaiveTxnDatabase,
    context: &str,
) -> Result<(), String> {
    let subject_ids: Vec<_> = subject_active.keys().copied().collect();
    let model_ids: Vec<_> = model.active.keys().copied().collect();
    if subject_ids != model_ids {
        return Err(format!(
            "state|{context}: active transactions differ: {subject_ids:?} != {model_ids:?}"
        ));
    }

    for (tx, subject) in subject_active {
        let modeled = model
            .active
            .get(tx)
            .ok_or_else(|| format!("state|{context}: model lost transaction {tx}"))?;
        if subject.snapshot().high().0 != modeled.snapshot_high {
            return Err(format!(
                "state|{context}: transaction {tx} snapshot differs: {} != {}",
                subject.snapshot().high().0,
                modeled.snapshot_high
            ));
        }
        if subject.read_set() != &modeled.reads {
            return Err(format!(
                "state|{context}: transaction {tx} reads differ: {:?} != {:?}",
                subject.read_set(),
                modeled.reads
            ));
        }
        if subject.is_aborted() != modeled.aborted_at.is_some() {
            return Err(format!(
                "state|{context}: transaction {tx} abort state differs"
            ));
        }
        if subject.effects().len() != modeled.effect_count {
            return Err(format!(
                "state|{context}: transaction {tx} effect count differs: {} != {}",
                subject.effects().len(),
                modeled.effect_count
            ));
        }
        if subject.statement_failures() != modeled.statement_failures {
            return Err(format!(
                "state|{context}: transaction {tx} statement failures differ: {} != {}",
                subject.statement_failures(),
                modeled.statement_failures
            ));
        }
        for vid in TXN_VERTICES {
            let actual = graph_property(subject.workspace(), vid)?;
            let expected = modeled.workspace.get(&vid).copied();
            if actual != expected {
                return Err(format!(
                    "state|{context}: transaction {tx} workspace vertex {vid} differs: {actual:?} != {expected:?}"
                ));
            }
        }
    }

    let subject_snapshot = subject_db
        .snapshot(GRAPH, BRANCH)
        .map_err(|error| format!("state|{context}: snapshot: {error}"))?;
    if subject_snapshot.high().0 != model.frontier {
        return Err(format!(
            "state|{context}: frontier differs: {} != {}",
            subject_snapshot.high().0,
            model.frontier
        ));
    }
    let subject_graph = subject_db
        .graph(GRAPH, BRANCH)
        .ok_or_else(|| format!("state|{context}: subject coordinate disappeared"))?;
    for vid in TXN_VERTICES {
        let actual = graph_property(subject_graph, vid)?;
        let expected = model.committed.get(&vid).copied();
        if actual != expected {
            return Err(format!(
                "state|{context}: committed vertex {vid} differs: {actual:?} != {expected:?}"
            ));
        }
    }

    if subject_history.len() != model.history.len() {
        return Err(format!(
            "trace|{context}: history length differs: {} != {}",
            subject_history.len(),
            model.history.len()
        ));
    }
    for (actual, expected) in subject_history.iter().zip(&model.history) {
        if actual.id != expected.id
            || actual.snapshot_high.0 != expected.snapshot_high
            || actual.commit_seq.map(|seq| seq.0) != expected.commit_seq
            || actual.reads != expected.reads
            || actual.writes != expected.writes
        {
            return Err(format!(
                "trace|{context}: trace differs: {actual:?} != {expected:?}"
            ));
        }
    }

    let actual_structures = dangerous_structures(subject_history);
    let expected_structures = naive_dangerous_structures(&model.history);
    if actual_structures != expected_structures {
        return Err(format!(
            "structure|{context}: history analysis differs: {actual_structures:?} != {expected_structures:?}"
        ));
    }
    Ok(())
}

fn run_transaction_history(case: &TxnGenerated) -> Result<(), String> {
    let mut subject_db = seeded_transaction_subject()?;
    let mut subject_active: BTreeMap<u8, Transaction> = BTreeMap::new();
    let mut subject_history = Vec::new();
    let mut model = NaiveTxnDatabase::seeded();

    for (index, action) in case.actions.iter().copied().enumerate() {
        let context = format!("action {index} {action:?}");
        match action {
            TxnAction::Begin { tx } => {
                model
                    .apply(action)
                    .map_err(|error| format!("reachability|{context}: {error}"))?;
                let transaction = Transaction::begin(&subject_db, GRAPH, BRANCH)
                    .map_err(|error| format!("subject|{context}: {error}"))?;
                if subject_active.insert(tx, transaction).is_some() {
                    return Err(format!(
                        "subject|{context}: transaction {tx} was already active"
                    ));
                }
            }
            TxnAction::Read { tx, vid } => {
                let expected = match model
                    .apply(action)
                    .map_err(|error| format!("reachability|{context}: {error}"))?
                {
                    TxnModelEvent::Read(value) => value,
                    other => {
                        return Err(format!(
                            "model|{context}: read produced unexpected event {other:?}"
                        ));
                    }
                };
                let subject = subject_active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("subject|{context}: inactive transaction {tx}"))?;
                let actual = scalar_int(subject.read_property(ElementId::Vertex(VId(vid)), PROP))?;
                if actual != expected {
                    return Err(format!(
                        "read|{context}: value differs: {actual:?} != {expected:?}"
                    ));
                }
            }
            TxnAction::Guard { tx, vid, expected } => {
                model
                    .apply(action)
                    .map_err(|error| format!("reachability|{context}: {error}"))?;
                subject_active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("subject|{context}: inactive transaction {tx}"))?
                    .execute(&[txn_guard_statement(
                        vid,
                        expected,
                        MismatchPolicy::StatementError,
                    )])
                    .map_err(|error| format!("subject|{context}: {error}"))?;
            }
            TxnAction::Write { tx, vid, value } => {
                model
                    .apply(action)
                    .map_err(|error| format!("reachability|{context}: {error}"))?;
                subject_active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("subject|{context}: inactive transaction {tx}"))?
                    .execute(&[txn_write_statement(vid, value)])
                    .map_err(|error| format!("subject|{context}: {error}"))?;
            }
            TxnAction::Abort { tx, guard_vid } => {
                model
                    .apply(action)
                    .map_err(|error| format!("reachability|{context}: {error}"))?;
                subject_active
                    .get_mut(&tx)
                    .ok_or_else(|| format!("subject|{context}: inactive transaction {tx}"))?
                    .execute(&[txn_guard_statement(
                        guard_vid,
                        ABORT_GUARD_VALUE,
                        MismatchPolicy::TxnAbort,
                    )])
                    .map_err(|error| format!("subject|{context}: {error}"))?;
            }
            TxnAction::Commit { tx } => {
                let proposed_seq = model
                    .frontier
                    .checked_add(1)
                    .ok_or_else(|| format!("model|{context}: frontier exhausted"))?;
                let expected = match model
                    .apply(action)
                    .map_err(|error| format!("reachability|{context}: {error}"))?
                {
                    TxnModelEvent::Terminal(outcome) => outcome,
                    other => {
                        return Err(format!(
                            "model|{context}: commit produced unexpected event {other:?}"
                        ));
                    }
                };
                let subject = subject_active
                    .remove(&tx)
                    .ok_or_else(|| format!("subject|{context}: inactive transaction {tx}"))?;
                let trace = subject.trace(usize::from(tx));
                let logical_seq = u64::try_from(index)
                    .ok()
                    .and_then(|value| value.checked_add(2))
                    .ok_or_else(|| format!("subject|{context}: logical sequence exhausted"))?;
                let actual = subject
                    .commit(
                        &mut subject_db,
                        REL,
                        TXN_SEMANTICS,
                        CommitSeq(proposed_seq),
                        LogicalCommandSeq(logical_seq),
                    )
                    .map_err(|error| format!("subject|{context}: {error}"))?;
                let trace = match actual {
                    TxnOutcome::WriteCommitted { commit_seq, .. } => trace.committed_at(commit_seq),
                    _ => trace,
                };
                subject_history.push(trace);
                if actual != expected {
                    return Err(format!(
                        "outcome|{context}: terminal differs: {actual:?} != {expected:?}"
                    ));
                }
            }
        }

        compare_transaction_state(
            &subject_db,
            &subject_active,
            &subject_history,
            &model,
            &context,
        )?;
    }
    Ok(())
}

fn append_transaction_action(
    actions: &mut Vec<TxnAction>,
    model: &mut NaiveTxnDatabase,
    action: TxnAction,
) -> Result<(), String> {
    model
        .apply(action)
        .map_err(|error| format!("generator rejected {action:?}: {error}"))?;
    actions.push(action);
    Ok(())
}

fn transaction_coverage(actions: &[TxnAction]) -> Result<TxnCoverage, String> {
    let mut model = NaiveTxnDatabase::seeded();
    let mut coverage = TxnCoverage::default();
    for action in actions.iter().copied() {
        let active_before = model.active.len();
        let transaction_before = model.active.get(&action.transaction()).cloned();
        let frontier_before = model.frontier;
        let event = model.apply(action)?;
        match action {
            TxnAction::Begin { .. } => {
                coverage.begins += 1;
                coverage.concurrent_begins += usize::from(active_before > 0);
            }
            TxnAction::Read { vid, .. } => {
                coverage.reads += 1;
                coverage.read_own_writes += usize::from(
                    transaction_before
                        .as_ref()
                        .is_some_and(|transaction| transaction.write_vertices.contains(&vid)),
                );
            }
            TxnAction::Guard { .. } => coverage.guards += 1,
            TxnAction::Write { .. } => coverage.writes += 1,
            TxnAction::Abort { .. } => {
                coverage.aborts_after_writes += usize::from(
                    transaction_before
                        .as_ref()
                        .is_some_and(|transaction| !transaction.write_vertices.is_empty()),
                );
            }
            TxnAction::Commit { .. } => match event {
                TxnModelEvent::Terminal(TxnOutcome::WriteCommitted { .. }) => {
                    coverage.disjoint_commits +=
                        usize::from(transaction_before.as_ref().is_some_and(|transaction| {
                            !transaction.write_vertices.is_empty()
                                && transaction.snapshot_high < frontier_before
                        }));
                }
                TxnModelEvent::Terminal(TxnOutcome::Conflicted { .. }) => {
                    coverage.conflicts += 1;
                }
                TxnModelEvent::Terminal(TxnOutcome::ReadClosed { .. }) => {
                    coverage.read_closes += 1;
                }
                TxnModelEvent::Terminal(TxnOutcome::Aborted { .. })
                | TxnModelEvent::Began
                | TxnModelEvent::Read(_)
                | TxnModelEvent::Executed => {}
            },
        }
    }
    coverage.histories_with_structures =
        usize::from(!naive_dangerous_structures(&model.history).is_empty());
    Ok(coverage)
}

fn generate_transaction_history(seed: u64, budget: usize) -> Result<TxnGenerated, String> {
    let mut rng = SplitMix64(seed ^ 0x7478_6e5f_6869_7374);
    let mut model = NaiveTxnDatabase::seeded();
    let mut actions = Vec::new();

    let prefix = match seed % 4 {
        0 => vec![
            TxnAction::Begin { tx: 1 },
            TxnAction::Begin { tx: 2 },
            TxnAction::Guard {
                tx: 1,
                vid: 2,
                expected: 1,
            },
            TxnAction::Guard {
                tx: 2,
                vid: 1,
                expected: 1,
            },
            TxnAction::Write {
                tx: 1,
                vid: 1,
                value: 0,
            },
            TxnAction::Write {
                tx: 2,
                vid: 2,
                value: 0,
            },
            TxnAction::Commit { tx: 1 },
            TxnAction::Commit { tx: 2 },
        ],
        1 => vec![
            TxnAction::Begin { tx: 1 },
            TxnAction::Begin { tx: 2 },
            TxnAction::Write {
                tx: 1,
                vid: 1,
                value: 2,
            },
            TxnAction::Write {
                tx: 2,
                vid: 1,
                value: 3,
            },
            TxnAction::Commit { tx: 1 },
            TxnAction::Commit { tx: 2 },
        ],
        2 => vec![
            TxnAction::Begin { tx: 1 },
            TxnAction::Write {
                tx: 1,
                vid: 1,
                value: 2,
            },
            TxnAction::Abort {
                tx: 1,
                guard_vid: 2,
            },
            TxnAction::Commit { tx: 1 },
            TxnAction::Begin { tx: 2 },
            TxnAction::Read { tx: 2, vid: 1 },
            TxnAction::Commit { tx: 2 },
        ],
        _ => vec![
            TxnAction::Begin { tx: 1 },
            TxnAction::Begin { tx: 2 },
            TxnAction::Write {
                tx: 1,
                vid: 1,
                value: 2,
            },
            TxnAction::Read { tx: 1, vid: 1 },
            TxnAction::Write {
                tx: 2,
                vid: 2,
                value: 3,
            },
            TxnAction::Commit { tx: 1 },
            TxnAction::Commit { tx: 2 },
        ],
    };
    for action in prefix {
        append_transaction_action(&mut actions, &mut model, action)?;
    }

    let mut next_tx = 3u8;
    while actions.len() < budget {
        if next_tx <= 9 && (model.active.is_empty() || (model.active.len() < 3 && rng.chance(28))) {
            append_transaction_action(&mut actions, &mut model, TxnAction::Begin { tx: next_tx })?;
            next_tx += 1;
            continue;
        }
        if model.active.is_empty() {
            break;
        }

        let active_ids: Vec<_> = model.active.keys().copied().collect();
        let tx = active_ids[rng.below(active_ids.len())];
        let transaction = model
            .active
            .get(&tx)
            .ok_or_else(|| format!("generator lost active transaction {tx}"))?;
        let action = if transaction.aborted_at.is_some() {
            TxnAction::Commit { tx }
        } else {
            let vid = TXN_VERTICES[rng.below(TXN_VERTICES.len())];
            match rng.below(100) {
                0..=21 => TxnAction::Read { tx, vid },
                22..=37 => {
                    let current = transaction
                        .workspace
                        .get(&vid)
                        .copied()
                        .ok_or_else(|| format!("generator workspace lost vertex {vid}"))?;
                    let expected = if rng.chance(75) {
                        current
                    } else {
                        current.saturating_add(41)
                    };
                    TxnAction::Guard { tx, vid, expected }
                }
                38..=67 => {
                    let current = transaction
                        .workspace
                        .get(&vid)
                        .copied()
                        .ok_or_else(|| format!("generator workspace lost vertex {vid}"))?;
                    let increment = i64::try_from(rng.next() % 3)
                        .map_err(|_| "generator increment does not fit i64".to_string())?;
                    TxnAction::Write {
                        tx,
                        vid,
                        value: current.saturating_add(1 + increment),
                    }
                }
                68..=77 if !transaction.write_vertices.is_empty() => {
                    TxnAction::Abort { tx, guard_vid: vid }
                }
                _ => TxnAction::Commit { tx },
            }
        };
        append_transaction_action(&mut actions, &mut model, action)?;
    }

    let remaining: Vec<_> = model.active.keys().copied().collect();
    for tx in remaining {
        append_transaction_action(&mut actions, &mut model, TxnAction::Commit { tx })?;
    }
    let coverage = transaction_coverage(&actions)?;
    Ok(TxnGenerated { actions, coverage })
}

fn transaction_failure_kind(error: &str) -> &str {
    error.split_once('|').map_or(error, |(kind, _)| kind)
}

fn drop_transaction(actions: &[TxnAction], tx: u8) -> Vec<TxnAction> {
    actions
        .iter()
        .copied()
        .filter(|action| action.transaction() != tx)
        .collect()
}

fn shrink_transaction_history(case: &TxnGenerated) -> TxnGenerated {
    let Some(original_error) = run_transaction_history(case).err() else {
        return case.clone();
    };
    let failure_kind = transaction_failure_kind(&original_error);
    let mut actions = case.actions.clone();

    loop {
        let mut changed = false;
        let transactions: BTreeSet<_> = actions
            .iter()
            .copied()
            .map(TxnAction::transaction)
            .collect();
        for tx in transactions {
            let candidate = drop_transaction(&actions, tx);
            let candidate_case = TxnGenerated {
                actions: candidate.clone(),
                coverage: TxnCoverage::default(),
            };
            if run_transaction_history(&candidate_case)
                .err()
                .is_some_and(|error| transaction_failure_kind(&error) == failure_kind)
            {
                actions = candidate;
                changed = true;
                break;
            }
        }
        if changed {
            continue;
        }

        for index in 0..actions.len() {
            let mut candidate = actions.clone();
            candidate.remove(index);
            let candidate_case = TxnGenerated {
                actions: candidate.clone(),
                coverage: TxnCoverage::default(),
            };
            if run_transaction_history(&candidate_case)
                .err()
                .is_some_and(|error| transaction_failure_kind(&error) == failure_kind)
            {
                actions = candidate;
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }

    TxnGenerated {
        actions,
        coverage: TxnCoverage::default(),
    }
}

fn report_transaction_failure(seed: u64, case: &TxnGenerated, error: &str) -> String {
    format!(
        "transaction seed {seed}: {error}\n\nPasteable reproduction:\n\n#[test]\nfn transaction_seed_{seed}_reproduction() {{\n    let case = TxnGenerated {{\n        actions: vec!{:#?},\n        coverage: TxnCoverage::default(),\n    }};\n    let error = run_transaction_history(&case).expect_err(\"reproduces\");\n    assert_eq!(transaction_failure_kind(&error), {:?});\n}}",
        case.actions,
        transaction_failure_kind(error)
    )
}

#[test]
fn generated_transaction_histories_match_the_independent_model() -> Result<(), String> {
    let mut coverage = TxnCoverage::default();
    for seed in 0..96u64 {
        let case = generate_transaction_history(seed, 30)?;
        coverage.merge(&case.coverage);
        if let Err(original) = run_transaction_history(&case) {
            let minimal = shrink_transaction_history(&case);
            let minimal_error = run_transaction_history(&minimal)
                .err()
                .unwrap_or_else(|| "shrunk transaction case no longer fails".to_string());
            if transaction_failure_kind(&minimal_error) != transaction_failure_kind(&original) {
                return Err(format!(
                    "transaction shrink changed failure kind: {original} -> {minimal_error}"
                ));
            }
            return Err(report_transaction_failure(seed, &minimal, &minimal_error));
        }
    }

    assert!(coverage.begins > 0, "no transaction begins: {coverage:?}");
    assert!(
        coverage.concurrent_begins > 0,
        "no concurrent transaction snapshots: {coverage:?}"
    );
    assert!(coverage.reads > 0, "no tracked reads: {coverage:?}");
    assert!(
        coverage.read_own_writes > 0,
        "no read-own-write observations: {coverage:?}"
    );
    assert!(coverage.guards > 0, "no conditional guards: {coverage:?}");
    assert!(coverage.writes > 0, "no transaction writes: {coverage:?}");
    assert!(
        coverage.disjoint_commits > 0,
        "no stale-snapshot disjoint commits: {coverage:?}"
    );
    assert!(coverage.conflicts > 0, "no write conflicts: {coverage:?}");
    assert!(
        coverage.aborts_after_writes > 0,
        "no transaction aborts after writing: {coverage:?}"
    );
    assert!(
        coverage.read_closes > 0,
        "no read-only closes: {coverage:?}"
    );
    assert!(
        coverage.histories_with_structures > 0,
        "no dependency structures: {coverage:?}"
    );
    Ok(())
}

#[test]
fn every_generated_transaction_history_is_reachable() -> Result<(), String> {
    for seed in 600..660u64 {
        let case = generate_transaction_history(seed, 26)?;
        let mut model = NaiveTxnDatabase::seeded();
        for (index, action) in case.actions.iter().copied().enumerate() {
            model.apply(action).map_err(|error| {
                format!("seed {seed} action {index} is unreachable: {action:?}: {error}")
            })?;
        }
    }
    Ok(())
}

#[test]
fn a_seed_reproduces_its_transaction_history_exactly() -> Result<(), String> {
    for seed in [7u64, 211, 44_001] {
        assert_eq!(
            generate_transaction_history(seed, 34)?.actions,
            generate_transaction_history(seed, 34)?.actions,
            "transaction seed {seed} drifted"
        );
    }
    Ok(())
}

#[test]
fn transaction_shrink_move_drops_the_whole_lifecycle() {
    let actions = vec![
        TxnAction::Begin { tx: 1 },
        TxnAction::Begin { tx: 2 },
        TxnAction::Write {
            tx: 1,
            vid: 1,
            value: 2,
        },
        TxnAction::Read { tx: 2, vid: 1 },
        TxnAction::Commit { tx: 1 },
        TxnAction::Commit { tx: 2 },
    ];
    let dropped = drop_transaction(&actions, 1);
    assert!(
        dropped.iter().all(|action| action.transaction() != 1),
        "every action owned by transaction 1 must be removed: {dropped:?}"
    );
    assert!(
        dropped.iter().any(|action| action.transaction() == 2),
        "the unrelated transaction must survive: {dropped:?}"
    );
}

#[test]
fn transaction_shrinker_preserves_failure_kind_and_removes_unrelated_work() {
    let case = TxnGenerated {
        actions: vec![
            TxnAction::Begin { tx: 1 },
            TxnAction::Write {
                tx: 1,
                vid: 1,
                value: 2,
            },
            TxnAction::Commit { tx: 1 },
            TxnAction::Begin { tx: 2 },
            TxnAction::Read { tx: 2, vid: 1 },
            TxnAction::Commit { tx: 2 },
            TxnAction::Commit { tx: 9 },
        ],
        coverage: TxnCoverage::default(),
    };
    let original = run_transaction_history(&case).expect_err("inactive commit must fail");
    let minimal = shrink_transaction_history(&case);
    let shrunk = run_transaction_history(&minimal).expect_err("shrunk case must still fail");
    assert_eq!(
        transaction_failure_kind(&shrunk),
        transaction_failure_kind(&original)
    );
    assert!(
        minimal.actions.len() < case.actions.len(),
        "shrinker retained unrelated work: {:?}",
        minimal.actions
    );
    assert!(
        minimal.actions.contains(&TxnAction::Commit { tx: 9 }),
        "shrinker removed the failing action: {:?}",
        minimal.actions
    );
    let report = report_transaction_failure(9, &minimal, &shrunk);
    assert!(
        report.contains("actions: vec!["),
        "the reproduction must construct the Vec it prints: {report}"
    );
}

// ---------------------------------------------------------------------------
// DPOR schedules: real transaction calls under the lab scheduler.
// ---------------------------------------------------------------------------

/// One transaction task in a schedule-generated SI history.
///
/// The transaction axis above deliberately owns arbitrary action histories and
/// their bespoke shrinker. This smaller language owns only the *schedule* seam:
/// each task captures a real snapshot, yields to the lab, then commits through
/// the same `Transaction` API. Keeping the axes separate prevents a scheduler
/// wrapper from silently becoming a second transaction generator.
#[derive(Clone, Debug)]
struct ScheduledTxnProgram {
    tx: u8,
    actions: Vec<TxnAction>,
}

/// The generated workload run under every explored schedule.
#[derive(Clone, Debug)]
struct ScheduledTxnCase {
    programs: Vec<ScheduledTxnProgram>,
}

/// A concrete observation of the scheduler's ordering. Begin owns the
/// transaction's synchronous statement phase; the explicit yield immediately
/// after it is the interleaving point whose ordering DPOR varies.
#[derive(Clone, Debug)]
enum ScheduledEvent {
    Begin {
        tx: u8,
        snapshot_high: u64,
        actions: Vec<TxnAction>,
        reads: Vec<(u128, Option<i64>)>,
    },
    Commit {
        tx: u8,
        outcome: TxnOutcome,
    },
}

struct ScheduledState {
    database: ReferenceDatabase,
    events: Vec<ScheduledEvent>,
    errors: Vec<String>,
}

#[derive(Default, Debug)]
struct ScheduleCoverage {
    runs: usize,
    overlapping_snapshots: usize,
    write_conflicts: usize,
    read_closes: usize,
    aborted: usize,
}

impl ScheduleCoverage {
    fn merge(&mut self, other: &Self) {
        self.runs += other.runs;
        self.overlapping_snapshots += other.overlapping_snapshots;
        self.write_conflicts += other.write_conflicts;
        self.read_closes += other.read_closes;
        self.aborted += other.aborted;
    }
}

/// Build a small, seed-stable family of overlapping transactions. Every case
/// contains a first-committer-wins race; the remaining operations vary the
/// observation shape without duplicating the broad transaction generator.
fn generate_scheduled_case(seed: u64) -> ScheduledTxnCase {
    let mut rng = SplitMix64(seed ^ 0x6470_6f72_5f73_6368);
    let contested = TXN_VERTICES[rng.below(TXN_VERTICES.len())];
    let witness = TXN_VERTICES
        .iter()
        .copied()
        .find(|vertex| *vertex != contested)
        .expect("three fixed vertices always leave a witness");
    let left_value = i64::try_from(2 + rng.below(20)).expect("small value fits i64");
    let right_value = i64::try_from(30 + rng.below(20)).expect("small value fits i64");

    let mut left = vec![TxnAction::Read {
        tx: 1,
        vid: witness,
    }];
    let mut right = vec![TxnAction::Guard {
        tx: 2,
        vid: witness,
        expected: 1,
    }];
    if seed % 3 == 0 {
        left.push(TxnAction::Guard {
            tx: 1,
            vid: contested,
            expected: 1,
        });
    }
    if seed % 5 == 0 {
        right.push(TxnAction::Read {
            tx: 2,
            vid: contested,
        });
    }
    left.push(TxnAction::Write {
        tx: 1,
        vid: contested,
        value: left_value,
    });
    right.push(TxnAction::Write {
        tx: 2,
        vid: contested,
        value: right_value,
    });

    ScheduledTxnCase {
        programs: vec![
            ScheduledTxnProgram {
                tx: 1,
                actions: left,
            },
            ScheduledTxnProgram {
                tx: 2,
                actions: right,
            },
        ],
    }
}

fn execute_scheduled_actions(
    transaction: &mut Transaction,
    tx: u8,
    actions: &[TxnAction],
) -> Result<Vec<(u128, Option<i64>)>, String> {
    let mut reads = Vec::new();
    for action in actions.iter().copied() {
        if action.transaction() != tx {
            return Err(format!(
                "scheduled transaction {tx} received action owned by {}: {action:?}",
                action.transaction()
            ));
        }
        match action {
            TxnAction::Read { vid, .. } => {
                let value =
                    scalar_int(transaction.read_property(ElementId::Vertex(VId(vid)), PROP))?;
                reads.push((vid, value));
            }
            TxnAction::Guard { vid, expected, .. } => transaction
                .execute(&[txn_guard_statement(
                    vid,
                    expected,
                    MismatchPolicy::StatementError,
                )])
                .map_err(|error| format!("scheduled guard: {error}"))?,
            TxnAction::Write { vid, value, .. } => transaction
                .execute(&[txn_write_statement(vid, value)])
                .map_err(|error| format!("scheduled write: {error}"))?,
            TxnAction::Abort { guard_vid, .. } => transaction
                .execute(&[txn_guard_statement(
                    guard_vid,
                    ABORT_GUARD_VALUE,
                    MismatchPolicy::TxnAbort,
                )])
                .map_err(|error| format!("scheduled abort: {error}"))?,
            TxnAction::Begin { .. } | TxnAction::Commit { .. } => {
                return Err(format!(
                    "scheduled transaction program must not contain lifecycle action: {action:?}"
                ));
            }
        }
    }
    Ok(reads)
}

async fn run_scheduled_transaction(
    program: ScheduledTxnProgram,
    state: Arc<AsyncMutex<ScheduledState>>,
) {
    let Some(cx) = Cx::current() else {
        return;
    };
    let result: Result<(), String> = async {
        let mut transaction = {
            let state = state
                .lock(&cx)
                .await
                .map_err(|error| format!("scheduled begin lock: {error}"))?;
            Transaction::begin(&state.database, GRAPH, BRANCH)
                .map_err(|error| format!("scheduled begin: {error}"))?
        };
        let snapshot_high = transaction.snapshot().high().0;
        let reads = execute_scheduled_actions(&mut transaction, program.tx, &program.actions)?;
        {
            let mut state = state
                .lock(&cx)
                .await
                .map_err(|error| format!("scheduled observation lock: {error}"))?;
            state.events.push(ScheduledEvent::Begin {
                tx: program.tx,
                snapshot_high,
                actions: program.actions.clone(),
                reads,
            });
        }

        // This is deliberately between snapshot capture and certification. A
        // yield outside the transaction would exercise only lab bookkeeping;
        // this one changes whether the peer is visible to FCW at commit.
        yield_now().await;

        let mut state = state
            .lock(&cx)
            .await
            .map_err(|error| format!("scheduled commit lock: {error}"))?;
        let next_commit = state
            .database
            .snapshot(GRAPH, BRANCH)
            .map_err(|error| format!("scheduled commit snapshot: {error}"))?
            .high()
            .0
            .checked_add(1)
            .ok_or_else(|| "scheduled commit sequence exhausted".to_string())?;
        let next_logical = state
            .database
            .logical_command_frontier()
            .0
            .checked_add(1)
            .ok_or_else(|| "scheduled logical sequence exhausted".to_string())?;
        let outcome = transaction
            .commit(
                &mut state.database,
                REL,
                TXN_SEMANTICS,
                CommitSeq(next_commit),
                LogicalCommandSeq(next_logical),
            )
            .map_err(|error| format!("scheduled commit: {error}"))?;
        state.events.push(ScheduledEvent::Commit {
            tx: program.tx,
            outcome,
        });
        Ok(())
    }
    .await;

    if let Err(error) = result {
        if let Ok(mut state) = state.lock(&cx).await {
            state.errors.push(error);
        }
    }
}

/// Replay the observed schedule against the independent SI model. The model
/// receives exactly the begin/commit order the lab chose, but never calls the
/// reference transaction implementation.
fn replay_scheduled_events(
    events: &[ScheduledEvent],
) -> Result<(NaiveTxnDatabase, ScheduleCoverage), String> {
    let mut model = NaiveTxnDatabase::seeded();
    let mut coverage = ScheduleCoverage::default();
    let mut active = BTreeSet::new();

    for event in events {
        match event {
            ScheduledEvent::Begin {
                tx,
                snapshot_high,
                actions,
                reads,
            } => {
                coverage.overlapping_snapshots += usize::from(!active.is_empty());
                let begun = model
                    .apply(TxnAction::Begin { tx: *tx })
                    .map_err(|error| format!("model scheduled begin {tx}: {error}"))?;
                if begun != TxnModelEvent::Began {
                    return Err(format!("model scheduled begin {tx} had event {begun:?}"));
                }
                let modeled = model
                    .active
                    .get(tx)
                    .ok_or_else(|| format!("model lost scheduled transaction {tx}"))?;
                if modeled.snapshot_high != *snapshot_high {
                    return Err(format!(
                        "scheduled snapshot {tx} differs: subject {snapshot_high} vs model {}",
                        modeled.snapshot_high
                    ));
                }
                active.insert(*tx);

                let mut expected_reads = Vec::new();
                for action in actions.iter().copied() {
                    let expected = model
                        .apply(action)
                        .map_err(|error| format!("model scheduled action {action:?}: {error}"))?;
                    if let TxnModelEvent::Read(value) = expected {
                        let TxnAction::Read { vid, .. } = action else {
                            return Err(format!(
                                "model read event from non-read action {action:?}"
                            ));
                        };
                        expected_reads.push((vid, value));
                    }
                }
                if expected_reads != *reads {
                    return Err(format!(
                        "scheduled reads for transaction {tx} differ: subject {reads:?} vs model {expected_reads:?}"
                    ));
                }
            }
            ScheduledEvent::Commit { tx, outcome } => {
                let expected = model
                    .apply(TxnAction::Commit { tx: *tx })
                    .map_err(|error| format!("model scheduled commit {tx}: {error}"))?;
                let TxnModelEvent::Terminal(expected) = expected else {
                    return Err(format!(
                        "model scheduled commit {tx} had non-terminal event {expected:?}"
                    ));
                };
                if &expected != outcome {
                    return Err(format!(
                        "scheduled terminal {tx} differs: subject {outcome:?} vs model {expected:?}"
                    ));
                }
                match outcome {
                    TxnOutcome::Conflicted { .. } => coverage.write_conflicts += 1,
                    TxnOutcome::ReadClosed { .. } => coverage.read_closes += 1,
                    TxnOutcome::Aborted { .. } => coverage.aborted += 1,
                    TxnOutcome::WriteCommitted { .. } => {}
                }
                if !active.remove(tx) {
                    return Err(format!("scheduled commit {tx} had no matching begin"));
                }
            }
        }
    }
    if !active.is_empty() {
        return Err(format!(
            "scheduled model retained active transactions: {active:?}"
        ));
    }
    coverage.runs = 1;
    Ok((model, coverage))
}

fn verify_scheduled_state(state: &ScheduledState) -> Result<ScheduleCoverage, String> {
    if !state.errors.is_empty() {
        return Err(format!("scheduled task errors: {:?}", state.errors));
    }
    let (model, coverage) = replay_scheduled_events(&state.events)?;
    let snapshot = state
        .database
        .snapshot(GRAPH, BRANCH)
        .map_err(|error| format!("scheduled final snapshot: {error}"))?;
    if snapshot.high().0 != model.frontier {
        return Err(format!(
            "scheduled final frontier differs: subject {} vs model {}",
            snapshot.high().0,
            model.frontier
        ));
    }
    let graph = state
        .database
        .graph(GRAPH, BRANCH)
        .ok_or_else(|| "scheduled subject lost its coordinate".to_string())?;
    for vid in TXN_VERTICES {
        let actual = graph_property(graph, vid)?;
        let expected = model.committed.get(&vid).copied();
        if actual != expected {
            return Err(format!(
                "scheduled final vertex {vid} differs: subject {actual:?} vs model {expected:?}"
            ));
        }
    }
    Ok(coverage)
}

fn run_scheduled_case(
    runtime: &mut LabRuntime,
    case: &ScheduledTxnCase,
) -> Result<ScheduleCoverage, String> {
    let state = Arc::new(AsyncMutex::with_name(
        "fgdb_generated_history_schedule",
        ScheduledState {
            database: seeded_transaction_subject()?,
            events: Vec::new(),
            errors: Vec::new(),
        },
    ));
    let root = runtime.state.create_root_region(Budget::INFINITE);
    for program in &case.programs {
        let program = program.clone();
        let task_state = Arc::clone(&state);
        let (task, _handle) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                run_scheduled_transaction(program, task_state).await;
            })
            .map_err(|error| format!("create scheduled task: {error}"))?;
        runtime.scheduler.lock().schedule(task, 0);
    }
    let report = runtime.run_until_quiescent_with_report();
    if !report.lab_test_passed() {
        return Err(format!(
            "scheduled lab run did not pass: quiescent={} invariants={:?}",
            report.quiescent, report.invariant_violations
        ));
    }
    let state = state
        .try_lock()
        .map_err(|error| format!("scheduled final state lock: {error}"))?;
    verify_scheduled_state(&state)
}

#[test]
fn generated_transaction_schedules_match_the_independent_si_model_under_dpor() -> Result<(), String>
{
    let mut coverage = ScheduleCoverage::default();
    for history_seed in 0..16u64 {
        let case = generate_scheduled_case(history_seed);
        let results = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&results);
        let mut explorer = DporExplorer::new(
            ExplorerConfig::new(history_seed, 24)
                .worker_count(2)
                .max_steps(2_000),
        );
        let report = explorer.explore(|runtime| {
            let result = run_scheduled_case(runtime, &case);
            captured
                .lock()
                .expect("schedule results mutex is not poisoned")
                .push(result);
        });
        if report.has_violations() {
            return Err(format!(
                "DPOR runtime invariant violation for history seed {history_seed}: {:?}",
                report.violations
            ));
        }
        let dpor = explorer.dpor_coverage();
        if report.total_runs < 2 {
            return Err(format!(
                "DPOR explored fewer than two schedules for history seed {history_seed}"
            ));
        }
        if dpor.total_backtrack_points == 0 {
            return Err(format!(
                "DPOR found no backtrack point for history seed {history_seed}"
            ));
        }
        let results = results
            .lock()
            .map_err(|error| format!("schedule results mutex poisoned: {error}"))?;
        for result in results.iter() {
            coverage.merge(result.as_ref().map_err(|error| {
                format!("history seed {history_seed} schedule differential: {error}")
            })?);
        }
    }
    assert!(coverage.runs > 0, "DPOR did not execute a schedule");
    assert!(
        coverage.overlapping_snapshots > 0,
        "DPOR never overlapped two transaction snapshots: {coverage:?}"
    );
    assert!(
        coverage.write_conflicts > 0,
        "DPOR never reached a first-committer-wins conflict: {coverage:?}"
    );
    Ok(())
}

#[test]
fn scheduled_differential_rejects_a_tampered_conflict_outcome() {
    let case = ScheduledTxnCase {
        programs: vec![
            ScheduledTxnProgram {
                tx: 1,
                actions: vec![TxnAction::Write {
                    tx: 1,
                    vid: 1,
                    value: 2,
                }],
            },
            ScheduledTxnProgram {
                tx: 2,
                actions: vec![TxnAction::Write {
                    tx: 2,
                    vid: 1,
                    value: 3,
                }],
            },
        ],
    };
    let events = vec![
        ScheduledEvent::Begin {
            tx: 1,
            snapshot_high: 1,
            actions: case.programs[0].actions.clone(),
            reads: vec![],
        },
        ScheduledEvent::Begin {
            tx: 2,
            snapshot_high: 1,
            actions: case.programs[1].actions.clone(),
            reads: vec![],
        },
        ScheduledEvent::Commit {
            tx: 1,
            outcome: TxnOutcome::WriteCommitted {
                commit_seq: CommitSeq(2),
                effects: 1,
                statement_failures: 0,
            },
        },
        // MUTATION CONTROL: the second writer began at the same snapshot and
        // must conflict. A fabricated commit is rejected by the independent
        // replay before this test can pass.
        ScheduledEvent::Commit {
            tx: 2,
            outcome: TxnOutcome::WriteCommitted {
                commit_seq: CommitSeq(3),
                effects: 1,
                statement_failures: 0,
            },
        },
    ];
    let error = replay_scheduled_events(&events).expect_err("tampered terminal must be red");
    assert!(
        error.contains("scheduled terminal 2 differs"),
        "unexpected schedule-differential failure: {error}"
    );
}
