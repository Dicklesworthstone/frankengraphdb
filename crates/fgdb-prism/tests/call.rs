use fgdb_prism::*;
use fgdb_types::ids::ObjectId;
use fgdb_types::{CommitSeq, EId, VId};
use fnx_classes::{Graph, digraph::DiGraph};
use std::convert::Infallible;

fn graph(vertices: &[VId], edges: &[ProjectionEdge], direction: Directedness) -> SnapshotGraphView {
    SnapshotGraphView::build(
        SnapshotBinding { root: ObjectId([2; 32]), as_of: CommitSeq(3) },
        vertices, edges,
        ProjectionSpec { directedness: direction, parallel_edges: ParallelEdgePolicy::Sum, self_loops: SelfLoopPolicy::Keep },
        ProjectionLimits { max_vertices: 100, max_input_edges: 1000, max_adjacency_entries: 2000, max_workspace_bytes: 1 << 22 },
    ).unwrap()
}
fn edge(id: u128, source: u128, target: u128, weight: f64) -> ProjectionEdge {
    ProjectionEdge { eid: EId(id), source: VId(source), target: VId(target), weight }
}
fn limits() -> FnxExecutionLimits {
    FnxExecutionLimits { max_iterations: 1000, max_result_rows: 100, max_estimated_work: 1 << 24 }
}
fn execute(call: &FnxCallSpec, view: &SnapshotGraphView) -> FnxResult {
    call.execute(view, limits(), || Ok::<(), Infallible>(())).unwrap()
}
fn bind(text: &str) -> FnxCallSpec { FnxCallSpec::bind(text, &FnxParameters::new()).unwrap() }
fn scores(result: &FnxResult) -> Vec<f64> {
    result.rows.iter().map(|row| match row[1] { FnxValue::Score(score) => score, _ => panic!("score column") }).collect()
}

#[test]
fn registry_describes_only_the_implemented_in_core_signatures() {
    assert_eq!(FnxSignatureRegistry::version(), 2);
    assert_eq!(FnxSignatureRegistry::signatures().len(), 5);
    let signature = FnxSignatureRegistry::lookup("fnx.pagerank").unwrap();
    assert_eq!(signature.graph_input_arity, 1);
    assert_eq!(signature.parameters.len(), 4);
    assert_eq!(signature.outputs, &[FnxOutput::Vertex, FnxOutput::Score]);
    assert_eq!(signature.implementation, FnxImplementationClass::InCoreDecodedCache);
    assert!(FnxSignatureRegistry::lookup("fnx.betweenness_centrality").is_none());
    assert!(FnxSignatureRegistry::lookup("pagerank").is_none());
}

#[test]
fn literals_parameters_defaults_and_whitespace_bind_identically() {
    let default = bind("CALL fnx.pagerank()");
    let explicit = bind("call fnx.pagerank(0.85,100,1e-6,true) yield vertex,score;");
    assert_eq!(default, explicit);
    let mut params = FnxParameters::new();
    params.insert("a".to_owned(), FnxArgument::Float(0.85));
    params.insert("k".to_owned(), FnxArgument::Integer(100));
    params.insert("t".to_owned(), FnxArgument::Float(1e-6));
    params.insert("w".to_owned(), FnxArgument::Boolean(true));
    let prepared = FnxCallSpec::bind(" CALL fnx.pagerank($a, $k, $t, $w) YIELD * ; ", &params).unwrap();
    assert_eq!(default.digest(), prepared.digest());
    params.insert("a".to_owned(), FnxArgument::Float(0.5));
    assert_eq!(prepared.options().unwrap().alpha(), 0.85); // frozen, not a late parameter lookup
    assert_eq!(bind("CALL fnx.pagerank(0.85)").digest(), default.digest());
    assert_eq!(bind("CALL fnx.pagerank(-0.0)").digest(), bind("CALL fnx.pagerank(0)").digest());
}

#[test]
fn yield_slots_aliases_and_call_digests_are_bound() {
    let call = bind("CALL fnx.pagerank() YIELD score AS rank, vertex AS id");
    assert_eq!(call.outputs()[0].field, FnxOutput::Score);
    assert_eq!(call.outputs()[0].name, "rank");
    let view = graph(&[VId(99)], &[], Directedness::Directed);
    let result = execute(&call, &view);
    assert_eq!(result.columns, vec!["rank", "id"]);
    assert_eq!(result.rows, vec![vec![FnxValue::Score(1.0), FnxValue::Vertex(VId(99))]]);
    assert_ne!(call.digest(), bind("CALL fnx.pagerank()").digest());
    assert_eq!(execute(&bind("CALL fnx.pagerank() YIELD score"), &view).rows, vec![vec![FnxValue::Score(1.0)]]);
}

#[test]
fn unsupported_or_malformed_calls_never_bind_a_partial_program() {
    for text in [
        "", "MATCH (n) RETURN n", "CALL fnx.pagerank", "CALL other.pagerank()",
        "CALL fnx.pagerankevil()", "CALL fnx.unknown()", "CALL fnx.pagerank(,)",
        "CALL fnx.pagerank(0.85,)", "CALL fnx.pagerank(0.85,100,1e-6,true,1)",
        "CALL fnx.pagerank() YIELD", "CALL fnx.pagerank() YIELD vertex,",
        "CALL fnx.pagerank() YIELD score,score", "CALL fnx.pagerank() YIELD vertex AS x,score AS x",
        "CALL fnx.pagerank() YIELD missing", "CALL fnx.pagerank() YIELD * RETURN vertex",
        "CALL fnx.pagerank(); DELETE n", "CALL fnx.pagerank();;", "CALL fnx.pagerank($missing)",
        "CALL fnx.pagerank('0.85')", "CALL fnx.pagerank(😺)", "CALL fnx.pagerank() YIELD 🦀",
    ] {
        assert!(FnxCallSpec::bind(text, &FnxParameters::new()).is_err(), "{text}");
    }
    let long = " ".repeat(MAX_FNX_CALL_BYTES + 1);
    assert_eq!(FnxCallSpec::bind(&long, &FnxParameters::new()).unwrap_err().kind, FnxBindErrorKind::TextTooLong);
}

#[test]
fn numeric_parameters_refuse_nonfinite_lossy_and_wrong_domain_values() {
    for text in [
        "CALL fnx.pagerank(1)", "CALL fnx.pagerank(-0.1)", "CALL fnx.pagerank(1e999)",
        "CALL fnx.pagerank(0.85,0)", "CALL fnx.pagerank(0.85,-1)",
        "CALL fnx.pagerank(0.85,1.0)", "CALL fnx.pagerank(0.85,10,0)",
        "CALL fnx.pagerank(0.85,10,-1e-6)", "CALL fnx.pagerank(0.85,10,1e-6,1)",
        "CALL fnx.pagerank(true)", "CALL fnx.pagerank(999999999999999999999999)",
    ] {
        assert!(FnxCallSpec::bind(text, &FnxParameters::new()).is_err(), "{text}");
    }
    for value in [FnxArgument::Float(f64::NAN), FnxArgument::Float(f64::INFINITY), FnxArgument::Integer(9_007_199_254_740_993)] {
        let mut params = FnxParameters::new();
        params.insert("tol".to_owned(), value);
        assert!(FnxCallSpec::bind("CALL fnx.pagerank(0.85,100,$tol)", &params).is_err());
    }
    assert!(PageRankOptions::new(0.5, 0, 1e-6, false).is_err());
    assert!(PageRankOptions::new(0.5, 1, f64::NAN, false).is_err());
}

#[test]
fn page_rank_matches_standalone_fnx_for_every_three_node_topology() {
    let vertices = [VId(0), VId(17), VId(u128::MAX)];
    let call = bind("CALL fnx.pagerank(0.85,1000,1e-12,false)");
    for mask in 0u16..512 {
        let edges: Vec<_> = (0..9).filter(|bit| mask & (1 << bit) != 0).map(|bit| {
            edge(bit as u128, vertices[bit / 3].0, vertices[bit % 3].0, 1.0)
        }).collect();
        for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
            let view = graph(&vertices, &edges, direction);
            // Build the oracle from the SELECTED simple projection, never the
            // raw multigraph. Independent fnx storage exercises trait parity.
            let mut directed = DiGraph::strict();
            let mut undirected = Graph::strict();
            for name in view.nodes_ordered() {
                let _ = directed.add_node(name);
                let _ = undirected.add_node(name);
            }
            for s in 0..3 {
                for &t in view.neighbors_indices(s).unwrap() {
                    let from = view.get_node_name(s).unwrap();
                    let to = view.get_node_name(t).unwrap();
                    directed.add_edge(from, to).unwrap();
                    if s <= t { undirected.add_edge(from, to).unwrap(); }
                }
            }
            let oracle = if direction == Directedness::Undirected {
                fnx_algorithms::pagerank_with_params(&undirected, 0.85, 1000, 1e-12)
            } else {
                fnx_algorithms::pagerank_with_params(&directed, 0.85, 1000, 1e-12)
            };
            let actual = execute(&call, &view);
            assert!(oracle.converged);
            let actual_scores = scores(&actual);
            for score in oracle.scores {
                let index = view.get_node_index(&score.node).unwrap();
                assert!((actual_scores[index] - score.score).abs() < 1e-12, "mask {mask}, {direction:?}");
            }
            assert_eq!(actual.certificate.witness, oracle.witness);
            assert_eq!(actual.certificate.vertices, 3);
            assert_eq!(actual.certificate.edges, view.edge_count());
        }
    }
}

#[test]
fn weighted_parallel_edges_and_dangling_vertices_match_dense_oracle() {
    let view = graph(&[VId(1), VId(2), VId(3)], &[
        edge(4, 1, 2, 1.0), edge(1, 1, 2, 2.0), edge(3, 1, 3, 1.0), edge(9, 2, 1, 2.0),
    ], Directedness::Directed);
    let result = execute(&bind("CALL fnx.pagerank(0.85,1000,1e-13,true)"), &view);
    // Explicit row-stochastic dense matrix; vertex 3 is a dangling row.
    let matrix = [[0.0, 0.75, 0.25], [1.0, 0.0, 0.0], [1.0/3.0; 3]];
    let mut expected = [1.0 / 3.0; 3];
    for _ in 0..1000 {
        let mut next = [0.15 / 3.0; 3];
        for t in 0..3 {
            for s in 0..3 { next[t] += 0.85 * expected[s] * matrix[s][t]; }
        }
        let residual: f64 = next.iter().zip(expected).map(|(a,b)| (a-b).abs()).sum();
        expected = next;
        if residual < 1e-14 { break; }
    }
    for (actual, expected) in scores(&result).into_iter().zip(expected) { assert!((actual - expected).abs() < 1e-11); }
    assert_eq!(result.certificate.input_edges, 4);
    assert_eq!(result.certificate.edges, 3);
    assert_ne!(scores(&result), scores(&execute(&bind("CALL fnx.pagerank(0.85,1000,1e-13,false)"), &view)));
}

#[test]
fn isolated_empty_zero_weight_and_reversed_graphs_are_not_lost() {
    let call = bind("CALL fnx.pagerank(0.85,1000,1e-12)");
    let empty = execute(&call, &graph(&[], &[], Directedness::Directed));
    assert!(empty.rows.is_empty());
    assert_eq!(empty.certificate.witness.nodes_touched, 0);
    let isolated = execute(&call, &graph(&[VId(9), VId(1)], &[], Directedness::Directed));
    assert_eq!(scores(&isolated), vec![0.5, 0.5]);
    let zero = execute(&call, &graph(&[VId(1), VId(2)], &[edge(1,1,2,0.0)], Directedness::Directed));
    assert_eq!(scores(&zero), vec![0.5, 0.5]);
    let forward = execute(&call, &graph(&[VId(1), VId(2)], &[edge(1,1,2,1.0)], Directedness::Directed));
    let reverse = execute(&call, &graph(&[VId(1), VId(2)], &[edge(1,1,2,1.0)], Directedness::Reversed));
    assert!(scores(&forward)[1] > scores(&forward)[0]);
    assert!((scores(&forward)[0] - scores(&reverse)[1]).abs() < 1e-12);
}

#[test]
fn numerical_and_convergence_failures_are_typed_not_silent_results() {
    let call = bind("CALL fnx.pagerank()");
    let negative = graph(&[VId(1), VId(2)], &[edge(1,1,2,-1.0)], Directedness::Directed);
    assert!(matches!(call.execute(&negative, limits(), || Ok::<(), Infallible>(())), Err(FnxExecutionError::NegativeWeight)));
    // The unweighted signature does not inspect an intentionally ignored weight.
    execute(&bind("CALL fnx.pagerank(0.85,100,1e-6,false)"), &negative);
    let overflow = graph(&[VId(1), VId(2), VId(3)], &[edge(1,1,2,f64::MAX), edge(2,1,3,f64::MAX)], Directedness::Directed);
    assert!(matches!(call.execute(&overflow, limits(), || Ok::<(), Infallible>(())), Err(FnxExecutionError::NonFiniteWeightSum)));
    let path = graph(&[VId(1), VId(2)], &[edge(1,1,2,1.0)], Directedness::Directed);
    let too_short = bind("CALL fnx.pagerank(0.85,1,1e-20)");
    match too_short.execute(&path, limits(), || Ok::<(), Infallible>(())) {
        Err(FnxExecutionError::NotConverged { max_iterations, witness }) => {
            assert_eq!(max_iterations, 1);
            assert_eq!(witness.algorithm, "pagerank_power_iteration");
        }
        other => panic!("expected convergence refusal: {other:?}"),
    }
}

#[test]
fn iteration_row_and_work_admission_refuse_before_algorithm_execution() {
    let call = bind("CALL fnx.pagerank()");
    let view = graph(&[VId(1), VId(2)], &[edge(1,1,2,1.0)], Directedness::Directed);
    for cap in [
        FnxExecutionLimits { max_iterations: 99, ..limits() },
        FnxExecutionLimits { max_result_rows: 1, ..limits() },
        FnxExecutionLimits { max_estimated_work: 302, ..limits() },
    ] {
        assert!(matches!(call.execute(&view, cap, || Ok::<(), Infallible>(())), Err(FnxExecutionError::LimitExceeded { .. })));
    }
    let cap = FnxExecutionLimits { max_estimated_work: 303, ..limits() };
    assert_eq!(call.execute(&view, cap, || Ok::<(), Infallible>(())).unwrap().certificate.estimated_work, 303);
    let huge = FnxCallSpec::pagerank(PageRankOptions::new(0.85, usize::MAX, 1e-6, true).unwrap());
    assert!(matches!(huge.execute(&view, FnxExecutionLimits { max_iterations: usize::MAX, ..limits() }, || Ok::<(), Infallible>(())), Err(FnxExecutionError::SizeOverflow)));
}

#[test]
fn cancellation_at_every_adapter_checkpoint_discards_results() {
    let call = bind("CALL fnx.pagerank()");
    let view = graph(&[VId(1), VId(2)], &[edge(1,1,2,1.0)], Directedness::Directed);
    let mut count = 0;
    call.execute(&view, limits(), || { count += 1; Ok::<(), &'static str>(()) }).unwrap();
    for stop in 1..=count {
        let mut seen = 0;
        let result = call.execute(&view, limits(), || {
            seen += 1;
            if seen == stop { Err("cancel") } else { Ok(()) }
        });
        assert!(matches!(result, Err(FnxExecutionError::Cancelled("cancel"))));
    }
    // Includes checkpoints inside the native, fnx-differential kernel.
    assert!(count > 5);
}

#[test]
fn evidence_binds_snapshot_call_projection_result_and_upstream_witness() {
    let call = bind("CALL fnx.pagerank()");
    let view = graph(&[VId(1), VId(2)], &[edge(1,1,2,1.0)], Directedness::Directed);
    let result = execute(&call, &view);
    assert_eq!(result, execute(&call, &view.clone()));
    let cert = &result.certificate;
    assert_eq!(cert.projection_digest, view.digest());
    assert_eq!(cert.call_digest, call.digest());
    assert_eq!(cert.snapshot, view.binding());
    assert_eq!(cert.adapter, AdapterPath::DecodedCache);
    assert_eq!(cert.implementation_revision, FNX_IMPLEMENTATION_REVISION);
    assert_eq!(cert.numeric_profile, FNX_NUMERIC_PROFILE);
    assert_eq!(cert.registry_version, FNX_SIGNATURE_REGISTRY_VERSION);
    let changed = execute(&bind("CALL fnx.pagerank(0.5)"), &view);
    assert_ne!(cert.digest, changed.certificate.digest);
    assert_ne!(cert.result_digest, changed.certificate.result_digest);
    let aliased = execute(&bind("CALL fnx.pagerank() YIELD score AS r,vertex"), &view);
    assert_ne!(cert.digest, aliased.certificate.digest);
}
