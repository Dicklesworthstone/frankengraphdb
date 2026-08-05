//! The LDFI target registry's honesty (plan §15.1 line 1132, bead fgdb-verif-sim-q97e).
//!
//! The registry's whole purpose is to keep the coverage denominator equal to
//! the plan's rather than to what we happen to have built, so the tests are
//! aimed at that and not at the table's shape:
//!
//! * `every_row_quotes_a_phrase_that_appears_in_the_plan_line` — a row nobody
//!   can find in line 1132 is invented, and inventing rows inflates the
//!   denominator just as omitting them deflates it;
//! * `the_coverage_gap_is_reported_not_hidden` — the gap must be non-zero and
//!   stated, because at this HEAD it emphatically is;
//! * `unreachable_targets_name_an_owning_bead` — an unreachable row without an
//!   owner is a permanent silent zero.

use fgdb_sim::ldfi::{
    Reachability, TARGETS, coverage_statement, reachable_count, unreachable_count,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Plan line 1132 — the LDFI target sentence. 1-based, as cited.
const TARGET_LINE: usize = 1132;

fn plan_line() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/<crate> has a repo root")
        .join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md");
    let plan = std::fs::read_to_string(path).expect("plan is readable");
    plan.lines()
        .nth(TARGET_LINE - 1)
        .expect("plan has line 1132")
        .to_ascii_lowercase()
}

#[test]
fn every_row_quotes_a_phrase_that_appears_in_the_plan_line() {
    let line = plan_line();

    // The anchor first: if line 1132 stops being the LDFI sentence, every
    // assertion below is meaningless and this is what says so.
    for marker in ["lineage-driven fault injection", "d1/d2", "raft"] {
        assert!(
            line.contains(marker),
            "plan line {TARGET_LINE} is not the LDFI target sentence (missing {marker:?})"
        );
    }

    for target in TARGETS {
        // A source phrase may be an ellipsis-joined excerpt ("key ... zero"),
        // so each non-elided fragment must appear rather than the whole string.
        for fragment in target.source_phrase.to_ascii_lowercase().split(" ... ") {
            assert!(
                line.contains(fragment.trim()),
                "target {:?} quotes {fragment:?}, which is not in plan line {TARGET_LINE}",
                target.id
            );
        }
    }
}

#[test]
fn ids_are_unique() {
    let ids: BTreeSet<&str> = TARGETS.iter().map(|target| target.id).collect();
    assert_eq!(
        ids.len(),
        TARGETS.len(),
        "duplicate LDFI target id inflates the denominator"
    );
}

#[test]
fn unreachable_targets_name_an_owning_bead() {
    for target in TARGETS {
        if let Reachability::NotYetBuilt { bead } = target.reachability {
            assert!(
                bead.starts_with("fgdb-"),
                "target {:?} is unreachable with no owning bead: {bead:?}",
                target.id
            );
        }
    }
}

/// THE TEST THE REGISTRY EXISTS FOR. Coverage is reported against the plan's
/// denominator, and the gap is a number rather than an omission.
#[test]
fn the_coverage_gap_is_reported_not_hidden() {
    assert_eq!(
        reachable_count() + unreachable_count(),
        TARGETS.len(),
        "the counts do not partition the table; coverage arithmetic would be wrong"
    );

    // Both sides non-zero, which is the honest state at this HEAD and also the
    // non-vacuity control: with no reachable targets the registry would be
    // aspirational, and with none unreachable it would be lying.
    assert!(
        reachable_count() > 0,
        "no target is reachable; the lab VFS faults D1/D2 writes and syncs today"
    );
    assert!(
        unreachable_count() > 0,
        "every declared target is reachable, which at this HEAD would mean the \
         denominator was quietly redefined to what we built"
    );

    let statement = coverage_statement();
    assert!(
        statement.contains(&TARGETS.len().to_string()),
        "the coverage statement must name the plan's denominator: {statement}"
    );
    assert!(
        statement.contains(&unreachable_count().to_string()),
        "the coverage statement must name the gap: {statement}"
    );
}

#[test]
fn reachable_targets_are_only_the_ones_the_lab_vfs_can_fault() {
    // The lab VFS injects at file writes and syncs. Anything else claiming
    // reachability would be asserting an injector that does not exist —
    // exactly the overclaim this registry is meant to prevent, committed by
    // the registry itself.
    for target in TARGETS {
        if target.reachability.is_reachable() {
            assert!(
                target.id.contains("file-write") || target.id.contains("file-sync"),
                "target {:?} claims reachability, but the only injector that exists \
                 is the lab VFS over file writes and syncs",
                target.id
            );
        }
    }
}
