//! Each operation launches the real CLI against its encrypted on-disk store.
//! Expected topology is spelled out independently of the expansion adapter;
//! only exact RRF arithmetic comes from Beacon. Compare complete NDJSON frames,
//! including terminal sequence/count and the null cells of disabled modalities.
#![recursion_limit = "256"]

use fgdb_beacon::expansion::{ExpansionLimits, GraphHit};
use fgdb_beacon::{GraphHybridQuery, WorkBudget};
use fgdb_types::VId;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const INVOCATION: &str = "{\"v\":1,\"event\":\"invocation\"}\n";
const COLUMNS: &str = concat!(
    "{\"v\":1,\"event\":\"columns\",\"columns\":[\"vertex\",\"score\",",
    "\"vector_rank\",\"text_rank\",\"graph_rank\",\"vector_distance\",",
    "\"text_score\",\"graph_hops\"]}\n",
);

struct Store {
    db: PathBuf,
    key: PathBuf,
}

impl Store {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "fgdb-graph-only-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&root).unwrap();
        let key = root.join("keys");
        std::fs::write(
            &key,
            format!(
                "{}\n{}\n{}\n",
                "5a".repeat(32),
                "77".repeat(32),
                "3c".repeat(32),
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self {
            db: root.join("db"),
            key,
        }
    }

    fn command(&self, verb: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fgdb"))
            .args(["--robot", verb, "--db"])
            .arg(&self.db)
            .arg("--key-file")
            .arg(&self.key)
            .args([
                "--label",
                "Doc=1",
                "--label",
                "Other=2",
                "--relation",
                "CITES=1",
                "--relation",
                "OTHER=2",
                "--property",
                "name=1",
            ])
            .args(args)
            .output()
            .expect("run the built fgdb binary")
    }

    fn create(&self) -> u64 {
        single_result(&success(self.command("create", &[])), "created", "}")
    }

    fn write(&self, args: &[&str]) -> u64 {
        single_result(
            &success(self.command("write", args)),
            "written",
            ",\"statements\":1}",
        )
    }

    fn vertex(&self, name: &str, seq: u64) -> VId {
        let query = format!("MATCH (n) WHERE n.name = '{name}' RETURN n AS node");
        let text = success(self.command("query", &[&query]));
        let prefix = concat!(
            "{\"v\":1,\"event\":\"invocation\"}\n",
            "{\"v\":1,\"event\":\"columns\",\"columns\":[\"node\"]}\n",
            "{\"v\":1,\"event\":\"row\",\"cells\":[{\"type\":\"vertex\",\"value\":\"",
        );
        let suffix = format!(
            "\"}}]}}\n{{\"v\":1,\"event\":\"result\",\"kind\":\"rows\",\"seq\":{seq},\"count\":1}}\n",
        );
        let identity = text
            .strip_prefix(prefix)
            .and_then(|body| body.strip_suffix(&suffix))
            .unwrap_or_else(|| panic!("unexpected identity query: {text}"));
        let id = identity.parse::<u128>().expect("one full-width vertex ID");
        assert_eq!(identity, id.to_string());
        VId(id)
    }

    #[track_caller]
    fn search(
        &self,
        seq: u64,
        k: usize,
        weight: u16,
        args: &[&str],
        expected: &[(VId, u32)],
    ) {
        let k_text = k.to_string();
        let weight_text = weight.to_string();
        let mut flags = vec!["--k", &k_text, "--graph-weight", &weight_text];
        flags.extend_from_slice(args);
        let actual = success(self.command("search", &flags));
        assert_eq!(actual, expected_search(seq, k, weight, expected), "{args:?}");
    }
}

#[track_caller]
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "status: {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.stderr.is_empty(), "unexpected diagnostics");
    String::from_utf8(output.stdout).expect("UTF-8 robot output")
}

/// These two command forms have precisely one terminal frame and no row frames.
fn single_result(text: &str, kind: &str, tail: &str) -> u64 {
    let prefix = format!("{INVOCATION}{{\"v\":1,\"event\":\"result\",\"kind\":\"{kind}\",\"seq\":");
    let suffix = format!("{tail}\n");
    let decimal = text
        .strip_prefix(&prefix)
        .and_then(|body| body.strip_suffix(&suffix))
        .unwrap_or_else(|| panic!("unexpected {kind} result: {text}"));
    let seq = decimal.parse::<u64>().expect("one commit sequence");
    assert_eq!(decimal, seq.to_string());
    seq
}

fn expected_search(seq: u64, k: usize, weight: u16, topology: &[(VId, u32)]) -> String {
    let mut population: Vec<_> = topology
        .iter()
        .map(|&(id, hops)| GraphHit { id, hops })
        .collect();
    population.sort_by_key(|hit| (hit.hops, hit.id));
    population.truncate(k);
    let hits = GraphHybridQuery::graph_only(k, u32::try_from(k).unwrap(), weight, 60)
        .unwrap()
        .fuse_graph_only(&population, |_| true, &mut WorkBudget::new(100_000))
        .unwrap();
    let mut output = format!("{INVOCATION}{COLUMNS}");
    for hit in &hits {
        output.push_str(&format!(
            "{{\"v\":1,\"event\":\"row\",\"cells\":[\
             {{\"type\":\"vertex\",\"value\":\"{}\"}},\
             {{\"type\":\"decimal\",\"value\":\"{}\"}},\
             {{\"type\":\"null\"}},{{\"type\":\"null\"}},\
             {{\"type\":\"int\",\"value\":\"{}\"}},\
             {{\"type\":\"null\"}},{{\"type\":\"null\"}},\
             {{\"type\":\"int\",\"value\":\"{}\"}}]}}\n",
            hit.id.0,
            hit.decimal_score,
            hit.graph_rank.unwrap().get(),
            hit.graph_hops.unwrap(),
        ));
    }
    output.push_str(&format!(
        "{{\"v\":1,\"event\":\"result\",\"kind\":\"searched\",\"seq\":{seq},\"count\":{}}}\n",
        hits.len(),
    ));
    output
}

struct Fixture {
    store: Store,
    seq: u64,
    a: VId,
    b: VId,
    c: VId,
    d: VId,
    bridge: VId,
    target: VId,
    isolate: VId,
}

impl Fixture {
    fn new() -> Self {
        let store = Store::new();
        let created = store.create();
        // No text or embedding properties exist. Parallel edges, a self-loop
        // and a cycle must not duplicate endpoints or change minimum hops.
        let inserted = store.write(&[
            "INSERT (a:Doc {name:'a'}), (b:Doc {name:'b'}), \
             (c:Doc {name:'c'}), (d:Doc {name:'d'}), \
             (x:Other {name:'bridge'}), (t:Doc {name:'target'}), \
             (z:Doc {name:'isolate'}), \
             (a)-[:CITES]->(b), (a)-[:CITES]->(b), \
             (b)-[:CITES]->(c), (c)-[:CITES]->(d), \
             (d)-[:CITES]->(a), (b)-[:CITES]->(b), \
             (a)-[:CITES]->(x), (x)-[:CITES]->(t)",
        ]);
        assert_eq!(inserted, created + 1);
        let seq = store.write(&[
            "--write-relation",
            "2",
            "MATCH (a:Doc), (z:Doc) WHERE a.name='a' AND z.name='isolate' \
             INSERT (a)-[:OTHER]->(z)",
        ]);
        assert_eq!(seq, inserted + 1);
        Self {
            a: store.vertex("a", seq),
            b: store.vertex("b", seq),
            c: store.vertex("c", seq),
            d: store.vertex("d", seq),
            bridge: store.vertex("bridge", seq),
            target: store.vertex("target", seq),
            isolate: store.vertex("isolate", seq),
            store,
            seq,
        }
    }
}

#[test]
fn graph_only_reopens_topology_and_preserves_selection_direction_and_endpoint_filtering() {
    let f = Fixture::new();
    let seed = f.a.0.to_string();
    let base = ["--expand-from", &seed, "--max-hops", "2"];
    let mut cites = base.to_vec();
    cites.extend(["--expand-relation", "CITES"]);
    f.store.search(
        f.seq,
        10,
        1,
        &cites,
        &[(f.b, 1), (f.bridge, 1), (f.c, 2), (f.target, 2)],
    );
    let mut selected = cites.clone();
    selected.extend(["--vertex-label", "Doc"]);
    // Label selection constrains transit: target is a Doc but its bridge is not.
    f.store.search(f.seq, 10, 1, &selected, &[(f.b, 1), (f.c, 2)]);
    f.store.search(
        f.seq,
        10,
        1,
        &base,
        &[
            (f.b, 1),
            (f.bridge, 1),
            (f.isolate, 1),
            (f.c, 2),
            (f.target, 2),
        ],
    );
    let from_c = f.c.0.to_string();
    for (direction, expected) in [
        ("in", vec![(f.b, 1)]),
        ("both", vec![(f.b, 1), (f.d, 1)]),
        ("out", vec![(f.d, 1)]),
    ] {
        f.store.search(
            f.seq,
            10,
            1,
            &[
                "--expand-from",
                &from_c,
                "--max-hops",
                "1",
                "--expand-relation",
                "CITES",
                "--expand-direction",
                direction,
            ],
            &expected,
        );
    }
    // Neither the seed nor the two transit vertices are eligible. With a
    // candidate budget of one, post-filtering instead of pre-filtering loses d.
    let endpoint = f.d.0.to_string();
    f.store.search(
        f.seq,
        1,
        5,
        &[
            "--expand-from",
            &seed,
            "--max-hops",
            "3",
            "--expand-relation",
            "CITES",
            "--eligible",
            &endpoint,
            "--graph-candidates",
            "1",
        ],
        &[(f.d, 3)],
    );
    let duplicate_seeds = format!("{},{},{}", f.isolate.0, u128::MAX, f.isolate.0);
    let zero = [
        "--expand-from",
        &duplicate_seeds,
        "--max-hops",
        "0",
        "--include-seeds",
        "true",
        "--expand-relation",
        "CITES",
    ];
    f.store.search(f.seq, 10, 1, &zero, &[(f.isolate, 0)]);
    let mut empty = zero.to_vec();
    empty.extend(["--eligible", ""]);
    f.store.search(f.seq, 10, 1, &empty, &[]);
    let missing = u128::MAX.to_string();
    f.store.search(
        f.seq,
        10,
        1,
        &["--expand-from", &missing, "--max-hops", "2"],
        &[],
    );
    // Searches were read-only; a later ordinary query still reports this cut.
    assert_eq!(f.store.vertex("a", f.seq), f.a);
}

#[test]
fn graph_only_historical_reopen_keeps_the_old_cut_after_parallel_edge_deletion() {
    let f = Fixture::new();
    let seed = f.a.0.to_string();
    let current = [
        "--expand-from",
        &seed,
        "--max-hops",
        "3",
        "--expand-relation",
        "CITES",
    ];
    let old_hits = [
        (f.b, 1),
        (f.bridge, 1),
        (f.c, 2),
        (f.target, 2),
        (f.d, 3),
    ];
    f.store.search(f.seq, 10, 1, &current, &old_hits);
    let changed = f.store.write(&[
        "MATCH (a)-[e:CITES]->(b) WHERE a.name='a' AND b.name='b' DELETE e",
    ]);
    assert_eq!(changed, f.seq + 1);
    f.store.search(changed, 10, 1, &current, &[(f.bridge, 1), (f.target, 2)]);
    let before = f.seq.to_string();
    let mut historical = current.to_vec();
    historical.extend(["--as-of", &before]);
    f.store.search(f.seq, 10, 1, &historical, &old_hits);
    // An empty endpoint set is an empty successful answer at the requested cut.
    historical.extend(["--eligible", ""]);
    f.store.search(f.seq, 10, 1, &historical, &[]);
    assert_eq!(f.store.vertex("a", changed), f.a);
}

#[test]
fn graph_only_refuses_invalid_budgets_and_seed_overflow_before_storage_open() {
    let store = Store::new();
    let limit = ExpansionLimits::default().max_seed_ids;
    let seeds = std::iter::repeat_n("1", limit + 1)
        .collect::<Vec<_>>()
        .join(",");
    for args in [
        vec!["--expand-from", "1"],
        vec![
            "--expand-from",
            "1",
            "--max-hops",
            "0",
            "--k",
            "2",
            "--graph-candidates",
            "1",
        ],
        vec![
            "--expand-from",
            &seeds,
            "--max-hops",
            "0",
            "--include-seeds",
            "true",
        ],
        vec![
            "--expand-from",
            &seeds,
            "--max-hops",
            "0",
            "--eligible",
            "",
        ],
    ] {
        let output = store.command("search", &args);
        assert_eq!(output.status.code(), Some(2), "{:?}", output);
        let text = String::from_utf8(output.stdout).unwrap();
        let diagnostic = text.strip_prefix(INVOCATION).expect("invocation first");
        assert_eq!(diagnostic.lines().count(), 1, "one refusal, no partial rows");
        assert!(diagnostic.starts_with(concat!(
            "{\"v\":1,\"event\":\"error\",\"class\":\"usage\",",
            "\"diagnostics\":[",
        )));
        assert!(diagnostic.ends_with("]}\n"));
        assert!(!output.stderr.is_empty());
        for key_byte in ["5a", "77", "3c"] {
            assert!(!text.contains(&key_byte.repeat(32)));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(&key_byte.repeat(32)));
        }
        assert!(!store.db.exists(), "invalid input must not create storage");
    }
    let at_limit = std::iter::repeat_n("1", limit).collect::<Vec<_>>().join(",");
    let admitted = store.command(
        "search",
        &["--expand-from", &at_limit, "--max-hops", "0"],
    );
    assert_eq!(
        admitted.status.code(),
        Some(4),
        "the exact seed ceiling must reach the missing-database open: {admitted:?}",
    );
    assert!(!store.db.exists());
}
