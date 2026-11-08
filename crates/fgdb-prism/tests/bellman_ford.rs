//! Public binding, delivery and admission laws for exact signed distances.
//! Numerical oracle cases are kept separate from these host boundary laws.

use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use std::convert::Infallible;

fn view(vertices: &[VId], edges: &[(VId, VId, f64)]) -> SnapshotGraphView {
    let edges: Vec<_> = edges
        .iter()
        .enumerate()
        .map(|(index, &(source, target, weight))| ProjectionEdge {
            eid: EId(index as u128 + 1),
            source,
            target,
            weight,
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0x42; 32]),
            as_of: CommitSeq(8),
        },
        vertices,
        &edges,
        ProjectionSpec {
            directedness: Directedness::Directed,
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 1000,
            max_adjacency_entries: 1000,
            max_workspace_bytes: 1 << 20,
        },
    )
    .unwrap()
}

fn limits() -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: 100,
        max_estimated_work: 1 << 24,
    }
}

#[test]
fn signed_distance_binding_delivery_and_exact_limits_share_one_result() {
    let source = VId(1 << 100);
    let vertices = [VId(u128::MAX), source, VId(0), VId(77)];
    let graph = view(
        &vertices,
        &[(source, VId(0), 4.0), (VId(0), VId(u128::MAX), -7.0)],
    );
    let arguments = FnxParameters::from([("origin".to_owned(), FnxArgument::Vertex(source))]);
    let default = FnxCallSpec::bind(
        "CALL fnx.single_source_bellman_ford_path_length(source => $origin)",
        &arguments,
    )
    .unwrap();
    assert_eq!(
        default,
        FnxCallSpec::single_source_bellman_ford_path_length(source)
    );
    assert!(default.supports_sealed_execution());
    assert!(default.supports_cooperative_sealed_execution());
    assert_eq!(
        default.numeric_profile(),
        "fgdb-bellman-ford-exact-dyadic-round-once-canonical-sweep-v1"
    );
    let call = FnxCallSpec::bind(
        "CALL fnx.single_source_bellman_ford_path_length($origin) YIELD distance AS cost,node AS destination",
        &arguments,
    ).unwrap();
    for text in [
        "CALL fnx.single_source_bellman_ford_path_length()",
        "CALL fnx.single_source_bellman_ford_path_length(-1)",
        "CALL fnx.single_source_bellman_ford_path_length(NULL)",
        "CALL fnx.single_source_bellman_ford_path_length(true)",
        "CALL fnx.single_source_bellman_ford_path_length(source => 1, cutoff => 2)",
        "CALL fnx.single_source_bellman_ford_path_length(1) YIELD score",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
    let mut checkpoints = 0;
    let expected = call
        .execute(&graph, limits(), || {
            checkpoints += 1;
            Ok::<(), &'static str>(())
        })
        .unwrap();
    assert_eq!(
        expected.rows,
        vec![
            vec![FnxValue::Float(4.0), FnxValue::Vertex(VId(0))],
            vec![FnxValue::Float(0.0), FnxValue::Vertex(source)],
            vec![FnxValue::Float(-3.0), FnxValue::Vertex(VId(u128::MAX))],
        ]
    );
    assert!(checkpoints <= expected.certificate.estimated_work);
    for stop in 1..=checkpoints {
        let mut at = 0;
        assert!(matches!(
            call.execute(&graph, limits(), || {
                at += 1;
                if at == stop { Err("stop") } else { Ok(()) }
            }),
            Err(FnxExecutionError::Cancelled("stop"))
        ));
        assert_eq!(at, stop);
    }
    let mut mapped = Vec::new();
    assert_eq!(
        call.execute_into(&graph, limits(), || Ok::<(), Infallible>(()), &mut mapped)
            .unwrap(),
        expected.certificate
    );
    assert_eq!(mapped, expected.rows);
    let mut cursor = call
        .execute_rows(&graph, limits(), || Ok::<(), Infallible>(()))
        .unwrap();
    assert_eq!(cursor.certificate(), &expected.certificate);
    let mut retained = Vec::new();
    let mut row = Vec::new();
    while cursor
        .next_row(&mut row, || Ok::<(), Infallible>(()))
        .unwrap()
    {
        retained.push(row.clone());
    }
    assert_eq!(retained, expected.rows);
    let exact = FnxExecutionLimits {
        max_result_rows: expected.rows.len(),
        max_estimated_work: expected.certificate.estimated_work,
        ..limits()
    };
    assert_eq!(
        call.execute(&graph, exact, || Ok::<(), Infallible>(()))
            .unwrap(),
        expected
    );
    for bad in [
        FnxExecutionLimits {
            max_result_rows: exact.max_result_rows - 1,
            ..exact
        },
        FnxExecutionLimits {
            max_estimated_work: exact.max_estimated_work - 1,
            ..exact
        },
    ] {
        assert!(matches!(
            call.execute(&graph, bad, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::LimitExceeded { .. })
        ));
    }
    assert!(matches!(
        FnxCallSpec::single_source_bellman_ford_path_length(VId(42)).execute(
            &graph,
            limits(),
            || Ok::<(), Infallible>(())
        ),
        Err(FnxExecutionError::UnknownSource(VId(42)))
    ));
}

#[test]
fn cycle_and_final_overflow_fail_before_a_mapped_sink_receives_rows() {
    let vertices = [VId(0), VId(1), VId(2)];
    let call = FnxCallSpec::single_source_bellman_ford_path_length(VId(0));
    let cycle = view(
        &vertices,
        &[
            (VId(0), VId(1), 1e20),
            (VId(1), VId(2), -1.0),
            (VId(2), VId(1), 0.0),
        ],
    );
    let overflow = view(
        &vertices,
        &[(VId(0), VId(1), f64::MAX), (VId(1), VId(2), f64::MAX)],
    );
    let mut sink = Vec::new();
    assert!(matches!(
        call.execute_into(&cycle, limits(), || Ok::<(), Infallible>(()), &mut sink),
        Err(FnxExecutionError::ReachableNegativeCycle)
    ));
    assert!(sink.is_empty());
    assert!(matches!(
        call.execute_into(&overflow, limits(), || Ok::<(), Infallible>(()), &mut sink),
        Err(FnxExecutionError::NonFiniteDistanceSum)
    ));
    assert!(sink.is_empty());
}
