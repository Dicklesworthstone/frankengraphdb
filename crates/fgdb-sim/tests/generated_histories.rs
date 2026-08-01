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

mod common;

use common::{Step, check_agrees, try_build};

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
