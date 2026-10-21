//! Live GAPBS differential comparison through the durable embedded database.
//!
//! ```text
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --fixture smoke --output /tmp/fgdb-gapbs-smoke
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --fixture hub --vertices 4096 --trials 5 \
//!   --output /tmp/fgdb-gapbs-hub
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --graph email-Eu-core.txt --vertices 1005 \
//!   --source 0 --trials 6 --sealed --output /tmp/fgdb-gapbs-email
//! ```
//!
//! Pass `--build-note 'profile and per-package --config overrides'` when using
//! custom build settings. This note is explicitly operator-supplied: runtime
//! environment and debug assertions cannot recover every compiler option.
//!
//! Build GAPBS from https://github.com/sbeamer/gapbs, or pass a clean source
//! checkout: this example compiles the actual upstream BFS/CC implementations
//! with a small output adapter. A C++11/OpenMP compiler (`CXX`, default `c++`)
//! plus Git and `rustc` must be available. The checkout's revision, compiler, input blob,
//! arguments, full results, verification transcripts and samples are retained
//! in a NEW output directory. Nothing is deleted and nothing is downloaded.
//! The real-data example uses the uncompressed edge list from
//! https://snap.stanford.edu/data/email-Eu-core.html (1,005 explicit vertices).
//!
//! Graphs use dense integer IDs in [0, vertices). The directed, unit-weight
//! projection explicitly drops loops and collapses parallel edges, matching
//! GAPBS. The explicit population preserves isolates; a dropped final-vertex
//! loop lets GAPBS's endpoint-based loader retain trailing isolates as well.
//! BFS compares every reachable distance and every unreachable vertex; WCC
//! compares all labels after canonicalizing to each component's minimum ID.
//! PageRank is intentionally absent: GAPBS's sink and float32 semantics differ
//! from the registered normalized float64 procedure.
//!
//! Timings are separate: input parsing, durable ingest/reopen, projection,
//! certified execution, complete result encoding, and the actual public CALL.
//! `execute_and_certify_ns` includes the production API's internal typed-row
//! conversion and certificate construction; it is NOT a bare-kernel timer.
//! GAPBS reports input/CSR/kernel/encoding/verification separately. Verification
//! is outside its kernel timer, and no loading time is hidden in a kernel
//! comparison. Subsequent database samples may use the ordinary projection
//! cache; `projection_reused` observes that fact rather than assuming it.
//! BFS samples retain the certificate's per-level pull decisions and actual
//! vertex/arc visits; their benefit bounds describe visits, not elapsed time.
//! Both engines run in every paired trial, with alternating order. Raw samples
//! and empirical quantiles describe this machine, not an activated §17 gate,
//! a fitted tail, a server/authorization benchmark, or external-memory parity.
//!
//! Optional `--sealed` adds a separate end-to-end series that cycles through six
//! orders of read-only checkpoint preparation, the previous writable-open/seal
//! path, and live GAPBS. It includes opening, preparation, certified execution,
//! complete output encoding, and release; it checks all rows, source identity,
//! projection/result digests and populations. Both sealed paths keep production
//! default source/image/projection admissions (including the raw source cap),
//! so opting in can refuse a graph admitted by the decoded-only comparison.
//! The old/new sealed comparison is a maintenance comparison, not a competitor
//! win. OS caches are ordinary warm caches, and no RSS or cold-start claim is made.
//! Release covers the native result, projection and resident writer. Canonical
//! verification rows remain until the paired check; constructing and encoding
//! that comparison copy is timed, while paired verification and logging are not.

#![forbid(unsafe_code)]

use asupersync::Budget;
use fgdb::{Database, WriteBatch};
use fgdb_bench::{KNOWS, keys};
use fgdb_delta_types::LabelId;
use fgdb_prism::{
    FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxResult, FnxValue, GraphView,
    ParallelEdgePolicy, SelfLoopPolicy, SnapshotGraphView,
};
use fgdb_types::{CommitCx, EId, PurposeContexts, QueryCx, VId};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Edge = (usize, usize);
type Metrics = BTreeMap<String, u128>;

const USAGE: &str = "gapbs_compare --gapbs CHECKOUT --output NEW_DIRECTORY \
    (--graph FILE --vertices N | --fixture smoke|sparse|hub [--vertices N]) \
    [--source 0] [--trials 5] [--threads 1] [--build-note TEXT] [--sealed]";

#[derive(Debug)]
struct Config {
    gapbs: PathBuf,
    output: PathBuf,
    graph: Option<PathBuf>,
    fixture: Option<String>,
    vertices: usize,
    source: usize,
    trials: usize,
    threads: usize,
    build_note: String,
    sealed: bool,
}

impl Config {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let mut arguments = arguments;
        let mut values = BTreeMap::new();
        let mut sealed = false;
        while let Some(key) = arguments.next() {
            if key == "--sealed" {
                if sealed {
                    return Err("repeated --sealed option".into());
                }
                sealed = true;
                continue;
            }
            if ![
                "--gapbs",
                "--output",
                "--graph",
                "--fixture",
                "--vertices",
                "--source",
                "--trials",
                "--threads",
                "--build-note",
            ]
            .contains(&key.as_str())
            {
                return Err(format!("unknown option {key}; {USAGE}").into());
            }
            let value = arguments.next().ok_or("option requires a value")?;
            if value.starts_with("--") || values.insert(key, value).is_some() {
                return Err("missing value or repeated option".into());
            }
        }
        let gapbs = values.remove("--gapbs").ok_or(USAGE)?.into();
        let output = values.remove("--output").ok_or(USAGE)?.into();
        let graph = values.remove("--graph").map(PathBuf::from);
        let fixture = values.remove("--fixture");
        if graph.is_some() == fixture.is_some() {
            return Err("choose exactly one of --graph and --fixture".into());
        }
        let vertices = match values.remove("--vertices") {
            Some(value) => value.parse()?,
            None if fixture.as_deref() == Some("smoke") => 8,
            None if fixture.is_some() => 4096,
            None => return Err("--graph requires an explicit --vertices population".into()),
        };
        let mut number = |key: &str, fallback: usize| -> Result<usize> {
            values
                .remove(key)
                .map_or(Ok(fallback), |value| Ok(value.parse()?))
        };
        let source = number("--source", 0)?;
        let trials = number("--trials", 5)?;
        let threads = number("--threads", 1)?;
        let build_note = values
            .remove("--build-note")
            .unwrap_or_else(|| "not supplied".to_owned());
        if vertices == 0 || vertices > i32::MAX as usize || source >= vertices {
            return Err("require 0 <= source < vertices <= i32::MAX".into());
        }
        if trials == 0 || threads == 0 {
            return Err("trials and threads must be positive".into());
        }
        Ok(Self {
            gapbs,
            output,
            graph,
            fixture,
            vertices,
            source,
            trials,
            threads,
            build_note,
            sealed,
        })
    }
}

fn input(reader: impl BufRead, vertices: usize) -> Result<Vec<Edge>> {
    let mut edges = Vec::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('%') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let source: usize = fields.next().ok_or("missing source")?.parse()?;
        let target: usize = fields.next().ok_or("missing target")?.parse()?;
        if fields.next().is_some() || source >= vertices || target >= vertices {
            return Err(format!(
                "line {}: expected exactly two IDs in [0, {vertices})",
                line_number + 1
            )
            .into());
        }
        edges.push((source, target));
    }
    Ok(edges)
}

fn fixture(name: &str, vertices: usize) -> Result<Vec<Edge>> {
    match name {
        "smoke" if vertices == 8 => Ok(vec![
            (0, 1),
            (0, 2),
            (1, 3),
            (2, 3),
            (3, 1),
            (0, 1),
            (3, 3),
            (4, 5),
            (5, 4),
        ]),
        "smoke" => Err("the smoke fixture requires exactly 8 vertices".into()),
        "sparse" | "hub" if vertices < 4 => {
            Err("sparse and hub fixtures require at least 4 vertices".into())
        }
        "sparse" => {
            let mut edges = Vec::new();
            for vertex in 0..vertices - 2 {
                edges.push((vertex, vertex + 1));
                if vertex.is_multiple_of(7) && vertex + 17 < vertices - 1 {
                    edges.push((vertex, vertex + 17));
                }
            }
            Ok(edges)
        }
        "hub" => {
            let mut edges = Vec::new();
            let split = vertices / 8 * 7;
            for (root, end) in [(0, split.max(2)), (split.max(2), vertices - 1)] {
                for vertex in root + 1..end {
                    edges.push((root, vertex));
                    edges.push((vertex, root));
                    if vertex + 1 < end {
                        edges.push((vertex, vertex + 1));
                    }
                }
            }
            Ok(edges)
        }
        _ => Err("unknown fixture; use smoke, sparse or hub".into()),
    }
}

fn new_file(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::new(
        OpenOptions::new().write(true).create_new(true).open(path)?,
    ))
}

fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = new_file(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

fn command_output(command: &mut Command) -> Result<String> {
    let result = command.output()?;
    if !result.status.success() {
        return Err(format!(
            "command {command:?} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(result.stdout)?.trim_end().to_owned())
}

fn json_string(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            value if value.is_control() => {
                use std::fmt::Write as _;
                write!(&mut result, "\\u{:04x}", value as u32).expect("String write");
            }
            value => result.push(value),
        }
    }
    result.push('"');
    result
}

fn emit(log: &mut impl Write, event: &str, fields: &[(&str, String)]) -> Result<()> {
    let mut line = format!("{{\"event\":{}", json_string(event));
    for (key, value) in fields {
        line.push(',');
        line.push_str(&json_string(key));
        line.push(':');
        line.push_str(&json_string(value));
    }
    line.push_str(",\"empirical_gate_activated\":false}\n");
    log.write_all(line.as_bytes())?;
    log.flush()?;
    print!("{line}");
    Ok(())
}

fn oracle_metrics(stdout: &str, success: bool) -> Result<Metrics> {
    let verdicts = verification_tokens(stdout);
    // Upstream's stock main returns zero even when its verifier prints FAIL.
    if !success || verdicts != ["PASS"] {
        return Err(format!("GAPBS did not verify exactly once: {verdicts:?}").into());
    }
    let mut metrics = Metrics::new();
    for line in stdout.lines() {
        if let Some(line) = line.strip_prefix("FGDB_GAPBS_METRIC ") {
            let (key, value) = line.split_once(' ').ok_or("malformed oracle metric")?;
            if metrics.insert(key.to_owned(), value.parse()?).is_some() {
                return Err("duplicate oracle metric".into());
            }
        }
    }
    for key in [
        "input_ns",
        "csr_ns",
        "kernel_ns",
        "verification_ns",
        "encoding_ns",
        "vertices",
        "arcs",
    ] {
        if !metrics.contains_key(key) {
            return Err(format!("missing GAPBS metric {key}").into());
        }
    }
    Ok(metrics)
}

fn verification_tokens(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Verification:"))
        .map(str::trim)
        .collect()
}

fn run_oracle(
    config: &Config,
    algorithm: &str,
    prefix: &str,
    corrupt: bool,
) -> Result<(Output, PathBuf)> {
    let output_path = config.output.join(format!("{prefix}.rows"));
    if output_path.exists() {
        return Err("refusing to overwrite oracle output".into());
    }
    let mut command = Command::new(config.output.join("gapbs-adapter"));
    command
        .arg(algorithm)
        .arg(config.output.join("graph.el"))
        .arg(config.vertices.to_string())
        .arg(config.source.to_string())
        .arg(&output_path)
        .env("OMP_NUM_THREADS", config.threads.to_string())
        .env("OMP_DYNAMIC", "FALSE");
    if corrupt {
        command.arg("corrupt");
    }
    let result = command.output()?;
    save(
        &config.output.join(format!("{prefix}.stdout")),
        &result.stdout,
    )?;
    save(
        &config.output.join(format!("{prefix}.stderr")),
        &result.stderr,
    )?;
    Ok((result, output_path))
}

fn canonical_rows(result: &FnxResult, algorithm: &str, vertices: usize) -> Result<Vec<i64>> {
    let expected_columns = match algorithm {
        "bfs" => ["vertex", "distance"],
        "wcc" => ["vertex", "component"],
        _ => return Err("unknown result algorithm".into()),
    };
    if result
        .columns
        .iter()
        .map(String::as_str)
        .ne(expected_columns)
    {
        return Err("production result has the wrong column schema".into());
    }
    let mut values = vec![-1; vertices];
    let mut previous = None;
    for row in &result.rows {
        let [FnxValue::Vertex(vertex), value] = row.as_slice() else {
            return Err("production result has the wrong row schema".into());
        };
        let vertex = usize::try_from(vertex.0)?;
        if vertex >= vertices || previous.is_some_and(|previous| vertex <= previous) {
            return Err("production rows contain unknown, duplicate or unsorted vertices".into());
        }
        previous = Some(vertex);
        values[vertex] = match (algorithm, value) {
            ("bfs", FnxValue::Integer(distance)) => i64::try_from(*distance)?,
            ("wcc", FnxValue::Vertex(component)) if component.0 < vertices as u128 => {
                i64::try_from(component.0)?
            }
            _ => return Err("production result has the wrong value type".into()),
        };
    }
    if algorithm == "wcc" && result.rows.len() != vertices {
        return Err("component result omitted vertices".into());
    }
    Ok(values)
}

fn write_rows(path: &Path, values: &[i64]) -> Result<()> {
    let mut output = new_file(path)?;
    for (vertex, value) in values.iter().enumerate() {
        writeln!(output, "{vertex} {value}")?;
    }
    output.flush()?;
    Ok(())
}

fn compare_rows(expected: &[i64], reader: impl BufRead) -> Result<()> {
    let mut lines = reader.lines();
    for (vertex, expected) in expected.iter().enumerate() {
        let line = lines.next().ok_or("oracle omitted result rows")??;
        let mut fields = line.split_whitespace();
        let id: usize = fields.next().ok_or("oracle row has no ID")?.parse()?;
        let value: i64 = fields.next().ok_or("oracle row has no value")?.parse()?;
        if id != vertex || value != *expected || fields.next().is_some() {
            return Err(format!(
                "full-row mismatch at vertex {vertex}: database={expected}, GAPBS={line}"
            )
            .into());
        }
    }
    if lines.next().is_some() {
        return Err("oracle emitted extra result rows".into());
    }
    Ok(())
}

fn statement(algorithm: &str, source: usize) -> String {
    if algorithm == "bfs" {
        format!("CALL fnx.single_source_shortest_path_length({source}) YIELD vertex,distance")
    } else {
        "CALL fnx.weakly_connected_components() YIELD vertex,component".to_owned()
    }
}

fn database_sample(
    config: &Config,
    database: &Database,
    query: &QueryCx,
    algorithm: &str,
    prefix: &str,
    previous: &mut Option<SnapshotGraphView>,
    log: &mut impl Write,
) -> Result<(Vec<i64>, u128, usize)> {
    let text = statement(algorithm, config.source);
    let call = FnxCallSpec::bind(&text, &FnxParameters::new())?;
    let mut options = FnxReadOptions::whole_graph_for(&call, None);
    options.selection.relation = Some(KNOWS);
    options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
    options.projection.self_loops = SelfLoopPolicy::Drop;

    let started = Instant::now();
    let view = database.read_session()?;
    let projection = view.prism_projection_at(
        query,
        view.frontier(),
        options.selection,
        options.projection,
        options.projection_limits,
        options.source_limits,
    )?;
    let projection_ns = started.elapsed().as_nanos();
    if projection.node_count() != config.vertices {
        return Err("durably loaded projection lost vertices".into());
    }
    let reused = previous
        .as_ref()
        .is_some_and(|previous| previous.shares_cache_with(&projection));
    let started = Instant::now();
    let result = call.execute(&projection, options.execution_limits, || query.checkpoint())?;
    let execution_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let values = canonical_rows(&result, algorithm, config.vertices)?;
    write_rows(&config.output.join(format!("{prefix}.rows")), &values)?;
    let encoding_ns = started.elapsed().as_nanos();
    let query_ns = projection_ns + execution_ns + encoding_ns;
    // Also measure the actual public request boundary with the admitted
    // projection retained. This is an explicitly warm, trusted embedded CALL.
    let started = Instant::now();
    let public = database.call_fnx(query, &text, &FnxParameters::new(), options)?;
    let public_values = canonical_rows(&public.analytics, algorithm, config.vertices)?;
    let public_call_ns = started.elapsed().as_nanos();
    if public_values != values || public.analytics != result {
        return Err("public CALL differs from the admitted certified execution".into());
    }
    let certificate = &result.certificate;
    emit(
        log,
        "database_sample",
        &[
            ("sample", prefix.to_owned()),
            ("algorithm", algorithm.to_owned()),
            (
                "scope",
                "trusted-embedded-durable-decoded-projection".to_owned(),
            ),
            ("projection_reused", reused.to_string()),
            ("projection_ns", projection_ns.to_string()),
            ("execute_and_certify_ns", execution_ns.to_string()),
            ("result_encoding_ns", encoding_ns.to_string()),
            ("projection_through_result_ns", query_ns.to_string()),
            (
                "warm_public_call_and_consume_ns",
                public_call_ns.to_string(),
            ),
            ("projected_arcs", certificate.edges.to_string()),
            ("result_rows", result.rows.len().to_string()),
            ("kernel", certificate.execution_kernel.to_owned()),
            (
                "projection_digest",
                format!("{:?}", certificate.projection_digest),
            ),
            (
                "kernel_source_digest",
                format!("{:?}", certificate.kernel_source_digest),
            ),
            ("result_digest", format!("{:?}", certificate.result_digest)),
            (
                "projection_resident_bytes",
                projection.charged_resident_bytes().to_string(),
            ),
            (
                "kernel_workspace_bytes",
                certificate.kernel_workspace_bytes.to_string(),
            ),
            (
                "edges_scanned",
                certificate.witness.edges_scanned.to_string(),
            ),
            (
                "nodes_touched",
                certificate.witness.nodes_touched.to_string(),
            ),
            (
                "bfs_pull_levels",
                certificate
                    .bfs_decision_card
                    .as_ref()
                    .map_or(0, |card| card.pulls.len())
                    .to_string(),
            ),
            ("correctness", "pending-live-GAPBS-comparison".to_owned()),
            (
                "component_pull_frontiers",
                certificate
                    .component_decision_card
                    .as_ref()
                    .map_or(0, |card| card.pulls.len())
                    .to_string(),
            ),
        ],
    )?;
    if let Some(card) = &certificate.bfs_decision_card {
        for pull in &card.pulls {
            emit(
                log,
                "bfs_pull_decision",
                &[
                    ("sample", prefix.to_owned()),
                    ("policy_version", card.policy_version.to_string()),
                    ("fallback", card.fallback.to_owned()),
                    ("dwell_levels", card.dwell_levels.to_string()),
                    (
                        "pull_cost_multiplier",
                        card.pull_cost_multiplier.to_string(),
                    ),
                    ("depth", pull.depth.to_string()),
                    ("frontier_vertices", pull.frontier_vertices.to_string()),
                    ("frontier_arcs", pull.frontier_arcs.to_string()),
                    (
                        "unseen_incoming_arcs",
                        pull.unseen_incoming_arcs.to_string(),
                    ),
                    ("vertices", pull.vertices.to_string()),
                    ("benefit_lower", pull.benefit_lower.to_string()),
                    ("benefit_upper", pull.benefit_upper.to_string()),
                    ("candidate_visits", pull.candidate_visits.to_string()),
                    ("incoming_visits", pull.incoming_visits.to_string()),
                ],
            )?;
        }
    }
    if let Some(card) = &certificate.component_decision_card {
        for pull in &card.pulls {
            emit(
                log,
                "component_pull_decision",
                &[
                    ("sample", prefix.to_owned()),
                    ("policy_version", card.policy_version.to_string()),
                    ("fallback", card.fallback.to_owned()),
                    ("dwell_frontiers", card.dwell_frontiers.to_string()),
                    (
                        "pull_cost_multiplier",
                        card.pull_cost_multiplier.to_string(),
                    ),
                    ("root", pull.root.to_string()),
                    ("frontier_vertices", pull.frontier_vertices.to_string()),
                    ("frontier_incidences", pull.frontier_incidences.to_string()),
                    ("unseen_incidences", pull.unseen_incidences.to_string()),
                    ("vertices", pull.vertices.to_string()),
                    ("benefit_lower", pull.benefit_lower.to_string()),
                    ("benefit_upper", pull.benefit_upper.to_string()),
                    ("candidate_visits", pull.candidate_visits.to_string()),
                    ("incidence_visits", pull.incidence_visits.to_string()),
                ],
            )?;
        }
    }
    *previous = Some(projection);
    Ok((values, query_ns, certificate.edges))
}

struct SealedSample {
    values: Vec<i64>,
    total_ns: u128,
    arcs: usize,
    projection_digest: String,
    result_digest: String,
}

/// Each sample times opening, image preparation, native answer and their release.
/// Only the canonical verification copy remains for the later paired check.
/// No resident database or projection cache is retained outside the timer.
#[allow(clippy::too_many_arguments)]
async fn sealed_sample(
    config: &Config,
    commit: &CommitCx,
    query: &QueryCx,
    database_path: &Path,
    algorithm: &str,
    prefix: &str,
    writable: bool,
    expected: (fgdb_types::ids::ObjectId, fgdb_types::CommitSeq),
    log: &mut impl Write,
) -> Result<SealedSample> {
    let call = FnxCallSpec::bind(&statement(algorithm, config.source), &FnxParameters::new())?;
    let mut options = FnxReadOptions::whole_graph_for(&call, None);
    options.selection.relation = Some(KNOWS);
    options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
    options.projection.self_loops = SelfLoopPolicy::Drop;
    let memory = FnxMemoryLimits {
        max_kernel_workspace_bytes: 64 * 1024 * 1024,
        max_result_bytes: 64 * 1024 * 1024,
    };
    let all_started = Instant::now();
    let started = Instant::now();
    let mut resident = None;
    let graph = if writable {
        let database = Database::open(commit, database_path, keys()).await?;
        let graph = database
            .prism_sealed_projection_with_source_limits(
                query,
                options.as_of,
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
                Default::default(),
                Default::default(),
            )
            .await?;
        resident = Some(database);
        graph
    } else {
        Database::open_prism_sealed_projection(
            commit,
            query,
            database_path,
            keys(),
            options,
            Default::default(),
            Default::default(),
        )
        .await?
    };
    let prepare_ns = started.elapsed().as_nanos();
    if graph.node_count() != config.vertices
        || graph.binding().root != expected.0
        || graph.binding().as_of != expected.1
    {
        return Err("sealed sample changed the source generation or vertex population".into());
    }
    let started = Instant::now();
    let result = call.execute_sealed(query, &graph, options.execution_limits, memory)?;
    let execute_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let values = canonical_rows(&result, algorithm, config.vertices)?;
    write_rows(&config.output.join(format!("{prefix}.rows")), &values)?;
    let encoding_ns = started.elapsed().as_nanos();
    let arcs = result.certificate.edges;
    let projection_digest = format!("{:?}", result.certificate.projection_digest);
    let result_digest = format!("{:?}", result.certificate.result_digest);
    let kernel = result.certificate.execution_kernel;
    let kernel_bytes = result.certificate.kernel_workspace_bytes;
    let rows = result.rows.len();
    let started = Instant::now();
    drop(result);
    drop(graph);
    drop(resident);
    let release_ns = started.elapsed().as_nanos();
    let total_ns = all_started.elapsed().as_nanos();
    emit(
        log,
        "sealed_database_sample",
        &[
            ("sample", prefix.to_owned()),
            ("algorithm", algorithm.to_owned()),
            (
                "preparation",
                if writable {
                    "writable-open-plus-seal"
                } else {
                    "read-only-checkpoint-seal"
                }
                .to_owned(),
            ),
            ("open_and_prepare_ns", prepare_ns.to_string()),
            ("execute_and_certify_ns", execute_ns.to_string()),
            ("complete_result_encoding_ns", encoding_ns.to_string()),
            ("release_ns", release_ns.to_string()),
            ("release_scope", "native result, projection and resident writer; canonical verification rows retained for the paired check".to_owned()),
            ("open_through_release_ns", total_ns.to_string()),
            ("source_root", format!("{:?}", expected.0)),
            ("as_of", expected.1.0.to_string()),
            ("projected_arcs", arcs.to_string()),
            ("result_rows", rows.to_string()),
            ("kernel", kernel.to_owned()),
            ("kernel_workspace_bytes", kernel_bytes.to_string()),
            ("projection_digest", projection_digest.clone()),
            ("result_digest", result_digest.clone()),
            (
                "budgets",
                "production default source/image/projection; 64 MiB kernel and result each"
                    .to_owned(),
            ),
            (
                "correctness",
                "pending paired and live-GAPBS comparison".to_owned(),
            ),
        ],
    )?;
    Ok(SealedSample {
        values,
        total_ns,
        arcs,
        projection_digest,
        result_digest,
    })
}

async fn sealed_trials(
    config: &Config,
    commit: &CommitCx,
    query: &QueryCx,
    database_path: &Path,
    expected: (fgdb_types::ids::ObjectId, fgdb_types::CommitSeq),
    log: &mut impl Write,
) -> Result<()> {
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for algorithm in ["bfs", "wcc"] {
        let mut read_only_times = Vec::new();
        let mut writable_times = Vec::new();
        let mut gapbs_times = Vec::new();
        for trial in 0..config.trials {
            let order = ORDERS[trial % ORDERS.len()];
            let mut read_only = None;
            let mut writable = None;
            let mut oracle = None;
            let oracle_prefix = format!("{algorithm}-{trial}-sealed-series-gapbs");
            for participant in order {
                match participant {
                    0 | 1 => {
                        let legacy = participant == 1;
                        let prefix = format!(
                            "{algorithm}-{trial}-sealed-{}",
                            if legacy { "writable" } else { "readonly" }
                        );
                        let sample = sealed_sample(
                            config,
                            commit,
                            query,
                            database_path,
                            algorithm,
                            &prefix,
                            legacy,
                            expected,
                            log,
                        )
                        .await?;
                        if legacy {
                            writable = Some(sample);
                        } else {
                            read_only = Some(sample);
                        }
                    }
                    _ => oracle = Some(run_oracle(config, algorithm, &oracle_prefix, false)?),
                }
            }
            let read_only = read_only.ok_or("read-only sample missing")?;
            let writable = writable.ok_or("writable sample missing")?;
            if read_only.values != writable.values
                || read_only.arcs != writable.arcs
                || read_only.projection_digest != writable.projection_digest
                || read_only.result_digest != writable.result_digest
            {
                return Err("read-only and writable sealed preparations disagree".into());
            }
            let (oracle, rows) = oracle.ok_or("live GAPBS sample missing")?;
            let metrics =
                oracle_metrics(&String::from_utf8(oracle.stdout)?, oracle.status.success())?;
            if metrics["vertices"] != config.vertices as u128
                || metrics["arcs"] != read_only.arcs as u128
            {
                return Err("live GAPBS and sealed populations differ".into());
            }
            compare_rows(&read_only.values, BufReader::new(File::open(rows)?))?;
            let gapbs_ns = metrics["input_ns"]
                + metrics["csr_ns"]
                + metrics["kernel_ns"]
                + metrics["encoding_ns"];
            emit(
                log,
                "sealed_triplet_verification",
                &[
                    ("algorithm", algorithm.to_owned()),
                    ("trial", trial.to_string()),
                    ("order", order.map(|actor| ["readonly", "writable", "gapbs"][actor]).join(",")),
                    ("correctness", "all rows and populations match; sealed identities match; live GAPBS verifier PASS".to_owned()),
                    ("read_only_open_through_release_ns", read_only.total_ns.to_string()),
                    ("writable_open_through_release_ns", writable.total_ns.to_string()),
                    ("gapbs_input_through_result_ns", gapbs_ns.to_string()),
                    ("gapbs_verification_ns", metrics["verification_ns"].to_string()),
                    ("comparison_scope", "old/new sealed is maintenance; live GAPBS has independently reported phases".to_owned()),
                ],
            )?;
            read_only_times.push(read_only.total_ns);
            writable_times.push(writable.total_ns);
            gapbs_times.push(gapbs_ns);
        }
        summary(
            log,
            &format!("{algorithm}:readonly-sealed-open-through-release"),
            &mut read_only_times,
        )?;
        summary(
            log,
            &format!("{algorithm}:writable-sealed-open-through-release"),
            &mut writable_times,
        )?;
        summary(
            log,
            &format!("{algorithm}:sealed-series-gapbs-input-through-result"),
            &mut gapbs_times,
        )?;
    }
    Ok(())
}

fn summary(log: &mut impl Write, name: &str, values: &mut [u128]) -> Result<()> {
    values.sort_unstable();
    let quantile = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    emit(
        log,
        "empirical_distribution",
        &[
            ("series", name.to_owned()),
            ("samples", values.len().to_string()),
            (
                "quantile_method",
                "nearest-rank; no fitted-tail or confidence claim".to_owned(),
            ),
            ("min_ns", values[0].to_string()),
            ("p50_ns", quantile(50).to_string()),
            ("p95_ns", quantile(95).to_string()),
            ("p99_ns", quantile(99).to_string()),
            ("max_ns", values[values.len() - 1].to_string()),
        ],
    )
}

async fn run(config: Config, commit: CommitCx, query: QueryCx) -> Result<()> {
    fs::create_dir(&config.output)?;
    let config = Config {
        gapbs: fs::canonicalize(&config.gapbs)?,
        output: fs::canonicalize(&config.output)?,
        ..config
    };
    let mut log = new_file(&config.output.join("results.ndjson"))?;
    let gapbs_revision = command_output(Command::new("git").args([
        "-C",
        config.gapbs.to_str().ok_or("non-UTF8 GAPBS path")?,
        "rev-parse",
        "HEAD",
    ]))?;
    command_output(Command::new("git").arg("-C").arg(&config.gapbs).args([
        "diff",
        "--exit-code",
        "HEAD",
        "--",
        "src",
    ]))?;
    let compiler = std::env::var("CXX").unwrap_or_else(|_| "c++".to_owned());
    let compiler_version = command_output(Command::new(&compiler).arg("--version"))?;
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let rustc_version = command_output(Command::new(rustc).arg("-vV"))?;
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let head = command_output(
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["rev-parse", "HEAD"]),
    )?;
    let tree = command_output(
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["rev-parse", "HEAD^{tree}"]),
    )?;
    let status = command_output(Command::new("git").arg("-C").arg(repository).args([
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
    ]))?;
    let executable_blob = command_output(
        Command::new("git")
            .arg("hash-object")
            .arg(std::env::current_exe()?),
    )?;
    let cpu_model = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                (key.trim() == "model name").then(|| value.trim().to_owned())
            })
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let mut build_environment: Vec<_> = std::env::vars()
        .filter(|(key, _)| {
            key.starts_with("CARGO_PROFILE_")
                || [
                    "RUSTFLAGS",
                    "CARGO_ENCODED_RUSTFLAGS",
                    "CARGO_BUILD_JOBS",
                    "CARGO_INCREMENTAL",
                    "RUSTUP_TOOLCHAIN",
                    "RUSTC",
                    "CARGO_TARGET_DIR",
                    "RUST_TEST_THREADS",
                    "RCH_CARGO_WRAPPER_BYPASS",
                ]
                .contains(&key.as_str())
        })
        .collect();
    build_environment.sort_unstable();
    let build_environment = build_environment
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    save(
        &config.output.join("gapbs-adapter.cc"),
        GAPBS_ADAPTER.as_bytes(),
    )?;
    command_output(
        Command::new(&compiler)
            .args(["-std=c++11", "-O3", "-Wall", "-fopenmp", "-I"])
            .arg(config.gapbs.join("src"))
            .arg(config.output.join("gapbs-adapter.cc"))
            .arg("-o")
            .arg(config.output.join("gapbs-adapter")),
    )?;
    let started = Instant::now();
    let mut edges = if let Some(path) = &config.graph {
        input(BufReader::new(File::open(path)?), config.vertices)?
    } else {
        fixture(
            config.fixture.as_deref().ok_or("missing fixture")?,
            config.vertices,
        )?
    };
    let input_ns = started.elapsed().as_nanos();
    let raw_edges = edges.len();
    edges.push((config.vertices - 1, config.vertices - 1));
    let started = Instant::now();
    let graph_path = config.output.join("graph.el");
    let mut graph_output = new_file(&graph_path)?;
    for (source, target) in &edges {
        writeln!(graph_output, "{source} {target}")?;
    }
    graph_output.flush()?;
    let shared_input_encoding_ns = started.elapsed().as_nanos();
    let graph_blob = command_output(Command::new("git").arg("hash-object").arg(&graph_path))?;
    emit(
        &mut log,
        "comparison_header",
        &[
            ("schema", "fgdb-gapbs-comparison-v1".to_owned()),
            ("gapbs_revision", gapbs_revision),
            (
                "gapbs_compiler",
                compiler_version.lines().next().unwrap_or("").to_owned(),
            ),
            (
                "gapbs_build_flags",
                "-std=c++11 -O3 -Wall -fopenmp".to_owned(),
            ),
            ("rust_debug_assertions", cfg!(debug_assertions).to_string()),
            ("rustc_verbose_version", rustc_version),
            ("invocation_worktree_head", head),
            ("invocation_head_tree", tree),
            ("invocation_worktree_dirty", (!status.is_empty()).to_string()),
            ("invocation_worktree_status", status),
            ("executable_git_blob", executable_blob),
            ("build_environment_at_invocation", build_environment),
            ("operator_supplied_build_note", config.build_note.clone()),
            (
                "build_identity_scope",
                "HEAD/tree describe the invocation checkout; dirty code and compiler overrides require source digests, binary blob and explicit build note".to_owned(),
            ),
            ("cpu_model", cpu_model),
            ("input_git_blob", graph_blob),
            (
                "input_path",
                config.graph.as_ref().map_or_else(
                    || format!("fixture:{}", config.fixture.as_deref().unwrap_or("")),
                    |path| path.display().to_string(),
                ),
            ),
            ("vertices", config.vertices.to_string()),
            ("raw_input_edges", raw_edges.to_string()),
            ("gapbs_isolate_sentinels", "1 dropped self-loop".to_owned()),
            ("source", config.source.to_string()),
            ("trials", config.trials.to_string()),
            ("gapbs_threads", config.threads.to_string()),
            ("sealed_end_to_end_enabled", config.sealed.to_string()),
            (
                "filesystem_cache",
                "input read during setup; ordinary OS cache; no cold-cache measurement".to_owned(),
            ),
            ("os", std::env::consts::OS.to_owned()),
            ("architecture", std::env::consts::ARCH.to_owned()),
            (
                "logical_cores",
                std::thread::available_parallelism()
                    .map_or(0, std::num::NonZeroUsize::get)
                    .to_string(),
            ),
            ("input_ns", input_ns.to_string()),
            (
                "shared_input_encoding_ns",
                shared_input_encoding_ns.to_string(),
            ),
            (
                "graph_policy",
                "directed; unit weights; collapse parallel; drop self-loops".to_owned(),
            ),
            (
                "durability",
                "production Database::create/write/open; fsync enabled".to_owned(),
            ),
            ("output", config.output.display().to_string()),
        ],
    )?;
    // This is a real negative control against the upstream verifier, not a
    // fabricated passing log: corrupt the computed source parent and require
    // both the nonzero exit and the explicit FAIL, with no result published.
    let (control, rows) = run_oracle(&config, "bfs", "negative-control", true)?;
    let control_log = String::from_utf8(control.stdout)?;
    if control.status.success()
        || verification_tokens(&control_log) != ["FAIL"]
        || rows.exists()
        || oracle_metrics(&control_log, control.status.success()).is_ok()
    {
        return Err("GAPBS failed the corrupted-result rejection control".into());
    }
    emit(
        &mut log,
        "negative_control",
        &[("outcome", "corrupted BFS rejected".to_owned())],
    )?;

    let started = Instant::now();
    let database_path = config.output.join("database");
    let mut database = Database::create(&commit, &database_path, keys()).await?;
    let mut writes = 0usize;
    for start in (0..config.vertices).step_by(256) {
        let mut batch = WriteBatch::new(KNOWS);
        for vertex in start..(start + 256).min(config.vertices) {
            batch.create_vertex(VId(vertex as u128), vec![LabelId(1)], vec![]);
        }
        database.write(&commit, batch).await?;
        writes += 1;
    }
    for (chunk, slice) in edges.chunks(256).enumerate() {
        let mut batch = WriteBatch::new(KNOWS);
        for (offset, (source, target)) in slice.iter().enumerate() {
            batch.add_edge(
                EId((chunk * 256 + offset) as u128),
                VId(*source as u128),
                VId(*target as u128),
                vec![],
            );
        }
        database.write(&commit, batch).await?;
        writes += 1;
    }
    let ingest_ns = started.elapsed().as_nanos();
    drop(edges);
    drop(database);
    let started = Instant::now();
    let database = Database::open(&commit, &database_path, keys()).await?;
    let reopen_ns = started.elapsed().as_nanos();
    emit(
        &mut log,
        "durable_load",
        &[
            ("ingest_ns", ingest_ns.to_string()),
            ("reopen_ns", reopen_ns.to_string()),
            ("commits", writes.to_string()),
            ("records_per_commit", "256".to_owned()),
        ],
    )?;
    let mut previous = None;
    for algorithm in ["bfs", "wcc"] {
        let mut database_samples = Vec::new();
        let mut gapbs_samples = Vec::new();
        for trial in 0..config.trials {
            let database_prefix = format!("{algorithm}-{trial}-database");
            let gapbs_prefix = format!("{algorithm}-{trial}-gapbs");
            let early_oracle = if trial.is_multiple_of(2) {
                Some(run_oracle(&config, algorithm, &gapbs_prefix, false)?)
            } else {
                None
            };
            let (values, database_ns, projected_arcs) = database_sample(
                &config,
                &database,
                &query,
                algorithm,
                &database_prefix,
                &mut previous,
                &mut log,
            )?;
            let (oracle, path) = match early_oracle {
                Some(result) => result,
                None => run_oracle(&config, algorithm, &gapbs_prefix, false)?,
            };
            let metrics =
                oracle_metrics(&String::from_utf8(oracle.stdout)?, oracle.status.success())?;
            if metrics["vertices"] != config.vertices as u128 {
                return Err("GAPBS vertex count differs from database".into());
            }
            if metrics["arcs"] != projected_arcs as u128 {
                return Err("GAPBS projected arc count differs from database".into());
            }
            compare_rows(&values, BufReader::new(File::open(path)?))?;
            let gapbs_ns = metrics["input_ns"]
                + metrics["csr_ns"]
                + metrics["kernel_ns"]
                + metrics["encoding_ns"];
            let mut fields = vec![
                ("sample", gapbs_prefix),
                ("algorithm", algorithm.to_owned()),
                (
                    "order",
                    if trial.is_multiple_of(2) {
                        "gapbs-first"
                    } else {
                        "database-first"
                    }
                    .to_owned(),
                ),
                (
                    "correctness",
                    "all vertices match; upstream verifier PASS".to_owned(),
                ),
                ("input_through_result_ns", gapbs_ns.to_string()),
            ];
            fields.extend(
                metrics
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.to_string())),
            );
            emit(&mut log, "paired_verification", &fields)?;
            database_samples.push(database_ns);
            gapbs_samples.push(gapbs_ns);
        }
        summary(
            &mut log,
            &format!("{algorithm}:database-projection-through-result"),
            &mut database_samples,
        )?;
        summary(
            &mut log,
            &format!("{algorithm}:gapbs-input-through-result"),
            &mut gapbs_samples,
        )?;
    }
    if config.sealed {
        let expected = (database.partition_root()?.0, database.frontier()?);
        // The incumbent needs its own writer lease; no earlier resident image
        // or cached projection is retained outside either sample's preparation.
        drop(previous);
        drop(database);
        sealed_trials(&config, &commit, &query, &database_path, expected, &mut log).await?;
    }
    emit(
        &mut log,
        "comparison_footer",
        &[("outcome", "PASS".to_owned())],
    )?;
    Ok(())
}

fn main() {
    if std::env::args()
        .skip(1)
        .any(|argument| argument == "--help")
    {
        println!("{USAGE}");
        return;
    }
    let result = (|| -> Result<()> {
        let config = Config::parse(std::env::args().skip(1))?;
        let runtime = fgdb::runtime_builder().build()?;
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        runtime.block_on(run(config, contexts.commit(), contexts.query()))
    })();
    if let Err(error) = result {
        eprintln!("GAPBS comparison failed: {error}");
        std::process::exit(1);
    }
}

// Keep the optional external oracle with its only consumer. Normal Cargo
// builds do not need a C++ compiler or an installed GAPBS checkout. The driver
// compiles this adapter before any measurement, retaining the exact source.
const GAPBS_ADAPTER: &str = r#"
#define main upstream_bfs_main
#include "bfs.cc"
#undef main
#define main upstream_cc_main
#include "cc.cc"
#undef main
#include <chrono>
#include <fstream>
#include <stdexcept>

using Clock = std::chrono::steady_clock;
static uint64_t ns(Clock::time_point start) {
  return std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now()-start).count();
}
static void metric(const char* key, uint64_t value) {
  std::cout << "FGDB_GAPBS_METRIC " << key << " " << value << std::endl;
}

int main(int argc, char** argv) {
  try {
    if (argc != 6 && argc != 7) throw std::runtime_error("invalid adapter arguments");
    const std::string algorithm(argv[1]);
    if (algorithm != "bfs" && algorithm != "wcc") throw std::runtime_error("invalid algorithm");
    const NodeID population = std::stoi(argv[3]);
    const NodeID source = std::stoi(argv[4]);
    if (population <= 0 || source < 0 || source >= population) throw std::runtime_error("invalid source/population");
    if (std::ifstream(argv[5]).good()) throw std::runtime_error("output already exists");
    char program[] = "gapbs-adapter", file_flag[] = "-f";
    char* builder_args[] = {program, file_flag, argv[2], nullptr};
    CLApp cli(3, builder_args, "live GAPBS comparison");
    if (!cli.ParseArgs()) throw std::runtime_error("builder arguments failed");
    Builder builder(cli);
    auto start = Clock::now();
    Reader<NodeID, NodeID, WeightT> reader(argv[2]);
    bool needs_weights = false;
    auto edges = reader.ReadFile(needs_weights);
    const uint64_t input_ns = ns(start);
    start = Clock::now();
    Graph graph;
    {
      auto raw = builder.MakeGraphFromEL(edges);
      graph = builder.SquishGraph(raw);
    }
    edges = decltype(edges)();
    const uint64_t csr_ns = ns(start);
    if (graph.num_nodes() != population) throw std::runtime_error("population mismatch");
    start = Clock::now();
    pvector<NodeID> result = algorithm == "bfs" ? DOBFS(graph, source) : Afforest(graph);
    const uint64_t kernel_ns = ns(start);
    if (argc == 7) {
      if (std::string(argv[6]) != "corrupt") throw std::runtime_error("invalid negative control");
      if (algorithm == "bfs") {
        result[source] = -1;
      } else {
        bool corrupted = false;
        for (NodeID u = 0; u < population && !corrupted; ++u) {
          for (NodeID v : graph.out_neigh(u)) {
            if (u == v) continue;
            result[u] = population;
            corrupted = true;
            break;
          }
        }
        if (!corrupted && population > 1) {
          std::fill(result.begin(), result.end(), 0);
        } else if (!corrupted) {
          throw std::runtime_error("negative control needs an edge or two vertices");
        }
      }
    }
    start = Clock::now();
    bool verified = algorithm == "bfs" ? BFSVerifier(graph, source, result) : CCVerifier(graph, result);
    const uint64_t verification_ns = ns(start);
    PrintLabel("Verification", verified ? "PASS" : "FAIL");
    if (!verified) return 3;
    start = Clock::now();
    std::vector<int64_t> values(population, -1);
    if (algorithm == "bfs") {
      values[source] = 0;
      std::vector<NodeID> path;
      for (NodeID vertex = 0; vertex < population; ++vertex) {
        if (result[vertex] < 0 || values[vertex] >= 0) continue;
        NodeID cursor = vertex;
        while (values[cursor] < 0) {
          path.push_back(cursor);
          cursor = result[cursor];
        }
        int64_t depth = values[cursor];
        while (!path.empty()) {
          values[path.back()] = ++depth;
          path.pop_back();
        }
      }
    } else {
      std::vector<NodeID> minimum(population, population);
      for (NodeID vertex = 0; vertex < population; ++vertex) {
        if (result[vertex] < 0 || result[vertex] >= population) throw std::runtime_error("invalid component");
        minimum[result[vertex]] = std::min(minimum[result[vertex]], vertex);
      }
      for (NodeID vertex = 0; vertex < population; ++vertex) values[vertex] = minimum[result[vertex]];
    }
    std::ofstream output(argv[5]);
    if (!output) throw std::runtime_error("cannot create result file");
    for (NodeID vertex = 0; vertex < population; ++vertex) output << vertex << " " << values[vertex] << '\n';
    output.close();
    if (!output) throw std::runtime_error("result write failed");
    const uint64_t encoding_ns = ns(start);
    metric("input_ns", input_ns);
    metric("csr_ns", csr_ns);
    metric("kernel_ns", kernel_ns);
    metric("verification_ns", verification_ns);
    metric("encoding_ns", encoding_ns);
    metric("vertices", graph.num_nodes());
    metric("arcs", graph.num_edges_directed());
    return 0;
  } catch (const std::exception& error) {
    std::cerr << error.what() << std::endl;
    return 2;
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_prism::{
        Directedness, FnxExecutionLimits, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SnapshotBinding,
    };
    use fgdb_types::{CommitSeq, ids::ObjectId};
    use std::convert::Infallible;

    fn smoke_projection() -> SnapshotGraphView {
        let vertices: Vec<_> = (0..8).map(VId).collect();
        let edges: Vec<_> = fixture("smoke", 8)
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(index, (source, target))| ProjectionEdge {
                eid: EId(index as u128),
                source: VId(source as u128),
                target: VId(target as u128),
                weight: 1.0,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([8; 32]),
                as_of: CommitSeq(1),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::CollapseUnit,
                self_loops: SelfLoopPolicy::Drop,
            },
            ProjectionLimits {
                max_vertices: 8,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len() * 2,
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap()
    }

    #[test]
    fn complete_smoke_results_preserve_diamonds_duplicates_loops_and_isolates() {
        let graph = smoke_projection();
        for (algorithm, expected) in [
            ("bfs", vec![0, 1, 1, 2, -1, -1, -1, -1]),
            ("wcc", vec![0, 0, 0, 0, 4, 4, 6, 7]),
        ] {
            let call = FnxCallSpec::bind(&statement(algorithm, 0), &FnxParameters::new()).unwrap();
            let result = call
                .execute(
                    &graph,
                    FnxExecutionLimits {
                        max_iterations: 0,
                        max_result_rows: 8,
                        max_estimated_work: 1000,
                    },
                    || Ok::<(), Infallible>(()),
                )
                .unwrap();
            assert_eq!(canonical_rows(&result, algorithm, 8).unwrap(), expected);
        }
    }

    #[test]
    fn result_normalization_rejects_wrong_missing_reordered_and_extra_columns() {
        let graph = smoke_projection();
        let call = FnxCallSpec::bind(&statement("bfs", 0), &FnxParameters::new()).unwrap();
        let mut result = call
            .execute(&graph, FnxReadOptions::DEFAULT_EXECUTION_LIMITS, || {
                Ok::<(), Infallible>(())
            })
            .unwrap();
        assert!(canonical_rows(&result, "bfs", 8).is_ok());
        // Every result value remains correct, but none of these schemas
        // matches the requested YIELD. A values-only comparison accepted them.
        for columns in [
            vec!["vertex", "component"],
            vec!["vertex"],
            vec!["distance", "vertex"],
            vec!["vertex", "distance", "extra"],
        ] {
            result.columns = columns.into_iter().map(str::to_owned).collect();
            assert!(canonical_rows(&result, "bfs", 8).is_err());
        }
    }

    #[test]
    fn success_exit_cannot_disguise_failed_missing_or_duplicate_verification() {
        for text in [
            "",
            "Verification: FAIL",
            "Verification: PASS\nVerification: PASS",
        ] {
            assert!(oracle_metrics(text, true).is_err());
        }
        assert!(oracle_metrics("Verification: PASS", false).is_err());
        assert!(
            oracle_metrics("Verification: PASS", true).is_err(),
            "missing metrics"
        );
    }

    #[test]
    fn full_row_comparison_rejects_wrong_missing_duplicate_and_extra_vertices() {
        let expected = [0, 1, -1];
        assert!(compare_rows(&expected, "0 0\n1 1\n2 -1\n".as_bytes()).is_ok());
        for text in [
            "0 0\n1 2\n2 -1\n",
            "0 0\n1 1\n",
            "0 0\n0 1\n2 -1\n",
            "0 0\n1 1\n2 -1\n3 -1\n",
        ] {
            assert!(compare_rows(&expected, text.as_bytes()).is_err());
        }
    }

    #[test]
    fn input_refuses_weighted_or_out_of_population_edges() {
        assert_eq!(
            input("# SNAP header\n% comment\n0 1\n1 1\n".as_bytes(), 3).unwrap(),
            [(0, 1), (1, 1)]
        );
        for text in ["0 1 3\n", "0 3\n", "-1 0\n", "0\n"] {
            assert!(input(text.as_bytes(), 3).is_err());
        }
    }
}
