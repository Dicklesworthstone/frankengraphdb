//! Execution-budget propagation through fresh CLI processes and durable state.
//! No mock commit or alternate evaluator: the built binary prepares, stages,
//! refuses/commits, closes and reopens each invocation. Numeric work thresholds
//! are discovered through the native engine, never inferred from elapsed time.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    directory: PathBuf,
    database: String,
    keys: String,
}

impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let instant = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "fgdb-write-budget-{name}-{}-{instant}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        let keys = directory.join("keys");
        std::fs::write(
            &keys,
            format!(
                "{}\n{}\n{}\n",
                "51".repeat(32),
                "52".repeat(32),
                "53".repeat(32)
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let fixture = Self {
            database: directory.join("database").to_str().unwrap().to_owned(),
            keys: keys.to_str().unwrap().to_owned(),
            directory,
        };
        success(&fixture.run("create", &[]));
        success(&fixture.run(
            "write",
            &["CREATE (:Person {p:1}), (:Person {p:2}), (:Person {p:3})"],
        ));
        fixture
    }

    fn run(&self, command: &str, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fgdb"))
            .args([
                "--robot",
                command,
                "--db",
                &self.database,
                "--key-file",
                &self.keys,
                "--label",
                "Person=1",
                "--relation",
                "R=1",
                "--property",
                "p=1",
            ])
            .args(arguments)
            .output()
            .expect("execute native CLI")
    }

    fn snapshot(&self) -> Vec<u8> {
        let output = self.run("query", &["MATCH (n:Person) RETURN n.p AS p ORDER BY p"]);
        success(&output);
        // Includes the committed sequence as well as every returned value.
        output.stdout
    }

    fn edge_snapshot(&self) -> Vec<u8> {
        let output = self.run(
            "query",
            &["MATCH (a:Person)-[r:R]->(b:Person) \
             RETURN a.p AS source,b.p AS destination,r.p AS edge_value \
             ORDER BY source,destination,edge_value"],
        );
        success(&output);
        output.stdout
    }

    fn csv(&self, name: &str, data: &str) -> String {
        let file = self.directory.join(name);
        std::fs::write(&file, data).unwrap();
        file.to_str().unwrap().to_owned()
    }
}

fn text(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

#[track_caller]
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        text(output),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!text(output).contains("\"event\":\"error\""));
    assert_eq!(text(output).matches("\"event\":\"result\"").count(), 1);
}

#[track_caller]
fn refused(output: &Output, diagnostic: &str) {
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout={} stderr={}",
        text(output),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(text(output).contains("\"class\":\"query\""));
    if !diagnostic.is_empty() {
        assert!(
            text(output).contains(diagnostic),
            "expected {diagnostic:?}: {}",
            text(output),
        );
    }
    assert_eq!(
        text(output).lines().count(),
        2,
        "only invocation and terminal error may escape",
    );
    for forbidden in [
        "\"event\":\"row\"",
        "\"event\":\"columns\"",
        "\"event\":\"statement\"",
        "\"event\":\"result\"",
    ] {
        assert!(
            !text(output).contains(forbidden),
            "a refused write leaked output: {}",
            text(output),
        );
    }
}

#[test]
fn plain_and_returning_write_families_use_the_requested_allowance() {
    for (at, statement) in [
        "MATCH (n:Person) SET n.p=7",
        "CREATE (n:Person {p:7}) RETURN n.p AS p",
        "MATCH (n:Person) SET n.p=7 RETURN n.p AS p",
        "MERGE (n:Person {p:1}) ON MATCH SET n.p=7 RETURN n.p AS p",
        "MATCH (a:Person {p:1}), (b:Person {p:2}) MERGE (a)-[r:R]->(b) RETURN r",
        "MATCH (a:Person {p:1}), (b:Person {p:2}) MERGE (a)-[r:R]->(b) \
         ON CREATE SET r.p=7 RETURN r,r.p AS p",
    ]
    .iter()
    .enumerate()
    {
        let fixture = Fixture::new(&format!("family-{at}"));
        let before = fixture.snapshot();
        let zero = fixture.run("write", &[statement, "--max-work-units", "0"]);
        refused(&zero, "WorkUnits");
        assert_eq!(
            fixture.snapshot(),
            before,
            "failed family {at} changed durable state"
        );
        // The positive control rules out a syntax refusal being mistaken for
        // enforcement. No query is retried automatically by production code.
        success(&fixture.run("write", &[statement, "--max-work-units", "1000000"]));
    }
}

#[test]
fn relationship_upsert_budgets_reach_both_entrypoints_and_preserve_the_whole_graph() {
    let statement = "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p>1 \
        MERGE (a)-[r:R]->(b) ON CREATE SET r.p=7 RETURN r,r.p AS p";
    for command in ["write", "transaction"] {
        for (flag, diagnostic) in [
            ("--max-work-units", "WorkUnits"),
            ("--max-snapshot-records", "SnapshotRecords"),
            ("--max-scratch-entries", "ScratchEntries"),
            ("--max-result-rows", "ResultRows"),
        ] {
            let fixture = Fixture::new("edge-upsert-budget");
            let before = (fixture.snapshot(), fixture.edge_snapshot());
            let execute = |limit| {
                if command == "transaction" {
                    fixture.run(command, &["--write", statement, flag, limit])
                } else {
                    fixture.run(command, &[statement, flag, limit])
                }
            };
            refused(&execute("0"), diagnostic);
            assert_eq!(
                (fixture.snapshot(), fixture.edge_snapshot()),
                before,
                "{command} {flag} published part of a refused upsert",
            );
            let accepted = execute("1000000");
            success(&accepted);
            assert_eq!(text(&accepted).matches("\"event\":\"row\"").count(), 2);
            let edges = String::from_utf8(fixture.edge_snapshot()).unwrap();
            assert_eq!(edges.matches("\"event\":\"row\"").count(), 2);
            assert_eq!(edges.matches("\"type\":\"int\",\"value\":\"7\"").count(), 2);
        }
    }
}

#[test]
fn upsert_final_row_caps_do_not_limit_effects_and_late_refusal_restores_existing_edges() {
    for command in ["write", "transaction"] {
        let fixture = Fixture::new("edge-upsert-late");
        let execute = |statement, limit| {
            if command == "transaction" {
                fixture.run(command, &["--write", statement, "--max-result-rows", limit])
            } else {
                fixture.run(command, &[statement, "--max-result-rows", limit])
            }
        };
        // Two effect-producing occurrences reduce to one returned aggregate
        // row. The native final-row contract permits this with a limit of one.
        let aggregate = execute(
            "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p>1 \
             MERGE (a)-[r:R]->(b) ON CREATE SET r.p=7 \
             RETURN count(r) AS total,sum(r.p) AS total_value",
            "1",
        );
        success(&aggregate);
        assert_eq!(text(&aggregate).matches("\"event\":\"row\"").count(), 1);
        assert!(text(&aggregate).contains(
            "\"cells\":[{\"type\":\"int\",\"value\":\"2\"},{\"type\":\"int\",\"value\":\"14\"}]"
        ));
        // Prove that this family accepts a legitimate LIMIT 0 at a zero
        // result allowance before using it to test late expression failure.
        let empty = execute(
            "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p>1 \
             MERGE (a)-[r:R]->(b) ON MATCH SET r.p=r.p \
             RETURN 10/r.p AS bad LIMIT 0",
            "0",
        );
        success(&empty);
        assert!(!text(&empty).contains("\"event\":\"row\""));
        let before = (fixture.snapshot(), fixture.edge_snapshot());
        let edges = std::str::from_utf8(&before.1).unwrap();
        assert_eq!(edges.matches("\"event\":\"row\"").count(), 2);
        assert_eq!(edges.matches("\"type\":\"int\",\"value\":\"7\"").count(), 2);

        let matched = "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p>1 \
            MERGE (a)-[r:R]->(b) ON MATCH SET r.p=r.p+1 RETURN r,r.p AS p";
        refused(&execute(matched, "1"), "ResultRows");
        assert_eq!(
            (fixture.snapshot(), fixture.edge_snapshot()),
            before,
            "late final-row refusal retained an edge-property update",
        );

        // This must execute the failing expression even though LIMIT 0 leaves
        // no deliverable rows. A successful zero-output shortcut would commit
        // the ON MATCH changes and conceal the failure.
        let failing = execute(
            "MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p>1 \
             MERGE (a)-[r:R]->(b) ON MATCH SET r.p=0 \
             RETURN 10/r.p AS bad LIMIT 0",
            "0",
        );
        refused(&failing, "");
        assert_eq!((fixture.snapshot(), fixture.edge_snapshot()), before);

        let accepted = execute(matched, "2");
        success(&accepted);
        assert_eq!(text(&accepted).matches("\"event\":\"row\"").count(), 2);
        let edges = String::from_utf8(fixture.edge_snapshot()).unwrap();
        assert_eq!(edges.matches("\"event\":\"row\"").count(), 2);
        assert_eq!(edges.matches("\"type\":\"int\",\"value\":\"8\"").count(), 2);
    }
}

#[test]
fn snapshot_and_scratch_limits_reach_matched_writes_without_publishing_a_prefix() {
    let fixture = Fixture::new("source-scratch");
    let statement = "MATCH (n:Person) SET n.p=n.p+10 RETURN n.p AS p";
    for (flag, diagnostic) in [
        ("--max-snapshot-records", "SnapshotRecords"),
        ("--max-scratch-entries", "ScratchEntries"),
        ("--max-result-rows", "ResultRows"),
    ] {
        let before = fixture.snapshot();
        refused(&fixture.run("write", &[statement, flag, "0"]), diagnostic);
        assert_eq!(fixture.snapshot(), before);
        success(&fixture.run("write", &[statement, flag, "1000000"]));
    }
}

/// Each probe begins from the same logical graph AND commit history. A successful
/// write advances history even when topology/cardinality stay fixed, so reusing
/// the probe database could make the threshold change during binary search.
fn one_record_bound(csv: bool) -> u64 {
    let statement = "MATCH (n:Person) SET n.p=$value";
    let run = |records: u64| {
        let fixture = Fixture::new(if csv { "csv-probe" } else { "rows-probe" });
        let before = fixture.snapshot();
        let bound = records.to_string();
        let output = if csv {
            let input = fixture.csv("single.csv", "value\n4\n");
            fixture.run(
                "import-csv",
                &[
                    statement,
                    "--input",
                    &input,
                    "--max-snapshot-records",
                    &bound,
                ],
            )
        } else {
            fixture.run(
                "write",
                &[
                    statement,
                    "--rows",
                    "json:[{\"value\":4}]",
                    "--max-snapshot-records",
                    &bound,
                ],
            )
        };
        if output.status.success() {
            success(&output);
            let after = String::from_utf8(fixture.snapshot()).unwrap();
            assert_eq!(after.matches("\"type\":\"int\",\"value\":\"4\"").count(), 3);
            true
        } else {
            refused(&output, "SnapshotRecords");
            assert_eq!(fixture.snapshot(), before);
            false
        }
    };
    let mut low = 0;
    let mut high = 1024;
    assert!(!run(low), "zero must refuse this nonempty source");
    assert!(
        run(high),
        "the positive control must be an executable write"
    );
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        if run(middle) {
            high = middle;
        } else {
            low = middle;
        }
    }
    // Recheck both sides on independent pristine fixtures, never on a database
    // changed by a preceding successful probe.
    assert!(!run(high - 1));
    assert!(run(high));
    high
}

#[test]
fn parameter_and_csv_records_share_one_execution_budget_not_one_each() {
    for csv in [false, true] {
        let bound = one_record_bound(csv);
        let fixture = Fixture::new(if csv { "csv-batch" } else { "rows-batch" });
        let batch = fixture.csv("batch.csv", "value\n10\n20\n");
        let before = fixture.snapshot();
        let statement = "MATCH (n:Person) SET n.p=$value";
        let execute = |records: u64| {
            let records = records.to_string();
            if csv {
                fixture.run(
                    "import-csv",
                    &[
                        statement,
                        "--input",
                        &batch,
                        "--max-snapshot-records",
                        &records,
                    ],
                )
            } else {
                fixture.run(
                    "write",
                    &[
                        statement,
                        "--rows",
                        "json:[{\"value\":10},{\"value\":20}]",
                        "--max-snapshot-records",
                        &records,
                    ],
                )
            }
        };
        let output = execute(bound);
        refused(&output, "SnapshotRecords");
        // Native program positions are zero-based. Prove that record two, not
        // record one or preparation, reaches the shared-budget refusal.
        assert!(
            text(&output).contains("statement 1 SnapshotRecords"),
            "{}",
            text(&output)
        );
        assert_eq!(
            fixture.snapshot(),
            before,
            "first record escaped atomic batch refusal"
        );
        success(&execute(1024));
        let after = String::from_utf8(fixture.snapshot()).unwrap();
        assert_eq!(
            after.matches("\"type\":\"int\",\"value\":\"20\"").count(),
            3
        );
    }
}

#[test]
fn transaction_limits_apply_to_reads_plain_writes_and_returning_writes() {
    let fixture = Fixture::new("transaction");
    for kind in ["--query", "--write"] {
        let statement = if kind == "--query" {
            "MATCH (n:Person) RETURN n.p AS p"
        } else {
            "MATCH (n:Person) SET n.p=n.p+1"
        };
        let before = fixture.snapshot();
        refused(
            &fixture.run(
                "transaction",
                &[kind, statement, "--max-snapshot-records", "0"],
            ),
            "SnapshotRecords",
        );
        assert_eq!(fixture.snapshot(), before);
        success(&fixture.run(
            "transaction",
            &[kind, statement, "--max-snapshot-records", "1000000"],
        ));
    }
    // Use a pristine graph: the preceding positive write changed p values.
    let fixture = Fixture::new("transaction-late-refusal");
    let before = fixture.snapshot();
    let first = "MATCH (n:Person {p:1}) SET n.p=10 RETURN n.p AS p";
    // A one-row first step is legal under the explicit one-row per-operation
    // limit. Its positive control prevents an early first-step refusal from
    // masquerading as rollback of a later failed step.
    let control = fixture.run(
        "transaction",
        &["--write", first, "--max-result-rows", "1", "--rollback"],
    );
    success(&control);
    assert!(text(&control).contains("\"kind\":\"rolled_back\""));
    assert_eq!(fixture.snapshot(), before);
    let late = fixture.run(
        "transaction",
        &[
            "--write",
            first,
            "--query",
            "MATCH (n:Person) RETURN n.p AS p",
            "--max-result-rows",
            "1",
        ],
    );
    refused(&late, "ResultRows");
    assert!(
        text(&late).contains("transaction step 2:"),
        "{}",
        text(&late)
    );
    assert_eq!(fixture.snapshot(), before);
    refused(
        &fixture.run(
            "transaction",
            &[
                "--write",
                "MATCH (n:Person) SET n.p=n.p+10 RETURN n.p AS p",
                "--max-result-rows",
                "1",
            ],
        ),
        "ResultRows",
    );
    assert_eq!(fixture.snapshot(), before);
    let output = fixture.run(
        "transaction",
        &[
            "--query",
            "MATCH (n:Person) RETURN n.p AS p LIMIT 1",
            "--query",
            "MATCH (n:Person) RETURN n.p AS p LIMIT 1",
            "--max-result-rows",
            "1",
        ],
    );
    success(&output);
    assert_eq!(text(&output).matches("\"event\":\"row\"").count(), 2);
    assert!(text(&output).contains("\"kind\":\"read_closed\""));
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn higher_execution_limits_do_not_bypass_csv_effect_limits() {
    let fixture = Fixture::new("effect-cap");
    let input = fixture.csv("creates.csv", "value\n30\n40\n");
    let before = fixture.snapshot();
    let output = fixture.run(
        "import-csv",
        &[
            "CREATE (n:Person {p:$value})",
            "--input",
            &input,
            "--max-snapshot-records",
            "1000000",
            "--max-work-units",
            "1000000",
            "--max-changes",
            "1",
        ],
    );
    refused(&output, "");
    assert_eq!(fixture.snapshot(), before);
    success(&fixture.run(
        "import-csv",
        &[
            "CREATE (n:Person {p:$value})",
            "--input",
            &input,
            "--max-snapshot-records",
            "1000000",
            "--max-work-units",
            "1000000",
            "--max-changes",
            "2",
        ],
    ));
}

#[test]
fn row_limits_also_bound_selection_in_programs_that_return_no_rows() {
    for csv in [false, true] {
        let fixture = Fixture::new(if csv {
            "csv-selections"
        } else {
            "row-selections"
        });
        let input = fixture.csv("selection.csv", "value\n10\n20\n");
        let before = fixture.snapshot();
        let statement = "MATCH (n:Person) SET n.p=$value";
        let execute = |rows: &str| {
            if csv {
                fixture.run(
                    "import-csv",
                    &[statement, "--input", &input, "--max-result-rows", rows],
                )
            } else {
                fixture.run(
                    "write",
                    &[
                        statement,
                        "--rows",
                        "json:[{\"value\":10},{\"value\":20}]",
                        "--max-result-rows",
                        rows,
                    ],
                )
            }
        };
        // Each statement selects three vertices. Three is enough for the first
        // statement but not both; a per-record reset would incorrectly succeed.
        let failed = execute("3");
        refused(&failed, "SelectedRows");
        assert!(
            text(&failed).contains("statement 1 SelectedRows"),
            "{}",
            text(&failed)
        );
        assert_eq!(fixture.snapshot(), before);
        // The exact whole-program boundary is six selections, with no returned
        // rows. The effect counter remains separate from this admission cap.
        let complete = execute("6");
        success(&complete);
        assert!(!text(&complete).contains("\"event\":\"row\""));
        let after = String::from_utf8(fixture.snapshot()).unwrap();
        assert_eq!(
            after.matches("\"type\":\"int\",\"value\":\"20\"").count(),
            3
        );
    }
}

#[test]
fn zero_result_allowance_accepts_an_empty_transaction_read_but_not_a_nonempty_one() {
    let fixture = Fixture::new("zero-delivery");
    let before = fixture.snapshot();
    let empty = fixture.run(
        "transaction",
        &[
            "--query",
            "MATCH (n:Person) WHERE n.p>100 RETURN n.p AS p",
            "--max-result-rows",
            "0",
        ],
    );
    success(&empty);
    assert!(text(&empty).contains("\"kind\":\"read_closed\""));
    assert!(!text(&empty).contains("\"event\":\"row\""));
    assert_eq!(fixture.snapshot(), before);
    let nonempty = fixture.run(
        "transaction",
        &[
            "--query",
            "MATCH (n:Person) RETURN n.p AS p",
            "--max-result-rows",
            "0",
        ],
    );
    refused(&nonempty, "ResultRows");
    assert_eq!(fixture.snapshot(), before);
}
