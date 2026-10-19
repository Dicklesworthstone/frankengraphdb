//! Mixed demand joins the existing exact index/fusion and native graph owners.
//! The cold indexed profile is an independent source/limit control; literal
//! ranks, hops and record counts check the admitted physical profile as well.

use super::*;
use crate::{Database, DatabaseKeys, EmbeddedReadView, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::WorkBudget;
use fgdb_delta_types::SchemaEpoch;
use fgdb_gql::{
    GqlParameters, GraphSetExecutionError, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x76; 32]);
const DOC: LabelId = LabelId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const HIGH: VId = VId(u128::MAX);

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn definition() -> Options {
    let mut options = Options::text(BODY);
    options.vertex_label = Some(DOC);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}

fn query(vector: bool, text: bool, k: usize) -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0],
            text: "graph",
            k,
            vector_candidates: u32::from(vector),
            text_candidates: u32::from(text),
            vector_mode: VectorSearch::Exact,
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::new(60, u16::from(vector), u16::from(text)).unwrap(),
        },
        graph_candidates: 1,
        graph_weight: 100,
    }
}

fn expansion() -> ExpansionSpec<'static, RelationId> {
    ExpansionSpec {
        seeds: &[VId(0)],
        relation: Some(R),
        direction: ExpansionDirection::Outgoing,
        max_hops: 2,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}

async fn fixture(cx: &CommitCx, disconnected: usize, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x75; 32], NS, [0x77; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, body, x) in [
        (VId(0), "graph", 0),
        (HIGH, "graph graph storage", 2),
        (VId(9), "storage", 10),
    ] {
        batch.create_vertex(
            id,
            vec![DOC],
            vec![(BODY, text(body)), (X, CanonicalScalar::Int(x))],
        );
    }
    // Rejected endpoints and propertyless vertices remain legitimate transit.
    batch.create_vertex(VId(2), vec![DOC], vec![]);
    for (id, source, target) in [
        (0, VId(0), VId(2)),
        (1, VId(0), VId(2)),
        (2, VId(2), HIGH),
        (3, VId(2), VId(2)),
        (u128::MAX, HIGH, VId(0)),
    ] {
        batch.add_edge(EId(id), source, target, vec![]);
    }
    for id in 0..disconnected {
        batch.create_vertex(
            VId(1000 + id as u128),
            vec![DOC],
            vec![
                (BODY, text("unrelated background corpus")),
                (X, CanonicalScalar::Int(100 + id as i64)),
            ],
        );
    }
    if hidden {
        batch.create_vertex(
            VId(90),
            vec![LabelId(99)],
            vec![
                (BODY, text("graph graph graph graph graph")),
                (X, CanonicalScalar::Int(0)),
            ],
        );
        batch.add_edge(EId(90), VId(0), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), HIGH, vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Doc") => Some(GraphSymbol::Label(DOC)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 100, 10_000_000, 1_000_000)
}

fn parameters(accept: bool) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "eligible",
            if accept {
                vec![GraphValue::Vertex(HIGH)]
            } else {
                Vec::new()
            },
        )
        .unwrap()
}

const CALL: &str = "CALL hybrid.search(text => 'graph', text_property => 'body', \
    vector => [0], vector_properties => ['x'], label => 'Doc', graph_access => 'demand', \
    seeds => [0], relation => 'R', max_hops => 2, eligible => $eligible, \
    candidates => 1, graph_candidates => 1, graph_weight => 100, k => 1) \
    YIELD node, score, vector_rank, text_rank, graph_rank, graph_hops, vector_distance, text_score \
    RETURN node, score, vector_rank, text_rank, graph_rank, graph_hops, vector_distance, text_score";

fn prepare(statement: &str, parameters: &GqlParameters) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(statement, symbols)
        .unwrap()
        .bind_parameters(parameters)
        .unwrap()
}

fn missing(result: Result<Vec<GraphHybridHit>, BeaconReadError<ReadError, Cancel>>) {
    assert!(
        matches!(
            result,
            Err(BeaconReadError::Index(BeaconError::InvalidQuery(
                "mixed demand search requires a compatible maintained exact index"
            )))
        ),
        "{result:?}",
    );
}

fn direct(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    options: &Options,
    query: GraphHybridQuery<'_>,
) -> Result<Vec<GraphHybridHit>, BeaconReadError<ReadError, Cancel>> {
    view.beacon_search_graph_demand_filtered(cx, options, query, expansion(), |id| id == HIGH)
}

#[test]
fn mixed_demand_keeps_exact_diagnostics_without_preparing_the_full_directory() {
    let ((), report) = run_async_under_lab(0xbeac_5501, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = fixture(&c.commit(), 128, false).await;
        let cold = db.read_session().unwrap();
        let handle = db
            .register_beacon_index(&cx, &definition(), ReadPolicy::default())
            .unwrap();
        let pin = db.read_session().unwrap();
        let at = pin.frontier();
        let mut bounded = definition();
        bounded.policy.max_staging_rows = 3;
        // max_documents is part of the index descriptor, not a substitute for
        // the query's three-entry membership allowance.
        let mut spec = expansion();
        spec.limits.max_vertices = 3;
        spec.limits.max_visited_vertices = 3;
        spec.limits.max_input_edges = 4;
        for (vector, text) in [(false, true), (true, false), (true, true)] {
            let request = query(vector, text, 1);
            let expected = cold
                .beacon_search_graph_indexed_filtered(
                    &cx,
                    &definition(),
                    request,
                    expansion(),
                    |id| id == HIGH,
                )
                .unwrap();
            assert_eq!(expected.len(), 1);
            let hit = &expected[0];
            assert_eq!(hit.id, HIGH);
            assert_eq!(hit.graph_hops, Some(2));
            assert_eq!(hit.graph_rank.map(|rank| rank.get()), Some(1));
            assert_eq!(hit.vector_rank.map(|rank| rank.get()), vector.then_some(1));
            assert_eq!(hit.text_rank.map(|rank| rank.get()), text.then_some(1));
            assert_eq!(
                (hit.score.numerator(), hit.score.denominator()),
                (100 + u128::from(vector) + u128::from(text), 61),
            );
            assert_eq!(
                db.beacon_search_graph_demand_filtered(&cx, &bounded, request, spec, |id| id
                    == HIGH,)
                    .unwrap(),
                expected,
            );
            assert_eq!(
                pin.beacon_search_graph_demand_filtered(&cx, &bounded, request, spec, |id| id
                    == HIGH,)
                    .unwrap(),
                expected,
            );
            assert!(matches!(
                cold.beacon_search_graph_indexed_filtered(
                    &cx,
                    &bounded,
                    request,
                    expansion(),
                    |id| id == HIGH,
                ),
                Err(BeaconReadError::Index(BeaconError::ResourceLimit {
                    resource: "staged vertices",
                    limit: 3,
                }))
            ));
        }

        let args = parameters(true);
        let plan = prepare(CALL, &args);
        let indexed = CALL.replace("graph_access => 'demand'", "graph_access => 'indexed'");
        let incumbent = cold
            .execute_graph_set_governed(&cx, &prepare(&indexed, &args), policy())
            .unwrap();
        let reached = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        assert_eq!(reached.value, incumbent.value);
        assert_eq!(reached.rows.snapshot_records, 3);
        assert_eq!(incumbent.rows.snapshot_records, 132);
        assert_eq!(
            pin.execute_graph_set_governed(&cx, &plan, policy())
                .unwrap(),
            reached,
        );
        let local = GqlQueryPolicy::new(3, 100, 10_000_000, 1_000_000);
        assert_eq!(
            db.execute_graph_set_governed(&cx, &plan, local)
                .unwrap()
                .value,
            reached.value,
        );
        assert!(
            db.execute_graph_set_governed(&cx, &prepare(&indexed, &args), local)
                .is_err(),
            "the indexed profile must still admit its full selected directory",
        );

        // The outer property read must use the SAME pin as all three lanes.
        let with_property = CALL.replace("RETURN node,", "RETURN node.x AS x,");
        let before = pin
            .query(&cx, &with_property, &args, symbols, policy())
            .unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(HIGH, BODY, Some(text("unrelated")));
        edit.set_vertex_property(HIGH, X, Some(CanonicalScalar::Int(30)));
        edit.delete_edge(EId(2));
        edit.add_edge(EId(20), VId(0), HIGH, vec![]);
        db.write(&c.commit(), edit).await.unwrap();
        let current = db
            .query(&cx, &with_property, &args, symbols, policy())
            .unwrap();
        assert_ne!(current, before);
        assert_eq!(
            current,
            db.query(
                &cx,
                &with_property.replace("graph_access => 'demand'", "graph_access => 'indexed'"),
                &args,
                symbols,
                policy(),
            )
            .unwrap(),
        );
        let mut historical = definition();
        historical.as_of = Some(at);
        missing(db.beacon_search_graph_demand_filtered(
            &cx,
            &historical,
            query(true, true, 1),
            expansion(),
            |id| id == HIGH,
        ));
        assert_eq!(
            pin.query(&cx, &with_property, &args, symbols, policy())
                .unwrap(),
            before,
        );
        db.compact(&c.commit()).await.unwrap();
        db.unregister_beacon_index(&cx, &handle).unwrap();
        drop(db);
        assert_eq!(
            pin.query(&cx, &with_property, &args, symbols, policy())
                .unwrap(),
            before,
        );
        assert_eq!(
            direct(&pin, &cx, &bounded, query(true, true, 1)).unwrap()[0].graph_hops,
            Some(2),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn missing_mismatched_historical_and_failed_generations_refuse_without_rebuilding() {
    let ((), report) = run_async_under_lab(0xbeac_5502, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = fixture(&c.commit(), 0, false).await;
        let cold = db.read_session().unwrap();
        let request = query(true, true, 1);
        missing(direct(&cold, &cx, &definition(), request));
        assert!(matches!(
            cold.execute_graph_set_governed(&cx, &prepare(CALL, &parameters(true)), policy(),),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                GqlError::Procedure(ProcedureError::Search(HybridCallError::Index(
                    BeaconError::InvalidQuery(
                        "mixed demand search requires a compatible maintained exact index"
                    )
                )))
            )))
        ));
        let handle = db
            .register_beacon_index(
                &cx,
                &definition(),
                ReadPolicy {
                    max_work_units: 0,
                    ..ReadPolicy::default()
                },
            )
            .unwrap();
        let healthy = db.read_session().unwrap();
        let expected = direct(&healthy, &cx, &definition(), request).unwrap();
        assert_eq!(expected.len(), 1);
        missing(direct(&cold, &cx, &definition(), request));
        for case in 0..7 {
            let mut changed = definition();
            match case {
                0 => changed.vertex_label = None,
                1 => changed.projection.text = Some(PropertyKeyId(8)),
                2 => changed.projection.vector = vec![PropertyKeyId(8)],
                3 => changed.projection.encoding = VectorEncoding::CoordinatesRoundedF32,
                4 => changed.index.text.as_mut().unwrap().k1 = 0.4,
                5 => {
                    changed.index.vector =
                        Some(HnswConfig::new(1, DistanceMetric::NegativeDotProduct));
                }
                6 => changed.index.max_documents = 1,
                _ => unreachable!(),
            }
            missing(direct(&healthy, &cx, &changed, request));
            // Neither reject-all nor k=0 can bypass admission of the index.
            let work = RefCell::new(WorkBudget::new(100_000));
            assert!(matches!(
                demand::evaluate_privileged(
                    &healthy.snapshot,
                    healthy.frontier(),
                    &changed,
                    query(true, true, 0),
                    expansion(),
                    &work,
                    &healthy.beacon_indexes,
                    |_| panic!("incompatible mixed demand inspected a native vertex"),
                    |_| false,
                ),
                Err(BeaconError::InvalidQuery(
                    "mixed demand search requires a compatible maintained exact index"
                ))
            ));
        }
        let mut ann = request;
        ann.retrieval.vector_mode = VectorSearch::Approximate { ef_search: 8 };
        missing(direct(&healthy, &cx, &definition(), ann));
        // An inactive projection is not part of this query's descriptor.
        let mut unused = definition();
        unused.projection.text = Some(PropertyKeyId(8));
        assert_eq!(
            direct(&healthy, &cx, &unused, query(true, false, 1)).unwrap(),
            direct(&healthy, &cx, &definition(), query(true, false, 1)).unwrap(),
        );

        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(HIGH, X, Some(CanonicalScalar::Int(7)));
        db.write(&c.commit(), edit).await.unwrap();
        assert!(
            db.beacon_index_status(&cx, &handle)
                .unwrap()
                .failure
                .is_some()
        );
        let failed = db.read_session().unwrap();
        missing(direct(&failed, &cx, &definition(), request));
        assert_eq!(
            direct(&healthy, &cx, &definition(), request).unwrap(),
            expected
        );
        db.rebuild_beacon_index(&cx, &handle, ReadPolicy::default(), ReadPolicy::default())
            .unwrap();
        let repaired = db.read_session().unwrap();
        assert_ne!(
            direct(&repaired, &cx, &definition(), request).unwrap(),
            expected
        );
        missing(direct(&failed, &cx, &definition(), request));
        let mut historical = definition();
        historical.as_of = Some(healthy.frontier());
        missing(direct(&repaired, &cx, &historical, request));
        historical.as_of = Some(repaired.frontier());
        assert!(matches!(
            direct(&healthy, &cx, &historical, request),
            Err(BeaconReadError::Read(ReadError::BeyondFrontier { .. }))
        ));
        // Historical topology-only demand keeps its existing index-free path.
        historical.as_of = Some(healthy.frontier());
        let graph = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
        let graph_rows = repaired
            .beacon_search_graph_demand_filtered(&cx, &historical, graph, expansion(), |id| {
                id == HIGH
            })
            .unwrap();
        assert_eq!(graph_rows[0].graph_hops, Some(2));
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_demand_charges_one_source_and_kernel_allowance_even_for_empty_results() {
    let ((), report) = run_async_under_lab(0xbeac_5503, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = fixture(&c.commit(), 0, false).await;
        db.register_beacon_index(&cx, &definition(), ReadPolicy::default())
            .unwrap();
        let view = db.read_session().unwrap();
        for k in [0, 1] {
            for accept in [false, true] {
                let request = query(true, true, k);
                let work = RefCell::new(WorkBudget::new(1_000_000));
                let nodes = Cell::new(0);
                let (expected, scratch) = demand::evaluate_privileged(
                    &view.snapshot,
                    view.frontier(),
                    &definition(),
                    request,
                    expansion(),
                    &work,
                    &view.beacon_indexes,
                    |_| {
                        nodes.set(nodes.get() + 1);
                        Ok(true)
                    },
                    |id| accept && id == HIGH,
                )
                .unwrap();
                assert_eq!(
                    nodes.get(),
                    3,
                    "transit is independent of output eligibility"
                );
                assert_eq!(expected.len(), usize::from(accept) * k);
                let units = 1_000_000 - work.into_inner().remaining();
                let mut exact = definition();
                exact.policy.max_work_units = units;
                exact.policy.max_source_scratch = usize::try_from(scratch).unwrap();
                assert!(scratch > 0);
                assert_eq!(
                    view.beacon_search_graph_demand_filtered(
                        &cx,
                        &exact,
                        request,
                        expansion(),
                        |id| accept && id == HIGH,
                    )
                    .unwrap(),
                    expected,
                );
                exact.policy.max_work_units -= 1;
                assert!(matches!(
                    view.beacon_search_graph_demand_filtered(
                        &cx,
                        &exact,
                        request,
                        expansion(),
                        |id| accept && id == HIGH,
                    ),
                    Err(BeaconReadError::Index(BeaconError::WorkBudgetExceeded))
                ));
                exact.policy.max_work_units += 1;
                for limit in 0..usize::try_from(scratch).unwrap() {
                    exact.policy.max_source_scratch = limit;
                    assert!(
                        matches!(
                            view.beacon_search_graph_demand_filtered(
                                &cx,
                                &exact,
                                request,
                                expansion(),
                                |id| accept && id == HIGH,
                            ),
                            Err(BeaconReadError::Index(BeaconError::ResourceLimit {
                                resource: "demand graph source admissions",
                                ..
                            }))
                        ),
                        "k={k}, accept={accept}, source scratch={limit}"
                    );
                }
                exact.policy.max_source_scratch = usize::try_from(scratch).unwrap();
                let mut shared = expansion();
                shared.limits.max_source_scratch = exact.policy.max_source_scratch - 1;
                assert!(matches!(
                    view.beacon_search_graph_demand_filtered(&cx, &exact, request, shared, |id| {
                        accept && id == HIGH
                    },),
                    Err(BeaconReadError::Index(BeaconError::ResourceLimit {
                        resource: "demand graph source admissions",
                        ..
                    }))
                ));
            }
        }
        let empty = query(true, true, 0);
        for (case, resource, limit) in [
            (0, "demand graph admitted vertices", 2),
            (1, "demand graph admitted vertices", 2),
            (2, "expansion visited vertices", 2),
            (3, "demand graph incidence visits", 3),
            (4, "expansion seed IDs", 0),
        ] {
            let mut options = definition();
            let mut spec = expansion();
            match case {
                0 => options.policy.max_staging_rows = limit,
                1 => spec.limits.max_vertices = limit,
                2 => spec.limits.max_visited_vertices = limit,
                3 => spec.limits.max_input_edges = limit,
                4 => spec.limits.max_seed_ids = limit,
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    view.beacon_search_graph_demand_filtered(
                        &cx, &options, empty, spec, |_| false,
                    ),
                    Err(BeaconReadError::Index(BeaconError::ResourceLimit {
                        resource: actual, limit: observed,
                    })) if actual == resource && observed == limit
                ),
                "case {case}"
            );
        }
        let mut disabled = query(true, true, 1);
        disabled.graph_weight = 0;
        let mut no_graph = expansion();
        no_graph.seeds = &[VId(90), VId(0)];
        no_graph.limits.max_seed_ids = 0;
        no_graph.limits.max_vertices = 0;
        no_graph.limits.max_visited_vertices = 0;
        no_graph.limits.max_input_edges = 0;
        no_graph.limits.max_source_scratch = 0;
        let mut options = definition();
        options.policy.max_staging_rows = 0;
        let rows = view
            .beacon_search_graph_demand_filtered(&cx, &options, disabled, no_graph, |id| id == HIGH)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].graph_hops, None);
        assert_eq!(rows[0].vector_rank.map(|rank| rank.get()), Some(1));
        assert_eq!(rows[0].text_rank.map(|rank| rank.get()), Some(1));
        assert_eq!(db.frontier().unwrap(), view.frontier());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_mixed_demand_replays_parent_limits_and_every_live_checkpoint() {
    let ((), report) = run_async_under_lab(0xbeac_5504, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = fixture(&c.commit(), 0, false).await;
        db.register_beacon_index(&cx, &definition(), ReadPolicy::default())
            .unwrap();
        let at = db.frontier().unwrap();
        for accept in [false, true] {
            let args = parameters(accept);
            let plan = prepare(&format!("{CALL} UNION ALL {CALL}"), &args);
            let expected = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
            assert_eq!(expected.rows.snapshot_records, 6);
            assert_eq!(expected.value.len(), if accept { 2 } else { 0 });
            let exact = GqlQueryPolicy::new(
                expected.rows.snapshot_records,
                expected.rows.result_rows,
                expected.evaluator.work_units,
                expected.evaluator.scratch_entries,
            );
            assert_eq!(
                db.execute_graph_set_governed(&cx, &plan, exact).unwrap(),
                expected,
            );
            for dimension in 0..3 {
                let short = match dimension {
                    0 => GqlQueryPolicy::new(
                        expected.rows.snapshot_records - 1,
                        expected.rows.result_rows,
                        expected.evaluator.work_units,
                        expected.evaluator.scratch_entries,
                    ),
                    1 => GqlQueryPolicy::new(
                        expected.rows.snapshot_records,
                        expected.rows.result_rows,
                        expected.evaluator.work_units - 1,
                        expected.evaluator.scratch_entries,
                    ),
                    _ => GqlQueryPolicy::new(
                        expected.rows.snapshot_records,
                        expected.rows.result_rows,
                        expected.evaluator.work_units,
                        expected.evaluator.scratch_entries - 1,
                    ),
                };
                assert!(
                    db.execute_graph_set_governed(&cx, &plan, short).is_err(),
                    "accept={accept}, short dimension={dimension}"
                );
            }
            let single = prepare(CALL, &args);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let successful = db
                .execute_graph_set_governed(
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    &single,
                    policy(),
                )
                .unwrap();
            let checkpoints = probe.calls();
            assert!(checkpoints > 2);
            for stop in 1..=checkpoints {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let result = db.execute_graph_set_governed(
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    &single,
                    policy(),
                );
                assert!(
                    matches!(result, Err(GqlQueryError::Interrupted(_))),
                    "accept={accept}, checkpoint={stop}: {result:?}"
                );
                assert_eq!(
                    probe.calls(),
                    stop,
                    "work continued after a refused live gate"
                );
            }
            assert_eq!(
                db.execute_graph_set_governed(&cx, &single, policy())
                    .unwrap(),
                successful,
            );
        }
        assert_eq!(db.frontier().unwrap(), at);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_and_staged_mixed_demand_never_borrow_a_privileged_corpus() {
    let ((), report) = run_async_under_lab(0xbeac_5505, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut clean = fixture(&c.commit(), 0, false).await;
        let mut full = fixture(&c.commit(), 0, true).await;
        let mut global = definition();
        global.vertex_label = None;
        for db in [&mut clean, &mut full] {
            db.register_beacon_index(&cx, &global, ReadPolicy::default())
                .unwrap();
        }
        let statement = CALL.replace("label => 'Doc', ", "");
        let indexed = statement.replace("graph_access => 'demand'", "graph_access => 'indexed'");
        let args = parameters(true);
        // The hidden document genuinely changes unmasked corpus diagnostics.
        assert_ne!(
            clean
                .query(&cx, &indexed, &args, symbols, policy())
                .unwrap(),
            full.query(&cx, &indexed, &args, symbols, policy()).unwrap(),
        );
        let authority = Authority::new(
            AuthKey::from_seed(5505),
            NS,
            "mixed-demand",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 100,
                max_work: 10_000_000,
                max_rows: 100,
            },
        );
        grant.labels = Scope::only([DOC]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([BODY, X]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let mut outputs = Vec::new();
        let mut refusals = Vec::new();
        for db in [&clean, &full] {
            let mut session = db
                .authorized_read_session(&cx, &authority, &token, "main", symbols, policy(), || 100)
                .unwrap();
            let prepared = session.prepare(&cx, &statement, &args).unwrap();
            let error = session.execute(&cx, &prepared, &args).unwrap_err();
            assert!(
                matches!(
                    &error,
                    crate::QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Source(
                        GqlError::Procedure(ProcedureError::Search(HybridCallError::Index(
                            BeaconError::InvalidQuery(
                                "index-free retrieval cannot discard an enabled text or vector lane"
                            )
                        )))
                    )))
                ),
                "{error:?}"
            );
            refusals.push(format!("{error:?}"));
            assert!(!session.is_closed());
            outputs.push(session.query(&cx, &indexed, &args).unwrap());
        }
        assert_eq!(refusals[0], refusals[1]);
        assert_eq!(
            outputs[0], outputs[1],
            "the ordinary authorized source remains masked"
        );

        // A committed generation cannot stand in for staged property edits.
        let before = clean
            .query(&cx, &indexed, &args, symbols, policy())
            .unwrap();
        let mut txn = clean.begin(&c.txn()).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(HIGH, X, Some(CanonicalScalar::Int(55)));
        txn.write(&mut clean, edit).unwrap();
        let plan = prepare(&statement, &args);
        assert!(matches!(
            txn.execute_graph_set_governed(&clean, &cx, &plan, policy()),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                crate::WriteTxnError::Gql(GqlError::Procedure(ProcedureError::Search(
                    HybridCallError::Combination("demand graph access requires a graph-only read")
                )))
            )))
        ));
        let staged = txn
            .execute_graph_set_governed(&clean, &cx, &prepare(&indexed, &args), policy())
            .unwrap();
        let committed = clean
            .execute_graph_set_governed(&cx, &prepare(&indexed, &args), policy())
            .unwrap();
        assert_ne!(staged.value, committed.value);
        assert_eq!(
            clean
                .query(&cx, &indexed, &args, symbols, policy())
                .unwrap(),
            before,
        );
        txn.finish(&mut clean, &c.commit()).await.unwrap();
        assert_eq!(
            clean
                .execute_graph_set_governed(&cx, &prepare(&statement, &args), policy())
                .unwrap()
                .value,
            staged.value,
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
