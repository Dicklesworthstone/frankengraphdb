//! registry-check CLI — the claims-lint / registry-validation CI job.
//!
//! Subcommands (all emit deterministic JSONL events on stdout; human
//! diagnostics go to stderr; exit 0 = clean, 1 = violations, 2 = usage or
//! load error):
//!
//!   registry-check validate --root <repo-root>
//!   registry-check lint     --root <repo-root>
//!   registry-check closure  --root <repo-root> --manifest <path>
//!   registry-check hash     --root <repo-root>
//!   registry-check all      --root <repo-root> [--manifest <path>]

use registry_check::closure;
use registry_check::hash::id_table_hash;
use registry_check::jsonl::{arr, event, n, s};
use registry_check::lint;
use registry_check::model::{self, Registries};
use registry_check::validate::{self, Violation, expected_invariant_ids};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Args {
    command: String,
    root: PathBuf,
    manifest: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut argv = std::env::args().skip(1);
    let command = argv.next().ok_or_else(usage)?;
    let mut root: Option<PathBuf> = None;
    let mut manifest: Option<PathBuf> = None;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--root" => {
                root = Some(PathBuf::from(argv.next().ok_or("--root requires a value")?));
            }
            "--manifest" => {
                manifest = Some(PathBuf::from(
                    argv.next().ok_or("--manifest requires a value")?,
                ));
            }
            other => return Err(format!("unknown flag {other:?}\n{}", usage())),
        }
    }
    Ok(Args {
        command,
        root: root.unwrap_or_else(|| PathBuf::from(".")),
        manifest,
    })
}

fn usage() -> String {
    "usage: registry-check <validate|lint|closure|hash|all> --root <repo-root> [--manifest <path>]"
        .to_string()
}

fn load(root: &Path) -> Result<Registries, String> {
    model::load_registries(&root.join("registries")).map_err(|e| e.to_string())
}

/// Emit registry_validated / clause_checked events; return violation count.
fn run_validate(r: &Registries, root: &Path) -> usize {
    let violations = validate::validate_all(r, root);
    let by_registry = |name: &str| -> Vec<&Violation> {
        violations.iter().filter(|v| v.registry == name).collect()
    };
    let row_counts: [(&str, i64); 6] = [
        (
            "constitution",
            (r.constitution.claim_classes.len()
                + r.constitution.constraints.len()
                + r.constitution.bets.len()) as i64,
        ),
        ("invariants", r.invariants.invariants.len() as i64),
        ("evidence", r.evidence.rows.len() as i64),
        ("slo", r.slo.rows.len() as i64),
        ("proof_lanes", r.proof_lanes.len() as i64),
        ("checker_index", r.checker_index.len() as i64),
    ];
    for (name, rows) in row_counts {
        let vs = by_registry(name);
        println!(
            "{}",
            event(&[
                ("event", s("registry_validated")),
                ("registry", s(name)),
                ("rows", n(rows)),
                ("violations", n(vs.len() as i64)),
                ("outcome", s(if vs.is_empty() { "pass" } else { "fail" })),
            ])
        );
    }
    for inv in &r.invariants.invariants {
        for clause in &inv.clauses {
            let clause_violations: Vec<&Violation> = violations
                .iter()
                .filter(|v| v.row_id == clause.key)
                .collect();
            println!(
                "{}",
                event(&[
                    ("event", s("clause_checked")),
                    ("registry", s("invariants")),
                    ("row_id", s(&clause.key)),
                    ("claim_class", s(&clause.claim_class)),
                    ("checker_symbol", s(&clause.checker_entrypoint)),
                    ("negative_test_symbol", s(&clause.negative_test_entrypoint)),
                    (
                        "outcome",
                        s(if clause_violations.is_empty() {
                            "pass"
                        } else {
                            "fail"
                        }),
                    ),
                ])
            );
        }
    }
    for v in &violations {
        println!(
            "{}",
            event(&[
                ("event", s("violation")),
                ("code", s(&v.code)),
                ("registry", s(&v.registry)),
                ("row_id", s(&v.row_id)),
                ("msg", s(&v.msg)),
            ])
        );
        eprintln!(
            "violation[{}] {}::{}: {}",
            v.code, v.registry, v.row_id, v.msg
        );
    }
    violations.len()
}

fn run_hash(r: &Registries) -> usize {
    let actual_ids: Vec<String> = r
        .invariants
        .invariants
        .iter()
        .map(|i| i.id.clone())
        .collect();
    let expected_ids = expected_invariant_ids();
    let recomputed = id_table_hash(&actual_ids);
    let pinned = &r.invariants.twenty_id_hash;
    let ok = recomputed == *pinned && actual_ids == expected_ids;
    let mut fields = vec![
        ("event", s("hash_checked")),
        ("registry", s("invariants")),
        ("pinned", s(pinned)),
        ("recomputed", s(&recomputed)),
        ("outcome", s(if ok { "pass" } else { "fail" })),
    ];
    // On any mismatch, log the exact row-level diff.
    let missing: Vec<String> = expected_ids
        .iter()
        .filter(|id| !actual_ids.contains(id))
        .cloned()
        .collect();
    let extra: Vec<String> = actual_ids
        .iter()
        .filter(|id| !expected_ids.contains(id))
        .cloned()
        .collect();
    if !ok {
        fields.push(("expected_ids", arr(expected_ids.clone())));
        fields.push(("actual_ids", arr(actual_ids.clone())));
        fields.push(("missing", arr(missing)));
        fields.push(("extra", arr(extra)));
    }
    println!("{}", event(&fields));
    usize::from(!ok)
}

fn run_lint(r: &Registries, root: &Path) -> Result<usize, String> {
    let config =
        lint::load_config(&root.join("registries/claims_lint.toml")).map_err(|e| e.to_string())?;
    let registered = lint::registered_markers(r);
    let hits = lint::run(root, &config, &registered).map_err(|e| e.to_string())?;
    for hit in &hits {
        println!(
            "{}",
            event(&[
                ("event", s("lint_hit")),
                ("file", s(&hit.file)),
                ("line", n(hit.line as i64)),
                ("marker", s(&hit.marker)),
                ("text", s(&hit.text)),
            ])
        );
        eprintln!(
            "{}:{}: unregistered claim marker {} in: {}",
            hit.file, hit.line, hit.marker, hit.text
        );
    }
    println!(
        "{}",
        event(&[
            ("event", s("lint_completed")),
            ("files_scanned", n(config.scan.len() as i64)),
            ("hits", n(hits.len() as i64)),
            ("outcome", s(if hits.is_empty() { "pass" } else { "fail" })),
        ])
    );
    Ok(hits.len())
}

fn run_closure(r: &Registries, manifest_path: &Path) -> Result<usize, String> {
    let manifest = model::load_manifest(manifest_path).map_err(|e| e.to_string())?;
    let report = closure::compute(r, &manifest);
    println!(
        "{}",
        event(&[
            ("event", s("closure_computed")),
            ("manifest", s(&report.manifest)),
            ("reachable", n(report.reachable.len() as i64)),
            ("live", n(report.live.len() as i64)),
            ("absent", n(report.absent.len() as i64)),
            ("absent_clauses", arr(report.absent.iter().cloned())),
            ("outcome", s(if report.ok() { "pass" } else { "fail" })),
        ])
    );
    for (capability, clauses) in &report.absent_capabilities {
        println!(
            "{}",
            event(&[
                ("event", s("capability_absent")),
                ("capability", s(capability)),
                ("clauses", arr(clauses.iter().cloned())),
                (
                    "reason",
                    s("reachable clause is not live; the capability is absent")
                ),
            ])
        );
        eprintln!("capability {capability:?} absent: non-live reachable clauses {clauses:?}");
    }
    Ok(report.absent.len())
}

fn run() -> Result<usize, String> {
    let args = parse_args()?;
    let r = load(&args.root)?;
    match args.command.as_str() {
        "validate" => Ok(run_validate(&r, &args.root)),
        "hash" => Ok(run_hash(&r)),
        "lint" => run_lint(&r, &args.root),
        "closure" => {
            let manifest = args.manifest.ok_or("closure requires --manifest <path>")?;
            run_closure(&r, &manifest)
        }
        "all" => {
            let mut failures = run_validate(&r, &args.root);
            failures += run_hash(&r);
            failures += run_lint(&r, &args.root)?;
            let manifest = args
                .manifest
                .unwrap_or_else(|| args.root.join("registries/sample_capability_manifest.toml"));
            failures += run_closure(&r, &manifest)?;
            println!(
                "{}",
                event(&[
                    ("event", s("run_completed")),
                    ("failures", n(failures as i64)),
                    ("outcome", s(if failures == 0 { "pass" } else { "fail" })),
                ])
            );
            Ok(failures)
        }
        other => Err(format!("unknown command {other:?}\n{}", usage())),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(msg) => {
            eprintln!("registry-check: {msg}");
            println!(
                "{}",
                event(&[
                    ("event", s("run_error")),
                    ("msg", s(&msg)),
                    ("outcome", s("error")),
                ])
            );
            ExitCode::from(2)
        }
    }
}
