//! Signed capability -> admitted historical source -> ordinary Prism kernels.
//! The control database physically omits hidden topology, not result rows.
//! These resident-source tests do not establish storage side-channel isolation.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxExecutionError, FnxExecutionLimits, FnxGraphKind,
    FnxParameters, FnxReadError, FnxReadOptions, FnxSelection, FnxSourceLimits,
    FnxValue, FnxWeightError, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy,
    ProjectionError, ProjectionLimits, ProjectionSpec, SelfLoopPolicy,
};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Restriction, Scope};

type Failure = FnxReadError<ReadError, QueryError>;
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x39; 32]);
const BRANCH: &str = "host-analytics-branch";
const BFS: &str = "CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance";
const DIJKSTRA: &str = "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD vertex,distance";
const CC: &str = "CALL fnx.connected_components() YIELD vertex,component";
const TRIANGLES: &str = "CALL fnx.triangles() YIELD vertex,triangles";
const CALLS: [&str; 8] = [
    "CALL fnx.pagerank(0.85,1000,1e-12,true) YIELD score AS rank,vertex AS id",
    BFS, DIJKSTRA, CC,
    "CALL fnx.weakly_connected_components() YIELD component,vertex",
    "CALL fnx.strongly_connected_components() YIELD vertex,component",
    TRIANGLES,
    "CALL fnx.clustering_coefficient() YIELD score,vertex",
];
fn authority(seed: u64, namespace: DatabaseSecurityNamespaceId) -> Authority {
    Authority::new(AuthKey::from_seed(seed), namespace, "host-graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut value = Grant::read_only(BRANCH, 1000, QueryLimits {
        max_nodes: 1000, max_work: 1_000_000, max_rows: 1000,
    });
    value.labels = Scope::only([LabelId(1)]);
    value.relations = Scope::only([RelationId(1)]);
    value.properties = Scope::only([PropertyKeyId(1)]);
    value
}
fn options(direction: Directedness) -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: None, relation: None,
            weight: FnxWeightSpec::Property { key: PropertyKeyId(1), missing: MissingWeightPolicy::Reject },
        },
        projection: ProjectionSpec {
            directedness: direction, parallel_edges: ParallelEdgePolicy::Minimum,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 100_000, max_scratch_entries: 10_000, max_staging_bytes: 1 << 20,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 100, max_input_edges: 100, max_adjacency_entries: 200,
            max_workspace_bytes: 1 << 20,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1000, max_result_rows: 100, max_estimated_work: 1 << 26,
        },
    }
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x31; 32], NS, [0x32; 32])).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [1, 2, 3, u128::MAX] {
        let labels = if id == 1 && hidden { vec![LabelId(1), LabelId(99)] } else { vec![LabelId(1)] };
        batch.create_vertex(VId(id), labels, vec![]);
    }
    for (eid, source, target, weight) in [(1,1,2,2), (2,2,3,3), (3,3,1,4), (4,1,2,5), (5,2,2,1)] {
        let mut props = vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))];
        if hidden { props.push((PropertyKeyId(2), CanonicalScalar::Bool(true))); }
        batch.add_edge(EId(eid), VId(source), VId(target), props);
    }
    if hidden {
        batch.create_vertex(VId(99), vec![LabelId(99)], vec![]);
        for (eid, source, target) in [(90,1,99), (91,99,3)] {
            batch.add_edge(EId(eid), VId(source), VId(target),
                vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(200), VId(1), VId(3),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))]);
        db.write(cx, other).await.unwrap();
    }
    db
}
fn read(db: &Database<MemVfs>, cx: &QueryCx, issuer: &Authority, token: &CapabilityToken,
    text: &str, options: FnxReadOptions) -> Vec<Vec<FnxValue>> {
    db.call_fnx_authorized(cx, issuer, token, BRANCH, text, &FnxParameters::new(), options, || 100).unwrap()
}
fn authorization(error: Failure, expected: Error) {
    assert!(matches!(error, FnxReadError::Cancelled(QueryError::Authorization(actual)) if actual == expected));
}

#[test]
fn all_eight_calls_match_a_physically_masked_database_in_every_compatible_direction() {
    let ((), report) = run_async_under_lab(0x5ec0_3001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), true).await;
        let oracle = database(&c.commit(), false).await;
        let issuer = authority(301, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
            for policy in [ParallelEdgePolicy::Minimum, ParallelEdgePolicy::Maximum,
                ParallelEdgePolicy::Sum, ParallelEdgePolicy::CollapseUnit] {
                for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                    let mut opt = options(direction);
                    opt.projection.parallel_edges = policy;
                    opt.projection.self_loops = loops;
                    for text in CALLS {
                        let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
                        let compatible = match call.signature().graph_kind {
                            FnxGraphKind::Any => true,
                            FnxGraphKind::Directed => direction != Directedness::Undirected,
                            FnxGraphKind::Undirected => direction == Directedness::Undirected,
                        };
                        if !compatible { continue; }
                        let expected = oracle.execute_fnx(&cx, &call, opt).unwrap().analytics.rows;
                        assert_eq!(read(&db, &cx, &issuer, &token, text, opt), expected,
                            "{text} {direction:?} {policy:?} {loops:?}");
                        assert_eq!(db.execute_fnx_authorized(&cx, &issuer, &token, BRANCH,
                            &call, opt, || 100).unwrap(), expected);
                    }
                }
            }
        }
        let mut opt = options(Directedness::Directed);
        opt.selection.weight = FnxWeightSpec::Unit;
        let masked = read(&db, &cx, &issuer, &token, BFS, opt);
        assert_eq!(masked, vec![
            vec![FnxValue::Vertex(VId(1)), FnxValue::Integer(0)],
            vec![FnxValue::Vertex(VId(2)), FnxValue::Integer(1)],
            vec![FnxValue::Vertex(VId(3)), FnxValue::Integer(2)],
        ]);
        // Filtering raw answers AFTER execution cannot repair this distance.
        let raw = db.call_fnx(&cx, BFS, &FnxParameters::new(), opt).unwrap().analytics.rows;
        assert!(raw.contains(&vec![FnxValue::Vertex(VId(3)), FnxValue::Integer(1)]));
        assert!(raw.iter().any(|row| row[0] == FnxValue::Vertex(VId(99))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_weight_values_and_label_names_are_absent_before_selection_or_reduction() {
    let ((), report) = run_async_under_lab(0x5ec0_3002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = database(&c.commit(), true).await;
        let issuer = authority(302, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut opt = options(Directedness::Directed);
        opt.selection.weight = FnxWeightSpec::Property { key: PropertyKeyId(2), missing: MissingWeightPolicy::Reject };
        assert!(matches!(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, DIJKSTRA,
            &FnxParameters::new(), opt, || 100), Err(FnxReadError::Weight { reason: FnxWeightError::Missing, .. })));
        // Restrict the raw control's graph to authorized endpoints, not fields.
        let mut raw = opt;
        raw.selection.vertex_label = Some(LabelId(1)); raw.selection.relation = Some(RelationId(1));
        assert!(matches!(db.call_fnx(&cx, DIJKSTRA, &FnxParameters::new(), raw),
            Err(FnxReadError::Weight { reason: FnxWeightError::NotNumeric, .. })));
        for (missing, expected) in [(MissingWeightPolicy::Unit, 2.0), (MissingWeightPolicy::Zero, 0.0)] {
            opt.selection.weight = FnxWeightSpec::Property { key: PropertyKeyId(2), missing };
            let result = read(&db, &cx, &issuer, &token, DIJKSTRA, opt);
            assert_eq!(result[2], vec![FnxValue::Vertex(VId(3)), FnxValue::Float(expected)]);
        }
        opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        opt.selection.weight = FnxWeightSpec::Property { key: PropertyKeyId(2), missing: MissingWeightPolicy::Reject };
        assert_eq!(read(&db, &cx, &issuer, &token, DIJKSTRA, opt)[2][1], FnxValue::Float(2.0));
        let both = token.attenuate(Restriction::Labels(Scope::only([LabelId(99)]))).unwrap();
        opt = options(Directedness::Undirected);
        opt.selection.weight = FnxWeightSpec::Unit;
        assert_eq!(read(&db, &cx, &issuer, &both, CC, opt),
            vec![vec![FnxValue::Vertex(VId(1)), FnxValue::Vertex(VId(1))]]);
        for label in [LabelId(1), LabelId(99)] {
            opt.selection.vertex_label = Some(label);
            assert!(read(&db, &cx, &issuer, &both, CC, opt).is_empty());
        }
        opt.selection.vertex_label = Some(LabelId(99));
        assert!(read(&db, &cx, &issuer, &token, CC, opt).is_empty());
        let denied_relations = token.attenuate(Restriction::Relations(Scope::only([]))).unwrap();
        opt = options(Directedness::Undirected);
        opt.selection.relation = Some(RelationId(2));
        let isolates = read(&db, &cx, &issuer, &token, CC, opt);
        assert_eq!(isolates.len(), 4);
        assert!(isolates.iter().all(|row| row[0] == row[1]));
        opt.selection.relation = None;
        assert_eq!(read(&db, &cx, &issuer, &denied_relations, CC, opt), isolates);
        let mut bad_loop = WriteBatch::new(RelationId(1));
        bad_loop.add_edge(EId(500), VId(u128::MAX), VId(u128::MAX),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))]);
        db.write(&c.commit(), bad_loop).await.unwrap();
        opt.projection.self_loops = SelfLoopPolicy::Drop;
        read(&db, &cx, &issuer, &token, TRIANGLES, opt);
        opt.projection.self_loops = SelfLoopPolicy::Keep;
        assert!(matches!(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, TRIANGLES,
            &FnxParameters::new(), opt, || 100), Err(FnxReadError::Weight { edge: EId(500), reason: FnxWeightError::NotNumeric })));
        opt.projection.self_loops = SelfLoopPolicy::Reject;
        assert!(matches!(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, TRIANGLES,
            &FnxParameters::new(), opt, || 100), Err(FnxReadError::Projection(ProjectionError::SelfLoop(_)))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_limits_and_native_limits_are_cumulative_and_independent() {
    let ((), report) = run_async_under_lab(0x5ec0_3003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit(), true).await;
        let issuer = authority(303, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let opt = options(Directedness::Undirected);
        let exact = token.attenuate(Restriction::MaxNodes(4)).unwrap()
            .attenuate(Restriction::MaxRows(4)).unwrap();
        assert_eq!(read(&db, &cx, &issuer, &exact, CC, opt).len(), 4);
        for (restriction, dimension) in [(Restriction::MaxNodes(3), LimitDimension::Nodes),
            (Restriction::MaxRows(3), LimitDimension::Rows), (Restriction::MaxWork(0), LimitDimension::Work)] {
            let denied = token.attenuate(restriction).unwrap();
            authorization(db.call_fnx_authorized(&cx, &issuer, &denied, BRANCH, CC,
                &FnxParameters::new(), opt, || 100).unwrap_err(), Error::LimitExceeded(dimension));
        }
        let mut clocks = 0u64;
        db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, CC, &FnxParameters::new(), opt,
            || { clocks += 1; 100 }).unwrap();
        // Every clock sample is a work charge except authentication, four
        // admitted-node charges, and the final row charge. No stage resets it.
        let work = clocks - 6;
        let exact_work = token.attenuate(Restriction::MaxWork(work)).unwrap();
        read(&db, &cx, &issuer, &exact_work, CC, opt);
        let short = token.attenuate(Restriction::MaxWork(work - 1)).unwrap();
        authorization(db.call_fnx_authorized(&cx, &issuer, &short, BRANCH, CC,
            &FnxParameters::new(), opt, || 100).unwrap_err(), Error::LimitExceeded(LimitDimension::Work));
        for resource in 0..6 {
            let mut tight = opt;
            match resource {
                0 => tight.source_limits.max_work_units = 0,
                1 => tight.source_limits.max_staging_bytes = 0,
                2 => tight.projection_limits.max_vertices = 0,
                3 => tight.projection_limits.max_workspace_bytes = 0,
                4 => tight.execution_limits.max_estimated_work = 0,
                _ => tight.execution_limits.max_result_rows = 3,
            }
            let error = db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, CC,
                &FnxParameters::new(), tight, || 100).unwrap_err();
            match resource {
                0 | 1 => assert!(matches!(error, FnxReadError::SourceLimit { .. })),
                2 | 3 => assert!(matches!(error, FnxReadError::Projection(ProjectionError::LimitExceeded { .. }))),
                _ => assert!(matches!(error, FnxReadError::Execution(FnxExecutionError::LimitExceeded { .. }))),
            }
        }
        let mut empty = opt; empty.selection.vertex_label = Some(LabelId(99));
        assert!(read(&db, &cx, &issuer, &token.attenuate(Restriction::MaxRows(0)).unwrap(), CC, empty).is_empty());
        let node_limit = token.attenuate(Restriction::MaxNodes(3)).unwrap();
        authorization(db.call_fnx_authorized(&cx, &issuer, &node_limit, BRANCH, CC,
            &FnxParameters::new(), empty, || 100).unwrap_err(), Error::LimitExceeded(LimitDimension::Nodes));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
