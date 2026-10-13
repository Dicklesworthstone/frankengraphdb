//! The real CLI parser and policy composition, not a second flag parser.
use super::*;

const FLAGS: [&str; 4] = [
    "--max-snapshot-records",
    "--max-result-rows",
    "--max-work-units",
    "--max-scratch-entries",
];
const COMMANDS: [&str; 3] = ["write", "import-csv", "transaction"];

fn arguments(command: &str, limits: &[(&str, &str)]) -> Vec<String> {
    let mut args = vec!["--db", "unused", "--key-file", "unused"];
    match command {
        "import-csv" => args.extend(["--input", "unused.csv", "CREATE (n)"]),
        "transaction" => args.extend(["--write", "CREATE (n)"]),
        "write" => args.push("CREATE (n)"),
        _ => {}
    }
    for &(flag, value) in limits {
        args.extend([flag, value]);
    }
    args.into_iter().map(str::to_owned).collect()
}

fn dimensions(policy: GqlQueryPolicy) -> [u64; 4] {
    [
        policy.rows.max_snapshot_records().unwrap(),
        policy.rows.max_result_rows().unwrap(),
        policy.evaluator.max_work_units,
        policy.evaluator.max_scratch_entries,
    ]
}

#[test]
fn write_import_and_transaction_accept_all_execution_limits_without_changing_defaults() {
    for command in COMMANDS {
        let defaults = parse(&arguments(command, &[]), command).unwrap();
        assert_eq!(
            dimensions(defaults.budget.policy()),
            [100_000, 100_000, 10_000_000, 10_000_000],
        );
        for value in ["0", "300000", "18446744073709551615"] {
            let limits: Vec<_> = FLAGS.iter().map(|&flag| (flag, value)).collect();
            let options = parse(&arguments(command, &limits), command).unwrap();
            assert_eq!(
                dimensions(options.budget.policy()),
                [value.parse::<u64>().unwrap(); 4],
            );
        }
    }
}

#[test]
fn each_override_changes_only_its_own_dimension() {
    for command in COMMANDS {
        for (at, flag) in FLAGS.iter().enumerate() {
            let options = parse(&arguments(command, &[(flag, "0")]), command).unwrap();
            let mut expected = dimensions(policy());
            expected[at] = 0;
            assert_eq!(dimensions(options.budget.policy()), expected);
        }
    }
}

#[test]
fn write_limit_syntax_is_single_use_and_decimal_before_any_open() {
    for command in COMMANDS {
        for flag in FLAGS {
            let error = parse(&arguments(command, &[(flag, "1"), (flag, "2")]), command)
                .err()
                .expect("duplicate execution limit must refuse");
            assert_eq!(error.code, 2);
            assert!(error.message.contains("only once"));
            for bad in ["", "-1", "+1", "1.5", " 1", "1 ", "18446744073709551616"] {
                let error = parse(&arguments(command, &[(flag, bad)]), command)
                    .err()
                    .expect("malformed execution limit must refuse");
                assert_eq!(error.code, 2, "{command} {flag} {bad:?}");
            }
        }
    }
}

#[test]
fn execution_flags_are_not_accepted_by_unrelated_commands() {
    for command in ["create", "load", "compact", "scrub", "search"] {
        for flag in FLAGS {
            let error = parse(&arguments(command, &[(flag, "1")]), command)
                .err()
                .expect("inapplicable limit must refuse");
            assert_eq!(error.code, 2);
        }
    }
}

#[test]
fn delivery_caps_intersect_user_rows_and_preserve_the_other_three_limits() {
    for rows in [None, Some(0), Some(3), Some(u64::MAX)] {
        let budget = QueryBudget {
            records: Some(300_000),
            rows,
            work: Some(17),
            scratch: Some(29),
        };
        let expected_rows = rows.unwrap_or(100_000);
        for cap in [0, 1, 3, 100_000, u64::MAX] {
            assert_eq!(
                dimensions(budget.policy_with_result_cap(cap)),
                [300_000, expected_rows.min(cap), 17, 29],
            );
        }
    }
    assert_eq!(
        dimensions(QueryBudget::default().policy_with_result_cap(100_000)),
        dimensions(policy()),
    );
}
