//! Native CALL composition must reuse the exact authorized index and graph cut.
use super::*;
use crate::{Database, DatabaseKeys, MemVfs, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::{DistanceMetric, HnswConfig};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x93; 32]);
const L: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const NAME: PropertyKeyId = PropertyKeyId(3);
const HIGH: VId = VId(u128::MAX);
const NOW: u64 = 100;
const EXPIRES: u64 = 100_000;

fn text(s: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(s).unwrap()
}
fn catalog(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        _ => None,
    }
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 16, 1_000_000, 100_000)
}
fn authority() -> Authority {
    Authority::new(
        AuthKey::from_seed(0x93),
        NS,
        "native-graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        EXPIRES,
        QueryLimits {
            max_nodes: 100,
            max_work: 100_000_000,
            max_rows: 16,
        },
    );
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([BODY, X, NAME]);
    grant.relations = Scope::only([R]);
    grant
}
fn options() -> Options {
    let mut options = Options::text(BODY);
    options.vertex_label = Some(L);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
/// `$x` only for a statement whose CALL reads it (the vector lane): binding
/// refuses an argument the statement never declares.
fn parameters(vector: bool, seeds: &[VId], eligible: &[VId]) -> GqlParameters {
    let parameters = if vector {
        GqlParameters::new().with_int64("x", 0).unwrap()
    } else {
        GqlParameters::new()
    };
    parameters
        .with_list(
            "seeds",
            seeds.iter().copied().map(GraphValue::Vertex).collect(),
        )
        .unwrap()
        .with_list(
            "eligible",
            eligible.iter().copied().map(GraphValue::Vertex).collect(),
        )
        .unwrap()
}
fn call(vector: bool, words: bool, ann: bool, direction: &str) -> String {
    let v = if vector {
        "vector => [$x], vector_properties => ['x'],"
    } else {
        ""
    };
    let t = if words {
        "text => 'graph', text_property => 'body',"
    } else {
        ""
    };
    let ann = if vector && ann { "ann => 16," } else { "" };
    format!(
        "CALL hybrid.search({v}{t}{ann}label => 'Node', seeds => $seeds, \
        max_hops => 2, direction => '{direction}', graph_weight => 100, \
        graph_candidates => 1, candidates => 1, k => 1, eligible => $eligible) \
        YIELD node, graph_hops, graph_rank, vector_rank, text_rank "
    )
}
fn statement(vector: bool, words: bool, ann: bool, direction: &str) -> String {
    format!(
        "{}RETURN node, node.name AS name, graph_hops, graph_rank, vector_rank, text_rank",
        call(vector, words, ann, direction)
    )
}
fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("retrieval published a write"),
    }
}
fn integer(n: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)))
}
fn expected(id: VId, name: &str, hops: i64, vector: bool, words: bool) -> Vec<Vec<QueryValue>> {
    let null = QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
    vec![vec![
        QueryValue::Value(GraphValue::Vertex(id)),
        QueryValue::Value(GraphValue::Scalar(text(name))),
        integer(hops),
        integer(1),
        if vector { integer(1) } else { null.clone() },
        if words { integer(1) } else { null },
    ]]
}
async fn database(cx: &CommitCx, noisy: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x92; 32], NS, [0x94; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, x, name) in [
        (VId(0), 0, "seed"),
        (HIGH, 9, "high"),
        (VId(9), 1, "isolate"),
    ] {
        batch.create_vertex(
            id,
            vec![L],
            vec![
                (BODY, text(&"graph ".repeat(1024))),
                (X, CanonicalScalar::Int(x)),
                (NAME, text(name)),
            ],
        );
    }
    // Membership is not the set of documents with text/vector properties.
    batch.create_vertex(VId(1), vec![L], vec![(NAME, text("transit"))]);
    for (eid, src, dst) in [
        (0, VId(0), VId(1)),
        (1, VId(0), VId(1)),
        (2, VId(1), HIGH),
        (3, HIGH, VId(0)),
        (4, VId(1), VId(1)),
    ] {
        batch.add_edge(EId(eid), src, dst, vec![]);
    }
    if noisy {
        batch.create_vertex(
            VId(90),
            vec![LabelId(99)],
            vec![
                (BODY, CanonicalScalar::Bool(true)),
                (X, text("not a vector")),
            ],
        );
        for (eid, src, dst) in [
            (10, VId(0), VId(90)),
            (11, VId(90), HIGH),
            (12, VId(90), VId(0)),
            (13, HIGH, VId(90)),
        ] {
            batch.add_edge(EId(eid), src, dst, vec![]);
        }
        for id in 100..228 {
            batch.add_edge(EId(id), VId(9), VId(9), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    let mut forbidden = WriteBatch::new(RelationId(99));
    forbidden.add_edge(EId(u128::MAX), VId(0), HIGH, vec![]);
    db.write(cx, forbidden).await.unwrap();
    db
}

#[test]
fn native_graph_calls_reuse_every_modality_and_rebind_full_width_seeds() {
    let ((), report) = run_async_under_lab(0xbeac_6401, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let resolved = Cell::new(0);
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                |kind, name: &str| {
                    resolved.set(resolved.get() + 1);
                    catalog(kind, name)
                },
                host(),
                || NOW,
            )
            .unwrap();
        let index = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        for (v, t) in [(true, true), (true, false), (false, true), (false, false)] {
            for ann in [false, true] {
                for direction in ["out", "in", "both"] {
                    let params = parameters(v, &[VId(0), VId(0), VId(90)], &[VId(90), HIGH, HIGH]);
                    let prepared = session
                        .prepare(&c.query(), &statement(v, t, ann, direction), &params)
                        .unwrap();
                    let count = resolved.get();
                    let actual = rows(
                        session
                            .execute_beacon_index(&c.query(), &index, &prepared, &params)
                            .unwrap(),
                    );
                    assert_eq!(
                        actual,
                        expected(HIGH, "high", if direction == "out" { 2 } else { 1 }, v, t)
                    );
                    assert!(
                        rows(
                            session
                                .execute_beacon_index(
                                    &c.query(),
                                    &index,
                                    &prepared,
                                    &parameters(v, &[HIGH], &[])
                                )
                                .unwrap()
                        )
                        .is_empty()
                    );
                    // A full-width seed really drives the reverse endpoint, not
                    // a truncated integer or a cached previous seed population.
                    if direction == "out" {
                        assert_eq!(
                            rows(
                                session
                                    .execute_beacon_index(
                                        &c.query(),
                                        &index,
                                        &prepared,
                                        &parameters(v, &[HIGH], &[VId(0)])
                                    )
                                    .unwrap()
                            ),
                            expected(VId(0), "seed", 1, v, t)
                        );
                    }
                    assert_eq!(resolved.get(), count);
                }
            }
        }
        let params = parameters(false, &[VId(0)], &[VId(1)]);
        let prepared = session
            .prepare(&c.query(), &statement(false, false, false, "out"), &params)
            .unwrap();
        assert_eq!(
            rows(
                session
                    .execute_beacon_index(&c.query(), &index, &prepared, &params)
                    .unwrap()
            ),
            expected(VId(1), "transit", 1, false, false)
        );
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_and_return_values_keep_the_prepared_history_without_lending_transit_authority() {
    let ((), report) = run_async_under_lab(0xbeac_6402, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), true).await;
        let old = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(1));
        change.set_vertex_property(HIGH, NAME, Some(text("new")));
        change.set_vertex_property(HIGH, BODY, Some(text("blue")));
        db.write(&c.commit(), change).await.unwrap();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                catalog,
                host(),
                || NOW,
            )
            .unwrap();
        let mut historical = options();
        historical.as_of = Some(old);
        let index = session
            .prepare_beacon_index(&c.query(), &historical)
            .unwrap();
        let params = parameters(true, &[VId(0)], &[HIGH]);
        let prepared = session
            .prepare(&c.query(), &statement(true, true, false, "out"), &params)
            .unwrap();
        let old_rows = expected(HIGH, "high", 2, true, true);
        assert_ne!(
            rows(session.execute(&c.query(), &prepared, &params).unwrap()),
            old_rows
        );
        // Merely traversing an identity does not authorize native property
        // dereference through the per-statement retrieved-result set.
        let supplied = params
            .clone()
            .with_list("supplied", vec![GraphValue::Vertex(VId(1))])
            .unwrap();
        let attack = session
            .prepare(
                &c.query(),
                &format!(
                    "{}WITH $supplied[0] AS outside RETURN outside.name",
                    call(true, true, false, "out")
                ),
                &supplied,
            )
            .unwrap();
        assert!(
            session
                .execute_beacon_index(&c.query(), &index, &attack, &supplied)
                .is_err()
        );
        db.compact(&c.commit()).await.unwrap();
        drop(db);
        assert_eq!(
            rows(
                session
                    .execute_beacon_index(&c.query(), &index, &prepared, &params)
                    .unwrap()
            ),
            old_rows
        );
        assert!(!session.is_closed());
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_call_sources_and_set_siblings_share_exact_record_and_signed_node_admission() {
    let ((), report) = run_async_under_lab(0xbeac_6403, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let authority = authority();
        let params = parameters(true, &[VId(0)], &[HIGH]);
        let text = statement(true, true, false, "out");
        for nodes in [7, 8] {
            let mut grant = grant();
            grant.limits.max_nodes = nodes;
            let token = authority.issue_at(&grant, NOW).unwrap();
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || NOW,
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let single = session.prepare(&c.query(), &text, &params).unwrap();
            let both = session
                .prepare(&c.query(), &format!("{text} UNION ALL {text}"), &params)
                .unwrap();
            // Four saved visible vertices plus four reached incidence visits.
            for records in [7, 8] {
                session.state.as_mut().unwrap().policy =
                    GqlQueryPolicy::new(records, 16, 1_000_000, 100_000);
                let result = session.execute_beacon_index(&c.query(), &index, &single, &params);
                assert_eq!(
                    result.is_ok(),
                    records == 8,
                    "records={records}: {result:?}"
                );
            }
            session.state.as_mut().unwrap().policy =
                GqlQueryPolicy::new(15, 16, 1_000_000, 100_000);
            assert!(
                session
                    .execute_beacon_index(&c.query(), &index, &both, &params)
                    .is_err()
            );
            session.state.as_mut().unwrap().policy =
                GqlQueryPolicy::new(16, 16, 1_000_000, 100_000);
            let result = session.execute_beacon_index(&c.query(), &index, &both, &params);
            assert_eq!(result.is_ok(), nodes == 8, "nodes={nodes}: {result:?}");
            if let Ok(result) = result {
                assert_eq!(rows(result).len(), 2);
            }
            // Even a filtered-empty first source spends the complete graph
            // admission, and cannot erase it before the next UNION operand.
            session.state.as_mut().unwrap().policy =
                GqlQueryPolicy::new(15, 16, 1_000_000, 100_000);
            assert!(
                session
                    .execute_beacon_index(
                        &c.query(),
                        &index,
                        &both,
                        &parameters(true, &[VId(0)], &[])
                    )
                    .is_err()
            );
            assert!(!session.is_closed());
            session.close();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn minimum(mut succeeds: impl FnMut(u64) -> bool, mut high: u64) -> u64 {
    assert!(succeeds(high));
    let mut low = 0;
    while low < high {
        let middle = low + (high - low) / 2;
        if succeeds(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    assert!(succeeds(low));
    assert!(low > 0 && !succeeds(low - 1));
    low
}

#[test]
fn native_work_and_scratch_thresholds_ignore_hidden_and_disconnected_incidence() {
    let ((), report) = run_async_under_lab(0xbeac_6404, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut thresholds = Vec::new();
        for noisy in [false, true] {
            let db = database(&c.commit(), noisy).await;
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || NOW,
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let params = parameters(true, &[VId(0)], &[HIGH]);
            let prepared = session
                .prepare(&c.query(), &statement(true, true, false, "out"), &params)
                .unwrap();
            let work = minimum(
                |units| {
                    session.state.as_mut().unwrap().policy =
                        GqlQueryPolicy::new(1000, 16, units, 100_000);
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .is_ok()
                },
                1_000_000,
            );
            let scratch = minimum(
                |units| {
                    session.state.as_mut().unwrap().policy =
                        GqlQueryPolicy::new(1000, 16, 1_000_000, units);
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .is_ok()
                },
                100_000,
            );
            // This allowance must support reuse, not an implicit reconstruction
            // of the three 6KB documents on the ordinary native call path.
            session.state.as_mut().unwrap().policy = GqlQueryPolicy::new(1000, 16, work, scratch);
            assert_eq!(
                rows(
                    session
                        .execute_beacon_index(&c.query(), &index, &prepared, &params)
                        .unwrap()
                ),
                expected(HIGH, "high", 2, true, true)
            );
            assert!(session.execute(&c.query(), &prepared, &params).is_err());
            thresholds.push((work, scratch));
            session.close();
        }
        assert_eq!(thresholds[0], thresholds[1]);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_call_cancellation_and_late_errors_never_publish_a_prefix_or_poison_reuse() {
    let ((), report) = run_async_under_lab(0xbeac_6405, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(
                &c.query(),
                &authority,
                &token,
                "main",
                catalog,
                host(),
                || NOW,
            )
            .unwrap();
        let index = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        for mixed in [false, true] {
            for eligible in [vec![HIGH], vec![]] {
                let params = parameters(mixed, &[VId(0)], &eligible);
                let prepared = session
                    .prepare(&c.query(), &statement(mixed, mixed, false, "out"), &params)
                    .unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let expected = session
                    .execute_beacon_index(
                        &c.query().with_checkpoint_probe(probe.clone()),
                        &index,
                        &prepared,
                        &params,
                    )
                    .unwrap();
                for cut in 1..=probe.calls() {
                    let stopped = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                    assert!(
                        session
                            .execute_beacon_index(
                                &c.query().with_checkpoint_probe(stopped.clone()),
                                &index,
                                &prepared,
                                &params
                            )
                            .is_err(),
                        "cut={cut}, mixed={mixed}"
                    );
                    assert!(stopped.calls() >= cut);
                    assert!(!session.is_closed());
                    assert_eq!(
                        session
                            .execute_beacon_index(&c.query(), &index, &prepared, &params)
                            .unwrap(),
                        expected
                    );
                }
            }
        }
        let params = parameters(true, &[VId(0)], &[HIGH]);
        let bad = session
            .prepare(
                &c.query(),
                &format!(
                    "{}RETURN 1 / 0 AS bad LIMIT 0",
                    call(true, true, false, "out")
                ),
                &params,
            )
            .unwrap();
        assert!(
            session
                .execute_beacon_index(&c.query(), &index, &bad, &params)
                .is_err()
        );
        // A graph-only statement reads no `$x`.
        let graph_only = parameters(false, &[VId(0)], &[HIGH]);
        let zero = session
            .prepare(
                &c.query(),
                &format!("{}RETURN node LIMIT 0", call(false, false, false, "out")),
                &graph_only,
            )
            .unwrap();
        let invalid = GqlParameters::new()
            .with_list("seeds", vec![GraphValue::Vertex(VId(0))])
            .unwrap()
            .with_list(
                "eligible",
                vec![GraphValue::Scalar(CanonicalScalar::Int(-1))],
            )
            .unwrap();
        assert!(
            session
                .execute_beacon_index(&c.query(), &index, &zero, &invalid)
                .is_err()
        );
        assert!(
            rows(
                session
                    .execute_beacon_index(&c.query(), &index, &zero, &graph_only)
                    .unwrap()
            )
            .is_empty()
        );
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_clock_gate_can_expire_graph_reuse_even_after_a_complete_empty_answer() {
    let ((), report) = run_async_under_lab(0xbeac_6406, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for eligible in [vec![HIGH], vec![]] {
            let samples = Cell::new(0_u64);
            let stop = Cell::new(u64::MAX);
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || {
                        let at = samples.get();
                        samples.set(at + 1);
                        if at >= stop.get() { EXPIRES } else { NOW }
                    },
                )
                .unwrap();
            let index = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            let params = parameters(true, &[VId(0)], &eligible);
            let prepared = session
                .prepare(&c.query(), &statement(true, true, false, "out"), &params)
                .unwrap();
            samples.set(0);
            session
                .execute_beacon_index(&c.query(), &index, &prepared, &params)
                .unwrap();
            let calls = samples.get();
            assert!(calls > 5);
            samples.set(0);
            stop.set(calls - 1);
            assert!(matches!(
                session.execute_beacon_index(&c.query(), &index, &prepared, &params),
                Err(QueryError::Authorization(WardenError::Expired))
            ));
            assert!(session.is_closed());
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
