use super::*;
use crate::{
    Directedness, FnxCallSpec, FnxExecutionLimits, FnxValue, ParallelEdgePolicy, ProjectionEdge,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
};
use fgdb_crypto::Digest;
use fgdb_types::{CommitSeq, EId, ids::ObjectId};
use std::cell::Cell;
use std::convert::Infallible;

thread_local! {
    static FORWARD_ONLY: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn forward_only() -> bool {
    FORWARD_ONLY.get()
}

pub(crate) struct ForwardGuard(bool);

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        FORWARD_ONLY.set(self.0);
    }
}

pub(crate) fn force_forward() -> ForwardGuard {
    ForwardGuard(FORWARD_ONLY.replace(true))
}

pub(crate) fn with_forward<T>(work: impl FnOnce() -> T) -> T {
    let _restore = force_forward();
    work()
}

fn vertex(index: usize) -> VId {
    VId((1u128 << 124) | ((index as u128) << 48) | 0x1234)
}

fn graph(n: usize, pairs: &[(usize, usize)], direction: Directedness) -> SnapshotGraphView {
    let vertices: Vec<_> = (0..n).rev().map(vertex).collect();
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(id, &(source, target))| ProjectionEdge {
            eid: EId((1u128 << 120) | id as u128),
            source: vertex(source),
            target: vertex(target),
            // Unweighted reachability ignores negative and non-unit weights.
            weight: [-3.0, 0.0, 7.0][id % 3],
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0xb1; 32]),
            as_of: CommitSeq(3),
        },
        &vertices,
        &edges,
        ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: n,
            max_input_edges: edges.len(),
            max_adjacency_entries: 2 * edges.len(),
            max_workspace_bytes: usize::MAX,
        },
    )
    .unwrap()
}

fn limits(rows: usize) -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: rows,
        max_estimated_work: usize::MAX,
    }
}

fn distance(output: &KernelOutput) -> Result<Option<usize>, &'static str> {
    match output.values {
        KernelValues::TargetHopDistance { distance, .. } => Ok(distance),
        _ => Err("target hop-distance output"),
    }
}

/// Independent min-plus closure uses the raw input, not either CSR frontier.
fn oracle(n: usize, pairs: &[(usize, usize)], direction: Directedness) -> Vec<Vec<usize>> {
    let mut distances = vec![vec![n + 1; n]; n];
    for (node, row) in distances.iter_mut().enumerate() {
        row[node] = 0;
    }
    for &(source, target) in pairs {
        if source == target {
            continue;
        }
        match direction {
            Directedness::Directed => distances[source][target] = 1,
            Directedness::Reversed => distances[target][source] = 1,
            Directedness::Undirected => {
                distances[source][target] = 1;
                distances[target][source] = 1;
            }
        }
    }
    for via in 0..n {
        for source in 0..n {
            for target in 0..n {
                distances[source][target] =
                    distances[source][target].min(distances[source][via] + distances[via][target]);
            }
        }
    }
    distances
}

#[test]
fn every_small_direction_target_and_cutoff_matches_independent_closure() {
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for n in 2..=3 {
            let possible: Vec<_> = (0..n).flat_map(|s| (0..n).map(move |t| (s, t))).collect();
            for mask in 0usize..(1 << possible.len()) {
                let pairs: Vec<_> = possible
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &edge)| (mask & (1 << bit) != 0).then_some(edge))
                    .collect();
                let expected = oracle(n, &pairs, direction);
                let view = graph(n, &pairs, direction);
                for (source, row) in expected.iter().enumerate() {
                    for (target, &hops) in row.iter().enumerate() {
                        for cutoff in [None, Some(0), Some(1), Some(2), Some(n)] {
                            let call = FnxCallSpec::shortest_path_length(
                                vertex(source),
                                vertex(target),
                                cutoff,
                            );
                            let actual = call
                                .execute(&view, limits(1), || Ok::<_, Infallible>(()))
                                .unwrap();
                            // Exercise the candidate independently of automatic
                            // routing, including narrow seeds that retain FIFO.
                            let primitive = run(
                                &view,
                                vertex(source),
                                vertex(target),
                                cutoff,
                                1,
                                &mut || Ok::<_, Infallible>(()),
                            )
                            .unwrap();
                            assert_eq!(
                                distance(&primitive.output),
                                Ok((hops < n && cutoff.is_none_or(|limit| hops <= limit))
                                    .then_some(hops))
                            );
                            let wanted = (hops < n && cutoff.is_none_or(|limit| hops <= limit))
                                .then(|| {
                                    vec![
                                        FnxValue::Vertex(vertex(target)),
                                        FnxValue::Integer(hops as u64),
                                    ]
                                });
                            assert_eq!(
                                actual.rows,
                                wanted.into_iter().collect::<Vec<_>>(),
                                "direction={direction:?} n={n} mask={mask} source={source} target={target} cutoff={cutoff:?}"
                            );
                            assert!(
                                actual.certificate.witness.edges_scanned
                                    <= view.outgoing_offsets()[n]
                            );
                            assert!(actual.certificate.witness.nodes_touched <= n);
                            let previous = with_forward(|| {
                                call.execute(&view, limits(1), || Ok::<_, Infallible>(()))
                            })
                            .unwrap();
                            assert_eq!(actual.columns, previous.columns);
                            assert_eq!(actual.rows, previous.rows);
                            assert_eq!(
                                actual.certificate.result_digest,
                                previous.certificate.result_digest
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn asymmetric_layers_switch_directions_without_accepting_a_longer_intersection() {
    let mut max_switches = 0;
    for seed in 0u64..16 {
        let n = 17;
        let mut state = seed + 1;
        let mut pairs = Vec::new();
        for source in 0..n {
            for target in 0..n {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                if state >> 60 < 3 {
                    pairs.push((source, target));
                }
            }
        }
        // Multiple unequal alternatives and a one-way chain prevent a single
        // easy intersection topology from being the whole test population.
        pairs.extend((0..n - 1).map(|source| (source, source + 1)));
        pairs.extend([(0, 8), (8, 16), (0, 1), (1, 4), (4, 16), (0, 8)]);
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let expected = oracle(n, &pairs, direction);
            let view = graph(n, &pairs, direction);
            for (source, row) in expected.iter().enumerate() {
                for (target, &hops) in row.iter().enumerate() {
                    if source == target {
                        continue;
                    }
                    for cutoff in [None, Some(hops.saturating_sub(1)), Some(hops)] {
                        let result = run(
                            &view,
                            vertex(source),
                            vertex(target),
                            cutoff,
                            1,
                            &mut || Ok::<_, Infallible>(()),
                        )
                        .unwrap();
                        let wanted =
                            (hops < n && cutoff.is_none_or(|limit| hops <= limit)).then_some(hops);
                        assert_eq!(
                            distance(&result.output),
                            Ok(wanted),
                            "seed={seed} direction={direction:?} {source}->{target} cutoff={cutoff:?}"
                        );
                        assert!(result.output.witness.edges_scanned <= view.outgoing_offsets()[n]);
                        assert!(result.decision.switches.len() <= usize::BITS as usize + 1);
                        max_switches = max_switches.max(result.decision.switches.len());
                        for switch in &result.decision.switches {
                            let forward = switch.forward_vertices + switch.forward_arcs;
                            let backward = switch.reverse_vertices + switch.reverse_arcs;
                            let (selected, other) = if switch.reverse {
                                (backward, forward)
                            } else {
                                (forward, backward)
                            };
                            assert!(other > selected && other - selected > selected);
                            assert_eq!(switch.benefit_lower, other - selected);
                            assert_eq!(switch.benefit_upper, other);
                        }
                    }
                }
            }
        }
    }
    assert!(
        max_switches >= 3,
        "both directions must switch back under the live policy"
    );
}

#[test]
fn growing_branching_search_uses_the_incoming_face_and_reduces_actual_work() {
    let n = 8192;
    let mut pairs: Vec<_> = (1..4096).map(|node| (0, node)).collect();
    pairs.extend((1..4096).map(|node| (node, node + 4095)));
    pairs.push((4096, n - 1));
    let view = graph(n, &pairs, Directedness::Directed);
    let call = FnxCallSpec::shortest_path_length(vertex(0), vertex(n - 1), None);
    let actual = call
        .execute(&view, limits(1), || Ok::<_, Infallible>(()))
        .unwrap();
    let previous =
        with_forward(|| call.execute(&view, limits(1), || Ok::<_, Infallible>(()))).unwrap();
    assert_eq!(actual.rows, previous.rows);
    assert_eq!(actual.rows[0][1], FnxValue::Integer(3));
    assert!(
        actual.certificate.witness.edges_scanned * 100 < previous.certificate.witness.edges_scanned
    );
    assert!(
        actual.certificate.kernel_workspace_bytes < previous.certificate.kernel_workspace_bytes
    );
    let card = actual
        .certificate
        .target_bfs_decision_card
        .as_ref()
        .unwrap();
    assert_eq!(card.policy_version, 1);
    assert_eq!(card.fallback, "current_direction_fifo");
    assert_eq!(card.dwell_layers, 1);
    assert_eq!(card.switch_cost_multiplier, 2);
    assert_eq!(
        card.switches,
        [TargetBfsSwitch {
            reverse: true,
            forward_depth: 0,
            reverse_depth: 0,
            forward_vertices: 1,
            reverse_vertices: 1,
            forward_arcs: 4095,
            reverse_arcs: 1,
            benefit_lower: 4094,
            benefit_upper: 4096,
        }]
    );
    assert_eq!(actual.certificate.execution_kernel, KERNEL);
    // Independently reconstruct the complete public v5 envelope, including
    // the known initial reverse decision. A locally correct card encoder is
    // insufficient if dispatch forgets to bind its evidence to the result.
    let certificate = &actual.certificate;
    let mut transcript = b"fgdb:prism:call-certificate:v5".to_vec();
    let text = |bytes: &mut Vec<u8>, value: &str| {
        bytes.extend_from_slice(&(value.len() as u128).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    };
    transcript.extend_from_slice(&certificate.registry_version.to_le_bytes());
    transcript.extend_from_slice(certificate.implementation_revision.as_bytes());
    text(&mut transcript, certificate.execution_kernel);
    transcript.extend_from_slice(&certificate.kernel_source_digest.0);
    text(&mut transcript, certificate.numeric_profile);
    transcript.extend_from_slice(&certificate.projection_digest.0);
    transcript.extend_from_slice(&certificate.call_digest.0);
    transcript.extend_from_slice(&certificate.result_digest.0);
    text(&mut transcript, certificate.adapter.as_str());
    for number in [
        certificate.vertices,
        certificate.edges,
        certificate.input_edges,
        certificate.estimated_work,
        certificate.kernel_workspace_bytes,
        certificate.witness.nodes_touched,
        certificate.witness.edges_scanned,
        certificate.witness.queue_peak,
    ] {
        transcript.extend_from_slice(&(number as u128).to_le_bytes());
    }
    text(&mut transcript, &certificate.witness.algorithm);
    text(&mut transcript, &certificate.witness.complexity_claim);
    assert!(certificate.bfs_decision_card.is_none());
    assert!(certificate.component_decision_card.is_none());
    transcript.extend_from_slice(&[0, 0]);
    transcript.extend_from_slice(b"fgdb:prism:target-bfs-policy:v1");
    transcript.extend_from_slice(&[1, 1, 0]);
    text(&mut transcript, "current_direction_fifo");
    for number in [1_u128, 2, 1] {
        transcript.extend_from_slice(&number.to_le_bytes());
    }
    transcript.push(1);
    for number in [0_u128, 0, 1, 1, 4095, 1, 4094, 4096] {
        transcript.extend_from_slice(&number.to_le_bytes());
    }
    let mut expected = Hasher::new();
    expected.update(&transcript);
    assert_eq!(certificate.digest, expected.finalize());
    let repeated = call
        .execute(&view, limits(1), || Ok::<_, Infallible>(()))
        .unwrap();
    assert_eq!(actual, repeated);
    let mut mapped = Vec::new();
    let mapped_certificate = call
        .execute_into(&view, limits(1), || Ok::<_, Infallible>(()), &mut mapped)
        .unwrap();
    assert_eq!(mapped_certificate, actual.certificate);
    assert_eq!(mapped, actual.rows);
    let mut cursor = call
        .execute_rows(&view, limits(1), || Ok::<_, Infallible>(()))
        .unwrap();
    assert_eq!(cursor.certificate(), &actual.certificate);
    assert_eq!(cursor.remaining(), 1);
    assert_eq!(cursor.max_integer(), Some(3));
    let mut row = Vec::new();
    assert!(
        cursor
            .next_row(&mut row, || Ok::<_, Infallible>(()))
            .unwrap()
    );
    assert_eq!(row, actual.rows[0]);
    assert_eq!(cursor.remaining(), 0);
    assert!(
        !cursor
            .next_row(&mut row, || -> Result<(), Infallible> {
                panic!("a consumed target has no remaining ordinal to examine")
            })
            .unwrap()
    );
    assert!(row.is_empty());
}

#[test]
fn long_skinny_search_retains_only_live_frontiers_and_bounded_decision_evidence() {
    let n = 4097;
    let pairs: Vec<_> = (0..n - 1).map(|node| (node, node + 1)).collect();
    let view = graph(n, &pairs, Directedness::Directed);
    let result = run(&view, vertex(0), vertex(n - 1), None, 1, &mut || {
        Ok::<_, Infallible>(())
    })
    .unwrap();
    assert_eq!(distance(&result.output), Ok(Some(n - 1)));
    assert_eq!(result.output.witness.edges_scanned, n - 1);
    assert!(result.decision.switches.is_empty());
    assert!(result.workspace <= 2 * n.div_ceil(64) * size_of::<u64>() + 16 * size_of::<usize>());
    assert_eq!(result.output.witness.queue_peak, 2);
}

#[test]
fn automatic_selection_retains_narrow_and_symmetric_fifo_controls() {
    let n = MIN_ASYMMETRIC_VERTICES;
    let chain: Vec<_> = (0..n - 1).map(|node| (node, node + 1)).collect();
    let grid: Vec<_> = (0..n)
        .flat_map(|node| [(node, node + 1), (node, node + 16)])
        .filter(|&(a, b)| b < n && (b - a == 16 || a / 16 == b / 16))
        .collect();
    let disconnected: Vec<_> = chain
        .iter()
        .copied()
        .filter(|&(source, _)| source != n / 2 - 1)
        .collect();
    for pairs in [chain, grid, disconnected] {
        let view = graph(n, &pairs, Directedness::Directed);
        assert!(!eligible(
            &view,
            vertex(0),
            vertex(n - 1),
            None,
            2,
            usize::MAX
        ));
        let call = FnxCallSpec::shortest_path_length(vertex(0), vertex(n - 1), None);
        let actual = call
            .execute(&view, limits(1), || Ok::<_, Infallible>(()))
            .unwrap();
        let fifo =
            with_forward(|| call.execute(&view, limits(1), || Ok::<_, Infallible>(()))).unwrap();
        assert_eq!(actual, fifo);
    }
    let branches: Vec<_> = (1..n).map(|node| ((node - 1) / 4, node)).collect();
    let view = graph(n, &branches, Directedness::Directed);
    assert!(eligible(
        &view,
        vertex(0),
        vertex(n - 1),
        None,
        2,
        usize::MAX
    ));
    assert!(!eligible(
        &view,
        vertex(0),
        vertex(n - 1),
        Some(1),
        2,
        usize::MAX
    ));
    assert!(!eligible(&view, vertex(0), vertex(n - 1), None, 2, 0));
    let empty = graph(n, &[(0, 1)], Directedness::Directed);
    assert!(eligible(
        &empty,
        vertex(0),
        vertex(n - 1),
        None,
        2,
        usize::MAX
    ));
    assert!(eligible(&empty, vertex(2), vertex(1), None, 2, usize::MAX));
    assert!(!eligible(
        &empty,
        vertex(0),
        vertex(n + 1),
        None,
        2,
        usize::MAX
    ));
    assert!(!more_than_twice(4, 2));
    assert!(more_than_twice(5, 2));
    assert!(more_than_twice(usize::MAX, usize::MAX / 2));
    assert!(!more_than_twice(usize::MAX, usize::MAX / 2 + 1));
    for small in [16, 64, 256, MIN_ASYMMETRIC_VERTICES - 1] {
        for pairs in [
            (1..small)
                .map(|node| ((node - 1) / 4, node))
                .collect::<Vec<_>>(),
            (1..small).map(|node| (0, node)).collect(),
        ] {
            let view = graph(small, &pairs, Directedness::Directed);
            assert!(!eligible(
                &view,
                vertex(0),
                vertex(small - 1),
                None,
                2,
                usize::MAX
            ));
        }
    }
}

#[test]
fn every_live_charge_can_cancel_without_a_partial_result() {
    let mut pairs: Vec<_> = (1..80).map(|node| (0, node)).collect();
    pairs.extend((1..80).map(|node| (node, 100 + node)));
    pairs.extend([(170, 190), (190, 199), (1, 2), (2, 3)]);
    let view = graph(MIN_ASYMMETRIC_VERTICES, &pairs, Directedness::Directed);
    for cutoff in [None, Some(2), Some(3), Some(4)] {
        let call = FnxCallSpec::shortest_path_length(vertex(0), vertex(199), cutoff);
        let mut total = 0;
        let completed = call
            .execute(&view, limits(1), || {
                total += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        assert!(completed.certificate.target_bfs_decision_card.is_some());
        for stop in 1..=total {
            let mut seen = 0;
            let result = call.execute(&view, limits(1), || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == stop));
            assert_eq!(seen, stop);
        }
    }
}

#[test]
fn zero_rows_unknown_id_and_tight_work_keep_typed_refusals_and_fifo_fallback() {
    let n = MIN_ASYMMETRIC_VERTICES;
    let view = graph(n, &[(0, 1), (0, 2), (0, 3), (0, 4)], Directedness::Directed);
    let call = FnxCallSpec::shortest_path_length(vertex(0), vertex(3), None);
    assert!(eligible(&view, vertex(0), vertex(3), None, 2, usize::MAX));
    assert!(matches!(
        call.execute(&view, limits(0), || Ok::<_, Infallible>(())),
        Err(FnxExecutionError::LimitExceeded {
            resource: "result rows",
            requested: 1,
            limit: 0
        })
    ));
    let absent = FnxCallSpec::shortest_path_length(vertex(0), vertex(5), None)
        .execute(&view, limits(0), || Ok::<_, Infallible>(()))
        .unwrap();
    assert!(absent.rows.is_empty());
    assert!(
        matches!(FnxCallSpec::shortest_path_length(vertex(0), vertex(n + 1), None)
        .execute(&view, limits(1), || Ok::<_, Infallible>(())), Err(FnxExecutionError::UnknownTarget(id)) if id == vertex(n + 1))
    );
    let old_work = crate::target_bfs::estimated_work::<Infallible>(n, 4, n, 2).unwrap();
    assert!(old_work < estimated_work(n, 4, 2).unwrap());
    let limited = FnxExecutionLimits {
        max_estimated_work: old_work,
        ..limits(1)
    };
    let actual = call
        .execute(&view, limited, || Ok::<_, Infallible>(()))
        .unwrap();
    let old = with_forward(|| call.execute(&view, limited, || Ok::<_, Infallible>(()))).unwrap();
    assert_eq!(actual, old);
    assert!(actual.certificate.target_bfs_decision_card.is_none());
}

fn digest(card: Option<&TargetBfsDecisionCard>) -> Digest {
    let mut hash = Hasher::new();
    hash_decision(&mut hash, card, &mut || Ok::<_, Infallible>(())).unwrap();
    hash.finalize()
}

#[test]
fn certificate_binds_every_policy_and_switch_field_and_checks_each_record() {
    let card = TargetBfsDecisionCard {
        policy_version: 1,
        fallback: "current_direction_fifo",
        dwell_layers: 1,
        switch_cost_multiplier: 2,
        switches: vec![TargetBfsSwitch {
            reverse: true,
            forward_depth: 2,
            reverse_depth: 3,
            forward_vertices: 7,
            reverse_vertices: 4,
            forward_arcs: 71,
            reverse_arcs: 14,
            benefit_lower: 60,
            benefit_upper: 78,
        }],
    };
    let original = digest(Some(&card));
    assert_ne!(original, digest(None));
    for field in 0..14 {
        let mut changed = card.clone();
        match field {
            0 => changed.policy_version += 1,
            1 => changed.fallback = "different",
            2 => changed.dwell_layers += 1,
            3 => changed.switch_cost_multiplier += 1,
            4 => changed.switches.clear(),
            5 => changed.switches[0].reverse = false,
            6 => changed.switches[0].forward_depth += 1,
            7 => changed.switches[0].reverse_depth += 1,
            8 => changed.switches[0].forward_vertices += 1,
            9 => changed.switches[0].reverse_vertices += 1,
            10 => changed.switches[0].forward_arcs += 1,
            11 => changed.switches[0].reverse_arcs += 1,
            12 => changed.switches[0].benefit_lower += 1,
            _ => changed.switches[0].benefit_upper += 1,
        }
        assert_ne!(original, digest(Some(&changed)), "field {field}");
    }
    let mut hash = Hasher::new();
    let mut calls = 0;
    let refused = hash_decision(&mut hash, Some(&card), &mut || {
        calls += 1;
        Err(17)
    });
    assert_eq!(refused, Err(17));
    assert_eq!(calls, 1);
}

/// Same-invocation maintenance comparison of complete registered calls.
/// Both paths include ordinary admission, certificates and result consumption.
/// The fixture projection is shared; durable loading and signed hosts are not
/// timed here. The opaque meter makes each host call observable. The separate
/// live GAPBS harness can cover database/oracle composition when executed.
#[test]
#[ignore = "paired complete target CALL benchmark; run --release --ignored --nocapture"]
fn target_bidirectional_public_call_paired_measurement() {
    use std::hint::black_box;
    use std::time::Instant;
    #[derive(Default)]
    struct Meter {
        units: usize,
        calls: usize,
    }
    impl FnxMeter<Infallible> for &mut Meter {
        fn unit(&mut self) -> Result<(), Infallible> {
            self.units(1)
        }
        #[inline(never)]
        fn units(&mut self, count: usize) -> Result<(), Infallible> {
            self.units = black_box(self.units + count);
            self.calls = black_box(self.calls + 1);
            Ok(())
        }
        #[inline(never)]
        fn poll(&mut self) -> Result<(), Infallible> {
            self.calls = black_box(self.calls + 1);
            Ok(())
        }
    }
    for shape in ["chain", "branching", "grid", "disconnected"] {
        let n = 16_384;
        let pairs: Vec<_> = match shape {
            "chain" => (0..n - 1).map(|node| (node, node + 1)).collect(),
            "branching" => (1..n).map(|node| ((node - 1) / 4, node)).collect(),
            "grid" => (0..n)
                .flat_map(|node| [(node, node + 1), (node, node + 128)])
                .filter(|&(a, b)| b < n && (b - a == 128 || a / 128 == b / 128))
                .collect(),
            _ => (0..n - 2)
                .filter(|node| node % 256 != 255)
                .map(|node| (node, node + 1))
                .collect(),
        };
        let view = graph(n, &pairs, Directedness::Directed);
        for target in [n / 2, n - 1] {
            let call = FnxCallSpec::shortest_path_length(vertex(0), vertex(target), None);
            let previous =
                with_forward(|| call.execute(&view, limits(1), &mut Meter::default())).unwrap();
            let current = call
                .execute(&view, limits(1), &mut Meter::default())
                .unwrap();
            assert_eq!(current.rows, previous.rows);
            assert_eq!(
                current.certificate.result_digest,
                previous.certificate.result_digest
            );
            let mut samples = [Vec::new(), Vec::new()];
            let mut accounting = [(0, 0); 2];
            for trial in 0..22 {
                for index in [trial % 2, 1 - trial % 2] {
                    let mut meter = Meter::default();
                    let start = Instant::now();
                    let result = if index == 0 {
                        with_forward(|| call.execute(&view, limits(1), &mut meter)).unwrap()
                    } else {
                        call.execute(&view, limits(1), &mut meter).unwrap()
                    };
                    let consumed = result.rows.iter().flatten().fold(0u128, |sum, cell| {
                        sum ^ match cell {
                            FnxValue::Vertex(id) => id.0,
                            FnxValue::Integer(value) => u128::from(*value),
                            _ => panic!("target call returned a non-distance cell"),
                        }
                    });
                    black_box((consumed, &result.certificate));
                    let elapsed = start.elapsed().as_nanos();
                    assert_eq!(result.rows, previous.rows);
                    accounting[index] = (meter.units, meter.calls);
                    if trial != 0 {
                        samples[index].push(elapsed);
                    }
                }
            }
            println!(
                "target_bidirectional shape={shape} n={n} target={target} fifo_ns={:?} routed_ns={:?} fifo_edges={} routed_edges={} fifo_workspace={} routed_workspace={} fifo_units_calls={:?} routed_units_calls={:?} kernel={} exact=true projection_excluded=true durable_io_excluded=true signed_host_excluded=true",
                samples[0],
                samples[1],
                previous.certificate.witness.edges_scanned,
                current.certificate.witness.edges_scanned,
                previous.certificate.kernel_workspace_bytes,
                current.certificate.kernel_workspace_bytes,
                accounting[0],
                accounting[1],
                current.certificate.execution_kernel
            );
        }
    }
}
