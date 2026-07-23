//! Honest registry-independent scalar graph-codec pipeline proof.
//!
//! This example deliberately proves only the mechanics that exist today:
//! deterministic graph generation, all three explicit neighbor arms,
//! type-safe scalar identity columns tied to that graph, a canonical diagnostic
//! adjacency transcript, scalar block round-trip/scan, and execution inside an
//! asupersync lab root task.
//!
//! It is **not** evidence for durable framing, registered codec IDs, a logical
//! digest, SIMD parity, `OriginBirthOrder`, delta/FOR identity slots, or the
//! production seal/run layout, codec-specific chaos/cancellation behavior, or
//! the final `codec_pipeline_e2e` gate. Elias-Fano and dense intervals expose
//! no encoded byte slice, so this proof emits no invented byte evidence for
//! those arms. Identity evidence covers only the explicitly non-durable scalar
//! payload below its future registered envelope.

#![forbid(unsafe_code)]

use std::{
    error::Error,
    fmt,
    io::{self, Write as _},
};

use asupersync::lab::run_async_under_lab;
use fgdb_codec::{
    block::{CodecProfile, OutputLimit},
    evidence::CodecRunRow,
    identity::{IdentityColumnLimits, IdentityParts, IdentityRepresentation, SortedIdentityColumn},
    kernel::{BlockKernel, IdentityColumnKernel, KernelOutput, NeighborKernel, ScalarKernels},
    neighbor::{EncodedNeighbors, EntryLimit, NeighborCodec},
};
use fgdb_types::{EId, VId};
use fnx_generators::GraphGenerator;

const NODE_COUNT: usize = 64;
const ATTACHMENT_COUNT: usize = 3;
const GENERATOR_SEED: u64 = 424_242;
const LAB_SEED: u64 = 0x5ca1_ab1e_2026_0723;
const EXPECTED_EDGE_COUNT: usize =
    ATTACHMENT_COUNT + (NODE_COUNT - ATTACHMENT_COUNT - 1) * ATTACHMENT_COUNT;
const FIXTURE_ID: &str = "barabasi-albert-n64-m3-seed424242";
const DIAGNOSTIC_TRANSCRIPT_MAGIC: &[u8] = b"FGDB-DIAGNOSTIC-ADJACENCY-V2\0";
const MAX_DIAGNOSTIC_TRANSCRIPT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProofError(String);

impl ProofError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProofError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ProofError {}

#[derive(Debug)]
struct EncodedAdjacency {
    values: Vec<u64>,
    arms: [EncodedNeighbors; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StableEdge {
    source: usize,
    target: usize,
    id: EId,
}

#[derive(Debug)]
struct StableFixtureIds {
    vertices: Vec<VId>,
    edges: Vec<StableEdge>,
    vertex_payload: KernelOutput,
    edge_payload: KernelOutput,
    vertex_prefixes: usize,
    edge_prefixes: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct DiagnosticScan {
    nodes: usize,
    edges: usize,
    adjacency_entries: usize,
    adjacency: Vec<Vec<u64>>,
}

#[derive(Debug)]
struct PipelineOutput {
    evidence_rows: String,
    stream_rows: usize,
    identities: StableFixtureIds,
    scan: DiagnosticScan,
    transcript_decoded_bytes: usize,
}

fn main() -> Result<(), Box<dyn Error>> {
    let (pipeline, report) = run_async_under_lab(LAB_SEED, |cx| async move {
        let current = asupersync::Cx::current()
            .ok_or_else(|| ProofError::new("lab root task has no current Cx"))?;
        if current.region_id() != cx.region_id() {
            return Err(ProofError::new("lab root Cx does not match current Cx"));
        }
        run_pipeline()
    });
    if !report.quiescent {
        return Err(ProofError::new("lab runtime did not reach quiescence").into());
    }
    if !report.oracle_report.all_passed() {
        return Err(ProofError::new("lab runtime oracle report was not green").into());
    }
    if report.oracle_report.total == 0 {
        return Err(ProofError::new("lab runtime produced no oracle coverage").into());
    }
    for invariant in ["obligation_leak", "quiescence"] {
        let entry = report.oracle_report.entry(invariant).ok_or_else(|| {
            ProofError::new(format!("lab runtime omitted {invariant} oracle evidence"))
        })?;
        if !entry.passed {
            return Err(
                ProofError::new(format!("lab runtime {invariant} oracle did not pass")).into(),
            );
        }
    }
    if !report.invariant_violations.is_empty() {
        return Err(ProofError::new(format!(
            "lab runtime reported {} invariant violations",
            report.invariant_violations.len()
        ))
        .into());
    }

    let pipeline = pipeline?;
    let summary = format!(
        concat!(
            "{{\"kind\":\"scope-summary\",",
            "\"proof\":\"scalar-graph-codec-pipeline-v1\",",
            "\"scope\":\"registry-independent-partial-e2e\",",
            "\"fixture\":\"{}\",",
            "\"nodes\":{},\"edges\":{},\"adjacency_entries\":{},",
            "\"neighbor_arms_per_list\":3,\"stream_evidence_rows\":{},",
            "\"identity_payload_evidence_rows\":2,",
            "\"vertex_identity_rows\":{},\"edge_identity_rows\":{},",
            "\"vertex_identity_prefixes\":{},\"edge_identity_prefixes\":{},",
            "\"diagnostic_transcript_decoded_bytes\":{},\"lab_seed\":{},",
            "\"lab_scope\":\"root-task-lifecycle-only\",",
            "\"lab_quiescent\":true,\"lab_oracles_passed\":true,",
            "\"omissions\":[",
            "\"durable-framing\",\"registered-ids\",\"logical-digest\",",
            "\"simd-parity\",\"origin-birth-order\",\"delta-for\",",
            "\"production-seal-run\",\"lab-chaos-cancellation\",",
            "\"final-codec-pipeline-e2e\"]}}\n"
        ),
        FIXTURE_ID,
        pipeline.scan.nodes,
        pipeline.scan.edges,
        pipeline.scan.adjacency_entries,
        pipeline.stream_rows,
        pipeline.identities.vertices.len(),
        pipeline.identities.edges.len(),
        pipeline.identities.vertex_prefixes,
        pipeline.identities.edge_prefixes,
        pipeline.transcript_decoded_bytes,
        LAB_SEED,
    );

    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_all(pipeline.evidence_rows.as_bytes())?;
    output.write_all(summary.as_bytes())?;
    Ok(())
}

fn run_pipeline() -> Result<PipelineOutput, ProofError> {
    let kernels = ScalarKernels;
    let adjacency = generate_fixture()?;
    let identities = verify_identity_columns(&adjacency, &kernels)?;
    let transcript = encode_diagnostic_transcript(&adjacency, &identities)?;
    let profile = CodecProfile::try_new(u16::MAX as usize, 4_096, MAX_DIAGNOSTIC_TRANSCRIPT_BYTES)
        .map_err(|error| ProofError::new(format!("invalid scalar block profile: {error}")))?;
    let compressed = BlockKernel::compress_output(&kernels, &transcript, profile)
        .map_err(|error| ProofError::new(format!("diagnostic compression failed: {error}")))?;
    let decompressed = BlockKernel::decompress(
        &kernels,
        compressed.as_bytes(),
        transcript.len(),
        OutputLimit::new(MAX_DIAGNOSTIC_TRANSCRIPT_BYTES),
    )
    .map_err(|error| ProofError::new(format!("diagnostic decompression failed: {error}")))?;
    if decompressed != transcript {
        return Err(ProofError::new(
            "scalar block round-trip changed the diagnostic transcript",
        ));
    }
    let scan = scan_diagnostic_transcript(&decompressed, &identities)?;
    if scan.nodes != NODE_COUNT || scan.edges != EXPECTED_EDGE_COUNT {
        return Err(ProofError::new(format!(
            "diagnostic scan returned nodes={} edges={}, expected {NODE_COUNT}/{}",
            scan.nodes, scan.edges, EXPECTED_EDGE_COUNT
        )));
    }
    if scan.adjacency != adjacency {
        return Err(ProofError::new(
            "diagnostic scan changed the generated adjacency",
        ));
    }

    // Run all neighbor codecs and every cross-arm intersection from the rows
    // recovered by the transcript scan, preserving the requested cycle order.
    let mut evidence_rows = String::new();
    let encoded = encode_and_verify_neighbors(&scan.adjacency, &mut evidence_rows, &kernels)?;
    verify_all_intersections(&encoded, &kernels)?;

    append_evidence_row(
        &mut evidence_rows,
        "identity-shared-prefix-fixed-scalar-payload-diagnostic",
        "ba64-vertex-ids",
        identities.vertices.len(),
        &identities.vertex_payload,
    )?;
    append_evidence_row(
        &mut evidence_rows,
        "identity-shared-prefix-fixed-scalar-payload-diagnostic",
        "ba64-edge-ids",
        identities.edges.len(),
        &identities.edge_payload,
    )?;

    append_evidence_row(
        &mut evidence_rows,
        "block-scalar-diagnostic-transcript",
        "ba64-stable-id-adjacency-diagnostic",
        scan.adjacency_entries,
        &compressed,
    )?;

    Ok(PipelineOutput {
        evidence_rows,
        stream_rows: NODE_COUNT,
        identities,
        scan,
        transcript_decoded_bytes: decompressed.len(),
    })
}

fn generate_fixture() -> Result<Vec<Vec<u64>>, ProofError> {
    let mut generator = GraphGenerator::strict();
    let report = generator
        .barabasi_albert_graph(NODE_COUNT, ATTACHMENT_COUNT, GENERATOR_SEED)
        .map_err(|error| ProofError::new(format!("fixture generation failed: {error}")))?;
    if !report.warnings.is_empty() {
        return Err(ProofError::new(format!(
            "strict fixture unexpectedly emitted warnings: {:?}",
            report.warnings
        )));
    }
    if report.graph.node_count() != NODE_COUNT || report.graph.edge_count() != EXPECTED_EDGE_COUNT {
        return Err(ProofError::new(format!(
            "fixture shape was nodes={} edges={}, expected {NODE_COUNT}/{}",
            report.graph.node_count(),
            report.graph.edge_count(),
            EXPECTED_EDGE_COUNT
        )));
    }

    let mut adjacency = Vec::new();
    adjacency
        .try_reserve_exact(NODE_COUNT)
        .map_err(|_| ProofError::new("could not reserve bounded fixture adjacency rows"))?;
    for node in 0..NODE_COUNT {
        let name = report
            .graph
            .get_node_name(node)
            .ok_or_else(|| ProofError::new(format!("fixture lost node index {node}")))?;
        let parsed = name.parse::<usize>().map_err(|error| {
            ProofError::new(format!("node name {name:?} is not numeric: {error}"))
        })?;
        if parsed != node {
            return Err(ProofError::new(format!(
                "fixture node index {node} has noncanonical name {name:?}"
            )));
        }

        let source = report
            .graph
            .neighbors_indices(node)
            .ok_or_else(|| ProofError::new(format!("fixture lost adjacency row {node}")))?;
        let mut row = Vec::new();
        row.try_reserve_exact(source.len())
            .map_err(|_| ProofError::new(format!("could not reserve adjacency row {node}")))?;
        for &neighbor in source {
            row.push(
                u64::try_from(neighbor)
                    .map_err(|_| ProofError::new("neighbor index does not fit u64"))?,
            );
        }
        row.sort_unstable();
        let before_dedup = row.len();
        row.dedup();
        if row.len() != before_dedup {
            return Err(ProofError::new(format!(
                "fixture adjacency row {node} contains duplicate neighbors"
            )));
        }
        if row.iter().any(|&neighbor| neighbor >= NODE_COUNT as u64) {
            return Err(ProofError::new(format!(
                "fixture adjacency row {node} contains an out-of-range neighbor"
            )));
        }
        adjacency.push(row);
    }
    Ok(adjacency)
}

fn encode_and_verify_neighbors(
    adjacency: &[Vec<u64>],
    evidence_rows: &mut String,
    kernels: &ScalarKernels,
) -> Result<Vec<EncodedAdjacency>, ProofError> {
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(adjacency.len())
        .map_err(|_| ProofError::new("could not reserve encoded adjacency rows"))?;

    for (node, values) in adjacency.iter().enumerate() {
        let limit = EntryLimit::new(values.len());
        let arms = [
            NeighborKernel::build_neighbors(kernels, NeighborCodec::EliasFano, values, limit),
            NeighborKernel::build_neighbors(kernels, NeighborCodec::StreamVByte, values, limit),
            NeighborKernel::build_neighbors(kernels, NeighborCodec::DenseIntervals, values, limit),
        ]
        .map(|result| {
            result.map_err(|error| {
                ProofError::new(format!("node {node} neighbor encoding failed: {error}"))
            })
        });
        let arms = [arms[0].clone()?, arms[1].clone()?, arms[2].clone()?];
        let actual_codecs = [arms[0].codec(), arms[1].codec(), arms[2].codec()];
        let expected_codecs = [
            NeighborCodec::EliasFano,
            NeighborCodec::StreamVByte,
            NeighborCodec::DenseIntervals,
        ];
        if actual_codecs != expected_codecs {
            return Err(ProofError::new(format!(
                "node {node} constructors returned {actual_codecs:?}, expected {expected_codecs:?}"
            )));
        }

        for arm in &arms {
            verify_select_and_rank(node, values, arm, kernels)?;
        }
        let stream_accounting = match &arms[1] {
            EncodedNeighbors::StreamVByte(stream) => {
                NeighborKernel::stream_vbyte_accounting_output(
                    kernels,
                    stream,
                    MAX_DIAGNOSTIC_TRANSCRIPT_BYTES,
                )
                .map_err(|error| {
                    ProofError::new(format!(
                        "node {node} stream accounting output failed: {error}"
                    ))
                })?
            }
            EncodedNeighbors::EliasFano(_) | EncodedNeighbors::DenseIntervals(_) => {
                return Err(ProofError::new(
                    "explicit StreamVByte constructor returned a different arm",
                ));
            }
        };
        let corpus_id = format!("{FIXTURE_ID}-node-{node:02}");
        append_evidence_row(
            evidence_rows,
            "stream-vbyte-payload-fences-scalar-diagnostic",
            &corpus_id,
            values.len(),
            &stream_accounting,
        )?;
        encoded.push(EncodedAdjacency {
            values: values.clone(),
            arms,
        });
    }
    Ok(encoded)
}

fn verify_select_and_rank(
    node: usize,
    values: &[u64],
    encoded: &EncodedNeighbors,
    kernels: &ScalarKernels,
) -> Result<(), ProofError> {
    for (index, &expected) in values.iter().enumerate() {
        if NeighborKernel::neighbors_select(kernels, encoded, index) != Some(expected) {
            return Err(ProofError::new(format!(
                "node {node} {:?} select({index}) diverged",
                encoded.codec()
            )));
        }
    }
    if NeighborKernel::neighbors_select(kernels, encoded, values.len()).is_some() {
        return Err(ProofError::new(format!(
            "node {node} {:?} returned an out-of-range select",
            encoded.codec()
        )));
    }

    for probe in (0..=NODE_COUNT as u64).chain(core::iter::once(u64::MAX)) {
        let expected = values.partition_point(|&candidate| candidate <= probe);
        let actual = NeighborKernel::neighbors_rank_le(kernels, encoded, probe);
        if actual != expected {
            return Err(ProofError::new(format!(
                "node {node} {:?} rank_le({probe}) was {}, expected {expected}",
                encoded.codec(),
                actual
            )));
        }
    }
    Ok(())
}

fn verify_all_intersections(
    encoded: &[EncodedAdjacency],
    kernels: &ScalarKernels,
) -> Result<(), ProofError> {
    for (left_node, left) in encoded.iter().enumerate() {
        for (right_node, right) in encoded.iter().enumerate() {
            let expected = naive_intersection(&left.values, &right.values);
            for left_arm in &left.arms {
                for right_arm in &right.arms {
                    let actual = NeighborKernel::neighbors_intersection(
                        kernels,
                        left_arm,
                        right_arm,
                        EntryLimit::new(expected.len()),
                    )
                    .map_err(|error| {
                        ProofError::new(format!(
                            "intersection {left_node}/{:?} x {right_node}/{:?} failed: {error}",
                            left_arm.codec(),
                            right_arm.codec()
                        ))
                    })?;
                    if actual != expected {
                        return Err(ProofError::new(format!(
                            "intersection {left_node}/{:?} x {right_node}/{:?} diverged",
                            left_arm.codec(),
                            right_arm.codec()
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn naive_intersection(left: &[u64], right: &[u64]) -> Vec<u64> {
    let mut result = Vec::with_capacity(left.len().min(right.len()));
    let mut left_index = 0;
    let mut right_index = 0;
    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            core::cmp::Ordering::Less => left_index += 1,
            core::cmp::Ordering::Greater => right_index += 1,
            core::cmp::Ordering::Equal => {
                result.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }
    result
}

fn verify_identity_columns(
    adjacency: &[Vec<u64>],
    kernels: &ScalarKernels,
) -> Result<StableFixtureIds, ProofError> {
    let vertex_ids = make_vertex_ids(adjacency.len())?;
    let stable_edges = make_edge_ids(adjacency)?;
    let edge_ids = stable_edges.iter().map(|edge| edge.id).collect::<Vec<_>>();
    let vertex_limits = IdentityColumnLimits::new(vertex_ids.len(), 8, 4_096);
    let edge_limits = IdentityColumnLimits::new(edge_ids.len(), 8, 4_096);
    let vertices =
        IdentityColumnKernel::build_sorted_identity_column(kernels, &vertex_ids, vertex_limits)
            .map_err(|error| ProofError::new(format!("VId column failed: {error}")))?;
    let edges = IdentityColumnKernel::build_sorted_identity_column(kernels, &edge_ids, edge_limits)
        .map_err(|error| ProofError::new(format!("EId column failed: {error}")))?;

    verify_vertex_lower_bounds(&vertices, &vertex_ids, kernels)?;
    verify_edge_lower_bounds(&edges, &edge_ids, kernels)?;
    let vertex_prefixes = verify_shared_identity_column(
        "VId",
        vertices.as_column().representation(),
        vertices
            .as_column()
            .prefix_dictionary()
            .map_or(0, <[fgdb_codec::identity::IdentityPrefix]>::len),
    )?;
    let edge_prefixes = verify_shared_identity_column(
        "EId",
        edges.as_column().representation(),
        edges
            .as_column()
            .prefix_dictionary()
            .map_or(0, <[fgdb_codec::identity::IdentityPrefix]>::len),
    )?;
    for (row, &expected) in vertex_ids.iter().enumerate() {
        if IdentityColumnKernel::sorted_identity_at(kernels, &vertices, row) != Some(expected) {
            return Err(ProofError::new(format!(
                "VId column reconstruction changed row {row}"
            )));
        }
    }
    for (row, &expected) in edge_ids.iter().enumerate() {
        if IdentityColumnKernel::sorted_identity_at(kernels, &edges, row) != Some(expected) {
            return Err(ProofError::new(format!(
                "EId column reconstruction changed row {row}"
            )));
        }
    }
    let vertex_payload =
        IdentityColumnKernel::encode_identity_payload(kernels, vertices.as_column(), 4_096)
            .map_err(|error| ProofError::new(format!("VId payload failed: {error}")))?;
    let edge_payload =
        IdentityColumnKernel::encode_identity_payload(kernels, edges.as_column(), 4_096)
            .map_err(|error| ProofError::new(format!("EId payload failed: {error}")))?;
    assert_payload_ceiling("VId", vertex_payload.len(), vertex_ids.len(), 16)?;
    assert_payload_ceiling("EId", edge_payload.len(), edge_ids.len(), 16)?;

    Ok(StableFixtureIds {
        vertices: vertex_ids,
        edges: stable_edges,
        vertex_payload,
        edge_payload,
        vertex_prefixes,
        edge_prefixes,
    })
}

fn assert_payload_ceiling(
    identity_name: &str,
    encoded_bytes: usize,
    entry_count: usize,
    max_bytes_per_entry: usize,
) -> Result<(), ProofError> {
    let limit = entry_count
        .checked_mul(max_bytes_per_entry)
        .ok_or_else(|| ProofError::new("identity byte-per-entry ceiling overflowed"))?;
    if encoded_bytes > limit {
        return Err(ProofError::new(format!(
            "{identity_name} scalar payload uses {encoded_bytes} bytes for {entry_count} entries, above {max_bytes_per_entry} bytes per entry"
        )));
    }
    Ok(())
}

fn make_vertex_ids(node_count: usize) -> Result<Vec<VId>, ProofError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(node_count)
        .map_err(|_| ProofError::new("could not reserve graph VIds"))?;
    let prefix_span = node_count.div_ceil(3).max(1);
    for node in 0..node_count {
        let prefix = node / prefix_span;
        let slot = u64::try_from(node % prefix_span)
            .map_err(|_| ProofError::new("vertex slot does not fit u64"))?;
        let partition = 7_u32
            .checked_add(
                u32::try_from(prefix)
                    .map_err(|_| ProofError::new("vertex prefix does not fit u32"))?,
            )
            .ok_or_else(|| ProofError::new("vertex partition overflowed"))?;
        let parts = IdentityParts::try_new(41 + prefix as u64, partition, slot + 1)
            .map_err(|error| ProofError::new(format!("invalid VId parts: {error}")))?;
        values.push(VId(parts.pack()));
    }
    Ok(values)
}

fn make_edge_ids(adjacency: &[Vec<u64>]) -> Result<Vec<StableEdge>, ProofError> {
    let mut endpoints = Vec::new();
    endpoints
        .try_reserve_exact(EXPECTED_EDGE_COUNT)
        .map_err(|_| ProofError::new("could not reserve graph edge endpoints"))?;
    for (source, neighbors) in adjacency.iter().enumerate() {
        for &target in neighbors {
            let target = usize::try_from(target)
                .map_err(|_| ProofError::new("edge target does not fit usize"))?;
            if source < target {
                endpoints.push((source, target));
            }
        }
    }
    if endpoints.len() != EXPECTED_EDGE_COUNT {
        return Err(ProofError::new(format!(
            "graph yielded {} canonical edges, expected {EXPECTED_EDGE_COUNT}",
            endpoints.len()
        )));
    }

    let mut edges = Vec::new();
    edges
        .try_reserve_exact(endpoints.len())
        .map_err(|_| ProofError::new("could not reserve graph EIds"))?;
    let prefix_span = endpoints.len().div_ceil(3).max(1);
    for (ordinal, (source, target)) in endpoints.into_iter().enumerate() {
        let prefix = ordinal / prefix_span;
        let slot = u64::try_from(ordinal % prefix_span)
            .map_err(|_| ProofError::new("edge slot does not fit u64"))?;
        let partition = 5_u32
            .checked_add(
                u32::try_from(prefix)
                    .map_err(|_| ProofError::new("edge prefix does not fit u32"))?,
            )
            .ok_or_else(|| ProofError::new("edge partition overflowed"))?;
        let parts = IdentityParts::try_new(91 + prefix as u64, partition, slot + 1)
            .map_err(|error| ProofError::new(format!("invalid EId parts: {error}")))?;
        edges.push(StableEdge {
            source,
            target,
            id: EId(parts.pack()),
        });
    }
    Ok(edges)
}

fn verify_vertex_lower_bounds(
    column: &SortedIdentityColumn<VId>,
    values: &[VId],
    kernels: &ScalarKernels,
) -> Result<(), ProofError> {
    let mut probes = vec![VId(0), VId(u128::MAX)];
    for &value in values {
        probes.push(value);
        probes.push(VId(value.0 - 1));
        probes.push(VId(value.0 + 1));
    }
    for probe in probes {
        let expected = values.partition_point(|&candidate| candidate < probe);
        let actual = IdentityColumnKernel::identity_lower_bound(kernels, column, probe);
        if actual != expected {
            return Err(ProofError::new(format!(
                "VId lower_bound({probe:?}) was {}, expected {expected}",
                actual
            )));
        }
    }
    Ok(())
}

fn verify_edge_lower_bounds(
    column: &SortedIdentityColumn<EId>,
    values: &[EId],
    kernels: &ScalarKernels,
) -> Result<(), ProofError> {
    let mut probes = vec![EId(0), EId(u128::MAX)];
    for &value in values {
        probes.push(value);
        probes.push(EId(value.0 - 1));
        probes.push(EId(value.0 + 1));
    }
    for probe in probes {
        let expected = values.partition_point(|&candidate| candidate < probe);
        let actual = IdentityColumnKernel::identity_lower_bound(kernels, column, probe);
        if actual != expected {
            return Err(ProofError::new(format!(
                "EId lower_bound({probe:?}) was {}, expected {expected}",
                actual
            )));
        }
    }
    Ok(())
}

fn verify_shared_identity_column(
    type_name: &str,
    representation: IdentityRepresentation,
    prefix_count: usize,
) -> Result<usize, ProofError> {
    if representation != IdentityRepresentation::SharedPrefixFixed {
        return Err(ProofError::new(format!(
            "{type_name} fixture did not select shared-prefix storage"
        )));
    }
    if prefix_count < 2 {
        return Err(ProofError::new(format!(
            "{type_name} fixture did not exercise a multi-prefix dictionary"
        )));
    }
    Ok(prefix_count)
}

fn encode_diagnostic_transcript(
    adjacency: &[Vec<u64>],
    identities: &StableFixtureIds,
) -> Result<Vec<u8>, ProofError> {
    if identities.vertices.len() != adjacency.len() || identities.edges.len() != EXPECTED_EDGE_COUNT
    {
        return Err(ProofError::new(
            "stable identity cardinalities do not match the generated graph",
        ));
    }
    let adjacency_entries = adjacency.iter().try_fold(0_usize, |sum, row| {
        sum.checked_add(row.len())
            .ok_or_else(|| ProofError::new("adjacency-entry count overflowed"))
    })?;
    let row_bytes = adjacency
        .len()
        .checked_mul(size_of::<u128>() + size_of::<u64>())
        .ok_or_else(|| ProofError::new("diagnostic row byte count overflowed"))?;
    let entry_bytes = adjacency_entries
        .checked_mul(2 * size_of::<u128>())
        .ok_or_else(|| ProofError::new("diagnostic entry byte count overflowed"))?;
    let capacity = DIAGNOSTIC_TRANSCRIPT_MAGIC
        .len()
        .checked_add(2 * size_of::<u64>())
        .and_then(|value| value.checked_add(row_bytes))
        .and_then(|value| value.checked_add(entry_bytes))
        .ok_or_else(|| ProofError::new("diagnostic transcript byte count overflowed"))?;
    if capacity > MAX_DIAGNOSTIC_TRANSCRIPT_BYTES {
        return Err(ProofError::new(format!(
            "diagnostic transcript needs {capacity} bytes, limit is {MAX_DIAGNOSTIC_TRANSCRIPT_BYTES}"
        )));
    }

    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| ProofError::new("could not reserve bounded diagnostic transcript"))?;
    output.extend_from_slice(DIAGNOSTIC_TRANSCRIPT_MAGIC);
    push_usize_le(&mut output, adjacency.len())?;
    push_usize_le(&mut output, identities.edges.len())?;
    for (node, neighbors) in adjacency.iter().enumerate() {
        output.extend_from_slice(&identities.vertices[node].0.to_be_bytes());
        push_usize_le(&mut output, neighbors.len())?;
        for &neighbor in neighbors {
            let neighbor = usize::try_from(neighbor)
                .map_err(|_| ProofError::new("neighbor does not fit usize"))?;
            let neighbor_id = identities
                .vertices
                .get(neighbor)
                .ok_or_else(|| ProofError::new("neighbor has no stable VId"))?;
            let edge_id = stable_edge_id(identities, node, neighbor)
                .ok_or_else(|| ProofError::new("adjacency entry has no stable EId"))?;
            output.extend_from_slice(&neighbor_id.0.to_be_bytes());
            output.extend_from_slice(&edge_id.0.to_be_bytes());
        }
    }
    if output.len() != capacity {
        return Err(ProofError::new(
            "diagnostic transcript length accounting diverged",
        ));
    }
    Ok(output)
}

fn stable_edge_id(identities: &StableFixtureIds, source: usize, target: usize) -> Option<EId> {
    let endpoints = if source < target {
        (source, target)
    } else {
        (target, source)
    };
    identities
        .edges
        .binary_search_by_key(&endpoints, |edge| (edge.source, edge.target))
        .ok()
        .map(|index| identities.edges[index].id)
}

fn push_usize_le(output: &mut Vec<u8>, value: usize) -> Result<(), ProofError> {
    let value =
        u64::try_from(value).map_err(|_| ProofError::new("diagnostic scalar does not fit u64"))?;
    output.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn scan_diagnostic_transcript(
    input: &[u8],
    identities: &StableFixtureIds,
) -> Result<DiagnosticScan, ProofError> {
    let mut cursor = 0_usize;
    let magic = read_bytes(input, &mut cursor, DIAGNOSTIC_TRANSCRIPT_MAGIC.len())?;
    if magic != DIAGNOSTIC_TRANSCRIPT_MAGIC {
        return Err(ProofError::new("diagnostic transcript has the wrong magic"));
    }
    let nodes = read_usize(input, &mut cursor)?;
    let edges = read_usize(input, &mut cursor)?;
    if nodes > NODE_COUNT || nodes != identities.vertices.len() {
        return Err(ProofError::new(format!(
            "diagnostic transcript declares {nodes} nodes, expected {} with limit {NODE_COUNT}",
            identities.vertices.len()
        )));
    }
    if edges != identities.edges.len() {
        return Err(ProofError::new(format!(
            "diagnostic transcript declares {edges} edges, expected {}",
            identities.edges.len()
        )));
    }

    let mut rows = Vec::new();
    rows.try_reserve_exact(nodes)
        .map_err(|_| ProofError::new("could not reserve bounded diagnostic scan rows"))?;
    let mut adjacency_entries = 0_usize;
    for node in 0..nodes {
        let source_id = VId(read_u128_be(input, &mut cursor)?);
        if source_id != identities.vertices[node] {
            return Err(ProofError::new(format!(
                "diagnostic row {node} changed its stable VId"
            )));
        }
        let degree = read_usize(input, &mut cursor)?;
        if degree > nodes {
            return Err(ProofError::new(format!(
                "diagnostic node {node} degree {degree} exceeds node count {nodes}"
            )));
        }
        adjacency_entries = adjacency_entries
            .checked_add(degree)
            .ok_or_else(|| ProofError::new("diagnostic adjacency-entry count overflowed"))?;
        let mut row = Vec::new();
        row.try_reserve_exact(degree)
            .map_err(|_| ProofError::new(format!("could not reserve diagnostic row {node}")))?;
        for _ in 0..degree {
            let neighbor_id = VId(read_u128_be(input, &mut cursor)?);
            let edge_id = EId(read_u128_be(input, &mut cursor)?);
            let neighbor = identities
                .vertices
                .binary_search(&neighbor_id)
                .map_err(|_| {
                    ProofError::new(format!("diagnostic node {node} has unknown neighbor VId"))
                })?;
            if neighbor >= nodes || neighbor == node {
                return Err(ProofError::new(format!(
                    "diagnostic node {node} has invalid neighbor {neighbor}"
                )));
            }
            if stable_edge_id(identities, node, neighbor) != Some(edge_id) {
                return Err(ProofError::new(format!(
                    "diagnostic edge {node}<->{neighbor} changed its stable EId"
                )));
            }
            let neighbor = u64::try_from(neighbor)
                .map_err(|_| ProofError::new("diagnostic neighbor does not fit u64"))?;
            if row.last().is_some_and(|&previous| previous >= neighbor) {
                return Err(ProofError::new(format!(
                    "diagnostic node {node} neighbors are not strictly increasing"
                )));
            }
            row.push(neighbor);
        }
        rows.push(row);
    }
    if cursor != input.len() {
        return Err(ProofError::new(format!(
            "diagnostic transcript has {} trailing bytes",
            input.len() - cursor
        )));
    }
    if !adjacency_entries.is_multiple_of(2) || adjacency_entries / 2 != edges {
        return Err(ProofError::new(format!(
            "diagnostic degree sum {adjacency_entries} does not bind edge count {edges}"
        )));
    }
    for (node, row) in rows.iter().enumerate() {
        for &neighbor in row {
            let neighbor_index = usize::try_from(neighbor)
                .map_err(|_| ProofError::new("diagnostic neighbor does not fit usize"))?;
            let node = u64::try_from(node)
                .map_err(|_| ProofError::new("diagnostic node does not fit u64"))?;
            if rows[neighbor_index].binary_search(&node).is_err() {
                return Err(ProofError::new(format!(
                    "diagnostic adjacency is asymmetric for {node}<->{neighbor}"
                )));
            }
        }
    }

    Ok(DiagnosticScan {
        nodes,
        edges,
        adjacency_entries,
        adjacency: rows,
    })
}

fn read_usize(input: &[u8], cursor: &mut usize) -> Result<usize, ProofError> {
    usize::try_from(read_u64_le(input, cursor)?)
        .map_err(|_| ProofError::new("diagnostic u64 does not fit usize"))
}

fn read_u64_le(input: &[u8], cursor: &mut usize) -> Result<u64, ProofError> {
    let bytes: [u8; 8] = read_bytes(input, cursor, size_of::<u64>())?
        .try_into()
        .map_err(|_| ProofError::new("diagnostic scalar has the wrong width"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u128_be(input: &[u8], cursor: &mut usize) -> Result<u128, ProofError> {
    let bytes: [u8; 16] = read_bytes(input, cursor, size_of::<u128>())?
        .try_into()
        .map_err(|_| ProofError::new("diagnostic identity has the wrong width"))?;
    Ok(u128::from_be_bytes(bytes))
}

fn read_bytes<'a>(
    input: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], ProofError> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| ProofError::new("diagnostic scan cursor overflowed"))?;
    let bytes = input
        .get(*cursor..end)
        .ok_or_else(|| ProofError::new("diagnostic transcript is truncated"))?;
    *cursor = end;
    Ok(bytes)
}

fn append_evidence_row(
    output: &mut String,
    codec_id: &str,
    corpus_id: &str,
    entry_count: usize,
    encoded_output: &KernelOutput,
) -> Result<(), ProofError> {
    let row = CodecRunRow::try_from_kernel_output(codec_id, corpus_id, entry_count, encoded_output)
        .map_err(|error| ProofError::new(format!("evidence construction failed: {error}")))?;
    let ndjson = row
        .to_ndjson()
        .map_err(|error| ProofError::new(format!("evidence encoding failed: {error}")))?;
    output.push_str(&ndjson);
    Ok(())
}
