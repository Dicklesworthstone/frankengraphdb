use fgdb_prism::*;
use fgdb_types::ids::ObjectId;
use fgdb_types::{CommitSeq, EId, VId};
use fnx_classes::{Graph, digraph::DiGraph};
use std::convert::Infallible;

fn graph(vertices: &[VId], edges: &[ProjectionEdge], direction: Directedness) -> SnapshotGraphView {
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([2; 32]),
            as_of: CommitSeq(3),
        },
        vertices,
        edges,
        ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 1000,
            max_adjacency_entries: 2000,
            max_workspace_bytes: 1 << 22,
        },
    )
    .unwrap()
}
fn edge(id: u128, source: u128, target: u128, weight: f64) -> ProjectionEdge {
    ProjectionEdge {
        eid: EId(id),
        source: VId(source),
        target: VId(target),
        weight,
    }
}
fn limits() -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 1000,
        max_result_rows: 100,
        max_estimated_work: 1 << 24,
    }
}
fn execute(call: &FnxCallSpec, view: &SnapshotGraphView) -> FnxResult {
    call.execute(view, limits(), || Ok::<(), Infallible>(()))
        .unwrap()
}
fn bind(text: &str) -> FnxCallSpec {
    FnxCallSpec::bind(text, &FnxParameters::new()).unwrap()
}
fn scores(result: &FnxResult) -> Vec<f64> {
    result
        .rows
        .iter()
        .map(|row| match row[1] {
            FnxValue::Score(score) => score,
            _ => panic!("score column"),
        })
        .collect()
}

#[test]
fn registry_describes_only_the_implemented_in_core_signatures() {
    assert_eq!(FnxSignatureRegistry::version(), 12);
    assert_eq!(FnxSignatureRegistry::signatures().len(), 16);
    let signature = FnxSignatureRegistry::lookup("fnx.pagerank").unwrap();
    assert_eq!(signature.graph_input_arity, 1);
    assert_eq!(signature.parameters.len(), 5);
    assert_eq!(
        signature.parameters.iter().filter(|p| p.positional).count(),
        4
    );
    assert_eq!(
        signature.parameters[4],
        FnxParameterSpec {
            name: "iterations",
            value_type: FnxParameterType::PositiveInteger,
            positional: false,
            default: None,
        }
    );
    assert_eq!(signature.outputs, &[FnxOutput::Vertex, FnxOutput::Score]);
    assert_eq!(
        signature.implementation,
        FnxImplementationClass::InCoreDecodedCache
    );
    // A native foundation row keeps its undirected law and identifies its
    // own CSR kernel, with pinned fnx serving as the semantics oracle.
    let foundation = FnxSignatureRegistry::lookup("fnx.betweenness_centrality").unwrap();
    assert_eq!(foundation.graph_kind, FnxGraphKind::Undirected);
    assert!(foundation.parameters.is_empty());
    assert_eq!(
        foundation.execution_kernel,
        "fgdb-prism/brandes-csr-source-order-v2"
    );
    // Katz stays unregistered: its checked foundation entry drops the result
    // and witness on non-convergence, so a refusal would have to invent one.
    assert!(FnxSignatureRegistry::lookup("fnx.katz_centrality").is_none());
    assert!(FnxSignatureRegistry::lookup("pagerank").is_none());
}

#[test]
fn cdlp_binds_required_nonnegative_iterations_without_losing_call_identity() {
    let registration = FnxSignatureRegistry::lookup("fnx.cdlp").unwrap();
    assert_eq!(registration.graph_kind, FnxGraphKind::Any);
    assert_eq!(registration.numeric_profile, FNX_DISCRETE_PROFILE);
    assert_eq!(registration.rng_policy, "none");
    assert_eq!(
        registration.parameters,
        &[FnxParameterSpec {
            name: "max_iter",
            value_type: FnxParameterType::NonNegativeInteger,
            positional: true,
            default: None,
        }]
    );
    assert_eq!(
        registration.outputs,
        &[FnxOutput::Vertex, FnxOutput::Component]
    );
    assert_eq!(
        registration.execution_kernel,
        "fgdb-prism/cdlp-synchronous-csr-v1"
    );
    for max_iter in [0, 1, 5] {
        let call = FnxCallSpec::cdlp(max_iter);
        assert_eq!(bind(&format!("CALL fnx.cdlp({max_iter})")), call);
        assert_eq!(
            bind(&format!("CALL fnx.cdlp(max_iter => {max_iter})")),
            call
        );
        assert_eq!(call.algorithm(), FnxAlgorithm::Cdlp { max_iter });
        let parameters =
            FnxParameters::from([("steps".to_owned(), FnxArgument::Integer(max_iter as i64))]);
        assert_eq!(
            FnxCallSpec::bind("CALL fnx.cdlp(max_iter => $steps)", &parameters).unwrap(),
            call
        );
        assert!(!call.supports_sealed_execution());
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            assert!(matches!(
                call.validate_sealed_projection(direction),
                Err(FnxSealedExecutionError::UnsupportedAlgorithm(
                    FnxAlgorithm::Cdlp { .. }
                ))
            ));
        }
    }
    assert_ne!(FnxCallSpec::cdlp(0).digest(), FnxCallSpec::cdlp(1).digest());
    assert_ne!(FnxCallSpec::cdlp(1).digest(), FnxCallSpec::cdlp(5).digest());
    let aliased = bind("CALL fnx.cdlp(2) YIELD component AS community,node AS member");
    assert_eq!(aliased.outputs()[0].field, FnxOutput::Component);
    assert_eq!(aliased.outputs()[0].name, "community");
    assert_eq!(aliased.outputs()[1].field, FnxOutput::Vertex);
    assert_ne!(aliased.digest(), FnxCallSpec::cdlp(2).digest());
    assert_eq!(
        FnxCallSpec::bind("CALL fnx.cdlp()", &FnxParameters::new())
            .unwrap_err()
            .kind,
        FnxBindErrorKind::MissingArgument("max_iter")
    );
    for argument in [
        FnxArgument::Integer(-1),
        FnxArgument::Float(2.0),
        FnxArgument::Boolean(false),
        FnxArgument::Null,
        FnxArgument::Vertex(VId(u128::MAX)),
    ] {
        let error =
            FnxCallSpec::from_named_arguments("fnx", "cdlp", &[("max_iter", argument)], &[])
                .unwrap_err();
        assert_eq!(error.site, FnxCallSite::Argument(0));
        assert!(matches!(error.kind, FnxBindErrorKind::InvalidArgument(_)));
    }
    for text in [
        "CALL fnx.cdlp(1,2)",
        "CALL fnx.cdlp(iterations => 2)",
        "CALL fnx.cdlp(max_iter => 1,max_iter => 2)",
        "CALL fnx.cdlp(340282366920938463463374607431768211455)",
        "CALL fnx.cdlp(2) YIELD score",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
}

/// fgdb-luq0b: `node` is the openCypher spelling of the vertex output. The
/// column keeps the name written, so the two spellings are different calls.
#[test]
fn node_selects_the_vertex_output_under_its_own_column_name() {
    let node = bind("CALL fnx.pagerank() YIELD node, score");
    assert_eq!(node.outputs()[0].field, FnxOutput::Vertex);
    assert_eq!(node.outputs()[0].name, "node");
    assert_ne!(
        node.digest(),
        bind("CALL fnx.pagerank() YIELD vertex, score").digest()
    );
    let aliased = bind("CALL fnx.pagerank() YIELD node AS v");
    assert_eq!(aliased.outputs()[0].field, FnxOutput::Vertex);
    assert_eq!(aliased.outputs()[0].name, "v");
    // Both spellings name one field, so together they repeat it.
    let twice = FnxCallSpec::bind(
        "CALL fnx.pagerank() YIELD vertex, node",
        &FnxParameters::new(),
    )
    .unwrap_err();
    assert_eq!(twice.kind, FnxBindErrorKind::DuplicateYield);
    // `node` spells the vertex only.
    assert!(!FnxOutput::Score.is_named("node"));
    assert!(!FnxOutput::Component.is_named("node"));
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
    let prepared =
        FnxCallSpec::bind(" CALL fnx.pagerank($a, $k, $t, $w) YIELD * ; ", &params).unwrap();
    assert_eq!(default.digest(), prepared.digest());
    params.insert("a".to_owned(), FnxArgument::Float(0.5));
    assert_eq!(prepared.options().unwrap().alpha(), 0.85); // frozen, not a late parameter lookup
    assert_eq!(bind("CALL fnx.pagerank(0.85)").digest(), default.digest());
    assert_eq!(
        bind("CALL fnx.pagerank(-0.0)").digest(),
        bind("CALL fnx.pagerank(0)").digest()
    );
}

#[test]
fn fixed_iteration_calls_freeze_named_controls_without_changing_positional_defaults() {
    let default = bind("CALL fnx.pagerank()");
    assert_eq!(
        default.options().unwrap().stopping_rule(),
        PageRankStoppingRule::Convergence
    );
    assert_eq!(default.options().unwrap().iterations(), None);
    assert_eq!(
        default,
        bind("CALL fnx.pagerank(weighted => true, tol => 1e-6, alpha => 0.85, max_iter => 100)")
    );
    let fixed = bind(
        "CALL fnx.pagerank(iterations => 2, weighted => false, alpha => 0.85) YIELD score AS rank, vertex AS id",
    );
    let reordered = FnxCallSpec::from_named_arguments(
        "fnx",
        "pagerank",
        &[
            ("alpha", FnxArgument::Float(0.85)),
            ("weighted", FnxArgument::Boolean(false)),
            ("iterations", FnxArgument::Integer(2)),
        ],
        &[("score", "rank"), ("vertex", "id")],
    )
    .unwrap();
    assert_eq!(fixed, reordered);
    let options = fixed.options().unwrap();
    assert_eq!(
        options.stopping_rule(),
        PageRankStoppingRule::ExactIterations
    );
    assert_eq!(options.iterations(), Some(2));
    assert_eq!(options.max_iter(), 2);
    assert!(!options.weighted());
    let mut parameters = [("k".to_owned(), FnxArgument::Integer(2))]
        .into_iter()
        .collect::<FnxParameters>();
    let prepared = FnxCallSpec::bind(
        "CALL fnx.pagerank(iterations /* count */ => $k, weighted => false) YIELD score AS rank, vertex AS id",
        &parameters,
    )
    .unwrap();
    parameters.insert("k".to_owned(), FnxArgument::Integer(7));
    assert_eq!(prepared, fixed);
    assert_ne!(
        fixed.digest(),
        bind("CALL fnx.pagerank(iterations => 3, weighted => false) YIELD score AS rank, vertex AS id").digest()
    );
    assert_ne!(
        fixed.digest(),
        bind("CALL fnx.pagerank(0.85,2,1e-6,false) YIELD score AS rank, vertex AS id").digest()
    );
    assert!(FnxCallSpec::continues_past_call(
        "CALL fnx.pagerank(iterations => $missing) YIELD vertex, score RETURN vertex, score"
    ));
    assert_eq!(
        FnxCallSpec::from_named_arguments(
            "fnx",
            "single_source_shortest_path_length",
            &[
                ("cutoff", FnxArgument::Integer(3)),
                ("source", FnxArgument::Vertex(VId(u128::MAX))),
            ],
            &[],
        )
        .unwrap(),
        FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), Some(3))
    );
}

#[test]
fn named_iteration_refusals_identify_the_supplied_argument_and_reject_ambiguous_controls() {
    for value in [
        FnxArgument::Integer(0),
        FnxArgument::Integer(-1),
        FnxArgument::Float(2.0),
        FnxArgument::Boolean(true),
        FnxArgument::Null,
        FnxArgument::Vertex(VId(u128::MAX)),
    ] {
        let error = FnxCallSpec::from_named_arguments(
            "fnx",
            "pagerank",
            &[
                ("weighted", FnxArgument::Boolean(false)),
                ("iterations", value),
            ],
            &[],
        )
        .unwrap_err();
        assert_eq!(error.site, FnxCallSite::Argument(1), "{value:?}");
        assert!(matches!(error.kind, FnxBindErrorKind::InvalidArgument(_)));
    }
    for supplied in [
        vec![
            ("iterations", FnxArgument::Integer(2)),
            ("max_iter", FnxArgument::Integer(100)),
        ],
        vec![
            ("iterations", FnxArgument::Integer(2)),
            ("tol", FnxArgument::Float(1e-6)),
        ],
        vec![
            ("weighted", FnxArgument::Boolean(false)),
            ("iterations", FnxArgument::Integer(2)),
            ("iterations", FnxArgument::Integer(3)),
        ],
        vec![
            ("iterations", FnxArgument::Integer(2)),
            ("unknown", FnxArgument::Integer(3)),
        ],
        vec![
            ("iterations", FnxArgument::Integer(2)),
            ("alpha", FnxArgument::Float(1.0)),
        ],
    ] {
        let error =
            FnxCallSpec::from_named_arguments("fnx", "pagerank", &supplied, &[]).unwrap_err();
        assert_eq!(error.site, FnxCallSite::Argument(supplied.len() - 1));
    }
    for supplied in [
        [
            ("tol", FnxArgument::Float(1e-6)),
            ("iterations", FnxArgument::Integer(2)),
        ],
        [
            ("max_iter", FnxArgument::Integer(100)),
            ("iterations", FnxArgument::Integer(2)),
        ],
    ] {
        assert_eq!(
            FnxCallSpec::from_named_arguments("fnx", "pagerank", &supplied, &[])
                .unwrap_err()
                .site,
            FnxCallSite::Argument(0)
        );
    }
    for text in [
        "CALL fnx.pagerank(0.85, iterations => 2)",
        "CALL fnx.pagerank(iterations => 2, 0.85)",
        "CALL fnx.pagerank(iterations => 2, max_iter => 100)",
        "CALL fnx.pagerank(tol => 1e-6, iterations => 2)",
        "CALL fnx.pagerank(iterations => 9223372036854775808)",
        "CALL fnx.pagerank(iterations => 340282366920938463463374607431768211456)",
        "CALL fnx.pagerank(0.85,100,1e-6,true,2)",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
    let invalid_text = "CALL fnx.pagerank(weighted => false, iterations => 0)";
    assert_eq!(
        FnxCallSpec::bind(invalid_text, &FnxParameters::new())
            .unwrap_err()
            .at,
        invalid_text.find("iterations").unwrap()
    );
    assert!(PageRankOptions::fixed_iterations(0.85, 0, true).is_err());
    assert!(PageRankOptions::fixed_iterations(f64::NAN, 2, true).is_err());
    assert!(PageRankOptions::fixed_iterations(1.0, 2, true).is_err());
    assert_eq!(
        FnxCallSpec::from_named_arguments("fnx", "single_source_shortest_path_length", &[], &[])
            .unwrap_err()
            .site,
        FnxCallSite::Arguments
    );
}

#[test]
fn standalone_text_matches_native_comments_and_numeric_spelling() {
    let canonical = bind("CALL fnx.pagerank(0.85,100,1e-6,true) YIELD vertex,score");
    let commented = bind(
        "/* lead */ CALL /* keyword */ fnx /* namespace */ . pagerank /* name */ (\
         85e-2, // alpha\n 100, 1E-6, true /* weighted */) \
         /* before yield */ YIELD vertex, /* split */ score ; // tail\n",
    );
    assert_eq!(commented, canonical);
    assert!(!FnxCallSpec::continues_past_call(
        "/* lead */ CALL fnx.pagerank() /* done */"
    ));
    assert!(FnxCallSpec::continues_past_call(
        "CALL fnx.pagerank() YIELD vertex /* pipeline */ RETURN vertex"
    ));

    for text in [
        "CALL fnx.pagerank(.85)",
        "CALL fnx.pagerank(0.)",
        "CALL fnx.pagerank(85e)",
        "CALL fnx.pagerank(85e+)",
        "CALL fnx.pagerank(85e-)",
        "CALL fnx.pagerank(0.8.5)",
        "CALL fnx.pagerank(-.5)",
        "CALL fnx.pagerank(+.5)",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }

    let unterminated =
        FnxCallSpec::bind("CALL fnx.pagerank() /* missing", &FnxParameters::new()).unwrap_err();
    assert_eq!(unterminated.kind, FnxBindErrorKind::Expected("closing */"));
}

#[test]
fn yield_slots_aliases_and_call_digests_are_bound() {
    let call = bind("CALL fnx.pagerank() YIELD score AS rank, vertex AS id");
    assert_eq!(call.outputs()[0].field, FnxOutput::Score);
    assert_eq!(call.outputs()[0].name, "rank");
    let view = graph(&[VId(99)], &[], Directedness::Directed);
    let result = execute(&call, &view);
    assert_eq!(result.columns, vec!["rank", "id"]);
    assert_eq!(
        result.rows,
        vec![vec![FnxValue::Score(1.0), FnxValue::Vertex(VId(99))]]
    );
    assert_ne!(call.digest(), bind("CALL fnx.pagerank()").digest());
    assert_eq!(
        execute(&bind("CALL fnx.pagerank() YIELD score"), &view).rows,
        vec![vec![FnxValue::Score(1.0)]]
    );
}

#[test]
fn unsupported_or_malformed_calls_never_bind_a_partial_program() {
    for text in [
        "",
        "MATCH (n) RETURN n",
        "CALL fnx.pagerank",
        "CALL other.pagerank()",
        "CALL fnx.pagerankevil()",
        "CALL fnx.unknown()",
        "CALL fnx.pagerank(,)",
        "CALL fnx.pagerank(0.85,)",
        "CALL fnx.pagerank(0.85,100,1e-6,true,1)",
        "CALL fnx.pagerank() YIELD",
        "CALL fnx.pagerank() YIELD vertex,",
        "CALL fnx.pagerank() YIELD score,score",
        "CALL fnx.pagerank() YIELD vertex AS x,score AS x",
        "CALL fnx.pagerank() YIELD missing",
        "CALL fnx.pagerank() YIELD * RETURN vertex",
        "CALL fnx.pagerank(); DELETE n",
        "CALL fnx.pagerank();;",
        "CALL fnx.pagerank($missing)",
        "CALL fnx.pagerank('0.85')",
        "CALL fnx.pagerank(😺)",
        "CALL fnx.pagerank() YIELD 🦀",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
    let long = " ".repeat(MAX_FNX_CALL_BYTES + 1);
    assert_eq!(
        FnxCallSpec::bind(&long, &FnxParameters::new())
            .unwrap_err()
            .kind,
        FnxBindErrorKind::TextTooLong
    );
}

#[test]
fn numeric_parameters_refuse_nonfinite_lossy_and_wrong_domain_values() {
    for text in [
        "CALL fnx.pagerank(1)",
        "CALL fnx.pagerank(-0.1)",
        "CALL fnx.pagerank(1e999)",
        "CALL fnx.pagerank(0.85,0)",
        "CALL fnx.pagerank(0.85,-1)",
        "CALL fnx.pagerank(0.85,1.0)",
        "CALL fnx.pagerank(0.85,10,0)",
        "CALL fnx.pagerank(0.85,10,-1e-6)",
        "CALL fnx.pagerank(0.85,10,1e-6,1)",
        "CALL fnx.pagerank(true)",
        "CALL fnx.pagerank(999999999999999999999999)",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
    for value in [
        FnxArgument::Float(f64::NAN),
        FnxArgument::Float(f64::INFINITY),
        FnxArgument::Integer(9_007_199_254_740_993),
    ] {
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
        let edges: Vec<_> = (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| edge(bit as u128, vertices[bit / 3].0, vertices[bit % 3].0, 1.0))
            .collect();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
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
                    if s <= t {
                        undirected.add_edge(from, to).unwrap();
                    }
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
                assert!(
                    (actual_scores[index] - score.score).abs() < 1e-12,
                    "mask {mask}, {direction:?}"
                );
            }
            assert_eq!(actual.certificate.witness, oracle.witness);
            assert_eq!(actual.certificate.vertices, 3);
            assert_eq!(actual.certificate.edges, view.edge_count());
        }
    }
}

#[test]
fn weighted_parallel_edges_and_dangling_vertices_match_dense_oracle() {
    let view = graph(
        &[VId(1), VId(2), VId(3)],
        &[
            edge(4, 1, 2, 1.0),
            edge(1, 1, 2, 2.0),
            edge(3, 1, 3, 1.0),
            edge(9, 2, 1, 2.0),
        ],
        Directedness::Directed,
    );
    let result = execute(&bind("CALL fnx.pagerank(0.85,1000,1e-13,true)"), &view);
    // Explicit row-stochastic dense matrix; vertex 3 is a dangling row.
    let matrix = [[0.0, 0.75, 0.25], [1.0, 0.0, 0.0], [1.0 / 3.0; 3]];
    let mut expected = [1.0 / 3.0; 3];
    for _ in 0..1000 {
        let mut next = [0.15 / 3.0; 3];
        for t in 0..3 {
            for s in 0..3 {
                next[t] += 0.85 * expected[s] * matrix[s][t];
            }
        }
        let residual: f64 = next
            .iter()
            .zip(expected)
            .map(|(&a, b)| f64::abs(a - b))
            .sum();
        expected = next;
        if residual < 1e-14 {
            break;
        }
    }
    for (actual, expected) in scores(&result).into_iter().zip(expected) {
        assert!((actual - expected).abs() < 1e-11);
    }
    assert_eq!(result.certificate.input_edges, 4);
    assert_eq!(result.certificate.edges, 3);
    assert_ne!(
        scores(&result),
        scores(&execute(
            &bind("CALL fnx.pagerank(0.85,1000,1e-13,false)"),
            &view
        ))
    );
}

#[test]
fn fixed_iteration_pagerank_matches_an_independent_dense_transition_oracle() {
    let view = graph(
        &[VId(1), VId(2), VId(3), VId(u128::MAX)],
        &[
            edge(4, 1, 2, 1.0),
            edge(1, 1, 2, 2.0),
            edge(3, 1, 3, 1.0),
            edge(9, 2, 1, 2.0),
            edge(12, 3, 2, 0.0),
            edge(13, 3, 3, 0.0),
        ],
        Directedness::Directed,
    );
    // Independently specified Markov matrices after the declared parallel
    // reduction. Row 3 has zero weighted outflow but two unweighted arcs;
    // the full-width fourth vertex is isolated in both interpretations.
    for (weighted, matrix) in [
        (
            true,
            [
                [0.0, 0.75, 0.25, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                [0.25; 4],
                [0.25; 4],
            ],
        ),
        (
            false,
            [
                [0.0, 0.5, 0.5, 0.0],
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 0.5, 0.5, 0.0],
                [0.25; 4],
            ],
        ),
    ] {
        for alpha in [0.0, 0.85, 0.99] {
            for iterations in [1, 2, 7] {
                let mut expected = [0.25; 4];
                for _ in 0..iterations {
                    let mut next = [(1.0 - alpha) / 4.0; 4];
                    for (target, value) in next.iter_mut().enumerate() {
                        for (rank, row) in expected.iter().zip(matrix) {
                            *value += alpha * rank * row[target];
                        }
                    }
                    expected = next;
                }
                let call = FnxCallSpec::pagerank(
                    PageRankOptions::fixed_iterations(alpha, iterations, weighted).unwrap(),
                );
                let actual = execute(&call, &view);
                for (actual, expected) in scores(&actual).into_iter().zip(expected) {
                    assert!(
                        (actual - expected).abs() <= 3e-15,
                        "weighted={weighted} alpha={alpha} iterations={iterations}: {actual} != {expected}"
                    );
                }
                assert_eq!(
                    actual.certificate.witness.algorithm,
                    "pagerank_fixed_iterations"
                );
                assert_eq!(actual.certificate.witness.nodes_touched, 4 * iterations);
                assert_eq!(actual.certificate.witness.edges_scanned, 5 * iterations);
                assert_eq!(actual.certificate.call_digest, call.digest());
            }
        }
    }
    let fixed = execute(&bind("CALL fnx.pagerank(iterations => 2)"), &view);
    assert_ne!(
        scores(&fixed),
        scores(&execute(&bind("CALL fnx.pagerank(iterations => 1)"), &view))
    );
    assert_ne!(
        scores(&fixed),
        scores(&execute(&bind("CALL fnx.pagerank()"), &view))
    );
}

#[test]
fn fixed_iterations_do_not_stop_at_stationarity_or_claim_convergence() {
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for edges in [vec![], vec![edge(1, 1, 2, 1.0), edge(2, 2, 1, 1.0)]] {
            let view = graph(&[VId(1), VId(2)], &edges, direction);
            let converged = execute(&FnxCallSpec::pagerank(PageRankOptions::default()), &view);
            let fixed = execute(&bind("CALL fnx.pagerank(iterations => 7)"), &view);
            assert_eq!(scores(&fixed), vec![0.5, 0.5]);
            assert_eq!(fixed.columns, converged.columns);
            assert_eq!(fixed.rows, converged.rows);
            // Result digests include the frozen call before the scalar rows;
            // equal stationary scores do not erase their stopping contract.
            assert_ne!(
                fixed.certificate.result_digest,
                converged.certificate.result_digest
            );
            assert_eq!(converged.certificate.witness.nodes_touched, 2);
            assert_eq!(fixed.certificate.witness.nodes_touched, 14);
            assert_eq!(fixed.certificate.witness.edges_scanned, edges.len() * 7);
            assert_ne!(
                fixed.certificate.call_digest,
                converged.certificate.call_digest
            );
            assert_ne!(fixed.certificate.digest, converged.certificate.digest);
        }
    }
    let empty = execute(
        &bind("CALL fnx.pagerank(iterations => 7)"),
        &graph(&[], &[], Directedness::Directed),
    );
    assert!(empty.rows.is_empty());
    assert_eq!(empty.certificate.witness.nodes_touched, 0);
    assert_eq!(
        empty.certificate.witness.algorithm,
        "pagerank_fixed_iterations"
    );
}

#[test]
fn fixed_iteration_limits_numeric_refusals_and_every_cancellation_cut_remain_enforced() {
    let call = bind("CALL fnx.pagerank(iterations => 2)");
    let view = graph(
        &[VId(1), VId(2)],
        &[edge(1, 1, 2, 1.0)],
        Directedness::Directed,
    );
    let expected = execute(&call, &view);
    let exact = FnxExecutionLimits {
        max_iterations: 2,
        max_result_rows: 2,
        max_estimated_work: expected.certificate.estimated_work,
    };
    assert_eq!(
        call.execute(&view, exact, || Ok::<(), Infallible>(()))
            .unwrap(),
        expected
    );
    for (resource, cap) in [
        (
            "iterations",
            FnxExecutionLimits {
                max_iterations: 1,
                ..exact
            },
        ),
        (
            "result rows",
            FnxExecutionLimits {
                max_result_rows: 1,
                ..exact
            },
        ),
        (
            "estimated work",
            FnxExecutionLimits {
                max_estimated_work: exact.max_estimated_work - 1,
                ..exact
            },
        ),
    ] {
        assert!(
            matches!(call.execute(&view, cap, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::LimitExceeded { resource: actual, .. }) if actual == resource)
        );
    }
    let huge =
        FnxCallSpec::pagerank(PageRankOptions::fixed_iterations(0.85, usize::MAX, true).unwrap());
    assert!(matches!(
        huge.execute(
            &view,
            FnxExecutionLimits {
                max_iterations: usize::MAX,
                ..limits()
            },
            || Ok::<(), Infallible>(())
        ),
        Err(FnxExecutionError::SizeOverflow)
    ));
    let negative = graph(
        &[VId(1), VId(2)],
        &[edge(1, 1, 2, -1.0)],
        Directedness::Directed,
    );
    assert!(matches!(
        call.execute(&negative, limits(), || Ok::<(), Infallible>(())),
        Err(FnxExecutionError::NegativeWeight)
    ));
    execute(
        &bind("CALL fnx.pagerank(iterations => 2, weighted => false)"),
        &negative,
    );
    let overflow = graph(
        &[VId(1), VId(2), VId(3)],
        &[edge(1, 1, 2, f64::MAX), edge(2, 1, 3, f64::MAX)],
        Directedness::Directed,
    );
    assert!(matches!(
        call.execute(&overflow, limits(), || Ok::<(), Infallible>(())),
        Err(FnxExecutionError::NonFiniteWeightSum)
    ));
    let mut count = 0;
    assert_eq!(
        call.execute(&view, exact, || {
            count += 1;
            Ok::<(), &'static str>(())
        })
        .unwrap(),
        expected
    );
    for stop in 1..=count {
        let mut seen = 0;
        assert!(matches!(
            call.execute(&view, exact, || {
                seen += 1;
                if seen == stop {
                    Err("fixed-step cancellation")
                } else {
                    Ok(())
                }
            }),
            Err(FnxExecutionError::Cancelled("fixed-step cancellation"))
        ));
        assert_eq!(seen, stop);
    }
    assert!(count > 2 * view.node_count());
    assert_eq!(execute(&call, &view), expected);
}

#[test]
fn isolated_empty_zero_weight_and_reversed_graphs_are_not_lost() {
    let call = bind("CALL fnx.pagerank(0.85,1000,1e-12)");
    let empty = execute(&call, &graph(&[], &[], Directedness::Directed));
    assert!(empty.rows.is_empty());
    assert_eq!(empty.certificate.witness.nodes_touched, 0);
    let isolated = execute(
        &call,
        &graph(&[VId(9), VId(1)], &[], Directedness::Directed),
    );
    assert_eq!(scores(&isolated), vec![0.5, 0.5]);
    let zero = execute(
        &call,
        &graph(
            &[VId(1), VId(2)],
            &[edge(1, 1, 2, 0.0)],
            Directedness::Directed,
        ),
    );
    assert_eq!(scores(&zero), vec![0.5, 0.5]);
    let forward = execute(
        &call,
        &graph(
            &[VId(1), VId(2)],
            &[edge(1, 1, 2, 1.0)],
            Directedness::Directed,
        ),
    );
    let reverse = execute(
        &call,
        &graph(
            &[VId(1), VId(2)],
            &[edge(1, 1, 2, 1.0)],
            Directedness::Reversed,
        ),
    );
    assert!(scores(&forward)[1] > scores(&forward)[0]);
    assert!((scores(&forward)[0] - scores(&reverse)[1]).abs() < 1e-12);
}

#[test]
fn numerical_and_convergence_failures_are_typed_not_silent_results() {
    let call = bind("CALL fnx.pagerank()");
    let negative = graph(
        &[VId(1), VId(2)],
        &[edge(1, 1, 2, -1.0)],
        Directedness::Directed,
    );
    assert!(matches!(
        call.execute(&negative, limits(), || Ok::<(), Infallible>(())),
        Err(FnxExecutionError::NegativeWeight)
    ));
    // The unweighted signature does not inspect an intentionally ignored weight.
    execute(&bind("CALL fnx.pagerank(0.85,100,1e-6,false)"), &negative);
    let overflow = graph(
        &[VId(1), VId(2), VId(3)],
        &[edge(1, 1, 2, f64::MAX), edge(2, 1, 3, f64::MAX)],
        Directedness::Directed,
    );
    assert!(matches!(
        call.execute(&overflow, limits(), || Ok::<(), Infallible>(())),
        Err(FnxExecutionError::NonFiniteWeightSum)
    ));
    let path = graph(
        &[VId(1), VId(2)],
        &[edge(1, 1, 2, 1.0)],
        Directedness::Directed,
    );
    let too_short = bind("CALL fnx.pagerank(0.85,1,1e-20)");
    match too_short.execute(&path, limits(), || Ok::<(), Infallible>(())) {
        Err(FnxExecutionError::NotConverged {
            max_iterations,
            witness,
        }) => {
            assert_eq!(max_iterations, 1);
            assert_eq!(witness.algorithm, "pagerank_power_iteration");
        }
        other => panic!("expected convergence refusal: {other:?}"),
    }
}

#[test]
fn iteration_row_and_work_admission_refuse_before_algorithm_execution() {
    let call = bind("CALL fnx.pagerank()");
    let view = graph(
        &[VId(1), VId(2)],
        &[edge(1, 1, 2, 1.0)],
        Directedness::Directed,
    );
    for cap in [
        FnxExecutionLimits {
            max_iterations: 99,
            ..limits()
        },
        FnxExecutionLimits {
            max_result_rows: 1,
            ..limits()
        },
        FnxExecutionLimits {
            max_estimated_work: 302,
            ..limits()
        },
    ] {
        assert!(matches!(
            call.execute(&view, cap, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::LimitExceeded { .. })
        ));
    }
    let cap = FnxExecutionLimits {
        max_estimated_work: 303,
        ..limits()
    };
    assert_eq!(
        call.execute(&view, cap, || Ok::<(), Infallible>(()))
            .unwrap()
            .certificate
            .estimated_work,
        303
    );
    let huge = FnxCallSpec::pagerank(PageRankOptions::new(0.85, usize::MAX, 1e-6, true).unwrap());
    assert!(matches!(
        huge.execute(
            &view,
            FnxExecutionLimits {
                max_iterations: usize::MAX,
                ..limits()
            },
            || Ok::<(), Infallible>(())
        ),
        Err(FnxExecutionError::SizeOverflow)
    ));
}

#[test]
fn cancellation_at_every_adapter_checkpoint_discards_results() {
    let call = bind("CALL fnx.pagerank()");
    let view = graph(
        &[VId(1), VId(2)],
        &[edge(1, 1, 2, 1.0)],
        Directedness::Directed,
    );
    let mut count = 0;
    call.execute(&view, limits(), || {
        count += 1;
        Ok::<(), &'static str>(())
    })
    .unwrap();
    for stop in 1..=count {
        let mut seen = 0;
        let result = call.execute(&view, limits(), || {
            seen += 1;
            if seen == stop { Err("cancel") } else { Ok(()) }
        });
        assert!(matches!(
            result,
            Err(FnxExecutionError::Cancelled("cancel"))
        ));
    }
    // Includes checkpoints inside the native, fnx-differential kernel.
    assert!(count > 5);
}

#[test]
fn evidence_binds_snapshot_call_projection_result_and_upstream_witness() {
    let call = bind("CALL fnx.pagerank()");
    let view = graph(
        &[VId(1), VId(2)],
        &[edge(1, 1, 2, 1.0)],
        Directedness::Directed,
    );
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
