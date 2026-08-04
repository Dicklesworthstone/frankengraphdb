//! The scenario registry's completeness (plan §15.1, bead fgdb-verif-sim-q97e).
//!
//! The registry is a const table because a replay must resolve its scenario id
//! in a **fresh process**: a runtime registry would make resolution depend on
//! which registration calls a binary happened to run first, so the same filed
//! artifact would replay in one process and fail in another. Doctrine #1 also
//! rules out the usual `linkme`/`inventory` answer — an external crate.
//!
//! The cost of a hand-written table is that it can go stale, so the tests here
//! exist to make staleness detectable rather than to admire the table:
//! `scenario_registry_is_complete` fails the moment a variant exists without a
//! row. It is the second half of a chain whose first half is a compile error
//! (`Scenario::index`'s exhaustive match), which is why the table can be
//! trusted without a proc macro.

use fgdb_sim::artifact::{SCENARIOS, Scenario, resolve};
use std::collections::BTreeSet;

/// THE COMPLETENESS TEST. Every variant has exactly one row, at its own index.
#[test]
fn scenario_registry_is_complete() {
    assert_eq!(
        SCENARIOS.len(),
        Scenario::COUNT,
        "the table and the variant count disagree; a variant was added without a row"
    );

    // Indices are dense and unique over 0..COUNT, which is what makes
    // `SCENARIOS[scenario.index()]` a total lookup rather than a lucky one.
    let indices: BTreeSet<usize> = SCENARIOS
        .iter()
        .map(|entry| entry.scenario.index())
        .collect();
    assert_eq!(
        indices.len(),
        Scenario::COUNT,
        "two scenarios share an index: {indices:?}"
    );
    assert_eq!(
        indices.iter().copied().max(),
        Some(Scenario::COUNT - 1),
        "indices are not dense over 0..COUNT: {indices:?}"
    );

    // Each row must sit AT its own index, or `entry()` returns another
    // scenario's row while every count above still passes.
    for (position, entry) in SCENARIOS.iter().enumerate() {
        assert_eq!(
            entry.scenario.index(),
            position,
            "row {position} holds {:?}, whose index is {}",
            entry.scenario,
            entry.scenario.index()
        );
    }
}

#[test]
fn every_row_agrees_with_the_scenario_it_names() {
    for entry in &SCENARIOS {
        assert_eq!(
            entry.id,
            entry.scenario.id(),
            "row id and Scenario::id disagree; the replay string would name a different row"
        );
        assert!(
            !entry.asserts.trim().is_empty(),
            "{:?}: empty asserts",
            entry.id
        );
        assert!(
            !entry.state_model.trim().is_empty(),
            "{:?}: a scenario with no declared state model cannot be reported as \
             BoundedExhausted, which is required to name the model it exhausted",
            entry.id
        );
        assert_eq!(
            entry.scenario.entry(),
            entry,
            "entry() returned another row"
        );
    }
}

#[test]
fn ids_are_unique() {
    let ids: BTreeSet<&str> = SCENARIOS.iter().map(|entry| entry.id).collect();
    assert_eq!(
        ids.len(),
        SCENARIOS.len(),
        "duplicate scenario id: resolve() would silently prefer one of them"
    );
}

#[test]
fn every_registered_id_resolves_back_to_its_scenario() {
    for entry in &SCENARIOS {
        assert_eq!(
            resolve(entry.id),
            Ok(entry.scenario),
            "{:?} does not resolve to itself",
            entry.id
        );
    }
}

#[test]
fn an_unknown_id_is_refused_and_names_the_registered_set() {
    let error = resolve("no-such-scenario").expect_err("an unknown id must not resolve");
    let rendered = error.to_string();

    assert!(
        rendered.contains("no-such-scenario"),
        "the error must repeat what was asked: {rendered}"
    );
    // The caller is usually holding a replay string from a filed artifact and
    // needs to tell a stale id from a typo. "unknown scenario" alone sends
    // them into the source.
    for entry in &SCENARIOS {
        assert!(
            rendered.contains(entry.id),
            "the error omits registered id {:?}: {rendered}",
            entry.id
        );
    }
}

/// The control. Every test above would pass against a `resolve` that accepted
/// everything, or against one that accepted nothing, if not paired.
#[test]
fn resolve_is_neither_total_nor_empty() {
    assert!(
        resolve("durable-append").is_ok(),
        "resolve rejects a registered id; the refusal tests would then be vacuous"
    );
    assert!(
        resolve("").is_err(),
        "resolve accepts the empty id; the acceptance tests would then be vacuous"
    );
}
