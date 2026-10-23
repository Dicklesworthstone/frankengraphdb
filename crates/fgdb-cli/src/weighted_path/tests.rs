use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};

const SOURCE: VId = VId(1_u128 << 100);
const TARGET: VId = VId(u128::MAX);
const RELATION: RelationId = RelationId(7);
const WEIGHT: PropertyKeyId = PropertyKeyId(9);

fn okay<T>(result: Result<T, Failure>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}

fn args(text: &str, source: VId, target: VId) -> Vec<String> {
    vec![
        "--db".into(),
        "never-opened-database".into(),
        "--key-file".into(),
        "never-opened-key".into(),
        "--relation".into(),
        "ROAD=7".into(),
        "--property".into(),
        "distance=9".into(),
        "--path-source".into(),
        source.0.to_string(),
        "--path-target".into(),
        target.0.to_string(),
        text.into(),
    ]
}

fn options(args: &[String]) -> Options {
    let mut options = okay(crate::parse(args, "query"));
    for (name, raw) in &options.raw_params {
        let value = okay(crate::parameter(raw, None));
        options.params.insert(name, value).unwrap();
    }
    options
}

fn statement(selector: &str, source: &str, target: &str, hops: &str) -> String {
    format!(
        "MATCH p = {selector} WALK ({source})-[e:ROAD*{hops}]->({target}) \
         COST e.distance RETURN p AS route"
    )
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(RELATION);
    for vertex in [SOURCE, VId(1), TARGET, VId(99)] {
        batch.create_vertex(vertex, vec![], vec![]);
    }
    for (edge, source, target, cost) in [
        (10, SOURCE, TARGET, 9),
        (20, SOURCE, VId(1), 4),
        (21, SOURCE, VId(1), 4),
        (30, VId(1), TARGET, -2),
    ] {
        batch.add_edge(
            EId(edge),
            source,
            target,
            vec![(WEIGHT, CanonicalScalar::Int(cost))],
        );
    }
    db.write(cx, batch).await.unwrap()
}

fn output(view: &EmbeddedReadView, cx: &QueryCx, options: &Options, robot: bool) -> String {
    let prepared = okay(prepare(options));
    let mut bytes = Vec::new();
    okay(run(view, cx, &prepared, robot, &mut bytes));
    String::from_utf8(bytes).unwrap()
}

fn rows(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| line.contains(r#""event":"row""#))
        .collect()
}

#[test]
fn endpoint_flags_preserve_full_identities_require_a_pair_and_reject_invalid_mixes() {
    let text = statement("ANY CHEAPEST", "s", "t", "1..2");
    let arguments = args(&text, SOURCE, TARGET);
    let parsed = options(&arguments);
    assert_eq!(parsed.weighted_path.source, Some(SOURCE));
    assert_eq!(parsed.weighted_path.target, Some(TARGET));
    assert!(prepare(&parsed).is_ok());

    for flag in PathFlags::FLAGS {
        let mut missing = arguments.clone();
        let at = missing.iter().position(|value| value == flag).unwrap();
        missing.drain(at..at + 2);
        let error = crate::parse(&missing, "query").err().unwrap();
        assert_eq!(error.code, 2);
        assert!(error.message.contains("must be supplied together"));

        let mut repeated = arguments.clone();
        repeated.extend([flag.into(), "1".into()]);
        assert_eq!(crate::parse(&repeated, "query").err().unwrap().code, 2);

        for value in [
            "",
            "+1",
            "-1",
            " 1",
            "1 ",
            "1.5",
            "0x10",
            "340282366920938463463374607431768211456",
            "secret",
        ] {
            let mut flags = PathFlags::default();
            let error = flags.set(flag, value).unwrap_err();
            assert_eq!(error.code, 2);
            assert!(!error.message.contains("secret"));
        }
    }

    for forbidden in [
        vec!["--stream"],
        vec!["--sealed"],
        vec!["--buffered"],
        vec!["--spill-dir", "unused"],
        vec!["--certify-to", "unused"],
        vec!["--graph-relation", "ROAD"],
        vec!["--weight", "distance"],
    ] {
        let mut arguments = arguments.clone();
        arguments.extend(forbidden.into_iter().map(str::to_owned));
        assert_eq!(crate::parse(&arguments, "query").err().unwrap().code, 2);
    }

    let call = args("CALL fnx.pagerank() YIELD vertex, score", SOURCE, TARGET);
    assert_eq!(crate::parse(&call, "query").err().unwrap().code, 2);
    for command in ["write", "diff", "transaction", "replay", "search"] {
        assert_eq!(crate::parse(&arguments, command).err().unwrap().code, 2);
    }
}

#[test]
fn sequence_flag_routes_after_endpoints_without_changing_standalone_analytics() {
    let text = statement("ANY CHEAPEST", "s", "t", "1..2");
    for seq in [0, u64::MAX] {
        for before in [false, true] {
            let mut arguments = args(&text, SOURCE, TARGET);
            let flags = ["--as-of".to_owned(), seq.to_string()];
            if before {
                arguments.splice(0..0, flags);
            } else {
                arguments.extend(flags);
            }
            let prepared = okay(prepare(&options(&arguments)));
            assert_eq!(prepared.as_of, Some(CommitSeq(seq)));
            arguments.extend(["--as-of".into(), "0".into()]);
            assert_eq!(crate::parse(&arguments, "query").err().unwrap().code, 2);
        }
    }
    for invalid in ["+1", "-1", "1.0", "18446744073709551616", "secret"] {
        let mut arguments = args(&text, SOURCE, TARGET);
        arguments.extend(["--as-of".into(), invalid.into()]);
        let error = crate::parse(&arguments, "query").err().unwrap();
        assert_eq!(error.code, 2);
        assert!(!error.message.contains("secret"));
    }

    let analytics: Vec<String> = [
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--as-of",
        "1",
        "CALL fnx.pagerank() YIELD vertex, score",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let parsed = okay(crate::parse(&analytics, "query"));
    assert!(parsed.fnx_call);
    assert!(crate::fnx::prepare(&parsed).is_ok());
    assert!(!parsed.weighted_path.enabled());
}

#[test]
fn weighted_definition_and_arguments_bind_without_opening_the_named_database() {
    let text = format!("{};  ", statement("CHEAPEST $k", "s", "t", "$lo..$hi"));
    let mut arguments = args(&text, SOURCE, TARGET);
    arguments.extend([
        "--param".into(),
        "k=int:3".into(),
        "--param".into(),
        "lo=uint:1".into(),
        "--param".into(),
        "hi=int:2".into(),
    ]);
    let bound = okay(prepare(&options(&arguments)));
    assert_eq!(bound.request.ranked_count(), Some(3));
    assert_eq!(bound.request.column_name(), "route");

    for replacement in [
        statement("ANY CHEAPEST", "s", "s", "1..2"),
        statement("ANY CHEAPEST", "s", "t", "2..1"),
        statement("ANY CHEAPEST", "s", "t", "1..2") + " RETURN p",
        statement("ANY CHEAPEST", "s", "t", "1..2") + ";;",
        "MATCH (n) RETURN n".into(),
    ] {
        let parsed = options(&args(&replacement, SOURCE, TARGET));
        let error = prepare(&parsed).err().unwrap();
        assert_eq!(error.code, 3);
        assert!(error.offset.is_some());
    }

    let missing = options(&args(
        &statement("CHEAPEST $k", "s", "t", "1..2"),
        SOURCE,
        TARGET,
    ));
    assert_eq!(
        prepare(&missing).err().unwrap().refusal,
        crate::refusal::Code::InvalidValue
    );
    let mut unknown = args(&statement("ANY CHEAPEST", "s", "t", "1..2"), SOURCE, TARGET);
    unknown.extend(["--param".into(), "unused=uint:1".into()]);
    assert_eq!(
        prepare(&options(&unknown)).err().unwrap().refusal,
        crate::refusal::Code::InvalidValue
    );
    let mut missing_symbol = args(&statement("ANY CHEAPEST", "s", "t", "1..2"), SOURCE, TARGET);
    let at = missing_symbol
        .iter()
        .position(|value| value == "--relation")
        .unwrap();
    missing_symbol.drain(at..at + 2);
    assert_eq!(
        prepare(&options(&missing_symbol)).err().unwrap().refusal,
        crate::refusal::Code::NotFound
    );
}

#[test]
fn ranked_paths_preserve_parallel_edges_negative_costs_and_exact_historical_cut() {
    let ((), report) = run_async_under_lab(0xc0a5_5101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let ranked = options(&args(
            &statement("CHEAPEST 3", "s", "t", "1..2"),
            SOURCE,
            TARGET,
        ));
        let encoded = output(&pinned, &cx, &ranked, true);
        assert!(
            encoded
                .lines()
                .next()
                .unwrap()
                .contains(r#""columns":["route","route_cost"]"#)
        );
        let paths = rows(&encoded);
        assert_eq!(paths.len(), 3);
        for (row, edges, cost) in [
            (paths[0], r#""edges":["20","30"]"#, "2"),
            (paths[1], r#""edges":["21","30"]"#, "2"),
            (paths[2], r#""edges":["10"]"#, "9"),
        ] {
            assert!(row.contains(edges));
            assert!(row.contains(&format!(r#"{{"type":"wideint","value":"{cost}"}}"#)));
            assert!(row.contains(&SOURCE.0.to_string()));
            assert!(row.contains(&TARGET.0.to_string()));
        }
        assert!(
            encoded
                .lines()
                .last()
                .unwrap()
                .contains(&format!(r#""seq":{},"count":3"#, before.0))
        );
        let human = output(&pinned, &cx, &ranked, false);
        assert!(human.contains("route_cost"));
        assert!(human.contains("path("));
        assert!(human.ends_with("3 row(s)\n"));

        let mut edit = WriteBatch::new(RELATION);
        edit.set_edge_property(EId(30), WEIGHT, Some(CanonicalScalar::Int(12)));
        let after = db.write(&commit, edit).await.unwrap();
        let current = db.read_session().unwrap();
        let one = statement("ANY CHEAPEST", "s", "t", "1..2");
        let current_options = options(&args(&one, SOURCE, TARGET));
        let now = output(&current, &cx, &current_options, true);
        assert!(rows(&now)[0].contains(r#""edges":["10"]"#));
        assert!(
            now.lines()
                .last()
                .unwrap()
                .contains(&format!(r#""seq":{},"count":1"#, after.0))
        );
        let mut historical = args(&one, SOURCE, TARGET);
        historical.extend(["--as-of".into(), before.0.to_string()]);
        let past = output(&current, &cx, &options(&historical), true);
        assert!(rows(&past)[0].contains(r#""edges":["20","30"]"#));
        assert!(
            past.lines()
                .last()
                .unwrap()
                .contains(&format!(r#""seq":{},"count":1"#, before.0))
        );
        assert_eq!(
            past,
            output(&pinned, &cx, &current_options, true),
            "an explicitly selected old cut matches the immutable older view"
        );

        let mut future = args(&one, SOURCE, TARGET);
        future.extend(["--as-of".into(), (after.0 + 1).to_string()]);
        let mut bytes = Vec::new();
        assert!(
            run(
                &current,
                &cx,
                &okay(prepare(&options(&future))),
                true,
                &mut bytes
            )
            .is_err()
        );
        assert!(bytes.is_empty());
        assert_eq!(db.frontier().unwrap(), after);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_zero_rank_and_zero_hop_results_keep_schema_and_success_boundary() {
    let ((), report) = run_async_under_lab(0xc0a5_5102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        for (selector, source, target, hops, count) in [
            ("ANY CHEAPEST", SOURCE, VId(99), "1..2", 0),
            ("CHEAPEST 0", SOURCE, TARGET, "1..2", 0),
            ("ANY CHEAPEST", SOURCE, SOURCE, "0", 1),
        ] {
            let parsed = options(&args(&statement(selector, "s", "t", hops), source, target));
            let encoded = output(&view, &cx, &parsed, true);
            assert!(encoded.contains(r#""columns":["route","route_cost"]"#));
            assert_eq!(rows(&encoded).len(), count);
            assert!(
                encoded
                    .lines()
                    .last()
                    .unwrap()
                    .contains(&format!(r#""count":{count}"#))
            );
            if count == 1 {
                assert!(rows(&encoded)[0].contains(r#""edges":[]"#));
                assert!(rows(&encoded)[0].contains(r#"{"type":"wideint","value":"0"}"#));
            }
        }
        let alias = statement("ANY CHEAPEST", "s", "t", "0").replace("AS route", "AS route_cost");
        let encoded = output(&view, &cx, &options(&args(&alias, SOURCE, SOURCE)), true);
        assert!(encoded.contains(r#""columns":["route_cost","route_cost_cost"]"#));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn costs_wider_than_int64_are_exact_in_robot_and_human_output() {
    let ((), report) = run_async_under_lab(0xc0a5_5103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut initial = WriteBatch::new(RELATION);
        for vertex in [SOURCE, VId(1), TARGET] {
            initial.create_vertex(vertex, vec![], vec![]);
        }
        initial.add_edge(
            EId(1),
            SOURCE,
            VId(1),
            vec![(WEIGHT, CanonicalScalar::Int(i64::MAX))],
        );
        initial.add_edge(
            EId(2),
            VId(1),
            TARGET,
            vec![(WEIGHT, CanonicalScalar::Int(i64::MAX))],
        );
        db.write(&commit, initial).await.unwrap();
        let parsed = options(&args(
            &statement("ANY CHEAPEST", "s", "t", "2"),
            SOURCE,
            TARGET,
        ));
        for weight in [i64::MAX, i64::MIN] {
            let mut edit = WriteBatch::new(RELATION);
            for edge in [EId(1), EId(2)] {
                edit.set_edge_property(edge, WEIGHT, Some(CanonicalScalar::Int(weight)));
            }
            db.write(&commit, edit).await.unwrap();
            let view = db.read_session().unwrap();
            let cost = i128::from(weight) * 2;
            let encoded = output(&view, &cx, &parsed, true);
            assert!(
                rows(&encoded)[0].contains(&format!(r#"{{"type":"wideint","value":"{cost}"}}"#))
            );
            assert!(output(&view, &cx, &parsed, false).contains(&cost.to_string()));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_limits_and_zero_rank_source_validation_refuse_before_columns() {
    let ((), report) = run_async_under_lab(0xc0a5_5104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        for flag in [
            "--max-snapshot-records",
            "--max-result-rows",
            "--max-work-units",
            "--max-scratch-entries",
        ] {
            let mut arguments = args(&statement("ANY CHEAPEST", "s", "t", "1..2"), SOURCE, TARGET);
            arguments.extend([flag.into(), "0".into()]);
            let prepared = okay(prepare(&options(&arguments)));
            let mut bytes = Vec::new();
            let error = run(&view, &cx, &prepared, true, &mut bytes).unwrap_err();
            assert_eq!(error.refusal, crate::refusal::Code::Budget);
            assert!(bytes.is_empty(), "{flag}");
        }

        let mut invalid = WriteBatch::new(RELATION);
        invalid.set_edge_property(EId(30), WEIGHT, None);
        db.write(&commit, invalid).await.unwrap();
        let view = db.read_session().unwrap();
        let parsed = options(&args(
            &statement("CHEAPEST 0", "s", "t", "1..2"),
            SOURCE,
            TARGET,
        ));
        let mut bytes = Vec::new();
        assert!(run(&view, &cx, &okay(prepare(&parsed)), true, &mut bytes).is_err());
        assert!(bytes.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
