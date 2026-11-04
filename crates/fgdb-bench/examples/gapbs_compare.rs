//! Live GAPBS differential comparison through the durable embedded database.
//!
//! ```text
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --fixture smoke --output /tmp/fgdb-gapbs-smoke
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --fixture smoke --algorithm bfs --trials 1 --sealed \
//!   --output /tmp/fgdb-gapbs-bfs-targets
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --fixture hub --vertices 4096 --trials 5 \
//!   --output /tmp/fgdb-gapbs-hub
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --graph email-Eu-core.txt --vertices 1005 \
//!   --source 0 --trials 6 --sealed --output /tmp/fgdb-gapbs-email
//! cargo run --release -p fgdb-bench --example gapbs_compare -- \
//!   --gapbs ../gapbs --graph roads.wel --vertices 10000 --weighted \
//!   --delta 16 --trials 6 --sealed --output /tmp/fgdb-gapbs-weighted
//! ```
//!
//! Pass `--build-note 'profile and per-package --config overrides'` when using
//! custom build settings. This note is explicitly operator-supplied: runtime
//! environment and debug assertions cannot recover every compiler option.
//!
//! Build GAPBS from https://github.com/sbeamer/gapbs, or pass a clean source
//! checkout: this example compiles the actual upstream BFS/CC/TC implementations
//! with a small output adapter. A C++11/OpenMP compiler (`CXX`, default `c++`)
//! plus Git and `rustc` must be available. The checkout's revision, compiler, input blob,
//! arguments, full results, verification transcripts and samples are retained
//! in a NEW output directory. Nothing is deleted and nothing is downloaded.
//! `--algorithm bfs|wcc|tc|sssp` selects one complete comparison, including its
//! verifier and any negative control. Omit it to retain the BFS/WCC/TC suite
//! plus SSSP when `--weighted` is present. Selecting SSSP requires `--weighted`;
//! weighted inputs remain legal for BFS/WCC/TC, which ignore stored weights.
//! Only adapters needed by the selected algorithms are compiled and executed.
//! The real-data example uses the uncompressed edge list from
//! https://snap.stanford.edu/data/email-Eu-core.html (1,005 explicit vertices).
//!
//! Graphs use dense integer IDs in [0, vertices). The directed, unit-weight
//! projection explicitly drops loops and collapses parallel edges, matching
//! GAPBS. The explicit population preserves isolates; a dropped final-vertex
//! loop lets GAPBS's endpoint-based loader retain trailing isolates as well.
//! BFS compares every reachable distance and every unreachable vertex; WCC
//! compares all labels after canonicalizing to each component's minimum ID.
//! Triangle counting symmetrizes that same input and compares the exact total
//! from all database per-vertex counts with GAPBS's native scalar result and
//! TCVerifier. Database vertex counts are fully consumed and retained; GAPBS
//! does not return per-vertex counts, so TC makes no per-vertex parity or
//! identical-output-cost claim. The database's independent fnx/brute-force
//! triangle tests cover each vertex. Duplicate/reciprocal edges collapse and
//! self-loops are dropped before either triangle kernel.
//! At GAPBS revision 941462c7c42e786e4e006a9211b0576ef78d23f7, TCVerifier
//! writes through an empty vector's iterator after reserve, which a checked
//! iterator build rejects. That existing TC lane is not qualified by a printed
//! PASS. `--algorithm bfs` isolates BFS evidence without invoking or changing
//! the defective upstream TC verifier; the default suite remains unchanged.
//! `--weighted` accepts three-column integer-weight edge lists (or assigns
//! deterministic fixture weights) and adds SSSP against upstream DeltaStep and
//! SSSPVerifier. Parallel weights reduce by minimum, including zero. Every
//! distance/unreachable vertex is compared exactly. When SSSP is selected,
//! admission proves all simple-path costs below GAPBS's int32 infinity sentinel
//! and requires a non-loop edge for upstream DeltaStep. BFS/WCC/TC accept any
//! nonnegative int32 input weights and ignore them; arbitrary f64 weights are
//! outside this comparison. `--delta` pins GAPBS's positive bucket width.
//! PageRank is intentionally absent: GAPBS's sink and float32 semantics differ
//! from the registered normalized float64 procedure.
//!
//! Timings are separate: input parsing, durable ingest/reopen, projection,
//! certified execution, complete result encoding, and the actual public CALL.
//! `execute_and_certify_ns` includes the production API's internal typed-row
//! conversion and certificate construction; it is NOT a bare-kernel timer.
//! GAPBS reports input/CSR/kernel/encoding/verification separately. Verification
//! is separate from execution; weighted SSSP uses upstream's complete loader
//! and reports input+CSR together, followed by numeric-domain admission.
//! No loading time is hidden in a kernel
//! comparison. Subsequent database samples may use the ordinary projection
//! cache; `projection_reused` observes that fact rather than assuming it.
//! BFS samples retain the certificate's per-level pull decisions and actual
//! vertex/arc visits; their benefit bounds describe visits, not elapsed time.
//! Both engines run in every paired trial, with alternating order. Raw samples
//! and empirical quantiles describe this machine, not an activated §17 gate,
//! a fitted tail, a server/authorization benchmark, or external-memory parity.
//! After each verified BFS pair, selected point-to-point public CALLs check
//! reachable, unreachable and cutoff-boundary answers against that same live
//! GAPBS result. Their warm request timings and scan counts are separate:
//! GAPBS computes all destinations, so these are correctness comparisons,
//! not point-to-point competitor speedups.
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
//!
//! The same `--sealed` trials also measure `open_sealed_read` followed by a
//! prepared native `CALL ... YIELD ... MATCH (vertex) RETURN ... ORDER BY vertex`.
//! This composed query alternates before/after the three participants above.
//! Statement preparation, the complete decoded+compressed read owner, ordinary
//! pattern matching, execution, full output encoding and release are timed
//! separately. Every emitted value matches both the direct sealed result and
//! the same trial's live GAPBS output. This is a distinct GQL request workload,
//! not a before/after speedup, bare-kernel comparison, or authorization benchmark.
//!
//! After each sealed BFS trial has matched live GAPBS, selected target/cutoff
//! cases also run through fresh read-only direct and cooperative projections
//! and the public compressed CALL+MATCH query owner. The first trial covers all
//! cases; later trials cover a deterministic rotating pair, bounding the added
//! source preparation to six opens per later trial. Sampling scope and case
//! indices are recorded. Each selected case still runs all three modes against
//! that trial's verified distances. Their source root/cut,
//! complete scalar answers and direct/cooperative projection/result digests
//! are checked. Each request includes its own preparation, execution, complete
//! answer encoding and release; no graph owner survives outside its timer.
//! Cooperative construction/execution uses the real runtime yield primitive
//! with 4096-step fuel. These are separate point-to-point request measurements,
//! never speedup ratios against GAPBS's all-destination traversal.

#![forbid(unsafe_code)]

use asupersync::Budget;
use fgdb::{Database, WriteBatch};
use fgdb_bench::{KNOWS, keys};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_prism::{
    FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxResult, FnxValue,
    FnxWeightSpec, GraphView, MissingWeightPolicy, ParallelEdgePolicy, SelfLoopPolicy,
    SnapshotGraphView,
};
use fgdb_types::{CanonicalScalar, CommitCx, EId, PurposeContexts, QueryCx, VId};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Edge = (usize, usize);
type Metrics = BTreeMap<String, u128>;
const WEIGHT: PropertyKeyId = PropertyKeyId(1);
const GAPBS_DISTANCE_INFINITY: i64 = i32::MAX as i64 / 2;

const USAGE: &str = "gapbs_compare --gapbs CHECKOUT --output NEW_DIRECTORY \
    (--graph FILE --vertices N | --fixture smoke|sparse|hub [--vertices N]) \
    [--source 0] [--trials 5] [--threads 1] [--build-note TEXT] [--sealed] \
    [--algorithm bfs|wcc|tc|sssp] [--weighted [--delta 16]]";

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
    algorithm: Option<&'static str>,
    weighted: bool,
    delta: usize,
}

impl Config {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let mut arguments = arguments;
        let mut values = BTreeMap::new();
        let mut sealed = false;
        let mut weighted = false;
        while let Some(key) = arguments.next() {
            if key == "--sealed" || key == "--weighted" {
                let flag = if key == "--sealed" {
                    &mut sealed
                } else {
                    &mut weighted
                };
                if *flag {
                    return Err(format!("repeated {key} option").into());
                }
                *flag = true;
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
                "--algorithm",
                "--delta",
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
        let algorithm = match values.remove("--algorithm").as_deref() {
            None => None,
            Some("bfs") => Some("bfs"),
            Some("wcc") => Some("wcc"),
            Some("tc") => Some("tc"),
            Some("sssp") if weighted => Some("sssp"),
            Some("sssp") => return Err("--algorithm sssp requires --weighted".into()),
            Some(_) => return Err("--algorithm must be bfs, wcc, tc, or sssp".into()),
        };
        if !weighted && values.contains_key("--delta") {
            return Err("--delta requires --weighted".into());
        }
        if values.contains_key("--delta") && algorithm.is_some_and(|name| name != "sssp") {
            return Err("--delta requires the SSSP series".into());
        }
        let mut number = |key: &str, fallback: usize| -> Result<usize> {
            values
                .remove(key)
                .map_or(Ok(fallback), |value| Ok(value.parse()?))
        };
        let source = number("--source", 0)?;
        let trials = number("--trials", 5)?;
        let threads = number("--threads", 1)?;
        let delta = number("--delta", 16)?;
        let build_note = values
            .remove("--build-note")
            .unwrap_or_else(|| "not supplied".to_owned());
        if vertices == 0 || vertices > i32::MAX as usize || source >= vertices {
            return Err("require 0 <= source < vertices <= i32::MAX".into());
        }
        if trials == 0 || threads == 0 {
            return Err("trials and threads must be positive".into());
        }
        if delta == 0 || delta >= GAPBS_DISTANCE_INFINITY as usize {
            return Err("delta must be positive and below GAPBS's distance infinity".into());
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
            algorithm,
            weighted,
            delta,
        })
    }

    fn algorithms(&self) -> &'static [&'static str] {
        match self.algorithm {
            Some("bfs") => &["bfs"],
            Some("wcc") => &["wcc"],
            Some("tc") => &["tc"],
            Some("sssp") => &["sssp"],
            None if self.weighted => &["bfs", "wcc", "tc", "sssp"],
            None => &["bfs", "wcc", "tc"],
            Some(_) => unreachable!("algorithm was validated by Config::parse"),
        }
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

fn weighted_input(
    reader: impl BufRead,
    vertices: usize,
    includes_sssp: bool,
) -> Result<(Vec<Edge>, Vec<i32>)> {
    let mut edges = Vec::new();
    let mut weights = Vec::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('%') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let source: usize = fields.next().ok_or("missing source")?.parse()?;
        let target: usize = fields.next().ok_or("missing target")?.parse()?;
        let weight: i32 = fields.next().ok_or("missing integer weight")?.parse()?;
        if fields.next().is_some() || source >= vertices || target >= vertices || weight < 0 {
            return Err(format!(
                "line {}: expected two IDs in [0, {vertices}) and a nonnegative int32 weight",
                line_number + 1
            )
            .into());
        }
        edges.push((source, target));
        weights.push(weight);
    }
    validate_weighted_input(&edges, &weights, vertices, includes_sssp)?;
    Ok((edges, weights))
}

fn validate_weighted_input(
    edges: &[Edge],
    weights: &[i32],
    vertices: usize,
    includes_sssp: bool,
) -> Result<()> {
    if vertices == 0 || edges.len() != weights.len() {
        return Err("weighted input has an invalid population or weight count".into());
    }
    if weights.iter().any(|&weight| weight < 0) {
        return Err("weighted input requires nonnegative int32 weights".into());
    }
    if !includes_sssp {
        return Ok(());
    }
    let maximum = i64::from(weights.iter().copied().max().unwrap_or(0));
    if maximum >= GAPBS_DISTANCE_INFINITY
        || (vertices.saturating_sub(1) as u128) * (maximum as u128)
            >= GAPBS_DISTANCE_INFINITY as u128
    {
        return Err(
            "weighted input cannot prove every simple path below GAPBS's int32 infinity".into(),
        );
    }
    // Upstream DeltaStep allocates its frontier by arc count and writes the
    // source to slot zero, including for an isolated source. Do not invoke it
    // on a zero-arc graph or modify its kernel to manufacture an oracle result.
    if !edges.iter().any(|&(source, target)| source != target) {
        return Err("upstream GAPBS DeltaStep requires at least one non-loop edge".into());
    }
    Ok(())
}

fn read_options(call: &FnxCallSpec, algorithm: &str) -> FnxReadOptions {
    let mut options = FnxReadOptions::whole_graph_for(call, None);
    options.selection.relation = Some(KNOWS);
    options.projection.self_loops = SelfLoopPolicy::Drop;
    options.projection.parallel_edges = if algorithm == "sssp" {
        options.selection.weight = FnxWeightSpec::Property {
            key: WEIGHT,
            missing: MissingWeightPolicy::Reject,
        };
        ParallelEdgePolicy::Minimum
    } else {
        ParallelEdgePolicy::CollapseUnit
    };
    options
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
    match (
        metrics.contains_key("input_ns"),
        metrics.contains_key("csr_ns"),
        metrics.contains_key("input_csr_ns"),
        metrics.contains_key("domain_validation_ns"),
    ) {
        (true, true, false, false) | (false, false, true, true) => {}
        _ => return Err("missing or ambiguous GAPBS loading phase metrics".into()),
    }
    Ok(metrics)
}

fn oracle_total_ns(metrics: &Metrics) -> Result<u128> {
    let loading = if let Some(combined) = metrics.get("input_csr_ns") {
        combined.checked_add(metrics["domain_validation_ns"])
    } else {
        metrics["input_ns"].checked_add(metrics["csr_ns"])
    };
    loading
        .and_then(|value| value.checked_add(metrics["kernel_ns"]))
        .and_then(|value| value.checked_add(metrics["encoding_ns"]))
        .ok_or("oracle elapsed time overflow".into())
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
    let mut command = Command::new(config.output.join(match algorithm {
        "tc" => "gapbs-tc-adapter",
        "sssp" => "gapbs-sssp-adapter",
        "bfs" | "wcc" => "gapbs-adapter",
        _ => return Err("unsupported GAPBS algorithm".into()),
    }));
    command
        .arg(algorithm)
        .arg(config.output.join(if algorithm == "sssp" {
            "graph.wel"
        } else {
            "graph.el"
        }))
        .arg(config.vertices.to_string())
        .arg(config.source.to_string())
        .arg(&output_path)
        .env("OMP_NUM_THREADS", config.threads.to_string())
        .env("OMP_DYNAMIC", "FALSE");
    if algorithm == "sssp" {
        command.arg(config.delta.to_string());
    }
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
        "bfs" | "sssp" => ["vertex", "distance"],
        "wcc" => ["vertex", "component"],
        "tc" => ["vertex", "triangles"],
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
            ("bfs" | "tc", FnxValue::Integer(value)) => i64::try_from(*value)?,
            ("sssp", FnxValue::Float(value)) => integer_distance(*value)?,
            ("wcc", FnxValue::Vertex(component)) if component.0 < vertices as u128 => {
                i64::try_from(component.0)?
            }
            _ => return Err("production result has the wrong value type".into()),
        };
    }
    if !matches!(algorithm, "bfs" | "sssp") && result.rows.len() != vertices {
        return Err("analytics result omitted vertices".into());
    }
    Ok(values)
}

/// A target query must return exactly its requested vertex, or no row when
/// unreachable. Do not allocate a population-sized comparison vector for a
/// single-row request: that would conceal the public path's actual cost.
fn target_distance(result: &FnxResult, target: usize) -> Result<i64> {
    if result
        .columns
        .iter()
        .map(String::as_str)
        .ne(["vertex", "distance"])
    {
        return Err("target result has the wrong column schema".into());
    }
    match result.rows.as_slice() {
        [] => Ok(-1),
        [row] => match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Integer(distance)]
                if vertex.0 == target as u128 =>
            {
                Ok(i64::try_from(*distance)?)
            }
            _ => Err("target result has the wrong identity or row schema".into()),
        },
        _ => Err("target query returned multiple rows".into()),
    }
}

/// Consume the complete native query result without allocating per-vertex
/// comparison state. An unreachable target is an empty result, not a returned
/// negative sentinel, NULL, or a differently named/typed scalar.
fn target_query_distance(result: &fgdb::QueryResult, target: usize) -> Result<i64> {
    use fgdb_gql::algebra::GraphValue;

    let fgdb::QueryResult::Rows { columns, rows } = result else {
        return Err("target composed query did not produce native rows".into());
    };
    if columns
        .iter()
        .map(String::as_str)
        .ne(["vertex", "distance"])
    {
        return Err("target composed result has the wrong column schema".into());
    }
    match rows.as_slice() {
        [] => Ok(-1),
        [row] => match row.as_slice() {
            [
                fgdb::QueryValue::Value(GraphValue::Vertex(vertex)),
                fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(distance))),
            ] if vertex.0 == target as u128 && *distance >= 0 => Ok(*distance),
            _ => Err("target composed result has the wrong identity or row schema".into()),
        },
        _ => Err("target composed query returned multiple rows".into()),
    }
}

fn target_statement(source: usize, target: usize, cutoff: Option<usize>) -> String {
    let suffix = cutoff.map_or_else(String::new, |cutoff| format!(",{cutoff}"));
    format!("CALL fnx.shortest_path_length({source},{target}{suffix}) YIELD vertex,distance")
}

fn expected_target_distance(
    distances: &[i64],
    target: usize,
    cutoff: Option<usize>,
) -> Result<i64> {
    let distance = *distances
        .get(target)
        .ok_or("target outside verified population")?;
    if distance < -1 {
        return Err("invalid verified target distance".into());
    }
    Ok(
        if distance >= 0 && cutoff.is_none_or(|limit| distance as u128 <= limit as u128) {
            distance
        } else {
            -1
        },
    )
}

fn write_target_row(path: &Path, target: usize, distance: i64) -> Result<()> {
    let mut output = new_file(path)?;
    writeln!(output, "{target} {distance}")?;
    output.flush()?;
    Ok(())
}

/// Cover fixed identities plus a farthest reachable and an unreachable
/// destination when present. Input distances have already matched every row
/// from the current trial's live GAPBS verifier, not an in-house BFS oracle.
fn target_cases(source: usize, distances: &[i64]) -> Result<Vec<(usize, Option<usize>)>> {
    if source >= distances.len()
        || distances[source] != 0
        || distances.iter().any(|&distance| distance < -1)
    {
        return Err("invalid verified BFS population".into());
    }
    let mut targets = vec![source, 0, distances.len() / 2, distances.len() - 1];
    if let Some((target, _)) = distances
        .iter()
        .enumerate()
        .filter(|&(_, &distance)| distance >= 0)
        .max_by_key(|&(_, &distance)| distance)
    {
        targets.push(target);
    }
    if let Some(target) = distances.iter().position(|&distance| distance == -1) {
        targets.push(target);
    }
    targets.sort_unstable();
    targets.dedup();
    let mut cases = Vec::new();
    for target in targets {
        cases.push((target, None));
        if distances[target] >= 0 {
            let distance = usize::try_from(distances[target])?;
            cases.push((target, Some(distance)));
            if let Some(short) = distance.checked_sub(1) {
                cases.push((target, Some(short)));
            }
        } else {
            cases.push((target, Some(distances.len() / 2)));
        }
    }
    Ok(cases)
}

/// Preserve full first-trial coverage without repeatedly authenticating and
/// sealing a large graph for every correctness case on every later trial.
/// Adjacent pairs wrap and advance by one case; the choice depends only on
/// trial number and the already verified case population, never on timings.
fn sealed_target_case_indices(trial: usize, total: usize) -> Vec<usize> {
    if trial == 0 || total <= 2 {
        return (0..total).collect();
    }
    let first = (trial - 1) % total;
    vec![first, (first + 1) % total]
}

fn target_samples(
    config: &Config,
    database: &Database,
    query: &QueryCx,
    trial: usize,
    verified_distances: &[i64],
    log: &mut impl Write,
) -> Result<()> {
    for (target, cutoff) in target_cases(config.source, verified_distances)? {
        let text = target_statement(config.source, target, cutoff);
        let call = FnxCallSpec::bind(&text, &FnxParameters::new())?;
        let options = read_options(&call, "bfs");
        let started = Instant::now();
        let result = database.call_fnx(query, &text, &FnxParameters::new(), options)?;
        let actual = target_distance(&result.analytics, target)?;
        let public_call_ns = started.elapsed().as_nanos();
        let expected = expected_target_distance(verified_distances, target, cutoff)?;
        if actual != expected {
            return Err(format!(
                "target mismatch: source={}, target={target}, cutoff={cutoff:?}, database={actual}, GAPBS={expected}",
                config.source
            )
            .into());
        }
        let cutoff_label = cutoff.map_or_else(|| "none".to_owned(), |value| value.to_string());
        let sample = format!("bfs-{trial}-target-{target}-cutoff-{cutoff_label}");
        let started = Instant::now();
        write_target_row(
            &config.output.join(format!("{sample}.rows")),
            target,
            actual,
        )?;
        let encoding_ns = started.elapsed().as_nanos();
        let certificate = &result.analytics.certificate;
        emit(
            log,
            "target_query_verification",
            &[
                ("sample", sample),
                ("source", config.source.to_string()),
                ("target", target.to_string()),
                ("cutoff", cutoff_label),
                ("distance", actual.to_string()),
                (
                    "warm_public_call_and_consume_ns",
                    public_call_ns.to_string(),
                ),
                ("result_encoding_ns", encoding_ns.to_string()),
                ("kernel", certificate.execution_kernel.to_owned()),
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
                ("result_digest", format!("{:?}", certificate.result_digest)),
                (
                    "projection_cache_state",
                    "preceded by complete BFS; reuse not assumed".to_owned(),
                ),
                (
                    "correctness",
                    "target and cutoff match the same trial's verified live GAPBS BFS".to_owned(),
                ),
                (
                    "comparison_scope",
                    "point-to-point correctness only; GAPBS computes all destinations".to_owned(),
                ),
            ],
        )?;
    }
    Ok(())
}

fn integer_distance(value: f64) -> Result<i64> {
    if !value.is_finite()
        || value < 0.0
        || value >= GAPBS_DISTANCE_INFINITY as f64
        || value.fract() != 0.0
    {
        return Err("SSSP returned a distance outside the admitted exact integer domain".into());
    }
    Ok(value as i64)
}

/// Normalize the actual public GQL rows, without an alternate graph evaluator.
/// Missing BFS rows are unreachable; WCC and TC must retain the full population.
fn canonical_query_rows(
    result: &fgdb::QueryResult,
    algorithm: &str,
    vertices: usize,
) -> Result<Vec<i64>> {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::CanonicalScalar;

    let fgdb::QueryResult::Rows { columns, rows } = result else {
        return Err("composed query did not produce native rows".into());
    };
    let value_column = match algorithm {
        "bfs" | "sssp" => "distance",
        "wcc" => "component",
        "tc" => "triangles",
        _ => return Err("unknown composed result algorithm".into()),
    };
    if columns
        .iter()
        .map(String::as_str)
        .ne(["vertex", value_column])
    {
        return Err("composed result has the wrong column schema".into());
    }
    let mut values = vec![-1; vertices];
    let mut previous = None;
    for row in rows {
        let [fgdb::QueryValue::Value(GraphValue::Vertex(vertex)), value] = row.as_slice() else {
            return Err("composed result has the wrong row schema".into());
        };
        let vertex = usize::try_from(vertex.0)?;
        if vertex >= vertices || previous.is_some_and(|previous| vertex <= previous) {
            return Err("composed rows contain unknown, duplicate or unsorted vertices".into());
        }
        previous = Some(vertex);
        values[vertex] = match (algorithm, value) {
            (
                "bfs" | "tc",
                fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value))),
            ) if *value >= 0 => *value,
            (
                "sssp",
                fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Float(value))),
            ) => integer_distance(value.get())?,
            ("wcc", fgdb::QueryValue::Value(GraphValue::Vertex(component)))
                if component.0 < vertices as u128 =>
            {
                i64::try_from(component.0)?
            }
            _ => return Err("composed result has the wrong value type".into()),
        };
    }
    if !matches!(algorithm, "bfs" | "sssp") && rows.len() != vertices {
        return Err("composed analytics result omitted vertices".into());
    }
    Ok(values)
}

/// GAPBS TC returns one exact graph total; the database returns one count per
/// vertex. Each triangle contributes to exactly three database rows. Preserve
/// and encode those complete rows before reducing only the comparison copy.
fn oracle_rows<'a>(values: &'a [i64], algorithm: &str) -> Result<Cow<'a, [i64]>> {
    if algorithm != "tc" {
        return Ok(Cow::Borrowed(values));
    }
    let incidences = values
        .iter()
        .try_fold(0u128, |sum, &value| -> Result<u128> {
            let value = u128::try_from(value)?;
            sum.checked_add(value)
                .ok_or("triangle count overflow".into())
        })?;
    if !incidences.is_multiple_of(3) {
        return Err("per-vertex triangle counts do not sum to three times a total".into());
    }
    Ok(Cow::Owned(vec![i64::try_from(incidences / 3)?]))
}

fn comparison_scope(algorithm: &str) -> &'static str {
    if algorithm == "tc" {
        "exact triangle total matches; all database vertex counts consumed; upstream TCVerifier PASS"
    } else {
        "all vertices match; upstream verifier PASS"
    }
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
    match algorithm {
        "bfs" => {
            format!("CALL fnx.single_source_shortest_path_length({source}) YIELD vertex,distance")
        }
        "sssp" => {
            format!("CALL fnx.single_source_dijkstra_path_length({source}) YIELD vertex,distance")
        }
        "wcc" => "CALL fnx.weakly_connected_components() YIELD vertex,component".to_owned(),
        "tc" => "CALL fnx.triangles() YIELD vertex,triangles".to_owned(),
        _ => unreachable!("the runner's algorithm set is closed"),
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
    let options = read_options(&call, algorithm);

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
            (
                "projected_arcs",
                projection.incoming_face().1.len().to_string(),
            ),
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
    let arcs = projection.incoming_face().1.len();
    *previous = Some(projection);
    Ok((values, query_ns, arcs))
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
    let options = read_options(&call, algorithm);
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
    let arcs = graph.adjacency_entry_count();
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

/// Time the public compressed GQL owner and its ordinary pattern composition.
/// The row adapter only consumes typed results; the production executor owns
/// every graph operation, including canonical ordering and identity correlation.
#[allow(clippy::too_many_arguments)]
async fn composed_sample(
    config: &Config,
    commit: &CommitCx,
    query: &QueryCx,
    database_path: &Path,
    algorithm: &str,
    prefix: &str,
    expected: (fgdb_types::ids::ObjectId, fgdb_types::CommitSeq),
    log: &mut impl Write,
) -> Result<(Vec<i64>, u128)> {
    let all_started = Instant::now();
    let started = Instant::now();
    let (name, field, arguments, direction) = match algorithm {
        "bfs" => (
            "single_source_shortest_path_length",
            "distance",
            format!("args => [{}], ", config.source),
            "directed",
        ),
        "sssp" => (
            "single_source_dijkstra_path_length",
            "distance",
            format!(
                "args => [{}], weight => 'gap_weight', missing => 'reject', ",
                config.source
            ),
            "directed",
        ),
        "wcc" => (
            "weakly_connected_components",
            "component",
            String::new(),
            "directed",
        ),
        "tc" => ("triangles", "triangles", String::new(), "undirected"),
        _ => return Err("unknown composed algorithm".into()),
    };
    let parallel = if algorithm == "sssp" {
        "min"
    } else {
        "collapse"
    };
    let text = format!(
        "CALL fnx.{name}({arguments}relation => 'R', direction => '{direction}', \
         parallel => '{parallel}', self_loops => 'drop') YIELD vertex,{field} \
         MATCH (vertex) RETURN vertex,{field} ORDER BY vertex"
    );
    let params = fgdb_gql::GqlParameters::new();
    let prepared =
        fgdb::PreparedNativeRead::prepare(&text, &params, |kind, name: &str| match (kind, name) {
            (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                Some(fgdb_gql::GraphSymbol::Relation(KNOWS))
            }
            (fgdb_gql::GraphSymbolKind::Property, "gap_weight") => {
                Some(fgdb_gql::GraphSymbol::Property(WEIGHT))
            }
            _ => None,
        })?;
    let statement_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let view = Database::open_sealed_read(
        commit,
        query,
        database_path,
        keys(),
        Some(expected.1),
        KNOWS,
        Default::default(),
        Default::default(),
    )
    .await?;
    let open_ns = started.elapsed().as_nanos();
    if view.partition_root().0 != expected.0
        || view.snapshot_seq() != expected.1
        || view.frontier() != expected.1
        || view.relation() != KNOWS
    {
        return Err("composed sample changed the source generation or relation".into());
    }
    let policy = fgdb_gql::GqlQueryPolicy::new(100_000_000, 10_000_000, 1 << 40, 10_000_000);
    let memory = FnxMemoryLimits {
        max_kernel_workspace_bytes: 64 * 1024 * 1024,
        max_result_bytes: 64 * 1024 * 1024,
    };
    let started = Instant::now();
    let result = view.execute_prepared(query, &prepared, &params, policy, memory)?;
    let execute_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let values = canonical_query_rows(&result, algorithm, config.vertices)?;
    write_rows(&config.output.join(format!("{prefix}.rows")), &values)?;
    let encoding_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    drop(result);
    drop(view);
    drop(prepared);
    drop(params);
    let release_ns = started.elapsed().as_nanos();
    let total_ns = all_started.elapsed().as_nanos();
    save(
        &config.output.join(format!("{prefix}.gql")),
        text.as_bytes(),
    )?;
    emit(
        log,
        "composed_sealed_query_sample",
        &[
            ("sample", prefix.to_owned()),
            ("algorithm", algorithm.to_owned()),
            ("scope", "trusted-embedded-compressed-CALL-plus-native-MATCH-and-order".to_owned()),
            ("statement_prepare_ns", statement_ns.to_string()),
            ("open_and_prepare_ns", open_ns.to_string()),
            ("execute_query_ns", execute_ns.to_string()),
            ("complete_result_encoding_ns", encoding_ns.to_string()),
            ("release_ns", release_ns.to_string()),
            ("prepare_through_release_ns", total_ns.to_string()),
            ("source_root", format!("{:?}", expected.0)),
            ("as_of", expected.1.0.to_string()),
            ("budgets", "production default source/image/projection; GQL 100M records, 10M rows, 2^40 work, 10M scratch entries; 64 MiB kernel and result each".to_owned()),
            ("release_scope", "native result, decoded+compressed owner and prepared statement; canonical verification rows retained for paired check".to_owned()),
            ("correctness", "pending direct-sealed and live-GAPBS comparison".to_owned()),
        ],
    )?;
    Ok((values, total_ns))
}

struct SealedTargetSample {
    distance: i64,
    total_ns: u128,
    result_digest: String,
}

/// Each target request owns its read-only preparation. The direct/cooperative
/// distinction changes scheduling only; both authenticate the same source and
/// execute the public bound call without a decoded analytics fallback.
#[allow(clippy::too_many_arguments)]
async fn sealed_target_sample(
    config: &Config,
    commit: &CommitCx,
    query: &QueryCx,
    database_path: &Path,
    prefix: &str,
    case: (usize, Option<usize>),
    cooperative: bool,
    expected: (fgdb_types::ids::ObjectId, fgdb_types::CommitSeq),
    reference: &SealedSample,
    log: &mut impl Write,
) -> Result<SealedTargetSample> {
    let (target, cutoff) = case;
    let all_started = Instant::now();
    let started = Instant::now();
    let text = target_statement(config.source, target, cutoff);
    let parameters = FnxParameters::new();
    let call = FnxCallSpec::bind(&text, &parameters)?;
    let mut options = read_options(&call, "bfs");
    options.as_of = Some(expected.1);
    let memory = FnxMemoryLimits {
        max_kernel_workspace_bytes: 64 * 1024 * 1024,
        max_result_bytes: 64 * 1024 * 1024,
    };
    let quantum = NonZeroUsize::new(4096).ok_or("zero cooperative quantum")?;
    let statement_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let graph = if cooperative {
        Database::open_prism_sealed_projection_cooperative(
            commit,
            query,
            database_path,
            keys(),
            options,
            Default::default(),
            Default::default(),
            quantum,
        )
        .await?
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
        || graph.adjacency_entry_count() != reference.arcs
        || graph.binding().root != expected.0
        || graph.binding().as_of != expected.1
    {
        return Err("sealed target preparation changed the verified source or population".into());
    }
    let started = Instant::now();
    let result = if cooperative {
        call.execute_sealed_cooperative(
            query,
            &graph,
            options.execution_limits,
            memory,
            quantum,
            asupersync::runtime::yield_now::yield_now,
        )
        .await?
    } else {
        call.execute_sealed(query, &graph, options.execution_limits, memory)?
    };
    let execute_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let distance = target_distance(&result, target)?;
    write_target_row(
        &config.output.join(format!("{prefix}.rows")),
        target,
        distance,
    )?;
    let encoding_ns = started.elapsed().as_nanos();
    let certificate = &result.certificate;
    let projection_digest = format!("{:?}", certificate.projection_digest);
    if certificate.snapshot != graph.binding()
        || certificate.vertices != config.vertices
        || projection_digest != reference.projection_digest
    {
        return Err("sealed target certificate changed the verified source or projection".into());
    }
    let result_digest = format!("{:?}", certificate.result_digest);
    let kernel = certificate.execution_kernel;
    let kernel_bytes = certificate.kernel_workspace_bytes;
    let nodes = certificate.witness.nodes_touched;
    let arcs = certificate.witness.edges_scanned;
    let rows = result.rows.len();
    let started = Instant::now();
    drop(result);
    drop(graph);
    drop(call);
    drop(parameters);
    let release_ns = started.elapsed().as_nanos();
    let total_ns = all_started.elapsed().as_nanos();
    emit(
        log,
        "sealed_target_query_sample",
        &[
            ("sample", prefix.to_owned()),
            ("source", config.source.to_string()),
            ("target", target.to_string()),
            (
                "cutoff",
                cutoff.map_or_else(|| "none".to_owned(), |value| value.to_string()),
            ),
            ("distance", distance.to_string()),
            ("preparation", "fresh-read-only-checkpoint-seal".to_owned()),
            (
                "scheduling",
                if cooperative {
                    "cooperative"
                } else {
                    "immediate"
                }
                .to_owned(),
            ),
            (
                "cooperative_quantum",
                if cooperative {
                    quantum.to_string()
                } else {
                    "none".to_owned()
                },
            ),
            ("statement_prepare_ns", statement_ns.to_string()),
            ("open_and_prepare_ns", prepare_ns.to_string()),
            ("execute_and_certify_ns", execute_ns.to_string()),
            ("complete_result_encoding_ns", encoding_ns.to_string()),
            ("release_ns", release_ns.to_string()),
            ("prepare_through_release_ns", total_ns.to_string()),
            ("source_root", format!("{:?}", expected.0)),
            ("as_of", expected.1.0.to_string()),
            ("projected_arcs", reference.arcs.to_string()),
            ("result_rows", rows.to_string()),
            ("kernel", kernel.to_owned()),
            ("kernel_workspace_bytes", kernel_bytes.to_string()),
            ("nodes_touched", nodes.to_string()),
            ("edges_scanned", arcs.to_string()),
            ("projection_digest", projection_digest),
            ("result_digest", result_digest.clone()),
            (
                "budgets",
                "production default source/image/projection; 64 MiB kernel and result each"
                    .to_owned(),
            ),
            (
                "release_scope",
                "native result, projection and bound call; scalar verification answer retained"
                    .to_owned(),
            ),
            (
                "correctness",
                "pending same-trial live-GAPBS target/cutoff comparison".to_owned(),
            ),
            (
                "comparison_scope",
                "point-to-point request; no ratio against all-destination GAPBS".to_owned(),
            ),
        ],
    )?;
    Ok(SealedTargetSample {
        distance,
        total_ns,
        result_digest,
    })
}

fn composed_target_statement(source: usize, target: usize, cutoff: Option<usize>) -> String {
    let suffix = cutoff.map_or_else(String::new, |cutoff| format!(",{cutoff}"));
    format!(
        "CALL fnx.shortest_path_length(args => [{source},{target}{suffix}], \
         relation => 'R', direction => 'directed', parallel => 'collapse', \
         self_loops => 'drop') YIELD vertex,distance \
         MATCH (vertex) RETURN vertex,distance ORDER BY vertex"
    )
}

/// The public GQL owner retains its ordinary pattern source as well as the
/// compressed analytics image. Account for that preparation on every request.
#[allow(clippy::too_many_arguments)]
async fn composed_target_sample(
    config: &Config,
    commit: &CommitCx,
    query: &QueryCx,
    database_path: &Path,
    prefix: &str,
    case: (usize, Option<usize>),
    expected: (fgdb_types::ids::ObjectId, fgdb_types::CommitSeq),
    log: &mut impl Write,
) -> Result<(i64, u128)> {
    let (target, cutoff) = case;
    let all_started = Instant::now();
    let started = Instant::now();
    let text = composed_target_statement(config.source, target, cutoff);
    let params = fgdb_gql::GqlParameters::new();
    let prepared =
        fgdb::PreparedNativeRead::prepare(&text, &params, |kind, name: &str| match (kind, name) {
            (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                Some(fgdb_gql::GraphSymbol::Relation(KNOWS))
            }
            _ => None,
        })?;
    let statement_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let view = Database::open_sealed_read(
        commit,
        query,
        database_path,
        keys(),
        Some(expected.1),
        KNOWS,
        Default::default(),
        Default::default(),
    )
    .await?;
    let prepare_ns = started.elapsed().as_nanos();
    if view.partition_root().0 != expected.0
        || view.snapshot_seq() != expected.1
        || view.frontier() != expected.1
        || view.relation() != KNOWS
    {
        return Err("composed target changed the verified source generation or relation".into());
    }
    let policy = fgdb_gql::GqlQueryPolicy::new(100_000_000, 10_000_000, 1 << 40, 10_000_000);
    let memory = FnxMemoryLimits {
        max_kernel_workspace_bytes: 64 * 1024 * 1024,
        max_result_bytes: 64 * 1024 * 1024,
    };
    let started = Instant::now();
    let result = view.execute_prepared(query, &prepared, &params, policy, memory)?;
    let execute_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    let distance = target_query_distance(&result, target)?;
    write_target_row(
        &config.output.join(format!("{prefix}.rows")),
        target,
        distance,
    )?;
    let encoding_ns = started.elapsed().as_nanos();
    let started = Instant::now();
    drop(result);
    drop(view);
    drop(prepared);
    drop(params);
    let release_ns = started.elapsed().as_nanos();
    let total_ns = all_started.elapsed().as_nanos();
    save(
        &config.output.join(format!("{prefix}.gql")),
        text.as_bytes(),
    )?;
    emit(
        log,
        "composed_sealed_target_query_sample",
        &[
            ("sample", prefix.to_owned()),
            ("source", config.source.to_string()),
            ("target", target.to_string()),
            ("cutoff", cutoff.map_or_else(|| "none".to_owned(), |value| value.to_string())),
            ("distance", distance.to_string()),
            ("result_rows", usize::from(distance >= 0).to_string()),
            ("scope", "trusted-embedded-compressed-target-CALL-plus-native-MATCH-and-order".to_owned()),
            ("statement_prepare_ns", statement_ns.to_string()),
            ("open_and_prepare_ns", prepare_ns.to_string()),
            ("execute_query_ns", execute_ns.to_string()),
            ("complete_result_encoding_ns", encoding_ns.to_string()),
            ("release_ns", release_ns.to_string()),
            ("prepare_through_release_ns", total_ns.to_string()),
            ("source_root", format!("{:?}", expected.0)),
            ("as_of", expected.1.0.to_string()),
            ("budgets", "production default source/image/projection; GQL 100M records, 10M rows, 2^40 work, 10M scratch entries; 64 MiB kernel and result each".to_owned()),
            ("release_scope", "native result, decoded+compressed owner and prepared statement; scalar verification answer retained".to_owned()),
            ("correctness", "pending same-trial live-GAPBS target/cutoff comparison".to_owned()),
            ("comparison_scope", "point-to-point composed request; no ratio against all-destination GAPBS".to_owned()),
        ],
    )?;
    Ok((distance, total_ns))
}

#[allow(clippy::too_many_arguments)]
async fn sealed_target_samples(
    config: &Config,
    commit: &CommitCx,
    query: &QueryCx,
    database_path: &Path,
    trial: usize,
    expected: (fgdb_types::ids::ObjectId, fgdb_types::CommitSeq),
    reference: &SealedSample,
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
    let cases = target_cases(config.source, &reference.values)?;
    let selected = sealed_target_case_indices(trial, cases.len());
    let sampling_scope = if trial == 0 {
        "complete target/cutoff case population"
    } else {
        "deterministic rotating adjacent pair; first index = (trial-1) modulo case count"
    };
    emit(
        log,
        "sealed_target_sampling",
        &[
            ("trial", trial.to_string()),
            ("sampling_scope", sampling_scope.to_owned()),
            ("available_cases", cases.len().to_string()),
            ("selected_cases", selected.len().to_string()),
            (
                "selected_case_indices",
                selected
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            ("fresh_owner_preparations", (3 * selected.len()).to_string()),
            (
                "oracle_scope",
                "each selected case uses this trial's complete verified live GAPBS BFS".to_owned(),
            ),
        ],
    )?;
    for index in selected {
        let case @ (target, cutoff) = cases[index];
        let wanted = expected_target_distance(&reference.values, target, cutoff)?;
        let cutoff_label = cutoff.map_or_else(|| "none".to_owned(), |value| value.to_string());
        let prefix = format!("bfs-{trial}-sealed-target-{target}-cutoff-{cutoff_label}");
        let order = ORDERS[(trial % ORDERS.len() + index % ORDERS.len()) % ORDERS.len()];
        let mut immediate = None;
        let mut cooperative = None;
        let mut composed = None;
        for mode in order {
            let sample = format!(
                "{prefix}-{}",
                ["immediate", "cooperative", "composed"][mode]
            );
            if mode == 2 {
                composed = Some(
                    composed_target_sample(
                        config,
                        commit,
                        query,
                        database_path,
                        &sample,
                        case,
                        expected,
                        log,
                    )
                    .await?,
                );
            } else {
                let result = sealed_target_sample(
                    config,
                    commit,
                    query,
                    database_path,
                    &sample,
                    case,
                    mode == 1,
                    expected,
                    reference,
                    log,
                )
                .await?;
                if mode == 1 {
                    cooperative = Some(result);
                } else {
                    immediate = Some(result);
                }
            }
        }
        let immediate = immediate.ok_or("immediate sealed target sample missing")?;
        let cooperative = cooperative.ok_or("cooperative sealed target sample missing")?;
        let (composed, composed_ns) = composed.ok_or("composed sealed target sample missing")?;
        if immediate.distance != wanted
            || cooperative.distance != wanted
            || composed != wanted
            || immediate.result_digest != cooperative.result_digest
        {
            return Err(format!(
                "sealed target mismatch: source={}, target={target}, cutoff={cutoff:?}, \
                 immediate={}, cooperative={}, composed={composed}, GAPBS={wanted}",
                config.source, immediate.distance, cooperative.distance,
            )
            .into());
        }
        emit(
            log,
            "sealed_target_query_verification",
            &[
                ("sample", prefix),
                ("trial", trial.to_string()),
                ("sampling_scope", sampling_scope.to_owned()),
                ("case_index", index.to_string()),
                ("source", config.source.to_string()),
                ("target", target.to_string()),
                ("cutoff", cutoff_label),
                ("distance", wanted.to_string()),
                ("order", order.map(|mode| ["immediate", "cooperative", "composed"][mode]).join(",")),
                ("source_root", format!("{:?}", expected.0)),
                ("as_of", expected.1.0.to_string()),
                ("immediate_prepare_through_release_ns", immediate.total_ns.to_string()),
                ("cooperative_prepare_through_release_ns", cooperative.total_ns.to_string()),
                ("composed_prepare_through_release_ns", composed_ns.to_string()),
                ("correctness", "complete scalar target/cutoff answers match the same trial's verified live GAPBS BFS; pinned source identities and direct/cooperative digests match".to_owned()),
                ("comparison_scope", "separate point-to-point request modes; GAPBS computes all destinations; no competitor speedup ratio".to_owned()),
            ],
        )?;
    }
    Ok(())
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
    for &algorithm in config.algorithms() {
        let mut read_only_times = Vec::new();
        let mut writable_times = Vec::new();
        let mut gapbs_times = Vec::new();
        let mut composed_times = Vec::new();
        for trial in 0..config.trials {
            let triplet_order = ORDERS[trial % ORDERS.len()];
            let order = if trial.is_multiple_of(2) {
                [3, triplet_order[0], triplet_order[1], triplet_order[2]]
            } else {
                [triplet_order[0], triplet_order[1], triplet_order[2], 3]
            };
            let mut read_only = None;
            let mut writable = None;
            let mut oracle = None;
            let mut composed = None;
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
                    2 => oracle = Some(run_oracle(config, algorithm, &oracle_prefix, false)?),
                    _ => {
                        composed = Some(
                            composed_sample(
                                config,
                                commit,
                                query,
                                database_path,
                                algorithm,
                                &format!("{algorithm}-{trial}-composed-sealed-query"),
                                expected,
                                log,
                            )
                            .await?,
                        );
                    }
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
            let (composed_values, composed_ns) = composed.ok_or("composed query missing")?;
            if composed_values != read_only.values {
                return Err("composed native query and direct sealed result disagree".into());
            }
            let metrics =
                oracle_metrics(&String::from_utf8(oracle.stdout)?, oracle.status.success())?;
            if metrics["vertices"] != config.vertices as u128
                || metrics["arcs"] != read_only.arcs as u128
            {
                return Err("live GAPBS and sealed populations differ".into());
            }
            compare_rows(
                &oracle_rows(&read_only.values, algorithm)?,
                BufReader::new(File::open(&rows)?),
            )?;
            compare_rows(
                &oracle_rows(&composed_values, algorithm)?,
                BufReader::new(File::open(&rows)?),
            )?;
            let gapbs_ns = oracle_total_ns(&metrics)?;
            emit(
                log,
                "sealed_triplet_verification",
                &[
                    ("algorithm", algorithm.to_owned()),
                    ("trial", trial.to_string()),
                    ("order", triplet_order.map(|actor| ["readonly", "writable", "gapbs"][actor]).join(",")),
                    ("correctness", format!("sealed rows, populations and identities match; {}", comparison_scope(algorithm))),
                    ("read_only_open_through_release_ns", read_only.total_ns.to_string()),
                    ("writable_open_through_release_ns", writable.total_ns.to_string()),
                    ("gapbs_input_through_result_ns", gapbs_ns.to_string()),
                    ("gapbs_verification_ns", metrics["verification_ns"].to_string()),
                    ("comparison_scope", "old/new sealed is maintenance; live GAPBS has independently reported phases".to_owned()),
                ],
            )?;
            emit(
                log,
                "composed_sealed_query_verification",
                &[
                    ("algorithm", algorithm.to_owned()),
                    ("trial", trial.to_string()),
                    ("order", order.map(|actor| ["readonly", "writable", "gapbs", "composed"][actor]).join(",")),
                    ("correctness", format!("complete native query rows match direct sealed execution; {}", comparison_scope(algorithm))),
                    ("prepare_through_release_ns", composed_ns.to_string()),
                    ("gapbs_input_through_result_ns", gapbs_ns.to_string()),
                    ("comparison_scope", "composed GQL request and live GAPBS have separate workload scopes; no before/after speedup claim".to_owned()),
                ],
            )?;
            if algorithm == "bfs" {
                // Only use this distance vector after every row has matched
                // the same trial's live upstream BFS and verifier. Target
                // requests do not enter the all-destination timing series.
                sealed_target_samples(
                    config,
                    commit,
                    query,
                    database_path,
                    trial,
                    expected,
                    &read_only,
                    log,
                )
                .await?;
            }
            read_only_times.push(read_only.total_ns);
            writable_times.push(writable.total_ns);
            gapbs_times.push(gapbs_ns);
            composed_times.push(composed_ns);
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
        summary(
            log,
            &format!("{algorithm}:composed-sealed-query-prepare-through-release"),
            &mut composed_times,
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
    for (binary, triangle) in [("gapbs-adapter", false), ("gapbs-tc-adapter", true)] {
        let selected = if triangle {
            config.algorithms().contains(&"tc")
        } else {
            config
                .algorithms()
                .iter()
                .any(|name| matches!(*name, "bfs" | "wcc"))
        };
        if !selected {
            continue;
        }
        let mut build = Command::new(&compiler);
        build.args(["-std=c++11", "-O3", "-Wall", "-fopenmp"]);
        if triangle {
            // tc.cc opts into libstdc++ parallel algorithms before including
            // headers. Keep it in its own translation unit so those macros
            // cannot change the existing BFS/WCC comparator builds.
            build.arg("-DFGDB_TC_ORACLE");
        }
        command_output(
            build
                .arg("-I")
                .arg(config.gapbs.join("src"))
                .arg(config.output.join("gapbs-adapter.cc"))
                .arg("-o")
                .arg(config.output.join(binary)),
        )?;
    }
    if config.algorithms().contains(&"sssp") {
        save(
            &config.output.join("gapbs-sssp-adapter.cc"),
            GAPBS_SSSP_ADAPTER.as_bytes(),
        )?;
        command_output(
            Command::new(&compiler)
                .args(["-std=c++11", "-O3", "-Wall", "-fopenmp"])
                .arg("-I")
                .arg(config.gapbs.join("src"))
                .arg(config.output.join("gapbs-sssp-adapter.cc"))
                .arg("-o")
                .arg(config.output.join("gapbs-sssp-adapter")),
        )?;
    }
    let started = Instant::now();
    let (mut edges, mut weights) = if let Some(path) = &config.graph {
        if config.weighted {
            let (edges, weights) = weighted_input(
                BufReader::new(File::open(path)?),
                config.vertices,
                config.algorithms().contains(&"sssp"),
            )?;
            (edges, Some(weights))
        } else {
            (
                input(BufReader::new(File::open(path)?), config.vertices)?,
                None,
            )
        }
    } else {
        let edges = fixture(
            config.fixture.as_deref().ok_or("missing fixture")?,
            config.vertices,
        )?;
        let weights = if config.weighted {
            let weights: Vec<i32> = (0..edges.len())
                .map(|index| ((index % 17 * 7 + 3) % 17) as i32)
                .collect();
            validate_weighted_input(
                &edges,
                &weights,
                config.vertices,
                config.algorithms().contains(&"sssp"),
            )?;
            Some(weights)
        } else {
            None
        };
        (edges, weights)
    };
    let input_ns = started.elapsed().as_nanos();
    let raw_edges = edges.len();
    edges.push((config.vertices - 1, config.vertices - 1));
    if let Some(weights) = &mut weights {
        weights.push(0);
    }
    let started = Instant::now();
    let graph_path = config.output.join("graph.el");
    let mut graph_output = new_file(&graph_path)?;
    for (source, target) in &edges {
        writeln!(graph_output, "{source} {target}")?;
    }
    graph_output.flush()?;
    let weighted_path = if let Some(weights) = &weights {
        let path = config.output.join("graph.wel");
        let mut output = new_file(&path)?;
        for (&(source, target), &weight) in edges.iter().zip(weights) {
            writeln!(output, "{source} {target} {weight}")?;
        }
        output.flush()?;
        Some(path)
    } else {
        None
    };
    let shared_input_encoding_ns = started.elapsed().as_nanos();
    let weighted_blob = weighted_path
        .map(|path| command_output(Command::new("git").arg("hash-object").arg(path)))
        .transpose()?;
    let graph_blob = command_output(Command::new("git").arg("hash-object").arg(&graph_path))?;
    emit(
        &mut log,
        "comparison_header",
        &[
            ("schema", "fgdb-gapbs-comparison-v2".to_owned()),
            ("gapbs_revision", gapbs_revision),
            (
                "gapbs_compiler",
                compiler_version.lines().next().unwrap_or("").to_owned(),
            ),
            (
                "gapbs_build_flags",
                "-std=c++11 -O3 -Wall -fopenmp; separate TC adapter adds -DFGDB_TC_ORACLE".to_owned(),
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
            ("algorithm_selection", config.algorithm.unwrap_or("default-suite").to_owned()),
            ("algorithms", config.algorithms().join(",")),
            (
                "build_identity_scope",
                "HEAD/tree describe the invocation checkout; dirty code and compiler overrides require source digests, binary blob and explicit build note".to_owned(),
            ),
            ("cpu_model", cpu_model),
            ("input_git_blob", graph_blob),
            ("weighted_input_git_blob", weighted_blob.unwrap_or_else(|| "disabled".to_owned())),
            ("weighted_input", config.weighted.to_string()),
            ("weighted_sssp_enabled", config.algorithms().contains(&"sssp").to_string()),
            ("gapbs_sssp_delta", config.delta.to_string()),
            ("sssp_numeric_domain", "nonnegative int32 weights; every simple path < INT_MAX/2; exact integral f64 distances".to_owned()),
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
                "BFS/WCC directed; TC undirected; unit weights/collapse parallel; optional SSSP directed/stored weights/minimum parallel; drop self-loops".to_owned(),
            ),
            (
                "durability",
                "production Database::create/write/open; fsync enabled".to_owned(),
            ),
            ("output", config.output.display().to_string()),
        ],
    )?;
    // This is a real negative control against the upstream verifier, not a
    // fabricated passing log: corrupt the source parent or triangle total and require
    // both the nonzero exit and the explicit FAIL, with no result published.
    for &algorithm in config
        .algorithms()
        .iter()
        .filter(|&&algorithm| algorithm != "wcc")
    {
        let (control, rows) = run_oracle(
            &config,
            algorithm,
            &format!("{algorithm}-negative-control"),
            true,
        )?;
        let control_log = String::from_utf8(control.stdout)?;
        if control.status.success()
            || verification_tokens(&control_log) != ["FAIL"]
            || rows.exists()
            || oracle_metrics(&control_log, control.status.success()).is_ok()
        {
            return Err(
                format!("GAPBS failed the {algorithm} corrupted-result rejection control").into(),
            );
        }
        emit(
            &mut log,
            "negative_control",
            &[
                ("algorithm", algorithm.to_owned()),
                ("outcome", "corrupted result rejected".to_owned()),
            ],
        )?;
    }

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
                weights.as_ref().map_or_else(Vec::new, |weights| {
                    vec![(
                        WEIGHT,
                        CanonicalScalar::Int(i64::from(weights[chunk * 256 + offset])),
                    )]
                }),
            );
        }
        database.write(&commit, batch).await?;
        writes += 1;
    }
    let ingest_ns = started.elapsed().as_nanos();
    drop(edges);
    drop(weights);
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
    for &algorithm in config.algorithms() {
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
            compare_rows(
                &oracle_rows(&values, algorithm)?,
                BufReader::new(File::open(path)?),
            )?;
            let gapbs_ns = oracle_total_ns(&metrics)?;
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
                ("correctness", comparison_scope(algorithm).to_owned()),
                ("input_through_result_ns", gapbs_ns.to_string()),
            ];
            fields.extend(
                metrics
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.to_string())),
            );
            emit(&mut log, "paired_verification", &fields)?;
            if algorithm == "bfs" {
                target_samples(&config, &database, &query, trial, &values, &mut log)?;
            }
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
const GAPBS_SSSP_ADAPTER: &str = r#"
#define main upstream_sssp_main
#include "sssp.cc"
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
    if ((argc != 7 && argc != 8) || std::string(argv[1]) != "sssp")
      throw std::runtime_error("invalid SSSP adapter arguments");
    const NodeID population = std::stoi(argv[3]);
    const NodeID source = std::stoi(argv[4]);
    const WeightT delta = std::stoi(argv[6]);
    if (population <= 0 || source < 0 || source >= population || delta <= 0 || delta >= kDistInf)
      throw std::runtime_error("invalid source/population/delta");
    if (std::ifstream(argv[5]).good()) throw std::runtime_error("output already exists");
    char program[] = "gapbs-sssp-adapter", flag[] = "-f";
    char* builder_args[] = {program, flag, argv[2], nullptr};
    CLApp cli(3, builder_args, "live GAPBS weighted comparison");
    if (!cli.ParseArgs()) throw std::runtime_error("builder arguments failed");
    WeightedBuilder builder(cli);
    auto start = Clock::now();
    // MakeGraph owns the needs_weights flag passed to Reader. Calling
    // MakeGraphFromEL on a newly constructed WeightedBuilder would overwrite
    // the supplied weights with generated ones. Keep the exact public loader
    // and report its input/CSR work as one honestly combined phase.
    WGraph graph = builder.MakeGraph();
    const uint64_t input_csr_ns = ns(start);
    if (graph.num_nodes() != population || graph.num_edges_directed() == 0)
      throw std::runtime_error("invalid population or zero-arc graph for upstream DeltaStep");
    start = Clock::now();
    WeightT maximum = 0;
    for (NodeID vertex = 0; vertex < population; ++vertex) {
      for (WNode neighbor : graph.out_neigh(vertex)) {
        if (neighbor.v < 0 || neighbor.v >= population || neighbor.w < 0)
          throw std::runtime_error("invalid weighted edge");
        maximum = std::max(maximum, neighbor.w);
      }
    }
    if (maximum >= kDistInf || int64_t(population-1)*maximum >= kDistInf)
      throw std::runtime_error("simple-path costs exceed GAPBS's exact finite domain");
    const uint64_t domain_validation_ns = ns(start);
    start = Clock::now();
    auto result = DeltaStep(graph, source, delta);
    const uint64_t kernel_ns = ns(start);
    if (argc == 8) {
      if (std::string(argv[7]) != "corrupt") throw std::runtime_error("invalid negative control");
      result[source] = 1;
    }
    start = Clock::now();
    const bool verified = SSSPVerifier(graph, source, result);
    const uint64_t verification_ns = ns(start);
    PrintLabel("Verification", verified ? "PASS" : "FAIL");
    if (!verified) return 3;
    start = Clock::now();
    std::ofstream output(argv[5]);
    if (!output) throw std::runtime_error("cannot create result file");
    for (NodeID vertex = 0; vertex < population; ++vertex) {
      if (result[vertex] < 0 || result[vertex] > kDistInf)
        throw std::runtime_error("invalid upstream distance");
      output << vertex << " " << (result[vertex] == kDistInf ? -1 : result[vertex]) << '\n';
    }
    output.close();
    if (!output) throw std::runtime_error("result write failed");
    const uint64_t encoding_ns = ns(start);
    metric("input_csr_ns", input_csr_ns);
    metric("domain_validation_ns", domain_validation_ns);
    metric("kernel_ns", kernel_ns);
    metric("verification_ns", verification_ns);
    metric("encoding_ns", encoding_ns);
    metric("vertices", graph.num_nodes());
    metric("arcs", graph.num_edges_directed());
    metric("delta", delta);
    return 0;
  } catch (const std::exception& error) {
    std::cerr << error.what() << std::endl;
    return 2;
  }
}
"#;

const GAPBS_ADAPTER: &str = r#"
#ifdef FGDB_TC_ORACLE
#define main upstream_tc_main
#include "tc.cc"
#undef main
#else
#define main upstream_bfs_main
#include "bfs.cc"
#undef main
#define main upstream_cc_main
#include "cc.cc"
#undef main
#endif
#include <chrono>
#include <fstream>
#include <limits>
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
#ifdef FGDB_TC_ORACLE
    if (algorithm != "tc") throw std::runtime_error("invalid TC algorithm");
#else
    if (algorithm != "bfs" && algorithm != "wcc") throw std::runtime_error("invalid algorithm");
#endif
    const NodeID population = std::stoi(argv[3]);
    const NodeID source = std::stoi(argv[4]);
    if (population <= 0 || source < 0 || source >= population) throw std::runtime_error("invalid source/population");
    if (std::ifstream(argv[5]).good()) throw std::runtime_error("output already exists");
    char program[] = "gapbs-adapter", file_flag[] = "-f", symmetric_file_flag[] = "-sf";
    char* builder_args[] = {program, algorithm == "tc" ? symmetric_file_flag : file_flag, argv[2], nullptr};
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
    pvector<NodeID> result;
    size_t triangles = 0;
#ifdef FGDB_TC_ORACLE
    if (graph.directed()) throw std::runtime_error("TC requires an undirected graph");
    triangles = Hybrid(graph);
#else
    result = algorithm == "bfs" ? DOBFS(graph, source) : Afforest(graph);
#endif
    const uint64_t kernel_ns = ns(start);
    if (argc == 7) {
      if (std::string(argv[6]) != "corrupt") throw std::runtime_error("invalid negative control");
      if (algorithm == "tc") {
        ++triangles;
      } else if (algorithm == "bfs") {
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
#ifdef FGDB_TC_ORACLE
    bool verified = TCVerifier(graph, triangles);
#else
    bool verified = algorithm == "bfs" ? BFSVerifier(graph, source, result) : CCVerifier(graph, result);
#endif
    const uint64_t verification_ns = ns(start);
    PrintLabel("Verification", verified ? "PASS" : "FAIL");
    if (!verified) return 3;
    start = Clock::now();
    std::vector<int64_t> values(algorithm == "tc" ? 1 : population, -1);
    if (algorithm == "tc") {
      if (triangles > static_cast<size_t>(std::numeric_limits<int64_t>::max()))
        throw std::runtime_error("TC total exceeds signed comparison range");
      values[0] = triangles;
    } else if (algorithm == "bfs") {
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
    for (size_t vertex = 0; vertex < values.size(); ++vertex) output << vertex << " " << values[vertex] << '\n';
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

    fn fixture_projection(name: &str, direction: Directedness) -> SnapshotGraphView {
        let vertices: Vec<_> = (0..8).map(VId).collect();
        let edges: Vec<_> = fixture(name, 8)
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
                directedness: direction,
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
    fn target_comparison_checks_identity_schema_cardinality_and_cutoff_boundaries() {
        let graph = fixture_projection("smoke", Directedness::Directed);
        let limits = FnxExecutionLimits {
            max_iterations: 0,
            max_result_rows: 8,
            max_estimated_work: 1000,
        };
        let expected = [0, 1, 1, 2, -1, -1, -1, -1];
        let cases = target_cases(0, &expected).unwrap();
        assert!(cases.contains(&(3, None)));
        assert!(cases.contains(&(3, Some(2))));
        assert!(cases.contains(&(3, Some(1))));
        assert!(cases.contains(&(4, None)));
        assert!(cases.contains(&(0, Some(0))));
        for (target, cutoff) in cases {
            let result = FnxCallSpec::shortest_path_length(VId(0), VId(target as u128), cutoff)
                .execute(&graph, limits, || Ok::<_, Infallible>(()))
                .unwrap();
            let distance = expected[target];
            let wanted = if distance >= 0 && cutoff.is_none_or(|cutoff| distance as usize <= cutoff)
            {
                distance
            } else {
                -1
            };
            assert_eq!(target_distance(&result, target).unwrap(), wanted);
        }
        assert!(target_cases(0, &[]).is_err());
        assert!(target_cases(2, &[0, 1]).is_err());
        assert!(target_cases(0, &[0, -2]).is_err());
        assert!(target_cases(0, &[1, 2]).is_err());

        let mut result = FnxCallSpec::shortest_path_length(VId(0), VId(3), None)
            .execute(&graph, limits, || Ok::<_, Infallible>(()))
            .unwrap();
        assert!(target_distance(&result, 2).is_err());
        result.rows.push(result.rows[0].clone());
        assert!(target_distance(&result, 3).is_err());
        result.rows.pop();
        result.rows[0][1] = FnxValue::Float(2.0);
        assert!(target_distance(&result, 3).is_err());
        result.rows[0][1] = FnxValue::Integer(u64::MAX);
        assert!(target_distance(&result, 3).is_err());
        result.rows.clear();
        result.columns.swap(0, 1);
        assert!(target_distance(&result, 3).is_err());
    }

    #[test]
    fn composed_target_comparison_rejects_identity_schema_cardinality_and_numeric_changes() {
        use fgdb_gql::algebra::GraphValue;

        let vertex = fgdb::QueryValue::Value(GraphValue::Vertex(VId(3)));
        let distance = fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2)));
        let query = |columns: &[&str], rows: Vec<Vec<fgdb::QueryValue>>| fgdb::QueryResult::Rows {
            columns: columns.iter().map(|column| (*column).to_owned()).collect(),
            rows,
        };
        let good_row = vec![vertex.clone(), distance.clone()];
        assert_eq!(
            target_query_distance(&query(&["vertex", "distance"], vec![good_row.clone()]), 3)
                .unwrap(),
            2,
        );
        assert_eq!(
            target_query_distance(&query(&["vertex", "distance"], vec![]), 3).unwrap(),
            -1,
        );
        for columns in [
            vec!["distance", "vertex"],
            vec!["vertex"],
            vec!["vertex", "distance", "extra"],
            vec!["vertex", "score"],
        ] {
            assert!(target_query_distance(&query(&columns, vec![good_row.clone()]), 3).is_err());
            assert!(target_query_distance(&query(&columns, vec![]), 3).is_err());
        }
        for rows in [
            vec![good_row.clone(), good_row.clone()],
            vec![vec![vertex.clone()]],
            vec![vec![vertex.clone(), distance.clone(), distance.clone()]],
            vec![vec![
                fgdb::QueryValue::Value(GraphValue::Vertex(VId(2))),
                distance.clone(),
            ]],
            vec![vec![
                fgdb::QueryValue::Value(GraphValue::Vertex(VId((1u128 << 96) | 3))),
                distance.clone(),
            ]],
            vec![vec![vertex.clone(), vertex.clone()]],
            vec![vec![
                vertex.clone(),
                fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(-1))),
            ]],
            vec![vec![
                vertex.clone(),
                fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            ]],
        ] {
            assert!(target_query_distance(&query(&["vertex", "distance"], rows), 3).is_err());
        }
        let verified = [0, 1, 1, 2, -1];
        assert_eq!(expected_target_distance(&verified, 3, None).unwrap(), 2);
        assert_eq!(expected_target_distance(&verified, 3, Some(2)).unwrap(), 2);
        assert_eq!(expected_target_distance(&verified, 3, Some(1)).unwrap(), -1);
        assert_eq!(expected_target_distance(&verified, 0, Some(0)).unwrap(), 0);
        assert_eq!(
            expected_target_distance(&verified, 4, Some(10)).unwrap(),
            -1
        );
        assert!(expected_target_distance(&verified, 5, None).is_err());
        assert!(expected_target_distance(&[-2], 0, None).is_err());
    }

    #[test]
    fn target_statements_bind_both_public_call_and_composed_query_forms() {
        let parameters = FnxParameters::new();
        let gql = fgdb_gql::GqlParameters::new();
        for (source, target, cutoff) in [
            (0, 3, None),
            (0, 3, Some(1)),
            (0, 3, Some(2)),
            (0, 0, Some(0)),
            (0, 7, None),
        ] {
            let bound =
                FnxCallSpec::bind(&target_statement(source, target, cutoff), &parameters).unwrap();
            assert_eq!(
                bound.algorithm(),
                FnxCallSpec::shortest_path_length(VId(source as u128), VId(target as u128), cutoff)
                    .algorithm(),
            );
            assert!(bound.supports_cooperative_sealed_execution());
            fgdb::PreparedNativeRead::prepare(
                &composed_target_statement(source, target, cutoff),
                &gql,
                |kind, name: &str| match (kind, name) {
                    (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                        Some(fgdb_gql::GraphSymbol::Relation(KNOWS))
                    }
                    _ => None,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn sealed_target_followups_rotate_without_reopening_every_case() {
        let cases = target_cases(0, &[0, 1, 1, 2, -1, -1, -1, -1]).unwrap();
        assert_eq!(cases.len(), 9);
        assert_eq!(
            sealed_target_case_indices(0, cases.len()),
            (0..9).collect::<Vec<_>>()
        );
        for (trial, expected) in [(1, [0, 1]), (2, [1, 2]), (9, [8, 0]), (10, [0, 1])] {
            assert_eq!(sealed_target_case_indices(trial, cases.len()), expected);
        }
        for trial in [0, 1, usize::MAX] {
            assert!(sealed_target_case_indices(trial, 0).is_empty());
            assert_eq!(sealed_target_case_indices(trial, 1), [0]);
            assert_eq!(sealed_target_case_indices(trial, 2), [0, 1]);
        }
        let last = sealed_target_case_indices(usize::MAX, cases.len());
        assert_eq!(last.len(), 2);
        assert!(last.iter().all(|&index| index < cases.len()));
        assert_ne!(last[0], last[1]);
    }

    #[test]
    fn complete_smoke_results_preserve_diamonds_duplicates_loops_and_isolates() {
        let graph = fixture_projection("smoke", Directedness::Directed);
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
    fn triangle_total_consumes_all_vertex_counts_and_rejects_incomplete_results() {
        let graph = fixture_projection("hub", Directedness::Undirected);
        let call = FnxCallSpec::bind(&statement("tc", 0), &FnxParameters::new()).unwrap();
        let mut result = call
            .execute(&graph, FnxReadOptions::DEFAULT_EXECUTION_LIMITS, || {
                Ok::<(), Infallible>(())
            })
            .unwrap();
        let values = canonical_rows(&result, "tc", 8).unwrap();
        assert_eq!(values, [5, 1, 2, 2, 2, 2, 1, 0]);
        assert_eq!(&*oracle_rows(&values, "tc").unwrap(), &[5]);
        assert_eq!(graph.edge_count(), 11);
        assert_eq!(graph.incoming_face().1.len(), 22);
        assert!(oracle_rows(&[1, 0, 0], "tc").is_err());
        assert!(oracle_rows(&[-1, 1, 0], "tc").is_err());
        assert!(oracle_rows(&[i64::MAX; 6], "tc").is_err());
        result.rows.pop();
        assert!(canonical_rows(&result, "tc", 8).is_err());
    }

    #[test]
    fn result_normalization_rejects_wrong_missing_reordered_and_extra_columns() {
        let graph = fixture_projection("smoke", Directedness::Directed);
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
    fn official_weighted_loader_keeps_one_combined_phase_without_double_counting() {
        let common = "Verification: PASS\nFGDB_GAPBS_METRIC kernel_ns 7\n\
            FGDB_GAPBS_METRIC verification_ns 100\nFGDB_GAPBS_METRIC encoding_ns 11\n\
            FGDB_GAPBS_METRIC vertices 8\nFGDB_GAPBS_METRIC arcs 7\n";
        for (loading, total) in [
            (
                "FGDB_GAPBS_METRIC input_ns 2\nFGDB_GAPBS_METRIC csr_ns 3\n",
                23,
            ),
            (
                "FGDB_GAPBS_METRIC input_csr_ns 5\nFGDB_GAPBS_METRIC domain_validation_ns 13\n",
                36,
            ),
        ] {
            let metrics = oracle_metrics(&format!("{common}{loading}"), true).unwrap();
            assert_eq!(oracle_total_ns(&metrics).unwrap(), total);
        }
        for loading in [
            "FGDB_GAPBS_METRIC input_csr_ns 5\n",
            "FGDB_GAPBS_METRIC input_ns 2\n",
            "FGDB_GAPBS_METRIC input_ns 2\nFGDB_GAPBS_METRIC csr_ns 3\nFGDB_GAPBS_METRIC input_csr_ns 5\nFGDB_GAPBS_METRIC domain_validation_ns 13\n",
        ] {
            assert!(oracle_metrics(&format!("{common}{loading}"), true).is_err());
        }
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

    #[test]
    fn weighted_input_preserves_zero_parallel_and_loop_weights_and_proves_numeric_domain() {
        let text = "# weighted road graph\n0 1 9\n0 1 3\n1 2 0\n2 2 7\n";
        let (edges, weights) = weighted_input(text.as_bytes(), 4, true).unwrap();
        assert_eq!(edges, [(0, 1), (0, 1), (1, 2), (2, 2)]);
        assert_eq!(weights, [9, 3, 0, 7]);
        for invalid in [
            "0 1\n",
            "0 1 -1\n",
            "0 1 0.5\n",
            "0 1 3 extra\n",
            "0 4 1\n",
            "0 1 2147483648\n",
        ] {
            for includes_sssp in [false, true] {
                assert!(
                    weighted_input(invalid.as_bytes(), 4, includes_sssp).is_err(),
                    "{invalid}; includes_sssp={includes_sssp}"
                );
            }
        }
        for sssp_only_invalid in ["0 1 1073741823\n", "0 1 536870912\n", "0 0 1\n", ""] {
            assert!(
                weighted_input(sssp_only_invalid.as_bytes(), 4, true).is_err(),
                "{sssp_only_invalid}"
            );
            assert!(
                weighted_input(sssp_only_invalid.as_bytes(), 4, false).is_ok(),
                "{sssp_only_invalid}"
            );
        }
        // The upper bound is strict: an otherwise reachable path must never
        // collide with GAPBS's unreachable sentinel.
        assert!(validate_weighted_input(&[(0, 1)], &[357_913_940], 4, true).is_ok());
        assert!(validate_weighted_input(&[(0, 1)], &[357_913_941], 4, true).is_err());
        for includes_sssp in [false, true] {
            assert!(validate_weighted_input(&[(0, 1)], &[], 4, includes_sssp).is_err());
            assert!(validate_weighted_input(&[(0, 1)], &[-1], 4, includes_sssp).is_err());
            assert!(validate_weighted_input(&[], &[], 0, includes_sssp).is_err());
        }
    }

    #[test]
    fn weighted_selector_applies_delta_step_bounds_only_to_sssp() {
        for (vertices, text, edge, weight) in [
            (1, "0 0 0\n", (0, 0), 0),
            (2, "0 1 2000000000\n", (0, 1), 2_000_000_000),
        ] {
            for selection in [
                "--algorithm bfs",
                "--algorithm wcc",
                "--algorithm tc",
                "--algorithm sssp",
                "",
            ] {
                let config = Config::parse(
                    format!(
                        "--gapbs ../gapbs --output unused --graph unused.wel \
                         --vertices {vertices} --weighted {selection}"
                    )
                    .split_whitespace()
                    .map(str::to_owned),
                )
                .unwrap();
                let includes_sssp = config.algorithms().contains(&"sssp");
                let result = weighted_input(text.as_bytes(), config.vertices, includes_sssp);
                if includes_sssp {
                    assert!(result.is_err(), "{selection}; {text}");
                } else {
                    assert_eq!(result.unwrap(), (vec![edge], vec![weight]));
                }
                // Fixture weights pass through the same admission helper.
                assert_eq!(
                    validate_weighted_input(&[edge], &[weight], config.vertices, includes_sssp)
                        .is_ok(),
                    !includes_sssp,
                    "fixture admission: {selection}; {text}"
                );
            }
        }
    }

    #[test]
    fn weighted_complete_results_match_independent_integral_shortest_paths() {
        let text = "0 1 9\n0 2 2\n1 3 3\n2 3 10\n3 1 0\n0 1 4\n3 3 1\n4 5 5\n5 4 1\n";
        let (edges, weights) = weighted_input(text.as_bytes(), 8, true).unwrap();
        let call = FnxCallSpec::bind(&statement("sssp", 0), &FnxParameters::new()).unwrap();
        let options = read_options(&call, "sssp");
        assert_eq!(
            options.selection.weight,
            FnxWeightSpec::Property {
                key: WEIGHT,
                missing: MissingWeightPolicy::Reject,
            }
        );
        assert_eq!(
            options.projection.parallel_edges,
            ParallelEdgePolicy::Minimum
        );
        let vertices: Vec<_> = (0..8).map(VId).collect();
        let projected: Vec<_> = edges
            .iter()
            .zip(&weights)
            .enumerate()
            .map(|(index, (&(source, target), &weight))| ProjectionEdge {
                eid: EId(index as u128),
                source: VId(source as u128),
                target: VId(target as u128),
                weight: f64::from(weight),
            })
            .collect();
        let graph = SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([7; 32]),
                as_of: CommitSeq(1),
            },
            &vertices,
            &projected,
            options.projection,
            options.projection_limits,
        )
        .unwrap();
        // Independent integer Bellman-Ford over raw input, with neither the
        // projection's reduction nor the production queue implementation.
        for source in 0..8 {
            let mut expected = vec![GAPBS_DISTANCE_INFINITY; 8];
            expected[source] = 0;
            for _ in 1..8 {
                for (&(from, to), &weight) in edges.iter().zip(&weights) {
                    if expected[from] != GAPBS_DISTANCE_INFINITY {
                        expected[to] = expected[to].min(expected[from] + i64::from(weight));
                    }
                }
            }
            for value in &mut expected {
                if *value == GAPBS_DISTANCE_INFINITY {
                    *value = -1;
                }
            }
            let call =
                FnxCallSpec::bind(&statement("sssp", source), &FnxParameters::new()).unwrap();
            let result = call
                .execute(
                    &graph,
                    options.execution_limits,
                    || Ok::<(), Infallible>(()),
                )
                .unwrap();
            let actual = canonical_rows(&result, "sssp", 8).unwrap();
            assert_eq!(actual, expected);
            if source == 0 {
                assert_eq!(actual, [0, 4, 2, 7, -1, -1, -1, -1]);
            }
        }
        for value in [
            f64::NAN,
            f64::INFINITY,
            -1.0,
            0.5,
            GAPBS_DISTANCE_INFINITY as f64,
        ] {
            assert!(integer_distance(value).is_err());
        }
        assert_eq!(integer_distance(0.0).unwrap(), 0);
        assert_eq!(
            integer_distance((GAPBS_DISTANCE_INFINITY - 1) as f64).unwrap(),
            GAPBS_DISTANCE_INFINITY - 1
        );
    }

    #[test]
    fn weighted_cli_is_explicit_and_delta_is_a_positive_bounded_integer() {
        let parse = |flags: &str| {
            Config::parse(
                format!("--gapbs ../gapbs --output unused --fixture smoke {flags}")
                    .split_whitespace()
                    .map(str::to_owned),
            )
        };
        assert_eq!(parse("").unwrap().algorithms(), ["bfs", "wcc", "tc"]);
        let weighted = parse("--weighted --delta 3 --sealed").unwrap();
        assert_eq!(weighted.algorithms(), ["bfs", "wcc", "tc", "sssp"]);
        assert_eq!(weighted.delta, 3);
        assert!(weighted.sealed);
        for flags in [
            "--delta 3",
            "--weighted --weighted",
            "--weighted --delta 0",
            "--weighted --delta 1073741823",
            "--weighted --delta -1",
        ] {
            assert!(parse(flags).is_err(), "{flags}");
        }
    }

    #[test]
    fn algorithm_selector_is_exact_and_preserves_default_and_weighted_domains() {
        let parse = |flags: &str| {
            Config::parse(
                format!("--gapbs ../gapbs --output unused --fixture smoke {flags}")
                    .split_whitespace()
                    .map(str::to_owned),
            )
        };
        assert_eq!(parse("").unwrap().algorithms(), ["bfs", "wcc", "tc"]);
        assert_eq!(
            parse("--weighted").unwrap().algorithms(),
            ["bfs", "wcc", "tc", "sssp"]
        );
        for algorithm in ["bfs", "wcc", "tc"] {
            let selected = parse(&format!("--algorithm {algorithm}")).unwrap();
            assert_eq!(selected.algorithms(), [algorithm]);
            assert!(!selected.weighted);
            let weighted = parse(&format!("--algorithm {algorithm} --weighted --sealed")).unwrap();
            assert_eq!(weighted.algorithms(), [algorithm]);
            assert!(weighted.weighted);
            assert!(weighted.sealed);
        }
        for flags in [
            "--weighted --algorithm sssp --delta 3",
            "--algorithm sssp --weighted",
        ] {
            let selected = parse(flags).unwrap();
            assert_eq!(selected.algorithms(), ["sssp"]);
            assert!(selected.weighted);
        }
        for flags in [
            "--algorithm",
            "--algorithm unknown",
            "--algorithm BFS",
            "--algorithm bfs,wcc",
            "--algorithm bfs --algorithm tc",
            "--algorithm --weighted",
            "--algorithm sssp",
            "--algorithm sssp --delta 3",
            "--algorithm bfs --weighted --delta 3",
            "--algorithm tc --weighted --delta 3",
            "--algorithm sssp --weighted --delta 0",
        ] {
            assert!(parse(flags).is_err(), "{flags}");
        }
    }
}
