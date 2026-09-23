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
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::Arc;

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

#[test]
fn authentication_precedes_frontier_and_projection_and_hidden_sources_are_absent() {
    let ((), report) = run_async_under_lab(0x5ec0_3004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit(), true).await;
        let issuer = authority(304, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut opt = options(Directedness::Directed);
        opt.as_of = Some(CommitSeq(u64::MAX));
        opt.source_limits.max_work_units = 0;
        let foreign = authority(304, DatabaseSecurityNamespaceId([0x40; 32]));
        authorization(db.call_fnx_authorized(&cx, &foreign, &token, BRANCH, CC,
            &FnxParameters::new(), opt, || panic!("namespace refusal must precede clock access")).unwrap_err(),
            Error::WrongAuthority);
        let forged = authority(305, NS).issue_at(&grant(), 100).unwrap();
        authorization(db.call_fnx_authorized(&cx, &issuer, &forged, BRANCH, CC,
            &FnxParameters::new(), opt, || 100).unwrap_err(), Error::Unauthenticated);
        authorization(db.call_fnx_authorized(&cx, &issuer, &token, "other-branch", CC,
            &FnxParameters::new(), opt, || 100).unwrap_err(), Error::ScopeDenied);
        let denied = token.attenuate(Restriction::Rights(Rights::Write)).unwrap();
        authorization(db.call_fnx_authorized(&cx, &issuer, &denied, BRANCH, CC,
            &FnxParameters::new(), opt, || 100).unwrap_err(), Error::PermissionDenied);
        authorization(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, CC,
            &FnxParameters::new(), opt, || 1000).unwrap_err(), Error::Expired);
        assert!(matches!(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, CC,
            &FnxParameters::new(), opt, || 100), Err(FnxReadError::Read(ReadError::BeyondFrontier { .. }))));
        opt.as_of = None;
        assert!(matches!(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH, CC,
            &FnxParameters::new(), opt, || 100),
            Err(FnxReadError::Execution(FnxExecutionError::GraphKind { required: FnxGraphKind::Undirected }))));
        // Binding has no catalog callbacks or source access. This is a public
        // syntax refusal, not permission to execute an unregistered procedure.
        assert!(matches!(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH,
            "CALL fnx.not_registered()", &FnxParameters::new(), opt,
            || panic!("binding must not read the graph or clock")), Err(FnxReadError::Bind(_))));
        opt = options(Directedness::Directed);
        for id in [99, 100] { // hidden existing identity and absent identity
            let call = FnxCallSpec::single_source_shortest_path_length(VId(id), None);
            assert!(matches!(db.execute_fnx_authorized(&cx, &issuer, &token, BRANCH,
                &call, opt, || 100), Err(FnxReadError::Execution(
                    FnxExecutionError::UnknownSource(actual))) if actual == VId(id)));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_winners_cannot_resurrect_a_hidden_successor_even_after_compaction() {
    let ((), report) = run_async_under_lab(0x5ec0_3005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit, true).await;
        let issuer = authority(306, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let before = db.frontier().unwrap();
        let mut opt = options(Directedness::Undirected);
        opt.selection.weight = FnxWeightSpec::Unit;
        let original = read(&db, &cx, &issuer, &token, TRIANGLES, opt);
        assert_eq!(original, vec![
            vec![FnxValue::Vertex(VId(1)), FnxValue::Integer(1)],
            vec![FnxValue::Vertex(VId(2)), FnxValue::Integer(1)],
            vec![FnxValue::Vertex(VId(3)), FnxValue::Integer(1)],
            vec![FnxValue::Vertex(VId(u128::MAX)), FnxValue::Integer(0)],
        ]);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(2), LabelId(1), false);
        change.set_vertex_label(VId(2), LabelId(99), true);
        change.set_edge_property(EId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
        db.write(&commit, change).await.unwrap();
        let hidden_at = db.frontier().unwrap();
        let hidden = vec![
            vec![FnxValue::Vertex(VId(1)), FnxValue::Integer(0)],
            vec![FnxValue::Vertex(VId(3)), FnxValue::Integer(0)],
            vec![FnxValue::Vertex(VId(u128::MAX)), FnxValue::Integer(0)],
        ];
        assert_eq!(read(&db, &cx, &issuer, &token, TRIANGLES, opt), hidden);
        // The raw graph still has the triangle and its transit vertex: absence
        // above must come from the winning labels, not a physical deletion.
        let raw = db.call_fnx(&cx, TRIANGLES, &FnxParameters::new(), opt).unwrap().analytics.rows;
        assert!(raw.iter().any(|row| row[0] == FnxValue::Vertex(VId(2)) && row[1] == FnxValue::Integer(1)));
        let mut restore = WriteBatch::new(RelationId(1));
        restore.set_vertex_label(VId(2), LabelId(1), true);
        db.write(&commit, restore).await.unwrap();
        let denied = token.attenuate(Restriction::Labels(Scope::only([]))).unwrap();
        for compacted in [false, true] {
            if compacted { db.compact(&commit).await.unwrap(); }
            assert_eq!(read(&db, &cx, &issuer, &token, TRIANGLES, opt), original);
            for (at, expected) in [(before, &original), (hidden_at, &hidden)] {
                let historical = FnxReadOptions { as_of: Some(at), ..opt };
                assert_eq!(&read(&db, &cx, &issuer, &token, TRIANGLES, historical), expected);
                assert!(read(&db, &cx, &issuer, &denied, TRIANGLES, historical).is_empty(),
                    "a historical cut cannot retain previously wider permissions");
                authorization(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH,
                    TRIANGLES, &FnxParameters::new(), historical, || 1000).unwrap_err(), Error::Expired);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_source_builder_kernel_and_delivery_clock_observes_expiry_and_retirement() {
    let ((), report) = run_async_under_lab(0x5ec0_3006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit(), true).await;
        for empty in [false, true] {
            let issuer = authority(307 + u64::from(empty), NS);
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let mut opt = options(Directedness::Undirected);
            if empty { opt.selection.vertex_label = Some(LabelId(99)); }
            let mut calls = 0;
            let expected = db.call_fnx_authorized(&cx, &issuer, &token, BRANCH,
                TRIANGLES, &FnxParameters::new(), opt, || { calls += 1; 100 }).unwrap();
            assert_eq!(expected.is_empty(), empty);
            assert!(calls > 10);
            for stop in 1..=calls {
                let mut seen = 0;
                let error = db.call_fnx_authorized(&cx, &issuer, &token, BRANCH,
                    TRIANGLES, &FnxParameters::new(), opt, || {
                        seen += 1; if seen == stop { 1000 } else { 100 }
                    }).unwrap_err();
                authorization(error, Error::Expired);
                assert_eq!(seen, stop, "no successful-prefix continuation after expiry");
            }
            let mut seen = 0;
            authorization(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH,
                TRIANGLES, &FnxParameters::new(), opt, || {
                    seen += 1; if seen == 2 { 99 } else { 100 }
                }).unwrap_err(), Error::ClockWentBackwards);
            assert_eq!(read(&db, &cx, &issuer, &token, TRIANGLES, opt), expected);
            let mut seen = 0;
            authorization(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH,
                TRIANGLES, &FnxParameters::new(), opt, || {
                    seen += 1; if seen == calls { issuer.retire(); } 100
                }).unwrap_err(), Error::AuthorityRetired);
            assert_eq!(seen, calls, "retire at final delivery, even for an empty result");
            authorization(db.call_fnx_authorized(&cx, &issuer, &token, BRANCH,
                TRIANGLES, &FnxParameters::new(), opt, || 100).unwrap_err(), Error::AuthorityRetired);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_query_context_checkpoint_discards_partial_analytics_without_changing_the_database() {
    let ((), report) = run_async_under_lab(0x5ec0_3007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit(), true).await;
        let issuer = authority(309, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let root_id = db.read_session().unwrap().partition_root();
        for (text, direction) in [(BFS, Directedness::Reversed), (TRIANGLES, Directedness::Undirected)] {
            let opt = options(direction);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
            let expected = read(&db, &observed, &issuer, &token, text, opt);
            let calls = probe.calls();
            assert!(calls > 10);
            for stop in 1..=calls {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                assert!(matches!(db.call_fnx_authorized(&controlled, &issuer, &token, BRANCH,
                    text, &FnxParameters::new(), opt, || 100), Err(FnxReadError::Cancelled(
                        QueryError::Pattern(fgdb_gql::GqlQueryError::Interrupted(_))))));
                assert_eq!(probe.calls(), stop, "stop at the first failed source/build/kernel checkpoint");
                assert_eq!(db.read_session().unwrap().partition_root(), root_id);
            }
            assert_eq!(read(&db, &cx, &issuer, &token, text, opt), expected);
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_rows_admit_only_reachable_output_and_empty_sources_still_require_authority() {
    let ((), report) = run_async_under_lab(0x5ec0_3008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit(), true).await;
        let issuer = authority(310, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let one = token.attenuate(Restriction::MaxRows(1)).unwrap();
        let opt = options(Directedness::Directed);
        let call = FnxCallSpec::single_source_shortest_path_length(VId(1), Some(0));
        assert_eq!(db.execute_fnx_authorized(&cx, &issuer, &one, BRANCH, &call, opt, || 100).unwrap(),
            vec![vec![FnxValue::Vertex(VId(1)), FnxValue::Integer(0)]]);
        let isolated = FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), None);
        assert_eq!(db.execute_fnx_authorized(&cx, &issuer, &one, BRANCH, &isolated, opt, || 100).unwrap(),
            vec![vec![FnxValue::Vertex(VId(u128::MAX)), FnxValue::Integer(0)]]);
        authorization(db.call_fnx_authorized(&cx, &issuer, &one, BRANCH, BFS,
            &FnxParameters::new(), opt, || 100).unwrap_err(), Error::LimitExceeded(LimitDimension::Rows));
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        authorization(db.execute_fnx_authorized(&cx, &issuer, &zero, BRANCH,
            &call, opt, || 100).unwrap_err(), Error::LimitExceeded(LimitDimension::Rows));
        let empty = Database::<MemVfs>::open_memory(&c.commit(),
            DatabaseKeys::new([0x31; 32], NS, [0x32; 32])).await.unwrap();
        for text in [CC, TRIANGLES, "CALL fnx.pagerank()"] {
            let opt = options(Directedness::Undirected);
            assert!(read(&empty, &cx, &issuer, &zero, text, opt).is_empty());
            authorization(empty.call_fnx_authorized(&cx, &issuer, &zero, BRANCH,
                text, &FnxParameters::new(), opt, || 1000).unwrap_err(), Error::Expired);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
