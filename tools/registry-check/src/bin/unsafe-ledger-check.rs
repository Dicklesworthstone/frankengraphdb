//! `unsafe-ledger-check` — CI entrypoint for the unsafe-boundary ledger
//! (bead `fgdb-w1-unsafe-ledger-icp`; plan §1 constraint 2, §18.1).
//!
//! Exit 0 only when the whole boundary verifies. Emits one NDJSON line per
//! event so CI keeps a machine-readable record of what was actually examined —
//! crates scanned, the per-crate forbid verdict, every site, every orphan row —
//! rather than a bare green bar. A green bar that cannot say what it checked is
//! the failure mode this bead exists to prevent.

use registry_check::jsonl::{arr, b, event, n, s};
use registry_check::unsafe_ledger;
use std::path::PathBuf;
use std::process::ExitCode;

pub const REPLAY_COMMAND: &str =
    "cargo run -p registry-check --bin unsafe-ledger-check -- --root .";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut root = PathBuf::from(".");
    let mut checked_plan = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => match args.next() {
                Some(value) => root = PathBuf::from(value),
                None => {
                    println!(
                        "{}",
                        event(&[
                            ("event", s("run_error")),
                            ("msg", s("--root requires a path")),
                            ("outcome", s("error")),
                        ])
                    );
                    return ExitCode::FAILURE;
                }
            },
            "--checked-plan" => checked_plan = true,
            other => {
                println!(
                    "{}",
                    event(&[
                        ("event", s("run_error")),
                        ("msg", s(format!("unknown argument {other:?}"))),
                        ("outcome", s("error")),
                    ])
                );
                return ExitCode::FAILURE;
            }
        }
    }

    let (report, violations) = unsafe_ledger::check_workspace(&root);

    if checked_plan {
        if !violations.is_empty() {
            for violation in &violations {
                eprintln!(
                    "{} {}: {}",
                    violation.code, violation.subject, violation.message
                );
            }
            return ExitCode::FAILURE;
        }
        let lanes_path = root.join(unsafe_ledger::VERIFICATION_LANES_PATH);
        let lanes = match unsafe_ledger::load_verification_lanes(&lanes_path) {
            Ok(lanes) => lanes,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        };
        let mut emitted = 0_usize;
        for cell in lanes
            .cells
            .iter()
            .filter(|cell| cell.disposition == "checked")
        {
            println!("{}\t{}\t{}", cell.tool, cell.site_row_id, cell.workload);
            emitted += 1;
        }
        if emitted == 0 {
            eprintln!("the checked unsafe-verification plan is empty");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    // The scanner's own control is reported FIRST and explicitly: every
    // zero-site conclusion below is licensed by it, so a reader can tell
    // whether an empty unsafe surface was proven or merely assumed.
    let licensed = report.scanner_self_test_sites == unsafe_ledger::SCANNER_FIXTURE_SITES;
    println!(
        "{}",
        event(&[
            ("event", s("unsafe_scanner_self_test")),
            ("sites_found", n(report.scanner_self_test_sites as i64)),
            (
                "sites_expected",
                n(unsafe_ledger::SCANNER_FIXTURE_SITES as i64)
            ),
            ("licensed", b(licensed)),
            ("outcome", s(if licensed { "pass" } else { "fail" })),
        ])
    );

    println!(
        "{}",
        event(&[
            ("event", s("unsafe_verification_matrix")),
            ("lanes", n(report.verification_lanes as i64)),
            ("cells", n(report.verification_cells as i64)),
            ("checked", n(report.checked_cells as i64)),
            ("candidate", n(report.candidate_cells as i64)),
            ("excluded", n(report.excluded_cells as i64)),
        ])
    );

    // The API reader's control, for the same reason and read the same way: it
    // licenses every "this island exports nothing unsafe" line below.
    let api_licensed =
        report.safe_facing_self_test_findings == unsafe_ledger::SAFE_FACING_FIXTURE_FINDINGS;
    println!(
        "{}",
        event(&[
            ("event", s("safe_facing_self_test")),
            (
                "findings_found",
                n(report.safe_facing_self_test_findings as i64)
            ),
            (
                "findings_expected",
                n(unsafe_ledger::SAFE_FACING_FIXTURE_FINDINGS as i64)
            ),
            ("licensed", b(api_licensed)),
            ("outcome", s(if api_licensed { "pass" } else { "fail" })),
        ])
    );

    // What the safe-facing conclusion is quantified over. A zero here means the
    // rule was enforced against nothing, and a green bar that cannot say how
    // many islands and items it read is the failure mode this bead is about.
    println!(
        "{}",
        event(&[
            ("event", s("island_public_api_scanned")),
            ("islands", n(report.islands_api_scanned as i64)),
            ("files", n(report.island_api_files as i64)),
            ("public_items", n(report.island_public_items as i64)),
        ])
    );

    for (krate, inherits) in &report.forbid_verdicts {
        println!(
            "{}",
            event(&[
                ("event", s("crate_forbid_verdict")),
                ("crate", s(krate)),
                ("inherits_workspace_forbid", b(*inherits)),
            ])
        );
    }

    println!(
        "{}",
        event(&[
            ("event", s("unsafe_sites_scanned")),
            ("count", n(report.scanned_sites.len() as i64)),
            (
                "sites",
                arr(report
                    .scanned_sites
                    .iter()
                    .map(|site| format!("{}:{} {}", site.path, site.line, site.symbol)))
            ),
        ])
    );

    println!(
        "{}",
        event(&[
            ("event", s("ledger_orphan_rows")),
            ("count", n(report.orphan_rows.len() as i64)),
            ("rows", arr(report.orphan_rows.iter().cloned())),
        ])
    );

    for violation in &violations {
        println!(
            "{}",
            event(&[
                ("event", s("unsafe_boundary_violation")),
                ("code", s(&violation.code)),
                ("subject", s(&violation.subject)),
                ("source_anchor", s(&violation.source_anchor)),
                ("msg", s(&violation.message)),
                ("replay_command", s(REPLAY_COMMAND)),
                ("outcome", s("fail")),
            ])
        );
    }

    let failed = !violations.is_empty();
    println!(
        "{}",
        event(&[
            ("event", s("unsafe_ledger_completed")),
            ("crates_scanned", n(report.crates_scanned as i64)),
            ("sites", n(report.scanned_sites.len() as i64)),
            ("orphan_rows", n(report.orphan_rows.len() as i64)),
            ("islands_api_scanned", n(report.islands_api_scanned as i64)),
            ("island_public_items", n(report.island_public_items as i64)),
            ("verification_lanes", n(report.verification_lanes as i64)),
            ("verification_cells", n(report.verification_cells as i64)),
            ("violations", n(violations.len() as i64)),
            ("outcome", s(if failed { "fail" } else { "pass" })),
        ])
    );

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
