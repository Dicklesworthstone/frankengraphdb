//! Standalone architecture-decision registry checker.
//!
//! stdout is stable NDJSON; stderr is human diagnostics.  Exit status is 0
//! for a clean registry, 1 for validation violations, and 2 for usage/load
//! failures.

#[allow(dead_code)]
#[path = "../architecture.rs"]
mod architecture;
#[allow(dead_code)]
#[path = "../hash.rs"]
mod hash;
#[allow(dead_code)]
#[path = "../jsonl.rs"]
mod jsonl;
#[allow(dead_code)]
#[path = "../toml.rs"]
mod toml;

use architecture::{
    ArchitectureRegistry, BeadProvenanceEntry, Violation, bead_provenance_index,
    check_source_blocks, effective_claim_classes, load_architecture, load_from_repo,
    provenance_index, validate_architecture,
};
use jsonl::{arr, event, n, s};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> String {
    "usage: architecture-check [--root <repo-root>] [--registry <registry-path>]".into()
}

struct Args {
    root: PathBuf,
    registry: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut arguments = std::env::args().skip(1);
    let mut root = None;
    let mut registry = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--root" => {
                if root.is_some() {
                    return Err(format!("--root may be provided only once\n{}", usage()));
                }
                root = Some(PathBuf::from(
                    arguments.next().ok_or("--root requires a value")?,
                ));
            }
            "--registry" => {
                if registry.is_some() {
                    return Err(format!("--registry may be provided only once\n{}", usage()));
                }
                registry = Some(PathBuf::from(
                    arguments.next().ok_or("--registry requires a value")?,
                ));
            }
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }
    Ok(Args {
        root: root.unwrap_or_else(|| PathBuf::from(".")),
        registry,
    })
}

fn sorted(values: &[String]) -> Vec<String> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    values
}

fn claim_class_map(
    registry: &ArchitectureRegistry,
    root: &std::path::Path,
) -> BTreeMap<String, Vec<String>> {
    effective_claim_classes(registry, root).unwrap_or_else(|_| {
        registry
            .decisions
            .iter()
            .map(|decision| {
                (
                    decision.id.clone(),
                    vec!["architectural_decision".to_string()],
                )
            })
            .collect()
    })
}

fn emit_source_events(registry: &ArchitectureRegistry, root: &std::path::Path) {
    let mut ids: Vec<String> = registry
        .source_blocks
        .iter()
        .map(|block| block.id.clone())
        .collect();
    ids.sort();
    for (id, result) in ids.into_iter().zip(check_source_blocks(registry, root)) {
        match result {
            Ok(check) => println!(
                "{}",
                event(&[
                    ("event", s("source_block_checked")),
                    ("source_block", s(check.id)),
                    ("line_count", n(check.line_count as i64)),
                    ("byte_count", n(check.byte_count as i64)),
                    ("fnv1a64", s(check.fnv1a64)),
                    ("exact_match", jsonl::b(check.exact_match)),
                    ("outcome", s(check.outcome)),
                ])
            ),
            Err(message) => println!(
                "{}",
                event(&[
                    ("event", s("source_block_checked")),
                    ("source_block", s(id)),
                    ("line_count", n(0)),
                    ("byte_count", n(0)),
                    ("fnv1a64", s("")),
                    ("exact_match", jsonl::b(false)),
                    ("outcome", s("fail")),
                    ("message", s(message)),
                ])
            ),
        }
    }
}

fn emit_decision_events(
    registry: &ArchitectureRegistry,
    violations: &[Violation],
    claim_classes: &BTreeMap<String, Vec<String>>,
) {
    let profiles: BTreeMap<&str, &architecture::Profile> = registry
        .profiles
        .iter()
        .map(|profile| (profile.id.as_str(), profile))
        .collect();
    let mut decisions: Vec<&architecture::Decision> = registry.decisions.iter().collect();
    decisions.sort_by(|left, right| left.id.cmp(&right.id));
    for decision in decisions {
        let decision_violations: Vec<&Violation> = violations
            .iter()
            .filter(|violation| violation.decision_id == decision.id)
            .collect();
        let contradiction_classes: BTreeSet<String> = decision_violations
            .iter()
            .map(|violation| violation.contradiction_class.clone())
            .collect();
        let owner_beads = sorted(&decision.owner_beads);
        let owner_crates = sorted(&decision.owner_crates);
        let claim_class = claim_classes
            .get(&decision.id)
            .cloned()
            .unwrap_or_else(|| vec!["architectural_decision".into()])
            .join("+");
        let replay_command = profiles
            .get(decision.profile.as_str())
            .map(|profile| profile.check_command.as_str())
            .unwrap_or("");
        let rationale = profiles
            .get(decision.profile.as_str())
            .map(|profile| profile.rationale.as_str())
            .unwrap_or("");
        println!(
            "{}",
            event(&[
                ("event", s("architecture_decision_checked")),
                ("decision_id", s(&decision.id)),
                ("relationship_kind", s(&decision.relationship_kind)),
                (
                    "owner_bead",
                    s(owner_beads.first().cloned().unwrap_or_default()),
                ),
                (
                    "owner_crate",
                    s(owner_crates.first().cloned().unwrap_or_default()),
                ),
                ("owner_beads", arr(owner_beads)),
                ("owner_crates", arr(owner_crates)),
                ("claim_class", s(claim_class)),
                ("checker_ids", arr(sorted(&decision.checker_ids))),
                ("evidence_ids", arr(sorted(&decision.evidence_ids))),
                ("status", s(&decision.status)),
                ("profile_id", s(&decision.profile)),
                ("rationale", s(rationale)),
                (
                    "contradiction_class",
                    s(if contradiction_classes.is_empty() {
                        "none".into()
                    } else {
                        contradiction_classes
                            .into_iter()
                            .collect::<Vec<_>>()
                            .join("+")
                    }),
                ),
                ("source_anchor", s(&decision.source_anchor)),
                ("replay_command", s(replay_command)),
                (
                    "outcome",
                    s(if decision_violations.is_empty() {
                        "pass"
                    } else {
                        "fail"
                    }),
                ),
            ])
        );
    }
}

fn emit_owner_index(registry: &ArchitectureRegistry, violations: &[Violation]) {
    for entry in provenance_index(registry) {
        let outcome = if violations
            .iter()
            .any(|violation| entry.decision_ids.contains(&violation.decision_id))
        {
            "fail"
        } else {
            "pass"
        };
        println!(
            "{}",
            event(&[
                ("event", s("architecture_owner_indexed")),
                ("ownership_scope", s(architecture::OWNERSHIP_SCOPE)),
                ("owner_kind", s(entry.owner_kind)),
                ("owner_id", s(entry.owner_id)),
                ("decision_ids", arr(entry.decision_ids)),
                ("profile_ids", arr(entry.profile_ids)),
                ("rationales", arr(entry.rationales)),
                ("outcome", s(outcome)),
            ])
        );
    }
}

fn emit_bead_provenance(entries: &[BeadProvenanceEntry], violations: &[Violation]) {
    for entry in entries {
        let outcome = if violations.iter().any(|violation| {
            violation.owner_bead == entry.bead_id
                || entry.decision_ids.contains(&violation.decision_id)
                || (violation.decision_id == "<registry>"
                    && violation.contradiction_class == "bead_provenance")
        }) {
            "fail"
        } else {
            "pass"
        };
        println!(
            "{}",
            event(&[
                ("event", s("architecture_bead_provenance_indexed")),
                ("bead_id", s(&entry.bead_id)),
                ("status", s(&entry.status)),
                ("resolution_class", s(&entry.resolution_class)),
                ("rule_id", s(&entry.rule_id)),
                ("decision_ids", arr(entry.decision_ids.clone())),
                ("profile_ids", arr(entry.profile_ids.clone())),
                ("summaries", arr(entry.summaries.clone())),
                ("rationales", arr(entry.rationales.clone())),
                ("source_anchors", arr(entry.source_anchors.clone())),
                ("replay_commands", arr(entry.replay_commands.clone())),
                ("outcome", s(outcome)),
            ])
        );
    }
}

fn emit_violation(violation: &Violation) {
    println!(
        "{}",
        event(&[
            ("event", s("architecture_violation")),
            ("code", s(&violation.code)),
            ("decision_id", s(&violation.decision_id)),
            ("relationship_kind", s(&violation.relationship_kind)),
            ("owner_bead", s(&violation.owner_bead)),
            ("owner_crate", s(&violation.owner_crate)),
            ("claim_class", s(&violation.claim_class)),
            ("checker_ids", arr(violation.checker_ids.clone())),
            ("evidence_ids", arr(violation.evidence_ids.clone())),
            ("status", s(&violation.status)),
            ("contradiction_class", s(&violation.contradiction_class),),
            ("source_anchor", s(&violation.source_anchor)),
            ("replay_command", s(&violation.replay_command)),
            ("outcome", s("fail")),
            ("message", s(&violation.message)),
        ])
    );
    eprintln!(
        "violation[{}] {} {}: {}",
        violation.code, violation.decision_id, violation.source_anchor, violation.message
    );
}

fn run(root: &std::path::Path, registry_path: Option<&std::path::Path>) -> Result<usize, String> {
    let registry = match registry_path {
        Some(path) if path.is_absolute() => load_architecture(path),
        Some(path) => load_architecture(&root.join(path)),
        None => load_from_repo(root),
    }
    .map_err(|error| error.to_string())?;
    let violations = validate_architecture(&registry, root);
    let claim_classes = claim_class_map(&registry, root);
    let bead_entries = bead_provenance_index(&registry, root).unwrap_or_default();
    println!(
        "{}",
        event(&[
            ("event", s("architecture_registry_checked")),
            ("registry", s(&registry.registry.name)),
            ("decision_count", n(registry.decisions.len() as i64)),
            ("profile_count", n(registry.profiles.len() as i64)),
            ("source_block_count", n(registry.source_blocks.len() as i64)),
            ("bead_count", n(bead_entries.len() as i64)),
            (
                "decision_id_hash",
                s(architecture::recompute_decision_id_hash(&registry)),
            ),
            (
                "bibliography_id_hash",
                s(architecture::recompute_bibliography_id_hash(&registry)),
            ),
            (
                "bibliography_anchor_hash",
                s(architecture::recompute_bibliography_anchor_hash(&registry)),
            ),
            (
                "semantic_contract_hash",
                s(architecture::recompute_semantic_contract_hash(&registry)),
            ),
            (
                "bead_binding_hash",
                s(architecture::recompute_bead_binding_hash(&bead_entries)),
            ),
            ("violations", n(violations.len() as i64)),
            (
                "outcome",
                s(if violations.is_empty() {
                    "pass"
                } else {
                    "fail"
                }),
            ),
        ])
    );
    emit_source_events(&registry, root);
    emit_decision_events(&registry, &violations, &claim_classes);
    emit_owner_index(&registry, &violations);
    emit_bead_provenance(&bead_entries, &violations);
    for violation in &violations {
        emit_violation(violation);
    }
    Ok(violations.len())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    match run(&args.root, args.registry.as_deref()) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(error) => {
            println!(
                "{}",
                event(&[
                    ("event", s("architecture_load_error")),
                    ("root", s(args.root.display().to_string())),
                    ("outcome", s("load_error")),
                    ("message", s(&error)),
                ])
            );
            eprintln!("architecture-check load error: {error}");
            ExitCode::from(2)
        }
    }
}
